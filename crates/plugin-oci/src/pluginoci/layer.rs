//! The `oci_layer` driver: turns target outputs into one image layer.
//!
//! A layer is a tar of a file tree, and that is all it is — there is no
//! execution here, nothing runs, nothing is fetched. `srcs` names targets, and
//! every file they produced lands under `prefix` inside the image.
//!
//! Layers are a separate rule rather than an attribute of [`super::image`] for
//! two reasons. A big shared layer (static assets, a CA bundle) is then built
//! and cached **once** and reused by every image that lists it — which is what
//! the layer format is for. And when a file ends up at the wrong path, the
//! answer is `heph run //pkg:that_layer` and one small tar, not an image build.
//!
//! # Reproducibility
//!
//! The layer's bytes are its digest, so nothing about the host may reach them.
//! Every tar header is built by hand:
//!
//! - **mtime 0**, uid/gid 0, empty uname/gname. `tar::Builder::append_path` is
//!   never used: it copies all four off the filesystem.
//! - **Entries sorted** by the raw bytes of the in-image path. The engine hands
//!   a dep's file list in walk order, which differs between filesystems.
//! - **Mode is the executable bit and nothing else** — `0755` or `0644` — unless
//!   `mode` says otherwise. This is not a simplification, it is the only honest
//!   answer: heph's own artifact hash records exactly one permission bit
//!   (`crates/walk`), the pack step normalizes to those two values, and what a
//!   file's mode *is* in the sandbox then depends on the umask, on whether some
//!   other target marked the dep read-only, and on whether the input crossed the
//!   size threshold that selects the FUSE sandbox over unpacking. Preserving it
//!   would make the same declared inputs produce different images.
//! - **Symlinks are preserved, never followed.** Symlink-ness *is* covered by
//!   the dep's hash, and the link's mode is pinned to `0o777` rather than
//!   lstat'ed (Linux reports `0777` there, macOS `0755`).
//!
//! Layers are **gzipped** by default (`application/vnd.oci.image.layer.v1.tar+gzip`),
//! once, here, and the compressed blob is what the cache keeps and every image
//! that lists the layer reuses. The encoder is [`super::gzip`], which pins the
//! deflate implementation and the header so the blob digest stays a function of
//! the tar. `compress = False` writes the tar as it is
//! (`application/vnd.oci.image.layer.v1.tar`), for a layer of already-compressed
//! files where deflate buys nothing.
//!
//! # The descriptor
//!
//! Next to the blob, a layer writes `<name>.oci-layer.json`
//! ([`LayerDescriptor`]): the blob's file name, its media type, and the digest
//! of the *uncompressed* tar, which an image config lists as the layer's
//! `diff_id`. `oci.image` reads the layer through it. That makes the descriptor
//! what marks an `oci.layer` output: an image refuses a `layers` entry that
//! has none rather than filing someone's file as a layer blob, which no build
//! or push step checks and a runtime only rejects at deploy.

use anyhow::Context as _;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path as OutPath};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::{Spec, TargetSpecCache};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

use super::{dep_files, ws_path};

/// Origin id prefix of a `srcs` dep input. The index is in the id so a
/// `HEPH_DEBUG_HASH` trace names the entry rather than "a src".
fn src_origin(i: usize) -> String {
    format!("srcs|{i}")
}

/// Media type of a layer written with `compress = False`.
pub(crate) const MEDIA_TYPE_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
/// Media type of a gzipped layer, the default.
pub(crate) const MEDIA_TYPE_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// File name suffix of a layer's [`LayerDescriptor`].
pub(crate) const DESCRIPTOR_SUFFIX: &str = ".oci-layer.json";

/// What a layer tells the image that lists it, written next to the blob.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LayerDescriptor {
    /// File name of the blob, in the same output as this descriptor.
    pub(crate) blob: String,
    /// [`MEDIA_TYPE_TAR`] or [`MEDIA_TYPE_TAR_GZIP`].
    pub(crate) media_type: String,
    /// `sha256:<hex>` of the uncompressed tar: the layer's `diff_id`. The same
    /// value as the blob's digest when the layer is not compressed.
    pub(crate) diff_id: String,
}

/// Packs target outputs into a single image layer, for `oci_image`. No
/// execution, no Dockerfile, no docker.
#[derive(Spec)]
struct OciLayerSpec {
    /// Targets whose files go into this layer. Every file they produce is
    /// included, at its workspace-relative path, rewritten by `strip` and
    /// `prefix`.
    ///
    /// A `srcs` that produces no files at all is an error, not an empty layer:
    /// an image missing its binary builds, pushes and starts, and fails when
    /// something tries to exec it.
    srcs: Vec<String>,
    /// Where the files land inside the image, e.g. `"/usr/bin"`.
    ///
    /// Required, deliberately. A default of `/` (files at their
    /// workspace-relative path, `/cmd/server/bin`) is never the answer anyone
    /// wants, so it would be overridden every time — and the one caller who
    /// forgot would get an image that builds and pushes clean and fails at
    /// `docker run` with `stat /usr/bin/server: no such file or directory`.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    prefix: String,
    /// A leading workspace-relative path to drop before applying `prefix`, e.g.
    /// `strip = "cmd/server"` puts `cmd/server/bin` at `<prefix>/bin`.
    ///
    /// A `strip` that matches nothing is an error rather than a silent no-op:
    /// the files would land somewhere unintended and nothing would say so.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    strip: Option<String>,
    /// File mode as an octal string, e.g. `"0755"`. Default: `0755` for files
    /// the producing target marked executable, `0644` otherwise.
    ///
    /// A string rather than a number because Starlark has no octal literal, and
    /// `mode = 755` would silently mean decimal 755 (`0o1363`).
    ///
    /// setuid, setgid and the sticky bit are rejected: nothing in heph can carry
    /// them from a producing target to here, so honouring them on this attribute
    /// alone would promise something the next layer could not keep.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    mode: Option<String>,
    /// Gzip the layer. Default `True`.
    ///
    /// The layer is compressed once, here, and every image that lists it
    /// reuses the compressed blob, so the cost is paid once per change to its
    /// sources. Turn it off for a layer of files that are already compressed
    /// (archives, images, a UPX-packed binary), where deflate costs time and
    /// saves nothing.
    #[spec(default = true)]
    compress: bool,
    /// Caching for the layer tar. Defaults to on for both tiers.
    cache: TargetSpecCache,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. IO-bound: an undeclared `cpu` costs 0.1. Never hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciLayerDef {
    /// Workspace-relative output path of the layer blob.
    out: String,
    /// Workspace-relative output path of its [`LayerDescriptor`].
    descriptor_out: String,
    /// Gzip the blob.
    compress: bool,
    /// The `srcs` addresses, normalized and sorted.
    ///
    /// In the hash because `hashin` cannot carry them: it folds a *sorted,
    /// unlabeled multiset* of dep hashouts, with no addr and — the sharp edge —
    /// no output-group selector, since `inputs_result_meta` folds every group a
    /// dep has. Without this, `srcs = [":bin|release"]` and `[":bin|debug"]`
    /// compute the same key for two different layers.
    ///
    /// Sorted rather than ordered: within one layer, entries are sorted by path
    /// and two `srcs` in either order produce the same tar.
    srcs: Vec<String>,
    /// In-image prefix, with no leading `/` (tar entry names are relative).
    prefix: String,
    strip: Option<String>,
    /// An explicit mode, or `None` to follow the producing target's exec bit.
    mode: Option<u32>,
}

/// Bump to invalidate cached layers when the emitted tar changes shape.
///
/// This covers the parts of the encoding that are *transforms* rather than
/// hashed values — the GNU header format, mtime 0, uid/gid 0, the sort key, the
/// exec-bit mode rule, the gzip encoder and the descriptor. Changing any of
/// them alters the bytes without altering a single field below, which is the
/// same-key-different-artifact shape.
///
/// 2: gzip by default, and the descriptor next to the blob.
const OCI_LAYER_FORMAT_VERSION: u32 = 2;

impl Hash for OciLayerDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_LAYER_FORMAT_VERSION.hash(state);
        self.out.hash(state);
        self.descriptor_out.hash(state);
        self.compress.hash(state);
        self.srcs.hash(state);
        self.prefix.hash(state);
        self.strip.hash(state);
        self.mode.hash(state);
    }
}

/// Normalize a `prefix` to a tar entry prefix: no leading `/`, no trailing `/`,
/// no `.` or `..` components.
fn normalize_prefix(prefix: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !prefix.is_empty(),
        "`prefix` is empty; give the path inside the image these files go to, e.g. \"/usr/bin\""
    );
    let trimmed = prefix.trim_matches('/');
    for part in trimmed.split('/') {
        anyhow::ensure!(
            part != ".." && part != ".",
            "`prefix` {prefix:?} contains a {part:?} component; give a plain absolute path \
             inside the image, e.g. \"/usr/bin\""
        );
    }
    Ok(trimmed.to_string())
}

/// Parse an octal `mode` string, rejecting the bits heph cannot carry.
fn parse_mode(mode: &str) -> anyhow::Result<u32> {
    let parsed = u32::from_str_radix(mode.trim_start_matches("0o"), 8)
        .with_context(|| format!("`mode` {mode:?} is not an octal number, e.g. \"0755\""))?;
    anyhow::ensure!(
        parsed & 0o7000 == 0,
        "`mode` {mode:?} sets setuid/setgid/sticky. heph records one permission bit per file \
         (executable or not), so those cannot survive a rebuild from cache — a layer that \
         depended on them would differ from itself. Drop them, or set them in the container at \
         run time."
    );
    anyhow::ensure!(
        parsed & !0o777 == 0,
        "`mode` {mode:?} has bits outside the permission range; expected something like \"0755\""
    );
    Ok(parsed)
}

/// Where one staged file lands inside the image.
fn dest_path(ws_rel: &Path, strip: Option<&str>, prefix: &str) -> anyhow::Result<Option<String>> {
    let rel = match strip {
        Some(s) => match ws_rel.strip_prefix(s) {
            Ok(r) => r,
            Err(_) => return Ok(None),
        },
        None => ws_rel,
    };
    let rel = rel.to_str().with_context(|| {
        format!("path {rel:?} is not valid UTF-8; an image entry name has to be")
    })?;
    anyhow::ensure!(
        !rel.is_empty(),
        "`strip` consumed the whole path {ws_rel:?}, leaving no name for the file inside the image"
    );
    Ok(Some(if prefix.is_empty() {
        rel.to_string()
    } else {
        format!("{prefix}/{rel}")
    }))
}

/// One entry on its way into the layer tar.
struct Entry {
    /// Absolute path in the sandbox.
    source: PathBuf,
    /// Name inside the image.
    dest: String,
    /// The target that produced it, for the error when two collide.
    from: String,
}

/// Whiteout markers are how a layer expresses a *deletion* at extraction time.
/// A source file that happens to be named like one would silently remove
/// something from the base image, so it is refused rather than carried.
fn reject_whiteout(dest: &str) -> anyhow::Result<()> {
    let name = dest.rsplit('/').next().unwrap_or(dest);
    anyhow::ensure!(
        !name.starts_with(".wh."),
        "{dest:?} is named like an OCI whiteout marker, which a runtime reads as \"delete this \
         path from the layers below\". oci_layer does not express deletions; rename the file."
    );
    Ok(())
}

/// Forwards every byte to `inner` and hashes it on the way: the uncompressed
/// tar's digest, computed in the one pass that writes it.
struct DiffIdWriter<W> {
    inner: W,
    hasher: sha2::Sha256,
}

impl<W> DiffIdWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: <sha2::Sha256 as sha2::Digest>::new(),
        }
    }
}

impl<W: std::io::Write> std::io::Write for DiffIdWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        sha2::Digest::update(&mut self.hasher, buf.get(..n).unwrap_or_default());
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Write the entries as a layer blob at `out`, gzipped when `compress` is set.
/// Returns the `diff_id`: `sha256:<hex>` of the uncompressed tar.
fn write_layer(
    out: &Path,
    entries: &[Entry],
    mode: Option<u32>,
    compress: bool,
) -> anyhow::Result<String> {
    let file = std::io::BufWriter::new(
        std::fs::File::create(out).with_context(|| format!("create layer {out:?}"))?,
    );
    let (file, hasher) = if compress {
        let gz = super::gzip::GzipWriter::new(file).context("start the gzip stream")?;
        let DiffIdWriter { inner, hasher } = write_tar(DiffIdWriter::new(gz), entries, mode)?;
        (inner.finish().context("finish the gzip stream")?, hasher)
    } else {
        let DiffIdWriter { inner, hasher } = write_tar(DiffIdWriter::new(file), entries, mode)?;
        (inner, hasher)
    };
    file.into_inner()
        .map_err(std::io::IntoInnerError::into_error)
        .with_context(|| format!("flush layer {out:?}"))?;
    Ok(format!(
        "sha256:{}",
        hex::encode(sha2::Digest::finalize(hasher))
    ))
}

/// Write the entries as a tar into `w`, and hand `w` back.
fn write_tar<W: std::io::Write>(w: W, entries: &[Entry], mode: Option<u32>) -> anyhow::Result<W> {
    let mut ar = tar::Builder::new(w);
    for entry in entries {
        let meta = std::fs::symlink_metadata(&entry.source)
            .with_context(|| format!("lstat {:?}", entry.source))?;
        // `Header::new_gnu` is zeroed, which is already uid/gid 0 with empty
        // uname/gname. Everything else is set explicitly; nothing is read off
        // the filesystem except the size and the executable bit.
        let mut header = tar::Header::new_gnu();
        header.set_mtime(0);
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&entry.source)
                .with_context(|| format!("readlink {:?}", entry.source))?;
            anyhow::ensure!(
                target.is_relative(),
                "{:?} is a symlink to the absolute path {:?}; inside an image that would point \
                 at the host's filesystem, not the layer's",
                entry.dest,
                target
            );
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            ar.append_link(&mut header, &entry.dest, &target)
                .with_context(|| format!("append symlink {}", entry.dest))?;
            continue;
        }
        let size = meta.len();
        header.set_size(size);
        header.set_mode(mode.unwrap_or(if is_executable(&meta) { 0o755 } else { 0o644 }));
        header.set_cksum();
        let mut src = std::fs::File::open(&entry.source)
            .with_context(|| format!("open {:?}", entry.source))?;
        ar.append_data(&mut header, &entry.dest, &mut src)
            .with_context(|| format!("append {}", entry.dest))?;
    }
    ar.into_inner().context("finish the layer tar")
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    meta.mode() & 0o111 != 0
}

/// Stateless: a layer is assembled from declared inputs and nothing else.
#[derive(Default)]
pub struct Driver;

impl Driver {
    pub fn new() -> Self {
        Driver
    }
}

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "layer".to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        OciLayerSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let pkg = addr.package.to_owned();
        let spec = OciLayerSpec::from(&req.target_spec.config).context("parse oci_layer config")?;

        anyhow::ensure!(
            !spec.srcs.is_empty(),
            "`srcs` is empty; an oci_layer with no sources would produce an empty layer"
        );
        let prefix = normalize_prefix(&spec.prefix)?;
        let mode = spec.mode.as_deref().map(parse_mode).transpose()?;

        let mut inputs = Vec::new();
        let mut srcs = Vec::new();
        for (i, src) in spec.srcs.iter().enumerate() {
            let r#ref = TargetAddr::parse(src, &pkg)
                .with_context(|| format!("parse `srcs` entry {src:?}"))?;
            // The normalized rendering, not the BUILD-file spelling: `:bin` and
            // `//app:bin` in package `app` are one dep and must be one key.
            srcs.push(r#ref.to_string());
            inputs.push(Input {
                r#ref,
                mode: InputMode::Standard,
                origin_id: src_origin(i),
                annotations: BTreeMap::new(),
                hashed: true,
                runtime: true,
            });
        }
        srcs.sort();

        let ext = if spec.compress { "tar.gz" } else { "tar" };
        let out = ws_path(pkg.as_str(), &format!("{}.{ext}", addr.name));
        let descriptor_out = ws_path(pkg.as_str(), &format!("{}{DESCRIPTOR_SUFFIX}", addr.name));
        let def = OciLayerDef {
            out: out.clone(),
            descriptor_out: descriptor_out.clone(),
            compress: spec.compress,
            srcs,
            prefix,
            strip: spec.strip,
            mode,
        };
        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("oci_layer_{}", addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs,
                // One group: the descriptor travels with the blob wherever the
                // layer is referenced, which is how an image finds both.
                outputs: vec![Output {
                    group: String::new(),
                    paths: [out, descriptor_out]
                        .into_iter()
                        .map(|p| OutPath {
                            content: Content::FilePath(p),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        })
                        .collect(),
                }],
                cache: spec.cache.into(),
                hash,
                resources: spec
                    .resources
                    .advertise(
                        hplugin::provider::PoolName::cpu(),
                        hplugin::driver::Resources::IO_CPU,
                    )
                    .context("oci.layer resources")?,
                ..Default::default()
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<OciLayerDef>().clone();
        let out_name = super::basename(&def.out)?;
        let out_path = req.sandbox_pkg_dir.join(out_name);

        let mut entries: Vec<Entry> = Vec::new();
        let mut stripped_none: Vec<String> = Vec::new();
        // Per origin, not per input: a `glob()` dep reaches here as one input
        // per matched file, and each origin's list already names all of them.
        // `parse` gave the i-th `srcs` entry origin `src_origin(i)`.
        for origin in (0..def.srcs.len()).map(src_origin) {
            let Some(src) = req.inputs.iter().find(|m| m.input.origin_id == origin) else {
                continue;
            };
            let from = src.input.source_addr.to_string();
            for path in dep_files(&req, &origin)? {
                let ws_rel = path.strip_prefix(&req.sandbox_ws_dir).unwrap_or(&path);
                match dest_path(ws_rel, def.strip.as_deref(), &def.prefix)? {
                    Some(dest) => {
                        reject_whiteout(&dest)?;
                        entries.push(Entry {
                            source: path.clone(),
                            dest,
                            from: from.clone(),
                        });
                    }
                    None => stripped_none.push(ws_rel.to_string_lossy().into_owned()),
                }
            }
        }

        // A layer that silently lost every file is the failure this driver is
        // most likely to produce and the least likely to be noticed: the image
        // builds, pushes, and dies at exec time. Name what the sources actually
        // produced and what the prefixes on offer are.
        if entries.is_empty() {
            let mut hint = String::new();
            if let Some(strip) = &def.strip {
                stripped_none.sort();
                stripped_none.dedup();
                let offers: BTreeSet<&str> = stripped_none
                    .iter()
                    .filter_map(|p| p.rsplit_once('/').map(|(dir, _)| dir))
                    .collect();
                hint = format!(
                    "\n  `strip = {strip:?}` matched none of them.\n  Set strip to one of: {} — \
                     or drop it.",
                    offers.into_iter().collect::<Vec<_>>().join(", ")
                );
            }
            anyhow::bail!(
                "this layer is empty (prefix {:?}).\n  its srcs produced: {}{hint}",
                def.prefix,
                if stripped_none.is_empty() {
                    "nothing at all".to_string()
                } else {
                    stripped_none.join(", ")
                }
            );
        }

        entries.sort_by(|a, b| a.dest.as_bytes().cmp(b.dest.as_bytes()));
        // Two srcs writing one path: tar allows the duplicate and extraction is
        // last-writer-wins, so which file ends up in the image would depend on
        // iteration order. Reported after the sort, so the pair is adjacent.
        for pair in entries.windows(2) {
            let [a, b] = pair else { continue };
            anyhow::ensure!(
                a.dest != b.dest,
                "{} and {} both put a file at {:?} in this layer; one would silently overwrite \
                 the other. Split them into two layers, or narrow `srcs`.",
                a.from,
                b.from,
                a.dest
            );
        }

        let diff_id = write_layer(&out_path, &entries, def.mode, def.compress)
            .with_context(|| format!("write the layer to {out_path:?}"))?;
        let descriptor = LayerDescriptor {
            blob: out_name
                .to_str()
                .with_context(|| format!("layer file name {out_name:?} is not UTF-8"))?
                .to_string(),
            media_type: if def.compress {
                MEDIA_TYPE_TAR_GZIP
            } else {
                MEDIA_TYPE_TAR
            }
            .to_string(),
            diff_id,
        };
        let descriptor_path = req
            .sandbox_pkg_dir
            .join(super::basename(&def.descriptor_out)?);
        std::fs::write(
            &descriptor_path,
            serde_json::to_vec(&descriptor).context("encode the layer descriptor")?,
        )
        .with_context(|| format!("write the layer descriptor to {descriptor_path:?}"))?;

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_normalize_and_reject_traversal() {
        assert_eq!(normalize_prefix("/usr/bin").expect("ok"), "usr/bin");
        assert_eq!(normalize_prefix("usr/bin/").expect("ok"), "usr/bin");
        assert_eq!(normalize_prefix("/").expect("ok"), "");
        for bad in ["", "/usr/../etc", "./x"] {
            assert!(normalize_prefix(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// setuid cannot survive a rebuild from cache — heph records one permission
    /// bit — so accepting it here would promise something the cache breaks.
    #[test]
    fn modes_are_octal_and_reject_setuid() {
        assert_eq!(parse_mode("0755").expect("ok"), 0o755);
        assert_eq!(parse_mode("644").expect("ok"), 0o644);
        let err = format!("{:#}", parse_mode("4755").expect_err("setuid"));
        assert!(err.contains("setuid"), "got: {err}");
        assert!(parse_mode("0999").is_err(), "9 is not an octal digit");
    }

    #[test]
    fn strip_and_prefix_rewrite_the_path() {
        let p = Path::new("cmd/server/bin");
        assert_eq!(
            dest_path(p, Some("cmd/server"), "usr/bin").expect("ok"),
            Some("usr/bin/bin".to_string())
        );
        assert_eq!(
            dest_path(p, None, "usr/bin").expect("ok"),
            Some("usr/bin/cmd/server/bin".to_string())
        );
        // Root prefix keeps the workspace-relative path.
        assert_eq!(
            dest_path(p, None, "").expect("ok"),
            Some("cmd/server/bin".to_string())
        );
        // A strip that does not match is reported to the caller, which turns it
        // into the empty-layer error rather than silently keeping the path.
        assert_eq!(dest_path(p, Some("other"), "usr/bin").expect("ok"), None);
    }

    #[test]
    fn whiteout_names_are_refused() {
        assert!(reject_whiteout("usr/bin/server").is_ok());
        let err = format!("{:#}", reject_whiteout("etc/.wh.passwd").expect_err("wh"));
        assert!(err.contains("whiteout"), "got: {err}");
    }

    fn entry(dir: &Path, name: &str, body: &[u8], exec: bool) -> Entry {
        let src = dir.join(name.replace('/', "_"));
        std::fs::write(&src, body).expect("write");
        if exec {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        Entry {
            source: src,
            dest: name.to_string(),
            from: "//app:src".to_string(),
        }
    }

    fn members(tar: &Path) -> Vec<(String, u32, u64)> {
        let f = std::fs::File::open(tar).expect("open");
        let mut ar = tar::Archive::new(f);
        ar.entries()
            .expect("entries")
            .map(|e| {
                let e = e.expect("entry");
                let h = e.header();
                (
                    e.path().expect("path").to_string_lossy().into_owned(),
                    h.mode().expect("mode"),
                    h.mtime().expect("mtime"),
                )
            })
            .collect()
    }

    /// The layer's bytes are its digest. Two runs from the same inputs must
    /// produce the same file, and the mode must follow the executable bit and
    /// nothing finer — a mode-preserving layer would differ between a cold run
    /// and one served from cache, and between two umasks.
    #[test]
    fn layers_are_byte_stable_and_carry_only_the_exec_bit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = [
            entry(dir.path(), "usr/bin/server", b"elf", true),
            entry(dir.path(), "etc/app.conf", b"k=v", false),
        ];
        let mut sorted: Vec<&Entry> = entries.iter().collect();
        sorted.sort_by(|a, b| a.dest.as_bytes().cmp(b.dest.as_bytes()));
        let sorted: Vec<Entry> = sorted
            .into_iter()
            .map(|e| Entry {
                source: e.source.clone(),
                dest: e.dest.clone(),
                from: e.from.clone(),
            })
            .collect();

        let a = dir.path().join("a.tar");
        let b = dir.path().join("b.tar");
        write_layer(&a, &sorted, None, false).expect("a");
        write_layer(&b, &sorted, None, false).expect("b");
        assert_eq!(
            std::fs::read(&a).expect("a"),
            std::fs::read(&b).expect("b"),
            "the same entries must produce the same layer bytes"
        );

        assert_eq!(
            members(&a),
            vec![
                ("etc/app.conf".to_string(), 0o644, 0),
                ("usr/bin/server".to_string(), 0o755, 0),
            ],
            "sorted by path, mtime zeroed, mode from the exec bit"
        );

        // A finer mode on the source is invisible: 0600 and 0644 are the same
        // layer, which is what makes a cache-served rebuild reproduce.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&sorted[0].source, std::fs::Permissions::from_mode(0o600))
                .expect("chmod");
            let c = dir.path().join("c.tar");
            write_layer(&c, &sorted, None, false).expect("c");
            assert_eq!(
                std::fs::read(&a).expect("a"),
                std::fs::read(&c).expect("c"),
                "only the executable bit may reach the layer"
            );
        }
    }

    /// A symlink must arrive as a symlink. Following it would silently duplicate
    /// the target's bytes, or — for a link out of the layer — embed a file from
    /// the sandbox that was never declared.
    #[cfg(unix)]
    #[test]
    fn symlinks_are_preserved_not_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("busybox"), b"bin").expect("write");
        std::os::unix::fs::symlink("busybox", dir.path().join("sh")).expect("symlink");
        let entries = vec![
            Entry {
                source: dir.path().join("busybox"),
                dest: "bin/busybox".to_string(),
                from: "//app:bb".to_string(),
            },
            Entry {
                source: dir.path().join("sh"),
                dest: "bin/sh".to_string(),
                from: "//app:bb".to_string(),
            },
        ];
        let out = dir.path().join("l.tar");
        write_layer(&out, &entries, None, false).expect("write");

        let f = std::fs::File::open(&out).expect("open");
        let mut ar = tar::Archive::new(f);
        let kinds: Vec<(String, tar::EntryType)> = ar
            .entries()
            .expect("entries")
            .map(|e| {
                let e = e.expect("entry");
                (
                    e.path().expect("path").to_string_lossy().into_owned(),
                    e.header().entry_type(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("bin/busybox".to_string(), tar::EntryType::Regular),
                ("bin/sh".to_string(), tar::EntryType::Symlink),
            ]
        );
    }

    /// An absolute symlink inside an image points at the *host* when the layer
    /// is written and at the container's root when it is read — two different
    /// files under one digest.
    #[cfg(unix)]
    #[test]
    fn absolute_symlinks_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("p")).expect("symlink");
        let entries = vec![Entry {
            source: dir.path().join("p"),
            dest: "etc/passwd".to_string(),
            from: "//app:p".to_string(),
        }];
        let err = format!(
            "{:#}",
            write_layer(&dir.path().join("x.tar"), &entries, None, false).expect_err("absolute")
        );
        assert!(err.contains("absolute path"), "got: {err}");
    }

    /// A gzipped layer holds the same tar the uncompressed one is, and its
    /// `diff_id` is that tar's digest — not the blob's, which is what an image
    /// config would otherwise list and a runtime reject on unpack.
    #[test]
    fn a_gzipped_layer_holds_the_tar_its_diff_id_names() {
        use std::io::Read as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![
            entry(dir.path(), "etc/app.conf", b"k=v", false),
            entry(dir.path(), "usr/bin/server", b"elf", true),
        ];
        let plain = dir.path().join("l.tar");
        let gz = dir.path().join("l.tar.gz");
        let plain_id = write_layer(&plain, &entries, None, false).expect("plain");
        let gz_id = write_layer(&gz, &entries, None, true).expect("gz");

        let plain_bytes = std::fs::read(&plain).expect("read plain");
        let mut inflated = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&gz).expect("open gz"))
            .read_to_end(&mut inflated)
            .expect("the blob is gzip");
        assert_eq!(inflated, plain_bytes, "gzip wraps the same tar");

        let tar_digest = format!(
            "sha256:{}",
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&plain_bytes))
        );
        assert_eq!(plain_id, tar_digest);
        assert_eq!(
            gz_id, tar_digest,
            "diff_id is the uncompressed tar's digest"
        );
    }

    /// The compressed blob's digest, frozen. It moves if the gzip encoder's
    /// output does — a `miniz_oxide` bump, a header change — and every cached
    /// layer would then keep its old key with new bytes. If this fails on
    /// purpose, bump `OCI_LAYER_FORMAT_VERSION` in the same commit.
    #[test]
    fn the_gzipped_blob_digest_is_pinned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![
            entry(dir.path(), "etc/app.conf", b"k=v\n", false),
            entry(dir.path(), "usr/bin/server", &b"elf ".repeat(4096), true),
        ];
        let out = dir.path().join("l.tar.gz");
        write_layer(&out, &entries, None, true).expect("write");
        let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(
            std::fs::read(&out).expect("read"),
        ));
        assert_eq!(
            digest, "07437eabade76ae29f87607f4ddeb8876768b7c394fe535e292431ba52b114ed",
            "the gzipped layer's bytes changed: bump OCI_LAYER_FORMAT_VERSION if deliberate"
        );
    }

    /// The sort is on the raw bytes of the entry name, which is not the same
    /// order `PathBuf` compares in — `a-b` and `a/b` swap between the two. Both
    /// are deterministic, so only a test pins which one the layer uses.
    #[test]
    fn the_sort_key_is_the_entry_name_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut entries = vec![
            entry(dir.path(), "a/b", b"1", false),
            entry(dir.path(), "a.b", b"2", false),
            entry(dir.path(), "a-b", b"3", false),
        ];
        entries.sort_by(|x, y| x.dest.as_bytes().cmp(y.dest.as_bytes()));
        let out = dir.path().join("s.tar");
        write_layer(&out, &entries, None, false).expect("write");
        assert_eq!(
            members(&out)
                .into_iter()
                .map(|(p, _, _)| p)
                .collect::<Vec<_>>(),
            vec!["a-b".to_string(), "a.b".to_string(), "a/b".to_string()],
            "'-' (0x2D) < '.' (0x2E) < '/' (0x2F)"
        );
    }
}
