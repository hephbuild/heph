//! The `oci_layer` driver: turns target outputs into one image layer.
//!
//! A layer is a tar of a file tree, and that is all it is — there is no
//! execution here, nothing runs, nothing is fetched. `srcs` names targets, and
//! every file they produced lands under `prefix` inside the image. Or `tar`
//! names one target whose prebuilt tar is adopted as the layer, byte for byte;
//! the rules below are then that target's to keep.
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

/// Origin id of the `tar` dep input.
const TAR_ORIGIN: &str = "tar";

/// Media type of a layer written with `compress = False`.
pub(crate) const MEDIA_TYPE_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
/// Media type of a gzipped layer, the default.
pub(crate) const MEDIA_TYPE_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// How a layer blob encodes its tar. Serialized as its media type, so a
/// descriptor naming any other media type fails to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum LayerEncoding {
    /// The tar as it is: `compress = False`.
    #[serde(rename = "application/vnd.oci.image.layer.v1.tar")]
    Tar,
    /// The tar, gzipped by [`super::gzip`]: the default.
    #[serde(rename = "application/vnd.oci.image.layer.v1.tar+gzip")]
    TarGzip,
}

impl LayerEncoding {
    fn from_compress(compress: bool) -> Self {
        if compress { Self::TarGzip } else { Self::Tar }
    }

    /// The blob's file extension.
    pub(crate) fn ext(self) -> &'static str {
        match self {
            Self::Tar => "tar",
            Self::TarGzip => "tar.gz",
        }
    }

    /// The media type a manifest lists the blob under.
    pub(crate) fn media_type(self) -> &'static str {
        match self {
            Self::Tar => MEDIA_TYPE_TAR,
            Self::TarGzip => MEDIA_TYPE_TAR_GZIP,
        }
    }

    /// The `cpu` a layer target advertises when it declares none: deflate is
    /// CPU work and takes a whole one, a plain tar only copies bytes.
    fn cpu(self) -> &'static str {
        match self {
            Self::Tar => hplugin::driver::Resources::IO_CPU,
            Self::TarGzip => "1",
        }
    }
}

/// File name suffix of a layer's [`LayerDescriptor`].
pub(crate) const DESCRIPTOR_SUFFIX: &str = ".oci-layer.json";

/// What a layer tells the image that lists it, written next to the blob.
///
/// Adding, removing or changing a field changes the descriptor's bytes, which
/// are part of the layer's cached output: bump [`OCI_LAYER_FORMAT_VERSION`] in
/// the same commit, or a cached layer keeps serving the old shape.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LayerDescriptor {
    /// File name of the blob, in the same output as this descriptor.
    pub(crate) blob: String,
    /// How the blob encodes the tar, written as its media type.
    pub(crate) media_type: LayerEncoding,
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
    ///
    /// Exactly one of `srcs` and `tar`.
    srcs: Vec<String>,
    /// Where the `srcs` files land inside the image, e.g. `"/usr/bin"`.
    ///
    /// Required with `srcs`, deliberately. A default of `/` (files at their
    /// workspace-relative path, `/cmd/server/bin`) is never the answer anyone
    /// wants, so it would be overridden every time — and the one caller who
    /// forgot would get an image that builds and pushes clean and fails at
    /// `docker run` with `stat /usr/bin/server: no such file or directory`.
    ///
    /// With `srcs` only: refused with `tar`, whose entries keep their own
    /// paths.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    prefix: Option<String>,
    /// Exactly one of `srcs` and `tar`. `srcs` is for files heph builds;
    /// `tar` is for an archive something else builds (a rootfs, a vendor's
    /// archive, a `docker export`).
    ///
    /// A target producing one uncompressed tar, used as the layer as it is:
    /// its entries keep their names, modes, owners and times. The tar's bytes
    /// are adopted unchanged and compressed per `compress` (gzipped unless
    /// `compress = False`); its `diff_id` is the tar's own digest.
    ///
    /// Every entry header is read, and the layer target fails when it runs
    /// (not at query time) on:
    ///
    /// - a file that is not a tar, or is truncated;
    /// - a gzipped tar (decompress it first);
    /// - a tar with no entries;
    /// - an entry with an absolute path or a `..` component, or a hardlink
    ///   to one, or a hardlink with no target;
    /// - anything but zero padding after the end of the archive (two tars
    ///   concatenated, say).
    ///
    /// Symlinks are kept whatever they point at: in a container an absolute
    /// symlink resolves against the image's root, as rootfs tars expect
    /// (`/usr/bin/awk -> /etc/alternatives/awk`). Whiteout entries are kept
    /// too: an adopted layer may express deletions. `prefix`, `strip` and
    /// `mode` rewrite paths and modes, so they do not apply and are refused
    /// with `tar`.
    ///
    /// Reproducibility is the producing target's: the layer's digest is a
    /// function of the tar's bytes, so a tar carrying a build time or the
    /// builder's uid makes a different layer on every build.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    tar: Option<String>,
    /// A leading workspace-relative path to drop before applying `prefix`, e.g.
    /// `strip = "cmd/server"` puts `cmd/server/bin` at `<prefix>/bin`.
    ///
    /// A `strip` that matches nothing is an error rather than a silent no-op:
    /// the files would land somewhere unintended and nothing would say so.
    ///
    /// With `srcs` only.
    #[spec(ty = hcore::htvalue::signature::ParamType::String)]
    strip: Option<String>,
    /// File mode as an octal string, e.g. `"0755"`. Default: `0755` for files
    /// the producing target marked executable, `0644` otherwise.
    ///
    /// With `srcs` only.
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
    ///
    /// Flipping it changes the layer's digest, and so the digest of every
    /// image that lists the layer. For an adopted `tar`, gzip changes the
    /// blob's digest but not the `diff_id`, which is always the tar's own.
    #[spec(default = true)]
    compress: bool,
    /// Caching for the layer tar. Defaults to on for both tiers.
    cache: TargetSpecCache,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. An undeclared `cpu` costs one CPU when the layer is
    /// gzipped (deflate is CPU work), and 0.1 with `compress = False`, which
    /// only copies bytes. Never hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciLayerDef {
    /// Workspace-relative output path of the layer blob.
    out: String,
    /// Workspace-relative output path of its [`LayerDescriptor`].
    descriptor_out: String,
    /// How the blob encodes the tar.
    encoding: LayerEncoding,
    /// Where the tar comes from.
    source: LayerSource,
}

/// Where a layer's tar comes from: written here from `srcs`, or adopted.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum LayerSource {
    Srcs {
        /// The `srcs` addresses, normalized and sorted.
        ///
        /// In the hash because `hashin` cannot carry them: it folds a *sorted,
        /// unlabeled multiset* of dep hashouts, with no addr and — the sharp
        /// edge — no output-group selector, since `inputs_result_meta` folds
        /// every group a dep has. Without this, `srcs = [":bin|release"]` and
        /// `[":bin|debug"]` compute the same key for two different layers.
        ///
        /// Sorted rather than ordered: within one layer, entries are sorted by
        /// path and two `srcs` in either order produce the same tar.
        srcs: Vec<String>,
        /// In-image prefix, with no leading `/` (tar entry names are relative).
        prefix: String,
        strip: Option<String>,
        /// An explicit mode, or `None` to follow the producing target's exec
        /// bit.
        mode: Option<u32>,
    },
    /// A prebuilt tar, copied as it is.
    Tar {
        /// The `tar` address, normalized. In the hash for the same reason
        /// `srcs` is: `hashin` carries no address or output-group selector.
        tar: String,
    },
}

/// Bump to invalidate cached layers when the emitted tar changes shape.
///
/// This covers the parts of the encoding that are *transforms* rather than
/// hashed values — the GNU header format, mtime 0, uid/gid 0, the sort key, the
/// exec-bit mode rule, the gzip encoder and the descriptor. Changing any of
/// them alters the bytes without altering a single field below, which is the
/// same-key-different-artifact shape.
///
/// It covers what `tar` adoption refuses as well: tightening that rule is a
/// bump too, or a layer adopted under the looser rule stays in the cache and
/// keeps being served.
///
/// 2: gzip by default, and the descriptor next to the blob.
const OCI_LAYER_FORMAT_VERSION: u32 = 2;

impl Hash for OciLayerDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        OCI_LAYER_FORMAT_VERSION.hash(state);
        self.out.hash(state);
        self.descriptor_out.hash(state);
        // The media type rather than a derived `Hash`, whose discriminant
        // encoding is the compiler's business, not this format's.
        self.encoding.media_type().hash(state);
        // A tag per source, spelled out for the same reason.
        match &self.source {
            LayerSource::Srcs {
                srcs,
                prefix,
                strip,
                mode,
            } => {
                "srcs".hash(state);
                srcs.hash(state);
                prefix.hash(state);
                strip.hash(state);
                mode.hash(state);
            }
            LayerSource::Tar { tar } => {
                "tar".hash(state);
                tar.hash(state);
            }
        }
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
        // A writer claiming more than it was given breaks `Write`'s contract;
        // hashing a guess would put a wrong diff_id in the image.
        let written = buf.get(..n).ok_or_else(|| {
            std::io::Error::other(format!(
                "the layer writer reported {n} bytes written of {}",
                buf.len()
            ))
        })?;
        sha2::Digest::update(&mut self.hasher, written);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Write a layer blob at `out` in `encoding`, with `fill` writing the
/// uncompressed tar into the sink it is handed. Returns the `diff_id`:
/// `sha256:<hex>` of that tar.
///
/// Synchronous and CPU-bound (deflate, sha256): call it from
/// `hcore::blocking::run`, never on an async worker.
fn write_blob(
    out: &Path,
    encoding: LayerEncoding,
    fill: impl FnOnce(&mut dyn std::io::Write) -> anyhow::Result<()>,
) -> anyhow::Result<String> {
    // Written beside `out` and renamed over it: an input can sit at `out`
    // itself (layer `rootfs` adopting `rootfs.tar` with `compress = False`),
    // and creating `out` up front would truncate it before `fill` reads it.
    let name = out
        .file_name()
        .with_context(|| format!("layer path {out:?} has no file name"))?;
    let partial = out.with_file_name(format!(".{}.partial", name.to_string_lossy()));
    let file = std::io::BufWriter::new(
        std::fs::File::create(&partial).with_context(|| format!("create layer {partial:?}"))?,
    );
    let (file, hasher) = match encoding {
        LayerEncoding::TarGzip => {
            let gz = super::gzip::GzipWriter::new(file).context("start the gzip stream")?;
            let mut w = DiffIdWriter::new(gz);
            fill(&mut w)?;
            let DiffIdWriter { inner, hasher } = w;
            (inner.finish().context("finish the gzip stream")?, hasher)
        }
        LayerEncoding::Tar => {
            let mut w = DiffIdWriter::new(file);
            fill(&mut w)?;
            (w.inner, w.hasher)
        }
    };
    file.into_inner()
        .map_err(std::io::IntoInnerError::into_error)
        .with_context(|| format!("flush layer {partial:?}"))?;
    std::fs::rename(&partial, out).with_context(|| format!("move {partial:?} to {out:?}"))?;
    Ok(format!(
        "sha256:{}",
        hex::encode(sha2::Digest::finalize(hasher))
    ))
}

/// Write the entries as a layer blob at `out`, in `encoding`. Returns the
/// `diff_id`. See [`write_blob`]: blocking.
fn write_layer(
    out: &Path,
    entries: &[Entry],
    mode: Option<u32>,
    encoding: LayerEncoding,
) -> anyhow::Result<String> {
    write_blob(out, encoding, |w| {
        write_tar(w, entries, mode)?;
        Ok(())
    })
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

/// The entries a `srcs` layer holds: every file its `srcs` produced, at its
/// in-image path, sorted by that path. Refuses an empty layer and two files
/// at one path.
fn srcs_entries(
    req: &ManagedRunRequest<'_, '_>,
    n_srcs: usize,
    prefix: &str,
    strip: Option<&str>,
) -> anyhow::Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut stripped_none: Vec<String> = Vec::new();
    // Per origin, not per input: a `glob()` dep reaches here as one input
    // per matched file, and each origin's list already names all of them.
    // `parse` gave the i-th `srcs` entry origin `src_origin(i)`.
    for origin in (0..n_srcs).map(src_origin) {
        let Some(src) = req.inputs.iter().find(|m| m.input.origin_id == origin) else {
            continue;
        };
        let from = src.input.source_addr.to_string();
        for path in dep_files(req, &origin)? {
            let ws_rel = path.strip_prefix(&req.sandbox_ws_dir).unwrap_or(&path);
            match dest_path(ws_rel, strip, prefix)? {
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
        if let Some(strip) = strip {
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
            "this layer is empty (prefix {prefix:?}).\n  its srcs produced: {}{hint}",
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
    Ok(entries)
}

/// The one file of `files`, what the `tar` dep produced. `ws_dir` is
/// stripped from the names an error lists.
fn tar_file(files: Vec<PathBuf>, ws_dir: &Path, dep: &str) -> anyhow::Result<PathBuf> {
    let files = match <[PathBuf; 1]>::try_from(files) {
        Ok([only]) => return Ok(only),
        Err(files) => files,
    };
    let names: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(ws_dir)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    anyhow::bail!(
        "`tar` {dep} must produce exactly one file, the tar to adopt; it produced {}: {}",
        names.len(),
        if names.is_empty() {
            "nothing".to_string()
        } else {
            names.join(", ")
        }
    )
}

/// The two bytes every gzip stream starts with.
pub(crate) const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Reads from `inner` and writes every byte it hands out into `sink`: the
/// adopted tar reaches the layer blob in the one pass that checks it.
struct Tee<'s, R> {
    inner: R,
    sink: &'s mut dyn std::io::Write,
    /// The sink's error, kept apart from read errors: a full disk is not a
    /// malformed tar.
    failed: Option<std::io::Error>,
}

impl<R: std::io::Read> std::io::Read for Tee<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let read = buf.get(..n).ok_or_else(|| {
            std::io::Error::other(format!(
                "the tar reader reported {n} bytes read of {}",
                buf.len()
            ))
        })?;
        if let Err(e) = self.sink.write_all(read) {
            let shown = std::io::Error::new(e.kind(), e.to_string());
            self.failed = Some(e);
            return Err(shown);
        }
        Ok(n)
    }
}

/// Forwards the bytes after a tar's end-of-archive block to the sink, and
/// fails on the first one that is not zero: the padding belongs to the layer,
/// anything else is data a runtime would drop or reject.
struct ZeroTail<'s> {
    sink: &'s mut dyn std::io::Write,
    /// Set when a non-zero byte was seen, as against a failing sink.
    trailing_data: bool,
    /// The sink's error, kept apart as [`Tee`] keeps it.
    failed: Option<std::io::Error>,
}

impl std::io::Write for ZeroTail<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.iter().any(|b| *b != 0) {
            self.trailing_data = true;
            return Err(std::io::Error::other("data after the end of the archive"));
        }
        if let Err(e) = self.sink.write_all(buf) {
            let shown = std::io::Error::new(e.kind(), e.to_string());
            self.failed = Some(e);
            return Err(shown);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}

/// Copy the tar at `src`, produced by `dep`, into `sink` byte for byte,
/// reading every entry header on the way. Streams: the tar is never held in
/// memory. See [`write_blob`]: blocking.
///
/// What is refused is what would reach outside the layer when a runtime
/// extracts it — an absolute or `..` entry name, a hardlink to one or with no
/// target — plus a file that is not a tar at all, a truncated one, a gzipped
/// one, one with no entries, and one with data after its end-of-archive
/// block. Symlinks are not: in a container they resolve against the image's
/// root, absolute ones included.
fn adopt_tar(src: &Path, dep: &str, sink: &mut dyn std::io::Write) -> anyhow::Result<()> {
    use std::io::BufRead as _;
    let shown = src.file_name().map_or_else(
        || src.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let file = std::fs::File::open(src).with_context(|| format!("open {shown} from {dep}"))?;
    let mut reader = std::io::BufReader::new(file);
    let head = reader
        .fill_buf()
        .with_context(|| format!("read {shown} from {dep}"))?;
    // No decoder on purpose: decompressing here would make the adopted bytes
    // something other than the file the dep produced.
    anyhow::ensure!(
        !head.starts_with(&GZIP_MAGIC),
        "{dep} produced {shown}, a gzip stream; `tar` takes an uncompressed tar. Decompress it \
         first (e.g. `gunzip`); the layer is compressed again per `compress`."
    );
    let mut ar = tar::Archive::new(Tee {
        inner: reader,
        sink,
        failed: None,
    });
    let walked = check_entries(&mut ar, dep, &shown);
    let mut tee = ar.into_inner();
    if let Some(e) = tee.failed.take() {
        return Err(e).context("write the layer");
    }
    let count = walked?;
    anyhow::ensure!(
        count > 0,
        "{dep} produced {shown}, a tar with no entries; this layer would be empty"
    );
    // The archive stops at the first zero block. The rest — the second one
    // and the padding to the record size — is part of the tar the dep wrote,
    // so it is part of the layer and of its diff_id. It must be zeros: a
    // second tar concatenated, or headers after a lone zero block, would be
    // dropped by one runtime and rejected by another.
    let Tee {
        mut inner, sink, ..
    } = tee;
    let mut tail = ZeroTail {
        sink,
        trailing_data: false,
        failed: None,
    };
    let copied = std::io::copy(&mut inner, &mut tail);
    if let Some(e) = tail.failed.take() {
        return Err(e).context("write the layer");
    }
    anyhow::ensure!(
        !tail.trailing_data,
        "{dep} produced {shown}, which has data after the end of the archive (two tars \
         concatenated, or entries after a zero block); a runtime would drop or reject it. \
         Produce one tar."
    );
    copied.with_context(|| format!("read {shown} from {dep}"))?;
    Ok(())
}

/// Read every entry header of `ar`, refusing what would escape the layer.
/// Returns the number of entries.
fn check_entries<R: std::io::Read>(
    ar: &mut tar::Archive<R>,
    dep: &str,
    shown: &str,
) -> anyhow::Result<usize> {
    let not_a_tar = || format!("{dep} produced {shown}, which is not a tar, or is cut short");
    let mut count = 0;
    for entry in ar.entries().with_context(not_a_tar)? {
        let entry = entry.with_context(not_a_tar)?;
        let path = entry.path().with_context(not_a_tar)?;
        anyhow::ensure!(
            !escapes(&path),
            "{dep} produced {shown}, whose entry {path:?} is absolute or has a `..` component; \
             extracted, it would land outside the layer"
        );
        // Whiteouts (`.wh.<name>`, `.wh..wh..opq`) are allowed, unlike in a
        // srcs layer: an adopted layer may be one that expresses deletions on
        // purpose, and what it deletes is its producer's decision.
        //
        // Symlinks are not checked at all. A runtime resolves one against
        // the image's root, absolute or climbing, and rootfs tars are full
        // of absolute ones (`/usr/bin/awk -> /etc/alternatives/awk`). The
        // srcs path refuses an absolute symlink because it is a host file,
        // whose target was meant for the host; an adopted tar's was written
        // for the image.
        if entry.header().entry_type() == tar::EntryType::Link {
            // An empty name (a PAX `linkpath=`) is no target either, and
            // would pass the escape check below.
            let target = entry
                .link_name()
                .with_context(not_a_tar)?
                .filter(|t| !t.as_os_str().is_empty())
                .with_context(|| {
                    format!(
                        "{dep} produced {shown}, whose hardlink {path:?} names no target; a \
                         hardlink names another entry of the layer"
                    )
                })?;
            anyhow::ensure!(
                !escapes(&target),
                "{dep} produced {shown}, whose hardlink {path:?} points at {target:?}, which is \
                 absolute or has a `..` component; a hardlink names another entry of the layer"
            );
        }
        count += 1;
    }
    Ok(count)
}

/// Whether an entry name is absolute or climbs out with `..`.
fn escapes(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            std::path::Component::RootDir
                | std::path::Component::Prefix(_)
                | std::path::Component::ParentDir
        )
    })
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

        let mut inputs = Vec::new();
        let source = match (spec.srcs.is_empty(), spec.tar) {
            (true, None) => anyhow::bail!(
                "neither `srcs` nor `tar` is set; give `srcs` (targets whose files go into the \
                 layer, with `prefix`) or `tar` (a target producing a prebuilt tar to use as the \
                 layer)"
            ),
            (false, Some(_)) => anyhow::bail!(
                "both `srcs` and `tar` are set; a layer is either files from `srcs` or one \
                 prebuilt `tar`, not both. Put the files in a layer of their own."
            ),
            (true, Some(tar)) => {
                // An adopted tar is the layer as it is; these three rewrite
                // its paths and modes, which would make it a different tar.
                for (attr, set) in [
                    ("prefix", spec.prefix.is_some()),
                    ("strip", spec.strip.is_some()),
                    ("mode", spec.mode.is_some()),
                ] {
                    anyhow::ensure!(
                        !set,
                        "`{attr}` does not apply with `tar`: an adopted tar is used as it is, \
                         entry names and modes included. Drop `{attr}`, or build the layer from \
                         `srcs`."
                    );
                }
                let r#ref = TargetAddr::parse(&tar, &pkg)
                    .with_context(|| format!("parse `tar` {tar:?}"))?;
                let tar = r#ref.to_string();
                inputs.push(Input {
                    r#ref,
                    mode: InputMode::Standard,
                    origin_id: TAR_ORIGIN.to_string(),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                });
                LayerSource::Tar { tar }
            }
            (false, None) => {
                // An absent `prefix` reads as the empty one, which
                // `normalize_prefix` refuses with the reason it is required.
                let prefix = normalize_prefix(spec.prefix.as_deref().unwrap_or_default())?;
                let mode = spec.mode.as_deref().map(parse_mode).transpose()?;
                let mut srcs = Vec::new();
                for (i, src) in spec.srcs.iter().enumerate() {
                    let r#ref = TargetAddr::parse(src, &pkg)
                        .with_context(|| format!("parse `srcs` entry {src:?}"))?;
                    // The normalized rendering, not the BUILD-file spelling:
                    // `:bin` and `//app:bin` in package `app` are one dep and
                    // must be one key.
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
                LayerSource::Srcs {
                    srcs,
                    prefix,
                    strip: spec.strip,
                    mode,
                }
            }
        };

        let encoding = LayerEncoding::from_compress(spec.compress);
        let out = ws_path(pkg.as_str(), &format!("{}.{}", addr.name, encoding.ext()));
        let descriptor_out = ws_path(pkg.as_str(), &format!("{}{DESCRIPTOR_SUFFIX}", addr.name));
        let def = OciLayerDef {
            out: out.clone(),
            descriptor_out: descriptor_out.clone(),
            encoding,
            source,
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
                    .advertise(hplugin::provider::PoolName::cpu(), encoding.cpu())
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

        // Tar, deflate and sha256 over every byte of the layer: CPU work, off
        // the async worker.
        let encoding = def.encoding;
        let job_out = out_path.clone();
        let diff_id = match def.source {
            LayerSource::Srcs {
                srcs,
                prefix,
                strip,
                mode,
            } => {
                let entries = srcs_entries(&req, srcs.len(), &prefix, strip.as_deref())?;
                hcore::blocking::run(move || write_layer(&job_out, &entries, mode, encoding)).await
            }
            LayerSource::Tar { tar } => {
                let src = tar_file(dep_files(&req, TAR_ORIGIN)?, &req.sandbox_ws_dir, &tar)?;
                hcore::blocking::run(move || {
                    write_blob(&job_out, encoding, |sink| adopt_tar(&src, &tar, sink))
                })
                .await
            }
        }
        .with_context(|| format!("write the layer to {out_path:?}"))?;
        let descriptor = LayerDescriptor {
            blob: out_name
                .to_str()
                .with_context(|| format!("layer file name {out_name:?} is not UTF-8"))?
                .to_string(),
            media_type: encoding,
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

        Ok(ManagedRunResponse::default())
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
        write_layer(&a, &sorted, None, LayerEncoding::Tar).expect("a");
        write_layer(&b, &sorted, None, LayerEncoding::Tar).expect("b");
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
            write_layer(&c, &sorted, None, LayerEncoding::Tar).expect("c");
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
        write_layer(&out, &entries, None, LayerEncoding::Tar).expect("write");

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
            write_layer(
                &dir.path().join("x.tar"),
                &entries,
                None,
                LayerEncoding::Tar
            )
            .expect_err("absolute")
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
        let plain_id = write_layer(&plain, &entries, None, LayerEncoding::Tar).expect("plain");
        let gz_id = write_layer(&gz, &entries, None, LayerEncoding::TarGzip).expect("gz");

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
    /// output on this corpus does — a `miniz_oxide` bump that changes its
    /// matching or block splitting, a header change — and every cached layer
    /// would then keep its old key with new bytes. If this fails on purpose,
    /// bump `OCI_LAYER_FORMAT_VERSION` in the same commit.
    ///
    /// The corpus is built to reach the parts of deflate a tiny input never
    /// does: well over one 64 KiB output chunk, matches across the 32 KiB
    /// window, incompressible stretches (stored blocks), text (dynamic Huffman
    /// tables) and long runs (maximum-length matches), over several entries.
    #[test]
    fn the_gzipped_blob_digest_is_pinned() {
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let noise: Vec<u8> = (0..150_000).map(|_| next().to_le_bytes()[0]).collect();
        const WORDS: [&str; 12] = [
            "layer",
            "image",
            "digest",
            "tar",
            "gzip",
            "window",
            "match",
            "the",
            "of",
            "a",
            "reproducible",
            "heph",
        ];
        let mut text = String::with_capacity(210_000);
        while text.len() < 200_000 {
            let w = WORDS
                .get((next() % WORDS.len() as u64) as usize)
                .copied()
                .unwrap_or("x");
            text.push_str(w);
            text.push(if next() % 9 == 0 { '\n' } else { ' ' });
        }
        let mut runs = vec![0u8; 100_000];
        runs.extend(std::iter::repeat_n(b'a', 70_000));
        runs.extend(b"ab".repeat(20_000));

        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![
            entry(dir.path(), "etc/app.conf", b"k=v\n", false),
            entry(dir.path(), "usr/bin/noise", &noise, true),
            entry(
                dir.path(),
                "usr/share/doc/words.txt",
                text.as_bytes(),
                false,
            ),
            entry(dir.path(), "var/lib/runs", &runs, false),
        ];
        let out = dir.path().join("l.tar.gz");
        write_layer(&out, &entries, None, LayerEncoding::TarGzip).expect("write");
        let blob = std::fs::read(&out).expect("read");
        assert!(
            blob.len() > 2 * 64 * 1024,
            "the blob must span several output chunks, got {} bytes",
            blob.len()
        );
        let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&blob));
        assert_eq!(
            digest, "25d78ef351c42bec82c44c3a305667e50f9c51b89c60c279d2ed0ff8c37f6fb8",
            "the gzipped layer's bytes changed: bump OCI_LAYER_FORMAT_VERSION if deliberate"
        );
    }

    fn parse_layer(pairs: &[(&str, hcore::htvalue::Value)]) -> anyhow::Result<TargetDef> {
        let req = ParseRequest {
            request_id: "test".to_string(),
            target_spec: Arc::new(hplugin::provider::TargetSpec {
                addr: hmodel::htaddr::parse_addr("//app:layer").expect("addr"),
                driver: "oci.layer".to_string(),
                config: pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.clone()))
                    .collect(),
                ..Default::default()
            }),
        };
        let resp = futures::executor::block_on(
            Driver::new().parse(req, &hcore::hasync::StdCancellationToken::new()),
        )?;
        Ok(resp.target_def)
    }

    fn s(v: &str) -> hcore::htvalue::Value {
        hcore::htvalue::Value::String(v.to_string())
    }

    fn list(vs: &[&str]) -> hcore::htvalue::Value {
        hcore::htvalue::Value::List(vs.iter().map(|v| s(v)).collect())
    }

    /// Deflate is CPU work: a gzipped layer reserves a whole CPU by default,
    /// and only a `compress = False` one keeps the IO-bound 0.1.
    #[test]
    fn a_compressed_layer_reserves_a_cpu() {
        let base = [("srcs", list(&[":bin"])), ("prefix", s("/usr/bin"))];
        let gz = parse_layer(&base).expect("gzip");
        assert_eq!(gz.resources.declared("cpu"), Some("1"));

        let mut plain = base.to_vec();
        plain.push(("compress", hcore::htvalue::Value::Bool(false)));
        let plain = parse_layer(&plain).expect("plain");
        assert_eq!(
            plain.resources.declared("cpu"),
            Some(hplugin::driver::Resources::IO_CPU)
        );
        assert_ne!(gz.hash, plain.hash, "the encoding is in the key");
    }

    /// The descriptor's `mediaType` is the encoding's serialization, so the
    /// enum and the media type constants cannot drift apart.
    #[test]
    fn the_encoding_serializes_as_its_media_type() {
        for e in [LayerEncoding::Tar, LayerEncoding::TarGzip] {
            assert_eq!(
                serde_json::to_value(e).expect("encode"),
                serde_json::json!(e.media_type())
            );
        }
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
        write_layer(&out, &entries, None, LayerEncoding::Tar).expect("write");
        assert_eq!(
            members(&out)
                .into_iter()
                .map(|(p, _, _)| p)
                .collect::<Vec<_>>(),
            vec!["a-b".to_string(), "a.b".to_string(), "a/b".to_string()],
            "'-' (0x2D) < '.' (0x2E) < '/' (0x2F)"
        );
    }

    fn parse_err(pairs: &[(&str, hcore::htvalue::Value)]) -> String {
        match parse_layer(pairs) {
            Ok(_) => panic!("must be refused: {pairs:?}"),
            Err(e) => format!("{e:#}"),
        }
    }

    /// `tar` takes one dep, under origin `tar`, at its normalized address —
    /// and nothing that rewrites the tar is accepted beside it.
    #[test]
    fn tar_and_srcs_are_exclusive_and_tar_takes_no_rewrites() {
        let def = parse_layer(&[("tar", s(":rootfs"))]).expect("tar alone");
        let [input] = def.inputs.as_slice() else {
            panic!("one input, got {}", def.inputs.len());
        };
        assert_eq!(input.origin_id, TAR_ORIGIN);
        assert_eq!(input.r#ref.to_string(), "//app:rootfs");
        let spelled = parse_layer(&[("tar", s("//app:rootfs"))]).expect("tar alone");
        assert_eq!(def.hash, spelled.hash, "one dep, one key, however spelled");

        /// A case: its name, the attributes, and the error text.
        type Case<'a> = (&'a str, &'a [(&'a str, hcore::htvalue::Value)], &'a str);
        let cases: &[Case] = &[
            ("neither", &[], "neither `srcs` nor `tar` is set"),
            (
                "both",
                &[("srcs", list(&[":bin"])), ("tar", s(":rootfs"))],
                "both `srcs` and `tar` are set",
            ),
            (
                "tar with prefix",
                &[("tar", s(":rootfs")), ("prefix", s("/usr/bin"))],
                "`prefix` does not apply with `tar`",
            ),
            (
                "tar with strip",
                &[("tar", s(":rootfs")), ("strip", s("cmd"))],
                "`strip` does not apply with `tar`",
            ),
            (
                "tar with mode",
                &[("tar", s(":rootfs")), ("mode", s("0755"))],
                "`mode` does not apply with `tar`",
            ),
            (
                "srcs without prefix",
                &[("srcs", list(&[":bin"]))],
                "`prefix` is empty; give the path inside the image",
            ),
        ];
        for (case, pairs, want) in cases {
            let err = parse_err(pairs);
            assert!(err.contains(want), "{case}: {err}");
        }
    }

    const TAR_DEP: &str = "//app:rootfs";

    /// Adopt `bytes` as the tar `rootfs.tar` from [`TAR_DEP`]; what reached
    /// the sink.
    fn adopt(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("rootfs.tar");
        std::fs::write(&src, bytes).expect("write");
        let mut sink = Vec::new();
        adopt_tar(&src, TAR_DEP, &mut sink)?;
        Ok(sink)
    }

    /// Write `field` into a raw header field: `set_path` and `set_link_name`
    /// refuse the very names these fixtures need.
    fn raw(field: &mut [u8], value: &[u8]) {
        field
            .get_mut(..value.len())
            .expect("fits the header field")
            .copy_from_slice(value);
    }

    /// A tar of one entry whose name and link target are written as raw
    /// bytes, unchecked.
    fn raw_tar(kind: tar::EntryType, name: &[u8], link: &[u8]) -> Vec<u8> {
        let mut h = tar::Header::new_gnu();
        raw(&mut h.as_old_mut().name, name);
        raw(&mut h.as_old_mut().linkname, link);
        h.set_entry_type(kind);
        h.set_size(0);
        h.set_mode(0o644);
        h.set_cksum();
        let mut b = tar::Builder::new(Vec::new());
        b.append(&h, std::io::empty()).expect("append");
        b.into_inner().expect("finish")
    }

    /// The layer is the dep's tar, every byte of it: the entries, the two
    /// zero blocks that end it and the padding to its record size, which the
    /// archive reader stops short of. Whiteouts and a relative symlink that
    /// climbs are kept: they are an adopted layer's to express.
    #[test]
    fn an_adopted_tar_reaches_the_sink_byte_for_byte() {
        let mut b = tar::Builder::new(Vec::new());
        let mut file = tar::Header::new_gnu();
        file.set_size(5);
        file.set_mode(0o755);
        file.set_mtime(1_700_000_000);
        file.set_cksum();
        b.append_data(&mut file, "usr/bin/tool", &b"hello"[..])
            .expect("file");
        let mut wh = tar::Header::new_gnu();
        wh.set_size(0);
        wh.set_cksum();
        b.append_data(&mut wh, "etc/.wh.motd", std::io::empty())
            .expect("whiteout");
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        b.append_link(&mut link, "usr/lib/tool", "../bin/tool")
            .expect("symlink");
        let mut hard = tar::Header::new_gnu();
        hard.set_entry_type(tar::EntryType::Link);
        hard.set_size(0);
        b.append_link(&mut hard, "usr/bin/alias", "usr/bin/tool")
            .expect("hardlink");
        // A body several reads long, of bytes the reader cannot mistake for
        // padding: bodies pass through the tee, not only headers.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let big: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[0] | 1
            })
            .collect();
        let mut data = tar::Header::new_gnu();
        data.set_size(big.len() as u64);
        data.set_mode(0o644);
        data.set_cksum();
        b.append_data(&mut data, "usr/share/blob", big.as_slice())
            .expect("big file");
        let mut bytes = b.into_inner().expect("finish");
        // GNU tar pads to a 10240-byte record; nothing past the first zero
        // block is read by the archive reader.
        bytes.resize(bytes.len().div_ceil(10240) * 10240, 0);

        let sink = adopt(&bytes).expect("a valid tar is adopted");
        assert!(
            sink == bytes,
            "the sink holds the tar exactly ({} bytes, want {})",
            sink.len(),
            bytes.len()
        );

        // Through the gzip writer, whose output crosses its 64 KiB chunk on
        // this incompressible body: the blob inflates to the tar, and the
        // diff_id is the tar's digest.
        use std::io::Read as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("rootfs.tar");
        std::fs::write(&src, &bytes).expect("write");
        let out = dir.path().join("l.tar.gz");
        let diff_id = write_blob(&out, LayerEncoding::TarGzip, |sink| {
            adopt_tar(&src, TAR_DEP, sink)
        })
        .expect("gzip adoption");
        let blob = std::fs::read(&out).expect("read blob");
        assert!(
            blob.len() > 64 * 1024,
            "the blob must cross a chunk, got {} bytes",
            blob.len()
        );
        let mut inflated = Vec::new();
        flate2::read::GzDecoder::new(blob.as_slice())
            .read_to_end(&mut inflated)
            .expect("the blob is gzip");
        assert!(inflated == bytes, "the blob inflates to the tar");
        assert_eq!(
            diff_id,
            format!(
                "sha256:{}",
                hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
            )
        );
    }

    /// Layer `rootfs` adopting `rootfs.tar` with `compress = False` writes its
    /// blob to the path its input sits at. The input is read whole before
    /// the blob replaces it, not truncated by the blob's creation.
    #[test]
    fn a_tar_at_the_blob_path_is_read_before_it_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rootfs.tar");
        let bytes = raw_tar(tar::EntryType::Regular, b"etc/motd", b"");
        std::fs::write(&path, &bytes).expect("write");
        write_blob(&path, LayerEncoding::Tar, |sink| {
            adopt_tar(&path, TAR_DEP, sink)
        })
        .expect("adopted in place");
        assert!(std::fs::read(&path).expect("read") == bytes);
    }

    /// An absolute symlink is adopted: in a container it resolves against
    /// the image's root, and rootfs tars are full of them.
    #[test]
    fn an_absolute_symlink_is_adopted() {
        let bytes = raw_tar(
            tar::EntryType::Symlink,
            b"usr/bin/awk",
            b"/etc/alternatives/awk",
        );
        assert!(adopt(&bytes).expect("adopted") == bytes);
    }

    /// A tar whose one entry has a long name or long link target, carried by
    /// an extension entry `ext` written by hand ahead of it: `set_path` and
    /// `set_link_name` refuse these names. The entry's own header fields
    /// hold short placeholders, which the extension overrides.
    fn long_tar(ext: LongExt, long: &[u8], kind: tar::EntryType) -> Vec<u8> {
        let (ext_kind, body) = match ext {
            LongExt::GnuName => (tar::EntryType::GNULongName, [long, b"\0"].concat()),
            LongExt::GnuLink => (tar::EntryType::GNULongLink, [long, b"\0"].concat()),
            LongExt::PaxPath => (tar::EntryType::XHeader, pax_record(b"path", long)),
            LongExt::PaxLinkpath => (tar::EntryType::XHeader, pax_record(b"linkpath", long)),
        };
        let mut h = tar::Header::new_gnu();
        raw(&mut h.as_old_mut().name, b"././@LongLink");
        h.set_entry_type(ext_kind);
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        let mut b = tar::Builder::new(Vec::new());
        b.append(&h, body.as_slice()).expect("extension");
        let mut e = tar::Header::new_gnu();
        raw(&mut e.as_old_mut().name, b"short");
        if kind == tar::EntryType::Link {
            raw(&mut e.as_old_mut().linkname, b"short");
        }
        e.set_entry_type(kind);
        e.set_size(0);
        e.set_mode(0o644);
        e.set_cksum();
        b.append(&e, std::io::empty()).expect("entry");
        b.into_inner().expect("finish")
    }

    /// Which extension entry carries a long name.
    #[derive(Clone, Copy)]
    enum LongExt {
        GnuName,
        GnuLink,
        PaxPath,
        PaxLinkpath,
    }

    /// One PAX record, `<len> <key>=<value>\n`, whose length counts itself.
    fn pax_record(key: &[u8], value: &[u8]) -> Vec<u8> {
        let rest = key.len() + value.len() + 3; // ' ', '=', '\n'
        let len = (1..=20)
            .map(|digits| rest + digits)
            .find(|n| n.to_string().len() + rest == *n)
            .expect("a length that counts itself");
        [len.to_string().as_bytes(), b" ", key, b"=", value, b"\n"].concat()
    }

    /// Everything an adopted tar is refused for, each naming the dep.
    #[test]
    fn adopted_tars_refuse_what_escapes_the_layer() {
        use tar::EntryType as T;
        let one = raw_tar(T::Regular, b"etc/motd", b"");
        let mut truncated = tar::Header::new_gnu();
        raw(&mut truncated.as_old_mut().name, b"etc/motd");
        truncated.set_size(1024);
        truncated.set_mode(0o644);
        truncated.set_cksum();
        let truncated = [truncated.as_bytes().as_slice(), &[b'x'; 10]].concat();
        let mut dirty_padding = one.clone();
        dirty_padding.resize(10240, 0);
        if let Some(last) = dirty_padding.last_mut() {
            *last = 1;
        }
        let long_abs = [b"/".as_slice(), &b"a/".repeat(60), b"passwd"].concat();
        let long_up = [b"usr/".as_slice(), &[b'x'; 120], b"/../../../etc/passwd"].concat();
        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            (
                "not a tar",
                b"hello, world".to_vec(),
                "which is not a tar, or is cut short",
            ),
            ("garbage block", vec![0xAA; 1024], "which is not a tar"),
            (
                "truncated body",
                truncated,
                "which is not a tar, or is cut short",
            ),
            (
                "truncated header",
                one.get(..300).expect("a header's worth").to_vec(),
                "which is not a tar, or is cut short",
            ),
            (
                "two tars concatenated",
                [one.as_slice(), &one].concat(),
                "which has data after the end of the archive",
            ),
            (
                "non-zero padding",
                dirty_padding,
                "which has data after the end of the archive",
            ),
            (
                "hardlink with no target",
                raw_tar(T::Link, b"usr/bin/x", b""),
                "whose hardlink \"usr/bin/x\" names no target",
            ),
            (
                "GNU long name, absolute",
                long_tar(LongExt::GnuName, &long_abs, T::Regular),
                "is absolute or has a `..` component",
            ),
            (
                "GNU long name, climbing",
                long_tar(LongExt::GnuName, &long_up, T::Regular),
                "is absolute or has a `..` component",
            ),
            (
                "PAX path, absolute",
                long_tar(LongExt::PaxPath, &long_abs, T::Regular),
                "is absolute or has a `..` component",
            ),
            (
                "PAX path, climbing",
                long_tar(LongExt::PaxPath, &long_up, T::Regular),
                "is absolute or has a `..` component",
            ),
            (
                "GNU long hardlink, absolute",
                long_tar(LongExt::GnuLink, &long_abs, T::Link),
                "whose hardlink \"short\" points at \"/a/a/",
            ),
            (
                "GNU long hardlink, climbing",
                long_tar(LongExt::GnuLink, &long_up, T::Link),
                "whose hardlink \"short\" points at \"usr/xxx",
            ),
            (
                "PAX linkpath, absolute",
                long_tar(LongExt::PaxLinkpath, &long_abs, T::Link),
                "whose hardlink \"short\" points at \"/a/a/",
            ),
            (
                "PAX linkpath, climbing",
                long_tar(LongExt::PaxLinkpath, &long_up, T::Link),
                "whose hardlink \"short\" points at \"usr/xxx",
            ),
            (
                "gzip",
                vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0],
                "a gzip stream; `tar` takes an uncompressed tar. Decompress it first",
            ),
            ("zero entries", vec![0; 1024], "a tar with no entries"),
            ("empty file", Vec::new(), "a tar with no entries"),
            (
                "absolute entry",
                raw_tar(T::Regular, b"/etc/passwd", b""),
                "whose entry \"/etc/passwd\" is absolute or has a `..` component",
            ),
            (
                "climbing entry",
                raw_tar(T::Regular, b"usr/../../etc/passwd", b""),
                "whose entry \"usr/../../etc/passwd\" is absolute or has a `..` component",
            ),
            (
                "hardlink out",
                raw_tar(T::Link, b"usr/bin/x", b"../../etc/shadow"),
                "whose hardlink \"usr/bin/x\" points at \"../../etc/shadow\"",
            ),
            (
                "absolute hardlink",
                raw_tar(T::Link, b"usr/bin/x", b"/etc/shadow"),
                "whose hardlink \"usr/bin/x\" points at \"/etc/shadow\"",
            ),
        ];
        for (case, bytes, want) in cases {
            let err = format!("{:#}", adopt(&bytes).expect_err(case));
            assert!(err.contains(want), "{case}: {err}");
            assert!(err.contains(TAR_DEP), "{case} names the dep: {err}");
        }
    }

    /// A symlink climbing with `..` resolves inside the image's root, and is
    /// adopted as a srcs layer would carry it.
    #[test]
    fn a_relative_symlink_may_climb() {
        let bytes = raw_tar(tar::EntryType::Symlink, b"usr/bin/x", b"../../lib/x");
        assert!(adopt(&bytes).expect("adopted") == bytes);
    }

    /// A `tar` dep must produce one file: two (a `glob()` matching both) or
    /// none are refused naming the dep and what it produced, workspace-relative.
    #[test]
    fn a_tar_dep_produces_exactly_one_file() {
        let ws = Path::new("/sandbox/ws");
        let only = ws.join("app/rootfs.tar");
        assert_eq!(
            tar_file(vec![only.clone()], ws, TAR_DEP).expect("one file"),
            only
        );

        let two = format!(
            "{:#}",
            tar_file(vec![only, ws.join("app/extra.tar")], ws, TAR_DEP).expect_err("two files")
        );
        assert!(
            two.contains(&format!(
                "`tar` {TAR_DEP} must produce exactly one file, the tar to adopt; it produced 2: \
                 app/rootfs.tar, app/extra.tar"
            )),
            "got: {two}"
        );

        let none = format!(
            "{:#}",
            tar_file(Vec::new(), ws, TAR_DEP).expect_err("no file")
        );
        assert!(
            none.contains(&format!("`tar` {TAR_DEP} must produce exactly one file")),
            "got: {none}"
        );
        assert!(none.contains("it produced 0: nothing"), "got: {none}");
    }
}
