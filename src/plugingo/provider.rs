use crate::engine::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, Provider as ProviderTrait, ProviderExecutor,
};
use crate::hasync::Cancellable;
use crate::hmemoizer::{Memoizer, unwrap_arc_err};
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;
use crate::pluginfs;
use crate::plugingo::addr_util::{
    GoPackageKind, decode_package, encode_firstparty, encode_stdlib, encode_thirdparty,
    encode_thirdparty_download, factors_to_args,
};
use crate::plugingo::factors::{Factors, current_goarch, current_goos};
use crate::plugingo::pkg_analysis::{
    GoPackage, PackageAddrs, decode_go_package, decode_package_addrs, find_module_for_import,
    is_stdlib_import_path, parse_go_mod_module_path, parse_go_mod_requires,
};
use crate::plugingo::target_bin;
use crate::plugingo::target_golist;
use crate::plugingo::target_lib;
use crate::plugingo::target_modfiles;
use crate::plugingo::target_std;
use crate::plugingo::target_test;
use crate::plugingo::thirdparty;
use anyhow::Context;
use enclose::enclose;
use futures::future::{BoxFuture, try_join_all};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DEFAULT_GO_BIN_ADDR: &str = "//@heph/bin:go";

pub struct Config {
    pub go_bin_addr: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            go_bin_addr: DEFAULT_GO_BIN_ADDR.to_string(),
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
    /// `handle_get` calls (`build_lib`, `build`, `build_test`, `build_lib#test`,
    /// `build_lib#xtest`, `build_testmain_lib`) that share the same root pkg + factors.
    libs_cache: Memoizer<LibsKey, Result<Arc<TransitiveDeps>, Arc<anyhow::Error>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LibsKey {
    imports: Vec<String>,
    extra: Vec<String>,
    factors: Factors,
    module_root: PathBuf,
    transitive: bool,
}

impl Provider {
    pub fn new(workspace_root: PathBuf) -> anyhow::Result<Self> {
        Self::with_config(workspace_root, Config::default())
    }

    pub fn from_options(
        workspace_root: PathBuf,
        opts: &crate::engine::config_file::Options,
    ) -> anyhow::Result<Self> {
        crate::engine::config_file::deny_unknown("go provider", opts, &["gotool"])?;
        let go_bin_addr: String =
            crate::engine::config_file::decode_opt(opts, "go provider", "gotool")?
                .unwrap_or_else(|| DEFAULT_GO_BIN_ADDR.to_string());
        Self::with_config(workspace_root, Config { go_bin_addr })
    }

    pub fn go_bin_addr(&self) -> &str {
        &self.inner.go_bin_addr
    }

    pub fn with_config(workspace_root: PathBuf, config: Config) -> anyhow::Result<Self> {
        let goroot = resolve_goroot()?;
        let gomodcache = resolve_go_env_var("GOMODCACHE")?;
        let gopath = resolve_go_env_var("GOPATH")?;
        let gocache = resolve_go_env_var("GOCACHE")?;
        Ok(Self {
            inner: Arc::new(ProviderInner {
                workspace_root,
                goroot,
                gomodcache,
                gopath,
                gocache,
                go_bin_addr: config.go_bin_addr,
                pkg_cache: Memoizer::with_tag("pkg_cache"),
                pkg_addrs_cache: Memoizer::with_tag("pkg_addrs_cache"),
                libs_cache: Memoizer::with_tag("libs_cache"),
            }),
        })
    }
}

fn resolve_go_env_var(name: &str) -> anyhow::Result<String> {
    if let Ok(val) = std::env::var(name)
        && !val.is_empty()
    {
        return Ok(val);
    }
    let output = std::process::Command::new("go")
        .arg("env")
        .arg(name)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run `go env {}`: {}", name, e))?;
    if !output.status.success() {
        anyhow::bail!("go env {} failed", name);
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(|e| anyhow::anyhow!("go env {} output not utf-8: {}", name, e))?
        .trim()
        .to_string())
}

fn resolve_goroot() -> anyhow::Result<String> {
    resolve_go_env_var("GOROOT")
}

fn collect_go_packages(
    dir: &Path,
    workspace_root: &Path,
    result: &mut Vec<anyhow::Result<ListPackageResponse>>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            result.push(Err(anyhow::anyhow!("read_dir {}: {}", dir.display(), e)));
            return;
        }
    };

    let mut has_go_files = false;
    let mut subdirs: Vec<PathBuf> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Ok(ft) = entry.file_type() {
            if ft.is_dir() {
                if !name_str.starts_with('.')
                    && !name_str.starts_with('_')
                    && name_str != "vendor"
                    && name_str != "testdata"
                {
                    subdirs.push(entry.path());
                }
            } else if ft.is_file() && name_str.ends_with(".go") {
                has_go_files = true;
            }
        }
    }

    if has_go_files {
        let rel = dir.strip_prefix(workspace_root).unwrap_or(dir);
        result.push(Ok(ListPackageResponse {
            pkg: PkgBuf::from(rel.to_string_lossy().as_ref()),
        }));
    }

    for subdir in subdirs {
        collect_go_packages(&subdir, workspace_root, result);
    }
}

/// Scan a directory for .go files and return (has_non_test, has_test).
fn scan_go_files(src_dir: &Path) -> (bool, bool) {
    let entries = match std::fs::read_dir(src_dir) {
        Ok(e) => e,
        Err(_) => return (false, false),
    };

    let mut has_non_test = false;
    let mut has_test = false;

    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".go") {
            continue;
        }
        if name_str.ends_with("_test.go") {
            has_test = true;
        } else {
            has_non_test = true;
        }
        if has_non_test && has_test {
            break;
        }
    }

    (has_non_test, has_test)
}

impl ProviderTrait for Provider {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        self.inner.config(req)
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>> {
        self.inner.list(req, ctoken)
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>>
    {
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
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>> {
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
                        as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>);
                }
            };

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
                        as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
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
                        as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
                }
                GoPackageKind::FirstParty { src_dir, .. } => {
                    let (has_non_test, has_test) = scan_go_files(src_dir);

                    let mut addrs: Vec<Addr> = Vec::new();

                    if has_non_test {
                        addrs.push(Addr::new(
                            req.package.clone(),
                            "_golist".to_string(),
                            factors_to_args(&factors),
                        ));
                        addrs.push(Addr::new(
                            req.package.clone(),
                            "build_lib".to_string(),
                            factors_to_args(&factors),
                        ));
                        // Always list build; get() returns NotFound for non-main
                        addrs.push(Addr::new(
                            req.package.clone(),
                            "build".to_string(),
                            factors_to_args(&factors),
                        ));
                        // Always list embed; get() returns NotFound if no embed files
                        addrs.push(Addr::new(
                            req.package.clone(),
                            "embed".to_string(),
                            factors_to_args(&factors),
                        ));
                        addrs.push(Addr::new(
                            req.package.clone(),
                            "embed#xtest".to_string(),
                            factors_to_args(&factors),
                        ));
                    }

                    if has_test {
                        addrs.push(Addr::new(
                            req.package.clone(),
                            "build_test".to_string(),
                            factors_to_args(&factors),
                        ));
                        addrs.push(Addr::new(
                            req.package,
                            "test".to_string(),
                            factors_to_args(&factors),
                        ));
                    }

                    let responses: Vec<anyhow::Result<ListResponse>> = addrs
                        .into_iter()
                        .map(|addr| Ok(ListResponse { addr }))
                        .collect();

                    Ok(Box::new(responses.into_iter())
                        as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
                }
            }
        })
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>>
    {
        Box::pin(async move {
            let prefix = req.prefix.as_str();

            // Can't enumerate stdlib or thirdparty packages
            if prefix.starts_with("@heph/go/") {
                return Ok(Box::new(std::iter::empty())
                    as Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>);
            }

            let search_dir = if prefix.is_empty() {
                self.workspace_root.clone()
            } else {
                self.workspace_root.join(prefix)
            };

            if !search_dir.exists() {
                return Ok(Box::new(std::iter::empty())
                    as Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>);
            }

            let packages = crate::process_supervisor::block_or_inline(
                enclose!((self.workspace_root => workspace_root) move || {
                    let mut packages = Vec::new();
                    collect_go_packages(&search_dir, &workspace_root, &mut packages);
                    packages
                }),
            );

            Ok(Box::new(packages.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>>,
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

impl ProviderInner {
    async fn handle_get(self: Arc<Self>, req: GetRequest) -> Result<GetResponse, GetError> {
        let addr = &req.addr;
        let factors = Factors::from_addr(addr);

        let kind = match decode_package(&addr.package, &self.workspace_root) {
            Some(k) => k,
            None => return Err(GetError::NotFound),
        };

        // _golist — generate spec without executing go list (before stdlib check so
        // stdlib packages can also expose a _golist target for cached dep resolution)
        if addr.name == "_golist" {
            return self
                .get_golist_spec(addr.clone(), &kind, &factors)
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
        let golist_addr = self.make_addr_with_name(&addr.package, "_golist", &factors);
        let pkg = self
            .read_golist_package(Arc::clone(&req.executor), &golist_addr)
            .await
            .map_err(GetError::Other)?;

        // Package excluded by build constraints or has errors with no files
        if pkg.go_files.is_empty() && pkg.error.is_some() {
            return Err(GetError::NotFound);
        }

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
                        embed_addr.as_ref(),
                        &pkg_addrs.embed_files,
                        &self.go_bin_addr,
                        &self.goroot,
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
            // Intermediate test target: generates testmain.go from test file analysis.
            "testmain" => {
                let has_tests = !pkg.test_go_files.is_empty() || !pkg.xtest_go_files.is_empty();
                if !has_tests {
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
                        &pkg_addrs.xtest_go_files,
                    ),
                })
            }
            // Intermediate test target: compile GoFiles + TestGoFiles in test mode.
            "build_lib#test" => {
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
                    Some(self.make_addr_with_name(&addr.package, "embed#test", &factors))
                } else {
                    None
                };

                let mut test_embed_files = pkg_addrs.embed_files.clone();
                test_embed_files.extend(pkg_addrs.test_embed_files.iter().cloned());
                let spec = target_test::build_lib_test_spec(
                    addr.clone(),
                    &import_path,
                    pkg.name.as_deref().unwrap_or(""),
                    &factors,
                    &transitive.libs,
                    &pkg_addrs.go_files,
                    &pkg_addrs.test_go_files,
                    &self.go_bin_addr,
                    &self.goroot,
                    embed_addr.as_ref(),
                    &test_embed_files,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Intermediate test target: compile XTestGoFiles.
            "build_lib#xtest" => {
                if pkg.xtest_go_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let xtest_imports_pkg = GoPackage {
                    import_path: format!("{}_test", import_path),
                    dir: pkg.dir.clone(),
                    name: pkg.name.clone(),
                    go_files: vec![],
                    s_files: vec![],
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
                let mut transitive = Arc::clone(&self)
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &xtest_imports_pkg,
                        &[],
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                // xtest imports the package under test as `import_path`, but the linker
                // gets `build_lib#test` (not `build_lib`) for that path — substitute here
                // so the importcfg fingerprints match.
                let test_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_lib#test", &factors);
                let mut found = false;
                for (ip, a) in &mut transitive.libs {
                    if ip == &import_path {
                        *a = test_lib_addr.clone();
                        found = true;
                        break;
                    }
                }
                if !found && (!pkg.go_files.is_empty() || !pkg.test_go_files.is_empty()) {
                    transitive.libs.push((import_path.clone(), test_lib_addr));
                }

                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;

                let xtest_embed_addr =
                    if !pkg.xtest_embed_patterns.is_empty() || !pkg.xtest_embed_files.is_empty() {
                        Some(self.make_addr_with_name(&addr.package, "embed#xtest", &factors))
                    } else {
                        None
                    };

                let spec = target_test::build_lib_xtest_spec(
                    addr.clone(),
                    &import_path,
                    pkg.name.as_deref().unwrap_or(""),
                    &factors,
                    &transitive.libs,
                    &pkg_addrs.xtest_go_files,
                    &self.go_bin_addr,
                    &self.goroot,
                    xtest_embed_addr.as_ref(),
                    &pkg_addrs.xtest_embed_files,
                );
                Ok(GetResponse { target_spec: spec })
            }
            // Intermediate test target: compile the generated testmain.go.
            "build_testmain_lib" => {
                let has_tests = !pkg.test_go_files.is_empty() || !pkg.xtest_go_files.is_empty();
                if !has_tests {
                    return Err(GetError::NotFound);
                }
                // testmain package imports: os, reflect, testing, testing/internal/testdeps
                // plus the test/xtest libs themselves
                let testmain_imports = ["os", "reflect", "testing", "testing/internal/testdeps"];
                let testmain_pkg = GoPackage {
                    import_path: "main".to_string(),
                    dir: pkg.dir.clone(),
                    name: Some("main".to_string()),
                    go_files: vec![],
                    s_files: vec![],
                    test_go_files: vec![],
                    xtest_go_files: vec![],
                    embed_patterns: vec![],
                    embed_files: vec![],
                    test_embed_patterns: vec![],
                    test_embed_files: vec![],
                    xtest_embed_patterns: vec![],
                    xtest_embed_files: vec![],
                    imports: testmain_imports.iter().map(|s| s.to_string()).collect(),
                    test_imports: vec![],
                    xtest_imports: vec![],
                    standard: false,
                    module: pkg.module.clone(),
                    match_: vec![],
                    incomplete: false,
                    error: None,
                };
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

                // Add test lib and xtest lib as transitive libs for the testmain
                let test_lib_addr =
                    self.make_addr_with_name(&addr.package, "build_lib#test", &factors);
                let has_test_files = !pkg.test_go_files.is_empty() || !pkg.go_files.is_empty();
                if has_test_files {
                    transitive.libs.push((import_path.clone(), test_lib_addr));
                }
                if !pkg.xtest_go_files.is_empty() {
                    let xtest_lib_addr =
                        self.make_addr_with_name(&addr.package, "build_lib#xtest", &factors);
                    transitive
                        .libs
                        .push((format!("{}_test", import_path), xtest_lib_addr));
                }

                // The testmain source comes from the "testmain" target
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
                );
                Ok(GetResponse { target_spec: spec })
            }
            "build_test" => {
                let has_tests = !pkg.test_go_files.is_empty() || !pkg.xtest_go_files.is_empty();
                if !has_tests {
                    return Err(GetError::NotFound);
                }

                // Collect all transitive libs for the linker importcfg
                let test_extra: Vec<String> = pkg
                    .test_imports
                    .iter()
                    .chain(&pkg.xtest_imports)
                    .chain(
                        ["os", "reflect", "testing", "testing/internal/testdeps"]
                            .iter()
                            .map(|s| &**s as &str)
                            .map(String::from)
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

                // Build the all_libs list for the linker
                let mut all_libs_map: HashMap<String, Addr> = HashMap::new();

                // Add test lib
                let has_test_files = !pkg.test_go_files.is_empty() || !pkg.go_files.is_empty();
                if has_test_files {
                    let test_lib_addr =
                        self.make_addr_with_name(&addr.package, "build_lib#test", &factors);
                    all_libs_map.insert(import_path.clone(), test_lib_addr);
                }
                // Add xtest lib
                if !pkg.xtest_go_files.is_empty() {
                    let xtest_lib_addr =
                        self.make_addr_with_name(&addr.package, "build_lib#xtest", &factors);
                    all_libs_map.insert(format!("{}_test", import_path), xtest_lib_addr);
                }
                // Add transitive libs
                for (ip, a) in transitive.libs {
                    all_libs_map.entry(ip).or_insert(a);
                }

                let all_libs: Vec<(String, Addr)> = all_libs_map.into_iter().collect();
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
            "test" => {
                let has_tests = !pkg.test_go_files.is_empty() || !pkg.xtest_go_files.is_empty();
                if !has_tests {
                    return Err(GetError::NotFound);
                }
                let build_test_addr =
                    self.make_addr_with_name(&addr.package, "build_test", &factors);
                // Add a query dep that pulls in any sibling target labeled
                // `go_test_data`. The engine resolves the query lazily via the
                // `@heph/query` provider — no eager scan of the package here.
                let data_query_addr = crate::htaddr::parse_addr(&format!(
                    "//{}:q@package={},label=go_test_data",
                    crate::pluginquery::PACKAGE,
                    addr.package.as_str(),
                ))
                .context("build go_test_data query addr")
                .map_err(GetError::Other)?;
                let spec = target_test::test_spec(addr.clone(), build_test_addr, &data_query_addr);
                Ok(GetResponse { target_spec: spec })
            }
            "embed" => {
                if pkg.embed_patterns.is_empty() && pkg.embed_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                Ok(GetResponse {
                    target_spec: build_embed_spec(
                        addr.clone(),
                        &golist_addr,
                        "embed",
                        &pkg_addrs.embed_files,
                    ),
                })
            }
            "embed#test" => {
                let has_any = !pkg.embed_patterns.is_empty()
                    || !pkg.embed_files.is_empty()
                    || !pkg.test_embed_patterns.is_empty()
                    || !pkg.test_embed_files.is_empty();
                if !has_any {
                    return Err(GetError::NotFound);
                }
                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                let mut files = pkg_addrs.embed_files.clone();
                files.extend(pkg_addrs.test_embed_files.iter().cloned());
                Ok(GetResponse {
                    target_spec: build_embed_spec(addr.clone(), &golist_addr, "test_embed", &files),
                })
            }
            "embed#xtest" => {
                if pkg.xtest_embed_patterns.is_empty() && pkg.xtest_embed_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let pkg_addrs = self
                    .read_golist_package_addrs(Arc::clone(&req.executor), &golist_addr)
                    .await
                    .map_err(GetError::Other)?;
                Ok(GetResponse {
                    target_spec: build_embed_spec(
                        addr.clone(),
                        &golist_addr,
                        "xtest_embed",
                        &pkg_addrs.xtest_embed_files,
                    ),
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
                    crate::htpkg::PkgBuf::from(module_root_rel.to_string_lossy().as_ref()),
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
                let pkg_str = addr.package.as_str().to_string();
                let go_src_query_addr = Addr::new(
                    crate::htpkg::PkgBuf::from(crate::pluginquery::PACKAGE),
                    "q".to_string(),
                    BTreeMap::from([
                        ("label".to_string(), "go_src".to_string()),
                        ("package".to_string(), pkg_str.clone()),
                        ("tree_output_to".to_string(), pkg_str.clone()),
                    ]),
                );
                // Include all non-Go files in the package directory so that go list
                // can resolve //go:embed patterns and populate EmbedFiles.
                let non_go_glob = if pkg.is_empty() {
                    "**/*".to_string()
                } else {
                    format!("{}/**/*", pkg)
                };
                let non_go_glob_addr = pluginfs::glob_addr(&non_go_glob, &[]);
                let non_go_codegen_query_addr = Addr::new(
                    crate::htpkg::PkgBuf::from(crate::pluginquery::PACKAGE),
                    "q".to_string(),
                    BTreeMap::from([
                        ("package".to_string(), pkg_str.clone()),
                        ("tree_output_to".to_string(), pkg_str),
                    ]),
                );
                let non_go_src_addrs = vec![
                    non_go_glob_addr.format(),
                    non_go_codegen_query_addr.format(),
                ];
                target_golist::build_spec_firstparty(
                    addr,
                    import_path,
                    factors,
                    &self.goroot,
                    &go_mod_addr,
                    &go_src_glob_addr,
                    Some(&go_src_query_addr),
                    &non_go_src_addrs,
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
                    crate::htpkg::PkgBuf::from(module_root_rel.to_string_lossy().as_ref()),
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
        let result = executor.result(golist_addr).await?;
        self.pkg_cache
            .once(
                golist_addr.clone(),
                enclose!((result) move || async move {
                    let pkg = crate::process_supervisor::block_or_inline(move || -> anyhow::Result<_> {
                        for artifact in &result.artifacts {
                            for entry_result in artifact.walk()? {
                                let entry = entry_result?;
                                if entry.path.file_name().and_then(|n| n.to_str())
                                    != Some("package.bin")
                                {
                                    continue;
                                }
                                let data = match entry.kind {
                                    crate::hartifactcontent::WalkEntryKind::File { data, .. } => data,
                                    crate::hartifactcontent::WalkEntryKind::Symlink { .. } => continue,
                                };
                                return decode_go_package(data);
                            }
                        }
                        anyhow::bail!("_golist produced no package.bin")
                    })?;
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
        // executor.result is called outside the once closure for the same
        // reason as in `read_golist_package`: waiters must register the dep
        // edge, not just the cache owner.
        let result = executor.result(golist_addr).await?;
        self.pkg_addrs_cache
            .once(
                golist_addr.clone(),
                enclose!((result) move || async move {
                    let addrs = crate::process_supervisor::block_or_inline(move || -> anyhow::Result<_> {
                        for artifact in &result.artifacts {
                            for entry_result in artifact.walk()? {
                                let entry = entry_result?;
                                if entry.path.file_name().and_then(|n| n.to_str())
                                    != Some("package_addrs.bin")
                                {
                                    continue;
                                }
                                let data = match entry.kind {
                                    crate::hartifactcontent::WalkEntryKind::File { data, .. } => data,
                                    crate::hartifactcontent::WalkEntryKind::Symlink { .. } => continue,
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
        package: &crate::htpkg::PkgBuf,
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
    /// across `build_lib`/`build`/`build_test`/`build_lib#test` etc. for the same
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

    async fn collect_libs_inner(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        root_imports: &[String],
        extra_imports: &[String],
        factors: &Factors,
        module_root: &Path,
        transitive: bool,
    ) -> anyhow::Result<TransitiveDeps> {
        let go_mod_path = module_root.join("go.mod");
        let (go_mod_requires, workspace_module_path) = if go_mod_path.exists() {
            let content = crate::process_supervisor::block_or_inline(
                enclose!((go_mod_path) move || std::fs::read_to_string(&go_mod_path)),
            )
            .with_context(|| format!("reading {}", go_mod_path.display()))?;
            (
                parse_go_mod_requires(&content),
                parse_go_mod_module_path(&content).unwrap_or_default(),
            )
        } else {
            (vec![], String::new())
        };

        if transitive {
            let mut visited: HashSet<String> = HashSet::new();
            let mut libs: Vec<(String, Addr)> = Vec::new();

            let mut frontier: Vec<String> = root_imports
                .iter()
                .chain(extra_imports.iter())
                .filter(|i| *i != "unsafe" && *i != "C" && visited.insert((*i).clone()))
                .cloned()
                .collect();

            while !frontier.is_empty() {
                let results = try_join_all(frontier.iter().map(|ip| {
                    self.resolve_import(
                        Arc::clone(&executor),
                        ip,
                        factors,
                        &go_mod_requires,
                        &workspace_module_path,
                        module_root,
                    )
                }))
                .await?;

                frontier.clear();
                for (import_path, dep_addr_opt, sub_imports) in results {
                    if let Some(dep_addr) = dep_addr_opt {
                        libs.push((import_path, dep_addr));
                    }
                    for sub in sub_imports {
                        if sub != "unsafe" && sub != "C" && visited.insert(sub.clone()) {
                            frontier.push(sub);
                        }
                    }
                }
            }

            return Ok(TransitiveDeps { libs });
        }

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
                &go_mod_requires,
                &workspace_module_path,
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

    /// Resolve one import path: returns `(import_path, Option<build_lib Addr>, sub_imports)`.
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
                crate::htpkg::PkgBuf::from(format!("@heph/go/std/{}", import_path)),
                "_golist".to_string(),
                factors_to_args(factors),
            );
            let sub_imports = self
                .read_golist_package(Arc::clone(&executor), &golist_addr)
                .await
                .map(|pkg| pkg.imports.clone())
                .unwrap_or_default();
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

        let sub_imports = self
            .read_golist_package(executor, &golist_addr)
            .await
            .map(|p| p.imports.clone())
            .unwrap_or_default();

        Ok((import_path.to_string(), Some(dep_addr), sub_imports))
    }

    /// Resolve an import path to a rheph `build_lib` Addr.
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
    embed_file_addrs: &[String],
) -> crate::engine::provider::TargetSpec {
    use crate::loosespecparser::TargetSpecValue;
    use std::collections::HashMap;

    let golist_dep = format!("{}|pkg", golist_addr.format());

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert(
        "variant".to_string(),
        TargetSpecValue::String(variant.to_string()),
    );
    let mut deps_map: HashMap<String, TargetSpecValue> = HashMap::new();
    deps_map.insert(
        "golist".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(golist_dep)]),
    );
    if !embed_file_addrs.is_empty() {
        deps_map.insert(
            "files".to_string(),
            TargetSpecValue::List(
                embed_file_addrs
                    .iter()
                    .map(|s| TargetSpecValue::String(s.clone()))
                    .collect(),
            ),
        );
    }
    config.insert("deps".to_string(), TargetSpecValue::Map(deps_map));
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "cfg".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("embedcfg".to_string())]),
        )])),
    );

    crate::engine::provider::TargetSpec {
        addr,
        driver: "go_embed".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

fn build_testmain_spec(
    addr: Addr,
    golist_addr: &Addr,
    test_file_addrs: &[String],
    xtest_file_addrs: &[String],
) -> crate::engine::provider::TargetSpec {
    use crate::loosespecparser::TargetSpecValue;
    use std::collections::HashMap;

    let golist_dep = format!("{}|pkg", golist_addr.format());

    let mut deps_map: HashMap<String, TargetSpecValue> = HashMap::new();
    deps_map.insert(
        "golist".to_string(),
        TargetSpecValue::List(vec![TargetSpecValue::String(golist_dep)]),
    );
    if !test_file_addrs.is_empty() {
        deps_map.insert(
            "test".to_string(),
            TargetSpecValue::List(
                test_file_addrs
                    .iter()
                    .map(|s| TargetSpecValue::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !xtest_file_addrs.is_empty() {
        deps_map.insert(
            "xtest".to_string(),
            TargetSpecValue::List(
                xtest_file_addrs
                    .iter()
                    .map(|s| TargetSpecValue::String(s.clone()))
                    .collect(),
            ),
        );
    }

    let mut config: HashMap<String, TargetSpecValue> = HashMap::new();
    config.insert("deps".to_string(), TargetSpecValue::Map(deps_map));
    config.insert(
        "out".to_string(),
        TargetSpecValue::Map(HashMap::from([(
            "go".to_string(),
            TargetSpecValue::List(vec![TargetSpecValue::String("testmain.go".to_string())]),
        )])),
    );

    crate::engine::provider::TargetSpec {
        addr,
        driver: "go_testmain".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

use crate::engine::provider::{ProbeRequest, ProbeResponse};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::provider::{GetError, GetRequest, Provider as ProviderTrait};
    use crate::engine::{ArtifactMeta, EResult};
    use crate::hartifactcontent::{Content, WalkEntry, WalkEntryKind};
    use crate::hasync::StdCancellationToken;
    use crate::htpkg::PkgBuf;
    use crate::loosespecparser::TargetSpecValue;
    use crate::plugingo::addr_util::decode_package;
    use crate::plugingo::factors::Factors;
    use crate::plugingo::pkg_analysis::run_go_list;
    use anyhow::Context;
    use futures::future::BoxFuture;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;

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
            _m: &'a crate::htmatcher::Matcher,
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

    // ---- simple_lib ----

    #[tokio::test]
    async fn test_simple_lib_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_out_has_a_group() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        let out = resp.target_spec.config.get("out").unwrap();
        assert!(matches!(out, TargetSpecValue::Map(m) if m.contains_key("a")));
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
            TargetSpecValue::Map(m) => m,
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
                m: &'a crate::htmatcher::Matcher,
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

    // ---- with_dep ----

    #[tokio::test]
    async fn test_with_dep_lib_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("lib", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
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
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let has_lib_dep = deps.keys().any(|k| k.contains("lib"));
        assert!(
            has_lib_dep,
            "cmd build_lib should depend on lib: got {:?}",
            deps.keys().collect::<Vec<_>>()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_with_dep_cmd_build_is_main() {
        require_go!();
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("cmd", "build")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
        let out = match resp.target_spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
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
        assert_eq!(resp.target_spec.driver, "bash");
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
        assert_eq!(resp.target_spec.driver, "bash");
        let out = match resp.target_spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
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
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(deps.contains_key("bin"));
        let bin_dep = match deps.get("bin").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!(),
        };
        let dep_str = match &bin_dep[0] {
            TargetSpecValue::String(s) => s,
            _ => panic!(),
        };
        assert!(
            dep_str.contains("build_test"),
            "dep should reference build_test: {}",
            dep_str
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
            TargetSpecValue::Map(m) => m,
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
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(
            matches!(runtime_env.get("GOOS"), Some(TargetSpecValue::String(s)) if s == "linux"),
            "GOOS should be linux, got {:?}",
            runtime_env.get("GOOS")
        );
        assert!(
            matches!(runtime_env.get("GOARCH"), Some(TargetSpecValue::String(s)) if s == "amd64"),
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
        assert_eq!(resp.target_spec.driver, "bash");
        let out = match resp.target_spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
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
            TargetSpecValue::Map(m) => m,
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
        let run = match resp.target_spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
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
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            deps.contains_key("golist"),
            "embed spec must dep on golist output"
        );
        let variant = match resp.target_spec.config.get("variant").unwrap() {
            TargetSpecValue::String(s) => s.as_str(),
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
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        assert!(
            !deps.contains_key("embed"),
            "build_lib for non-embed package must not have 'embed' dep"
        );
        let run = match resp.target_spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
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
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected deps map"),
        };
        let src_list = match deps.get("").unwrap() {
            TargetSpecValue::List(v) => v,
            _ => panic!("expected list"),
        };
        assert!(
            !src_list.is_empty(),
            "default dep group must not be empty for a package with go files"
        );
        for entry in src_list {
            let s = match entry {
                TargetSpecValue::String(s) => s,
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

    // ---- integration test helpers ----

    fn build_test_engine(workspace: &std::path::Path) -> Arc<crate::engine::Engine> {
        let mut e = crate::engine::Engine::new(crate::engine::Config {
            root: workspace.to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
        })
        .unwrap();

        e.register_provider(|_| Box::new(crate::pluginhostbin::Provider))
            .unwrap();

        e.register_driver(Box::new(crate::pluginhostbin::Driver))
            .unwrap();

        e.register_managed_driver(Box::new(crate::pluginexec::Driver::new_bash()))
            .unwrap();

        e.register_provider(|root| Box::new(Provider::new(root.to_path_buf()).unwrap()))
            .unwrap();

        Arc::new(e)
    }

    // ---- integration tests (run with `cargo test -- --ignored`) ----

    #[tokio::test]
    #[ignore]
    async fn test_simple_lib_build_lib_e2e() {
        use crate::engine::{OutputMatcher, ResultOptions};

        let sandbox = copy_fixture("simple_lib");
        let engine = build_test_engine(sandbox.path());
        let rs = engine.new_state();
        let addr = make_addr("", "build_lib");

        let result = engine
            .clone()
            .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
            .await
            .unwrap();

        assert!(
            !result.artifacts.is_empty(),
            "build_lib should produce at least one artifact"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn test_with_dep_cmd_build_e2e() {
        use crate::engine::{OutputMatcher, ResultOptions};

        let sandbox = copy_fixture("with_dep");
        let engine = build_test_engine(sandbox.path());
        let rs = engine.new_state();
        let addr = make_addr("cmd", "build");

        let result = engine
            .clone()
            .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
            .await
            .unwrap();

        assert!(
            !result.artifacts.is_empty(),
            "cmd build should produce at least one artifact"
        );
    }

    // ---- list() tests ----

    async fn provider_list(p: &Provider, package: &str) -> Vec<String> {
        let ctoken = StdCancellationToken::new();
        let req = ListRequest {
            request_id: "test".to_string(),
            package: PkgBuf::from(package),
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

    #[tokio::test]
    async fn test_list_simple_lib_no_test_targets() {
        require_go!();
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "").await;
        assert!(
            !names.iter().any(|n| n == "build_test"),
            "expected no build_test in list: {:?}",
            names
        );
        assert!(
            !names.iter().any(|n| n == "test"),
            "expected no test in list: {:?}",
            names
        );
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

    #[tokio::test]
    async fn test_list_test_only_pkg_no_build_lib() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let names = provider_list(&p, "pkg").await;
        assert!(
            !names.iter().any(|n| n == "build_lib"),
            "test-only package should not have build_lib: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn test_test_only_build_test_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("pkg", "build_test"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
        let out = match resp.target_spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("bin"));
    }

    #[tokio::test]
    async fn test_test_only_test_exists() {
        require_go!();
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _resp = provider_get(&p, make_addr("pkg", "test")).await.unwrap();
    }

    // ---- mod-asm ----

    #[tokio::test]
    async fn test_mod_asm_build_lib_driver() {
        require_go!();
        let sandbox = copy_fixture("mod-asm");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
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

        let run = match resp.target_spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };

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
}
