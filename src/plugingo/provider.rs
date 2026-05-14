use crate::engine::EResult;
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
    factors_to_args,
};
use crate::plugingo::embed;
use crate::plugingo::factors::{Factors, current_goarch, current_goos};
use crate::plugingo::gen_testmain::analyze_test_main;
use crate::plugingo::pkg_analysis::{
    GoPackage, find_module_for_import, is_stdlib_import_path, parse_go_list_reader,
    parse_go_mod_module_path, parse_go_mod_requires,
};
use crate::plugingo::target_bin;
use crate::plugingo::target_golist;
use crate::plugingo::target_lib;
use crate::plugingo::target_modfiles;
use crate::plugingo::target_std;
use crate::plugingo::target_test;
use crate::plugingo::thirdparty;
use futures::future::{BoxFuture, try_join_all};
use std::collections::{HashMap, HashSet};
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

pub struct Provider {
    workspace_root: PathBuf,
    goroot: String,
    gomodcache: String,
    gopath: String,
    gocache: String,
    go_bin_addr: String,
    /// Cache: golist addr → GoPackage. Memoizes the full executor.result + parse across requests.
    pkg_cache: Memoizer<String, Result<Arc<GoPackage>, Arc<anyhow::Error>>>,
    /// Cache: stdlib import path → direct imports.
    stdlib_imports_cache: Memoizer<String, Result<Arc<Vec<String>>, Arc<anyhow::Error>>>,
}

impl Provider {
    pub fn new(workspace_root: PathBuf) -> anyhow::Result<Self> {
        Self::with_config(workspace_root, Config::default())
    }

    pub fn with_config(workspace_root: PathBuf, config: Config) -> anyhow::Result<Self> {
        let goroot = resolve_goroot()?;
        let gomodcache = resolve_go_env_var("GOMODCACHE")?;
        let gopath = resolve_go_env_var("GOPATH")?;
        let gocache = resolve_go_env_var("GOCACHE")?;
        Ok(Self {
            workspace_root,
            goroot,
            gomodcache,
            gopath,
            gocache,
            go_bin_addr: config.go_bin_addr,
            pkg_cache: Memoizer::new(),
            stdlib_imports_cache: Memoizer::new(),
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

            match kind {
                GoPackageKind::Stdlib { .. } => {
                    let addr = Addr {
                        package: req.package,
                        name: "build_lib".to_string(),
                        args: factors_to_args(&factors),
                    };
                    Ok(Box::new(std::iter::once(Ok(ListResponse { addr })))
                        as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
                }
                GoPackageKind::ThirdParty { .. } => {
                    let addrs = vec![
                        Addr {
                            package: req.package.clone(),
                            name: "_golist".to_string(),
                            args: factors_to_args(&factors),
                        },
                        Addr {
                            package: req.package,
                            name: "build_lib".to_string(),
                            args: factors_to_args(&factors),
                        },
                    ];
                    let responses: Vec<anyhow::Result<ListResponse>> = addrs
                        .into_iter()
                        .map(|addr| Ok(ListResponse { addr }))
                        .collect();
                    Ok(Box::new(responses.into_iter())
                        as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
                }
                GoPackageKind::FirstParty { src_dir, .. } => {
                    let (has_non_test, has_test) = scan_go_files(&src_dir);

                    let mut addrs: Vec<Addr> = Vec::new();

                    if has_non_test {
                        addrs.push(Addr {
                            package: req.package.clone(),
                            name: "_golist".to_string(),
                            args: factors_to_args(&factors),
                        });
                        addrs.push(Addr {
                            package: req.package.clone(),
                            name: "build_lib".to_string(),
                            args: factors_to_args(&factors),
                        });
                        // Always list build; get() returns NotFound for non-main
                        addrs.push(Addr {
                            package: req.package.clone(),
                            name: "build".to_string(),
                            args: factors_to_args(&factors),
                        });
                        // Always list embed; get() returns NotFound if no embed files
                        addrs.push(Addr {
                            package: req.package.clone(),
                            name: "embed".to_string(),
                            args: factors_to_args(&factors),
                        });
                    }

                    if has_test {
                        addrs.push(Addr {
                            package: req.package.clone(),
                            name: "build_test".to_string(),
                            args: factors_to_args(&factors),
                        });
                        addrs.push(Addr {
                            package: req.package,
                            name: "test".to_string(),
                            args: factors_to_args(&factors),
                        });
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

            let mut packages: Vec<anyhow::Result<ListPackageResponse>> = Vec::new();
            collect_go_packages(&search_dir, &self.workspace_root, &mut packages);

            Ok(Box::new(packages.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>>,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move { self.handle_get(req).await })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
    }
}

struct TransitiveDeps {
    /// `(import_path, build_lib_addr)` for every reachable dep (for importcfg).
    libs: Vec<(String, Addr)>,
}

impl Provider {
    async fn handle_get(&self, req: GetRequest) -> Result<GetResponse, GetError> {
        let addr = &req.addr;
        let factors = Factors::from_addr(addr);

        let kind = match decode_package(&addr.package, &self.workspace_root) {
            Some(k) => k,
            None => return Err(GetError::NotFound),
        };

        // Stdlib — no go list needed
        if let GoPackageKind::Stdlib { import_path } = &kind {
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

        // _golist — generate spec without executing go list
        if addr.name == "_golist" {
            return self
                .get_golist_spec(addr.clone(), &kind, &factors)
                .map_err(GetError::Other);
        }

        let import_path = match &kind {
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

        let dir = pkg.dir.as_deref().ok_or_else(|| {
            GetError::Other(anyhow::anyhow!("package '{}' has no Dir", import_path))
        })?;
        let src_dir = Path::new(dir);

        // The module root drives which directory `go list` runs from for transitive deps.
        let module_root = match &kind {
            GoPackageKind::FirstParty { module_root, .. } => module_root.clone(),
            GoPackageKind::ThirdParty { module_root, .. } => module_root.clone(),
            GoPackageKind::Stdlib { .. } => return Err(GetError::NotFound),
        };

        match addr.name.as_str() {
            "build_lib" => {
                let transitive = self
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &[],
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let embed_addr = if !pkg.embed_patterns.is_empty() {
                    Some(self.make_addr_with_name(&addr.package, "embed", &factors))
                } else {
                    None
                };

                let spec = match &kind {
                    GoPackageKind::ThirdParty { .. } => thirdparty::build_spec(
                        addr.clone(),
                        &pkg,
                        &factors,
                        &transitive.libs,
                        &self.go_bin_addr,
                        &self.goroot,
                    ),
                    _ => {
                        let pkg_str = addr.package.as_str();
                        let src_addrs: Vec<Addr> = pkg
                            .go_files
                            .iter()
                            .map(|f| {
                                let rel = if pkg_str.is_empty() {
                                    f.clone()
                                } else {
                                    format!("{}/{}", pkg_str, f)
                                };
                                pluginfs::file_addr(&rel)
                            })
                            .collect();
                        target_lib::build_spec(
                            addr.clone(),
                            &import_path,
                            pkg.name.as_deref().unwrap_or(""),
                            &factors,
                            &transitive.libs,
                            &src_addrs,
                            &self.go_bin_addr,
                            &self.goroot,
                            embed_addr.as_ref(),
                        )
                    }
                };
                Ok(GetResponse { target_spec: spec })
            }
            "build" => {
                if pkg.name.as_deref() != Some("main") {
                    return Err(GetError::NotFound);
                }

                let own_lib_addr = self.build_lib_addr(addr, &factors);
                let mut transitive = self
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
                let mut test_file_args: Vec<(&'static str, String)> = Vec::new();
                for f in &pkg.test_go_files {
                    test_file_args.push(("_test", src_dir.join(f).to_string_lossy().into_owned()));
                }
                for f in &pkg.xtest_go_files {
                    test_file_args.push(("_xtest", src_dir.join(f).to_string_lossy().into_owned()));
                }
                let file_refs: Vec<(&str, &str)> = test_file_args
                    .iter()
                    .map(|(prefix, path)| (*prefix, path.as_str()))
                    .collect();
                let analysis =
                    analyze_test_main(&import_path, &file_refs).map_err(GetError::Other)?;
                let spec =
                    target_test::testmain_spec(addr.clone(), &import_path, &analysis, &factors);
                Ok(GetResponse { target_spec: spec })
            }
            // Intermediate test target: compile GoFiles + TestGoFiles in test mode.
            "build_lib#test" => {
                let has_tests = !pkg.test_go_files.is_empty();
                let has_go = !pkg.go_files.is_empty();
                if !has_tests && !has_go {
                    return Err(GetError::NotFound);
                }
                let test_extra: Vec<String> = pkg.test_imports.clone();
                let transitive = self
                    .collect_direct_libs(
                        Arc::clone(&req.executor),
                        &pkg,
                        &test_extra,
                        &factors,
                        &module_root,
                    )
                    .await
                    .map_err(GetError::Other)?;

                let pkg_str = addr.package.as_str();
                let src_addrs: Vec<Addr> = pkg
                    .go_files
                    .iter()
                    .map(|f| {
                        let rel = if pkg_str.is_empty() {
                            f.clone()
                        } else {
                            format!("{}/{}", pkg_str, f)
                        };
                        pluginfs::file_addr(&rel)
                    })
                    .collect();
                let test_src_addrs: Vec<Addr> = pkg
                    .test_go_files
                    .iter()
                    .map(|f| {
                        let rel = if pkg_str.is_empty() {
                            f.clone()
                        } else {
                            format!("{}/{}", pkg_str, f)
                        };
                        pluginfs::file_addr(&rel)
                    })
                    .collect();

                let embed_addr = if !pkg.embed_patterns.is_empty() {
                    Some(self.make_addr_with_name(&addr.package, "embed", &factors))
                } else {
                    None
                };

                let spec = target_test::build_lib_test_spec(
                    addr.clone(),
                    &import_path,
                    pkg.name.as_deref().unwrap_or(""),
                    &factors,
                    &transitive.libs,
                    &src_addrs,
                    &test_src_addrs,
                    &self.go_bin_addr,
                    &self.goroot,
                    embed_addr.as_ref(),
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
                    test_go_files: vec![],
                    xtest_go_files: vec![],
                    embed_patterns: vec![],
                    embed_files: vec![],
                    imports: pkg.xtest_imports.clone(),
                    test_imports: vec![],
                    xtest_imports: vec![],
                    standard: false,
                    module: pkg.module.clone(),
                    match_: vec![],
                    incomplete: false,
                    error: None,
                };
                let mut transitive = self
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

                let pkg_str = addr.package.as_str();
                let xtest_src_addrs: Vec<Addr> = pkg
                    .xtest_go_files
                    .iter()
                    .map(|f| {
                        let rel = if pkg_str.is_empty() {
                            f.clone()
                        } else {
                            format!("{}/{}", pkg_str, f)
                        };
                        pluginfs::file_addr(&rel)
                    })
                    .collect();

                let spec = target_test::build_lib_xtest_spec(
                    addr.clone(),
                    &import_path,
                    pkg.name.as_deref().unwrap_or(""),
                    &factors,
                    &transitive.libs,
                    &xtest_src_addrs,
                    &self.go_bin_addr,
                    &self.goroot,
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
                    test_go_files: vec![],
                    xtest_go_files: vec![],
                    embed_patterns: vec![],
                    embed_files: vec![],
                    imports: testmain_imports.iter().map(|s| s.to_string()).collect(),
                    test_imports: vec![],
                    xtest_imports: vec![],
                    standard: false,
                    module: pkg.module.clone(),
                    match_: vec![],
                    incomplete: false,
                    error: None,
                };
                let mut transitive = self
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
                let testmain_src_addr = Addr {
                    package: addr.package.clone(),
                    name: "testmain".to_string(),
                    args: factors_to_args(&factors),
                };

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
                let transitive = self
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
                let spec = target_test::test_spec(addr.clone(), build_test_addr);
                Ok(GetResponse { target_spec: spec })
            }
            "embed" => {
                if pkg.embed_files.is_empty() {
                    return Err(GetError::NotFound);
                }
                let spec = embed::build_spec(addr.clone(), &pkg.embed_patterns, src_dir)
                    .map_err(GetError::Other)?;
                Ok(GetResponse { target_spec: spec })
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
                let go_mod_addr = Addr {
                    package: crate::htpkg::PkgBuf::from(module_root_rel.to_string_lossy().as_ref()),
                    name: "_go_mod".to_string(),
                    args: Default::default(),
                };
                // Use a pluginfs glob directly instead of _go_src: _go_src spec
                // generation calls executor.result(_golist), which would deadlock.
                let pkg = addr.package.as_str();
                let src_glob = if pkg.is_empty() {
                    "*.go".to_string()
                } else {
                    format!("{}/*.go", pkg)
                };
                let go_src_glob_addr = pluginfs::glob_addr(&src_glob, &[]);
                target_golist::build_spec_firstparty(
                    addr,
                    import_path,
                    module_root,
                    factors,
                    &self.go_bin_addr,
                    &self.goroot,
                    &go_mod_addr,
                    &go_src_glob_addr,
                )?
            }
            GoPackageKind::ThirdParty {
                module,
                subpath,
                module_root,
                ..
            } => {
                let import_path = if subpath.is_empty() {
                    module.clone()
                } else {
                    format!("{}/{}", module, subpath)
                };
                let module_root_rel = module_root
                    .strip_prefix(&self.workspace_root)
                    .unwrap_or(module_root);
                let go_mod_addr = Addr {
                    package: crate::htpkg::PkgBuf::from(module_root_rel.to_string_lossy().as_ref()),
                    name: "_go_mod".to_string(),
                    args: Default::default(),
                };
                target_golist::build_spec_thirdparty(
                    addr,
                    &import_path,
                    module_root,
                    factors,
                    &self.go_bin_addr,
                    &self.goroot,
                    &go_mod_addr,
                )?
            }
            GoPackageKind::Stdlib { .. } => {
                anyhow::bail!("_golist is not available for stdlib packages")
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

    /// Fetch the direct imports of a stdlib package, memoized by import path.
    /// Used to recurse into stdlib transitive deps when building the importcfg.
    async fn get_stdlib_pkg_imports(&self, import_path: &str) -> anyhow::Result<Arc<Vec<String>>> {
        let key = import_path.to_string();
        let go_bin = format!("{}/bin/go", self.goroot);
        let import_path_owned = import_path.to_string();

        self.stdlib_imports_cache
            .once(key, move || async move {
                let output = tokio::process::Command::new(&go_bin)
                    .args(["list", "-json", "-e", &import_path_owned])
                    .env_remove("GOFLAGS")
                    .output()
                    .await
                    .with_context(|| format!("go list -json -e {}", import_path_owned))?;

                let pkgs = parse_go_list_reader(output.stdout.as_slice())?;

                let imports = pkgs
                    .into_values()
                    .next()
                    .map(|p| p.imports)
                    .unwrap_or_default();

                Ok(Arc::new(imports))
            })
            .await
            .map_err(unwrap_arc_err)
    }

    /// Read and parse the single package from a `_golist` target's output artifact.
    ///
    /// The full executor.result call is inside the once closure so the engine pipeline
    /// (including disk cache lookup) runs at most once per golist addr per Provider lifetime.
    /// Concurrent callers for the same addr share one future; subsequent callers get the
    /// cached result immediately without hitting the engine at all.
    async fn read_golist_package(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        golist_addr: &Addr,
    ) -> anyhow::Result<Arc<GoPackage>> {
        let key = golist_addr.format();
        let golist_addr = golist_addr.clone();

        self.pkg_cache
            .once(key, move || async move {
                let result: EResult = executor.result(&golist_addr).await?;
                let pkg = tokio::task::spawn_blocking(move || {
                    for artifact in &result.artifacts {
                        if let Some(entry_result) = artifact.walk()?.next() {
                            let entry = entry_result?;
                            let map = parse_go_list_reader(entry.data)?;
                            return map
                                .into_values()
                                .next()
                                .ok_or_else(|| anyhow::anyhow!("_golist produced empty output"));
                        }
                    }
                    anyhow::bail!("_golist produced no output")
                })
                .await
                .map_err(|e| anyhow::anyhow!("read_golist_package task panicked: {e}"))??;
                Ok(Arc::new(pkg))
            })
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
        Addr {
            package: package.clone(),
            name: name.to_string(),
            args: factors_to_args(factors),
        }
    }

    /// Collect all transitive lib addresses for a package's imports, recursively.
    ///
    /// Each BFS frontier is processed concurrently via `try_join_all`. Each dep's
    /// `_golist` target is fetched via `executor.result` (engine pipeline with disk
    /// cache), memoized in `pkg_cache` for the Provider lifetime so repeated calls
    /// for the same dep cost nothing after the first resolution.
    async fn collect_transitive_libs(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        root_pkg: &GoPackage,
        extra_imports: &[String],
        factors: &Factors,
        module_root: &Path,
    ) -> anyhow::Result<TransitiveDeps> {
        let mut visited: HashSet<String> = HashSet::new();
        let mut libs: Vec<(String, Addr)> = Vec::new();

        let go_mod_path = module_root.join("go.mod");
        let (go_mod_requires, workspace_module_path) = if go_mod_path.exists() {
            let content = tokio::fs::read_to_string(&go_mod_path)
                .await
                .with_context(|| format!("reading {}", go_mod_path.display()))?;
            (
                parse_go_mod_requires(&content),
                parse_go_mod_module_path(&content).unwrap_or_default(),
            )
        } else {
            (vec![], String::new())
        };

        let mut frontier: Vec<String> = root_pkg
            .imports
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

        Ok(TransitiveDeps { libs })
    }

    /// Resolve direct imports only (no recursion) — correct for compile steps.
    async fn collect_direct_libs(
        &self,
        executor: Arc<dyn ProviderExecutor>,
        root_pkg: &GoPackage,
        extra_imports: &[String],
        factors: &Factors,
        module_root: &Path,
    ) -> anyhow::Result<TransitiveDeps> {
        let go_mod_path = module_root.join("go.mod");
        let (go_mod_requires, workspace_module_path) = if go_mod_path.exists() {
            let content = tokio::fs::read_to_string(&go_mod_path)
                .await
                .with_context(|| format!("reading {}", go_mod_path.display()))?;
            (
                parse_go_mod_requires(&content),
                parse_go_mod_module_path(&content).unwrap_or_default(),
            )
        } else {
            (vec![], String::new())
        };

        let imports: Vec<String> = root_pkg
            .imports
            .iter()
            .chain(extra_imports.iter())
            .filter(|i| *i != "unsafe" && *i != "C")
            .cloned()
            .collect::<std::collections::HashSet<_>>()
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
        if is_stdlib_import_path(import_path) {
            let addr = encode_stdlib(import_path, factors);
            let sub_imports = self
                .get_stdlib_pkg_imports(import_path)
                .await
                .map(|v| v.as_ref().clone())
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

        let golist_addr = Addr {
            package: dep_addr.package.clone(),
            name: "_golist".to_string(),
            args: dep_addr.args.clone(),
        };

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

use crate::engine::provider::{ProbeRequest, ProbeResponse};
use anyhow::Context;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::provider::{GetError, GetRequest, Provider as ProviderTrait};
    use crate::engine::{ArtifactMeta, EResult};
    use crate::hartifactcontent::{Content, WalkEntry};
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

    // --- Test executor that resolves _golist by running go list directly ---

    struct GoListTestExecutor {
        workspace_root: PathBuf,
    }

    struct StringContent(String);

    impl Content for StringContent {
        fn reader(&self) -> anyhow::Result<Box<dyn io::Read>> {
            Ok(Box::new(io::Cursor::new(self.0.as_bytes().to_vec())))
        }

        fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<WalkEntry>> + '_>> {
            let entry = WalkEntry {
                path: PathBuf::from("package.json"),
                data: Box::new(io::Cursor::new(self.0.as_bytes().to_vec())),
                x: false,
            };
            Ok(Box::new(std::iter::once(Ok(entry))))
        }

        fn hashout(&self) -> anyhow::Result<String> {
            Ok("test_hashout".to_string())
        }
    }

    impl ProviderExecutor for GoListTestExecutor {
        fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<EResult>> {
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

                let (import_path, module_root) = match &kind {
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
                    GoPackageKind::Stdlib { .. } => {
                        anyhow::bail!("GoListTestExecutor: stdlib not supported")
                    }
                };

                // Run go list (no -deps) for just this package
                let packages = run_go_list(&import_path, &factors, &module_root).await?;
                // Return the single package as JSON
                let pkg = packages.into_values().next().ok_or_else(|| {
                    anyhow::anyhow!("go list returned no packages for {}", import_path)
                })?;
                let json = serde_json::to_string(&pkg).context("serialize package")?;

                Ok(EResult {
                    artifacts: vec![Arc::new(StringContent(json)) as Arc<dyn Content>],
                    artifacts_meta: vec![ArtifactMeta {
                        hashout: "test_hashout".to_string(),
                    }],
                })
            })
        }
    }

    fn test_executor(workspace_root: &std::path::Path) -> Arc<dyn ProviderExecutor> {
        Arc::new(GoListTestExecutor {
            workspace_root: workspace_root.to_path_buf(),
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
        Addr {
            package: PkgBuf::from(package),
            name: name.to_string(),
            args: Default::default(),
        }
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
        let workspace = p.workspace_root.clone();
        p.get(make_get_req(addr, &workspace), &ctoken).await
    }

    // ---- simple_lib ----

    #[tokio::test]
    async fn test_simple_lib_build_lib_driver() {
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_out_has_a_group() {
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("", "build_lib")).await.unwrap();
        let out = resp.target_spec.config.get("out").unwrap();
        assert!(matches!(out, TargetSpecValue::Map(m) if m.contains_key("a")));
    }

    #[tokio::test]
    async fn test_simple_lib_no_build_target() {
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("", "build")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    #[tokio::test]
    async fn test_simple_lib_golist_target() {
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
            }),
        };
        let resp = p.get(req, &ctoken).await.unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
        let out = match resp.target_spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!("expected map"),
        };
        assert!(out.contains_key("json"));
    }

    // ---- with_dep ----

    #[tokio::test]
    async fn test_with_dep_lib_build_lib_driver() {
        let sandbox = copy_fixture("with_dep");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("lib", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
    }

    #[tokio::test]
    async fn test_with_dep_cmd_build_lib_has_dep_on_lib() {
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
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("@heph/go/std/fmt", "build_lib"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
    }

    #[tokio::test]
    async fn test_stdlib_build_returns_not_found() {
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("@heph/go/std/fmt", "build")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // ---- with_test ----

    #[tokio::test]
    async fn test_with_test_build_test_exists() {
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
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "embed"))
            .await
            .unwrap();
        assert_eq!(resp.target_spec.driver, "bash");
        let out = match resp.target_spec.config.get("out").unwrap() {
            TargetSpecValue::Map(m) => m,
            _ => panic!(),
        };
        assert!(out.contains_key("cfg"));
    }

    // ---- factors ----

    #[tokio::test]
    async fn test_factors_linux_amd64_runtime_env() {
        let sandbox = copy_fixture("simple_lib");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let mut addr = make_addr("", "build_lib");
        addr.args.insert("goos".to_string(), "linux".to_string());
        addr.args.insert("goarch".to_string(), "amd64".to_string());
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
    async fn test_with_embed_embed_target_run_contains_filename() {
        let sandbox = copy_fixture("with_embed");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let resp = provider_get(&p, make_addr("server", "embed"))
            .await
            .unwrap();
        let run = match resp.target_spec.config.get("run").unwrap() {
            TargetSpecValue::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        assert!(
            run.contains("index.html"),
            "embed target run must reference the embedded file: {run}"
        );
    }

    #[tokio::test]
    async fn test_simple_lib_build_lib_no_embed_dep() {
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
        let sandbox = tempfile::tempdir().unwrap();
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let result = provider_get(&p, make_addr("somepkg", "build_lib")).await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // ---- integration test helpers ----

    fn build_test_engine(workspace: &std::path::Path) -> Arc<crate::engine::Engine> {
        let goroot = std::process::Command::new("go")
            .args(["env", "GOROOT"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .expect("go env GOROOT must succeed");
        let go_bin_path = format!("{}/bin/go", goroot);

        let mut e = crate::engine::Engine::new(crate::engine::Config {
            root: workspace.to_path_buf(),
            parallelism: None,
        })
        .unwrap();

        e.register_provider(|_| {
            Box::new(
                crate::pluginstatictarget::Provider::new(vec![crate::pluginstatictarget::Target {
                    addr: "//@heph/bin:go".to_string(),
                    driver: "bash".to_string(),
                    run: Some(format!("cp -p \"{}\" go", go_bin_path)),
                    out: Some("go".to_string()),
                    deps: Default::default(),
                    labels: vec![],
                }])
                .unwrap(),
            )
        })
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
        let sandbox = copy_fixture("test_only");
        let p = Provider::new(sandbox.path().to_path_buf()).unwrap();
        let _resp = provider_get(&p, make_addr("pkg", "test")).await.unwrap();
    }
}
