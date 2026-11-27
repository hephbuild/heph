//! The `archive.extract` driver: unpack the one archive file a `src` target
//! produced into a directory output, in-process. What it refuses, and why, is
//! [`harchive::unpack`]; the user-facing description is `docs/ARCHIVE.md`.
//! `heph.gh.release_asset(extract = True)` declares one.

use crate::{FormatArg, check_rel, in_package};
use anyhow::Context as _;
use async_trait::async_trait;
use harchive::{Format, Globs, Limits, UnpackOptions};
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
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

/// Bump when what a glob pattern matches changes: a wax upgrade, or a change
/// to `harchive::Globs`'s matcher or to the path it is matched against. The
/// key holds only the pattern strings, so without this an upgrade that made
/// `bin/*` match something else would serve the old tree. Hashed inside each
/// pattern section (`include`, an `executable` list), so a target without
/// patterns keeps its key. `harchive`'s `matching_is_pinned` test fails when
/// an answer moves.
const GLOB_VERSION: u32 = 1;

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
    /// Glob patterns (a string or a list, the syntax of `group`'s `include`)
    /// over each entry's path after `strip_components`: only the files and
    /// links that match one are extracted, and each pattern must match one.
    /// Empty (the default): every entry.
    include: Vec<String>,
    /// `True`: every extracted file is executable (`0o755`), as a gz's one
    /// file or a zip built on Windows needs. A list of glob patterns: only
    /// the files that match one, and each must match one. The bit is only
    /// ever added; an archive's own `0o755` stays.
    #[spec(ty = ExecutableArg::spec_param_type())]
    executable: ExecutableArg,
    /// Output directory, relative to the target's package. Defaults to the
    /// target name.
    #[spec(ty = ParamType::String)]
    out: Option<String>,
    /// The most bytes the archive may decode to for its entries (default
    /// 4 GiB), written or read past: in a tar, an entry `include` or
    /// `strip_components` leaves out is decoded to reach the next one, and
    /// counts. Exceeding it fails the run; raise it for an archive that is
    /// meant to be larger.
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
    /// The `include` patterns, sorted and deduped. Absent from a def
    /// serialized before it existed.
    #[serde(default)]
    include: Vec<String>,
    /// Absent from a def serialized before it existed; a bool in one
    /// serialized before it took patterns.
    #[serde(default)]
    executable: ExecutableArg,
    limits: Limits,
    /// Package-relative output directory.
    out: String,
}

/// The `executable` option: a bool, or glob patterns naming the files to
/// make executable. A pattern list is sorted and deduped, and an empty one is
/// `Bool(false)`, so equal choices are one value (and one key).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum ExecutableArg {
    Bool(bool),
    Patterns(Vec<String>),
}

impl Default for ExecutableArg {
    fn default() -> Self {
        Self::Bool(false)
    }
}

impl ExecutableArg {
    /// What [`harchive::unpack`] does with it, compiling the patterns.
    fn to_unpack(&self) -> anyhow::Result<harchive::Executable> {
        Ok(match self {
            Self::Bool(false) => harchive::Executable::Archive,
            Self::Bool(true) => harchive::Executable::All,
            // Never parsed (an empty list is `Bool(false)`), but a def is
            // data: no pattern means the archive's modes.
            Self::Patterns(p) if p.is_empty() => harchive::Executable::Archive,
            Self::Patterns(p) => harchive::Executable::Matching(Globs::new(p)?),
        })
    }
}

impl hplugin::htspec::FromSpecValue for ExecutableArg {
    fn from_spec_value(v: &Value) -> anyhow::Result<Self> {
        match v {
            Value::Null() => Ok(Self::default()),
            Value::Bool(b) => Ok(Self::Bool(*b)),
            Value::String(_) | Value::List(_) => {
                let patterns = normalize(Vec::<String>::from_spec_value(v)?);
                if patterns.is_empty() {
                    return Ok(Self::Bool(false));
                }
                Globs::new(&patterns)?;
                Ok(Self::Patterns(patterns))
            }
            other => anyhow::bail!("{EXECUTABLE_TYPE_ERROR}, got {other:?}"),
        }
    }

    fn spec_param_type() -> ParamType {
        ParamType::union(vec![
            ParamType::Bool,
            ParamType::String,
            ParamType::list(ParamType::String),
        ])
    }
}

/// What a wrong `executable` value is told, word for word what
/// `heph.gh.release_asset`'s says (`crates/plugin-gh/src/release.rs`): the
/// one option, two entry points.
const EXECUTABLE_TYPE_ERROR: &str =
    "`executable` must be a bool, a glob pattern or a list of glob patterns";

/// Patterns sorted and deduped: their order and repetition change nothing
/// the unpacker does, so they must change no key.
fn normalize(mut patterns: Vec<String>) -> Vec<String> {
    patterns.sort_unstable();
    patterns.dedup();
    patterns
}

/// Hash a pattern list, after its section's marker: [`GLOB_VERSION`] (a
/// pattern's meaning is wax's, not in its text), its length, then each
/// pattern behind a NUL. The length keeps a list from running into what is
/// hashed after it (a pattern holds no NUL, but may spell the next section's
/// marker).
fn hash_patterns(h: &mut Xxh3, patterns: &[String]) {
    h.update(&GLOB_VERSION.to_le_bytes());
    h.update(&(patterns.len() as u64).to_le_bytes());
    for p in patterns {
        h.update(b"\0");
        h.update(p.as_bytes());
    }
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
        // With `format` omitted, checked at run once the format is known.
        if format == Some(Format::Gz) && spec.strip_components > 0 {
            anyhow::bail!(
                "{}: `strip_components` with `format = \"gz\"` would remove its one file, \
                 which has no directory to strip; drop it",
                addr.format()
            );
        }
        let include = normalize(spec.include);
        Globs::new(&include).with_context(|| format!("{}: `include`", addr.format()))?;
        let executable = spec.executable;

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
        // Each only when set, so every key from before the option stays put.
        // `out` holds no NUL (`check_rel`), so the separator keeps the two
        // apart. `True` is exactly what it hashed as before patterns existed;
        // a pattern list follows the marker with a tag byte a bare `True`
        // never has after it (nothing, or `include`'s NUL). Each pattern
        // section carries `GLOB_VERSION` (`hash_patterns`): the key holds the
        // patterns' text, and a wax upgrade that changes what one matches
        // bumps it.
        match &executable {
            ExecutableArg::Bool(false) => {}
            ExecutableArg::Bool(true) => h.update(b"\0executable"),
            ExecutableArg::Patterns(p) => {
                h.update(b"\0executable\x01");
                hash_patterns(&mut h, p);
            }
        }
        if !include.is_empty() {
            h.update(b"\0include");
            hash_patterns(&mut h, &include);
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
                    include,
                    executable,
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
        // Compiled again from the def's strings, which parse checked.
        let opts = UnpackOptions {
            strip_components: def.strip_components,
            include: Globs::new(&def.include).with_context(|| format!("{addr}: `include`"))?,
            executable: def
                .executable
                .to_unpack()
                .with_context(|| format!("{addr}: `executable`"))?,
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
                    check_inferred(f, &archive, &opts)
                        .with_context(|| format!("{cx}: `format`"))?;
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
    opts: &UnpackOptions,
) -> anyhow::Result<()> {
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
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
        let archive = std::path::Path::new("/dl/tool.gz");
        let strip = UnpackOptions {
            strip_components: 1,
            ..UnpackOptions::default()
        };
        let msg = format!(
            "{:#}",
            check_inferred(Format::Gz, archive, &strip).unwrap_err()
        );
        assert!(
            msg.contains("inferred as gz") && msg.contains("from \"tool.gz\""),
            "{msg}"
        );
        check_inferred(Format::TarGz, archive, &strip).unwrap();
        check_inferred(Format::Zip, archive, &UnpackOptions::default()).unwrap();
        // `executable` holds for every format, inferred or not.
        let exe = UnpackOptions {
            executable: harchive::Executable::All,
            ..UnpackOptions::default()
        };
        for f in Format::ALL {
            check_inferred(f, archive, &exe).unwrap();
        }
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

        // An empty pattern list is no list: no section, no `GLOB_VERSION`.
        let mut cfg = base();
        cfg.extend([
            ("include", Value::List(vec![])),
            ("executable", Value::List(vec![])),
        ]);
        let def = parse("//pkg:t", &cfg).await.unwrap();
        assert_eq!(def.hash, pinned(0, "t"));
    }

    /// The bytes a target with both pattern lists hashes, written out: a
    /// refactor of the framing (the markers, `GLOB_VERSION`'s place, the
    /// lengths) fails here rather than silently re-keying every target.
    #[tokio::test]
    async fn a_target_with_patterns_keeps_its_def_hash() {
        let mut h = Xxh3::new();
        h.update(&1u32.to_le_bytes()); // EXTRACT_FORMAT_VERSION
        h.update(b"//pkg:t\0//pkg:a\0");
        h.update(&[1]);
        h.update(b"tar.gz\0");
        h.update(&0u32.to_le_bytes()); // strip_components
        h.update(b"t");
        h.update(b"\0executable\x01");
        h.update(&1u32.to_le_bytes()); // GLOB_VERSION
        h.update(&1u64.to_le_bytes());
        h.update(b"\0bin/*");
        h.update(b"\0include");
        h.update(&1u32.to_le_bytes()); // GLOB_VERSION
        h.update(&2u64.to_le_bytes());
        h.update(b"\0bin/*\0lib/**");
        let want = format!("{:016x}", h.digest()).into_bytes();

        let def = parse(
            "//pkg:t",
            &[
                ("src", s(":a")),
                ("format", s("tar.gz")),
                ("include", Value::List(vec![s("lib/**"), s("bin/*")])),
                ("executable", s("bin/*")),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            def.hash, want,
            "the def hash of a target with patterns moved; if that is meant, every such \
             target re-keys"
        );
        assert_eq!(GLOB_VERSION, 1, "a GLOB_VERSION bump updates the pin above");
    }

    /// A def holding an empty pattern list (none is parsed so, but a def is
    /// data) unpacks with the archive's modes.
    #[test]
    fn an_empty_executable_list_is_the_archives_modes() {
        assert!(matches!(
            ExecutableArg::Patterns(vec![]).to_unpack().unwrap(),
            harchive::Executable::Archive
        ));
    }

    /// `heph.gh.release_asset` declares this option with its own type, and
    /// says the same: the two are pinned to the same literal.
    #[test]
    fn executable_accepts_and_refuses_as_release_asset_does() {
        use hplugin::htspec::FromSpecValue as _;
        assert_eq!(
            ExecutableArg::spec_param_type(),
            ParamType::union(vec![
                ParamType::Bool,
                ParamType::String,
                ParamType::list(ParamType::String),
            ])
        );
        let e = ExecutableArg::from_spec_value(&Value::Int(1)).unwrap_err();
        assert_eq!(
            format!("{e:#}"),
            "`executable` must be a bool, a glob pattern or a list of glob patterns, got Int(1)"
        );
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

    /// `executable = True` holds for every format, named or inferred, and is
    /// keyed only when set: `True` is exactly the bytes it hashed as when it
    /// was a gz's bool.
    #[tokio::test]
    async fn executable_holds_for_every_format_and_is_keyed_only_when_set() {
        let def = |cfg: Vec<(&'static str, Value)>| async move {
            let mut full = vec![("src", s(":a"))];
            full.extend(cfg);
            parse("//pkg:t", &full).await
        };
        let exe = ("executable", Value::Bool(true));
        let mut formats: Vec<_> = Format::ALL
            .into_iter()
            .map(|f| vec![("format", s(f.as_str()))])
            .collect();
        formats.push(vec![]);
        for format in formats {
            let plain = def(format.clone()).await.unwrap();
            let mut with = format.clone();
            with.push(exe.clone());
            let marked = def(with).await.unwrap();
            assert_ne!(plain.hash, marked.hash, "`executable` reaches the key");
            for off in [Value::Bool(false), Value::List(vec![]), Value::Null()] {
                let mut cfg = format.clone();
                cfg.push(("executable", off.clone()));
                assert_eq!(
                    def(cfg).await.unwrap().hash,
                    plain.hash,
                    "`executable = {off:?}` is the default and keeps the key"
                );
            }
            let raw = marked.def_de::<ExtractDef>();
            assert_eq!(raw.executable, ExecutableArg::Bool(true));
        }
    }

    /// A def serialized before `include` and executable patterns existed
    /// still deserializes: no `executable` key, and `executable` as a bool.
    #[test]
    fn an_older_def_still_deserializes() {
        let old = |exe: Option<bool>| {
            let mut v = serde_json::json!({
                "format": "tar",
                "strip_components": 0,
                "limits": Limits::default(),
                "out": "o",
            });
            if let Some(b) = exe {
                v["executable"] = serde_json::Value::Bool(b);
            }
            serde_json::from_value::<ExtractDef>(v).unwrap()
        };
        let raw = old(None);
        assert_eq!(raw.executable, ExecutableArg::Bool(false));
        assert!(raw.include.is_empty());
        assert_eq!(old(Some(true)).executable, ExecutableArg::Bool(true));
        assert_eq!(old(Some(false)).executable, ExecutableArg::Bool(false));
    }

    /// A def round-trips through JSON, as it crosses the ABI.
    #[tokio::test]
    async fn include_and_executable_patterns_reach_the_def() {
        let def = parse(
            "//pkg:t",
            &[
                ("src", s(":a")),
                ("include", Value::List(vec![s("b/*"), s("a"), s("b/*")])),
                ("executable", s("bin/tool")),
            ],
        )
        .await
        .unwrap();
        let json = serde_json::to_value(def.def_de::<ExtractDef>()).unwrap();
        let raw: ExtractDef = serde_json::from_value(json).unwrap();
        assert_eq!(raw.include, ["a", "b/*"], "sorted and deduped");
        assert_eq!(
            raw.executable,
            ExecutableArg::Patterns(vec!["bin/tool".to_string()])
        );
        assert!(matches!(
            raw.executable.to_unpack().unwrap(),
            harchive::Executable::Matching(_)
        ));
    }

    /// `include` and `executable` take a string, a list, and (`executable`)
    /// a bool.
    #[tokio::test]
    async fn include_and_executable_take_a_string_a_list_and_a_bool() {
        for (k, v) in [
            ("include", s("tool")),
            ("include", Value::List(vec![s("tool"), s("**/*.so")])),
            ("include", Value::List(vec![])),
            ("executable", s("bin/*")),
            ("executable", Value::List(vec![s("a"), s("b")])),
            ("executable", Value::Bool(true)),
            ("executable", Value::Bool(false)),
        ] {
            parse("//pkg:t", &[("src", s(":a")), (k, v.clone())])
                .await
                .unwrap_or_else(|e| panic!("{k} = {v:?}: {e:#}"));
        }
    }

    #[tokio::test]
    async fn bad_patterns_are_refused_at_parse() {
        for key in ["include", "executable"] {
            for (v, want) in [
                (s("a[b"), "invalid glob pattern"),
                (s(""), "empty pattern"),
                (Value::List(vec![s("ok"), s("")]), "empty pattern"),
                (s("a\0b"), "NUL"),
                (
                    s("/bin/tool"),
                    r#"relative to the output; write "bin/tool""#,
                ),
                (s("./bin/tool"), r#"no leading `./`; write "bin/tool""#),
                (
                    s("bin/"),
                    r#""bin/" matches no file; to take a directory's files write "bin/**""#,
                ),
                (s("bin/./tool"), "no `.` component"),
                (s("bin/../tool"), "no `..` component"),
                (s("bin//tool"), r#"empty component; write "bin/tool""#),
            ] {
                let Err(e) = parse("//pkg:t", &[("src", s(":a")), (key, v.clone())]).await else {
                    panic!("{key} = {v:?} must be refused")
                };
                let msg = format!("{e:#}");
                assert!(msg.contains(want), "{key} = {v:?}: {msg}");
            }
        }
        let Err(e) = parse(
            "//pkg:t",
            &[("src", s(":a")), ("executable", Value::Int(1))],
        )
        .await
        else {
            panic!("`executable = 1` must be refused")
        };
        assert!(format!("{e:#}").contains("executable"), "{e:#}");
    }

    /// The hash moves with `include` and with an executable pattern list,
    /// ignores their order and duplicates, and a list never collides with
    /// `True` nor runs into the option after it.
    #[tokio::test]
    async fn patterns_reach_the_hash_order_free() {
        let hash = |extra: Vec<(&'static str, Value)>| async move {
            let mut cfg = vec![("src", s(":a")), ("format", s("zip"))];
            cfg.extend(extra);
            parse("//pkg:t", &cfg).await.unwrap().hash
        };
        let list = |ps: &[&str]| Value::List(ps.iter().map(|p| s(p)).collect());
        let plain = hash(vec![]).await;
        let inc = hash(vec![("include", list(&["a", "b"]))]).await;
        assert_ne!(inc, plain, "`include` reaches the key");
        assert_ne!(inc, hash(vec![("include", list(&["a"]))]).await);
        assert_eq!(
            inc,
            hash(vec![("include", list(&["b", "a", "b"]))]).await,
            "order and duplicates change nothing"
        );
        assert_eq!(
            hash(vec![("include", s("a"))]).await,
            hash(vec![("include", list(&["a"]))]).await,
            "a string is a one-pattern list"
        );

        let exe = hash(vec![("executable", list(&["a", "b"]))]).await;
        let all = hash(vec![("executable", Value::Bool(true))]).await;
        assert_ne!(exe, plain, "an executable list reaches the key");
        assert_ne!(exe, all, "a list is not `True`");
        assert_ne!(exe, inc, "an executable list is not an include list");
        assert_eq!(
            exe,
            hash(vec![("executable", list(&["b", "a", "a"]))]).await,
            "order and duplicates change nothing"
        );
        // A pattern may spell the next marker; the list's length keeps it in.
        assert_ne!(
            hash(vec![("executable", list(&["a", "include", "b"]))]).await,
            hash(vec![
                ("executable", list(&["a"])),
                ("include", list(&["b"]))
            ])
            .await
        );
        assert_ne!(
            hash(vec![
                ("executable", Value::Bool(true)),
                ("include", list(&["a"]))
            ])
            .await,
            hash(vec![("include", list(&["a"]))]).await
        );
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
