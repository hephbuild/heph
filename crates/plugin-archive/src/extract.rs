//! The `archive.extract` driver: unpack the one archive file a `src` target
//! produced into a directory output, in-process. What it refuses, and why, is
//! [`harchive::unpack`]; the user-facing description is `docs/ARCHIVE.md`.
//! `heph.gh.release_asset(extract = True)` declares one.

use crate::{FormatArg, check_rel, in_package};
use anyhow::Context as _;
use async_trait::async_trait;
use harchive::{Format, Limits, UnpackOptions};
use hcore::hasync::Cancellable;
use hcore::htvalue::signature::ParamType;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunRequest, ManagedRunResponse, single_dep_file,
};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::{FromSpecValue as _, Spec, TargetSpecCache};
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

/// The driver's local name; the engine reaches it as `archive.extract`.
pub const DRIVER_NAME: &str = "extract";

/// Bump to invalidate every extracted tree when the unpacking rules change
/// what an archive produces (modes, what is refused, link handling, a gz's
/// file name and mode). A change to how an omitted `format` is inferred bumps
/// [`INFERENCE_VERSION`] instead, which moves only the inferring targets.
const EXTRACT_FORMAT_VERSION: u32 = 1;

/// Bump when the rules that infer an omitted `format` change
/// ([`infer_format`], `Format::infer`'s suffixes, `Format::sniff`'s
/// signatures, `harchive::gz_holds_tar`): the same archive can then unpack
/// as another format, so every target that infers must re-key. Hashed only
/// when `format` is omitted, so a target that names its format keeps its
/// key.
///
/// 2: `.tar.xz`/`.txz` and `.gz` suffixes, the xz signature, and the longest
/// suffix winning; a `.gz` whose inflated head carries tar's `ustar` magic
/// at offset 257 is a `tar.gz` (as it sniffed under 1), any other `.gz` a
/// bare `gz`.
const INFERENCE_VERSION: u32 = 2;

/// The dep input's origin id.
const SRC_ORIGIN: &str = "extract:src";

/// Config for an `extract` target.
#[derive(Spec)]
struct ExtractSpec {
    /// Address of the target whose single output file is the archive.
    #[spec(required)]
    src: String,
    /// Archive format: `tar`, `tar.gz`, `tar.xz`, `tar.zst`, `zip`, or `gz`
    /// (one gzipped file, not a tar). When omitted, inferred at run from the
    /// archive file's name suffix, else from its leading bytes.
    #[spec(ty = FormatArg::spec_param_type())]
    format: Option<FormatArg>,
    /// Leading path components dropped from every entry, as `tar
    /// --strip-components`. An entry with no more components than this is
    /// skipped; an archive from which nothing remains is an error.
    strip_components: u32,
    /// With a `gz` only: make its one file executable. gzip records no mode;
    /// an archive format's entries carry their own, so it is refused with one.
    executable: bool,
    /// Output directory, relative to the target's package. Defaults to the
    /// target name.
    #[spec(ty = ParamType::String)]
    out: Option<String>,
    /// The most bytes the archive may unpack to (default 4 GiB). Exceeding it
    /// fails the run; raise it for an archive that is meant to be larger.
    #[spec(ty = ParamType::Int)]
    max_bytes: Option<NonZeroU64>,
    /// The most entries the archive may hold (default 1,000,000).
    #[spec(ty = ParamType::Int)]
    max_entries: Option<NonZeroU64>,
    /// Cache the extracted tree locally and remotely (default). The tree is a
    /// pure function of the archive's bytes, which reach the key through
    /// `src`. `cache = False` disables both tiers; the dict form
    /// (`cache = {"remote": False}`) keeps it local-only.
    cache: TargetSpecCache,
    /// What the target reserves from the resource pools while it runs, e.g.
    /// `{"cpu": 2}`. Defaults to 1 cpu (decompression is CPU-bound). Never
    /// hashed.
    resources: hplugin::driver::Resources,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ExtractDef {
    /// `None`: inferred at run by [`infer_format`].
    format: Option<Format>,
    strip_components: u32,
    /// A `gz`'s file is `0o755`. Absent from a def serialized before it was.
    #[serde(default)]
    executable: bool,
    limits: Limits,
    /// Package-relative output directory.
    out: String,
}

pub struct Driver;

#[async_trait]
impl ManagedDriver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        ExtractSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let addr = &req.target_spec.addr;
        let spec = ExtractSpec::from(&req.target_spec.config).context("parse extract config")?;
        let out = spec.out.unwrap_or_else(|| addr.name.clone());
        check_rel(&out).with_context(|| format!("{}: `out`", addr.format()))?;
        let format = spec.format.map(|f| f.0);
        // With `format` omitted, both are checked at run once the format is
        // known.
        if let Some(f) = format {
            if spec.executable && !f.takes_executable() {
                anyhow::bail!(
                    "{}: `executable` applies only to `format = \"gz\"`; a {} records each \
                     entry's mode",
                    addr.format(),
                    f.as_str()
                );
            }
            if f == Format::Gz && spec.strip_components > 0 {
                anyhow::bail!(
                    "{}: `strip_components` with `format = \"gz\"` would remove its one file, \
                     which has no directory to strip; drop it",
                    addr.format()
                );
            }
        }

        let pkg = addr.package.clone();
        let src = TargetAddr::parse(&spec.src, &pkg)
            .with_context(|| format!("parsing extract src {:?}", spec.src))?;
        let out_ws = in_package(pkg.as_str(), &out);

        let defaults = Limits::default();
        let limits = Limits {
            max_bytes: spec.max_bytes.map_or(defaults.max_bytes, NonZeroU64::get),
            max_entries: spec
                .max_entries
                .map_or(defaults.max_entries, NonZeroU64::get),
        };

        let mut h = Xxh3::new();
        h.update(&EXTRACT_FORMAT_VERSION.to_le_bytes());
        h.update(addr.format().as_bytes());
        h.update(b"\0");
        // The archive's bytes reach the key through the dep's hashout, so
        // only the address goes in here — all of it: `|group` and `[filters]`
        // pick which of the dep's files are the archive.
        // For a gz, the output file's name derives from the dep's file name,
        // which is keyed through the dep's hashout too.
        h.update(src.to_string().as_bytes());
        h.update(b"\0");
        hash_format(&mut h, format, INFERENCE_VERSION);
        h.update(&spec.strip_components.to_le_bytes());
        h.update(out.as_bytes());
        // Only when set, so every key from before the option stays put. `out`
        // holds no NUL (`check_rel`), so the separator keeps the two apart.
        if spec.executable {
            h.update(b"\0executable");
        }
        // Not `limits`: they decide only whether the run fails, and a failure
        // is never cached; when it succeeds the tree is the same whatever the
        // limits were, so raising one must not re-extract every archive.
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(ExtractDef {
                    format,
                    strip_components: spec.strip_components,
                    executable: spec.executable,
                    limits,
                    out,
                }),
                inputs: vec![Input {
                    r#ref: src,
                    mode: InputMode::Standard,
                    origin_id: SRC_ORIGIN.to_string(),
                    annotations: BTreeMap::new(),
                    hashed: true,
                    runtime: true,
                }],
                outputs: vec![Output {
                    group: String::new(),
                    paths: vec![Path {
                        content: Content::DirPath(out_ws),
                        codegen_tree: CodegenMode::None,
                        collect: true,
                    }],
                }],
                hash,
                // A pure function of the archive's bytes (and `hash` above).
                cache: spec.cache.into(),
                // Not in `hash` above: a declaration moves no key.
                resources: spec.resources,
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
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<ExtractDef>();
        let addr = req.request.target.addr.format();
        let src =
            single_dep_file(&req.inputs, SRC_ORIGIN).with_context(|| format!("{addr}: `src`"))?;
        let dest = req.sandbox_pkg_dir.join(&def.out);
        let format = def.format;
        let opts = UnpackOptions {
            strip_components: def.strip_components,
            executable: def.executable,
            limits: def.limits,
        };

        // Reading the list, sniffing, decompressing and writing are blocking
        // work; off the async workers. The token is polled on every read, so
        // a cancelled run stops within one chunk instead of unpacking the
        // rest.
        let token = ctoken.clone_arc();
        let cx = addr.clone();
        hcore::blocking::run(move || {
            let archive = src.single_file().with_context(|| format!("{cx}: `src`"))?;
            let format = match format {
                Some(f) => f,
                None => {
                    let f = infer_format(&archive).with_context(|| format!("{cx}: `format`"))?;
                    check_inferred(f, &archive, opts).with_context(|| format!("{cx}: `format`"))?;
                    f
                }
            };
            harchive::unpack(&archive, format, opts, &dest, &|| token.is_cancelled())
                .with_context(|| format!("{cx}: extract {}", format.as_str()))
        })
        .await?;

        Ok(ManagedRunResponse::default())
    }
}

/// The format choice's part of the def hash. An inferred format is a function
/// of the archive file's name and bytes, both in the dep's hashout, and of the
/// inference rules, which `inference_version` stands for; a given format is
/// its spelling, and the rules' version stays out of its key.
fn hash_format(h: &mut Xxh3, format: Option<Format>, inference_version: u32) {
    match format {
        None => {
            h.update(&[0]);
            h.update(&inference_version.to_le_bytes());
        }
        Some(f) => {
            h.update(&[1]);
            h.update(f.as_str().as_bytes());
            h.update(b"\0");
        }
    }
}

/// The options parse checks against a named format, checked against an
/// inferred one, saying it was inferred: the author set no `format`, so the
/// one the error names is the archive's, and `format` is the fix.
fn check_inferred(
    format: Format,
    archive: &std::path::Path,
    opts: UnpackOptions,
) -> anyhow::Result<()> {
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    if opts.executable && !format.takes_executable() {
        anyhow::bail!(
            "`executable` applies only to a bare `gz`, and the format was inferred as {} from \
             {name:?}; set `format = \"gz\"` if it is one gzipped file, or drop `executable`",
            format.as_str()
        );
    }
    if format == Format::Gz && opts.strip_components > 0 {
        anyhow::bail!(
            "the format was inferred as gz (one gzipped file) from {name:?}, and \
             `strip_components` would remove that file; drop it, or set `format` if the \
             archive is a tar"
        );
    }
    Ok(())
}

/// The format of the archive at `path`, for an `extract` with no `format`:
/// its file name's suffix, else its leading bytes. A `.gz` name is a bare
/// `gz` unless the stream inflates to a tar (`harchive::gz_holds_tar`), as
/// a `.gz`-named tarball sniffed as a `tar.gz` before names inferred `gz`.
/// The name and the bytes are the dep's content, which its hashout covers,
/// so the answer is keyed.
fn infer_format(path: &std::path::Path) -> anyhow::Result<Format> {
    use std::io::Read as _;

    if let Some(f) = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(Format::infer)
    {
        if f == Format::Gz && harchive::gz_holds_tar(path)? {
            return Ok(Format::TarGz);
        }
        return Ok(f);
    }
    let mut head = Vec::with_capacity(Format::SNIFF_LEN);
    std::fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(Format::SNIFF_LEN as u64)
        .read_to_end(&mut head)
        .with_context(|| format!("read {}", path.display()))?;
    Format::sniff(&head).with_context(|| {
        format!(
            "cannot tell the archive format of {}: its name has no archive suffix and its first \
             bytes are no archive's; set `format` (one of: {})",
            path.display(),
            Format::spellings().join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htaddr::parse_addr;
    use hplugin::provider::TargetSpec;
    use std::collections::HashMap;

    async fn parse(addr: &str, cfg: &[(&str, Value)]) -> anyhow::Result<TargetDef> {
        let config: HashMap<String, Value> = cfg
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        Ok(Driver
            .parse(
                ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(TargetSpec {
                        addr: parse_addr(addr)?,
                        driver: DRIVER_NAME.to_string(),
                        config,
                        ..Default::default()
                    }),
                },
                &StdCancellationToken::new(),
            )
            .await?
            .target_def)
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    #[tokio::test]
    async fn src_is_a_hashed_input_and_out_a_package_dir() {
        let def = parse(
            "//pkg:tool",
            &[("src", s(":tool_archive")), ("format", s("tar.gz"))],
        )
        .await
        .unwrap();
        assert_eq!(def.inputs.len(), 1);
        assert_eq!(def.inputs[0].r#ref.r#ref.format(), "//pkg:tool_archive");
        assert!(def.inputs[0].hashed);
        assert!(matches!(
            &def.outputs[0].paths[0].content,
            Content::DirPath(p) if p == "pkg/tool"
        ));
    }

    #[tokio::test]
    async fn every_option_moves_the_hash_and_resources_do_not() {
        let base = [("src", s(":a")), ("format", s("tar.gz"))];
        let hash = |extra: Vec<(&'static str, Value)>| {
            let mut cfg = base.to_vec();
            cfg.extend(extra);
            async move { parse("//pkg:t", &cfg).await.unwrap().hash }
        };
        let plain = hash(vec![]).await;
        for (k, v) in [
            ("format", s("zip")),
            ("strip_components", Value::Int(1)),
            ("out", s("elsewhere")),
            ("src", s(":b")),
        ] {
            assert_ne!(
                hash(vec![(k, v)]).await,
                plain,
                "{k} must reach the def hash"
            );
        }
        let heavy = hash(vec![(
            "resources",
            Value::Map([("cpu".to_string(), Value::Int(4))].into()),
        )])
        .await;
        assert_eq!(heavy, plain, "resources never reach the hash");
        // Exceeding a limit fails, and a failure is never cached.
        for (k, v) in [("max_bytes", Value::Int(1)), ("max_entries", Value::Int(1))] {
            assert_eq!(
                hash(vec![(k, v)]).await,
                plain,
                "{k} never reaches the hash"
            );
        }
    }

    /// The whole `src` reaches the hash: a group or a filter picks another
    /// file of the same target as the archive.
    #[tokio::test]
    async fn the_src_group_and_filters_reach_the_hash() {
        let hash = |src: &'static str| async move {
            parse("//pkg:t", &[("src", s(src)), ("format", s("tar"))])
                .await
                .unwrap()
                .hash
        };
        assert_ne!(hash(":a|x").await, hash(":a|y").await);
        assert_ne!(hash(":a|x").await, hash(":a").await);
        assert_ne!(hash(":a[x.tar]").await, hash(":a[y.tar]").await);
    }

    #[tokio::test]
    async fn extract_is_cached_unless_told_otherwise() {
        let base = [("src", s(":a")), ("format", s("tar"))];
        let def = parse("//pkg:t", &base).await.unwrap();
        assert!(def.cache.enabled, "cached by default");
        let mut off = base.to_vec();
        off.push(("cache", Value::Bool(false)));
        assert!(!parse("//pkg:t", &off).await.unwrap().cache.enabled);
    }

    #[tokio::test]
    async fn limits_reach_the_def() {
        let def = parse(
            "//pkg:t",
            &[
                ("src", s(":a")),
                ("format", s("tar")),
                ("max_bytes", Value::Int(7)),
            ],
        )
        .await
        .unwrap();
        // Across the ABI the def travels serialized; read it back that way.
        let json = serde_json::to_value(def.def_de::<ExtractDef>()).unwrap();
        assert_eq!(json["format"], "tar");
        let raw: ExtractDef = serde_json::from_value(json).unwrap();
        assert_eq!(raw.limits.max_bytes, 7);
        assert_eq!(raw.limits.max_entries, Limits::DEFAULT_MAX_ENTRIES);
        assert_eq!(raw.format, Some(Format::Tar));
    }

    /// `format` is optional: omitted, the def carries none (inferred at run)
    /// and the hash says so; given, it is the def's and the hash's.
    #[tokio::test]
    async fn format_is_optional_and_hashed_when_given() {
        let inferred = parse("//pkg:t", &[("src", s(":a"))]).await.unwrap();
        let json = serde_json::to_value(inferred.def_de::<ExtractDef>()).unwrap();
        let raw: ExtractDef = serde_json::from_value(json).unwrap();
        assert_eq!(raw.format, None);
        let mut hashes = vec![inferred.hash];
        for f in Format::ALL {
            let def = parse("//pkg:t", &[("src", s(":a")), ("format", s(f.as_str()))])
                .await
                .unwrap();
            hashes.push(def.hash);
        }
        let distinct: std::collections::BTreeSet<_> = hashes.iter().collect();
        assert_eq!(
            distinct.len(),
            hashes.len(),
            "inferring and each format differ"
        );
    }

    /// A change to the inference rules (a bumped [`INFERENCE_VERSION`])
    /// re-keys every target that infers its format, and no target that names
    /// one.
    #[test]
    fn the_inference_rules_version_keys_only_an_inferred_format() {
        let key = |format: Option<Format>, version: u32| {
            let mut h = Xxh3::new();
            hash_format(&mut h, format, version);
            h.digest128()
        };
        assert_ne!(
            key(None, INFERENCE_VERSION),
            key(None, INFERENCE_VERSION + 1),
            "an inferred format's key must cover the inference rules"
        );
        for f in Format::ALL {
            assert_eq!(
                key(Some(f), INFERENCE_VERSION),
                key(Some(f), INFERENCE_VERSION + 1),
                "naming {} must keep the key whatever the inference rules",
                f.as_str()
            );
        }
    }

    /// Inference: the name's suffix first (it is what the author sees), the
    /// leading bytes when the name says nothing, and an error naming
    /// `format` when neither does.
    #[test]
    fn the_format_is_inferred_from_the_name_else_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut ustar = vec![0u8; 512];
        ustar[257..262].copy_from_slice(b"ustar");
        let gzip = [0x1f, 0x8b, 0x08, 0];
        for (name, body, want) in [
            // The name wins, whatever the bytes.
            ("a.tar.gz", &b"junk"[..], Format::TarGz),
            ("A.TGZ", &b"junk"[..], Format::TarGz),
            ("a.zip", &gzip[..], Format::Zip),
            ("a.tar", &b""[..], Format::Tar),
            // No archive suffix: the bytes.
            ("blob", &gzip[..], Format::TarGz),
            ("blob.bin", &[0x28, 0xb5, 0x2f, 0xfd, 0][..], Format::TarZst),
            ("blob", &b"PK\x03\x04...."[..], Format::Zip),
            ("tool.tar.gz.download", &ustar[..], Format::Tar),
            ("blob", &b"\xfd7zXZ\x00\x00\x04"[..], Format::TarXz),
            ("a.tar.xz", &b"junk"[..], Format::TarXz),
            ("a.txz", &b"junk"[..], Format::TarXz),
            // A `.gz` that is not a `.tar.gz` is one gzipped file.
            ("tool-linux.gz", &gzip[..], Format::Gz),
            ("a.tar.gz", &gzip[..], Format::TarGz),
        ] {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            assert_eq!(infer_format(&p).unwrap(), want, "{name}");
            std::fs::remove_file(&p).unwrap();
        }
        for (name, body) in [("blob", &b"#!/bin/sh\n"[..]), ("empty", &b""[..])] {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            let msg = format!("{:#}", infer_format(&p).unwrap_err());
            assert!(msg.contains("set `format`"), "{name}: {msg}");
        }
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    /// A `.gz` that inflates to a tar is a `tar.gz`, as it sniffed before
    /// `.gz` named a bare `gz`; any other `.gz` is a bare `gz`. The name
    /// alone decides no more than that.
    #[test]
    fn a_gz_name_is_a_tar_gz_when_it_inflates_to_a_tar() {
        let dir = tempfile::tempdir().unwrap();
        let mut ustar = vec![0u8; 1024];
        ustar[257..262].copy_from_slice(b"ustar");
        for (name, body, want) in [
            ("tool.gz", gzip(&ustar), Format::TarGz),
            ("TOOL.GZ", gzip(&ustar), Format::TarGz),
            ("tool.gz", gzip(b"\x7fELF a binary"), Format::Gz),
            ("tool.gz", gzip(b""), Format::Gz),
            // Not gzip at all: still the name's `gz`, which fails at run.
            ("tool.gz", b"plain".to_vec(), Format::Gz),
            // A `.tar.gz` is one whatever it holds.
            ("tool.tar.gz", gzip(b"\x7fELF"), Format::TarGz),
        ] {
            let p = dir.path().join(name);
            std::fs::write(&p, &body).unwrap();
            assert_eq!(infer_format(&p).unwrap(), want, "{name}: {body:?}");
            std::fs::remove_file(&p).unwrap();
        }
    }

    /// With `format` omitted, the options parse checks against a named
    /// format are checked once it is inferred, and the error says so.
    #[test]
    fn options_are_checked_against_an_inferred_format() {
        let archive = std::path::Path::new("/dl/tool.tar.xz");
        let exe = UnpackOptions {
            executable: true,
            ..UnpackOptions::default()
        };
        let msg = format!(
            "{:#}",
            check_inferred(Format::TarXz, archive, exe).unwrap_err()
        );
        assert!(
            msg.contains("inferred as tar.xz from \"tool.tar.xz\"")
                && msg.contains("set `format = \"gz\"`"),
            "{msg}"
        );
        check_inferred(Format::Gz, archive, exe).unwrap();
        let strip = UnpackOptions {
            strip_components: 1,
            ..UnpackOptions::default()
        };
        let msg = format!(
            "{:#}",
            check_inferred(Format::Gz, archive, strip).unwrap_err()
        );
        assert!(msg.contains("inferred as gz"), "{msg}");
        check_inferred(Format::TarGz, archive, strip).unwrap();
        check_inferred(Format::Zip, archive, UnpackOptions::default()).unwrap();
    }

    /// The def hash of a target that names its format, rebuilt from the
    /// bytes it hashed before `tar.xz`, `gz` and `executable` existed: those
    /// must not move an existing key, `executable = False` included.
    #[tokio::test]
    async fn an_existing_target_keeps_its_def_hash() {
        let pinned = |strip: u32, out: &str| {
            let mut h = Xxh3::new();
            h.update(&EXTRACT_FORMAT_VERSION.to_le_bytes());
            h.update(b"//pkg:t");
            h.update(b"\0");
            h.update(b"//pkg:a");
            h.update(b"\0");
            h.update(&[1]);
            h.update(b"tar.gz");
            h.update(b"\0");
            h.update(&strip.to_le_bytes());
            h.update(out.as_bytes());
            format!("{:016x}", h.digest()).into_bytes()
        };
        let base = || vec![("src", s(":a")), ("format", s("tar.gz"))];
        let def = parse("//pkg:t", &base()).await.unwrap();
        assert_eq!(def.hash, pinned(0, "t"));

        let mut cfg = base();
        cfg.extend([
            ("strip_components", Value::Int(1)),
            ("out", s("o")),
            ("executable", Value::Bool(false)),
        ]);
        let def = parse("//pkg:t", &cfg).await.unwrap();
        assert_eq!(def.hash, pinned(1, "o"));
    }

    /// `strip_components` with a named `gz` could only ever fail at run: its
    /// one file is all there is.
    #[tokio::test]
    async fn strip_components_with_a_named_gz_is_refused_at_parse() {
        let cfg = [
            ("src", s(":a")),
            ("format", s("gz")),
            ("strip_components", Value::Int(1)),
        ];
        let Err(e) = parse("//pkg:t", &cfg).await else {
            panic!("`strip_components` with a named gz must be refused")
        };
        let msg = format!("{e:#}");
        assert!(
            msg.contains("`strip_components` with `format = \"gz\"`"),
            "{msg}"
        );
        // Inferred, it is checked at run, once the format is known.
        parse("//pkg:t", &[cfg[0].clone(), cfg[2].clone()])
            .await
            .unwrap();
    }

    /// `executable` is a `gz`'s: refused at parse with a named archive
    /// format, kept with `gz` or an inferred format (the unpacker refuses it
    /// at run if that is not a `gz`), and keyed only when set.
    #[tokio::test]
    async fn executable_is_a_gz_option_and_keyed_only_when_set() {
        let def = |cfg: Vec<(&'static str, Value)>| async move {
            let mut full = vec![("src", s(":a"))];
            full.extend(cfg);
            parse("//pkg:t", &full).await
        };
        let exe = ("executable", Value::Bool(true));
        for f in Format::ALL.into_iter().filter(|f| *f != Format::Gz) {
            let Err(e) = def(vec![("format", s(f.as_str())), exe.clone()]).await else {
                panic!("{}: `executable` must be refused", f.as_str())
            };
            let msg = format!("{e:#}");
            assert!(msg.contains("only to `format = \"gz\"`"), "{msg}");
        }
        for format in [vec![("format", s("gz"))], vec![]] {
            let plain = def(format.clone()).await.unwrap();
            let mut with = format.clone();
            with.push(exe.clone());
            let marked = def(with).await.unwrap();
            assert_ne!(plain.hash, marked.hash, "`executable` reaches the key");
            let mut off = format;
            off.push(("executable", Value::Bool(false)));
            assert_eq!(
                def(off).await.unwrap().hash,
                plain.hash,
                "`executable = False` is the default and keeps the key"
            );
            let raw = marked.def_de::<ExtractDef>();
            assert!(raw.executable);
        }
    }

    /// A def serialized before `executable` existed still deserializes.
    #[test]
    fn a_def_without_executable_reads_as_false() {
        let raw: ExtractDef = serde_json::from_value(serde_json::json!({
            "format": "tar",
            "strip_components": 0,
            "limits": Limits::default(),
            "out": "o",
        }))
        .unwrap();
        assert!(!raw.executable);
    }

    #[tokio::test]
    async fn bad_options_are_refused_at_parse() {
        for (cfg, want) in [
            (
                vec![("src", s(":a")), ("format", s("rar"))],
                "unknown archive format",
            ),
            (vec![("format", s("tar"))], "src"),
            (vec![("src", s(":a")), ("format", Value::Int(1))], "format"),
            (
                vec![("src", s(":a")), ("format", s("tar")), ("out", s("../x"))],
                "`..`",
            ),
            (
                vec![("src", s(":a")), ("format", s("tar")), ("out", s("/abs"))],
                "relative",
            ),
            // Would hash as `out = "o", executable = True`.
            (
                vec![
                    ("src", s(":a")),
                    ("format", s("gz")),
                    ("out", s("o\0executable")),
                ],
                "NUL",
            ),
        ] {
            let Err(e) = parse("//pkg:t", &cfg).await else {
                panic!("{cfg:?} must be refused")
            };
            let msg = format!("{e:#}");
            assert!(msg.contains(want), "{cfg:?}: {msg}");
        }
    }
}
