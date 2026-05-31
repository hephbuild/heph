use crate::engine::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse,
    outputartifact::{Content, ContentFile, OutputArtifact, Type},
    targetdef::{
        CacheConfig, Output, TargetDef,
        path::{CodegenMode, Content as PathContent, Path},
    },
};
use crate::engine::provider::{
    ConfigRequest as ProviderConfigRequest, ConfigResponse as ProviderConfigResponse, GetError,
    GetRequest, GetResponse, ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse,
    ProbeRequest, ProbeResponse, Provider as EProvider, TargetSpec,
};
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::TargetSpecValue;
use anyhow::Context;
use async_trait::async_trait;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use xxhash_rust::xxh3::Xxh3;

const PKG: &str = "@heph/fs";
pub const DRIVER_NAME: &str = "fs";

/// Returns the `Addr` for a single-file fs target.
/// Consumers use this to reference a file without knowing the internal address format.
pub fn file_addr(path: &str) -> Addr {
    Addr::new(
        PkgBuf::from(PKG),
        "file".to_string(),
        BTreeMap::from([("f".to_string(), path.to_string())]),
    )
}

/// Returns the `Addr` for a glob fs target.
/// `exclude` is a list of glob patterns to skip; pass `&[]` for none.
pub fn glob_addr(pattern: &str, exclude: &[&str]) -> Addr {
    let mut args = BTreeMap::from([("p".to_string(), pattern.to_string())]);
    if !exclude.is_empty() {
        args.insert("e".to_string(), exclude.join(","));
    }
    Addr::new(PkgBuf::from(PKG), "glob".to_string(), args)
}

/// Returns `true` if `addr` refers to a single-file fs target.
pub fn is_file_addr(addr: &Addr) -> bool {
    addr.package.as_str() == PKG && addr.name == "file"
}

/// Returns `true` if `addr` refers to a glob fs target.
pub fn is_glob_addr(addr: &Addr) -> bool {
    addr.package.as_str() == PKG && addr.name == "glob"
}

// ─── Provider ────────────────────────────────────────────────────────────────

pub struct Provider;

impl EProvider for Provider {
    fn config(&self, _req: ProviderConfigRequest) -> anyhow::Result<ProviderConfigResponse> {
        Ok(ProviderConfigResponse {
            name: "fs".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            let items: Vec<anyhow::Result<ListPackageResponse>> = vec![Ok(ListPackageResponse {
                pkg: PkgBuf::from(PKG),
            })];
            Ok(Box::new(items.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            if req.addr.package != PKG {
                return Err(GetError::NotFound);
            }

            match req.addr.name.as_str() {
                "file" | "glob" => {}
                _ => return Err(GetError::NotFound),
            }

            // Forward addr args as config values for the driver.
            let config = req
                .addr
                .args
                .iter()
                .map(|(k, v)| (k.clone(), TargetSpecValue::String(v.clone())))
                .collect();

            Ok(GetResponse {
                target_spec: TargetSpec {
                    addr: req.addr,
                    driver: DRIVER_NAME.to_string(),
                    config,
                    labels: vec![],
                    transitive: Default::default(),
                },
            })
        })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
    }
}

// ─── Driver ──────────────────────────────────────────────────────────────────

const BUILT_IN_GLOB_EXCLUDES: &[&str] = &["**/.git/**", "**/.heph3/**", "**/.heph/**"];

/// Directory basenames whose subtree is always excluded — pruned during the
/// walk so we never descend into them. Kept in sync with the recursive
/// `**/<name>/**` entries in `BUILT_IN_GLOB_EXCLUDES`: matching the basename at
/// any depth is equivalent to those patterns, and every descendant is excluded,
/// so pruning is a pure optimization with identical results.
const BUILT_IN_PRUNE_DIRS: &[&str] = &[".git", ".heph3", ".heph"];

/// Returns the leading literal path prefix of `pattern` — the run of complete
/// components that contain no glob metacharacters.
///
/// Used as the walk root so a pattern like `mgmt/protos/x/**/*` scans only
/// `<root>/mgmt/protos/x` instead of the whole workspace. A fully-literal
/// pattern returns itself, letting `walkdir` visit just that one path.
///
/// `pattern` must already be normalized (no empty, `.`, or `..` components).
fn literal_prefix(pattern: &str) -> &str {
    // wax glob metacharacters (plus `\` escape and `,` which is only meaningful
    // inside `{…}` but signals a non-literal component).
    const META: &[char] = &['*', '?', '[', ']', '{', '}', '<', '>', '\\', ','];
    let mut end = 0usize;
    let mut pos = 0usize;
    for comp in pattern.split('/') {
        if comp.contains(META) {
            break;
        }
        pos += comp.len();
        end = pos; // include this component in the prefix
        pos += 1; // skip the '/' separator
    }
    &pattern[..end]
}

/// Collapses `.`, empty, and `..` components in a forward-slash path.
///
/// `wax` accepts patterns like `a/./b` or `a/b/../c` at parse time but its
/// walker matches them against on-disk paths that never contain literal `.` or
/// `..` segments, so the pattern matches nothing. Normalizing before compile +
/// hash keeps semantically equivalent patterns behaving identically and stable
/// in the cache.
///
/// Fails if `..` would pop past the root — the fs root is the heph workspace
/// root and targets must not escape it.
fn normalize_path(path: &str) -> anyhow::Result<String> {
    let mut out: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    anyhow::bail!("path '{}' escapes workspace root", path);
                }
            }
            other => out.push(other),
        }
    }
    Ok(out.join("/"))
}

struct CompiledGlob {
    glob: Arc<wax::Glob<'static>>,
    not: Arc<wax::Any<'static>>,
    /// Literal directory prefix of the pattern, used as the walk root.
    prefix: String,
}

/// Process-global cache of compiled globs keyed by pattern string.
///
/// Identical patterns (`**/*.go`, etc.) repeat across hundreds of packages in
/// the go-test pipeline; each `wax::Glob::new` builds a regex NFA. Patterns are
/// already normalized before reaching here, so the key set is small and stable.
/// Read-mostly: the lock is contended only on first sight of each pattern.
fn glob_cache() -> &'static RwLock<FxHashMap<String, Arc<wax::Glob<'static>>>> {
    static CACHE: OnceLock<RwLock<FxHashMap<String, Arc<wax::Glob<'static>>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// Returns the compiled `Glob` for `pattern`, memoizing across calls.
fn cached_glob(pattern: &str) -> Result<Arc<wax::Glob<'static>>, wax::BuildError> {
    if let Some(g) = glob_cache().read().get(pattern) {
        return Ok(g.clone());
    }
    let glob = Arc::new(wax::Glob::new(pattern).map(wax::Glob::into_owned)?);
    glob_cache()
        .write()
        .entry(pattern.to_owned())
        .or_insert_with(|| glob.clone());
    Ok(glob)
}

/// Returns the compiled built-in exclude `Any`, built once per process.
///
/// The common case is a glob target with no user excludes, so reusing this Arc
/// avoids rebuilding the 3-pattern regex union (`**/.git/**`, …) on every parse.
fn builtin_excludes() -> anyhow::Result<Arc<wax::Any<'static>>> {
    static BUILTIN: OnceLock<Arc<wax::Any<'static>>> = OnceLock::new();
    if let Some(a) = BUILTIN.get() {
        return Ok(a.clone());
    }
    let globs = BUILT_IN_GLOB_EXCLUDES
        .iter()
        .map(|s| {
            cached_glob(s)
                .map(|g| (*g).clone())
                .with_context(|| format!("invalid built-in exclude pattern '{s}'"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let any = Arc::new(wax::any(globs).context("compiling built-in exclude patterns")?);
    // Lost a race? Keep whichever value won; both are equivalent.
    Ok(BUILTIN.get_or_init(|| any).clone())
}

#[derive(serde::Serialize)]
enum FsDef {
    File {
        path: String,
    },
    Glob {
        pattern: String,
        exclude: Vec<String>,
        #[serde(skip)]
        compiled: Arc<CompiledGlob>,
    },
}

fn compile_glob(pattern: &str, exclude: &[String]) -> anyhow::Result<CompiledGlob> {
    let glob = cached_glob(pattern).with_context(|| format!("invalid glob pattern '{pattern}'"))?;

    // No user excludes: reuse the cached built-in `Any` directly.
    let not = if exclude.is_empty() {
        builtin_excludes()?
    } else {
        let exclude_globs: Vec<wax::Glob<'static>> = BUILT_IN_GLOB_EXCLUDES
            .iter()
            .copied()
            .chain(exclude.iter().map(String::as_str))
            .map(|s| {
                cached_glob(s)
                    .map(|g| (*g).clone())
                    .with_context(|| format!("invalid exclude pattern '{s}'"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Arc::new(wax::any(exclude_globs).context("compiling exclude patterns")?)
    };

    Ok(CompiledGlob {
        glob,
        not,
        prefix: literal_prefix(pattern).to_owned(),
    })
}

pub struct Driver;

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_meta: &std::fs::Metadata) -> bool {
    false
}

fn file_hashout(meta: &std::fs::Metadata) -> String {
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut h = Xxh3::new();
    h.update(&size.to_le_bytes());
    h.update(&mtime.to_le_bytes());
    format!("{:x}", h.digest())
}

#[async_trait]
impl crate::engine::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let get_str = |key: &str| -> anyhow::Result<Option<String>> {
            match req.target_spec.config.get(key) {
                None => Ok(None),
                Some(TargetSpecValue::String(s)) => Ok(Some(s.clone())),
                Some(v) => anyhow::bail!("fs driver: '{}' must be a string, got {:?}", key, v),
            }
        };

        let file_path = get_str("f")?
            .map(|p| normalize_path(&p))
            .transpose()
            .context("normalizing fs file path")?;
        let glob_pattern = get_str("p")?
            .map(|p| normalize_path(&p))
            .transpose()
            .context("normalizing fs glob pattern")?;
        let exclude_raw = get_str("e")?.unwrap_or_default();

        // "e" arg is comma-separated glob patterns.
        let exclude: Vec<String> = if exclude_raw.is_empty() {
            vec![]
        } else {
            exclude_raw.split(',').map(str::to_owned).collect()
        };

        let (def, outputs, hash) = match (file_path, glob_pattern) {
            (Some(path), _) => {
                let mut h = Xxh3::new();
                h.update(b"fs_file");
                h.update(path.as_bytes());
                let hash = format!("{:x}", h.digest()).into_bytes();

                let def = FsDef::File { path: path.clone() };
                let outputs = vec![Output {
                    group: "".to_string(),
                    paths: vec![Path {
                        content: PathContent::FilePath(path),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                }];
                (def, outputs, hash)
            }
            (None, Some(pattern)) => {
                let mut h = Xxh3::new();
                h.update(b"fs_glob");
                h.update(pattern.as_bytes());
                for e in &exclude {
                    h.update(e.as_bytes());
                }
                let hash = format!("{:x}", h.digest()).into_bytes();

                let compiled = Arc::new(compile_glob(&pattern, &exclude)?);
                let def = FsDef::Glob {
                    pattern: pattern.clone(),
                    exclude,
                    compiled,
                };
                let outputs = vec![Output {
                    group: "".to_string(),
                    paths: vec![Path {
                        content: PathContent::Glob(pattern),
                        codegen_tree: CodegenMode::None,
                        collect: false,
                    }],
                }];
                (def, outputs, hash)
            }
            (None, None) => {
                anyhow::bail!("fs driver requires 'f' (file path) or 'p' (glob pattern) in config")
            }
        };

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                inputs: vec![],
                outputs,
                support_files: vec![],
                cache: CacheConfig::off(),
                pty: false,
                hash,
                transparent: false,
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
        req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        let def = req.target.def::<FsDef>();
        let root = &req.tree_root_path;

        match def {
            FsDef::File { path } => {
                let abs = root.join(path);
                let meta = std::fs::metadata(&abs)
                    .with_context(|| format!("stat file '{}'", abs.display()))?;
                let x = is_exec(&meta);
                let hashout = file_hashout(&meta);

                let source_path = abs
                    .to_str()
                    .ok_or_else(|| {
                        anyhow::anyhow!("file path is not valid UTF-8: {}", abs.display())
                    })?
                    .to_string();

                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path.as_str())
                    .to_string();

                Ok(RunResponse {
                    artifacts: vec![OutputArtifact {
                        group: "".to_string(),
                        name,
                        r#type: Type::Output,
                        content: Content::File(ContentFile {
                            source_path,
                            out_path: path.clone(),
                            x,
                        }),
                        hashout,
                    }],
                    ..Default::default()
                })
            }

            FsDef::Glob { compiled, .. } => {
                // Start the walk at the pattern's literal prefix so a rooted
                // pattern (`a/b/**/*`) scans only `<root>/a/b`, not the whole
                // tree. Matching uses the cached glob/exclude NFAs directly —
                // no per-run regex (WalkProgram / exclude `Any`) compilation.
                let walk_root = if compiled.prefix.is_empty() {
                    root.clone()
                } else {
                    root.join(&compiled.prefix)
                };

                let mut artifacts = vec![];

                let walker = walkdir::WalkDir::new(&walk_root)
                    .into_iter()
                    .filter_entry(|entry| {
                        // Never descend into always-excluded subtrees (.git, …).
                        !(entry.file_type().is_dir()
                            && entry
                                .file_name()
                                .to_str()
                                .is_some_and(|n| BUILT_IN_PRUNE_DIRS.contains(&n)))
                    });

                for entry in walker {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => {
                            // A missing walk root (or a path that vanished
                            // mid-walk) is an empty match, not an error.
                            if e.io_error()
                                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                            {
                                continue;
                            }
                            return Err(anyhow::Error::from(e))
                                .context("walking glob entries");
                        }
                    };

                    if entry.file_type().is_dir() {
                        continue;
                    }

                    let abs_path = entry.path();
                    let rel = abs_path
                        .strip_prefix(root)
                        .with_context(|| "strip root prefix from glob entry")?;

                    // Include on a pattern match, drop on an exclude match.
                    use wax::Program as _;
                    if !compiled.glob.is_match(rel) || compiled.not.is_match(rel) {
                        continue;
                    }

                    let rel_str = rel.to_str().ok_or_else(|| {
                        anyhow::anyhow!("glob entry path is not valid UTF-8: {}", rel.display())
                    })?;

                    let meta = entry
                        .metadata()
                        .with_context(|| format!("stat glob entry '{}'", abs_path.display()))?;

                    let x = is_exec(&meta);
                    let hashout = file_hashout(&meta);

                    let source_path = abs_path
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("glob entry path is not valid UTF-8"))?
                        .to_string();

                    let name = rel_str.replace('/', "_");

                    artifacts.push(OutputArtifact {
                        group: "".to_string(),
                        name,
                        r#type: Type::Output,
                        content: Content::File(ContentFile {
                            source_path,
                            out_path: rel_str.to_string(),
                            x,
                        }),
                        hashout,
                    });
                }

                Ok(RunResponse {
                    artifacts,
                    ..Default::default()
                })
            }
        }
    }

    async fn run_shell<'a, 'io>(
        &self,
        _req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        anyhow::bail!("run_shell not implemented for pluginfs")
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::driver::{Driver as EDriver, RunRequest};
    use crate::engine::provider::Provider as EProvider;
    use crate::hasync::StdCancellationToken;
    use crate::htaddr::parse_addr;
    use std::fs;
    use tempfile::tempdir;

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    struct NoopExecutor;
    impl crate::engine::provider::ProviderExecutor for NoopExecutor {
        fn result<'a>(
            &'a self,
            _addr: &'a crate::htaddr::Addr,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<std::sync::Arc<crate::engine::EResult>>>
        {
            Box::pin(async { anyhow::bail!("noop") })
        }

        fn query<'a>(
            &'a self,
            _m: &'a crate::htmatcher::Matcher,
            _extra_skip: &'a [String],
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<crate::htaddr::Addr>>> {
            Box::pin(async { anyhow::bail!("noop") })
        }
    }

    fn make_get_req(addr_str: &str) -> GetRequest {
        GetRequest {
            request_id: "test".to_string(),
            addr: parse_addr(addr_str).unwrap(),
            states: vec![],
            executor: Arc::new(NoopExecutor),
        }
    }

    // ─── Provider tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_provider_wrong_package_not_found() {
        let p = Provider;
        let result = p
            .get(make_get_req("//other/pkg:file@f=foo.txt"), &ctoken())
            .await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_provider_wrong_name_not_found() {
        let p = Provider;
        let result = p
            .get(
                make_get_req(&format!("//{PKG}:unknown@f=foo.txt")),
                &ctoken(),
            )
            .await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_provider_file_returns_spec() {
        let p = Provider;
        let result = p
            .get(make_get_req(&format!("//{PKG}:file@f=foo.txt")), &ctoken())
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
        assert_eq!(result.target_spec.addr.name, "file");
        assert_eq!(
            result.target_spec.config.get("f"),
            Some(&TargetSpecValue::String("foo.txt".to_string()))
        );
    }

    #[tokio::test]
    async fn test_provider_glob_returns_spec() {
        let p = Provider;
        let result = p
            .get(make_get_req(&format!("//{PKG}:glob@p=src/*.rs")), &ctoken())
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
        assert_eq!(result.target_spec.addr.name, "glob");
        assert_eq!(
            result.target_spec.config.get("p"),
            Some(&TargetSpecValue::String("src/*.rs".to_string()))
        );
    }

    // ─── Driver tests ──────────────────────────────────────────────────────

    fn make_parse_req(config: std::collections::HashMap<String, TargetSpecValue>) -> ParseRequest {
        ParseRequest {
            request_id: "test".to_string(),
            target_spec: std::sync::Arc::new(TargetSpec {
                addr: parse_addr(&format!("//{PKG}:file")).unwrap(),
                driver: DRIVER_NAME.to_string(),
                config,
                labels: vec![],
                transitive: Default::default(),
            }),
        }
    }

    fn make_run_req<'a>(
        target: &'a TargetDef,
        request_id: &'a String,
        root: std::path::PathBuf,
        hashin: &'a String,
    ) -> RunRequest<'a, 'static> {
        RunRequest {
            request_id,
            target,
            tree_root_path: root.clone(),
            inputs: vec![],
            hashin,
            stdin: None,
            stdout: None,
            stderr: None,
            sandbox_dir: root,
        }
    }

    #[tokio::test]
    async fn test_driver_parse_missing_both_errors() {
        let driver = Driver;
        let result = driver
            .parse(make_parse_req(Default::default()), &ctoken())
            .await;
        let Err(err) = result else {
            panic!("expected error")
        };
        let msg = err.to_string();
        assert!(
            msg.contains("requires 'f' (file path) or 'p'"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_driver_parse_invalid_glob_fails() {
        let driver = Driver;
        // Unmatched alternation — wax rejects at parse time.
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("ogen.{yml,yaml".to_string()),
        )]);
        let result = driver.parse(make_parse_req(config), &ctoken()).await;
        let Err(err) = result else {
            panic!("expected parse error")
        };
        let msg = err.to_string();
        assert!(
            msg.contains("invalid glob pattern"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_driver_parse_invalid_exclude_fails() {
        let driver = Driver;
        let config = std::collections::HashMap::from([
            ("p".to_string(), TargetSpecValue::String("*.rs".to_string())),
            (
                "e".to_string(),
                TargetSpecValue::String("good/**,bad.{yml".to_string()),
            ),
        ]);
        let result = driver.parse(make_parse_req(config), &ctoken()).await;
        let Err(err) = result else {
            panic!("expected parse error")
        };
        let msg = err.to_string();
        assert!(
            msg.contains("invalid exclude pattern"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_driver_parse_file_config() {
        let driver = Driver;
        let config = std::collections::HashMap::from([(
            "f".to_string(),
            TargetSpecValue::String("src/main.rs".to_string()),
        )]);
        let res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();
        let def = res.target_def.def::<FsDef>();
        assert!(matches!(def, FsDef::File { path } if path == "src/main.rs"));
        assert!(!res.target_def.cache.enabled);
        assert!(!res.target_def.cache.remote_enabled);
    }

    #[tokio::test]
    async fn test_driver_parse_glob_config() {
        let driver = Driver;
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("src/*.rs".to_string()),
        )]);
        let res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();
        let def = res.target_def.def::<FsDef>();
        assert!(matches!(def, FsDef::Glob { pattern, .. } if pattern == "src/*.rs"));
    }

    #[tokio::test]
    async fn test_driver_parse_glob_with_exclude() {
        let driver = Driver;
        let config = std::collections::HashMap::from([
            (
                "p".to_string(),
                TargetSpecValue::String("**/*.rs".to_string()),
            ),
            (
                "e".to_string(),
                TargetSpecValue::String("vendor/**,generated/**".to_string()),
            ),
        ]);
        let res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();
        let def = res.target_def.def::<FsDef>();
        match def {
            FsDef::Glob { exclude, .. } => {
                assert_eq!(exclude, &["vendor/**", "generated/**"]);
            }
            FsDef::File { .. } => panic!("expected glob"),
        }
    }

    #[tokio::test]
    async fn test_driver_run_file_exists() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let file_path = tmp.path().join("hello.txt");
        fs::write(&file_path, b"hello world").unwrap();

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            TargetSpecValue::String("hello.txt".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        assert_eq!(res.artifacts.len(), 1);
        let artifact = &res.artifacts[0];
        assert_eq!(artifact.name, "hello.txt");
        assert_eq!(artifact.r#type, Type::Output);
        match &artifact.content {
            Content::File(f) => {
                assert_eq!(f.out_path, "hello.txt");
                assert!(f.source_path.ends_with("hello.txt"));
            }
            _ => panic!("expected File content"),
        }
    }

    #[tokio::test]
    async fn test_driver_run_file_missing_errors() {
        let driver = Driver;
        let tmp = tempdir().unwrap();

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            TargetSpecValue::String("nonexistent.txt".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        assert!(driver.run(req, &ctoken()).await.is_err());
    }

    #[tokio::test]
    async fn test_driver_run_glob_matches_files() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), b"").unwrap();
        fs::write(tmp.path().join("b.rs"), b"").unwrap();
        fs::write(tmp.path().join("c.txt"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("*.rs".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        assert_eq!(res.artifacts.len(), 2);
        let names: Vec<_> = res.artifacts.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"a.rs"));
        assert!(names.contains(&"b.rs"));
    }

    #[tokio::test]
    async fn test_driver_run_glob_excludes_git() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("config"), b"").unwrap();
        fs::write(tmp.path().join("main.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("**/*".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        let names: Vec<_> = res.artifacts.iter().map(|a| &a.name).collect();
        assert!(
            names.iter().all(|n| !n.contains(".git")),
            "should exclude .git: {:?}",
            names
        );
        assert_eq!(res.artifacts.len(), 1);
        assert_eq!(res.artifacts[0].name, "main.rs");
    }

    #[tokio::test]
    async fn test_driver_run_glob_user_exclude() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("keep.rs"), b"").unwrap();
        fs::write(tmp.path().join("skip.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([
            ("p".to_string(), TargetSpecValue::String("*.rs".to_string())),
            (
                "e".to_string(),
                TargetSpecValue::String("skip.rs".to_string()),
            ),
        ]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        assert_eq!(res.artifacts.len(), 1);
        assert_eq!(res.artifacts[0].name, "keep.rs");
    }

    #[tokio::test]
    async fn test_driver_run_glob_out_path_relative() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("lib.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("**/*.rs".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        assert_eq!(res.artifacts.len(), 1);
        let artifact = &res.artifacts[0];
        assert_eq!(artifact.name, "sub_lib.rs");
        match &artifact.content {
            Content::File(f) => {
                assert_eq!(f.out_path, "sub/lib.rs");
            }
            _ => panic!("expected File content"),
        }
    }

    // ─── Addr utility tests ────────────────────────────────────────────────

    #[test]
    fn test_file_addr_package_and_name() {
        let addr = file_addr("src/main.rs");
        assert_eq!(addr.package.as_str(), PKG);
        assert_eq!(addr.name, "file");
        assert_eq!(addr.args.get("f").map(String::as_str), Some("src/main.rs"));
    }

    #[test]
    fn test_file_addr_roundtrips_through_format_and_parse() {
        let addr = file_addr("some/path/foo.txt");
        let formatted = addr.format();
        let parsed = crate::htaddr::parse_addr(&formatted).unwrap();
        assert_eq!(parsed.package, addr.package);
        assert_eq!(parsed.name, addr.name);
        assert_eq!(parsed.args.get("f"), addr.args.get("f"));
    }

    #[test]
    fn test_glob_addr_no_excludes() {
        let addr = glob_addr("src/**/*.rs", &[]);
        assert_eq!(addr.package.as_str(), PKG);
        assert_eq!(addr.name, "glob");
        assert_eq!(addr.args.get("p").map(String::as_str), Some("src/**/*.rs"));
        assert!(!addr.args.contains_key("e"));
    }

    #[test]
    fn test_glob_addr_with_excludes() {
        let addr = glob_addr("**/*.go", &["vendor/**", "gen/**"]);
        assert_eq!(
            addr.args.get("e").map(String::as_str),
            Some("vendor/**,gen/**")
        );
    }

    #[test]
    fn test_glob_addr_roundtrips_through_format_and_parse() {
        let addr = glob_addr("src/*.rs", &["skip.rs"]);
        let formatted = addr.format();
        let parsed = crate::htaddr::parse_addr(&formatted).unwrap();
        assert_eq!(parsed.name, "glob");
        assert_eq!(parsed.args.get("p"), addr.args.get("p"));
        assert_eq!(parsed.args.get("e"), addr.args.get("e"));
    }

    #[tokio::test]
    async fn test_provider_accepts_file_addr() {
        let p = Provider;
        let addr = file_addr("README.md");
        let result = p
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
    }

    #[test]
    fn test_glob_addr_brace_pattern_roundtrips() {
        let pattern = "dir/ogen.{yml,yaml}";
        let input = format!("//{PKG}:glob@p=\"{pattern}\"");

        let parsed = parse_addr(&input).unwrap();
        assert_eq!(parsed.package.as_str(), PKG);
        assert_eq!(parsed.name, "glob");
        assert_eq!(parsed.args.get("p").map(String::as_str), Some(pattern));

        // Pattern contains ',' → format must quote.
        let formatted = parsed.format();
        assert_eq!(formatted, input);

        // Roundtrip again to confirm format output is itself parseable.
        let reparsed = parse_addr(&formatted).unwrap();
        assert_eq!(reparsed, parsed);
    }

    #[tokio::test]
    async fn test_driver_run_glob_dot_segment_in_middle_matches() {
        // Pattern with `/./` in the middle previously compiled in wax but
        // matched nothing because wax walks on-disk paths that never contain
        // literal `.` segments. Driver normalizes before compile.
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let nested = tmp.path().join("mgmt/protos/x/bsdiff/v1");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("proto.proto"), b"").unwrap();
        fs::write(tmp.path().join("mgmt/protos/x/bsdiff/BUILD"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("mgmt/protos/x/bsdiff/./**/*".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        let mut out_paths: Vec<String> = res
            .artifacts
            .iter()
            .map(|a| match &a.content {
                Content::File(f) => f.out_path.clone(),
                _ => panic!("expected File"),
            })
            .collect();
        out_paths.sort();
        assert_eq!(
            out_paths,
            vec![
                "mgmt/protos/x/bsdiff/BUILD".to_string(),
                "mgmt/protos/x/bsdiff/v1/proto.proto".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn test_driver_run_glob_parent_segment_in_middle_matches() {
        // `a/b/../c` is equivalent to `a/c` on disk; wax does not collapse
        // `..` so driver normalizes before compile.
        let driver = Driver;
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("mgmt/protos/x")).unwrap();
        fs::write(tmp.path().join("mgmt/protos/x/go.mod"), b"").unwrap();
        fs::write(tmp.path().join("mgmt/protos/x/other.txt"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("mgmt/protos/x/bsdiff/../go.mod".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        let names: Vec<_> = res.artifacts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["mgmt_protos_x_go.mod"]);
    }

    #[tokio::test]
    async fn test_driver_parse_glob_rejects_root_escape() {
        let driver = Driver;
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("some/../../file".to_string()),
        )]);
        let result = driver.parse(make_parse_req(config), &ctoken()).await;
        let Err(err) = result else {
            panic!("expected escape error")
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes workspace root"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_driver_parse_glob_rejects_leading_parent() {
        let driver = Driver;
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("../escape/**".to_string()),
        )]);
        let result = driver.parse(make_parse_req(config), &ctoken()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_driver_parse_file_rejects_root_escape() {
        let driver = Driver;
        let config = std::collections::HashMap::from([(
            "f".to_string(),
            TargetSpecValue::String("a/../../file.txt".to_string()),
        )]);
        let result = driver.parse(make_parse_req(config), &ctoken()).await;
        let Err(err) = result else {
            panic!("expected escape error")
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes workspace root"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_driver_run_glob_parent_segments_hash_equivalence() {
        let driver = Driver;
        let with_parent = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("a/b/../c/**/*.rs".to_string()),
        )]);
        let collapsed = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("a/c/**/*.rs".to_string()),
        )]);
        let h1 = driver
            .parse(make_parse_req(with_parent), &ctoken())
            .await
            .unwrap()
            .target_def
            .hash;
        let h2 = driver
            .parse(make_parse_req(collapsed), &ctoken())
            .await
            .unwrap()
            .target_def
            .hash;
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn test_driver_run_glob_dot_segments_hash_equivalence() {
        // Two semantically equal patterns must produce the same hash, so the
        // cache key is stable regardless of how the user wrote the path.
        let driver = Driver;
        let with_dot = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("a/./b/**/*.rs".to_string()),
        )]);
        let without_dot = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("a/b/**/*.rs".to_string()),
        )]);
        let h1 = driver
            .parse(make_parse_req(with_dot), &ctoken())
            .await
            .unwrap()
            .target_def
            .hash;
        let h2 = driver
            .parse(make_parse_req(without_dot), &ctoken())
            .await
            .unwrap()
            .target_def
            .hash;
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn test_driver_run_glob_missing_path_is_empty() {
        let driver = Driver;
        let tmp = tempdir().unwrap();

        let pattern = "does/not/exist/*.rs";
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String(pattern.to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();
        assert!(res.artifacts.is_empty());
    }

    #[tokio::test]
    async fn test_driver_run_glob_missing_root_is_empty() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let missing_root = tmp.path().join("nonexistent");

        let pattern = "*.rs";
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String(pattern.to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(&parse_res.target_def, &request_id, missing_root, &hashin);
        let res = driver.run(req, &ctoken()).await.unwrap();
        assert!(res.artifacts.is_empty());
    }

    #[tokio::test]
    async fn test_driver_run_glob_brace_pattern() {
        let driver = Driver;
        let tmp = tempdir().unwrap();
        let sub = tmp.path().join("dir");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("ogen.yml"), b"").unwrap();
        fs::write(sub.join("ogen.yaml"), b"").unwrap();
        fs::write(sub.join("other.txt"), b"").unwrap();

        let pattern = "dir/ogen.{yml,yaml}";
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String(pattern.to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        let mut out_paths: Vec<String> = res
            .artifacts
            .iter()
            .map(|a| match &a.content {
                Content::File(f) => f.out_path.clone(),
                _ => panic!("expected File"),
            })
            .collect();
        out_paths.sort();
        assert_eq!(
            out_paths,
            vec!["dir/ogen.yaml".to_string(), "dir/ogen.yml".to_string(),]
        );
    }

    #[test]
    fn test_cached_glob_returns_same_arc() {
        let a = cached_glob("test_cache/**/*.unique-ext").unwrap();
        let b = cached_glob("test_cache/**/*.unique-ext").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "identical patterns must share one Arc");
    }

    #[test]
    fn test_builtin_excludes_shared() {
        let a = builtin_excludes().unwrap();
        let b = builtin_excludes().unwrap();
        assert!(Arc::ptr_eq(&a, &b), "built-in Any built once per process");
    }

    #[test]
    fn test_compile_glob_no_exclude_reuses_builtin() {
        let builtin = builtin_excludes().unwrap();
        let compiled = compile_glob("**/*.rs", &[]).unwrap();
        assert!(
            Arc::ptr_eq(&compiled.not, &builtin),
            "empty-exclude path must reuse the cached built-in Any"
        );
    }

    #[test]
    fn test_literal_prefix() {
        assert_eq!(literal_prefix("mgmt/protos/x/**/*"), "mgmt/protos/x");
        // Fully-literal pattern returns itself (walk visits just that path).
        assert_eq!(literal_prefix("mgmt/protos/x/go.mod"), "mgmt/protos/x/go.mod");
        assert_eq!(literal_prefix("*.rs"), "");
        assert_eq!(literal_prefix("**/*.rs"), "");
        assert_eq!(literal_prefix("dir/ogen.{yml,yaml}"), "dir");
        assert_eq!(literal_prefix("a/b?c/d"), "a");
    }

    #[tokio::test]
    async fn test_driver_run_glob_rooted_prefix_ignores_siblings() {
        // A rooted pattern walks only its literal-prefix subtree; sibling
        // directories with matching-looking files must not appear.
        let driver = Driver;
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("pkg/sub")).unwrap();
        fs::write(tmp.path().join("pkg/keep.rs"), b"").unwrap();
        fs::write(tmp.path().join("pkg/sub/deep.rs"), b"").unwrap();
        fs::create_dir_all(tmp.path().join("other")).unwrap();
        fs::write(tmp.path().join("other/nope.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            TargetSpecValue::String("pkg/**/*.rs".to_string()),
        )]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );
        let res = driver.run(req, &ctoken()).await.unwrap();

        let mut names: Vec<_> = res.artifacts.iter().map(|a| a.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["pkg_keep.rs", "pkg_sub_deep.rs"]);
    }

    #[tokio::test]
    async fn test_provider_accepts_glob_addr() {
        let p = Provider;
        let addr = glob_addr("**/*.rs", &[]);
        let result = p
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
    }
}
