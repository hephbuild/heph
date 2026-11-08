use anyhow::Context;
use async_trait::async_trait;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
use hcore::htvalue::signature::{FnSignature, Param, ParamType};
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse, RunRequest, RunResponse,
    outputartifact::{Content, ContentFile, OutputArtifact, Type},
    targetdef::{
        Output, TargetDef,
        path::{CodegenMode, Content as PathContent, Path},
    },
};
use hplugin::function::{FnArgs, FnCallContext, FnOutcome, PluginFn, PluginFnDef};
use hplugin::htspec::Spec;
use hplugin::provider::{
    ConfigRequest as ProviderConfigRequest, ConfigResponse as ProviderConfigResponse, GetError,
    GetRequest, GetResponse, ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse,
    ProbeRequest, ProbeResponse, Provider as EProvider, TargetSpec,
};
use hwalk::{CachedWalker, Ignore};
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use xxhash_rust::xxh3::Xxh3;

const PKG: &str = "@heph/fs";
pub const DRIVER_NAME: &str = "fs";

/// Extended-attribute name the engine stamps on net-new codegen files written
/// back to the tree (value = the generator's addr), for an N-1 heph and
/// `heph tool codegen import`. Set only: no reader here looks at it; the
/// codegen index decides what is generated (see [`codegen`]).
pub const CODEGEN_XATTR: &str = "user.heph.codegen";

mod codegen;
pub use codegen::{
    CodegenFileError, CodegenFileErrorKind, CodegenReport, FsCodegen, NoReport, exclusion_summary,
    modified_message, reason_str, status_str, unrecorded_message,
};

/// wax's escape, for a caller building an fs glob pattern from a literal path.
pub use wax::escape;

use hwalk::HEPH_DIR_PREFIX;

/// True if `path` resolves inside a `.heph*` directory (e.g. `.heph/cache/...`),
/// pointing at an engine-internal artifact — a materialized cache output, not raw
/// workspace source — so it must be skipped by source reads like a stamped
/// codegen file.
///
/// This holds whether `path` is *itself* a symlink into the cache (`gen ->
/// .heph/…`) **or** lies *under* such a symlinked directory (`gen/some/file.txt`,
/// a real file reached through the `gen` link). [`std::fs::canonicalize`] resolves
/// every symlink along the path — ancestors included — so a file transitively
/// inside the cache is detected, not just a leaf symlink.
///
/// A dangling link, a nonexistent path, or any IO error is treated as "not a heph
/// path" so source reads never break on such trees.
pub fn resolves_into_heph_dir(path: &std::path::Path) -> bool {
    let Ok(resolved) = std::fs::canonicalize(path) else {
        return false;
    };
    has_heph_component(&resolved)
}

/// Whether any component of an already-resolved path names a `.heph*` directory.
fn has_heph_component(resolved: &std::path::Path) -> bool {
    resolved.components().any(|c| {
        matches!(c, std::path::Component::Normal(name)
            if name.to_str().is_some_and(|n| n.starts_with(HEPH_DIR_PREFIX)))
    })
}

/// [`resolves_into_heph_dir`] for a directory entry, given the answer already
/// computed for its parent directory.
///
/// `canonicalize` resolves every component, so calling it per file re-walks the
/// same ancestor chain once per entry. A glob over a package subtree visits many
/// files per directory — on a 500-package Go corpus the walk makes ~6.4k file
/// visits over 1.2k distinct files — and `canonicalize` showed up at 1.9% of a
/// fully-cached run's CPU.
///
/// The parent's answer settles a **non-symlink** entry outright: when `name` is
/// not itself a link, `canonicalize(dir/name)` is exactly
/// `canonicalize(dir).join(name)`, so its components are the parent's plus
/// `name`. Both are still checked. A symlink entry may point anywhere, so it
/// falls back to resolving the full path — which is the case the check exists
/// for (`gen -> .heph/…`).
fn entry_resolves_into_heph_dir(
    abs: &std::path::Path,
    name: &str,
    is_symlink: bool,
    parent_in_heph: bool,
) -> bool {
    if is_symlink {
        return resolves_into_heph_dir(abs);
    }
    parent_in_heph || name.starts_with(HEPH_DIR_PREFIX)
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

/// The `Addr` of the lenient single-file fs target over a `copy` target's own
/// output, `//@heph/fs:copy_output@f=<path>` (Decision 25): an excluded path
/// is zero artifacts, with no error and no warning; any other path is the
/// file, as with [`file_addr`]. Only the engine's `introspect:outputs`
/// expansion emits it (through [`output_fs_addr`]).
pub fn copy_output_addr(path: &str) -> Addr {
    Addr::new(
        PkgBuf::from(PKG),
        COPY_OUTPUT.to_string(),
        BTreeMap::from([("f".to_string(), path.to_string())]),
    )
}

const COPY_OUTPUT: &str = "copy_output";

/// The fs addr reading an output path: [`copy_output_addr`] for a `copy`
/// target's `FilePath`, strict [`file_addr`] for any other file (an
/// `in_place` output included), a glob for a glob or a directory. The one
/// mapping both `introspect:outputs` and the in-place guard use.
pub fn output_fs_addr(path: &Path) -> Addr {
    match &path.content {
        PathContent::FilePath(p) if matches!(path.codegen_tree, CodegenMode::Copy) => {
            copy_output_addr(p)
        }
        PathContent::FilePath(p) => file_addr(p),
        PathContent::Glob(p) => glob_addr(p, &[]),
        PathContent::DirPath(p) => glob_addr(&format!("{}/**/*", p.trim_end_matches('/')), &[]),
    }
}

/// What an fs addr reads, independent of how strictly: `file` and
/// `copy_output` over one path are one [`FsPathKey::File`]. Pairing on this,
/// never on raw `Addr` equality, keeps a strict/lenient mismatch from
/// silently skipping a guard.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FsPathKey {
    File(String),
    Glob {
        pattern: String,
        exclude: Option<String>,
    },
}

/// The [`FsPathKey`] of an fs addr; `None` for any other addr.
pub fn fs_path_key(addr: &Addr) -> Option<FsPathKey> {
    if !is_fs_addr(addr) {
        return None;
    }
    match addr.name.as_str() {
        "file" | COPY_OUTPUT => addr.args.get("f").cloned().map(FsPathKey::File),
        "glob" => addr.args.get("p").map(|p| FsPathKey::Glob {
            pattern: p.clone(),
            exclude: addr.args.get("e").cloned(),
        }),
        _ => None,
    }
}

/// Returns `true` if `addr` refers to a single-file fs target.
pub fn is_file_addr(addr: &Addr) -> bool {
    addr.package.as_str() == PKG && addr.name == "file"
}

/// Returns `true` if `addr` is the lenient [`copy_output_addr`] target.
pub fn is_copy_output_addr(addr: &Addr) -> bool {
    addr.package.as_str() == PKG && addr.name == COPY_OUTPUT
}

/// Returns `true` if `addr` refers to a glob fs target.
pub fn is_glob_addr(addr: &Addr) -> bool {
    addr.package.as_str() == PKG && addr.name == "glob"
}

/// Returns `true` if `addr` is any fs-provider target (e.g. `file`/`glob`), i.e.
/// it lives in the fs provider's package.
pub fn is_fs_addr(addr: &Addr) -> bool {
    addr.package.as_str() == PKG
}

// ─── Provider ────────────────────────────────────────────────────────────────

/// The `fs` provider: the source-file targets under `//@heph/fs`. Its BUILD
/// functions are the plugin's, not its own — see [`functions`].
#[derive(Default)]
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
                "file" | "glob" | COPY_OUTPUT => {}
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
                    ..Default::default()
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

/// The `fs` plugin's BUILD-file functions, `heph.fs.*`. `glob` walks with
/// `skip` and `walker` — the same `Ignore` and walker as the `fs` provider, so
/// BUILD-time expansion prunes exactly what the provider and driver prune.
///
/// `glob` and `parent` read the tree under the root (the documented
/// tree-reading exemption of `PluginFn`'s purity contract); the rest are pure
/// string handling. Without a codegen index, `glob` excludes nothing as
/// generated; see [`functions_with`].
pub fn functions(skip: &Arc<Ignore>, walker: &Arc<CachedWalker>) -> Vec<PluginFnDef> {
    functions_with(skip, walker, &FsCodegen::default())
}

/// [`functions`] whose `glob` reads the codegen index through `codegen`: the
/// same snapshot lookup as the fs driver's glob, with no engine call (J6).
pub fn functions_with(
    skip: &Arc<Ignore>,
    walker: &Arc<CachedWalker>,
    codegen: &FsCodegen,
) -> Vec<PluginFnDef> {
    {
        // A single required string positional, returning a string.
        let one_path = || FnSignature {
            positional: vec![Param::required("path", ParamType::String)],
            named: vec![],
            variadic: None,
            returns: ParamType::String,
        };
        vec![
            PluginFnDef {
                name: "glob".to_string(),
                signature: FnSignature {
                    positional: vec![Param::required("pattern", ParamType::String)],
                    named: vec![],
                    variadic: None,
                    returns: ParamType::list(ParamType::String),
                },
                doc: "Expand a glob pattern (relative to the current package) into \
                      the list of matching source-file target addresses. Engine- and \
                      build-managed directories are skipped."
                    .to_string(),
                func: Arc::new(GlobFn {
                    skip: Arc::clone(skip),
                    walker: Arc::clone(walker),
                    codegen: codegen.clone(),
                }),
            },
            PluginFnDef {
                name: "join".to_string(),
                // Variadic `join(a, b, c)` — Go `path.Join` style.
                signature: FnSignature {
                    positional: vec![],
                    named: vec![],
                    variadic: Some(Param::required("elems", ParamType::String)),
                    returns: ParamType::String,
                },
                doc: "Join path segments into a single `/`-separated path, cleaning \
                      `.`/`..` and redundant separators — Go `path.Join` style. \
                      Example: `heph.fs.join(\"a\", \"b/\", \"c\")` → `\"a/b/c\"`."
                    .to_string(),
                func: Arc::new(JoinFn),
            },
            PluginFnDef {
                name: "dir".to_string(),
                signature: one_path(),
                doc: "The directory portion of `path` (everything up to the last \
                      `/`), Go `path.Dir` style."
                    .to_string(),
                func: Arc::new(DirFn),
            },
            PluginFnDef {
                name: "base".to_string(),
                signature: one_path(),
                doc: "The final element of `path` (everything after the last `/`), \
                      Go `path.Base` style."
                    .to_string(),
                func: Arc::new(BaseFn),
            },
            PluginFnDef {
                name: "parent".to_string(),
                signature: FnSignature {
                    positional: vec![Param::required("filename", ParamType::String)],
                    named: vec![],
                    variadic: None,
                    // `null` when no ancestor holds the file, so callers can
                    // `heph.fs.parent("go.mod") or default`.
                    returns: ParamType::union(vec![ParamType::String, ParamType::Null]),
                },
                doc: "Search the current package directory and each of its \
                      ancestors (up to the workspace root) for a file named \
                      `filename`, returning the workspace-root-relative path of \
                      the closest match, or `None` if no ancestor holds it. \
                      Example: `heph.fs.parent(\"go.mod\")`."
                    .to_string(),
                func: Arc::new(ParentFn),
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
    walker: Arc<CachedWalker>,
    codegen: FsCodegen,
}

#[async_trait]
impl PluginFn for GlobFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        // Arg shape is enforced by the declared signature before we get here.
        let pattern = str_arg("heph.fs.glob", &args)?;

        let resolved = hmodel::htpkg::join_rel_checked(ctx.pkg, pattern)
            .context("heph.fs.glob: normalizing pattern")?;

        // No user excludes, so `request_id` is irrelevant (the built-in exclude
        // path is taken). Reuses the driver's compiled glob + walk verbatim.
        let compiled = compile_glob(&self.skip, "heph.fs.glob", &resolved, &[])?;
        // The same snapshot read as the driver's glob: no engine call, no
        // evaluation, no second pass. No request: events have nowhere to go,
        // warnings dedupe per engine.
        let (artifacts, excluded) = walk_glob(&self.walker, ctx.root, &compiled, &self.codegen)?;
        codegen::report(&self.codegen, None, &excluded);

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
        Ok(Value::List(paths.into_iter().map(Value::String).collect()).into())
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

/// `heph.fs.join(*elems)` — join path elements (variadic strings) with `/` and
/// clean the result (Go `path.Join` semantics). Empty elements are skipped;
/// all-empty yields "".
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
impl PluginFn for JoinFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        // Signature: `join(*elems: string) -> string`. The variadic shape (each
        // element a string) is enforced by the declared signature before we get
        // here, so every positional is a string.
        let mut elems: Vec<&str> = Vec::with_capacity(args.positional.len());
        for v in &args.positional {
            match v {
                Value::String(s) => elems.push(s.as_str()),
                other => anyhow::bail!("heph.fs.join: elems must be strings, got {other:?}"),
            }
        }
        Ok(Value::String(path_join(&elems)).into())
    }
}

struct DirFn;

#[async_trait]
impl PluginFn for DirFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        Ok(Value::String(path_dir(str_arg("heph.fs.dir", &args)?)).into())
    }
}

struct BaseFn;

#[async_trait]
impl PluginFn for BaseFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        Ok(Value::String(path_base(str_arg("heph.fs.base", &args)?)).into())
    }
}

/// `heph.fs.parent(filename)` — walk up from the calling package's directory,
/// through each ancestor up to the workspace root, and return the
/// workspace-root-relative path of the closest `filename`. The search is bounded
/// at the root so it never wanders the host filesystem (hermeticity), and the
/// starting directory itself is checked first. Returns `null` when no ancestor
/// holds the file.
struct ParentFn;

#[async_trait]
impl PluginFn for ParentFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        let filename = str_arg("heph.fs.parent", &args)?;
        // A bare name is required — a nested path would make "closest parent"
        // ambiguous and could escape the package with `..`.
        if filename.is_empty() || filename.contains('/') {
            anyhow::bail!(
                "heph.fs.parent: filename must be a non-empty bare file name, got {filename:?}"
            );
        }

        // Start at the calling package's directory and climb toward the root.
        // `dir` stays root-relative so the returned path is too; `""` is the root.
        let mut dir = std::path::PathBuf::from(ctx.pkg);
        loop {
            let candidate_abs = ctx.root.join(&dir).join(filename);
            if candidate_abs.is_file() {
                let rel = dir.join(filename);
                let s = rel.to_str().ok_or_else(|| {
                    anyhow::anyhow!("heph.fs.parent: path is not valid UTF-8: {}", rel.display())
                })?;
                return Ok(Value::String(s.to_string()).into());
            }
            // `pop` returns false at the root of the relative path — stop there
            // rather than climbing above the workspace root.
            if !dir.pop() {
                return Ok(Value::Null().into());
            }
        }
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
    /// The whole normalized pattern. Read only for the one match the split
    /// below cannot answer on its own — see [`pattern_matches`].
    pattern: String,
    glob: GlobMatcher,
    not: Arc<wax::Any<'static>>,
    /// Literal directory prefix of the pattern, used as the walk root. Whole
    /// components; a trailing `/` only on a fully-literal pattern (from
    /// `heph.fs.glob("sub/")`).
    prefix: String,
    /// Engine-provided dirs/globs to prune during the walk.
    skip: Arc<Ignore>,
}

/// How a [`CompiledGlob`] decides a match; see [`pattern_matches`].
enum GlobMatcher {
    /// A fully-literal pattern: matches exactly its prefix path.
    Literal,
    /// The pattern past its literal prefix ([`split_literal_prefix`]), matched
    /// against the path past that prefix. The common case, and the one the
    /// [`glob_cache`] is for.
    Remainder(Arc<wax::Glob<'static>>),
    /// The whole pattern, matched against the whole root-relative path. Taken
    /// when the remainder does not parse on its own: wax rejects a few token
    /// sequences in leading position only (`pkg/(?i)**` parses, `(?i)**` does
    /// not).
    Whole(Arc<wax::Glob<'static>>),
}

/// Process-global cache of compiled globs, keyed by the string compiled —
/// usually the pattern past its literal prefix ([`split_literal_prefix`]).
///
/// Each `wax::Glob::new` builds a regex NFA, and the go provider roots one
/// source glob per package (`<pkg>/**/*`, `compute_pkg_src_addrs`), so
/// patterns that differ only in their literal prefix are the common case.
/// Keyed on the remainder, a workspace compiles a handful (`**/*`, `**/*.go`,
/// …) once each. Read-mostly: the lock is contended only on first sight of
/// each key.
fn glob_cache() -> &'static RwLock<FxHashMap<String, Arc<wax::Glob<'static>>>> {
    static CACHE: OnceLock<RwLock<FxHashMap<String, Arc<wax::Glob<'static>>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(FxHashMap::default()))
}

/// Returns the compiled `Glob` for `pattern`, memoizing across calls. Every
/// caller gets the one `Arc` that won the insert, even when several compiled
/// it concurrently.
fn cached_glob(pattern: &str) -> Result<Arc<wax::Glob<'static>>, wax::BuildError> {
    if let Some(g) = glob_cache().read().get(pattern) {
        return Ok(g.clone());
    }
    let glob = Arc::new(wax::Glob::new(pattern).map(wax::Glob::into_owned)?);
    Ok(glob_cache()
        .write()
        .entry(pattern.to_owned())
        .or_insert(glob)
        .clone())
}

/// `pattern` split at its [`literal_prefix`]: the prefix, and the rest without
/// the separating `/`. `mgmt/protos/x/**/*` → (`mgmt/protos/x`, `**/*`),
/// `**/*.rs` → (``, `**/*.rs`), and a fully-literal pattern → (itself, ``).
fn split_literal_prefix(pattern: &str) -> (&str, &str) {
    let prefix = literal_prefix(pattern);
    // The prefix is a run of whole components, so the cut lands on a `/` or at
    // an end and never splits a component.
    let rest = pattern.get(prefix.len()..).unwrap_or_default();
    (prefix, rest.strip_prefix('/').unwrap_or(rest))
}

/// Whether the root-relative path `rel` matches `compiled`'s pattern: it lies
/// under the literal prefix, and the compiled remainder matches what follows.
///
/// Equivalent to matching the whole pattern against `rel` for every pattern
/// [`compile_glob`] accepts — which are exactly the patterns wax accepts. The
/// prefix holds no wax syntax that could match anything but itself, so the
/// regex is needed only past it. `test_pattern_matches_agrees_with_whole_pattern`
/// holds it to wax's answer.
///
/// Fails only if the whole pattern does not compile when it is needed, which
/// `compile_glob`'s parse check rules out short of wax's regex size limit.
fn pattern_matches(compiled: &CompiledGlob, rel: &std::path::Path) -> anyhow::Result<bool> {
    use wax::Program as _;

    let Some(tail) = strip_literal_prefix(rel, &compiled.prefix) else {
        return Ok(false);
    };
    Ok(match &compiled.glob {
        GlobMatcher::Literal => tail.as_os_str().is_empty(),
        GlobMatcher::Whole(glob) => glob.is_match(rel),
        GlobMatcher::Remainder(glob) if tail.as_os_str().is_empty() => {
            // `rel` *is* the prefix, which only `walk_glob`'s file-root branch
            // asks: the walk root names a file and the pattern continues below
            // it (`go.mod/**`). Whether that matches is wax's call on the whole
            // pattern — `pkg/**` matches `pkg`, `pkg/*` does not — and the
            // remainder alone answers differently (`*` matches an empty path).
            // Rare enough to compile the whole pattern for.
            cached_glob(&compiled.pattern)
                .with_context(|| format!("invalid glob pattern '{}'", compiled.pattern))?
                .is_match(rel)
        }
        GlobMatcher::Remainder(glob) => glob.is_match(tail),
    })
}

/// `rel` past the literal `prefix`, or `None` when `rel` does not lie under it.
///
/// Byte-wise, with the boundary checked by hand, so a near-miss (`pkg` against
/// `pkgx/f`) is a miss and an empty prefix strips nothing. Not
/// `Path::strip_prefix`: that parses components on both sides, which costs more
/// per walked entry than the regex match it guards (measured 2x the whole
/// match). Byte-wise is exact here because `rel` is the walk root (`prefix`
/// itself) followed by directory entry names, and wax matches the same raw
/// bytes. A prefix with a trailing `/` (`heph.fs.glob("sub/")`) is always a
/// fully-literal pattern, so the walk only asks about `rel == prefix`.
fn strip_literal_prefix<'r>(rel: &'r std::path::Path, prefix: &str) -> Option<&'r std::path::Path> {
    use std::os::unix::ffi::OsStrExt as _;

    if prefix.is_empty() {
        return Some(rel);
    }
    let rest = rel.as_os_str().as_bytes().strip_prefix(prefix.as_bytes())?;
    let tail = match rest {
        [] => rest,
        [b'/', tail @ ..] => tail,
        _ => return None,
    };
    Some(std::path::Path::new(std::ffi::OsStr::from_bytes(tail)))
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
        /// The lenient `copy_output` target rather than strict `file`.
        copy_output: bool,
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
    // Validity is the whole pattern's, as wax judges it — not the remainder's.
    // The literal prefix is cut on heph's metacharacter set, which is not wax's
    // (`a(b`, `a:b` and `$` are wax syntax that `literal_prefix` treats as
    // literal), and a cut can also remove the left neighbour a wax rule checks
    // (`pkg/{**/a,b}` is rejected, `{**/a,b}` is not). Parsing the whole without
    // compiling it keeps the accepted set exactly what it was, at a fraction of
    // a compile.
    hwalk::check_glob(pattern).with_context(|| format!("invalid glob pattern '{pattern}'"))?;

    let (prefix, remainder) = split_literal_prefix(pattern);
    // Only the part past the literal prefix needs a regex; see `glob_cache`.
    let glob = match remainder {
        "" => GlobMatcher::Literal,
        remainder => match cached_glob(remainder) {
            Ok(glob) => GlobMatcher::Remainder(glob),
            // Not a verdict on the pattern: the whole decides, and its error
            // is the one the user sees.
            Err(_) => GlobMatcher::Whole(
                cached_glob(pattern)
                    .with_context(|| format!("invalid glob pattern '{pattern}'"))?,
            ),
        },
    };

    // Compiles the engine's ignore set on first use. Every fs walk (the driver's
    // and `heph.fs.glob`'s) gets its `CompiledGlob` here, so a set that does not
    // compile fails the glob instead of `walk_glob` pruning nothing by glob.
    let ignore_any = skip
        .file_matcher()
        .with_context(|| format!("expanding glob '{pattern}'"))?;

    // No user excludes: reuse the ignore's prebuilt file matcher directly.
    let not = if exclude.is_empty() {
        ignore_any.clone()
    } else {
        cached_exclude_any(skip, request_id, exclude)?
    };

    Ok(CompiledGlob {
        pattern: pattern.to_owned(),
        glob,
        not,
        prefix: prefix.to_owned(),
        skip: skip.clone(),
    })
}

/// Walks `root` for files matching `compiled`, returning their artifacts.
///
/// Recursion runs through the shared [`CachedWalker`]: every `readdir` and every
/// file content hash is served from the cross-run fswalk cache when the tree is
/// unchanged. Filtering (glob match, excludes, skip-dir pruning, generated
/// files) is applied here — the walker itself is consumer-agnostic. Starts at
/// the pattern's literal prefix so a rooted pattern (`a/b/**/*`) scans only
/// `<root>/a/b`, not the whole tree.
///
/// Generated files: one codegen index snapshot per walk, asked per matched
/// file, with the section's territory carried down the walk like
/// `dir_in_heph`; `Generated` and `Modified` are left out and returned beside
/// the artifacts for the caller to report. After the walk, a commit since the
/// snapshot is rechecked against the kept paths.
fn walk_glob(
    walker: &CachedWalker,
    root: &std::path::Path,
    compiled: &CompiledGlob,
    codegen: &FsCodegen,
) -> anyhow::Result<(Vec<OutputArtifact>, Vec<codegen::Excluded>)> {
    let walk_root = if compiled.prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&compiled.prefix)
    };

    let lookup = codegen::GlobLookup::open(codegen, root)?;
    let lookup = lookup.as_ref();
    let mut excluded = vec![];
    let mut artifacts = vec![];
    // The literal prefix can name a file directly (a fully-literal pattern), in
    // which case the walk root is that single file, not a directory to descend.
    match std::fs::metadata(&walk_root) {
        Ok(m) if m.is_dir() => {
            // "Does this path resolve inside a `.heph*` dir" is carried down the
            // walk instead of being re-derived per entry. `canonicalize` resolves
            // every component, so asking it per file re-walks the same ancestor
            // chain once per entry; carried, the whole walk pays one call for the
            // root plus one per symlink it meets.
            //
            // Sound because `read_dir` reports a symlink as `Symlink`, never as
            // `Dir` — so every directory pushed here is a real one, and for a real
            // directory `canonicalize(dir/name)` is `canonicalize(dir).join(name)`.
            // Its components are therefore the parent's plus `name`, which is
            // exactly what the two operands below test.
            let root_in_heph = resolves_into_heph_dir(&walk_root);
            // Section territory is carried the same way: a directory a line
            // covers makes every entry under it territory without a match.
            let root_territory = lookup.is_some_and(|l| l.start_territory(&walk_root));
            let mut stack = vec![(walk_root, root_in_heph, root_territory)];
            while let Some((dir, dir_in_heph, dir_territory)) = stack.pop() {
                let listing = walker.read_dir(&dir)?;
                for entry in &listing.entries {
                    let abs = dir.join(&entry.name);
                    if entry.kind == hwalk::EntryKind::Dir {
                        // Never descend into skip dirs, skip-glob subtrees, or
                        // `.heph*` dirs (`prune_dir` prunes those by name), so a
                        // real directory pushed here is in heph only if its
                        // parent was.
                        let rel = abs.strip_prefix(root).unwrap_or(&abs);
                        if !compiled.skip.prune_dir(&abs, rel) {
                            let territory =
                                lookup.is_some_and(|l| l.dir_territory(&abs, dir_territory));
                            stack.push((abs, dir_in_heph, territory));
                        }
                    } else if matches!(
                        entry.kind,
                        hwalk::EntryKind::File | hwalk::EntryKind::Symlink
                    ) {
                        // A symlink-to-dir is rejected by `file_hash` (which follows
                        // and errors on a dir), matching the old walk.
                        emit_glob_file(
                            walker,
                            root,
                            compiled,
                            &abs,
                            entry.kind == hwalk::EntryKind::Symlink,
                            dir_in_heph,
                            lookup.map(|l| (l, Some(dir_territory))),
                            &mut artifacts,
                            &mut excluded,
                        )?;
                    }
                }
            }
        }
        // A fully-literal pattern names one path with no walk above it, so there
        // is no parent answer to carry: resolve it in full.
        Ok(_) => emit_glob_file(
            walker,
            root,
            compiled,
            &walk_root,
            true,
            false,
            lookup.map(|l| (l, None)),
            &mut artifacts,
            &mut excluded,
        )?,
        Err(_) => {} // missing walk root ⇒ empty match
    }

    if let Some(l) = lookup {
        l.recheck(&mut artifacts, &mut excluded)?;
    }
    Ok((artifacts, excluded))
}

/// If `abs` matches the glob (and isn't excluded, generated or engine
/// internal), hash it via the walker and push its artifact onto `out`; a
/// generated file goes onto `excluded` instead. `codegen` is the walk's index
/// lookup and the territory of `abs`'s directory (`None`: a lone path).
#[expect(
    clippy::too_many_arguments,
    reason = "one walk step's state, threaded from the loop above"
)]
fn emit_glob_file(
    walker: &CachedWalker,
    root: &std::path::Path,
    compiled: &CompiledGlob,
    abs: &std::path::Path,
    is_symlink: bool,
    parent_in_heph: bool,
    codegen: Option<(&codegen::GlobLookup<'_>, Option<bool>)>,
    out: &mut Vec<OutputArtifact>,
    excluded: &mut Vec<codegen::Excluded>,
) -> anyhow::Result<()> {
    use wax::Program as _;

    let Ok(rel) = abs.strip_prefix(root) else {
        return Ok(());
    };
    // Include on a pattern match, drop on an exclude match. Excludes are
    // root-relative patterns (`vendor/**`), so they see the whole `rel`.
    if !pattern_matches(compiled, rel)? || compiled.not.is_match(rel) {
        return Ok(());
    }
    // Paths resolving into a `.heph*` dir are engine-internal artifacts, not
    // raw source.
    let name = abs.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if entry_resolves_into_heph_dir(abs, name, is_symlink, parent_in_heph) {
        return Ok(());
    }
    let Some(rel_str) = rel.to_str() else {
        anyhow::bail!("glob entry path is not valid UTF-8: {}", rel.display());
    };
    // A generated file is sourced from its generator only: reading it here
    // would double-source the generated content.
    if let Some((lookup, parent_territory)) = codegen
        && let Some(ex) = lookup.exclusion(abs, rel_str, parent_territory)?
    {
        excluded.push(ex);
        return Ok(());
    }
    // Hash via the walker (follows symlinks). A symlink-to-dir or a
    // dangling/vanished link errors here → skip.
    let fh = match walker.file_hash(abs) {
        Ok(fh) => fh,
        Err(_) => return Ok(()),
    };
    let Some(source_path) = abs.to_str().map(str::to_owned) else {
        anyhow::bail!("glob entry path is not valid UTF-8: {}", abs.display());
    };
    // `out_path` is root-relative and `/`-separated on every supported (Unix)
    // target, so the bound hashout is the same on Linux and macOS.
    let out_path = rel_str.to_string();
    // Path-bound, not the bare content hash: the glob addr carries only the
    // pattern, so this is the only place the entry's path reaches a
    // consumer's `hashin` (see `hwalk::fs_entry_hashout`).
    let hashout = hwalk::fs_entry_hashout(&out_path, &fh.hashout);
    out.push(OutputArtifact {
        group: String::new(),
        name: rel_str.replace('/', "_"),
        r#type: Type::Output,
        content: Content::File(ContentFile {
            source_path,
            out_path,
            x: fh.exec,
            // Workspace source file: durable for the build, so the engine can
            // reference it by path instead of tar-packing it into the cache.
            passthrough: true,
        }),
        hashout,
    });
    Ok(())
}

/// The per-request memo key of a glob walk over `(root, pattern, exclude)`.
fn glob_walk_key(root: &std::path::Path, pattern: &str, exclude: &[String]) -> String {
    let mut sorted: Vec<&str> = exclude.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    // `root` is part of the key: the same pattern walked under a different tree
    // root yields different artifacts (and source paths).
    format!("{}\0{}\0{}", root.display(), pattern, sorted.join("\u{1}"))
}

/// The glob walk artifacts memoized under `key` within `request_id` (the tree
/// is immutable mid-request, so repeat calls reuse the assembled list).
/// Cross-run reuse of readdir + file hashes happens inside the shared
/// [`CachedWalker`].
fn memoized_glob_walk(request_id: &str, key: &str) -> Option<Arc<Vec<OutputArtifact>>> {
    glob_result_cache()
        .read()
        .get(request_id)
        .and_then(|m| m.get(key))
        .cloned()
}

/// Memoizes `artifacts` under `key` within `request_id`. A concurrent walk of
/// the same key that landed first wins, so every caller sees one list.
fn memoize_glob_walk(
    request_id: &str,
    key: String,
    artifacts: Vec<OutputArtifact>,
) -> Arc<Vec<OutputArtifact>> {
    glob_result_cache()
        .write()
        .entry(request_id.to_owned())
        .or_default()
        .entry(key)
        .or_insert_with(|| Arc::new(artifacts))
        .clone()
}

/// The single artifact of the fs `file` / `copy_output` target over `path`
/// under `root`, or none for a path into a `.heph*` dir. Synchronous: one
/// codegen index lookup, a stat and, on a walker miss, a full read and hash.
///
/// A generated path is never re-read as raw source: `file` fails for one heph
/// recorded ([`CodegenFileError`]) and yields nothing for an unrecorded
/// section match, with a warning; `copy_output` yields nothing, silently.
fn file_artifacts(
    walker: &CachedWalker,
    root: &std::path::Path,
    path: &str,
    codegen: (&FsCodegen, codegen::FileMode),
    request_id: &str,
) -> anyhow::Result<(Vec<OutputArtifact>, Option<codegen::FileWarning>)> {
    let abs = root.join(path);

    // A path resolving into a `.heph*` dir (engine-internal artifact)
    // resolves to nothing.
    if resolves_into_heph_dir(&abs) {
        return Ok((vec![], None));
    }
    let (cg, mode) = codegen;
    if let codegen::FileSource::Excluded { warning } =
        codegen::file_is_source(cg, root, path, mode, Some(request_id))?
    {
        return Ok((vec![], warning));
    }

    // Content hash + exec bit via the shared walker (cross-run cached).
    let fh = walker
        .file_hash(&abs)
        .with_context(|| format!("hash file '{}'", abs.display()))?;

    let source_path = abs
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("file path is not valid UTF-8: {}", abs.display()))?
        .to_string();

    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string();

    // Bound to the path exactly as a glob entry is, so this file
    // has one hashout whether it was sourced by `file()` or by a
    // `glob()` that matched it (see `hwalk::fs_entry_hashout`).
    let hashout = hwalk::fs_entry_hashout(path, &fh.hashout);

    let artifact = OutputArtifact {
        group: "".to_string(),
        name,
        r#type: Type::Output,
        content: Content::File(ContentFile {
            source_path,
            out_path: path.to_string(),
            x: fh.exec,
            // Workspace source file: durable for the build, so the
            // engine references it by path instead of tar-packing it.
            passthrough: true,
        }),
        hashout,
    };
    Ok((vec![artifact], None))
}

pub struct Driver {
    /// Engine-owned + built-in dirs pruned during glob walks.
    skip: Arc<Ignore>,
    /// Shared cross-run filesystem-walk cache (readdir + file hashes).
    walker: Arc<CachedWalker>,
    /// The codegen index the globs and file targets read, and where they
    /// report what it excluded.
    codegen: FsCodegen,
}

impl Default for Driver {
    fn default() -> Self {
        Self {
            skip: Arc::default(),
            walker: Arc::new(CachedWalker::disabled()),
            codegen: FsCodegen::default(),
        }
    }
}

impl Driver {
    pub fn new(skip: Arc<Ignore>, walker: Arc<CachedWalker>) -> Self {
        Self {
            skip,
            walker,
            codegen: FsCodegen::default(),
        }
    }

    /// Read generated files through `codegen` (without it, nothing is).
    #[must_use]
    pub fn with_codegen(mut self, codegen: FsCodegen) -> Self {
        self.codegen = codegen;
        self
    }
}

/// Config for an `fs` target: exactly the `f` / `p` / `e` keys the fs provider
/// emits. `#[derive(Spec)]` provides the parser and the LSP schema. `f` takes
/// precedence over `p`; with neither, `parse` errors.
#[derive(Spec)]
struct FsSpec {
    /// File path to expose as the single output.
    #[spec(ty = ParamType::String)]
    f: Option<String>,
    /// Glob pattern selecting outputs.
    #[spec(ty = ParamType::String)]
    p: Option<String>,
    /// Comma-separated glob patterns to exclude (glob mode only).
    #[spec(ty = ParamType::String)]
    e: Option<String>,
    /// What the target reserves from the resource pools while it runs.
    /// Defaults to 0 cpu: it reserves nothing. Never hashed.
    resources: hplugin::driver::Resources,
}

#[async_trait]
impl hplugin::driver::Driver for Driver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: DRIVER_NAME.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        FsSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let spec = FsSpec::from(&req.target_spec.config).context("parse fs config")?;

        let file_path = spec
            .f
            .map(|p| normalize_path(&p))
            .transpose()
            .context("normalizing fs file path")?;
        let glob_pattern = spec
            .p
            .map(|p| normalize_path(&p))
            .transpose()
            .context("normalizing fs glob pattern")?;
        let exclude_raw = spec.e.unwrap_or_default();

        // "e" arg is comma-separated glob patterns.
        let exclude: Vec<String> = if exclude_raw.is_empty() {
            vec![]
        } else {
            exclude_raw.split(',').map(str::to_owned).collect()
        };

        // `copy_output` is `file` that never fails on a generated path; its own
        // tag keeps every strict `file` key unchanged (Decision 25).
        let copy_output = req.target_spec.addr.name == COPY_OUTPUT;
        let (def, outputs, hash) = match (file_path, glob_pattern) {
            (Some(path), _) => {
                let mut h = Xxh3::new();
                h.update(if copy_output {
                    b"fs_copy_output".as_slice()
                } else {
                    b"fs_file".as_slice()
                });
                h.update(path.as_bytes());
                let hash = format!("{:x}", h.digest()).into_bytes();

                let def = FsDef::File {
                    path: path.clone(),
                    copy_output,
                };
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
                outputs,
                hash,
                // Not in `hash` above: a declaration moves no key.
                resources: spec
                    .resources
                    .advertise(
                        hplugin::provider::PoolName::cpu(),
                        hplugin::driver::Resources::VIEW_CPU,
                    )
                    .context("fs resources")?,
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
        req: RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<RunResponse> {
        let def = req.target.def::<FsDef>();
        let root = &req.tree_root_path;

        // The stat, `read_dir` and hash below are synchronous filesystem work:
        // run inline, each would park a runtime worker for its whole duration,
        // and fs targets reserve no cpu, so enough concurrent ones would park
        // every worker and stall the reactor (#180). They run on the bounded
        // blocking pool instead. Neither the walker (parking_lot mutexes, an
        // r2d2 sqlite pool) nor the walk awaits or waits on another
        // `hcore::blocking` job, so nothing here nests.
        let root = root.to_path_buf();
        match def {
            FsDef::File { path, copy_output } => {
                let path = path.clone();
                let mode = if *copy_output {
                    codegen::FileMode::CopyOutput
                } else {
                    codegen::FileMode::Strict
                };
                let request_id = req.request_id.clone();
                let (artifacts, warning) = hcore::blocking::run(
                    enclose::enclose!((self.walker => walker, self.codegen => cg) move || {
                        file_artifacts(&walker, &root, &path, (&cg, mode), &request_id)
                    }),
                )
                .await?;
                if let Some(warning) = warning {
                    warning.log();
                }

                Ok(RunResponse {
                    artifacts,
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
                // skip even the walker round-trips, and the hop to the pool.
                let key = glob_walk_key(&root, pattern, exclude);
                let artifacts = match memoized_glob_walk(req.request_id, &key) {
                    Some(artifacts) => artifacts,
                    None => {
                        let (walked, excluded) = hcore::blocking::run(
                            enclose::enclose!((self.walker => walker, compiled, self.codegen => cg) move || {
                                walk_glob(&walker, &root, &compiled, &cg)
                            }),
                        )
                        .await?;
                        codegen::report(&self.codegen, Some(req.request_id), &excluded);
                        if !excluded.is_empty() {
                            tracing::debug!(
                                pattern = %pattern,
                                "{}",
                                exclusion_summary(&excluded)
                            );
                        }
                        memoize_glob_walk(req.request_id, key, walked)
                    }
                };

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
    use hcore::hasync::StdCancellationToken;
    use hmodel::htaddr::parse_addr;
    use hplugin::driver::{Driver as EDriver, RunRequest};
    use hplugin::provider::Provider as EProvider;
    use hwalk::codegen_index::CodegenIndex;
    use hwalk::file_hashout;
    use std::fs;
    use tempfile::tempdir;

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    struct NoopExecutor;
    impl hplugin::provider::ProviderExecutor for NoopExecutor {
        fn result<'a>(
            &'a self,
            _addr: &'a hmodel::htaddr::Addr,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<std::sync::Arc<hplugin::eresult::EResult>>>
        {
            Box::pin(async { anyhow::bail!("noop") })
        }

        fn query<'a>(
            &'a self,
            _m: &'a hmodel::htmatcher::Matcher,
            _extra_skip: &'a [String],
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<hmodel::htaddr::Addr>>> {
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
        let v = futures::executor::block_on(
            GlobFn {
                skip,
                walker: Arc::new(CachedWalker::disabled()),
                codegen: FsCodegen::default(),
            }
            .call(&ctx, args),
        )
        .unwrap()
        .into_value_only()
        .unwrap();
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

    fn call_path_fn(f: &dyn PluginFn, args: Vec<&str>) -> String {
        let root = std::path::Path::new("/");
        let ctx = FnCallContext { pkg: "", root };
        let args = FnArgs {
            positional: args
                .into_iter()
                .map(|s| Value::String(s.to_string()))
                .collect(),
            named: Default::default(),
        };
        match futures::executor::block_on(f.call(&ctx, args))
            .unwrap()
            .into_value_only()
            .unwrap()
        {
            Value::String(s) => s,
            other => panic!("expected string, got {other:?}"),
        }
    }

    /// `join` is variadic — each element is its own string positional.
    fn call_join(elems: Vec<&str>) -> String {
        let root = std::path::Path::new("/");
        let ctx = FnCallContext { pkg: "", root };
        let args = FnArgs {
            positional: elems
                .into_iter()
                .map(|s| Value::String(s.to_string()))
                .collect(),
            named: Default::default(),
        };
        match futures::executor::block_on(JoinFn.call(&ctx, args))
            .unwrap()
            .into_value_only()
            .unwrap()
        {
            Value::String(s) => s,
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn test_join_fn() {
        assert_eq!(call_join(vec!["a", "b", "c"]), "a/b/c");
        // Cleans embedded `.`/`..` and redundant separators.
        assert_eq!(call_join(vec!["a/", "b"]), "a/b");
        assert_eq!(call_join(vec!["a", "..", "b"]), "b");
        assert_eq!(call_join(vec!["a", ".", "b"]), "a/b");
        // Empty elements are skipped; all-empty yields "".
        assert_eq!(call_join(vec!["", "a", ""]), "a");
        assert_eq!(call_join(vec!["", ""]), "");
        assert_eq!(call_join(vec![]), "");
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

    // ─── Parent (find-up) tests ────────────────────────────────────────────

    /// Call `heph.fs.parent(filename)` from `pkg` rooted at `root`; `Null` → None.
    fn call_parent(root: &std::path::Path, pkg: &str, filename: &str) -> Option<String> {
        let ctx = FnCallContext { pkg, root };
        let args = FnArgs {
            positional: vec![Value::String(filename.to_string())],
            named: Default::default(),
        };
        match futures::executor::block_on(ParentFn.call(&ctx, args))
            .unwrap()
            .into_value_only()
            .unwrap()
        {
            Value::String(s) => Some(s),
            Value::Null() => None,
            other => panic!("expected string or null, got {other:?}"),
        }
    }

    #[test]
    fn test_parent_finds_in_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("go.mod"), "").unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        // From a/b, the closest go.mod is at the workspace root.
        assert_eq!(
            call_parent(root, "a/b", "go.mod").as_deref(),
            Some("go.mod")
        );
    }

    #[test]
    fn test_parent_prefers_closest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("go.mod"), "").unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/go.mod"), "").unwrap();
        // The nearer `a/go.mod` wins over the root one.
        assert_eq!(
            call_parent(root, "a/b", "go.mod").as_deref(),
            Some("a/go.mod")
        );
    }

    #[test]
    fn test_parent_checks_starting_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/go.mod"), "").unwrap();
        // The package's own directory is searched first.
        assert_eq!(
            call_parent(root, "a/b", "go.mod").as_deref(),
            Some("a/b/go.mod")
        );
    }

    #[test]
    fn test_parent_not_found_is_null() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        assert_eq!(call_parent(root, "a/b", "go.mod"), None);
    }

    #[test]
    fn test_parent_does_not_escape_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A file OUTSIDE the workspace root must never be returned.
        fs::write(root.join("outside.txt"), "").unwrap();
        let inner = root.join("ws");
        fs::create_dir_all(inner.join("a")).unwrap();
        assert_eq!(call_parent(&inner, "a", "outside.txt"), None);
    }

    #[test]
    fn test_parent_rejects_nested_path() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = FnCallContext {
            pkg: "",
            root: tmp.path(),
        };
        let args = FnArgs {
            positional: vec![Value::String("a/b".to_string())],
            named: Default::default(),
        };
        let err = futures::executor::block_on(ParentFn.call(&ctx, args)).unwrap_err();
        assert!(err.to_string().contains("bare file name"), "{err}");
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
            Some(&Value::String("foo.txt".to_string()))
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
                ..Default::default()
            }),
        }
    }

    fn make_run_req<'a>(
        target: &'a TargetDef,
        request_id: &'a String,
        root: std::path::PathBuf,
        hashin: &'a str,
    ) -> RunRequest<'a, 'static> {
        let mut req = RunRequest::new(request_id, target, root.clone());
        req.tree_root_path = root;
        req.hashin = hashin;
        req
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

    /// An `fs.skip` set that parses but exceeds the regex size limit fails every
    /// fs walk — the driver's glob, with and without user excludes, and
    /// `heph.fs.glob` — naming the limit, rather than walking with nothing
    /// pruned by glob.
    #[tokio::test]
    async fn test_an_uncompilable_skip_set_fails_the_glob() {
        let tmp = tempdir().unwrap();
        let skip = Arc::new(Ignore::new(&[], &["*a".repeat(20_000)]).expect("it parses"));
        let driver = Driver::new(Arc::clone(&skip), Arc::new(CachedWalker::disabled()));
        for exclude in [None, Some("x/**")] {
            let mut config =
                std::collections::HashMap::from([("p".to_string(), Value::String("**/*".into()))]);
            if let Some(e) = exclude {
                config.insert("e".to_string(), Value::String(e.into()));
            }
            let Err(err) = driver.parse(make_parse_req(config), &ctoken()).await else {
                panic!("exclude {exclude:?}: expected the glob to fail")
            };
            let msg = format!("{err:#}");
            assert!(msg.contains("expanding glob '**/*'"), "{msg}");
            assert!(msg.contains("regex size limit"), "{msg}");
        }

        let ctx = FnCallContext {
            pkg: "",
            root: tmp.path(),
        };
        let args = FnArgs {
            positional: vec![Value::String("**/*".to_string())],
            named: Default::default(),
        };
        let walker = Arc::new(CachedWalker::disabled());
        let Err(err) = GlobFn {
            skip,
            walker,
            codegen: FsCodegen::default(),
        }
        .call(&ctx, args)
        .await
        else {
            panic!("heph.fs.glob: expected the glob to fail")
        };
        assert!(format!("{err:#}").contains("regex size limit"), "{err:#}");
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

    /// An fs target that declares no `cpu` advertises 0 CPU (D3); a
    /// declaration replaces that `cpu`, keeps its other pools, and moves no
    /// def hash (U5).
    #[tokio::test]
    async fn resources_default_light_and_declared_unhashed() {
        let parse = |extra: Option<Value>| async move {
            let mut config = std::collections::HashMap::from([(
                "f".to_string(),
                Value::String("src/main.rs".to_string()),
            )]);
            if let Some(r) = extra {
                config.insert("resources".to_string(), r);
            }
            Driver::default()
                .parse(make_parse_req(config), &ctoken())
                .await
                .unwrap()
                .target_def
        };
        let plain = parse(None).await;
        assert_eq!(plain.resources.declared("cpu"), Some("0"));
        assert_eq!(plain.resources.len(), 1);
        let gpu_only = parse(Some(Value::Map(
            [("gpu".to_string(), Value::Int(1))].into(),
        )))
        .await;
        assert_eq!(gpu_only.resources.declared("cpu"), Some("0"));
        assert_eq!(gpu_only.resources.declared("gpu"), Some("1"));
        let heavy = parse(Some(Value::Map(
            [("cpu".to_string(), Value::Int(2))].into(),
        )))
        .await;
        assert_eq!(heavy.resources.declared("cpu"), Some("2"));
        assert_eq!(heavy.hash, plain.hash);
        assert_eq!(gpu_only.hash, plain.hash);
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
        assert!(matches!(
            def,
            FsDef::File { path, copy_output: false } if path == "src/main.rs"
        ));
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

    /// One file has one hashout however it was sourced: `fs:file` binds its
    /// path into the hashout exactly as a glob entry does, so `file("pkg/a.go")`
    /// and a `glob()` matching only that file emit the same hashout — and it is
    /// the path-bound form, not the bare content hash, so the same bytes at
    /// another path hash differently.
    #[tokio::test]
    async fn test_file_and_single_file_glob_share_a_hashout() {
        let driver = Driver::default();
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("pkg")).unwrap();
        fs::write(tmp.path().join("pkg/a.go"), b"package a\n").unwrap();
        fs::write(tmp.path().join("pkg/b.go"), b"package a\n").unwrap();

        let run = |key: &'static str, val: &'static str| {
            let config = std::collections::HashMap::from([(
                key.to_string(),
                Value::String(val.to_string()),
            )]);
            let driver = &driver;
            let root = tmp.path().to_path_buf();
            async move {
                let parse_res = driver
                    .parse(make_parse_req(config), &ctoken())
                    .await
                    .unwrap();
                let request_id = "test".to_string();
                let hashin = String::new();
                let req = make_run_req(&parse_res.target_def, &request_id, root, &hashin);
                let res = driver.run(req, &ctoken()).await.unwrap();
                assert_eq!(res.artifacts.len(), 1, "{key}={val}");
                res.artifacts[0].hashout.clone()
            }
        };

        let file = run("f", "pkg/a.go").await;
        assert_eq!(file, run("p", "pkg/a.go").await, "glob of the same file");
        assert_eq!(file, run("p", "pkg/a.*").await, "glob matching only it");

        let content = file_hashout(&tmp.path().join("pkg/a.go"), false).unwrap();
        assert_eq!(file, hwalk::fs_entry_hashout("pkg/a.go", &content));
        assert_ne!(file, content, "not the bare content hash");
        assert_ne!(
            file,
            run("f", "pkg/b.go").await,
            "same bytes at another path"
        );
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

    /// `walk_glob` matches the right files (recursively), hashes them, and skips a
    /// file the codegen index recorded. Cross-run cache mechanics are tested in
    /// `htwalk::cached_walker`.
    #[test]
    fn test_walk_glob_matches_and_skips_codegen() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("a.rs"), b"aaa").unwrap();
        fs::write(root.join("sub").join("b.rs"), b"bbbb").unwrap();
        fs::write(root.join("c.txt"), b"ignored").unwrap();

        let skip = Arc::new(Ignore::new(&[], &[]).unwrap());
        let compiled = compile_glob(&skip, "t", "**/*.rs", &[]).unwrap();
        let walker = CachedWalker::disabled();

        let (arts, excluded) = walk_glob(&walker, root, &compiled, &FsCodegen::default()).unwrap();
        let mut names: Vec<_> = arts.iter().map(|a| a.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["a.rs".to_string(), "sub_b.rs".to_string()]);
        assert!(arts.iter().all(|a| !a.hashout.is_empty()));
        assert!(excluded.is_empty());

        // A file the index recorded drops out of a re-walk, and is returned
        // beside the artifacts for the caller to report.
        let index = index_recording(root, "sub/b.rs", "//gen:it");
        let cg = FsCodegen::new(index, Arc::new(NoReport));
        let (arts, excluded) = walk_glob(&walker, root, &compiled, &cg).unwrap();
        let names: Vec<_> = arts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a.rs"], "recorded sub/b.rs is excluded");
        let excluded: Vec<_> = excluded.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(excluded, vec!["sub/b.rs"]);
    }

    /// End of walk (codegen index spec, Snapshot): another writer commits a
    /// pending row for `b.rs` while the glob walks. The walk classified `b.rs`
    /// from its snapshot, taken before that commit, so it kept it as `Source`;
    /// the end-of-walk recheck must move it to excluded.
    ///
    /// The commit is injected from the index's `lstat` of the recorded `a.rs`,
    /// which only the walk's own lookup of `a.rs` makes: after the snapshot,
    /// before the recheck, whichever of `a.rs`/`b.rs` the walk meets first.
    #[test]
    fn glob_reclassifies_when_generation_moves_mid_walk() {
        use hwalk::codegen_index::{
            FileStat, IndexFs, RealFs, Row, RowKind, StatusKind, StatusReason, Store, StoreOptions,
            db_path,
        };
        use std::sync::atomic::{AtomicBool, Ordering};

        struct CommitOnLstat {
            inner: RealFs,
            db: std::path::PathBuf,
            armed: AtomicBool,
            fired: AtomicBool,
        }
        impl IndexFs for CommitOnLstat {
            fn lstat(&self, path: &std::path::Path) -> std::io::Result<FileStat> {
                if self.armed.load(Ordering::SeqCst)
                    && path.ends_with("a.rs")
                    && !self.fired.swap(true, Ordering::SeqCst)
                {
                    // Another process's commit: its own store handle, not
                    // published to this process's index.
                    let store = Store::open(&self.db, &StoreOptions::default())
                        .map_err(std::io::Error::other)?;
                    let mut row = Row::new("b.rs", "//gen:other", RowKind::File);
                    row.pending_hash = Some("in-flight".into());
                    store
                        .write(|t| t.put(&row))
                        .map_err(std::io::Error::other)?;
                }
                self.inner.lstat(path)
            }
            fn read_link(&self, path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
                self.inner.read_link(path)
            }
            fn content_hash(&self, path: &std::path::Path) -> anyhow::Result<String> {
                self.inner.content_hash(path)
            }
            fn fresh_hash(&self, path: &std::path::Path, x: bool) -> anyhow::Result<String> {
                self.inner.fresh_hash(path, x)
            }
            fn unlink(&self, path: &std::path::Path) -> std::io::Result<()> {
                self.inner.unlink(path)
            }
        }

        let tmp = tempdir().unwrap();
        let root = tmp.path();
        for name in ["a.rs", "b.rs", "c.rs"] {
            fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        let walker = Arc::new(CachedWalker::disabled());
        let cfs = Arc::new(CommitOnLstat {
            inner: RealFs::new(Arc::clone(&walker)),
            db: db_path(root, &root.join(".heph")),
            armed: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        });
        let index = index_recording_over(root, "a.rs", "//gen:it", cfs.clone());
        let cg = FsCodegen::new(index, Arc::new(NoReport));
        let skip = Arc::new(Ignore::new(&[], &[]).unwrap());
        let compiled = compile_glob(&skip, "t", "*.rs", &[]).unwrap();

        cfs.armed.store(true, Ordering::SeqCst);
        let (arts, mut excluded) = walk_glob(&walker, root, &compiled, &cg).unwrap();
        assert!(
            cfs.fired.load(Ordering::SeqCst),
            "the walk's lookup of a.rs must have committed b.rs's row"
        );

        let names: Vec<_> = arts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["c.rs"], "b.rs went pending mid-walk");
        excluded.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            excluded.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["a.rs", "b.rs"]
        );
        let b = &excluded[1];
        assert_eq!(
            (b.kind, b.reason, b.addr.as_deref()),
            (
                StatusKind::Generated,
                Some(StatusReason::Pending),
                Some("//gen:other")
            )
        );
    }

    /// The walk carries "resolves into a `.heph*` dir" down from each directory
    /// rather than re-deriving it per entry, so a symlink pointing into the cache
    /// must still be caught: it is the one entry kind the carried answer cannot
    /// settle, and the whole check exists for it.
    #[cfg(unix)]
    #[test]
    fn test_walk_glob_skips_a_symlink_into_the_cache_beside_plain_files() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        // The cache lives outside the walked tree so the walk cannot reach it by
        // descent — only by following the link.
        let cache = tempdir().unwrap();
        let cache_dir = cache.path().join(".heph3");
        fs::create_dir(&cache_dir).unwrap();
        fs::write(cache_dir.join("generated.rs"), b"generated").unwrap();

        fs::create_dir(root.join("sub")).unwrap();
        // Plain files either side of the link: these take the carried answer.
        fs::write(root.join("sub").join("a.rs"), b"aaa").unwrap();
        fs::write(root.join("sub").join("z.rs"), b"zzz").unwrap();
        std::os::unix::fs::symlink(
            cache_dir.join("generated.rs"),
            root.join("sub").join("linked.rs"),
        )
        .unwrap();

        let skip = Arc::new(Ignore::new(&[], &[]).unwrap());
        let compiled = compile_glob(&skip, "t", "**/*.rs", &[]).unwrap();
        let walker = CachedWalker::disabled();

        let (arts, _) = walk_glob(&walker, root, &compiled, &FsCodegen::default()).unwrap();
        let mut names: Vec<_> = arts.iter().map(|a| a.name.clone()).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["sub_a.rs".to_string(), "sub_z.rs".to_string()],
            "a symlink resolving into a .heph dir must be excluded while its \
             plain siblings are kept"
        );
    }

    /// A directory literally named `.heph*` inside the walked tree excludes its
    /// contents too — the carried answer has to pick that up when the directory
    /// is pushed, not only at the walk root.
    #[test]
    fn test_walk_glob_skips_files_under_a_nested_heph_dir() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("keep.rs"), b"keep").unwrap();
        fs::create_dir(root.join("sub").join(".heph3")).unwrap();
        fs::write(root.join("sub").join(".heph3").join("drop.rs"), b"drop").unwrap();

        let skip = Arc::new(Ignore::new(&[], &[]).unwrap());
        let compiled = compile_glob(&skip, "t", "**/*.rs", &[]).unwrap();
        let walker = CachedWalker::disabled();

        let (arts, _) = walk_glob(&walker, root, &compiled, &FsCodegen::default()).unwrap();
        let names: Vec<_> = arts.iter().map(|a| a.name.clone()).collect();
        assert_eq!(
            names,
            vec!["sub_keep.rs".to_string()],
            "files under a nested .heph* dir must be excluded"
        );
    }

    /// A `.heph*` directory is pruned by name, not walked and filtered: listing it
    /// would fail here, because it is unreadable.
    #[cfg(unix)]
    #[test]
    fn test_walk_glob_does_not_descend_into_a_heph_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("keep.rs"), b"keep").unwrap();
        let heph = root.join(".heph-anything");
        fs::create_dir(&heph).unwrap();
        fs::write(heph.join("drop.rs"), b"drop").unwrap();
        fs::set_permissions(&heph, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&heph).is_ok() {
            // Running as root: permissions do not stop the read, so the test
            // cannot tell a pruned dir from a walked-and-filtered one.
            fs::set_permissions(&heph, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping: unreadable dirs are readable to this user");
            return;
        }

        let skip = Arc::new(Ignore::new(&[], &[]).unwrap());
        let compiled = compile_glob(&skip, "t", "**/*.rs", &[]).unwrap();
        let res = walk_glob(
            &CachedWalker::disabled(),
            root,
            &compiled,
            &FsCodegen::default(),
        );
        fs::set_permissions(&heph, fs::Permissions::from_mode(0o755)).unwrap();

        let names: Vec<_> = res
            .expect("the walk must not list the .heph* dir")
            .0
            .iter()
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(names, vec!["keep.rs".to_string()]);
    }

    /// Cross-run via the shared walker: a first driver populates the fswalk db; a
    /// fresh driver (new process) serves the unchanged tree from it, and re-walks
    /// after a file is added.
    #[tokio::test]
    async fn test_glob_cross_run_via_walker() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        // Keep the fswalk db out of the globbed tree so its writes don't bump the
        // walked dirs' mtimes (in production it lives under the pruned home).
        let dbdir = tempdir().unwrap();
        let db = dbdir.path().join("fswalk.db");
        fs::write(root.join("a.rs"), b"aaa").unwrap();
        fs::write(root.join("b.rs"), b"bbb").unwrap();

        let skip = Arc::new(Ignore::new(&[], &[]).unwrap());
        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("*.rs".to_string()))]);
        let hashin = String::new();
        let (id1, id2, id3) = ("r1".to_string(), "r2".to_string(), "r3".to_string());

        let parse_res = {
            let driver = Driver::new(skip.clone(), Arc::new(CachedWalker::open(&db)));
            let pr = driver
                .parse(make_parse_req(config), &ctoken())
                .await
                .unwrap();
            let first = driver
                .run(
                    make_run_req(&pr.target_def, &id1, root.to_path_buf(), &hashin),
                    &ctoken(),
                )
                .await
                .unwrap();
            assert_eq!(first.artifacts.len(), 2);
            pr
        };

        // A fresh walker reads the populated db (unchanged tree).
        let driver2 = Driver::new(skip.clone(), Arc::new(CachedWalker::open(&db)));
        let second = driver2
            .run(
                make_run_req(&parse_res.target_def, &id2, root.to_path_buf(), &hashin),
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(second.artifacts.len(), 2);

        // Add a file → dir mtime bumps → re-walk picks it up.
        fs::write(root.join("c.rs"), b"ccc").unwrap();
        std::fs::File::open(root)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(7200))
            .unwrap();
        let third = driver2
            .run(
                make_run_req(&parse_res.target_def, &id3, root.to_path_buf(), &hashin),
                &ctoken(),
            )
            .await
            .unwrap();
        assert_eq!(third.artifacts.len(), 3, "newly added c.rs is picked up");
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
        // Exec marker + content both come from the followed target (NOT the
        // symlink's own mode); the entry's hashout binds that content hash to
        // the link's own path.
        let real = file_hashout(&root.join("real.txt"), false).unwrap();
        assert_eq!(
            by_name["link.txt"],
            hwalk::fs_entry_hashout("link.txt", &real)
        );
        assert_eq!(
            by_name["real.txt"],
            hwalk::fs_entry_hashout("real.txt", &real)
        );
    }

    /// A glob entry's hashout is the only place its path reaches a consumer's
    /// `hashin` (the glob addr names just the pattern). It must move when a
    /// file is renamed with its bytes kept, and when two files swap contents;
    /// stay stable across runs over an unchanged tree; and not be the bare
    /// content hash.
    #[test]
    fn test_glob_entry_hashout_binds_the_path() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("pkg")).unwrap();
        fs::write(root.join("pkg/a.go"), b"package a\n").unwrap();
        fs::write(root.join("pkg/b.go"), b"package b\n").unwrap();

        let skip = Arc::new(Ignore::new(&[], &[]).unwrap());
        let compiled = compile_glob(&skip, "t", "pkg/*.go", &[]).unwrap();
        let walker = CachedWalker::disabled();
        let walk = || -> Vec<(String, String)> {
            let mut v: Vec<_> = walk_glob(&walker, root, &compiled, &FsCodegen::default())
                .unwrap()
                .0
                .into_iter()
                .map(|a| (a.name, a.hashout))
                .collect();
            v.sort();
            v
        };
        let sorted_hashouts = |v: &[(String, String)]| {
            let mut h: Vec<String> = v.iter().map(|(_, h)| h.clone()).collect();
            h.sort();
            h
        };

        let first = walk();
        assert_eq!(first, walk(), "stable across runs over an unchanged tree");
        let a_content = file_hashout(&root.join("pkg/a.go"), false).unwrap();
        assert_eq!(
            first[0],
            (
                "pkg_a.go".to_string(),
                hwalk::fs_entry_hashout("pkg/a.go", &a_content)
            )
        );
        assert_ne!(first[0].1, a_content, "not the bare content hash");

        // Rename, bytes kept: the content-hash multiset is unchanged, the
        // entry hashouts are not.
        fs::rename(root.join("pkg/a.go"), root.join("pkg/a_darwin.go")).unwrap();
        let renamed = walk();
        assert_ne!(sorted_hashouts(&first), sorted_hashouts(&renamed));

        // Swap two files' contents.
        fs::rename(root.join("pkg/a_darwin.go"), root.join("pkg/a.go")).unwrap();
        fs::write(root.join("pkg/a.go"), b"package b\n").unwrap();
        fs::write(root.join("pkg/b.go"), b"package a\n").unwrap();
        let swapped = walk();
        assert_ne!(sorted_hashouts(&first), sorted_hashouts(&swapped));
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
        // Deliberately not `.heph*`-prefixed: that prefix is skipped by name on
        // its own, and this asserts the skip dir the engine hands over is pruned.
        let home = tmp.path().join("engine-home");
        fs::create_dir_all(home.join("cache")).unwrap();
        fs::write(home.join("cache").join("blob"), b"").unwrap();
        fs::write(tmp.path().join("main.rs"), b"").unwrap();

        // The engine hands the fs plugin its skip dirs (the heph home); the walk
        // must prune that subtree.
        let skip = Arc::new(Ignore::new(&[home], &[]).unwrap());
        let driver = Driver::new(skip, Arc::new(CachedWalker::disabled()));

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
        let driver = Driver::new(skip, Arc::new(CachedWalker::disabled()));

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
        let driver = Driver::new(skip, Arc::new(CachedWalker::disabled()));

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
        let parsed = hmodel::htaddr::parse_addr(&formatted).unwrap();
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
        let parsed = hmodel::htaddr::parse_addr(&formatted).unwrap();
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
    fn test_compile_glob_shares_one_remainder_across_prefixes() {
        // The go provider roots one glob per package (`<pkg>/**/*`). Every
        // package must reuse the one compiled `**/*`, not build its own.
        let skip = Arc::new(Ignore::default());
        let a = compile_glob(&skip, "t", "go/svc/alpha/**/*", &[]).unwrap();
        let b = compile_glob(&skip, "t", "go/svc/beta/deeper/**/*", &[]).unwrap();
        let c = compile_glob(&skip, "t", "**/*", &[]).unwrap();
        assert_eq!(a.prefix, "go/svc/alpha");
        assert_eq!(b.prefix, "go/svc/beta/deeper");
        assert_eq!(c.prefix, "");
        let remainder = |c: &CompiledGlob| match &c.glob {
            GlobMatcher::Remainder(g) => Arc::clone(g),
            _ => panic!("{}: expected a compiled remainder", c.pattern),
        };
        let (a, b, c) = (remainder(&a), remainder(&b), remainder(&c));
        assert!(
            Arc::ptr_eq(&a, &b) && Arc::ptr_eq(&b, &c),
            "patterns differing only in their literal prefix must share one compiled remainder"
        );

        // A fully-literal pattern has no remainder and compiles nothing.
        let lit = compile_glob(&skip, "t", "go/svc/alpha/go.mod", &[]).unwrap();
        assert!(matches!(lit.glob, GlobMatcher::Literal));
        assert_eq!(lit.prefix, "go/svc/alpha/go.mod");

        // A remainder wax rejects in leading position falls back to the whole
        // pattern; the pattern itself is still accepted.
        let whole = compile_glob(&skip, "t", "pkg/(?i)**", &[]).unwrap();
        assert!(matches!(whole.glob, GlobMatcher::Whole(_)));
        assert_eq!(whole.prefix, "pkg");

        // A pattern that is invalid as a whole reports the whole pattern.
        let err = compile_glob(&skip, "t", "pkg/**/**", &[])
            .err()
            .expect("`pkg/**/**` is rejected by wax");
        assert!(
            format!("{err:#}").contains("invalid glob pattern 'pkg/**/**'"),
            "{err:#}"
        );
    }

    /// The one match the prefix split cannot decide alone: the walk root is a
    /// file and the pattern continues below it. wax's answer for the whole
    /// pattern is what counts — `go.mod/**` matches the file, `go.mod/*` does
    /// not — not the remainder's answer for an empty path.
    #[test]
    fn test_walk_glob_file_at_literal_prefix_matches_as_whole_pattern() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("go.mod"), b"module x").unwrap();
        let skip = Arc::new(Ignore::new(&[], &[]).unwrap());
        let walker = CachedWalker::disabled();

        let tree = compile_glob(&skip, "t", "go.mod/**", &[]).unwrap();
        let none = FsCodegen::default();
        let (arts, _) = walk_glob(&walker, root, &tree, &none).unwrap();
        assert_eq!(
            arts.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["go.mod"]
        );

        let star = compile_glob(&skip, "t", "go.mod/*", &[]).unwrap();
        assert!(walk_glob(&walker, root, &star, &none).unwrap().0.is_empty());
    }

    /// `pattern_matches` must answer exactly what wax answers for the whole
    /// pattern: the prefix split is an optimization, not a semantics change.
    #[test]
    fn test_pattern_matches_agrees_with_whole_pattern() {
        use wax::Program as _;

        let skip = Arc::new(Ignore::default());
        // (pattern, path): prefixes of every depth, `**` matching zero
        // components, near-miss prefixes, the directory the prefix itself
        // names, fully-literal patterns, brace/`?`/class/escape/flag
        // metacharacters at the cut, `.` in the prefix (a literal, not regex
        // any-char), non-ASCII.
        let cases: &[(&str, &str)] = &[
            ("pkg/**/*", "pkg/a.txt"),
            ("pkg/**/*", "pkg/a/b/c.txt"),
            ("pkg/**/*", "pkgx/a.txt"),
            ("pkg/**/*", "other/pkg/a.txt"),
            ("pkg/**/*", "pkg"),
            ("pkg/**", "pkg/a/b.txt"),
            ("pkg/**", "pkg"),
            ("pkg/*", "pkg"),
            ("pkg/*", "pkg/a"),
            ("pkg/sub/*.go", "pkg/sub/a.go"),
            ("pkg/sub/*.go", "pkg/sub/deep/a.go"),
            ("pkg/sub/*.go", "pkg/a.go"),
            ("**/*.rs", "a.rs"),
            ("**/*.rs", "x/y/a.rs"),
            ("**/*.rs", "a.go"),
            ("*.rs", "a.rs"),
            ("*.rs", "x/a.rs"),
            ("dir/ogen.{yml,yaml}", "dir/ogen.yaml"),
            ("dir/ogen.{yml,yaml}", "dir/ogen.json"),
            ("a/b?c/d", "a/bxc/d"),
            ("a/b?c/d", "a/bc/d"),
            ("a/[bc]/d", "a/b/d"),
            ("a/[bc]/d", "a/x/d"),
            ("mgmt/protos/x/go.mod", "mgmt/protos/x/go.mod"),
            ("mgmt/protos/x/go.mod", "mgmt/protos/x/go.mod2"),
            ("mgmt/protos/x/go.mod", "mgmt/protos/x"),
            ("lit/dir", "lit/dir/file"),
            ("p.q/**/*.txt", "p.q/a.txt"),
            ("p.q/**/*.txt", "pXq/a.txt"),
            ("café/**/*", "café/é.txt"),
            ("café/**/*", "cafe/é.txt"),
            ("a\\*b/*.txt", "a*b/c.txt"),
            ("a\\*b/*.txt", "axb/c.txt"),
            ("pkg/(?i)*.go", "pkg/A.GO"),
            ("pkg/(?i)*.go", "pkg/a.go"),
            ("pkg/(?i)*.go", "PKG/a.go"),
            // Remainders wax rejects in leading position: the whole decides.
            ("pkg/(?i)**", "pkg"),
            ("pkg/(?i)**", "pkg/A/b"),
            ("pkg/(?i)**/*.go", "pkg/x/A.GO"),
            ("pkg/(?i)**/*.go", "pkg/x/a.rs"),
        ];
        for (pattern, path) in cases {
            let whole = wax::Glob::new(pattern).unwrap();
            let compiled = compile_glob(&skip, "t", pattern, &[]).unwrap();
            let path = std::path::Path::new(path);
            assert_eq!(
                pattern_matches(&compiled, path).unwrap(),
                whole.is_match(path),
                "pattern {pattern:?} against {}",
                path.display()
            );
        }
    }

    #[test]
    fn test_compile_glob_no_exclude_reuses_builtin() {
        let skip = Arc::new(Ignore::default());
        let compiled = compile_glob(&skip, "req-test", "**/*.rs", &[]).unwrap();
        assert!(
            Arc::ptr_eq(&compiled.not, skip.file_matcher().unwrap()),
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

    #[test]
    fn test_strip_literal_prefix() {
        use std::path::Path;
        for (rel, prefix, tail) in [
            ("pkg/a/b.go", "pkg", Some("a/b.go")),
            ("pkg", "pkg", Some("")),
            ("pkgx/f", "pkg", None),
            ("pk", "pkg", None),
            ("other/pkg/f", "pkg", None),
            ("a/b/c", "a/b", Some("c")),
            ("a/bc", "a/b", None),
            ("x/y", "", Some("x/y")),
            ("café/é", "café", Some("é")),
            // A trailing-slash prefix (`heph.fs.glob("sub/")`, always fully
            // literal) never strips an entry below it.
            ("pkg/sub/f", "pkg/sub/", None),
        ] {
            assert_eq!(
                strip_literal_prefix(Path::new(rel), prefix),
                tail.map(Path::new),
                "{rel:?} past {prefix:?}"
            );
        }
    }

    #[test]
    fn test_split_literal_prefix() {
        for (pattern, prefix, remainder) in [
            ("mgmt/protos/x/**/*", "mgmt/protos/x", "**/*"),
            ("**/*.rs", "", "**/*.rs"),
            ("*.rs", "", "*.rs"),
            // Fully literal: nothing left to compile.
            ("mgmt/protos/x/go.mod", "mgmt/protos/x/go.mod", ""),
            ("dir/ogen.{yml,yaml}", "dir", "ogen.{yml,yaml}"),
            ("a/b?c/d", "a", "b?c/d"),
            // Only the one separating `/` is cut.
            ("pkg/sub/", "pkg/sub/", ""),
        ] {
            assert_eq!(
                split_literal_prefix(pattern),
                (prefix, remainder),
                "{pattern}"
            );
        }
    }

    /// `compile_glob` accepts exactly the patterns wax accepts. The prefix cut
    /// must not widen that set: wax syntax `literal_prefix` treats as literal
    /// (`(`, `)`, `:`, `$`), and wax rules that look left of the cut (a `**` or
    /// `/` opening a branch), are both judged on the whole pattern.
    #[test]
    fn test_compile_glob_accepts_exactly_what_wax_accepts() {
        let skip = Arc::new(Ignore::default());
        for pattern in [
            "pkg/**/*",
            "pkg/a.txt",
            "pkg/(?i)**",
            "pkg/(?i)*.go",
            "pkg/a$b/**",
            "pkg/$$/**",
            "a(b",
            "a(b/**",
            "a:b/*.go",
            "docs/x)y/**/*",
            "docs/Report (final).pdf",
            "third_party/foo(1)/**/*",
            "pkg/{**/a.go,b.go}",
            "pkg/{**/x,y/**}",
            "pkg/</a:1,>",
            "pkg/<**/a:1,>",
            "pkg/**/**",
            "(?i)**",
            "**/",
            "pkg/sub/",
            "{unclosed",
            "[z-a]",
        ] {
            assert_eq!(
                compile_glob(&skip, "t", pattern, &[]).is_ok(),
                wax::Glob::new(pattern).is_ok(),
                "{pattern:?}: accepted set differs from wax"
            );
        }
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

    // ─── Codegen-index exclusion ───────────────────────────────────────────

    /// What a [`CodegenReport`] was told, as `(request id, event)` pairs.
    #[derive(Default)]
    struct Recorder {
        events: parking_lot::Mutex<Vec<(Option<String>, hcore::events::BuildEventKind)>>,
    }

    impl CodegenReport for Recorder {
        fn emit(&self, request_id: Option<&str>, kind: hcore::events::BuildEventKind) {
            self.events
                .lock()
                .push((request_id.map(str::to_owned), kind));
        }

        fn first_in_run(&self, _request_id: Option<&str>, _key: &str) -> bool {
            true
        }
    }

    /// A real codegen index over `root` (its db under `root/.heph`) in which
    /// `rel`, already on disk, is recorded as written by `addr` — through the
    /// write protocol, as the engine's `copy` write-back records it.
    fn index_recording(root: &std::path::Path, rel: &str, addr: &str) -> Arc<CodegenIndex> {
        let walker = Arc::new(CachedWalker::disabled());
        let fs = Arc::new(hwalk::codegen_index::RealFs::new(walker));
        index_recording_over(root, rel, addr, fs)
    }

    /// [`index_recording`] over the given filesystem.
    fn index_recording_over(
        root: &std::path::Path,
        rel: &str,
        addr: &str,
        fs: Arc<dyn hwalk::codegen_index::IndexFs>,
    ) -> Arc<CodegenIndex> {
        use hwalk::codegen_index::{ImportLockGuard, Landing, LandingContent, TargetLockGuard};
        let index = Arc::new(CodegenIndex::open(root, &root.join(".heph"), fs).unwrap());
        index
            .create_db(&ImportLockGuard::new(Box::new(())))
            .unwrap();
        let bytes = fs::read(root.join(rel)).unwrap();
        let landing = Landing {
            path: rel.as_bytes().to_vec(),
            output: "out".into(),
            content: LandingContent::File {
                hash: hwalk::bytes_hashout(&bytes, false),
                x: false,
            },
        };
        let plan = index
            .snapshot()
            .unwrap()
            .needs_write_back(addr, &[landing])
            .unwrap()
            .expect("an unrecorded path needs a write-back");
        let wb = index
            .begin_write_back(TargetLockGuard::new(addr, Box::new(())), plan)
            .unwrap();
        // Step 3 (the bytes) already happened: they are on disk.
        wb.settle(&|_| true).unwrap();
        index
    }

    /// Parse and run the fs target `//@heph/fs:<name>` with `config` over `root`.
    async fn run_fs(
        driver: &Driver,
        name: &str,
        config: std::collections::HashMap<String, Value>,
        root: &std::path::Path,
    ) -> anyhow::Result<Vec<OutputArtifact>> {
        let mut parse_req = make_parse_req(config);
        let mut spec = (*parse_req.target_spec).clone();
        spec.addr = parse_addr(&format!("//{PKG}:{name}")).unwrap();
        parse_req.target_spec = Arc::new(spec);
        let parse_res = driver.parse(parse_req, &ctoken()).await.unwrap();
        let request_id = "test".to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            root.to_path_buf(),
            &hashin,
        );
        Ok(driver.run(req, &ctoken()).await?.artifacts)
    }

    fn f_config(path: &str) -> std::collections::HashMap<String, Value> {
        std::collections::HashMap::from([("f".to_string(), Value::String(path.to_string()))])
    }

    /// A glob leaves out a path the index recorded, and reports it to the
    /// request that ran the glob as a typed `CodegenExcluded` event.
    #[tokio::test]
    async fn test_driver_run_glob_excludes_recorded_codegen() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("plain.rs"), b"plain").unwrap();
        fs::write(tmp.path().join("generated.rs"), b"generated").unwrap();
        let index = index_recording(tmp.path(), "generated.rs", "//pkg:gen");
        let recorder = Arc::new(Recorder::default());
        let driver = Driver::default().with_codegen(FsCodegen::new(
            index,
            Arc::clone(&recorder) as Arc<dyn CodegenReport>,
        ));

        let config =
            std::collections::HashMap::from([("p".to_string(), Value::String("*.rs".to_string()))]);
        let arts = run_fs(&driver, "glob", config, tmp.path()).await.unwrap();

        let names: Vec<_> = arts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["plain.rs"],
            "the recorded file must be excluded"
        );
        let events = recorder.events.lock();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(
            matches!(
                &events[0],
                (Some(rid), hcore::events::BuildEventKind::CodegenExcluded {
                    path,
                    status,
                    reason: Some(reason),
                    addr: Some(addr),
                }) if rid == "test"
                    && path == "generated.rs"
                    && status == "generated"
                    && reason == "recorded"
                    && addr == "//pkg:gen"
            ),
            "{events:?}"
        );
    }

    /// Strict `file` over a recorded path fails, naming the generator; the
    /// engine reads the typed error to reword it for a removed target.
    #[tokio::test]
    async fn test_driver_run_file_of_recorded_codegen_fails() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("generated.rs"), b"generated").unwrap();
        let index = index_recording(tmp.path(), "generated.rs", "//pkg:gen");
        let driver = Driver::default().with_codegen(FsCodegen::new(index, Arc::new(NoReport)));

        let Err(err) = run_fs(&driver, "file", f_config("generated.rs"), tmp.path()).await else {
            panic!("file() over a recorded generated path must fail");
        };
        let cg = err
            .chain()
            .find_map(|e| e.downcast_ref::<CodegenFileError>())
            .unwrap_or_else(|| panic!("not a CodegenFileError: {err:#}"));
        assert_eq!(
            *cg,
            CodegenFileError {
                path: "generated.rs".to_string(),
                addr: "//pkg:gen".to_string(),
                kind: CodegenFileErrorKind::Generated,
            }
        );
        assert!(
            format!("{err:#}").contains("depend on //pkg:gen"),
            "{err:#}"
        );
    }

    /// The engine's `copy_output` over the same path is zero artifacts, with
    /// no error; over a path the index does not exclude, it is the file.
    #[tokio::test]
    async fn test_driver_run_copy_output_of_recorded_codegen_yields_nothing() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("generated.rs"), b"generated").unwrap();
        fs::write(tmp.path().join("plain.rs"), b"plain").unwrap();
        let index = index_recording(tmp.path(), "generated.rs", "//pkg:gen");
        let recorder = Arc::new(Recorder::default());
        let driver = Driver::default().with_codegen(FsCodegen::new(
            index,
            Arc::clone(&recorder) as Arc<dyn CodegenReport>,
        ));

        let arts = run_fs(&driver, "copy_output", f_config("generated.rs"), tmp.path())
            .await
            .unwrap();
        assert!(
            arts.is_empty(),
            "copy_output of a recorded path yields nothing"
        );
        assert!(recorder.events.lock().is_empty(), "copy_output is silent");

        let arts = run_fs(&driver, "copy_output", f_config("plain.rs"), tmp.path())
            .await
            .unwrap();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].name, "plain.rs");
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

    // ─── `.heph*` symlink exclusion ────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn test_driver_run_glob_excludes_heph_symlink() {
        use std::os::unix::fs::symlink;

        let driver = Driver::default();
        let tmp = tempdir().unwrap();

        // Engine-internal artifact living under a `.heph*` dir, plus a symlink
        // to it inside the workspace tree.
        let cache = tmp.path().join(".heph3").join("cache");
        fs::create_dir_all(&cache).unwrap();
        let blob = cache.join("blob.rs");
        fs::write(&blob, b"generated").unwrap();
        symlink(&blob, tmp.path().join("linked.rs")).unwrap();

        let plain = tmp.path().join("plain.rs");
        fs::write(&plain, b"plain").unwrap();

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
        assert_eq!(
            names,
            vec!["plain.rs"],
            "symlink into a .heph* dir must be excluded"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_driver_run_file_heph_symlink_yields_nothing() {
        use std::os::unix::fs::symlink;

        let driver = Driver::default();
        let tmp = tempdir().unwrap();

        let cache = tmp.path().join(".heph3").join("cache");
        fs::create_dir_all(&cache).unwrap();
        let blob = cache.join("blob.rs");
        fs::write(&blob, b"generated").unwrap();
        symlink(&blob, tmp.path().join("linked.rs")).unwrap();

        let config = std::collections::HashMap::from([(
            "f".to_string(),
            Value::String("linked.rs".to_string()),
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
            "file() over a .heph* symlink must yield no artifacts"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_resolves_into_heph_dir() {
        use std::os::unix::fs::symlink;

        let tmp = tempdir().unwrap();

        // Plain file, not under any cache: not a heph path.
        let plain = tmp.path().join("plain.rs");
        fs::write(&plain, b"x").unwrap();
        assert!(!resolves_into_heph_dir(&plain));

        // Symlink to a normal (non-.heph) file.
        let normal = tmp.path().join("normal.rs");
        fs::write(&normal, b"x").unwrap();
        let link_normal = tmp.path().join("link_normal.rs");
        symlink(&normal, &link_normal).unwrap();
        assert!(!resolves_into_heph_dir(&link_normal));

        // Symlink directly into a `.heph*` dir.
        let cache = tmp.path().join(".heph3").join("cache");
        fs::create_dir_all(&cache).unwrap();
        let blob = cache.join("blob.rs");
        fs::write(&blob, b"x").unwrap();
        let link_heph = tmp.path().join("link_heph.rs");
        symlink(&blob, &link_heph).unwrap();
        assert!(resolves_into_heph_dir(&link_heph));

        // The load-bearing case: a real file reached *through* a symlinked
        // directory (`gen -> .heph/cache/gen`). The leaf is not itself a
        // symlink, but it transitively resolves inside the cache.
        let cache_gen = cache.join("gen");
        fs::create_dir_all(cache_gen.join("some")).unwrap();
        let deep = cache_gen.join("some").join("file.txt");
        fs::write(&deep, b"x").unwrap();
        let gen_link = tmp.path().join("gen");
        symlink(&cache_gen, &gen_link).unwrap();
        // The symlinked dir itself…
        assert!(resolves_into_heph_dir(&gen_link));
        // …and a plain file underneath it, via the link.
        assert!(resolves_into_heph_dir(
            &gen_link.join("some").join("file.txt")
        ));
    }

    /// A glob walk runs on the blocking pool, not on the runtime worker: on a
    /// one-thread runtime, a 1ms heartbeat keeps ticking while the walk is
    /// stuck hashing a file.
    #[tokio::test(flavor = "current_thread")]
    async fn glob_walk_does_not_park_the_runtime_worker() {
        assert_run_leaves_the_worker_free(
            "p",
            "*.txt",
            "glob_walk_does_not_park_the_runtime_worker",
        )
        .await;
    }

    /// Same for `file()`: its stat and hash run on the blocking pool too.
    #[tokio::test(flavor = "current_thread")]
    async fn file_hash_does_not_park_the_runtime_worker() {
        assert_run_leaves_the_worker_free(
            "f",
            "slow.txt",
            "file_hash_does_not_park_the_runtime_worker",
        )
        .await;
    }

    /// Runs an fs target whose one match, `slow.txt`, is a symlink to a FIFO,
    /// alongside a 1ms heartbeat, and asserts the heartbeat never stalls. The
    /// FIFO makes hashing block in `open` until a writer thread shows up
    /// `STALL` later — a run of a known, deterministic duration with no huge
    /// tree on disk. Inline on the only worker, the heartbeat stalls for all
    /// of it.
    async fn assert_run_leaves_the_worker_free(key: &str, val: &str, request_id: &str) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        const STALL: Duration = Duration::from_millis(300);

        let tmp = tempdir().unwrap();
        let fifo = tmp.path().join("slow.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed");
        std::os::unix::fs::symlink(&fifo, tmp.path().join("slow.txt")).unwrap();

        let driver = Driver::default();
        let config =
            std::collections::HashMap::from([(key.to_string(), Value::String(val.to_string()))]);
        let parse_res = driver
            .parse(make_parse_req(config), &ctoken())
            .await
            .unwrap();
        // The walk memo is process-wide, keyed by request: a key of its own
        // keeps another test's walk from answering this one.
        let request_id = request_id.to_string();
        let hashin = String::new();
        let req = make_run_req(
            &parse_res.target_def,
            &request_id,
            tmp.path().to_path_buf(),
            &hashin,
        );

        let stop = Arc::new(AtomicBool::new(false));
        let heartbeat = tokio::spawn(enclose::enclose!((stop) async move {
            let mut max_gap = Duration::ZERO;
            let mut last = Instant::now();
            while !stop.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
                let now = Instant::now();
                max_gap = max_gap.max(now - last);
                last = now;
            }
            max_gap
        }));
        // Let the heartbeat start ticking before the walk can take the thread.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let writer = std::thread::spawn(enclose::enclose!((fifo) move || {
            std::thread::sleep(STALL);
            fs::write(&fifo, b"slow\n").expect("write fifo");
        }));
        let started = Instant::now();
        let res = driver.run(req, &ctoken()).await.unwrap();
        let walk = started.elapsed();
        stop.store(true, Ordering::Release);
        let max_gap = heartbeat.await.unwrap();
        writer.join().unwrap();

        assert_eq!(res.artifacts.len(), 1, "the FIFO link is the one match");
        assert!(walk >= STALL, "the run must wait on the writer: {walk:?}");
        assert!(
            max_gap < STALL / 3,
            "heartbeat stalled {max_gap:?} during a {walk:?} run"
        );
    }
}
