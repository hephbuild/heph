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
    ConfigRequest as ProviderConfigRequest, ConfigResponse as ProviderConfigResponse, FnArgs,
    FnCallContext, GetError, GetRequest, GetResponse, ListPackageResponse, ListPackagesRequest,
    ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider as EProvider, ProviderFn,
    ProviderFunctionDef, TargetSpec,
};
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;
use crate::htvalue::Value;
use crate::htwalk::Ignore;
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

/// Extended-attribute name stamped on net-new codegen files written back to the
/// tree (value = the generator's addr). Raw source reads must skip these so a
/// generated file is never double-sourced.
pub const CODEGEN_XATTR: &str = "user.heph.codegen";

/// True if the file at `path` carries the codegen provenance xattr.
///
/// A filesystem that does not support xattrs (or any IO error) is treated as
/// "not stamped" so globbing/file reads never break on such trees.
fn has_codegen_xattr(path: &std::path::Path) -> bool {
    matches!(xattr::get(path, CODEGEN_XATTR), Ok(Some(_)))
}

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

#[derive(Default)]
pub struct Provider {
    /// Dirs the `glob` provider function must prune, shared with the driver.
    skip: Arc<Ignore>,
}

impl Provider {
    pub fn new(skip: Arc<Ignore>) -> Self {
        Self { skip }
    }
}

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
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
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

    fn functions(&self) -> Vec<ProviderFunctionDef> {
        vec![
            ProviderFunctionDef {
                name: "glob".to_string(),
                func: Arc::new(GlobFn {
                    skip: self.skip.clone(),
                }),
            },
            ProviderFunctionDef {
                name: "join".to_string(),
                func: Arc::new(JoinFn),
            },
            ProviderFunctionDef {
                name: "dir".to_string(),
                func: Arc::new(DirFn),
            },
            ProviderFunctionDef {
                name: "base".to_string(),
                func: Arc::new(BaseFn),
            },
        ]
    }
}

// ─── Exposed functions ─────────────────────────────────────────────────────────

/// `heph.fs.glob(pattern)` — expand `pattern` (resolved relative to the calling
/// package) against the workspace filesystem and return matched file paths relative
/// to that package. Uses the exact same `wax` walk as the `fs` driver
/// ([`compile_glob`] + [`walk_glob`]), so BUILD-time expansion matches what the
/// driver globs at execution time: brace alternation, `.`/`..` normalization, and
/// the engine's skip dirs/globs. Result is sorted.
struct GlobFn {
    skip: Arc<Ignore>,
}

#[async_trait]
impl ProviderFn for GlobFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value> {
        let pattern = match args.positional.first() {
            Some(Value::String(s)) => s.as_str(),
            Some(other) => anyhow::bail!("heph.fs.glob: pattern must be a string, got {:?}", other),
            None => anyhow::bail!("heph.fs.glob: missing pattern argument"),
        };

        let resolved = if ctx.pkg.is_empty() {
            pattern.to_string()
        } else {
            format!("{}/{}", ctx.pkg, pattern)
        };
        let resolved = normalize_path(&resolved).context("heph.fs.glob: normalizing pattern")?;

        // No user excludes, so `request_id` is irrelevant (the built-in exclude
        // path is taken). Reuses the driver's compiled glob + walk verbatim.
        let compiled = compile_glob(&self.skip, "heph.fs.glob", &resolved, &[])?;
        let artifacts = walk_glob(ctx.root, &compiled)?;

        let pkg_prefix = (!ctx.pkg.is_empty()).then(|| std::path::Path::new(ctx.pkg));

        let mut paths: Vec<String> = Vec::new();
        for artifact in &artifacts {
            let Content::File(file) = &artifact.content else {
                continue;
            };
            // `out_path` is root-relative; strip the package prefix to make it
            // package-relative (a no-op at the workspace root).
            let rel = std::path::Path::new(&file.out_path);
            let pkg_rel = pkg_prefix
                .and_then(|prefix| rel.strip_prefix(prefix).ok())
                .unwrap_or(rel);
            let s = pkg_rel.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "heph.fs.glob: entry path is not valid UTF-8: {}",
                    pkg_rel.display()
                )
            })?;
            paths.push(s.to_string());
        }

        paths.sort();
        Ok(Value::List(paths.into_iter().map(Value::String).collect()))
    }
}

/// Lexical path cleanup, matching Go's `path.Clean`: collapses `.` and inner
/// `..`, drops redundant slashes, and preserves a leading `..`/`/`. Returns "."
/// for an empty result. Purely lexical — never touches the filesystem.
fn path_clean(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if rooted || matches!(out.last(), Some(&last) if last != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

/// `heph.fs.join(*elems)` — join path elements with `/` and clean the result
/// (Go `path.Join` semantics). Empty elements are skipped; all-empty yields "".
fn path_join(elems: &[&str]) -> String {
    let non_empty: Vec<&str> = elems.iter().copied().filter(|e| !e.is_empty()).collect();
    if non_empty.is_empty() {
        return String::new();
    }
    path_clean(&non_empty.join("/"))
}

/// `heph.fs.dir(path)` — everything but the last element, cleaned (Go
/// `path.Dir`). Returns "." when there is no directory part.
fn path_dir(path: &str) -> String {
    match path.rfind('/') {
        // `split_at` keeps the leading-slash case rooted (Go `path.Dir("/a") == "/"`).
        Some(i) => path_clean(path.split_at(i + 1).0),
        None => path_clean(""),
    }
}

/// `heph.fs.base(path)` — the last element of `path` (Go `path.Base`). Trailing
/// slashes are stripped first; returns "." for empty and "/" for all-slashes.
fn path_base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rsplit_once('/') {
        Some((_, base)) => base.to_string(),
        None => trimmed.to_string(),
    }
}

/// Extracts a single string positional arg for `fn_name`, erroring on a missing
/// or non-string value.
fn str_arg<'a>(fn_name: &str, args: &'a FnArgs) -> anyhow::Result<&'a str> {
    match args.positional.first() {
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(other) => anyhow::bail!("{fn_name}: argument must be a string, got {other:?}"),
        None => anyhow::bail!("{fn_name}: missing path argument"),
    }
}

struct JoinFn;

#[async_trait]
impl ProviderFn for JoinFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value> {
        let mut elems: Vec<&str> = Vec::with_capacity(args.positional.len());
        for v in &args.positional {
            match v {
                Value::String(s) => elems.push(s.as_str()),
                other => anyhow::bail!("heph.fs.join: arguments must be strings, got {other:?}"),
            }
        }
        Ok(Value::String(path_join(&elems)))
    }
}

struct DirFn;

#[async_trait]
impl ProviderFn for DirFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value> {
        Ok(Value::String(path_dir(str_arg("heph.fs.dir", &args)?)))
    }
}

struct BaseFn;

#[async_trait]
impl ProviderFn for BaseFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value> {
        Ok(Value::String(path_base(str_arg("heph.fs.base", &args)?)))
    }
}

// ─── Driver ──────────────────────────────────────────────────────────────────

/// Compiles a set of glob patterns into a single `wax::Any`, reusing the
/// process-wide [`cached_glob`] cache for each pattern.
fn compile_any(patterns: &[String]) -> anyhow::Result<Arc<wax::Any<'static>>> {
    let globs = patterns
        .iter()
        .map(|s| {
            cached_glob(s)
                .map(|g| (*g).clone())
                .with_context(|| format!("invalid exclude pattern '{s}'"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Arc::new(
        wax::any(globs).context("compiling exclude patterns")?,
    ))
}

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
    // `end` always lands on a `/` separator (ASCII) or 0/len, so it is a valid
    // char boundary; `get` keeps that guarantee explicit and panic-free.
    pattern.get(..end).unwrap_or(pattern)
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
    /// Engine-provided dirs/globs to prune during the walk.
    skip: Arc<Ignore>,
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

/// Per-request cache of compiled exclude `Any` unions.
///
/// Outer key is the `request_id`; inner key is the sorted user-exclude set.
/// Within a request, targets sharing the same excludes (common when a rule
/// template fans out across packages) reuse one `wax::any` regex union. Built-in
/// excludes are constant and prepended on build, so the user set alone keys the
/// inner entry.
/// One request's excludes: sorted-exclude-set key → compiled `Any`.
type ExcludeBucket = FxHashMap<String, Arc<wax::Any<'static>>>;
/// request_id → its [`ExcludeBucket`].
type ExcludeCache = FxHashMap<String, ExcludeBucket>;

fn exclude_any_cache() -> &'static RwLock<ExcludeCache> {
    static CACHE: OnceLock<RwLock<ExcludeCache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(ExcludeCache::default()))
}

/// Per-request cache of glob walk results.
///
/// fs-glob targets are `CacheConfig::off()`, so without this `run` re-walks the
/// tree (walkdir + per-entry stat) on every call. Within one request the on-disk
/// tree is immutable, so the artifact set for a given `(root, pattern, excludes)`
/// is stable — memoize it and skip the walk on repeat calls. Keyed by
/// `request_id` (outer) and `(root, pattern, excludes)` (inner) so it never
/// reuses a stale tree across requests.
type GlobResultBucket = FxHashMap<String, Arc<Vec<OutputArtifact>>>;
/// request_id → its [`GlobResultBucket`].
type GlobResultCache = FxHashMap<String, GlobResultBucket>;

fn glob_result_cache() -> &'static RwLock<GlobResultCache> {
    static CACHE: OnceLock<RwLock<GlobResultCache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(GlobResultCache::default()))
}

/// Returns the compiled exclude `Any` (engine ignore globs + user `exclude`) for
/// `request_id`, memoizing across calls within that request. `exclude` must be
/// non-empty; the no-exclude case reuses the ignore's file matcher directly.
fn cached_exclude_any(
    skip: &Ignore,
    request_id: &str,
    exclude: &[String],
) -> anyhow::Result<Arc<wax::Any<'static>>> {
    let mut sorted: Vec<&str> = exclude.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let key = sorted.join("\0");

    if let Some(a) = exclude_any_cache()
        .read()
        .get(request_id)
        .and_then(|m| m.get(&key))
    {
        return Ok(a.clone());
    }

    let patterns: Vec<String> = skip
        .globs()
        .iter()
        .cloned()
        .chain(exclude.iter().cloned())
        .collect();
    let any = compile_any(&patterns)?;

    Ok(exclude_any_cache()
        .write()
        .entry(request_id.to_owned())
        .or_default()
        .entry(key)
        .or_insert(any)
        .clone())
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

fn compile_glob(
    skip: &Arc<Ignore>,
    request_id: &str,
    pattern: &str,
    exclude: &[String],
) -> anyhow::Result<CompiledGlob> {
    let glob = cached_glob(pattern).with_context(|| format!("invalid glob pattern '{pattern}'"))?;

    // No user excludes: reuse the ignore's prebuilt file matcher directly.
    let not = if exclude.is_empty() {
        skip.file_matcher().clone()
    } else {
        cached_exclude_any(skip, request_id, exclude)?
    };

    Ok(CompiledGlob {
        glob,
        not,
        prefix: literal_prefix(pattern).to_owned(),
        skip: skip.clone(),
    })
}

/// Walks `root` for files matching `compiled`, returning their artifacts.
///
/// Starts at the pattern's literal prefix so a rooted pattern (`a/b/**/*`) scans
/// only `<root>/a/b`, not the whole tree. Matching uses the cached glob/exclude
/// NFAs directly — no per-run regex compilation.
fn walk_glob(
    root: &std::path::Path,
    compiled: &CompiledGlob,
) -> anyhow::Result<Vec<OutputArtifact>> {
    let walk_root = if compiled.prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&compiled.prefix)
    };

    let mut artifacts = vec![];

    let walker = walkdir::WalkDir::new(&walk_root)
        .into_iter()
        .filter_entry(|entry| {
            // Never descend into the engine's skip dirs (heph home + literal
            // `fs.skip` dirs) or skip-glob subtrees (e.g. `**/node_modules/**`).
            if !entry.file_type().is_dir() {
                return true;
            }
            let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
            !compiled.skip.prune_dir(entry.path(), rel)
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                // A missing walk root (or a path that vanished mid-walk) is an
                // empty match, not an error.
                if e.io_error()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                {
                    continue;
                }
                return Err(anyhow::Error::from(e)).context("walking glob entries");
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

        // Skip net-new codegen outputs stamped back into the tree — sourcing
        // them here would double-source the generated content.
        if has_codegen_xattr(abs_path) {
            continue;
        }

        let rel_str = rel.to_str().ok_or_else(|| {
            anyhow::anyhow!("glob entry path is not valid UTF-8: {}", rel.display())
        })?;

        // Resolve through symlinks. The walker has follow_links off (so it never
        // descends INTO a symlinked dir), but an individual symlink entry that
        // points at a regular file is still a valid source. Stat the target so
        // the exec marker and the hashed content both come from what
        // `file_hashout` actually opens — and so a symlink-to-dir (or a
        // dangling/vanished link) is skipped instead of erroring on read.
        let meta = match std::fs::metadata(abs_path) {
            Ok(m) => m,
            // A dangling/vanished symlink resolves to nothing — skip, don't fail.
            // Other stat errors (e.g. permission) still surface with context.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("stat glob entry '{}'", abs_path.display()));
            }
        };
        if meta.is_dir() {
            continue;
        }

        let x = is_exec(&meta);
        let hashout = file_hashout(abs_path, x)
            .with_context(|| format!("hash glob entry '{}'", abs_path.display()))?;

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

    Ok(artifacts)
}

/// Returns the glob walk artifacts for `(root, pattern, exclude)`, memoizing
/// across calls within `request_id`. The first call walks; repeats reuse the
/// cached `Arc`.
fn cached_glob_walk(
    request_id: &str,
    root: &std::path::Path,
    pattern: &str,
    exclude: &[String],
    compiled: &CompiledGlob,
) -> anyhow::Result<Arc<Vec<OutputArtifact>>> {
    let mut sorted: Vec<&str> = exclude.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    // `root` is part of the key: the same pattern walked under a different tree
    // root yields different artifacts (and source paths).
    let key = format!("{}\0{}\0{}", root.display(), pattern, sorted.join("\u{1}"));

    if let Some(a) = glob_result_cache()
        .read()
        .get(request_id)
        .and_then(|m| m.get(&key))
    {
        return Ok(a.clone());
    }

    let artifacts = Arc::new(walk_glob(root, compiled)?);

    Ok(glob_result_cache()
        .write()
        .entry(request_id.to_owned())
        .or_default()
        .entry(key)
        .or_insert(artifacts)
        .clone())
}

#[derive(Default)]
pub struct Driver {
    /// Engine-owned + built-in dirs pruned during glob walks.
    skip: Arc<Ignore>,
}

impl Driver {
    pub fn new(skip: Arc<Ignore>) -> Self {
        Self { skip }
    }
}

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Content identity for a sourced file: a hash of its bytes plus the executable
/// marker. Deliberately ignores size and mtime — only the content and the `x`
/// bit determine the artifact, so a file rewritten with identical bytes (new
/// mtime, same content) hashes the same and stays a cache hit.
fn file_hashout(path: &std::path::Path, x: bool) -> anyhow::Result<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)
        .with_context(|| format!("open file for hashing '{}'", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut h = Xxh3::new();
    // Stream in chunks so large inputs never load wholesale into memory.
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("read file for hashing '{}'", path.display()))?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            h.update(chunk);
        }
    }
    h.update(&[x as u8]);
    Ok(format!("{:x}", h.digest()))
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
                Some(Value::String(s)) => Ok(Some(s.clone())),
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

                let compiled = Arc::new(compile_glob(
                    &self.skip,
                    &req.request_id,
                    &pattern,
                    &exclude,
                )?);
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

                // A file() over a net-new codegen output (stamped back into the
                // tree) resolves to nothing — the generated content must only be
                // sourced from its generator, never re-read as raw source.
                if has_codegen_xattr(&abs) {
                    return Ok(RunResponse {
                        artifacts: vec![],
                        ..Default::default()
                    });
                }

                let meta = std::fs::metadata(&abs)
                    .with_context(|| format!("stat file '{}'", abs.display()))?;
                let x = is_exec(&meta);
                let hashout = file_hashout(&abs, x)
                    .with_context(|| format!("hash file '{}'", abs.display()))?;

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

            FsDef::Glob {
                pattern,
                exclude,
                compiled,
            } => {
                // Within a request the tree is immutable, so the walk result for
                // this `(root, pattern, excludes)` is memoized — repeat calls
                // skip walkdir + per-entry stat entirely.
                let artifacts = cached_glob_walk(req.request_id, root, pattern, exclude, compiled)?;

                Ok(RunResponse {
                    artifacts: (*artifacts).clone(),
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

    // ─── Exposed-function (glob) tests ─────────────────────────────────────

    fn call_glob(root: &std::path::Path, pkg: &str, pattern: &str) -> Vec<String> {
        call_glob_with_skip(root, pkg, pattern, Arc::<Ignore>::default())
    }

    fn call_glob_with_skip(
        root: &std::path::Path,
        pkg: &str,
        pattern: &str,
        skip: Arc<Ignore>,
    ) -> Vec<String> {
        let ctx = FnCallContext { pkg, root };
        let args = FnArgs {
            positional: vec![Value::String(pattern.to_string())],
            named: Default::default(),
        };
        let v = futures::executor::block_on(GlobFn { skip }.call(&ctx, args)).unwrap();
        match v {
            Value::List(l) => l
                .into_iter()
                .map(|e| match e {
                    Value::String(s) => s,
                    other => panic!("expected string, got {other:?}"),
                })
                .collect(),
            other => panic!("expected list, got {other:?}"),
        }
    }

    #[test]
    fn test_glob_fn_pkg_relative_and_skips_dirs() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("mypkg");
        fs::create_dir_all(pkg.join("src")).unwrap();
        fs::write(pkg.join("src").join("a.yaml"), "").unwrap();
        fs::write(pkg.join("src").join("b.yaml"), "").unwrap();
        fs::write(pkg.join("src").join("c.txt"), "").unwrap();

        let got = call_glob(tmp.path(), "mypkg", "src/*.yaml");
        assert_eq!(
            got,
            vec!["src/a.yaml".to_string(), "src/b.yaml".to_string()]
        );
    }

    #[test]
    fn test_glob_fn_excludes_skip_globs() {
        let tmp = tempdir().unwrap();
        let pkg = tmp.path().join("mypkg");
        fs::create_dir_all(pkg.join("node_modules/dep")).unwrap();
        fs::write(pkg.join("node_modules/dep/index.js"), "").unwrap();
        fs::write(pkg.join("main.rs"), "").unwrap();

        // The engine's skip globs prune matching subtrees for `heph.fs.glob` too.
        let skip = Arc::new(Ignore::new(&[], &["**/node_modules/**".to_string()]).unwrap());
        let got = call_glob_with_skip(tmp.path(), "mypkg", "**/*", skip);
        assert!(!got.iter().any(|s| s.contains("node_modules")), "{got:?}");
        assert!(got.contains(&"main.rs".to_string()), "{got:?}");
    }

    #[test]
    fn test_glob_fn_at_root_unprefixed() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.yaml"), "").unwrap();
        fs::write(tmp.path().join("b.yaml"), "").unwrap();

        let got = call_glob(tmp.path(), "", "*.yaml");
        assert_eq!(got, vec!["a.yaml".to_string(), "b.yaml".to_string()]);
    }

    // ─── Path helper (join/dir/base) tests ─────────────────────────────────

    fn call_path_fn(f: &dyn ProviderFn, args: Vec<&str>) -> String {
        let root = std::path::Path::new("/");
        let ctx = FnCallContext { pkg: "", root };
        let args = FnArgs {
            positional: args
                .into_iter()
                .map(|s| Value::String(s.to_string()))
                .collect(),
            named: Default::default(),
        };
        match futures::executor::block_on(f.call(&ctx, args)).unwrap() {
            Value::String(s) => s,
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn test_join_fn() {
        assert_eq!(call_path_fn(&JoinFn, vec!["a", "b", "c"]), "a/b/c");
        // Cleans embedded `.`/`..` and redundant separators.
        assert_eq!(call_path_fn(&JoinFn, vec!["a/", "b"]), "a/b");
        assert_eq!(call_path_fn(&JoinFn, vec!["a", "..", "b"]), "b");
        assert_eq!(call_path_fn(&JoinFn, vec!["a", ".", "b"]), "a/b");
        // Empty elements are skipped; all-empty yields "".
        assert_eq!(call_path_fn(&JoinFn, vec!["", "a", ""]), "a");
        assert_eq!(call_path_fn(&JoinFn, vec!["", ""]), "");
        assert_eq!(call_path_fn(&JoinFn, vec![]), "");
    }

    #[test]
    fn test_dir_fn() {
        assert_eq!(call_path_fn(&DirFn, vec!["a/b/c"]), "a/b");
        assert_eq!(call_path_fn(&DirFn, vec!["a"]), ".");
        assert_eq!(call_path_fn(&DirFn, vec!["/a/b"]), "/a");
        assert_eq!(call_path_fn(&DirFn, vec!["/a"]), "/");
        assert_eq!(call_path_fn(&DirFn, vec![""]), ".");
        assert_eq!(call_path_fn(&DirFn, vec!["a/b/"]), "a/b");
    }

    #[test]
    fn test_base_fn() {
        assert_eq!(call_path_fn(&BaseFn, vec!["a/b/c.rs"]), "c.rs");
        assert_eq!(call_path_fn(&BaseFn, vec!["c.rs"]), "c.rs");
        assert_eq!(call_path_fn(&BaseFn, vec!["a/b/"]), "b");
        assert_eq!(call_path_fn(&BaseFn, vec![""]), ".");
        assert_eq!(call_path_fn(&BaseFn, vec!["/"]), "/");
        assert_eq!(call_path_fn(&BaseFn, vec!["///"]), "/");
    }

    #[test]
    fn test_dir_fn_requires_string() {
        let root = std::path::Path::new("/");
        let ctx = FnCallContext { pkg: "", root };
        let err = futures::executor::block_on(DirFn.call(&ctx, FnArgs::default())).unwrap_err();
        assert!(err.to_string().contains("missing path argument"), "{err}");
    }

    // ─── Provider tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_provider_wrong_package_not_found() {
        let p = Provider::default();
        let result = p
            .get(make_get_req("//other/pkg:file@f=foo.txt"), &ctoken())
            .await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_provider_wrong_name_not_found() {
        let p = Provider::default();
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
        let p = Provider::default();
        let result = p
            .get(make_get_req(&format!("//{PKG}:file@f=foo.txt")), &ctoken())
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
        assert_eq!(result.target_spec.addr.name, "file");
        assert_eq!(
            result.target_spec.config.get("f"),
            Some(&Value::String("foo.txt".to_string()))
        );
    }

    #[tokio::test]
    async fn test_provider_glob_returns_spec() {
        let p = Provider::default();
        let result = p
            .get(make_get_req(&format!("//{PKG}:glob@p=src/*.rs")), &ctoken())
            .await
            .unwrap();
        assert_eq!(result.target_spec.driver, DRIVER_NAME);
        assert_eq!(result.target_spec.addr.name, "glob");
        assert_eq!(
            result.target_spec.config.get("p"),
            Some(&Value::String("src/*.rs".to_string()))
        );
    }

    // ─── Driver tests ──────────────────────────────────────────────────────

    fn make_parse_req(config: std::collections::HashMap<String, Value>) -> ParseRequest {
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
        let driver = Driver::default();
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
        let driver = Driver::default();
        // Unmatched alternation — wax rejects at parse time.
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("ogen.{yml,yaml".to_string()),
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
        let driver = Driver::default();
        let config = std::collections::HashMap::from([
            ("p".to_string(), Value::String("*.rs".to_string())),
            (
                "e".to_string(),
                Value::String("good/**,bad.{yml".to_string()),
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
        let driver = Driver::default();
        let config = std::collections::HashMap::from([(
            "f".to_string(),
            Value::String("src/main.rs".to_string()),
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
        let driver = Driver::default();
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("src/*.rs".to_string()),
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
        let driver = Driver::default();
        let config = std::collections::HashMap::from([
            ("p".to_string(), Value::String("**/*.rs".to_string())),
            (
                "e".to_string(),
                Value::String("vendor/**,generated/**".to_string()),
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
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let file_path = tmp.path().join("hello.txt");
        fs::write(&file_path, b"hello world").unwrap();

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            Value::String("hello.txt".to_string()),
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
        let driver = Driver::default();
        let tmp = tempdir().unwrap();

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            Value::String("nonexistent.txt".to_string()),
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

    #[test]
    fn test_file_hashout_is_content_based() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"same bytes").unwrap();
        fs::write(&b, b"same bytes").unwrap();
        // Identical bytes + same exec marker hash identically, regardless of path.
        assert_eq!(
            file_hashout(&a, false).unwrap(),
            file_hashout(&b, false).unwrap()
        );
        fs::write(&b, b"other bytes").unwrap();
        assert_ne!(
            file_hashout(&a, false).unwrap(),
            file_hashout(&b, false).unwrap()
        );
    }

    #[test]
    fn test_file_hashout_includes_exec_marker() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f");
        fs::write(&p, b"#!/bin/sh\n").unwrap();
        // Same bytes, different exec marker => different identity.
        assert_ne!(
            file_hashout(&p, false).unwrap(),
            file_hashout(&p, true).unwrap()
        );
    }

    #[test]
    fn test_file_hashout_ignores_mtime() {
        use std::time::{Duration, SystemTime};
        let dir = tempdir().unwrap();
        let p = dir.path().join("f");
        fs::write(&p, b"stable content").unwrap();
        let before = file_hashout(&p, false).unwrap();
        // Bump mtime far into the future without touching the bytes; the content
        // hash must not move (mtime is no longer part of the identity).
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(60 * 60 * 24 * 365))
            .unwrap();
        drop(f);
        let after = file_hashout(&p, false).unwrap();
        assert_eq!(
            before, after,
            "mtime change must not affect the content hash"
        );
    }

    /// A glob over a tree containing a symlink-to-dir (matching the pattern) and a
    /// dangling symlink must NOT error: `file_hashout` opens+reads, so these
    /// would otherwise blow up with EISDIR/ENOENT. A symlink-to-FILE is sourced,
    /// with its exec marker and content both taken from the followed target (so
    /// it hashes identically to the target itself).
    #[cfg(unix)]
    #[tokio::test]
    async fn test_driver_run_glob_follows_file_links_and_skips_dir_links() {
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("real.txt"), b"real").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        // Symlink to a real dir, named to MATCH the *.txt pattern.
        std::os::unix::fs::symlink(root.join("sub"), root.join("subdir.txt")).unwrap();
        // Symlink to the real file.
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();
        // Dangling symlink that matches the pattern.
        std::os::unix::fs::symlink(root.join("missing"), root.join("dangling.txt")).unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("*.txt".to_string()),
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
            root.to_path_buf(),
            &hashin,
        );
        // The headline regression: must not error on the symlinked dir / dangling.
        let res = driver.run(req, &ctoken()).await.unwrap();

        let by_name: std::collections::HashMap<&str, &str> = res
            .artifacts
            .iter()
            .map(|a| (a.name.as_str(), a.hashout.as_str()))
            .collect();
        assert!(by_name.contains_key("real.txt"));
        assert!(by_name.contains_key("link.txt"));
        assert!(
            !by_name.contains_key("subdir.txt"),
            "a symlink-to-dir must be skipped, not hashed"
        );
        assert!(
            !by_name.contains_key("dangling.txt"),
            "a dangling symlink must be skipped"
        );
        // Exec marker + content both come from the followed target, so the link
        // hashes the same as the file it points at (NOT the symlink's own mode).
        assert_eq!(by_name["link.txt"], by_name["real.txt"]);
    }

    #[tokio::test]
    async fn test_driver_run_glob_matches_files() {
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), b"").unwrap();
        fs::write(tmp.path().join("b.rs"), b"").unwrap();
        fs::write(tmp.path().join("c.txt"), b"").unwrap();

        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("*.rs".to_string()))]);
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
    async fn test_driver_run_glob_excludes_engine_skip_dir() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join(".heph3");
        fs::create_dir_all(home.join("cache")).unwrap();
        fs::write(home.join("cache").join("blob"), b"").unwrap();
        fs::write(tmp.path().join("main.rs"), b"").unwrap();

        // The engine hands the fs plugin its skip dirs (the heph home); the walk
        // must prune that subtree.
        let skip = Arc::new(Ignore::new(&[home], &[]).unwrap());
        let driver = Driver::new(skip);

        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("**/*".to_string()))]);
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
        assert_eq!(
            names,
            vec!["main.rs"],
            "engine skip dir must be pruned: {names:?}"
        );
    }

    #[tokio::test]
    async fn test_driver_run_glob_excludes_config_skip_dir() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("vendor")).unwrap();
        fs::write(tmp.path().join("vendor").join("dep.rs"), b"").unwrap();
        fs::write(tmp.path().join("main.rs"), b"").unwrap();

        // A `fs.skip` dir from the config file (resolved to an absolute path) is
        // pruned just like the engine home.
        let skip = Arc::new(Ignore::new(&[tmp.path().join("vendor")], &[]).unwrap());
        let driver = Driver::new(skip);

        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("**/*".to_string()))]);
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
        assert_eq!(
            names,
            vec!["main.rs"],
            "config skip dir must be pruned: {names:?}"
        );
    }

    #[tokio::test]
    async fn test_driver_run_glob_excludes_config_skip_glob() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("pkg/node_modules/dep")).unwrap();
        fs::write(tmp.path().join("pkg/node_modules/dep/index.js"), b"").unwrap();
        fs::write(tmp.path().join("pkg/main.rs"), b"").unwrap();

        // A `fs.skip` glob (`**/node_modules/**`) excludes the whole subtree at
        // any depth — and prunes the dir so the walk never descends into it.
        let skip = Arc::new(Ignore::new(&[], &["**/node_modules/**".to_string()]).unwrap());
        let driver = Driver::new(skip);

        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("**/*".to_string()))]);
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
        assert_eq!(
            names,
            vec!["pkg_main.rs"],
            "config skip glob must exclude node_modules subtree: {names:?}"
        );
    }

    #[tokio::test]
    async fn test_driver_run_glob_user_exclude() {
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("keep.rs"), b"").unwrap();
        fs::write(tmp.path().join("skip.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([
            ("p".to_string(), Value::String("*.rs".to_string())),
            ("e".to_string(), Value::String("skip.rs".to_string())),
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
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("lib.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("**/*.rs".to_string()),
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
        let p = Provider::default();
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
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let nested = tmp.path().join("mgmt/protos/x/bsdiff/v1");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("proto.proto"), b"").unwrap();
        fs::write(tmp.path().join("mgmt/protos/x/bsdiff/BUILD"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("mgmt/protos/x/bsdiff/./**/*".to_string()),
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
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("mgmt/protos/x")).unwrap();
        fs::write(tmp.path().join("mgmt/protos/x/go.mod"), b"").unwrap();
        fs::write(tmp.path().join("mgmt/protos/x/other.txt"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("mgmt/protos/x/bsdiff/../go.mod".to_string()),
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
        let driver = Driver::default();
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("some/../../file".to_string()),
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
        let driver = Driver::default();
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("../escape/**".to_string()),
        )]);
        let result = driver.parse(make_parse_req(config), &ctoken()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_driver_parse_file_rejects_root_escape() {
        let driver = Driver::default();
        let config = std::collections::HashMap::from([(
            "f".to_string(),
            Value::String("a/../../file.txt".to_string()),
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
        let driver = Driver::default();
        let with_parent = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("a/b/../c/**/*.rs".to_string()),
        )]);
        let collapsed = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("a/c/**/*.rs".to_string()),
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
        let driver = Driver::default();
        let with_dot = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("a/./b/**/*.rs".to_string()),
        )]);
        let without_dot = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("a/b/**/*.rs".to_string()),
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
        let driver = Driver::default();
        let tmp = tempdir().unwrap();

        let pattern = "does/not/exist/*.rs";
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String(pattern.to_string()),
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
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let missing_root = tmp.path().join("nonexistent");

        let pattern = "*.rs";
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String(pattern.to_string()),
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
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let sub = tmp.path().join("dir");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("ogen.yml"), b"").unwrap();
        fs::write(sub.join("ogen.yaml"), b"").unwrap();
        fs::write(sub.join("other.txt"), b"").unwrap();

        let pattern = "dir/ogen.{yml,yaml}";
        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String(pattern.to_string()),
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
    fn test_compile_glob_no_exclude_reuses_builtin() {
        let skip = Arc::new(Ignore::default());
        let compiled = compile_glob(&skip, "req-test", "**/*.rs", &[]).unwrap();
        assert!(
            Arc::ptr_eq(&compiled.not, skip.file_matcher()),
            "empty-exclude path must reuse the ignore's prebuilt file matcher"
        );
    }

    #[test]
    fn test_compile_glob_user_exclude_shares_any() {
        // Within one request, the same exclude set (any order) reuses one
        // compiled `Any`; a different set does not.
        let skip = Arc::new(Ignore::default());
        let req = "req-share";
        let a = compile_glob(
            &skip,
            req,
            "**/*.go",
            &["vendor/**".into(), "out/**".into()],
        )
        .unwrap();
        let b = compile_glob(
            &skip,
            req,
            "src/**/*.go",
            &["out/**".into(), "vendor/**".into()],
        )
        .unwrap();
        assert!(
            Arc::ptr_eq(&a.not, &b.not),
            "identical exclude sets must share one Any regardless of order"
        );

        let c = compile_glob(&skip, req, "**/*.go", &["other/**".into()]).unwrap();
        assert!(
            !Arc::ptr_eq(&a.not, &c.not),
            "distinct exclude sets must not share an Any"
        );
    }

    #[test]
    fn test_compile_glob_exclude_cache_is_per_request() {
        // Different request ids do not share exclude `Any`s, even for the same
        // set — buckets are keyed by request.
        let skip = Arc::new(Ignore::default());
        let exclude = ["vendor/**".to_string()];
        let a = compile_glob(&skip, "req-iso-a", "**/*.go", &exclude).unwrap();
        let b = compile_glob(&skip, "req-iso-b", "**/*.go", &exclude).unwrap();
        assert!(
            !Arc::ptr_eq(&a.not, &b.not),
            "distinct requests must not share an exclude Any"
        );
    }

    #[tokio::test]
    async fn test_driver_run_glob_memoized_per_request_skips_rewalk() {
        // Within one request the walk result is memoized: a file created after
        // the first run must NOT appear in a second run with the same id.
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), b"").unwrap();

        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("*.rs".to_string()))]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        // Unique request id so this test does not collide with the shared cache.
        let request_id = "test-memo-rewalk".to_string();
        let hashin = String::new();

        let res1 = driver
            .run(
                make_run_req(
                    &parse_res.target_def,
                    &request_id,
                    tmp.path().to_path_buf(),
                    &hashin,
                ),
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(res1.artifacts.len(), 1);

        // New file on disk; a fresh walk would see it, the memoized result won't.
        fs::write(tmp.path().join("b.rs"), b"").unwrap();

        let res2 = driver
            .run(
                make_run_req(
                    &parse_res.target_def,
                    &request_id,
                    tmp.path().to_path_buf(),
                    &hashin,
                ),
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(
            res2.artifacts.len(),
            1,
            "second run in same request must reuse memoized walk"
        );
    }

    #[tokio::test]
    async fn test_driver_run_glob_distinct_requests_rewalk() {
        // A different request id must not see the first request's memoized walk.
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), b"").unwrap();

        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("*.rs".to_string()))]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();

        let hashin = String::new();
        let req_a = "test-memo-iso-a".to_string();
        let res1 = driver
            .run(
                make_run_req(
                    &parse_res.target_def,
                    &req_a,
                    tmp.path().to_path_buf(),
                    &hashin,
                ),
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(res1.artifacts.len(), 1);

        fs::write(tmp.path().join("b.rs"), b"").unwrap();

        let req_b = "test-memo-iso-b".to_string();
        let res2 = driver
            .run(
                make_run_req(
                    &parse_res.target_def,
                    &req_b,
                    tmp.path().to_path_buf(),
                    &hashin,
                ),
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(
            res2.artifacts.len(),
            2,
            "distinct request must walk fresh and see the new file"
        );
    }

    #[test]
    fn test_literal_prefix() {
        assert_eq!(literal_prefix("mgmt/protos/x/**/*"), "mgmt/protos/x");
        // Fully-literal pattern returns itself (walk visits just that path).
        assert_eq!(
            literal_prefix("mgmt/protos/x/go.mod"),
            "mgmt/protos/x/go.mod"
        );
        assert_eq!(literal_prefix("*.rs"), "");
        assert_eq!(literal_prefix("**/*.rs"), "");
        assert_eq!(literal_prefix("dir/ogen.{yml,yaml}"), "dir");
        assert_eq!(literal_prefix("a/b?c/d"), "a");
    }

    #[tokio::test]
    async fn test_driver_run_glob_rooted_prefix_ignores_siblings() {
        // A rooted pattern walks only its literal-prefix subtree; sibling
        // directories with matching-looking files must not appear.
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("pkg/sub")).unwrap();
        fs::write(tmp.path().join("pkg/keep.rs"), b"").unwrap();
        fs::write(tmp.path().join("pkg/sub/deep.rs"), b"").unwrap();
        fs::create_dir_all(tmp.path().join("other")).unwrap();
        fs::write(tmp.path().join("other/nope.rs"), b"").unwrap();

        let config = std::collections::HashMap::from([(
            "p".to_string(),
            Value::String("pkg/**/*.rs".to_string()),
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
        let p = Provider::default();
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

    // ─── Codegen-xattr exclusion ───────────────────────────────────────────

    /// Stamps the codegen xattr on `path` and returns whether the platform/FS
    /// actually persisted it. Tests skip their assertions when this is `false`
    /// so a tmpfs without xattr support can't make them flake.
    fn stamp_codegen(path: &std::path::Path) -> bool {
        if xattr::set(path, CODEGEN_XATTR, b"//pkg:gen").is_err() {
            return false;
        }
        has_codegen_xattr(path)
    }

    #[tokio::test]
    async fn test_driver_run_glob_excludes_stamped_codegen() {
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let plain = tmp.path().join("plain.rs");
        let generated = tmp.path().join("generated.rs");
        fs::write(&plain, b"plain").unwrap();
        fs::write(&generated, b"generated").unwrap();

        if !stamp_codegen(&generated) {
            // Filesystem here doesn't support the codegen xattr; the exclusion
            // can't be exercised, so don't assert anything misleading.
            return;
        }

        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("*.rs".to_string()))]);
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
        assert_eq!(names, vec!["plain.rs"], "stamped file must be excluded");
    }

    #[tokio::test]
    async fn test_driver_run_file_stamped_codegen_yields_nothing() {
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let generated = tmp.path().join("generated.rs");
        fs::write(&generated, b"generated").unwrap();

        if !stamp_codegen(&generated) {
            return;
        }

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            Value::String("generated.rs".to_string()),
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

        assert!(
            res.artifacts.is_empty(),
            "file() over a stamped codegen file must yield no artifacts"
        );
    }

    #[tokio::test]
    async fn test_driver_run_file_plain_yields_one_artifact() {
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        let plain = tmp.path().join("plain.rs");
        fs::write(&plain, b"plain").unwrap();

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            Value::String("plain.rs".to_string()),
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
        assert_eq!(res.artifacts[0].name, "plain.rs");
    }
}
