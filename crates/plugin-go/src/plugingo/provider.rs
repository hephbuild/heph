use crate::plugingo::addr_util::{
    GoPackageKind, decode_package, encode_firstparty, encode_stdlib, encode_thirdparty,
    encode_thirdparty_download, factors_to_args,
};
use crate::plugingo::errors::NoGoFilesError;
use crate::plugingo::factors::{Factors, current_goarch, current_goos};
use crate::plugingo::pkg_analysis::{
    GoPackage, PackageAddrs, decode_go_package, decode_package_addrs, find_module_for_import,
    is_stdlib_import_path, parse_go_mod_module_path, parse_go_mod_requires, parse_go_sum_modules,
};
use crate::plugingo::target_bin;
use crate::plugingo::target_golist;
use crate::plugingo::target_lib;
use crate::plugingo::target_modfiles;
use crate::plugingo::target_std;
use crate::plugingo::target_test;
use crate::plugingo::thirdparty;
use anyhow::Context;
use async_recursion::async_recursion;
use async_trait::async_trait;
use enclose::enclose;
use futures::future::{BoxFuture, try_join_all};
use hbuiltins::pluginfs;
use hcore::hasync::Cancellable;
use hcore::hmemoizer::{Memoizer, downcast_chain_ref, unwrap_arc_err};
use hcore::htvalue::signature::{FnSignature, Param, ParamType};
use hcore::htvalue::{Value, parse_strings};
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, FnArgs, FnCallContext, GetError, GetRequest, GetResponse,
    ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse, Provider as ProviderTrait,
    ProviderExecutor, ProviderFn, ProviderFunctionDef, State,
};
use hwalk::{CachedWalker, EntryKind, Ignore};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DEFAULT_GO_BIN_ADDR: &str = "//@heph/bin:go";

pub struct Config {
    pub go_bin_addr: String,
    /// Directories pruned during package discovery: engine skip dirs/globs plus
    /// this provider's own `skip` option. See [`hwalk::Ignore`].
    pub skip: Arc<Ignore>,
    /// Reject target names this provider doesn't own *before* the `_golist`
    /// resolve (see [`is_known_go_target_name`]). On by default: it avoids a
    /// wasted `go list` for foreign names (e.g. a buildfile codegen target
    /// sharing a Go package dir) and the cycle that resolve would induce. The
    /// engine contains that cycle regardless (a cyclic provider attempt falls
    /// through to the next provider), so this is a perf/clarity guard, not a
    /// correctness crutch — tests that exercise the engine's containment path
    /// turn it off.
    pub foreign_name_guard: bool,
    /// Shared cross-run filesystem-walk cache. `collect_go_packages` reads each
    /// directory through it, so an unchanged tree skips `readdir` (package
    /// identity depends only on the directory layout, which the walker caches by
    /// mtime). Disabled by default — unit tests that build a bare provider walk
    /// live; the engine injects the real shared walker via `from_options`.
    pub walker: Arc<CachedWalker>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            go_bin_addr: DEFAULT_GO_BIN_ADDR.to_string(),
            skip: Arc::new(Ignore::default()),
            foreign_name_guard: true,
            walker: Arc::new(CachedWalker::disabled()),
        }
    }
}

/// Public façade: thin wrapper around `Arc<ProviderInner>` so trait-method
/// closures (e.g. `Memoizer::once` in `collect_*_libs`) can capture `Arc<Self>`
/// without dragging `&self` lifetimes through `'static` future bounds.
pub struct Provider {
    inner: Arc<ProviderInner>,
}

pub(crate) struct ProviderInner {
    workspace_root: PathBuf,
    goroot: String,
    gomodcache: String,
    gopath: String,
    gocache: String,
    go_bin_addr: String,
    /// Directories pruned during `collect_go_packages` (engine home + user globs).
    skip: Arc<Ignore>,
    /// Shared cross-run fs-walk cache backing the package walk. See [`Config::walker`].
    walker: Arc<CachedWalker>,
    /// See [`Config::foreign_name_guard`].
    foreign_name_guard: bool,
    /// Cache: golist addr → parsed GoPackage. Memoizes the artifact parse only;
    /// the underlying `executor.result(golist_addr)` is always called outside
    /// the `once` closure so every caller (owner + waiter) registers the
    /// `parent → golist_addr` edge in the engine's `DepDag`. Caching the
    /// executor call here would let a waiter skip dep registration and hide a
    /// target-dep cycle as a memoizer deadlock.
    pkg_cache: Memoizer<Addr, Result<Arc<GoPackage>, Arc<anyhow::Error>>>,
    /// Cache: golist addr → driver-resolved per-file addresses. Same constraint
    /// as `pkg_cache` — caches the parse, not the executor call.
    pkg_addrs_cache: Memoizer<Addr, Result<Arc<PackageAddrs>, Arc<anyhow::Error>>>,
    /// Cache: dedup `collect_direct_libs` / `collect_transitive_libs` BFS across
    /// `handle_get` calls (`build_lib`, `build`, `build_test`, `build_test_lib`,
    /// `build_xtest_lib`, `build_testmain_lib`) that share the same root pkg + factors.
    libs_cache: Memoizer<LibsKey, Result<Arc<TransitiveDeps>, Arc<anyhow::Error>>>,
    /// Cache: per-`(import_path, factors, module_root)` transitive closure. Used by the
    /// `transitive=true` path of `collect_libs_inner`: each top-level walk recursively
    /// composes per-import sub-closures from this cache instead of BFS-walking the
    /// full subtree itself. Subtree-sharing across all consumers — e.g. if 200
    /// top-level targets all transitively depend on `fmt`, `fmt`'s closure is computed
    /// once total, not 200 times.
    import_closure_cache:
        Memoizer<ImportClosureKey, Result<Arc<ImportClosure>, Arc<anyhow::Error>>>,
    /// Cache: parsed `go.mod` per `module_root`. `collect_libs_inner` is invoked
    /// per `LibsKey` (≥ K×N for K target variants × N root pkgs in module), so
    /// the same `go.mod` is otherwise read+parsed hundreds of times per build.
    /// Race-tolerant; parse is sync, idempotent, µs-scale — no Memoizer needed.
    go_mod_cache: RwLock<HashMap<PathBuf, Arc<GoModData>>>,
}

#[derive(Debug)]
pub(crate) struct GoModData {
    pub requires: Vec<(String, String)>,
    pub module_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LibsKey {
    imports: Vec<String>,
    extra: Vec<String>,
    factors: Factors,
    module_root: PathBuf,
    transitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImportClosureKey {
    import_path: String,
    factors: Factors,
    module_root: PathBuf,
}

/// Result of `import_closure(ip)`: the import_path's lib (if any) plus its full
/// transitive closure, in topological order (deps before dependents). Composing
/// multiple `ImportClosure`s requires de-duplication by `import_path` at the
/// composition site (see `compose_closures`).
///
/// Import paths are `Arc<str>` so that composing a node's closure from its
/// children's closures (which copies every transitive entry, at every node — an
/// O(closure-size) copy per node) is a refcount bump rather than a String
/// heap-allocation+copy. The flattened result is converted to owned `String`s
/// once, at the top of `collect_libs_inner`.
#[derive(Debug, Clone, Default)]
struct ImportClosure {
    libs: Vec<(Arc<str>, Addr)>,
}

/// Flatten already-deps-first-deduped `sub_closures` into one deps-first list,
/// deduped by import_path, with `self_lib` (if any) appended last. Shared by
/// `import_closure` (composing a node from its children's closures) and
/// `collect_libs_inner` (composing the transitive set from the root imports).
///
/// Import paths are `Arc<str>`, so re-listing a child's transitive entries here —
/// which happens at *every* node, an O(closure-size) copy per node — is a refcount
/// bump rather than a String heap-copy. The dedup set and result are pre-sized to
/// the summed child closure size (+ self).
fn compose_closures(
    sub_closures: &[Arc<ImportClosure>],
    self_lib: Option<(Arc<str>, Addr)>,
) -> Vec<(Arc<str>, Addr)> {
    let extra = self_lib.is_some() as usize;
    let cap = sub_closures.iter().map(|s| s.libs.len()).sum::<usize>() + extra;
    let mut seen: HashSet<Arc<str>> = HashSet::with_capacity(cap);
    let mut libs: Vec<(Arc<str>, Addr)> = Vec::with_capacity(cap);
    for sub in sub_closures {
        for (ip, addr) in &sub.libs {
            if seen.insert(Arc::clone(ip)) {
                libs.push((Arc::clone(ip), addr.clone()));
            }
        }
    }
    if let Some((ip, addr)) = self_lib
        && seen.insert(Arc::clone(&ip))
    {
        libs.push((ip, addr));
    }
    libs
}

impl Provider {
    pub fn new(workspace_root: PathBuf) -> anyhow::Result<Self> {
        Self::with_config(workspace_root, Config::default())
    }

    pub fn from_options(
        workspace_root: PathBuf,
        skip_dirs: &[PathBuf],
        skip_globs: &[String],
        opts: &hplugin::config::Options,
        walker: Arc<CachedWalker>,
    ) -> anyhow::Result<Self> {
        hplugin::config::deny_unknown("go provider", opts, &["gotool", "skip"])?;
        let go_bin_addr: String = hplugin::config::decode_opt(opts, "go provider", "gotool")?
            .unwrap_or_else(|| DEFAULT_GO_BIN_ADDR.to_string());
        // Engine-wide `fs.skip` globs are merged ahead of this provider's own
        // `skip` option so both prune the same workspace-relative paths.
        let mut globs = skip_globs.to_vec();
        let user_skip: Vec<String> =
            hplugin::config::decode_opt(opts, "go provider", "skip")?.unwrap_or_default();
        globs.extend(user_skip);
        let skip = Arc::new(Ignore::new(skip_dirs, &globs)?);
        Self::with_config(
            workspace_root,
            Config {
                go_bin_addr,
                skip,
                walker,
                ..Default::default()
            },
        )
    }

    pub fn go_bin_addr(&self) -> &str {
        &self.inner.go_bin_addr
    }

    pub fn with_config(workspace_root: PathBuf, config: Config) -> anyhow::Result<Self> {
        // One `go env` round-trip for all four variables instead of four spawns.
        let env = resolve_go_env_vars(&["GOROOT", "GOMODCACHE", "GOPATH", "GOCACHE"])?;
        let [goroot, gomodcache, gopath, gocache] = <[String; 4]>::try_from(env).map_err(|v| {
            anyhow::anyhow!(
                "resolve_go_env_vars returned {} values, expected 4",
                v.len()
            )
        })?;
        Ok(Self {
            inner: Arc::new(ProviderInner {
                workspace_root,
                goroot,
                gomodcache,
                gopath,
                gocache,
                go_bin_addr: config.go_bin_addr,
                skip: config.skip,
                walker: config.walker,
                foreign_name_guard: config.foreign_name_guard,
                pkg_cache: Memoizer::with_tag("pkg_cache"),
                pkg_addrs_cache: Memoizer::with_tag("pkg_addrs_cache"),
                libs_cache: Memoizer::with_tag("libs_cache"),
                import_closure_cache: Memoizer::with_tag("import_closure_cache"),
                go_mod_cache: RwLock::new(HashMap::new()),
            }),
        })
    }
}

/// Resolve several `go env` variables in a single subprocess, in the order
/// requested. A process env var of the same name overrides (matching go's own
/// precedence); only the names not already set are fetched, via one
/// `go env A B C` round-trip rather than one `go` spawn per variable. Each
/// spawn is ~15-20 ms, and the go provider needs four of these at construction,
/// so batching removes ~3 subprocess spawns from every invocation's startup.
fn resolve_go_env_vars(names: &[&str]) -> anyhow::Result<Vec<String>> {
    // Per name: `Some` if satisfied from the process environment, else `None`
    // (must come from `go env`). `missing` preserves request order.
    let mut from_env: Vec<Option<String>> = Vec::with_capacity(names.len());
    let mut missing: Vec<&str> = Vec::new();
    for name in names {
        match std::env::var(name) {
            Ok(val) if !val.is_empty() => from_env.push(Some(val)),
            _ => {
                from_env.push(None);
                missing.push(name);
            }
        }
    }

    let mut fetched: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    if !missing.is_empty() {
        let output = std::process::Command::new("go")
            .arg("env")
            .args(&missing)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run `go env {}`: {}", missing.join(" "), e))?;
        if !output.status.success() {
            anyhow::bail!("go env {} failed", missing.join(" "));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|e| anyhow::anyhow!("go env output not utf-8: {}", e))?;
        // `go env A B C` prints one value per line, in the requested order.
        let lines: Vec<&str> = stdout.lines().collect();
        if lines.len() != missing.len() {
            anyhow::bail!(
                "go env returned {} lines for {} variables",
                lines.len(),
                missing.len()
            );
        }
        for (name, line) in missing.iter().zip(lines) {
            fetched.insert(name, line.trim().to_string());
        }
    }

    Ok(names
        .iter()
        .zip(from_env)
        .map(|(name, env_val)| {
            env_val.unwrap_or_else(|| fetched.get(name).cloned().unwrap_or_default())
        })
        .collect())
}

/// Recursively enumerate directories at or below `dir` that live under a
/// `go.mod` ancestor. Replaces the prior `.go`-file-based detection: any dir
/// under a Go module is a candidate package, and `_golist` decides what is real
/// when the engine actually queries it.
///
/// `under_gomod` is the inherited flag from the parent walk — once we've found
/// a `go.mod` at or above the current dir, every descendant inherits it and we
/// skip the per-dir `find_go_mod` lookup. The `find_go_mod` cache absorbs the
/// cost when the flag is unset.
fn collect_go_packages(
    walker: &CachedWalker,
    dir: &Path,
    workspace_root: &Path,
    under_gomod: bool,
    skip: &Ignore,
    result: &mut Vec<anyhow::Result<ListPackageResponse>>,
) {
    let is_under = under_gomod || crate::plugingo::addr_util::find_go_mod(dir).is_some();

    if is_under {
        let rel = dir.strip_prefix(workspace_root).unwrap_or(dir);
        result.push(Ok(ListPackageResponse {
            pkg: PkgBuf::from(rel.to_string_lossy().as_ref()),
        }));
    }

    // Read through the shared walker: an unchanged tree skips the `readdir`
    // syscall entirely (the cached listing is keyed by directory mtime). A
    // symlinked dir lists as `Symlink`, not `Dir`, so it is not descended —
    // matching the previous `file_type()` (no-follow) behavior.
    let listing = match walker.read_dir(dir) {
        Ok(l) => l,
        Err(e) => {
            result.push(Err(e.context(format!("read_dir {}", dir.display()))));
            return;
        }
    };

    for entry in &listing.entries {
        if entry.kind != EntryKind::Dir {
            continue;
        }
        // Skip dot/underscore-prefixed dirs and the go-convention non-package
        // dirs by raw bytes. A leading `.`/`_` is a single ASCII byte and UTF-8
        // lead/continuation bytes are all >= 0x80, so a byte compare can't
        // misfire on a multibyte name.
        let bytes = entry.name.as_bytes();
        if matches!(bytes.first(), Some(b'.' | b'_')) || bytes == b"vendor" || bytes == b"testdata"
        {
            continue;
        }
        let entry_path = dir.join(&entry.name);
        let rel = entry_path
            .strip_prefix(workspace_root)
            .unwrap_or(&entry_path);
        if skip.prune_dir(&entry_path, rel) {
            continue;
        }
        collect_go_packages(walker, &entry_path, workspace_root, is_under, skip, result);
    }
}

impl ProviderTrait for Provider {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        self.inner.config(req)
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        self.inner.list(req, ctoken)
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        self.inner.list_packages(req, ctoken)
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.handle_get(req).await })
    }

    fn probe<'a>(
        &'a self,
        req: ProbeRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        self.inner.probe(req, ctoken)
    }

    fn functions(&self) -> Vec<ProviderFunctionDef> {
        vec![ProviderFunctionDef {
            name: "build_addr".to_string(),
            signature: FnSignature {
                positional: vec![
                    Param::required("pkg", ParamType::String),
                    Param::required("goos", ParamType::String),
                    Param::required("goarch", ParamType::String),
                ],
                named: vec![Param::optional(
                    "tags",
                    ParamType::list(ParamType::String),
                    Value::List(vec![]),
                )],
                variadic: None,
                returns: ParamType::String,
            },
            doc: "Build the address of a Go package's `_golist` target for a given \
                  GOOS/GOARCH (and optional build tags), as used in `deps`."
                .to_string(),
            func: Arc::new(BuildAddrFn),
        }]
    }

    fn state_schema(&self) -> Option<hplugin::provider::StateSchema> {
        use hplugin::provider::{StateField, StateSchema};
        let field = |name: &str, ty: ParamType, doc: &str| StateField {
            name: name.to_string(),
            ty,
            doc: doc.to_string(),
            required: false,
        };
        Some(StateSchema {
            fields: vec![
                field(
                    "go_codegen_root",
                    ParamType::Bool,
                    "Mark this package (and descendants) as a Go codegen root.",
                ),
                field(
                    "go_codegen_deps",
                    ParamType::list(ParamType::String),
                    "Extra dependencies (target addresses) injected into generated Go targets.",
                ),
                field(
                    "test",
                    ParamType::map(ParamType::Bool),
                    "Test settings for this package, e.g. `{\"skip\": True}` to skip its tests.",
                ),
            ],
        })
    }
}

/// `heph.go.build_addr(pkg, goos, goarch, tags=[])` — format the heph
/// address of a Go target without resolving anything. Takes a heph package (the addr's
/// package, e.g. `"mylib"`, `"@heph/go/std/fmt"`, or a thirdparty `@heph/go/thirdparty/…@v`
/// path) and the platform factors, and returns the canonical addr
/// string `//<pkg>:build@goos=…,goarch=…[,tags=…]`. Pure string transform — same
/// factor encoding the provider uses internally ([`factors_to_args`]), so the result
/// matches the addr the provider serves for that package.
struct BuildAddrFn;

impl BuildAddrFn {
    fn arg_str<'a>(args: &'a FnArgs, idx: usize, name: &str) -> anyhow::Result<&'a str> {
        let v = args
            .named
            .get(name)
            .or_else(|| args.positional.get(idx))
            .ok_or_else(|| anyhow::anyhow!("heph.go.build_addr: missing `{name}` argument"))?;
        match v {
            Value::String(s) => Ok(s.as_str()),
            other => anyhow::bail!("heph.go.build_addr: `{name}` must be a string, got {other:?}"),
        }
    }
}

#[async_trait]
impl ProviderFn for BuildAddrFn {
    async fn call(&self, _ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value> {
        let pkg = Self::arg_str(&args, 0, "pkg")?;
        let goos = Self::arg_str(&args, 1, "goos")?;
        let goarch = Self::arg_str(&args, 2, "goarch")?;

        let mut build_tags = match args.named.get("tags") {
            Some(v) => parse_strings(v).context("heph.go.build_addr: parsing `tags`")?,
            None => Vec::new(),
        };
        build_tags.sort();

        let factors = Factors {
            goos: goos.to_string(),
            goarch: goarch.to_string(),
            build_tags,
        };
        let addr = Addr::new(
            PkgBuf::from(pkg),
            "build".to_string(),
            factors_to_args(&factors),
        );
        Ok(Value::String(addr.format()))
    }
}

impl ProviderInner {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "go".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            let factors = Factors {
                goos: current_goos(),
                goarch: current_goarch(),
                build_tags: vec![],
            };

            let kind = match decode_package(&req.package, &self.workspace_root) {
                Some(k) => k,
                None => {
                    return Ok(Box::new(std::iter::empty())
                        as Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>);
                }
            };

            // A first-party package inside a skipped subtree lists nothing —
            // mirroring what the package walk (`collect_go_packages`) prunes.
            if matches!(&*kind, GoPackageKind::FirstParty { .. })
                && self
                    .skip
                    .prunes_package(&self.workspace_root, Path::new(req.package.as_str()))
            {
                return Ok(Box::new(std::iter::empty())
                    as Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>);
            }

            match &*kind {
                GoPackageKind::Stdlib { .. } => {
                    let addrs = vec![
                        Addr::new(
                            req.package.clone(),
                            "_golist".to_string(),
                            factors_to_args(&factors),
                        ),
                        Addr::new(
                            req.package,
                            "build_lib".to_string(),
                            factors_to_args(&factors),
                        ),
                    ];
                    let responses: Vec<anyhow::Result<ListResponse>> = addrs
                        .into_iter()
                        .map(|addr| Ok(ListResponse { addr }))
                        .collect();
                    Ok(Box::new(responses.into_iter())
                        as Box<
                            dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                        >)
                }
                GoPackageKind::ThirdParty { subpath, .. } => {
                    let mut addrs = vec![
                        Addr::new(
                            req.package.clone(),
                            "_golist".to_string(),
                            factors_to_args(&factors),
                        ),
                        Addr::new(
                            req.package.clone(),
                            "build_lib".to_string(),
                            factors_to_args(&factors),
                        ),
                    ];
                    // The `download` target lives at the module root only and
                    // is factor-independent (one per module@version).
                    if subpath.is_empty() {
                        addrs.push(Addr::new(
                            req.package,
                            "download".to_string(),
                            Default::default(),
                        ));
                    }
                    let responses: Vec<anyhow::Result<ListResponse>> = addrs
                        .into_iter()
                        .map(|addr| Ok(ListResponse { addr }))
                        .collect();
                    Ok(Box::new(responses.into_iter())
                        as Box<
                            dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                        >)
                }
                GoPackageKind::FirstParty { .. } => {
                    // Emit the full candidate set unconditionally for any dir
                    // under a go.mod (decode_package guarantees that). Each
                    // variant is filtered at `get` time by inspecting the
                    // `_golist` result — no filesystem scan needed.
                    let skip_tests = pick_test_skip(&req.states);
                    let names: &[&str] = if skip_tests {
                        &["_golist", "build_lib", "build", "embed"]
                    } else {
                        &[
                            "_golist",
                            "build_lib",
                            "build",
                            "embed",
                            "embed_xtest",
                            "build_test",
                            "test",
                            "build_xtest",
                            "xtest",
                        ]
                    };
                    let responses: Vec<anyhow::Result<ListResponse>> = names
                        .iter()
                        .map(|name| {
                            Ok(ListResponse {
                                addr: Addr::new(
                                    req.package.clone(),
                                    (*name).to_string(),
                                    factors_to_args(&factors),
                                ),
                            })
                        })
                        .collect();

                    Ok(Box::new(responses.into_iter())
                        as Box<
                            dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                        >)
                }
            }
        })
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            let prefix = req.prefix.as_str();

            // Can't enumerate stdlib or thirdparty packages
            if prefix.starts_with("@heph/go/") {
                return Ok(Box::new(std::iter::empty())
                    as Box<
                        dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                    >);
            }

            let search_dir = if prefix.is_empty() {
                self.workspace_root.clone()
            } else {
                self.workspace_root.join(prefix)
            };

            if !search_dir.exists() {
                return Ok(Box::new(std::iter::empty())
                    as Box<
                        dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                    >);
            }

            let packages = hproc::process_supervisor::block_or_inline(
                enclose!((self.workspace_root => workspace_root, self.skip => skip, self.walker => walker) move || {
                    let mut packages = Vec::new();
                    collect_go_packages(&walker, &search_dir, &workspace_root, false, &skip, &mut packages);
                    packages
                }),
            );

            Ok(Box::new(packages.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
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

#[derive(Clone)]
struct TransitiveDeps {
    /// `(import_path, build_lib_addr)` for every reachable dep (for importcfg).
    libs: Vec<(String, Addr)>,
}

/// Pick the closest (deepest) ancestor `provider_state(provider="go", ...)` that
/// has `go_codegen_root=True` and whose package is a prefix of `addr_pkg`.
///
/// Engine pre-filters states by provider name, so callers only see Go states.
fn pick_codegen_root(states: &[State]) -> Option<&State> {
    states
        .iter()
        .filter(|s| matches!(s.state.get("go_codegen_root"), Some(Value::Bool(true))))
        .max_by_key(|s| s.package.as_str().len())
}

/// Go test-target name set. Every variant the provider may emit (list) or
/// resolve (get) that exists solely to support `go test`. Used by
/// `pick_test_skip` to gate both endpoints from a single source of truth.
const TEST_TARGET_NAMES: &[&str] = &[
    "test",
    "xtest",
    "build_test",
    "build_xtest",
    "build_test_lib",
    "build_xtest_lib",
    "build_testmain_lib",
    "build_xtestmain_lib",
    "testmain",
    "xtestmain",
    "embed_test",
    "embed_xtest",
];

fn is_test_target_name(name: &str) -> bool {
    TEST_TARGET_NAMES.contains(&name)
}

/// Targets handled by `handle_get` *before* the `_golist` resolve (no go list
/// needed): the golist target itself, the go.mod copy, and the module download.
const SPECIAL_TARGET_NAMES: &[&str] = &["_golist", "_go_mod", "download"];

/// Non-test first-party/thirdparty target names this provider owns and resolves
/// through `_golist` (see the `match addr.name` arms in `handle_get`).
const GOLIST_TARGET_NAMES: &[&str] = &["build_lib", "build", "embed"];

/// Whether this provider owns `name` — the complete set of go targets it can
/// generate: the pre-golist special targets, the `_golist`-resolved non-test
/// targets, and every test variant.
fn is_known_go_target_name(name: &str) -> bool {
    SPECIAL_TARGET_NAMES.contains(&name)
        || GOLIST_TARGET_NAMES.contains(&name)
        || TEST_TARGET_NAMES.contains(&name)
}

/// Choose which lib variant of P (the current package) to expose in the
/// xtest_lib compile and the build_xtest link, so both agree.
///
/// xtest bin's importcfg must reference the SAME .a for P that xtest_lib's
/// compile embedded — otherwise the linker rejects the fingerprint mismatch.
///
/// - GoFiles non-empty → `build_lib` (normal). Required by the cycle-safety
///   rule documented on `build_xtest`: all consumers of P agree on normal.
/// - GoFiles empty, TestGoFiles non-empty → `build_test_lib`. test-only and
///   test+xtest-only packages have no normal lib, so the test-augmented lib
///   is the only available flavor; no cycle is possible because nothing else
///   in the build can import P (it has no buildable non-test sources).
/// - both empty (xtest-only) → `None`. P has no lib at all; xtest source
///   can't legitimately reference symbols from a package with no declarations.
fn pick_xtest_p_lib_name(pkg: &GoPackage) -> Option<&'static str> {
    if !pkg.go_files.is_empty() {
        Some("build_lib")
    } else if !pkg.test_go_files.is_empty() {
        Some("build_test_lib")
    } else {
        None
    }
}

/// Return true if the closest (deepest) ancestor `provider_state(provider="go", ...)`
/// that carries a `test = {...}` map sets `skip = True`. Deeper states fully
/// override shallower ones — a `test = {"skip": False}` closer to the target
/// re-enables tests even if a root-level state disabled them.
fn pick_test_skip(states: &[State]) -> bool {
    let Some(state) = states
        .iter()
        .filter(|s| s.state.contains_key("test"))
        .max_by_key(|s| s.package.as_str().len())
    else {
        return false;
    };
    let Some(Value::Map(test_map)) = state.state.get("test") else {
        return false;
    };
    matches!(test_map.get("skip"), Some(Value::Bool(true)))
}

/// Pick the closest (deepest) ancestor state carrying `go_codegen_deps`.
/// Independent of `go_codegen_root` — a BUILD file declaring only
/// `go_codegen_deps` must still inject those deps into descendant `_golist`
/// targets so generated `.go` files reach the sandbox. Mirrors `getCodegenDeps`
/// in the Go reference impl (`heph/plugin/plugingo/plugin.go:184-200`), which
/// scans for deps independently of the root marker.
fn pick_codegen_deps(states: &[State]) -> Option<&State> {
    states
        .iter()
        .filter(|s| s.state.contains_key("go_codegen_deps"))
        .max_by_key(|s| s.package.as_str().len())
}

/// Source addrs the package sandbox needs beyond the canonical `*.go` filesystem
/// glob. Includes:
/// 1. `**/*` filesystem glob (excluding `.go` files) — picks up checked-in
///    non-Go sources (e.g. embed targets).
/// 2. `q@label=go_src,tree_output_to=pkg` query — unpacks the full output tree
///    of any codegen target labelled `go_src` into the pkg dir, so both
///    generated `.go` files and any sibling non-go outputs (e.g. `.wasm.br`)
///    land in the sandbox.
/// 3. `go_codegen_deps` from the closest ancestor BUILD state — explicit
///    codegen targets that don't carry the `go_src` label.
///
/// Shared between `_golist` (so `go list` can resolve `//go:embed` patterns
/// into `EmbedFiles`) and `embed` (so the driver's runtime re-glob of
/// `embed_patterns` against `sandbox_pkg_dir` matches Go's resolution).
fn compute_pkg_src_addrs(pkg_str: &str, states: &[State]) -> anyhow::Result<Vec<String>> {
    let non_go_glob = if pkg_str.is_empty() {
        "**/*".to_string()
    } else {
        format!("{}/**/*", pkg_str)
    };
    let non_go_glob_addr = pluginfs::glob_addr(&non_go_glob, &["**/*.go"]);
    let mut addrs = vec![non_go_glob_addr.format()];

    let codegen_root = pick_codegen_root(states);
    let mut q_args = BTreeMap::from([
        ("label".to_string(), "go_src".to_string()),
        ("tree_output_to".to_string(), pkg_str.to_string()),
    ]);
    match codegen_root {
        Some(root) => {
            q_args.insert(
                "package_prefix".to_string(),
                root.package.as_str().to_string(),
            );
        }
        None => {
            q_args.insert("package".to_string(), pkg_str.to_string());
        }
    }
    let go_src_query_addr = Addr::new(
        hmodel::htpkg::PkgBuf::from(hplugin_query::pluginquery::PACKAGE),
        "q".to_string(),
        q_args,
    );
    addrs.push(go_src_query_addr.format());

    if let Some(deps_state) = pick_codegen_deps(states)
        && let Some(deps_val) = deps_state.state.get("go_codegen_deps")
    {
        let deps =
            parse_strings(deps_val).context("parsing go_codegen_deps from go provider_state")?;
        addrs.extend(deps);
    }
    Ok(addrs)
}

impl ProviderInner {
    async fn handle_get(self: Arc<Self>, req: GetRequest) -> Result<GetResponse, GetError> {
        let addr = &req.addr;
        let factors = Factors::from_addr(addr);

        let kind = match decode_package(&addr.package, &self.workspace_root) {
            Some(k) => k,
            None => return Err(GetError::NotFound),
        };

        // A first-party package inside a skipped subtree does not resolve —
        // mirroring what the package walk (`collect_go_packages`) prunes.
        if matches!(&*kind, GoPackageKind::FirstParty { .. })
            && self
                .skip
                .prunes_package(&self.workspace_root, Path::new(addr.package.as_str()))
        {
            return Err(GetError::NotFound);
        }

        // Reject names this provider doesn't own as early as possible — before any
        // special-case handler or `go list`. A foreign name (e.g. a buildfile
        // codegen target sharing a Go package dir) would otherwise drag `go list`
        // and its `q@label=go_src` query into resolution and trip a cycle. On by
        // default (perf/clarity); the engine contains the cycle regardless (cyclic
        // provider attempts fall through to the next provider), so tests exercising
        // that path disable it via `Config::foreign_name_guard`. Owned names —
        // including the specials handled just below (`_golist`/`_go_mod`/`download`)
        // — are in `is_known_go_target_name`, so this never rejects a real target.
        if self.foreign_name_guard && !is_known_go_target_name(&addr.name) {
            return Err(GetError::NotFound);
        }

        // _golist — generate spec without executing go list (before stdlib check so
        // stdlib packages can also expose a _golist target for cached dep resolution)
        if addr.name == "_golist" {
            return self
                .get_golist_spec(addr.clone(), &kind, &factors, &req.states)
                .map_err(GetError::Other);
        }

        // Stdlib — no go list needed for other targets
        if let GoPackageKind::Stdlib { import_path } = &*kind {
            return self.get_stdlib(addr.clone(), import_path, &factors);
        }

        // _go_mod — copy go.mod/go.sum; no go list needed
        if addr.name == "_go_mod" {
            let module_root = self.workspace_root.join(addr.package.as_str());
            let mod_files: Vec<String> = ["go.mod", "go.sum"]
                .iter()
                .filter(|f| module_root.join(f).exists())
                .map(|f| f.to_string())
                .collect();
            if mod_files.is_empty() {
                return Err(GetError::NotFound);
            }
            let spec = target_modfiles::build_spec(addr.clone(), &mod_files);
            return Ok(GetResponse { target_spec: spec });
        }

        // download — module-root only, factor-independent. Runs `go mod download`
        // and exposes the module source tree as artifacts so downstream build_lib
        // / embed targets get fully sandboxed sources instead of host GOMODCACHE.
        if addr.name == "download" {
            if let GoPackageKind::ThirdParty {
                module,
                version,
                subpath,
                ..
            } = &*kind
            {
                if !subpath.is_empty() {
                    return Err(GetError::NotFound);
                }
                let spec = thirdparty::build_download_spec(
                    addr.clone(),
                    module,
                    version,
                    &self.go_bin_addr,
                    &self.goroot,
                );
                return Ok(GetResponse { target_spec: spec });
            }
            return Err(GetError::NotFound);
        }

        // provider_state(provider="go", test={"skip": True}) opts the package
        // (and all descendants) out of test-target generation. Gate every test
        // variant before the `_golist` resolve below so a skipped pkg never
        // forces a `go list` round-trip purely to learn there are no tests.
        if is_test_target_name(&addr.name) && pick_test_skip(&req.states) {
            return Err(GetError::NotFound);
        }

        let import_path = match &*kind {
            GoPackageKind::FirstParty { import_path, .. } => import_path.clone(),
            GoPackageKind::ThirdParty {
                module, subpath, ..
            } => {
                if subpath.is_empty() {
                    module.clone()
                } else {
                    format!("{}/{}", module, subpath)
                }
            }
            GoPackageKind::Stdlib { .. } => return Err(GetError::NotFound),
        };

        // Resolve package info via _golist target (cached by the engine).
        // `NoGoFilesError` (raised by `read_golist_package` when `go list -e`
        // reports the package has no buildable Go files) maps uniformly to
        // `NotFound` for every variant — no per-arm duck-typed check needed.
        let golist_addr = self.make_addr_with_name(&addr.package, "_golist", &factors);
        let pkg = match self
            .read_golist_package(Arc::clone(&req.executor), &golist_addr)
            .await
        {
            Ok(p) => p,
            Err(e) if downcast_chain_ref::<NoGoFilesError>(&e).is_some() => {
                return Err(GetError::NotFound);
            }
            Err(e) => return Err(GetError::Other(e)),
        };

        if pkg.dir.is_none() {
            return Err(GetError::Other(anyhow::anyhow!(
                "package '{}' has no Dir",
                import_path
            )));
        }

        // The module root drives which directory `go list` runs from for transitive deps.
        let module_root = match &*kind {
            GoPackageKind::FirstParty { module_root, .. } => module_root.clone(),
            GoPackageKind::ThirdParty { module_root, .. } => module_root.clone(),
            GoPackageKind::Stdlib { .. } => return Err(GetError::NotFound),
        };

        match addr.name.as_str() {
            "build_lib" => {
                // A library with no Go source files isn't buildable. Previously
                // caught by a shared `go_files.is_empty() && error.is_some()`
                // guard above; now the sentinel only fires on the NOGO case
                // (`error.is_some()`), so the no-source-but-test-files case
                // (e.g. test-only packages) needs its own guard here.
                if pkg.go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &[],
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let embed_addr = if !pkg.embed_patterns.is_empty() || !pkg.embed_files.is_empty() {
                    Some(self.make_addr_with_name(&addr.package, "embed", &factors))
                } else {
                    None
                };

                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                let spec = match &*kind {
                    GoPackageKind::ThirdParty { .. } => thirdparty::build_lib_spec(
                        addr.clone(),
                        &pkg,
                        &factors,
                        &transitive.libs,
                        &pkg_addrs.go_files,
                        &pkg_addrs.s_files,
                        &pkg_addrs.h_files,
                        embed_addr.as_ref(),
                        &pkg_addrs.embed_files,
                        &self.go_bin_addr,
                        &self.goroot,
                        &self.gocache,
                    ),
                    _ => target_lib::build_spec(
                        addr.clone(),
                        &import_path,
                        pkg.name.as_deref().unwrap_or(""),
                        &factors,
                        &transitive.libs,
                        &pkg_addrs.go_files,
                        &self.go_bin_addr,
                        &self.goroot,
                        &self.gocache,
                        embed_addr.as_ref(),
                        &pkg_addrs.embed_files,
                    ),
                };
                Ok(GetResponse { target_spec: spec })
            }
            "build" => {
                if pkg.name.as_deref() != Some("main") {
                    return Err(GetError::NotFound);
                }

                let own_lib_addr = self.build_lib_addr(addr, &factors);
                let mut transitive = Arc::clone(&self)
                    .collect_transitive_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &[],
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;
                transitive
                    .libs
                    .insert(0, (import_path.clone(), own_lib_addr));

                let spec = target_bin::build_spec(
                    addr.clone(),
                    &import_path,
                    &factors,
                    &transitive.libs,
                    &self.go_bin_addr,
                    &self.goroot,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Generates testmain.go for the INTERNAL test bin (only `_test` imports).
            // Internal and external (xtest) testmains are emitted separately so each
            // test bin's importcfg is consistent: internal needs P=build_test_lib,
            // xtest needs P=build_lib (normal) — combining them in one bin is what
            // creates the fingerprint mismatch on cycle cases.
            "testmain" => {
                if pkg.test_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                Ok(GetResponse {
                    target_spec: build_testmain_spec(
                        addr.clone(),
                        &golist_addr,
                        &pkg_addrs.test_go_files,
                        &[],
                    ),
                })
            }
            // Generates testmain.go for the EXTERNAL (xtest) test bin (only
            // `_xtest` imports). See `testmain` arm for the split rationale.
            "xtestmain" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                Ok(GetResponse {
                    target_spec: build_testmain_spec(
                        addr.clone(),
                        &golist_addr,
                        &[],
                        &pkg_addrs.xtest_go_files,
                    ),
                })
            }
            // Intermediate test target: compile GoFiles + TestGoFiles in test mode.
            "build_test_lib" => {
                let has_tests = !pkg.test_go_files.is_empty();
                let has_go = !pkg.go_files.is_empty();
                if !has_tests && !has_go {
                    return Err(GetError::NotFound);
                }
                let test_extra: Vec<String> = pkg.test_imports.clone();
                let transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &test_extra,
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;

                let has_any_embed = !pkg.embed_patterns.is_empty()
                    || !pkg.embed_files.is_empty()
                    || !pkg.test_embed_patterns.is_empty()
                    || !pkg.test_embed_files.is_empty();
                let embed_addr = if has_any_embed {
                    Some(self.make_addr_with_name(&addr.package, "embed_test", &factors))
                } else {
                    None
                };

                let mut test_embed_files = pkg_addrs.embed_files.clone();
                test_embed_files.extend(pkg_addrs.test_embed_files.iter().cloned());
                let spec = target_test::build_test_lib_spec(
                    addr.clone(),
                    &import_path,
                    pkg.name.as_deref().unwrap_or(""),
                    &factors,
                    &transitive.libs,
                    &pkg_addrs.go_files,
                    &pkg_addrs.test_go_files,
                    &self.go_bin_addr,
                    &self.goroot,
                    &self.gocache,
                    embed_addr.as_ref(),
                    &test_embed_files,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Intermediate test target: compile XTestGoFiles.
            "build_xtest_lib" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let xtest_imports_pkg = GoPackage {
                    import_path: format!("{}_test", import_path),
                    dir: pkg.dir.clone(),
                    name: pkg.name.clone(),
                    go_files: vec![],
                    s_files: vec![],
                    h_files: vec![],
                    test_go_files: vec![],
                    xtest_go_files: vec![],
                    embed_patterns: vec![],
                    embed_files: vec![],
                    test_embed_patterns: vec![],
                    test_embed_files: vec![],
                    xtest_embed_patterns: vec![],
                    xtest_embed_files: vec![],
                    imports: pkg.xtest_imports.clone(),
                    test_imports: vec![],
                    xtest_imports: vec![],
                    standard: false,
                    module: pkg.module.clone(),
                    match_: vec![],
                    incomplete: false,
                    error: None,
                };
                // xtest (`package P_test`) imports P as a normal external import.
                // Normally xtest_lib's compile uses P=normal build_lib so its
                // embedded fingerprint matches the xtest bin's link-time view
                // (which also uses P=normal — xtest doesn't need P's test
                // variant). That keeps the xtest cycle (Q→P) consistent.
                //
                // Test-only flavors break the "P=normal" rule because P has no
                // GoFiles, so build_lib doesn't exist:
                //   - go_files empty, test_go_files non-empty (test+xtest-only)
                //     → use build_test_lib for P. No cycle is possible because
                //     nothing can import P normally.
                //   - both empty (xtest-only) → drop P from importcfg; xtest
                //     source can't reference symbols that don't exist.
                let transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &xtest_imports_pkg,
                        &[],
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let p_lib_name = pick_xtest_p_lib_name(&pkg);
                let rewritten_libs: Vec<(String, Addr)> = transitive
                    .libs
                    .into_iter()
                    .filter_map(|(ip, a)| {
                        if ip != import_path {
                            return Some((ip, a));
                        }
                        p_lib_name.map(|name| {
                            (ip, self.make_addr_with_name(&addr.package, name, &factors))
                        })
                    })
                    .collect();

                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;

                let xtest_embed_addr =
                    if !pkg.xtest_embed_patterns.is_empty() || !pkg.xtest_embed_files.is_empty() {
                        Some(self.make_addr_with_name(&addr.package, "embed_xtest", &factors))
                    } else {
                        None
                    };

                let spec = target_test::build_xtest_lib_spec(
                    addr.clone(),
                    &import_path,
                    pkg.name.as_deref().unwrap_or(""),
                    &factors,
                    &rewritten_libs,
                    &pkg_addrs.xtest_go_files,
                    &self.go_bin_addr,
                    &self.goroot,
                    &self.gocache,
                    xtest_embed_addr.as_ref(),
                    &pkg_addrs.xtest_embed_files,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Compile the INTERNAL testmain.go (imports `_test "P"` only).
            // Direct imports: testmain stdlib + P (via test_lib).
            "build_testmain_lib" => {
                if pkg.test_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let testmain_imports = ["os", "reflect", "testing", "testing/internal/testdeps"];
                let testmain_pkg = make_testmain_pkg(&pkg, &testmain_imports);
                let mut transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &testmain_pkg,
                        &[],
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;
                // testmain imports `_test "P"` → importcfg needs P→test_lib.
                let test_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_test_lib", &factors);
                transitive.libs.push((import_path.clone(), test_lib_addr));

                let testmain_src_addr = Addr::new(
                    addr.package.clone(),
                    "testmain".to_string(),
                    factors_to_args(&factors),
                );
                let spec = target_test::build_testmain_lib_spec(
                    addr.clone(),
                    &factors,
                    &testmain_src_addr,
                    &transitive.libs,
                    &self.go_bin_addr,
                    &self.goroot,
                    &self.gocache,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Compile the EXTERNAL (xtest) testmain.go (imports `_xtest "P_test"` only).
            // Direct imports: testmain stdlib + P_test (via xtest_lib).
            "build_xtestmain_lib" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let testmain_imports = ["os", "reflect", "testing", "testing/internal/testdeps"];
                let testmain_pkg = make_testmain_pkg(&pkg, &testmain_imports);
                let mut transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &testmain_pkg,
                        &[],
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;
                // testmain imports `_xtest "P_test"` → importcfg needs P_test→xtest_lib.
                let xtest_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_xtest_lib", &factors);
                transitive
                    .libs
                    .push((format!("{}_test", import_path), xtest_lib_addr));

                let testmain_src_addr = Addr::new(
                    addr.package.clone(),
                    "xtestmain".to_string(),
                    factors_to_args(&factors),
                );
                let spec = target_test::build_testmain_lib_spec(
                    addr.clone(),
                    &factors,
                    &testmain_src_addr,
                    &transitive.libs,
                    &self.go_bin_addr,
                    &self.goroot,
                    &self.gocache,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Link the INTERNAL test bin.
            // importcfg: P=build_test_lib, transitive(P.imports ∪ P.test_imports)=build_lib.
            // Go rejects internal-test cycles, so no transitive importer of P appears here.
            "build_test" => {
                if pkg.test_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let test_extra: Vec<String> = pkg
                    .test_imports
                    .iter()
                    .chain(
                        ["os", "reflect", "testing", "testing/internal/testdeps"]
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect::<Vec<_>>()
                            .iter(),
                    )
                    .cloned()
                    .collect();
                let transitive = Arc::clone(&self)
                    .collect_transitive_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &test_extra,
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                // Assemble flat importcfg list: dedup by importpath, then add P→test_lib.
                let mut all_libs: Vec<(String, Addr)> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                for (ip, a) in transitive.libs {
                    if ip == import_path {
                        continue; // P slot reserved for test_lib below
                    }
                    if seen.insert(ip.clone()) {
                        all_libs.push((ip, a));
                    }
                }
                let test_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_test_lib", &factors);
                all_libs.push((import_path.clone(), test_lib_addr));

                let testmain_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_testmain_lib", &factors);
                let spec = target_test::build_test_spec(
                    addr.clone(),
                    &factors,
                    &self.go_bin_addr,
                    &target_test::GoEnv {
                        gomodcache: &self.gomodcache,
                        gopath: &self.gopath,
                        gocache: &self.gocache,
                    },
                    &testmain_lib_addr,
                    &all_libs,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Link the EXTERNAL (xtest) test bin.
            // importcfg: P=build_lib (NORMAL — xtest_lib was compiled against
            // normal P too, so all consumers of P agree on the same .a),
            // P_test=build_xtest_lib, transitive(P.xtest_imports ∪ P.imports)=build_lib.
            // Allows xtest cycle (bsfilter→bsquery) because every reference to P
            // resolves to the SAME normal .a.
            "build_xtest" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let xtest_extra: Vec<String> = pkg
                    .xtest_imports
                    .iter()
                    .chain(&pkg.imports)
                    .chain(
                        ["os", "reflect", "testing", "testing/internal/testdeps"]
                            .iter()
                            .map(|s| (*s).to_string())
                            .collect::<Vec<_>>()
                            .iter(),
                    )
                    .cloned()
                    .collect();
                // Walk from a synthetic pkg whose imports are xtest_extra; pkg.imports
                // are already in there, so the transitive closure covers everything
                // both P (normal) and P_test (xtest_lib) need.
                let xtest_root = GoPackage {
                    import_path: format!("{}_test", import_path),
                    dir: pkg.dir.clone(),
                    name: pkg.name.clone(),
                    go_files: vec![],
                    s_files: vec![],
                    h_files: vec![],
                    test_go_files: vec![],
                    xtest_go_files: vec![],
                    embed_patterns: vec![],
                    embed_files: vec![],
                    test_embed_patterns: vec![],
                    test_embed_files: vec![],
                    xtest_embed_patterns: vec![],
                    xtest_embed_files: vec![],
                    imports: xtest_extra.clone(),
                    test_imports: vec![],
                    xtest_imports: vec![],
                    standard: false,
                    module: pkg.module.clone(),
                    match_: vec![],
                    incomplete: false,
                    error: None,
                };
                let transitive = Arc::clone(&self)
                    .collect_transitive_libs(
                        Arc::clone(&req.executor),
                        &xtest_root,
                        &[],
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let mut all_libs: Vec<(String, Addr)> = Vec::new();
                let mut seen: HashSet<String> = HashSet::new();
                // P's flavor in xtest bin must match xtest_lib's compile-time
                // view (see `pick_xtest_p_lib_name`). For pure xtest-only
                // packages P has no lib at all — skip the slot entirely so we
                // don't request a non-existent target.
                if let Some(p_lib_name) = pick_xtest_p_lib_name(&pkg) {
                    let p_addr = self.make_addr_with_name(&addr.package, p_lib_name, &factors);
                    all_libs.push((import_path.clone(), p_addr));
                    seen.insert(import_path.clone());
                } else {
                    // Still reserve the slot so a transitive resolution of P
                    // (resolves to build_lib addr that doesn't exist) doesn't
                    // sneak into the importcfg.
                    seen.insert(import_path.clone());
                }
                let p_test = format!("{}_test", import_path);
                seen.insert(p_test.clone()); // reserve for xtest_lib below
                for (ip, a) in transitive.libs {
                    if seen.insert(ip.clone()) {
                        all_libs.push((ip, a));
                    }
                }
                let xtest_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_xtest_lib", &factors);
                all_libs.push((p_test, xtest_lib_addr));

                let testmain_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_xtestmain_lib", &factors);
                let spec = target_test::build_test_spec(
                    addr.clone(),
                    &factors,
                    &self.go_bin_addr,
                    &target_test::GoEnv {
                        gomodcache: &self.gomodcache,
                        gopath: &self.gopath,
                        gocache: &self.gocache,
                    },
                    &testmain_lib_addr,
                    &all_libs,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Run the INTERNAL test bin.
            "test" => {
                if pkg.test_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let build_test_addr =
                    self.make_addr_with_name(&addr.package, "build_test", &factors);
                let data_query_addr = hmodel::htaddr::parse_addr(&format!(
                    "//{}:q@package={},label=go_test_data",
                    hplugin_query::pluginquery::PACKAGE,
                    addr.package.as_str(),
                ))
                .context("build go_test_data query addr")
                .map_err(GetError::Other)?;
                let spec = target_test::test_spec(addr.clone(), build_test_addr, &data_query_addr);
                Ok(GetResponse { target_spec: spec })
            }
            // Run the EXTERNAL (xtest) test bin.
            "xtest" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let build_xtest_addr =
                    self.make_addr_with_name(&addr.package, "build_xtest", &factors);
                let data_query_addr = hmodel::htaddr::parse_addr(&format!(
                    "//{}:q@package={},label=go_test_data",
                    hplugin_query::pluginquery::PACKAGE,
                    addr.package.as_str(),
                ))
                .context("build go_test_data query addr")
                .map_err(GetError::Other)?;
                let spec = target_test::test_spec(addr.clone(), build_xtest_addr, &data_query_addr);
                Ok(GetResponse { target_spec: spec })
            }
            "embed" => {
                if pkg.embed_patterns.is_empty() && pkg.embed_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                Ok(GetResponse {
                    target_spec: build_embed_spec(addr.clone(), &golist_addr, "embed"),
                })
            }
            "embed_test" => {
                let has_any = !pkg.embed_patterns.is_empty()
                    || !pkg.embed_files.is_empty()
                    || !pkg.test_embed_patterns.is_empty()
                    || !pkg.test_embed_files.is_empty();
                if !has_any {
                    return Err(GetError::NotFound);
                }
                Ok(GetResponse {
                    target_spec: build_embed_spec(addr.clone(), &golist_addr, "test_embed"),
                })
            }
            "embed_xtest" => {
                if pkg.xtest_embed_patterns.is_empty() && pkg.xtest_embed_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                Ok(GetResponse {
                    target_spec: build_embed_spec(addr.clone(), &golist_addr, "xtest_embed"),
                })
            }
            _ => Err(GetError::NotFound),
        }
    }

    /// Generate the `_golist` target spec without executing go list.
    fn get_golist_spec(
        &self,
        addr: Addr,
        kind: &GoPackageKind,
        factors: &Factors,
        states: &[State],
    ) -> anyhow::Result<GetResponse> {
        let spec = match kind {
            GoPackageKind::FirstParty {
                import_path,
                module_root,
                ..
            } => {
                let module_root_rel = module_root
                    .strip_prefix(&self.workspace_root)
                    .unwrap_or(module_root);
                let go_mod_addr = Addr::new(
                    hmodel::htpkg::PkgBuf::from(module_root_rel.to_string_lossy().as_ref()),
                    "_go_mod".to_string(),
                    Default::default(),
                );
                // Use a pluginfs glob directly instead of _go_src: _go_src spec
                // generation calls executor.result(_golist), which would deadlock.
                let pkg = addr.package.as_str();
                let src_glob = if pkg.is_empty() {
                    "*.go".to_string()
                } else {
                    format!("{}/*.go", pkg)
                };
                let go_src_glob_addr = pluginfs::glob_addr(&src_glob, &[]);
                // Non-Go src tree + go_src codegen query + go_codegen_deps — needed
                // so `go list` can resolve //go:embed patterns into EmbedFiles, and
                // shared with the downstream `embed` target.
                let extra_src_addrs = compute_pkg_src_addrs(pkg, states)?;
                target_golist::build_spec_firstparty(
                    addr,
                    import_path,
                    factors,
                    &self.goroot,
                    &go_mod_addr,
                    &go_src_glob_addr,
                    None,
                    &extra_src_addrs,
                )?
            }
            GoPackageKind::ThirdParty {
                module,
                version,
                subpath,
                module_root,
            } => {
                let import_path = if subpath.is_empty() {
                    module.clone()
                } else {
                    format!("{}/{}", module, subpath)
                };
                let module_root_rel = module_root
                    .strip_prefix(&self.workspace_root)
                    .unwrap_or(module_root);
                let go_mod_addr = Addr::new(
                    hmodel::htpkg::PkgBuf::from(module_root_rel.to_string_lossy().as_ref()),
                    "_go_mod".to_string(),
                    Default::default(),
                );
                let base_pkg = module_root_rel.to_string_lossy();
                let download_addr = encode_thirdparty_download(module, version, &base_pkg);
                target_golist::build_spec_thirdparty(
                    addr,
                    &import_path,
                    factors,
                    &self.goroot,
                    &go_mod_addr,
                    &download_addr,
                )?
            }
            GoPackageKind::Stdlib { import_path } => {
                target_golist::build_spec_stdlib(addr, import_path, factors, &self.goroot)?
            }
        };
        Ok(GetResponse { target_spec: spec })
    }

    fn get_stdlib(
        &self,
        addr: Addr,
        import_path: &str,
        factors: &Factors,
    ) -> Result<GetResponse, GetError> {
        match addr.name.as_str() {
            "build_lib" => {
                let spec = target_std::build_spec(
                    addr,
                    import_path,
                    factors,
                    &self.go_bin_addr,
                    &self.goroot,
                );
                Ok(GetResponse { target_spec: spec })
            }
            _ => Err(GetError::NotFound),
        }
    }

    /// Read and parse the single package from a `_golist` target's output artifact.
    ///
    /// `executor.result(golist_addr)` is called OUTSIDE the `once` closure so
    /// every caller (owner + cache-hit waiters) routes through
    /// `Engine::result_addr`, which registers the `parent → golist_addr` edge
    /// in the request's `DepDag`. Memoizing the executor call inside the
    /// closure would let waiters skip dep registration, hiding a real target-
    /// dep cycle as a memoizer deadlock. The cache only memoizes the artifact
    /// parse, which is the expensive part.
    async fn read_golist_package(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        golist_addr: &Addr,
    ) -> anyhow::Result<Arc<GoPackage>> {
        // Every caller registers its `parent → golist_addr` edge (the host's
        // cycle check) with the cheap edge-only `note_dep`. The expensive data
        // fetch (`executor.result` + parse) is deduped to a single call per
        // distinct golist via `pkg_cache` — so N importers of a shared package
        // cost N cheap note_deps plus one `result()`, not N full `result()`
        // round-trips (each of which re-runs the engine result pipeline + streams
        // the artifact). This is the dominant cost on the remote resolve path.
        executor.note_dep(golist_addr).await?;
        self.pkg_cache
            .once(
                golist_addr.clone(),
                enclose!((executor, golist_addr) move || async move {
                    let result = executor.result(&golist_addr).await?;
                    let pkg = hproc::process_supervisor::block_or_inline(move || -> anyhow::Result<_> {
                        for artifact in &result.artifacts {
                            for entry_result in artifact.walk()? {
                                let entry = entry_result?;
                                if entry.path.file_name().and_then(|n| n.to_str())
                                    != Some("package.bin")
                                {
                                    continue;
                                }
                                let data = match entry.kind {
                                    hcore::hartifactcontent::WalkEntryKind::File { data, .. } => data,
                                    hcore::hartifactcontent::WalkEntryKind::Symlink { .. } => continue,
                                };
                                return decode_go_package(data);
                            }
                        }
                        anyhow::bail!("_golist produced no package.bin")
                    })?;
                    // `go list -e` reports no-buildable-files cases as a JSON
                    // entry with the Error field populated and empty GoFiles.
                    // Surface that as a typed sentinel so consumers can map it
                    // to NotFound uniformly (mirrors errNoGoFiles in the Go
                    // reference impl: heph/plugin/plugingo/pkg_analysis.go:34).
                    //
                    // Test-only packages (only `package pkg_test` xtest files)
                    // are NOT NOGO — go list -test reports a synthetic primary
                    // entry without an Error in that case, so xtest variants
                    // remain reachable.
                    if pkg.go_files.is_empty() && pkg.error.is_some() {
                        return Err(anyhow::Error::new(NoGoFilesError {
                            import_path: pkg.import_path.clone(),
                        }));
                    }
                    Ok(Arc::new(pkg))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    async fn read_golist_package_addrs(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        golist_addr: &Addr,
    ) -> anyhow::Result<Arc<PackageAddrs>> {
        // Cheap edge-only note_dep for every caller; data result() deduped to one
        // per distinct golist (see read_golist_package).
        executor.note_dep(golist_addr).await?;
        self.pkg_addrs_cache
            .once(
                golist_addr.clone(),
                enclose!((executor, golist_addr) move || async move {
                    let result = executor.result(&golist_addr).await?;
                    let addrs = hproc::process_supervisor::block_or_inline(move || -> anyhow::Result<_> {
                        for artifact in &result.artifacts {
                            for entry_result in artifact.walk()? {
                                let entry = entry_result?;
                                if entry.path.file_name().and_then(|n| n.to_str())
                                    != Some("package_addrs.bin")
                                {
                                    continue;
                                }
                                let data = match entry.kind {
                                    hcore::hartifactcontent::WalkEntryKind::File { data, .. } => data,
                                    hcore::hartifactcontent::WalkEntryKind::Symlink { .. } => continue,
                                };
                                return decode_package_addrs(data);
                            }
                        }
                        anyhow::bail!("_golist produced no package_addrs.bin")
                    })?;
                    Ok(Arc::new(addrs))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    fn build_lib_addr(&self, addr: &Addr, factors: &Factors) -> Addr {
        self.make_addr_with_name(&addr.package, "build_lib", factors)
    }

    fn make_addr_with_name(
        &self,
        package: &hmodel::htpkg::PkgBuf,
        name: &str,
        factors: &Factors,
    ) -> Addr {
        Addr::new(package.clone(), name.to_string(), factors_to_args(factors))
    }

    /// Build the cache key for `collect_*_libs`. Imports are sorted+deduped so
    /// distinct caller-side orderings of the same logical input set hash to one entry.
    fn libs_key(
        root_pkg: &GoPackage,
        extra_imports: &[String],
        factors: &Factors,
        module_root: &Path,
        transitive: bool,
    ) -> LibsKey {
        let mut imports: Vec<String> = root_pkg.imports.clone();
        imports.sort();
        imports.dedup();
        let mut extra: Vec<String> = extra_imports.to_vec();
        extra.sort();
        extra.dedup();
        LibsKey {
            imports,
            extra,
            factors: factors.clone(),
            module_root: module_root.to_path_buf(),
            transitive,
        }
    }

    /// Collect all transitive lib addresses for a package's imports, recursively.
    ///
    /// Each BFS frontier is processed concurrently via `try_join_all`. Each dep's
    /// `_golist` target is fetched via `executor.result` (engine pipeline with disk
    /// cache), memoized in `pkg_cache` for the Provider lifetime so repeated calls
    /// for the same dep cost nothing after the first resolution.
    ///
    /// The full BFS result is itself memoized via `libs_cache`, deduping calls
    /// across `build_lib`/`build`/`build_test`/`build_test_lib` etc. for the same
    /// root pkg + factors within a single Provider lifetime.
    async fn collect_transitive_libs(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        root_pkg: &GoPackage,
        extra_imports: &[String],
        factors: &Factors,
        module_root: &Path,
    ) -> anyhow::Result<TransitiveDeps> {
        self.collect_libs(
            executor,
            root_pkg,
            extra_imports,
            factors,
            module_root,
            true,
        )
        .await
    }

    /// Resolve direct imports only (no recursion) — correct for compile steps.
    async fn collect_direct_libs(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        root_pkg: &GoPackage,
        extra_imports: &[String],
        factors: &Factors,
        module_root: &Path,
    ) -> anyhow::Result<TransitiveDeps> {
        self.collect_libs(
            executor,
            root_pkg,
            extra_imports,
            factors,
            module_root,
            false,
        )
        .await
    }

    async fn collect_libs(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        root_pkg: &GoPackage,
        extra_imports: &[String],
        factors: &Factors,
        module_root: &Path,
        transitive: bool,
    ) -> anyhow::Result<TransitiveDeps> {
        let key = Self::libs_key(root_pkg, extra_imports, factors, module_root, transitive);
        let extra = extra_imports.to_vec();
        let module_root = module_root.to_path_buf();
        let arc = self
            .libs_cache
            .once(
                key,
                enclose!((self => me, executor, factors, root_pkg.imports => root_imports) move || async move {
                    me.collect_libs_inner(
                        executor,
                        &root_imports,
                        &extra,
                        &factors,
                        &module_root,
                        transitive,
                    )
                    .await
                    .map(Arc::new)
                }),
            )
            .await
            .map_err(unwrap_arc_err)?;
        Ok((*arc).clone())
    }

    fn load_go_mod(&self, module_root: &Path) -> anyhow::Result<Arc<GoModData>> {
        let go_mod_path = module_root.join("go.mod");
        if let Some(hit) = self.go_mod_cache.read().get(&go_mod_path) {
            return Ok(Arc::clone(hit));
        }
        let data = if go_mod_path.exists() {
            let content = hproc::process_supervisor::block_or_inline(
                enclose!((go_mod_path) move || std::fs::read_to_string(&go_mod_path)),
            )
            .with_context(|| format!("reading {}", go_mod_path.display()))?;

            // Start from go.mod's explicit requires, then fill in modules that
            // appear only in go.sum. An untidied go.mod (pre-1.17 / never
            // tidied) omits indirect requires, so a thirdparty package's
            // transitive imports (e.g. golang.org/x/net behind oauth2) would
            // otherwise fail to resolve to a module version and get dropped from
            // the importcfg. go.mod entries win on conflict — they carry the
            // authoritative/replace intent — so go.sum only adds modules go.mod
            // doesn't already pin.
            let mut requires = parse_go_mod_requires(&content);
            let go_sum_path = module_root.join("go.sum");
            if go_sum_path.exists() {
                let sum_content = hproc::process_supervisor::block_or_inline(
                    enclose!((go_sum_path) move || std::fs::read_to_string(&go_sum_path)),
                )
                .with_context(|| format!("reading {}", go_sum_path.display()))?;
                let known: std::collections::HashSet<&str> =
                    requires.iter().map(|(m, _)| m.as_str()).collect();
                let extra: Vec<(String, String)> = parse_go_sum_modules(&sum_content)
                    .into_iter()
                    .filter(|(m, _)| !known.contains(m.as_str()))
                    .collect();
                requires.extend(extra);
            }

            Arc::new(GoModData {
                requires,
                module_path: parse_go_mod_module_path(&content).unwrap_or_default(),
            })
        } else {
            Arc::new(GoModData {
                requires: Vec::new(),
                module_path: String::new(),
            })
        };
        let mut w = self.go_mod_cache.write();
        Ok(Arc::clone(
            w.entry(go_mod_path).or_insert_with(|| Arc::clone(&data)),
        ))
    }

    /// Memoized per-import_path transitive closure.
    ///
    /// Returns `import_path`'s lib (if any) plus the transitive closure of its
    /// sub-imports, in deps-first order with import_paths deduped. Cached by
    /// `(import_path, factors, module_root)` — so a hot dep like `fmt` is walked
    /// once per request even if hundreds of top-level targets reach it.
    ///
    /// Recurses via `try_join_all` over each sub-import; each recursive call
    /// hits the same cache, so the work for any subtree is amortized.
    #[async_recursion]
    async fn import_closure(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        import_path: String,
        factors: Factors,
        go_mod: Arc<GoModData>,
        module_root: PathBuf,
    ) -> anyhow::Result<Arc<ImportClosure>> {
        let key = ImportClosureKey {
            import_path: import_path.clone(),
            factors: factors.clone(),
            module_root: module_root.clone(),
        };
        self.import_closure_cache
            .once(
                key,
                enclose!((self => me, executor, import_path, factors, go_mod, module_root) move || async move {
                    let (resolved_path, dep_addr_opt, sub_imports) = me
                        .resolve_import(
                            Arc::clone(&executor),
                            &import_path,
                            &factors,
                            &go_mod.requires,
                            &go_mod.module_path,
                            &module_root,
                        )
                        .await?;

                    let sub_closures = try_join_all(
                        sub_imports
                            .into_iter()
                            .filter(|s| s != "unsafe" && s != "C")
                            .map(|sub| {
                                Arc::clone(&me).import_closure(
                                    Arc::clone(&executor),
                                    sub,
                                    factors.clone(),
                                    Arc::clone(&go_mod),
                                    module_root.clone(),
                                )
                            }),
                    )
                    .await?;

                    // Deps first, then self (deduped by import_path so a diamond
                    // dependency only shows once).
                    let self_lib =
                        dep_addr_opt.map(|addr| (Arc::<str>::from(resolved_path), addr));
                    let libs = compose_closures(&sub_closures, self_lib);

                    anyhow::Ok(Arc::new(ImportClosure { libs }))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    async fn collect_libs_inner(
        self: Arc<Self>,
        executor: Arc<dyn ProviderExecutor>,
        root_imports: &[String],
        extra_imports: &[String],
        factors: &Factors,
        module_root: &Path,
        transitive: bool,
    ) -> anyhow::Result<TransitiveDeps> {
        let go_mod = self.load_go_mod(module_root)?;

        if transitive {
            // Pre-dedupe the top-level set so we don't fan out the same
            // import_path twice (also halves cache lookups for repeated entries
            // between `root_imports` and `extra_imports`).
            let unique_imports: Vec<String> = root_imports
                .iter()
                .chain(extra_imports.iter())
                .filter(|i| *i != "unsafe" && *i != "C")
                .cloned()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();

            let module_root_buf = module_root.to_path_buf();
            let sub_closures = try_join_all(unique_imports.into_iter().map(|ip| {
                Arc::clone(&self).import_closure(
                    Arc::clone(&executor),
                    ip,
                    factors.clone(),
                    Arc::clone(&go_mod),
                    module_root_buf.clone(),
                )
            }))
            .await?;

            // Compose the root sub-closures into one deps-first deduped set, then
            // materialize owned `String`s once for `TransitiveDeps` — O(closure)
            // String allocations total rather than O(closure) per node.
            let libs = compose_closures(&sub_closures, None)
                .into_iter()
                .map(|(ip, addr)| (ip.to_string(), addr))
                .collect();
            return Ok(TransitiveDeps { libs });
        }

        let go_mod_requires = &go_mod.requires;
        let workspace_module_path = go_mod.module_path.as_str();

        let imports: Vec<String> = root_imports
            .iter()
            .chain(extra_imports.iter())
            .filter(|i| *i != "unsafe" && *i != "C")
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        let results = try_join_all(imports.iter().map(|ip| {
            self.resolve_import(
                Arc::clone(&executor),
                ip,
                factors,
                go_mod_requires,
                workspace_module_path,
                module_root,
            )
        }))
        .await?;

        let libs = results
            .into_iter()
            .filter_map(|(import_path, addr_opt, _sub)| addr_opt.map(|a| (import_path, a)))
            .collect();

        Ok(TransitiveDeps { libs })
    }

    /// Resolve one import path: returns `(import_path, Option<lib Addr>, sub_imports)`.
    async fn resolve_import(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        import_path: &str,
        factors: &Factors,
        go_mod_requires: &[(String, String)],
        workspace_module_path: &str,
        module_root: &Path,
    ) -> anyhow::Result<(String, Option<Addr>, Vec<String>)> {
        let is_workspace_module = !workspace_module_path.is_empty()
            && (import_path == workspace_module_path
                || import_path.starts_with(&format!("{}/", workspace_module_path)));
        if !is_workspace_module && is_stdlib_import_path(import_path) {
            let addr = encode_stdlib(import_path, factors);
            let golist_addr = Addr::new(
                hmodel::htpkg::PkgBuf::from(format!("@heph/go/std/{}", import_path)),
                "_golist".to_string(),
                factors_to_args(factors),
            );
            // Propagate golist errors instead of swallowing them: a missing or
            // partial closure here turns into a broken link step downstream
            // ("cannot find package errors (using -importcfg)") that's
            // impossible to root-cause from the user's side.
            let sub_imports = self
                .read_golist_package(Arc::clone(&executor), &golist_addr)
                .await
                .map(|pkg| pkg.imports.clone())
                .with_context(|| format!("read _golist for stdlib {}", import_path))?;
            return Ok((import_path.to_string(), Some(addr), sub_imports));
        }

        let dep_addr = match self.resolve_import_to_addr(
            import_path,
            factors,
            module_root,
            workspace_module_path,
            go_mod_requires,
        ) {
            Some(a) => a,
            None => return Ok((import_path.to_string(), None, vec![])),
        };

        let golist_addr = Addr::new(
            dep_addr.package.clone(),
            "_golist".to_string(),
            dep_addr.args.clone(),
        );

        let sub_imports = match self.read_golist_package(executor, &golist_addr).await {
            Ok(p) => p.imports.clone(),
            // The dep's directory has Go files but none are buildable for these
            // factors (all excluded by build constraints). Its `build_lib`
            // resolves to NotFound (see the NoGoFilesError arm in `handle_get`),
            // so there's no lib to link against — drop the import instead of
            // failing the importer's get_spec, exactly as an unresolvable import
            // returns `None` above.
            Err(e) if downcast_chain_ref::<NoGoFilesError>(&e).is_some() => {
                return Ok((import_path.to_string(), None, vec![]));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("read _golist for {}", import_path));
            }
        };

        Ok((import_path.to_string(), Some(dep_addr), sub_imports))
    }

    /// Resolve an import path to a heph `build_lib` Addr.
    fn resolve_import_to_addr(
        &self,
        import_path: &str,
        factors: &Factors,
        module_root: &Path,
        workspace_module_path: &str,
        go_mod_requires: &[(String, String)],
    ) -> Option<Addr> {
        // Check if it's a first-party import (in the workspace module).
        // We use workspace_module_path (from go.mod) rather than root_pkg.module.path
        // because root_pkg may itself be a third-party package — its sub-packages must
        // still be resolved as third-party, not mapped into the workspace.
        if !workspace_module_path.is_empty()
            && (import_path == workspace_module_path
                || import_path.starts_with(&format!("{}/", workspace_module_path)))
        {
            let rel_suffix = import_path
                .strip_prefix(workspace_module_path)
                .and_then(|s| s.strip_prefix('/'))
                .unwrap_or("");
            let module_rel = module_root
                .strip_prefix(&self.workspace_root)
                .unwrap_or(module_root);
            let src_dir = if rel_suffix.is_empty() {
                self.workspace_root.join(module_rel)
            } else {
                self.workspace_root.join(module_rel).join(rel_suffix)
            };
            return Some(encode_firstparty(&src_dir, &self.workspace_root, factors));
        }

        // Third-party: look up in go.mod requires
        if let Some((mod_path, version)) = find_module_for_import(import_path, go_mod_requires) {
            let subpath = import_path
                .strip_prefix(&mod_path)
                .and_then(|s| s.strip_prefix('/'))
                .unwrap_or("")
                .to_string();
            let base_pkg = module_root
                .strip_prefix(&self.workspace_root)
                .unwrap_or_else(|_| Path::new(""))
                .to_string_lossy()
                .to_string();
            return Some(encode_thirdparty(
                &mod_path, &version, &subpath, &base_pkg, factors,
            ));
        }

        None
    }
}

fn build_embed_spec(
    addr: Addr,
    golist_addr: &Addr,
    variant: &str,
) -> hplugin::provider::TargetSpec {
    use hcore::htvalue::Value;
    use std::collections::HashMap;

    let golist_dep = format!("{}|pkg", golist_addr.format());

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("variant".to_string(), Value::String(variant.to_string()));
    let mut deps_map: HashMap<String, Value> = HashMap::new();
    deps_map.insert(
        "golist".to_string(),
        Value::List(vec![Value::String(golist_dep)]),
    );
    config.insert("deps".to_string(), Value::Map(deps_map));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "cfg".to_string(),
            Value::List(vec![Value::String("embedcfg".to_string())]),
        )])),
    );

    hplugin::provider::TargetSpec {
        addr,
        driver: "go_embed".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

/// Build a synthetic `GoPackage` representing the `main` package of a
/// testmain.go file. Only `imports` is set — used to drive
/// `collect_direct_libs` resolution.
fn make_testmain_pkg(pkg: &GoPackage, imports: &[&str]) -> GoPackage {
    GoPackage {
        import_path: "main".to_string(),
        dir: pkg.dir.clone(),
        name: Some("main".to_string()),
        go_files: vec![],
        s_files: vec![],
        h_files: vec![],
        test_go_files: vec![],
        xtest_go_files: vec![],
        embed_patterns: vec![],
        embed_files: vec![],
        test_embed_patterns: vec![],
        test_embed_files: vec![],
        xtest_embed_patterns: vec![],
        xtest_embed_files: vec![],
        imports: imports.iter().map(|s| (*s).to_string()).collect(),
        test_imports: vec![],
        xtest_imports: vec![],
        standard: false,
        module: pkg.module.clone(),
        match_: vec![],
        incomplete: false,
        error: None,
    }
}

fn build_testmain_spec(
    addr: Addr,
    golist_addr: &Addr,
    test_file_addrs: &[String],
    xtest_file_addrs: &[String],
) -> hplugin::provider::TargetSpec {
    use hcore::htvalue::Value;
    use std::collections::HashMap;

    let golist_dep = format!("{}|pkg", golist_addr.format());

    let mut deps_map: HashMap<String, Value> = HashMap::new();
    deps_map.insert(
        "golist".to_string(),
        Value::List(vec![Value::String(golist_dep)]),
    );
    if !test_file_addrs.is_empty() {
        deps_map.insert(
            "test".to_string(),
            Value::List(
                test_file_addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !xtest_file_addrs.is_empty() {
        deps_map.insert(
            "xtest".to_string(),
            Value::List(
                xtest_file_addrs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }

    // Tell the `go_testmain` driver which file set to analyze. Without `mode`
    // it analyzes BOTH test_go_files and xtest_go_files from the golist, which
    // breaks the split-bin design (xtestmain tries to open internal test files
    // that weren't staged → "No such file or directory").
    let mode = if !test_file_addrs.is_empty() && !xtest_file_addrs.is_empty() {
        "both"
    } else if !xtest_file_addrs.is_empty() {
        "xtest"
    } else {
        "internal"
    };

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("deps".to_string(), Value::Map(deps_map));
    config.insert("mode".to_string(), Value::String(mode.to_string()));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            "go".to_string(),
            Value::List(vec![Value::String("testmain.go".to_string())]),
        )])),
    );

    hplugin::provider::TargetSpec {
        addr,
        driver: "go_testmain".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

use hplugin::provider::{ProbeRequest, ProbeResponse};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugingo::addr_util::decode_package;
    use crate::plugingo::factors::Factors;
    use crate::plugingo::pkg_analysis::run_go_list;
    use anyhow::Context;
    use futures::future::BoxFuture;
    use hcore::hartifactcontent::{Content, WalkEntry, WalkEntryKind};
    use hcore::hasync::StdCancellationToken;
    use hcore::htvalue::Value;
    use hmodel::htpkg::PkgBuf;
    use hplugin::eresult::{ArtifactMeta, EResult};
    use hplugin::provider::{GetError, GetRequest, Provider as ProviderTrait};
    use std::collections::HashMap;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn build_addr_ctx() -> FnCallContext<'static> {
        FnCallContext {
            pkg: "",
            root: std::path::Path::new("/"),
        }
    }

    #[tokio::test]
    async fn test_build_addr_basic() {
        let args = FnArgs {
            positional: vec![
                Value::String("mylib".into()),
                Value::String("linux".into()),
                Value::String("amd64".into()),
            ],
            named: HashMap::new(),
        };
        let v = BuildAddrFn.call(&build_addr_ctx(), args).await.unwrap();
        assert_eq!(
            v,
            Value::String("//mylib:build@goarch=amd64,goos=linux".into())
        );
    }

    #[tokio::test]
    async fn test_build_addr_tags() {
        let mut named = HashMap::new();
        // Unsorted on input — must come out sorted (bar,foo) in the addr.
        named.insert(
            "tags".to_string(),
            Value::List(vec![
                Value::String("foo".into()),
                Value::String("bar".into()),
            ]),
        );
        let args = FnArgs {
            positional: vec![
                Value::String("@heph/go/std/fmt".into()),
                Value::String("darwin".into()),
                Value::String("arm64".into()),
            ],
            named,
        };
        let v = BuildAddrFn.call(&build_addr_ctx(), args).await.unwrap();
        assert_eq!(
            v,
            Value::String(
                "//@heph/go/std/fmt:build@goarch=arm64,goos=darwin,tags=\"bar,foo\"".into()
            )
        );
    }

    #[tokio::test]
    async fn test_build_addr_named_args() {
        let mut named = HashMap::new();
        named.insert("pkg".to_string(), Value::String("mylib".into()));
        named.insert("goos".to_string(), Value::String("linux".into()));
        named.insert("goarch".to_string(), Value::String("amd64".into()));
        let args = FnArgs {
            positional: vec![],
            named,
        };
        let v = BuildAddrFn.call(&build_addr_ctx(), args).await.unwrap();
        assert_eq!(
            v,
            Value::String("//mylib:build@goarch=amd64,goos=linux".into())
        );
    }

    #[tokio::test]
    async fn test_build_addr_missing_goarch_errors() {
        let args = FnArgs {
            positional: vec![Value::String("mylib".into()), Value::String("linux".into())],
            named: HashMap::new(),
        };
        let err = BuildAddrFn.call(&build_addr_ctx(), args).await.unwrap_err();
        assert!(err.to_string().contains("missing `goarch`"), "{err}");
    }

    fn run_str(spec: &hplugin::provider::TargetSpec) -> String {
        match spec.config.get("run").unwrap() {
            Value::String(s) => s.clone(),
            Value::List(v) => v
                .iter()
                .map(|x| match x {
                    Value::String(s) => s.as_str(),
                    _ => panic!("run entry not a string"),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => panic!("run not string or list"),
        }
    }

    fn go_available() -> bool {
        std::process::Command::new("go")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    macro_rules! require_go {
        () => {
            if !go_available() {
                eprintln!("skipping: go not in PATH");
                return;
            }
        };
    }

    // --- Test executor that resolves _golist by running go list directly ---

    struct GoListTestExecutor {
        workspace_root: PathBuf,
        /// Source map applied when generating `package_addrs.bin`.
        source_map: HashMap<String, String>,
    }

    struct BinaryArtifact {
        path: PathBuf,
        bytes: Vec<u8>,
        hashout: String,
    }

    impl Content for BinaryArtifact {
        fn reader(&self) -> anyhow::Result<Box<dyn io::Read>> {
            Ok(Box::new(io::Cursor::new(self.bytes.clone())))
        }

        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            let entry = WalkEntry {
                path: self.path.clone(),
                kind: WalkEntryKind::File {
                    data: Box::new(io::Cursor::new(self.bytes.clone())),
                    x: false,
                },
            };
            Ok(Box::new(std::iter::once(Ok(entry))))
        }

        fn hashout(&self) -> anyhow::Result<String> {
            Ok(self.hashout.clone())
        }
    }

    impl ProviderExecutor for GoListTestExecutor {
        fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
            Box::pin(async move {
                if addr.name != "_golist" {
                    anyhow::bail!(
                        "GoListTestExecutor: only handles _golist, got: {}",
                        addr.format()
                    );
                }

                let factors = Factors::from_addr(addr);
                let kind = decode_package(&addr.package, &self.workspace_root)
                    .ok_or_else(|| anyhow::anyhow!("unknown package: {}", addr.package))?;

                let (import_path, run_dir) = match &*kind {
                    GoPackageKind::FirstParty {
                        import_path,
                        module_root,
                        ..
                    } => (import_path.clone(), module_root.clone()),
                    GoPackageKind::ThirdParty {
                        module,
                        subpath,
                        module_root,
                        ..
                    } => {
                        let ip = if subpath.is_empty() {
                            module.clone()
                        } else {
                            format!("{}/{}", module, subpath)
                        };
                        (ip, module_root.clone())
                    }
                    GoPackageKind::Stdlib { import_path } => {
                        // Stdlib packages don't need a module root; run go list from workspace.
                        (import_path.clone(), self.workspace_root.clone())
                    }
                };

                // Run go list (no -deps) for just this package
                let packages = run_go_list(&import_path, &factors, &run_dir).await?;
                // `-test` returns multiple variants; pick the canonical entry whose
                // ImportPath matches the request.
                let pkg = packages.get(&import_path).cloned().ok_or_else(|| {
                    anyhow::anyhow!("go list returned no entry for {}", import_path)
                })?;
                let pkg_bin = crate::plugingo::pkg_analysis::encode_go_package(&pkg)
                    .context("encode package.bin")?;

                // Mirror the real driver: also emit package_addrs.bin so the provider
                // can resolve per-file addrs without re-running the driver.
                let addrs = crate::plugingo::pkg_analysis::resolve_package_addrs(
                    &pkg,
                    addr.package.as_str(),
                    &self.source_map,
                    None,
                );
                let addrs_bin = crate::plugingo::pkg_analysis::encode_package_addrs(&addrs)
                    .context("encode package_addrs.bin")?;

                let artifacts: Vec<Arc<dyn Content>> = vec![
                    Arc::new(BinaryArtifact {
                        path: PathBuf::from("package.bin"),
                        bytes: pkg_bin,
                        hashout: "test_hashout".to_string(),
                    }) as Arc<dyn Content>,
                    Arc::new(BinaryArtifact {
                        path: PathBuf::from("package_addrs.bin"),
                        bytes: addrs_bin,
                        hashout: "test_hashout_addrs".to_string(),
                    }) as Arc<dyn Content>,
                ];

                Ok(Arc::new(EResult {
                    artifacts_meta: artifacts
                        .iter()
                        .map(|_| ArtifactMeta {
                            hashout: "test_hashout".to_string(),
                        })
                        .collect(),
                    artifacts,
                    support_artifacts: vec![],
                }))
            })
        }

        fn query<'a>(
            &'a self,
            _m: &'a hmodel::htmatcher::Matcher,
            _extra_skip: &'a [String],
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    fn test_executor(workspace_root: &std::path::Path) -> Arc<dyn ProviderExecutor> {
        Arc::new(GoListTestExecutor {
            workspace_root: workspace_root.to_path_buf(),
            source_map: HashMap::new(),
        })
    }

    /// Copy a testdata fixture directory to a fresh tempdir sandbox.
    fn copy_fixture(name: &str) -> tempfile::TempDir {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/plugingo/testdata")
            .join(name);
        let tmp = tempfile::tempdir().unwrap();
        copy_dir_all(&src, tmp.path()).unwrap();
        tmp
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
            } else {
                std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    fn make_addr(package: &str, name: &str) -> Addr {
        Addr::new(PkgBuf::from(package), name.to_string(), Default::default())
    }

    #[test]
    fn collect_go_packages_respects_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("go.mod"), "module example.com/x\n").unwrap();
        // root/heph-home (core skip dir, non-dotted so the dot-rule can't mask
        // it), root/internal (glob skip), root/src (kept).
        let home = root.join("heph-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(root.join("internal")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        // Built-in (byte-compared) prunes: dot/underscore prefixes and the
        // go-convention `vendor` / `testdata` dirs.
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::create_dir_all(root.join("_ignored")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::create_dir_all(root.join("testdata")).unwrap();

        let skip = Ignore::new(&[home.clone()], &["internal".to_string()]).unwrap();
        let walker = CachedWalker::disabled();
        let mut out = Vec::new();
        collect_go_packages(&walker, root, root, false, &skip, &mut out);
        let pkgs: Vec<String> = out
            .into_iter()
            .map(|r| r.unwrap().pkg.to_string())
            .collect();

        assert!(pkgs.contains(&"".to_string()));
        assert!(pkgs.contains(&"src".to_string()));
        assert!(
            !pkgs.contains(&"heph-home".to_string()),
            "core dir not pruned"
        );
        assert!(!pkgs.contains(&"internal".to_string()), "glob not pruned");
        for pruned in [".hidden", "_ignored", "vendor", "testdata"] {
            assert!(
                !pkgs.contains(&pruned.to_string()),
                "built-in prune rule dropped: {pruned}"
            );
        }
    }

    #[test]
    fn collect_go_packages_through_enabled_walker_matches_uncached() {
        // The package walk reads each dir through the shared CachedWalker. With a
        // real (enabled) walker backing it the discovered package set must be
        // identical to a raw, uncached walk — the walker is transparent, it only
        // caches the `readdir` by directory mtime.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("go.mod"), "module example.com/x\n").unwrap();
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        std::fs::create_dir_all(root.join("c")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();

        let skip = Ignore::default();
        let to_pkgs = |v: Vec<anyhow::Result<ListPackageResponse>>| {
            let mut s: Vec<String> = v.into_iter().map(|r| r.unwrap().pkg.to_string()).collect();
            s.sort();
            s
        };

        let mut uncached = Vec::new();
        collect_go_packages(
            &CachedWalker::disabled(),
            root,
            root,
            false,
            &skip,
            &mut uncached,
        );

        let dbdir = tempfile::tempdir().unwrap();
        let walker = CachedWalker::open(&dbdir.path().join("fswalk.db"));
        let mut cached = Vec::new();
        collect_go_packages(&walker, root, root, false, &skip, &mut cached);
        // Second walk: now served from the walker's cache; still identical.
        let mut cached2 = Vec::new();
        collect_go_packages(&walker, root, root, false, &skip, &mut cached2);

        let expected = to_pkgs(uncached);
        assert!(expected.contains(&"a".to_string()));
        assert!(expected.contains(&"a/b".to_string()));
        assert!(!expected.iter().any(|p| p == "vendor"));
        assert_eq!(expected, to_pkgs(cached));
        assert_eq!(expected, to_pkgs(cached2));
    }

    #[test]
    fn compose_closures_dedups_diamond_and_keeps_deps_first() {
        // T1.3 froze the closure-composition contract while switching import paths
        // to `Arc<str>`: children's transitive libs come first, a diamond dep
        // appears once (first-seen wins), and the composing node's own lib is
        // appended last. `import_closure` and `collect_libs_inner` both route
        // through `compose_closures`, so this freezes both.
        let mk = |ip: &str| {
            (
                Arc::<str>::from(ip),
                Addr::new(PkgBuf::from("p"), format!("lib_{ip}"), Default::default()),
            )
        };
        // Diamond: top → {left, right}; left → shared; right → shared.
        let left = Arc::new(ImportClosure {
            libs: vec![mk("shared"), mk("left")],
        });
        let right = Arc::new(ImportClosure {
            libs: vec![mk("shared"), mk("right")],
        });

        let composed = compose_closures(&[left, right], Some(mk("top")));
        let ips: Vec<&str> = composed.iter().map(|(ip, _)| ip.as_ref()).collect();
        assert_eq!(
            ips,
            ["shared", "left", "right", "top"],
            "deps-first, diamond 'shared' deduped to its first occurrence, self last"
        );

        // Without a self lib (the `collect_libs_inner` shape), only the merged,
        // deduped child set remains.
        let a = Arc::new(ImportClosure {
            libs: vec![mk("x"), mk("y")],
        });
        let b = Arc::new(ImportClosure {
            libs: vec![mk("y"), mk("z")],
        });
        let merged = compose_closures(&[a, b], None);
        let ips: Vec<&str> = merged.iter().map(|(ip, _)| ip.as_ref()).collect();
        assert_eq!(ips, ["x", "y", "z"], "diamond 'y' deduped, order preserved");
    }

    #[test]
    fn resolve_go_env_vars_batches_in_request_order() {
        require_go!();
        // One value per requested name, in request order, matching individual
        // `go env` calls — the batched single-spawn path must not reorder or drop.
        let vals = resolve_go_env_vars(&["GOROOT", "GOCACHE", "GOMODCACHE"]).expect("go env");
        assert_eq!(vals.len(), 3, "one value per requested variable");
        let single = |name: &str| {
            let out = std::process::Command::new("go")
                .args(["env", name])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        assert_eq!(vals[0], single("GOROOT"), "GOROOT stays in slot 0");
        assert_eq!(vals[1], single("GOCACHE"), "GOCACHE stays in slot 1");
        assert_eq!(vals[2], single("GOMODCACHE"), "GOMODCACHE stays in slot 2");
        assert!(
            !vals[0].is_empty() && !vals[1].is_empty(),
            "GOROOT/GOCACHE resolve to non-empty paths"
        );
    }

    fn make_get_req(addr: Addr, workspace_root: &std::path::Path) -> GetRequest {
        GetRequest {
            request_id: "test".to_string(),
            addr,
            states: vec![],
            executor: test_executor(workspace_root),
        }
    }

    async fn provider_get(p: &Provider, addr: Addr) -> Result<GetResponse, GetError> {
        let ctoken = StdCancellationToken::new();
        let workspace = p.inner.workspace_root.clone();
        p.get(make_get_req(addr, &workspace), &ctoken).await
    }

    #[test]
    fn test_load_go_mod_merges_go_sum_for_indirect_modules() {
        require_go!();
        // An untidied go.mod that directly requires oauth2 but not x/net, with a
        // go.sum that carries x/net's selected version. load_go_mod must expose
        // x/net so that oauth2/internal's import of x/net/context/ctxhttp can
        // resolve to a thirdparty addr instead of being silently dropped.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("go.mod"),
            "module infiot.com/infiot/tools/gogithub\n\ngo 1.12\n\nrequire golang.org/x/oauth2 v0.0.0-20200107190931-bf48bf16ab8d\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("go.sum"),
            "golang.org/x/net v0.0.0-20190108225652-1e06a53dbb7e h1:bRhVy7zSSasaqNksaRZiA5EEI+Ei4I1nO5Jh72wfHlg=\n\
golang.org/x/net v0.0.0-20190108225652-1e06a53dbb7e/go.mod h1:mL1N/T3taQHkDXs73rZJwtUhF3w3ftmwwsq0BUmARs4=\n\
golang.org/x/oauth2 v0.0.0-20200107190931-bf48bf16ab8d h1:pE8b58s1HRDMi8RDc79m0HISf9D4TzseP40cEA6IGfs=\n",
        )
        .unwrap();

        let p = Provider::new(dir.path().to_path_buf()).expect("provider");
        let data = p.inner.load_go_mod(dir.path()).expect("load_go_mod");

        let net = find_module_for_import("golang.org/x/net/context/ctxhttp", &data.requires)
            .expect("x/net must resolve via go.sum even though go.mod omits it");
        assert_eq!(net.0, "golang.org/x/net");
        assert_eq!(net.1, "v0.0.0-20190108225652-1e06a53dbb7e");

        // go.mod-listed module still resolves, and only once (no go.sum dup).
        let oauth2 = find_module_for_import("golang.org/x/oauth2", &data.requires).unwrap();
        assert_eq!(oauth2.1, "v0.0.0-20200107190931-bf48bf16ab8d");
        let oauth2_count = data
            .requires
            .iter()
            .filter(|(m, _)| m == "golang.org/x/oauth2")
            .count();
        assert_eq!(
            oauth2_count, 1,
            "go.mod module must not be duplicated by go.sum"
        );
    }

    // ---- simple_lib ----

    #[tokio::test]
    async fn test_simple_lib_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_out_has_a_group() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        let out = resp.target_spec.config.get("out").unwrap();
        assert!(matches!(out, Value::Map(m) if m.contains_key("a")));
    }

    #[tokio::test]
    async fn test_simple_lib_no_build_target() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("", "build")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_simple_lib_golist_target() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        // _golist should return a spec without calling executor
        let ctoken = StdCancellationToken::new();
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "_golist"),
            states: vec![],
            executor: Arc::new(GoListTestExecutor {
                workspace_root: sandbox.path().to_path_buf(),
                source_map: HashMap::new(),
            }),
        };
        let resp = p.get(req, &ctoken).await.unwrap();
        assert_eq!(resp.target_spec.driver, "go_golist");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(out.contains_key("pkg"));
    }

    // Regression: pkg_cache used to wrap executor.result inside the once
    // closure, so cache-hit waiters bypassed the executor entirely. A target-
    // dep cycle could then hide as a memoizer deadlock instead of surfacing
    // as a synchronous CycleError. The fix hoists executor.result out of the
    // closure; every caller must route through it so result_addr's
    // dep_dag.add_dep runs for waiters too.
    #[tokio::test]
    async fn read_golist_package_calls_executor_for_every_caller() {
        require_go!();

        struct CountingExecutor {
            inner: Arc<dyn ProviderExecutor>,
            result_calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl ProviderExecutor for CountingExecutor {
            fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
                self.result_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.result(addr)
            }
            fn query<'a>(
                &'a self,
                m: &'a hmodel::htmatcher::Matcher,
                extra_skip: &'a [String],
            ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
                self.inner.query(m, extra_skip)
            }
        }

        let sandbox = copy_fixture("simple_lib");
        let inner: Arc<dyn ProviderExecutor> = Arc::new(GoListTestExecutor {
            workspace_root: sandbox.path().to_path_buf(),
            source_map: HashMap::new(),
        });
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor: Arc<dyn ProviderExecutor> = Arc::new(CountingExecutor {
            inner,
            result_calls: Arc::clone(&counter),
        });

        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let golist = make_addr("", "_golist");

        p.inner
            .read_golist_package(Arc::clone(&executor), &golist)
            .await
            .unwrap();
        p.inner
            .read_golist_package(Arc::clone(&executor), &golist)
            .await
            .unwrap();

        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "every read_golist_package must call executor.result so all \
             callers register the parent → golist edge in DepDag"
        );
    }

    // Regression: a target name the go provider doesn't own (e.g. a buildfile
    // codegen target sharing a Go package dir) must resolve to NotFound WITHOUT
    // resolving `_golist`. Otherwise the `q@label=go_src` query that `_golist`
    // pulls in re-enters get_spec for this same addr, trips a false CycleError,
    // and `Engine::query` silently drops the target — so `q codegen .` misses
    // targets that `q all .` (addr-only match, no get_spec) finds. No `go`
    // binary needed: a correct provider bails before any `go list`.
    #[tokio::test]
    async fn foreign_target_name_not_found_without_golist_resolve() {
        // `Provider::new` resolves GOROOT via `go env`, so this needs `go` even
        // though the guard short-circuits before any go list / query.
        require_go!();

        // `Provider::new` enables the foreign-name guard by default, so a name
        // this provider doesn't own resolves to NotFound without touching the
        // executor (no `go list`, no `q@label=go_src` query).
        struct BailExecutor {
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl ProviderExecutor for BailExecutor {
            fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let a = addr.format();
                Box::pin(async move { anyhow::bail!("BailExecutor: result called for {a}") })
            }
            fn query<'a>(
                &'a self,
                _m: &'a hmodel::htmatcher::Matcher,
                _extra_skip: &'a [String],
            ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { anyhow::bail!("BailExecutor: query called") })
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("go.mod"), "module example.com/x\n").unwrap();
        let p = Provider::new(dir.path().to_path_buf()).unwrap();

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor: Arc<dyn ProviderExecutor> = Arc::new(BailExecutor {
            calls: Arc::clone(&calls),
        });
        let ctoken = StdCancellationToken::new();
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "codegen_gen"),
            states: vec![],
            executor,
        };

        let res = p.get(req, &ctoken).await;
        assert!(
            matches!(res, Err(GetError::NotFound)),
            "foreign name must be NotFound, got driver: {:?}",
            res.map(|r| r.target_spec.driver)
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "go provider must not resolve _golist (or its go_src query) for a \
             target name it doesn't own — that re-entry is what trips the false cycle"
        );
    }

    // ---- with_dep ----

    #[tokio::test]
    async fn test_with_dep_lib_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("lib", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
    }

    #[tokio::test]
    async fn test_with_dep_cmd_build_lib_has_dep_on_lib() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("cmd", "build_lib"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let has_lib_dep = deps.keys().any(|k| k.contains("lib"));
        assert!(
            has_lib_dep,
            "cmd build_lib should depend on lib: got {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    // Regression: an imported package whose directory has Go files that are all
    // excluded by build constraints (here a never-set `//go:build` tag, modeling
    // a goos/goarch-excluded package) resolves its `build_lib` to NotFound via
    // NoGoFilesError. Resolving the *importer's* deps must drop that import
    // rather than fail the importer's get_spec — otherwise a query touching the
    // importer dies with "read _golist for …: no Go files in package …".
    #[tokio::test]
    async fn test_dep_constrained_importer_skips_unbuildable_dep() {
        require_go!();
        let sandbox = copy_fixture("dep_constrained");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();

        // The excluded dep itself is NotFound (no buildable Go files).
        let lib_res = provider_get(&p, make_addr("lib", "build_lib")).await;
        assert!(
            matches!(lib_res, Err(GetError::NotFound)),
            "constraint-excluded lib must be NotFound"
        );

        // The importer's get_spec must still succeed, with the unbuildable dep dropped.
        let resp = provider_get(&p, make_addr("cmd", "build_lib"))
            .await
            .expect("cmd build_lib get_spec must succeed despite unbuildable dep");
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        // Dep addrs live in the group values; the excluded lib's `//lib:build_lib`
        // addr must not appear in any group.
        let has_lib = deps.values().any(|v| match v {
            Value::List(items) => items
                .iter()
                .any(|it| matches!(it, Value::String(s) if s.contains("//lib:"))),
            _ => false,
        });
        assert!(
            !has_lib,
            "unbuildable lib must not appear in cmd deps: {:?}",
            deps
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_with_dep_cmd_build_is_main() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("cmd", "build")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(
            out.contains_key(""),
            "build out should have empty-string group: {:?}",
            out.keys().collect::<Vec<_>>()
        );
    }

    // ---- stdlib ----

    #[tokio::test]
    async fn test_stdlib_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("@heph/go/std/fmt", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
    }

    #[tokio::test]
    async fn test_stdlib_build_returns_not_found() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("@heph/go/std/fmt", "build")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // ---- with_test ----

    #[tokio::test]
    async fn test_with_test_build_test_exists() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_test"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[tokio::test]
    async fn test_with_test_test_deps_on_build_test() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "test")).await.unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(deps.contains_key("bin"));
        let bin_dep = match deps.get("bin").unwrap() {
            Value::List(v) => v,
            _ => panic!(),
        };
        let dep_str = match &bin_dep[0] {
            Value::String(s) => s,
            _ => panic!(),
        };
        assert!(
            dep_str.contains("build_test"),
            "dep should reference build_test: {}",
            dep_str
        );
    }

    // ---- with_test_cycle ----
    // pkgb has internal _test.go (no cycle: internal tests can't import pkga
    // because pkga imports pkgb — Go rejects). pkgb also has xtest
    // (`package pkgb_test`) that DOES import pkga — Go allows this because
    // pkgb_test is a distinct package.
    //
    // The bug was: combining internal and xtest into one `build_test` bin
    // forced testmain to reference both P=build_test_lib (for internal tests)
    // AND P_test=build_xtest_lib (for xtest). xtest_lib was compiled against
    // P=normal build_lib, so the linker (providing P=build_test_lib) found a
    // fingerprint mismatch on pkga (which imports P=normal).
    //
    // Fix: split into separate `build_test` (internal) and `build_xtest`
    // (xtest) bins. Internal bin doesn't include pkga at all (Go rejects
    // cycle there). Xtest bin uses P=normal everywhere — testmain_xtest
    // imports `_xtest "P_test"` only, never P directly.

    #[tokio::test]
    async fn test_with_test_cycle_build_test_uses_test_lib_for_p() {
        require_go!();
        let sandbox = copy_fixture("with_test_cycle");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkgb", "build_test"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        // Internal bin's importcfg entry for pkgb's import path must point at
        // build_test_lib (not normal build_lib).
        let pkgb_group = deps
            .iter()
            .find(|(k, _)| k.as_str() == "lib_example_com_with_test_cycle_pkgb")
            .map(|(_, v)| v)
            .expect("build_test deps must include pkgb's lib group");
        let pkgb_addr = match pkgb_group {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!("expected string"),
            },
            _ => panic!("expected list"),
        };
        assert!(
            pkgb_addr.contains("build_test_lib"),
            "pkgb in internal build_test must reference build_test_lib: got {}",
            pkgb_addr
        );
    }

    #[tokio::test]
    async fn test_with_test_cycle_build_xtest_uses_normal_p_and_xtest_lib() {
        require_go!();
        let sandbox = copy_fixture("with_test_cycle");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkgb", "build_xtest"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        // pkgb (P) must be NORMAL build_lib — xtest_lib was compiled against
        // it; mismatch would occur if xtest bin substituted test_lib here.
        let pkgb_group = deps
            .get("lib_example_com_with_test_cycle_pkgb")
            .expect("xtest bin must include pkgb's lib group");
        let pkgb_addr = match pkgb_group {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            pkgb_addr.contains("build_lib"),
            "pkgb in xtest bin must use build_lib (normal): got {}",
            pkgb_addr
        );
        assert!(
            !pkgb_addr.contains("build_test_lib"),
            "pkgb in xtest bin must NOT use build_test_lib: got {}",
            pkgb_addr
        );

        // P_test must point at build_xtest_lib.
        let pkgb_test_group = deps
            .get("lib_example_com_with_test_cycle_pkgb_test")
            .expect("xtest bin must include pkgb_test (xtest_lib) group");
        let pkgb_test_addr = match pkgb_test_group {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            pkgb_test_addr.contains("build_xtest_lib"),
            "pkgb_test must reference build_xtest_lib: got {}",
            pkgb_test_addr
        );

        // pkga (the cycle dep) must be normal build_lib — no `for_test_of` flavoring.
        let pkga_group = deps
            .iter()
            .find(|(k, _)| k.contains("pkga"))
            .map(|(_, v)| v)
            .expect("xtest bin must include pkga");
        let pkga_addr = match pkga_group {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.as_str(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            !pkga_addr.contains("for_test_of"),
            "pkga must NOT carry for_test_of arg (split bin design): got {}",
            pkga_addr
        );
        assert!(
            !pkga_addr.contains("build_test_lib"),
            "pkga must use normal build_lib: got {}",
            pkga_addr
        );
    }

    // ---- with_embed ----

    #[tokio::test]
    async fn test_with_embed_has_embed_target() {
        require_go!();
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "embed"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "go_embed");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("cfg"));
    }

    // ---- factors ----

    #[tokio::test]
    async fn test_factors_linux_amd64_runtime_env() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let addr = {
            let mut args = std::collections::BTreeMap::new();
            args.insert("goos".to_string(), "linux".to_string());
            args.insert("goarch".to_string(), "amd64".to_string());
            Addr::new(PkgBuf::from(""), "build_lib".to_string(), args)
        };
        let resp = provider_get(&p, addr).await.unwrap();
        let runtime_env = match resp.target_spec.config.get("runtime_env").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(runtime_env.get("GOOS"), Some(Value::String(s)) if s == "linux"),
            "GOOS should be linux, got {:?}",
            runtime_env.get("GOOS")
        );
        assert!(
            matches!(runtime_env.get("GOARCH"), Some(Value::String(s)) if s == "amd64"),
            "GOARCH should be amd64, got {:?}",
            runtime_env.get("GOARCH")
        );
    }

    // ---- with_embed ----

    #[tokio::test]
    async fn test_with_embed_build_lib_exists() {
        require_go!();
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("a"));
    }

    #[tokio::test]
    async fn test_with_embed_build_lib_has_embed_dep() {
        require_go!();
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "build_lib"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("embed"),
            "build_lib for embed package must depend on the embed target: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_with_embed_build_lib_run_has_embedcfg_flag() {
        require_go!();
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "build_lib"))
            .await
            .unwrap();
        let run = run_str(&resp.target_spec);
        assert!(
            run.contains("-embedcfg"),
            "build_lib run script must contain -embedcfg for embed package: {run}"
        );
    }

    #[tokio::test]
    async fn test_with_embed_embed_target_has_golist_dep() {
        require_go!();
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "embed"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "go_embed");
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("golist"),
            "embed spec must dep on golist output"
        );
        let variant = match resp.target_spec.config.get("variant").unwrap() {
            Value::String(s) => s.as_str(),
            _ => panic!("expected string"),
        };
        assert_eq!(variant, "embed");
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_no_embed_dep() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            !deps.contains_key("embed"),
            "build_lib for non-embed package must not have 'embed' dep"
        );
        let run = run_str(&resp.target_spec);
        assert!(
            !run.contains("-embedcfg"),
            "build_lib for non-embed package must not include -embedcfg: {run}"
        );
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_default_deps_are_pluginfs_addrs() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let src_list = match deps.get("").unwrap() {
            Value::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            !src_list.is_empty(),
            "default dep group must not be empty for a package with go files"
        );
        for entry in src_list {
            let s = match entry {
                Value::String(s) => s,
                _ => panic!("expected string"),
            };
            assert!(
                s.contains("@heph/fs"),
                "each src dep must be a pluginfs addr, got: {}",
                s
            );
            assert!(
                s.ends_with(".go") || s.contains(".go"),
                "src dep must reference a .go file: {}",
                s
            );
        }
    }

    #[tokio::test]
    async fn test_list_simple_lib_no_go_src_target() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "").await;
        assert!(
            !names.iter().any(|n| n == "_go_src"),
            "_go_src must not appear in list output: {:?}",
            names
        );
    }

    // ---- non-Go package ----

    #[tokio::test]
    async fn test_non_go_package_returns_not_found() {
        require_go!();
        let sandbox = tempfile::tempdir().unwrap();
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("somepkg", "build_lib")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // ---- list() tests ----

    async fn provider_list(p: &Provider, package: &str) -> Vec<String> {
        let ctoken = StdCancellationToken::new();
        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from(package),
            states: vec![],
        };
        p.list(req, &ctoken)
            .await
            .unwrap()
            .map(|r| r.unwrap().addr.name.clone())
            .collect()
    }

    #[tokio::test]
    async fn test_list_with_test_includes_build_test() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "build_test"),
            "expected build_test in list: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_list_with_test_includes_test() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "test"),
            "expected test in list: {:?}",
            names
        );
    }

    // `list` now emits the full candidate set unconditionally for any dir
    // under a go.mod; `get` is what filters by inspecting the `_golist` result.
    // For a package with no `_test.go` files, build_test/test must resolve to
    // NotFound via the per-arm `pkg.test_go_files.is_empty()` guard.
    #[tokio::test]
    async fn test_simple_lib_no_test_targets_via_get() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        assert!(matches!(
            provider_get(&p, make_addr("", "build_test")).await,
            Err(GetError::NotFound)
        ));
        assert!(matches!(
            provider_get(&p, make_addr("", "test")).await,
            Err(GetError::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_list_with_test_includes_build_lib() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "build_lib"),
            "expected build_lib in list: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_list_test_only_pkg_includes_build_test() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "build_test"),
            "expected build_test in list for test-only package: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_list_test_only_pkg_includes_test() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            names.iter().any(|n| n == "test"),
            "expected test in list for test-only package: {:?}",
            names
        );
    }

    // A test-only package (only `package pkg_test` xtest files) has empty
    // GoFiles, so `build_lib` must resolve to NotFound via the per-arm
    // `pkg.go_files.is_empty()` guard.
    #[tokio::test]
    async fn test_test_only_pkg_build_lib_not_found_via_get() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        assert!(matches!(
            provider_get(&p, make_addr("pkg", "build_lib")).await,
            Err(GetError::NotFound)
        ));
    }

    // test_only fixture has ONLY xtest_go_files (package pkg_test) — no
    // internal _test.go and no go.go. So `build_test`/`test` (internal-only
    // since the split) return NotFound; the xtest variant exists.
    #[tokio::test]
    async fn test_test_only_build_xtest_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_xtest"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[tokio::test]
    async fn test_test_only_xtest_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _resp = provider_get(&p, make_addr("pkg", "xtest")).await.unwrap();
    }

    #[tokio::test]
    async fn test_test_only_internal_build_test_not_found() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("pkg", "build_test")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // Regression: xtest-only package's `build_xtest` previously hard-coded P =
    // build_lib in importcfg, even though build_lib doesn't exist for a pkg
    // with no GoFiles. `pick_xtest_p_lib_name` now skips the P slot entirely.
    #[tokio::test]
    async fn xtest_only_build_xtest_omits_p_slot() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_xtest"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        let p_group =
            crate::plugingo::addr_util::import_path_to_dep_group("example.com/testonly/pkg");
        assert!(
            !deps.contains_key(&p_group),
            "xtest-only bin must not reference build_lib for P: {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    // ---- test_only_internal ----
    // Only internal `package pkg` _test.go file. GoFiles is empty;
    // TestGoFiles is non-empty. build_lib/build_xtest/build_xtest_lib must
    // resolve NotFound; build_test/test/build_test_lib must succeed.

    #[tokio::test]
    async fn test_only_internal_build_lib_not_found() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        assert!(matches!(
            provider_get(&p, make_addr("pkg", "build_lib")).await,
            Err(GetError::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_only_internal_build_test_lib_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_test_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
    }

    #[tokio::test]
    async fn test_only_internal_build_test_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_test"))
            .await
            .unwrap();
        let out = match resp.target_spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[tokio::test]
    async fn test_only_internal_test_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _ = provider_get(&p, make_addr("pkg", "test")).await.unwrap();
    }

    #[tokio::test]
    async fn test_only_internal_xtest_variants_not_found() {
        require_go!();
        let sandbox = copy_fixture("test_only_internal");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        for name in [
            "build_xtest",
            "build_xtest_lib",
            "build_xtestmain_lib",
            "xtest",
            "xtestmain",
        ] {
            assert!(
                matches!(
                    provider_get(&p, make_addr("pkg", name)).await,
                    Err(GetError::NotFound)
                ),
                "{name} must be NotFound for test-only-internal package"
            );
        }
    }

    // ---- test_xtest_only ----
    // Both internal _test.go (package pkg) and external x_test.go
    // (package pkg_test) present. xtest imports the internal package, which
    // has no GoFiles → P's xtest slot must use build_test_lib, not build_lib.

    #[tokio::test]
    async fn test_xtest_only_build_lib_not_found() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        assert!(matches!(
            provider_get(&p, make_addr("pkg", "build_lib")).await,
            Err(GetError::NotFound)
        ));
    }

    #[tokio::test]
    async fn test_xtest_only_build_test_exists() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _ = provider_get(&p, make_addr("pkg", "build_test"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_xtest_only_build_xtest_exists() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _ = provider_get(&p, make_addr("pkg", "build_xtest"))
            .await
            .unwrap();
    }

    // P (the internal pkg) has only TestGoFiles, so xtest_lib and xtest bin
    // must both reference build_test_lib for P (not build_lib, which doesn't
    // exist).
    #[tokio::test]
    async fn test_xtest_only_build_xtest_p_slot_uses_test_lib() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_xtest"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        let p_group =
            crate::plugingo::addr_util::import_path_to_dep_group("example.com/testxtestonly/pkg");
        let entry = deps
            .get(&p_group)
            .expect("xtest bin must include P's lib group");
        let s = match entry {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            s.contains("build_test_lib"),
            "P in xtest bin must reference build_test_lib for test+xtest-only pkg: {s}"
        );
        assert!(
            !s.contains(":build_lib"),
            "P must NOT reference normal build_lib: {s}"
        );
    }

    #[tokio::test]
    async fn test_xtest_only_build_xtest_lib_p_in_importcfg_uses_test_lib() {
        require_go!();
        let sandbox = copy_fixture("test_xtest_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_xtest_lib"))
            .await
            .unwrap();
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!(),
        };
        let p_group =
            crate::plugingo::addr_util::import_path_to_dep_group("example.com/testxtestonly/pkg");
        let entry = deps
            .get(&p_group)
            .expect("xtest_lib must include P in importcfg (xtest source imports P)");
        let s = match entry {
            Value::List(v) => match &v[0] {
                Value::String(s) => s.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert!(
            s.contains("build_test_lib"),
            "P in xtest_lib must reference build_test_lib for test+xtest-only pkg: {s}"
        );
    }

    // ---- mod-asm ----

    #[tokio::test]
    async fn test_mod_asm_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("mod-asm");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "sh");
    }

    #[tokio::test]
    async fn test_mod_asm_thirdparty_with_sfiles_generates_asm_steps() {
        require_go!();
        let sandbox = copy_fixture("mod-asm");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();

        // github.com/klauspost/cpuid/v2 has assembly on all architectures
        let addr = make_addr(
            "@heph/go/thirdparty/github.com/klauspost/cpuid/v2@v2.2.5",
            "build_lib",
        );
        let resp = match provider_get(&p, addr).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping: could not get cpuid build_lib: {:?}", e);
                return;
            }
        };

        let run = run_str(&resp.target_spec);

        // Only assert asm steps when the package actually has .s files on this platform.
        // If SFiles is empty for this arch, tool asm won't appear and that's correct.
        if run.contains("tool asm") {
            assert!(
                run.contains("tool pack r"),
                "asm package must pack .o files: {run}"
            );
            assert!(
                run.contains("-asmhdr"),
                "asm package compile step must emit -asmhdr: {run}"
            );
            assert!(
                run.contains("$GOROOT/pkg/include"),
                "asm step must include GOROOT/pkg/include: {run}"
            );
        }
    }

    fn state_with_root(pkg: &str, root: bool) -> State {
        let mut m = HashMap::new();
        m.insert("go_codegen_root".to_string(), Value::Bool(root));
        State {
            package: PkgBuf::from(pkg),
            provider: "go".to_string(),
            state: m,
        }
    }

    #[test]
    fn pick_codegen_root_picks_deepest_matching_state() {
        let states = vec![
            state_with_root("src", true),
            state_with_root("src/foo", true),
            state_with_root("other", true),
        ];
        let picked = pick_codegen_root(&states).unwrap();
        assert_eq!(picked.package.as_str(), "src/foo");
    }

    #[test]
    fn pick_codegen_root_ignores_states_with_root_false() {
        let states = vec![state_with_root("src", false)];
        let picked = pick_codegen_root(&states);
        assert!(picked.is_none());
    }

    #[test]
    fn pick_codegen_root_returns_none_when_no_states() {
        let picked = pick_codegen_root(&[]);
        assert!(picked.is_none());
    }

    #[test]
    fn pick_codegen_root_matches_root_state_at_empty_pkg() {
        let states = vec![state_with_root("", true)];
        let picked = pick_codegen_root(&states).unwrap();
        assert_eq!(picked.package.as_str(), "");
    }

    fn state_with_test_skip(pkg: &str, skip: bool) -> State {
        let mut test_map = HashMap::new();
        test_map.insert("skip".to_string(), Value::Bool(skip));
        let mut m = HashMap::new();
        m.insert("test".to_string(), Value::Map(test_map));
        State {
            package: PkgBuf::from(pkg),
            provider: "go".to_string(),
            state: m,
        }
    }

    #[test]
    fn pick_test_skip_false_when_no_states() {
        assert!(!pick_test_skip(&[]));
    }

    #[test]
    fn pick_test_skip_true_when_root_state_sets_skip() {
        let states = vec![state_with_test_skip("", true)];
        assert!(pick_test_skip(&states));
    }

    #[test]
    fn pick_test_skip_deeper_state_overrides_shallower() {
        // Root says skip=True; deeper pkg says skip=False → tests must run.
        let states = vec![
            state_with_test_skip("", true),
            state_with_test_skip("src/foo", false),
        ];
        assert!(!pick_test_skip(&states));
    }

    #[test]
    fn pick_test_skip_state_without_test_key_returns_false() {
        let mut m = HashMap::new();
        m.insert("other".to_string(), Value::Bool(true));
        let states = vec![State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: m,
        }];
        assert!(!pick_test_skip(&states));
    }

    #[tokio::test]
    async fn list_excludes_test_targets_when_test_skip_set() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let ctoken = StdCancellationToken::new();
        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from("pkg"),
            states: vec![state_with_test_skip("", true)],
        };
        let names: Vec<String> = p
            .list(req, &ctoken)
            .await
            .unwrap()
            .map(|r| r.unwrap().addr.name.clone())
            .collect();
        for name in TEST_TARGET_NAMES {
            assert!(
                !names.iter().any(|n| n == name),
                "test target {name} must not appear in list when test.skip=True: {names:?}"
            );
        }
        // Non-test targets still emitted.
        assert!(names.iter().any(|n| n == "build_lib"));
        assert!(names.iter().any(|n| n == "_golist"));
    }

    #[tokio::test]
    async fn get_returns_not_found_for_test_targets_when_test_skip_set() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let ctoken = StdCancellationToken::new();
        for name in ["test", "build_test", "xtest", "build_xtest"] {
            let req = GetRequest {
                request_id: "test".to_string(),
                addr: make_addr("pkg", name),
                states: vec![state_with_test_skip("", true)],
                executor: test_executor(sandbox.path()),
            };
            let res = p.get(req, &ctoken).await;
            assert!(
                matches!(res, Err(GetError::NotFound)),
                "get({name}) must return NotFound when test.skip=True"
            );
        }
    }

    #[tokio::test]
    async fn get_build_test_still_works_when_test_skip_false_overrides() {
        require_go!();
        let sandbox = copy_fixture("with_test");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let ctoken = StdCancellationToken::new();
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("pkg", "build_test"),
            states: vec![
                state_with_test_skip("", true),
                state_with_test_skip("pkg", false),
            ],
            executor: test_executor(sandbox.path()),
        };
        let res = p.get(req, &ctoken).await;
        assert!(res.is_ok(), "deeper test.skip=False must override");
    }

    fn extract_srcfiles(resp: &GetResponse) -> Vec<String> {
        let deps = match resp.target_spec.config.get("deps").unwrap() {
            Value::Map(m) => m,
            _ => panic!("deps not a map"),
        };
        match deps.get("srcfiles").unwrap() {
            Value::List(l) => l
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    _ => panic!("srcfiles entry not a string"),
                })
                .collect(),
            _ => panic!("srcfiles not a list"),
        }
    }

    #[tokio::test]
    async fn golist_default_uses_package_matcher_for_go_src_query() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "_golist"),
            states: vec![],
            executor: Arc::new(GoListTestExecutor {
                workspace_root: sandbox.path().to_path_buf(),
                source_map: HashMap::new(),
            }),
        };
        let resp = p.get(req, &StdCancellationToken::new()).await.unwrap();
        let srcfiles = extract_srcfiles(&resp);
        let go_src_query = srcfiles
            .iter()
            .find(|s| s.contains("label=go_src"))
            .expect("go_src query addr present");
        assert!(
            go_src_query.contains("package="),
            "default must use package= matcher, got: {go_src_query}"
        );
        assert!(
            !go_src_query.contains("package_prefix="),
            "default must not use package_prefix, got: {go_src_query}"
        );
    }

    #[tokio::test]
    async fn golist_codegen_root_widens_go_src_query_and_appends_deps() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let mut state_map = HashMap::new();
        state_map.insert("go_codegen_root".to_string(), Value::Bool(true));
        state_map.insert(
            "go_codegen_deps".to_string(),
            Value::List(vec![Value::String("//codegen:gen".to_string())]),
        );
        let state = State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: state_map,
        };
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "_golist"),
            states: vec![state],
            executor: Arc::new(GoListTestExecutor {
                workspace_root: sandbox.path().to_path_buf(),
                source_map: HashMap::new(),
            }),
        };
        let resp = p.get(req, &StdCancellationToken::new()).await.unwrap();
        let srcfiles = extract_srcfiles(&resp);
        let go_src_query = srcfiles
            .iter()
            .find(|s| s.contains("label=go_src"))
            .expect("go_src query addr present");
        assert!(
            go_src_query.contains("package_prefix="),
            "codegen_root must use package_prefix matcher, got: {go_src_query}"
        );
        assert!(
            !go_src_query.contains(",package="),
            "codegen_root must drop package= matcher, got: {go_src_query}"
        );
        assert!(
            srcfiles.iter().any(|s| s == "//codegen:gen"),
            "go_codegen_deps must be appended to srcfiles, got: {srcfiles:?}"
        );
    }

    // Regression: a BUILD file declaring only `go_codegen_deps` (no
    // `go_codegen_root=true`) on an ancestor package must still inject those
    // deps into a descendant `_golist` sandbox. Previously the deps lookup was
    // nested inside the root-marker check, so without the marker the codegen
    // target never ran and `_golist` for an empty (codegen-only) directory
    // hit NoGoFilesError.
    #[tokio::test]
    async fn golist_appends_codegen_deps_without_root_marker() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let mut state_map = HashMap::new();
        state_map.insert(
            "go_codegen_deps".to_string(),
            Value::List(vec![Value::String("//codegen:gen".to_string())]),
        );
        let state = State {
            package: PkgBuf::from(""),
            provider: "go".to_string(),
            state: state_map,
        };
        let req = GetRequest {
            request_id: "test".to_string(),
            addr: make_addr("", "_golist"),
            states: vec![state],
            executor: Arc::new(GoListTestExecutor {
                workspace_root: sandbox.path().to_path_buf(),
                source_map: HashMap::new(),
            }),
        };
        let resp = p.get(req, &StdCancellationToken::new()).await.unwrap();
        let srcfiles = extract_srcfiles(&resp);
        assert!(
            srcfiles.iter().any(|s| s == "//codegen:gen"),
            "go_codegen_deps must be appended even without go_codegen_root, got: {srcfiles:?}"
        );
    }
}
