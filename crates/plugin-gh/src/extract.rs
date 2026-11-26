//! The `extract` driver (`<plugin>.extract`, `gh.extract` under the shipped
//! manifest): unpack the one archive file a `src` target produced into a
//! directory output, in-process. What it refuses, and why, is
//! [`harchive::unpack`]; the user-facing description is `docs/GH_RELEASES.md`.

use anyhow::Context as _;
use async_trait::async_trait;
use harchive::{Format, Limits};
use hcore::hasync::Cancellable;
use hcore::htvalue::signature::ParamType;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunInput, ManagedRunRequest, ManagedRunResponse,
};
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{Input, InputMode, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, TargetAddr,
};
use hplugin::htspec::{FromSpecValue, Spec, TargetSpecCache};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

/// The driver's local name; the engine reaches it as `<plugin>.extract`.
pub const DRIVER_NAME: &str = "extract";

/// Bump to invalidate every extracted tree when the unpacking rules change
/// what an archive produces (modes, what is refused, link handling).
const EXTRACT_FORMAT_VERSION: u32 = 1;

/// The dep input's origin id.
const SRC_ORIGIN: &str = "extract:src";

/// An archive [`Format`] option: one of its spellings. A newtype because both
/// `Format` and [`FromSpecValue`] are other crates'.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FormatArg(pub(crate) Format);

impl FromSpecValue for FormatArg {
    fn from_spec_value(v: &hcore::htvalue::Value) -> anyhow::Result<Self> {
        Format::parse(&String::from_spec_value(v)?).map(Self)
    }

    fn spec_param_type() -> ParamType {
        ParamType::one_of(Format::ALL.iter().map(|f| f.as_str()))
    }
}

/// A positive count option (`max_bytes`, `max_entries`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Count(pub(crate) u64);

impl FromSpecValue for Count {
    fn from_spec_value(v: &hcore::htvalue::Value) -> anyhow::Result<Self> {
        use hcore::htvalue::Value;
        match v {
            Value::Int(i) if *i > 0 => Ok(Self(i.unsigned_abs())),
            Value::Uint(u) if *u > 0 => Ok(Self(*u)),
            other => anyhow::bail!("must be a positive integer, got {other:?}"),
        }
    }

    fn spec_param_type() -> ParamType {
        ParamType::Int
    }
}

/// Config for an `extract` target.
#[derive(Spec)]
struct ExtractSpec {
    /// Address of the target whose single output file is the archive.
    #[spec(required)]
    src: String,
    /// Archive format: `tar`, `tar.gz`, `tar.zst` or `zip`.
    #[spec(required)]
    format: FormatArg,
    /// Leading path components dropped from every entry, as `tar
    /// --strip-components`. An entry with no more components than this is
    /// skipped; an archive from which nothing remains is an error.
    strip_components: u32,
    /// Output directory, relative to the target's package. Defaults to the
    /// target name.
    #[spec(ty = ParamType::String)]
    out: Option<String>,
    /// The most bytes the archive may unpack to (default 4 GiB). Exceeding it
    /// fails the run; raise it for an archive that is meant to be larger.
    #[spec(ty = ParamType::Int)]
    max_bytes: Option<Count>,
    /// The most entries the archive may hold (default 1,000,000).
    #[spec(ty = ParamType::Int)]
    max_entries: Option<Count>,
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
    format: Format,
    strip_components: u32,
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
        check_out(&out).with_context(|| format!("{}: `out`", addr.format()))?;

        let pkg = addr.package.clone();
        let src = TargetAddr::parse(&spec.src, &pkg)
            .with_context(|| format!("parsing extract src {:?}", spec.src))?;
        let out_ws = if pkg.as_str().is_empty() {
            out.clone()
        } else {
            format!("{}/{out}", pkg.as_str())
        };

        let defaults = Limits::default();
        let limits = Limits {
            max_bytes: spec.max_bytes.map_or(defaults.max_bytes, |c| c.0),
            max_entries: spec.max_entries.map_or(defaults.max_entries, |c| c.0),
        };

        let mut h = Xxh3::new();
        h.update(&EXTRACT_FORMAT_VERSION.to_le_bytes());
        h.update(addr.format().as_bytes());
        h.update(b"\0");
        // The archive's bytes reach the key through the dep's hashout, so
        // only the address goes in here — all of it: `|group` and `[filters]`
        // pick which of the dep's files are the archive.
        h.update(src.to_string().as_bytes());
        h.update(b"\0");
        h.update(spec.format.0.as_str().as_bytes());
        h.update(b"\0");
        h.update(&spec.strip_components.to_le_bytes());
        h.update(out.as_bytes());
        // Not `limits`: they decide only whether the run fails, and a failure
        // is never cached; when it succeeds the tree is the same whatever the
        // limits were, so raising one must not re-extract every archive.
        let hash = format!("{:016x}", h.digest()).into_bytes();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(ExtractDef {
                    format: spec.format.0,
                    strip_components: spec.strip_components,
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
        let format = def.format;
        let archive = single_archive(&req).with_context(|| format!("{addr}: `src`"))?;
        let dest = req.sandbox_pkg_dir.join(&def.out);
        let (strip, limits) = (def.strip_components, def.limits);

        // Decompressing and writing is blocking work; off the async workers.
        // The token is polled on every read, so a cancelled run stops within
        // one chunk instead of unpacking the rest.
        let token = ctoken.clone_arc();
        hcore::blocking::run(move || {
            harchive::unpack(&archive, format, strip, limits, &dest, &|| {
                token.is_cancelled()
            })
        })
        .await
        .with_context(|| format!("{addr}: extract {}", format.as_str()))?;

        Ok(ManagedRunResponse::default())
    }
}

/// `out` is a directory below the package: relative, and no `.`/`..`/empty
/// component, so it names one place on every platform and cannot reach
/// outside the package's tree.
fn check_out(out: &str) -> anyhow::Result<()> {
    if out.is_empty() {
        anyhow::bail!("must not be empty");
    }
    if out.starts_with('/') {
        anyhow::bail!("{out:?} must be relative to the package");
    }
    if out
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        anyhow::bail!("{out:?} must not contain empty, `.` or `..` components");
    }
    Ok(())
}

/// The archive: the one file the `src` dep materialized. Several files have
/// no defensible answer (which one is the archive would depend on walk
/// order), so it is refused with the count.
fn single_archive(req: &ManagedRunRequest<'_, '_>) -> anyhow::Result<PathBuf> {
    let input = req
        .inputs
        .iter()
        .find(|m| {
            m.input.origin_id == SRC_ORIGIN
                && matches!(
                    m.input.artifact.r#type,
                    hplugin::driver::inputartifact::Type::Dep
                )
        })
        .context("no archive input in the sandbox")?;
    let mut files = dep_files(input)?.into_iter();
    let first = files
        .next()
        .context("produced no file; point `src` at a target with one file output")?;
    let extra = files.count();
    if extra > 0 {
        anyhow::bail!(
            "must produce exactly one file, but it produced {} ({:?} and {extra} more); \
             point `src` at a single file target",
            extra + 1,
            first.display(),
        );
    }
    Ok(first)
}

/// The paths a dep input's `.list` file names: one absolute materialized path
/// per line, blank lines skipped.
fn dep_files(m: &ManagedRunInput) -> anyhow::Result<Vec<PathBuf>> {
    let list_path = m.require_list_path()?;
    let content = std::fs::read_to_string(list_path)
        .with_context(|| format!("read dep list {}", list_path.display()))?;
    Ok(content
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
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
        assert_eq!(raw.format, Format::Tar);
    }

    #[tokio::test]
    async fn bad_options_are_refused_at_parse() {
        for (cfg, want) in [
            (
                vec![("src", s(":a")), ("format", s("rar"))],
                "unknown archive format",
            ),
            (vec![("format", s("tar"))], "src"),
            (vec![("src", s(":a"))], "format"),
            (
                vec![("src", s(":a")), ("format", s("tar")), ("out", s("../x"))],
                "`..`",
            ),
            (
                vec![("src", s(":a")), ("format", s("tar")), ("out", s("/abs"))],
                "relative",
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
