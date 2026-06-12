use crate::engine::provider::GetError::NotFound;
use crate::engine::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    Provider as EProvider, ProviderFunctionRegistry, State, TargetSpec,
};
use crate::hasync::Cancellable;
use crate::hmemoizer::Memoizer;
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;
use crate::htwalk::{CachedWalker, Ignore};
use crate::pluginbuildfile::run_file::RunResult;
use anyhow::Context;
use enclose::enclose;
use futures::future::BoxFuture;
use starlark::environment::Globals;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// Synthetic package + target the buildfile provider always exposes for the
/// BUILD-file language server. Built (via the `exec` driver) by re-execing the
/// current heph binary as `heph tool build-lsp`. Uncached — it is a long-lived
/// server, not a reproducible artifact.
const LSP_PKG: &str = "@heph/build";
const LSP_NAME: &str = "lsp";

/// Build the synthetic `TargetSpec` for `//@heph/build:lsp`: an `exec` target
/// that runs the current binary's `tool build-lsp` hidden command with caching
/// off. Does not touch the filesystem or any BUILD file.
fn synth_lsp_spec(addr: Addr) -> TargetSpec {
    use crate::htvalue::Value;
    let heph_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "heph".to_string());
    let mut config = HashMap::new();
    config.insert(
        "run".to_string(),
        Value::List(vec![
            Value::String(heph_bin),
            Value::String("tool".to_string()),
            Value::String("build-lsp".to_string()),
        ]),
    );
    // Long-running server: never cache.
    config.insert("cache".to_string(), Value::Bool(false));
    // Pass the editor's full environment through to the server at run time
    // (PATH, locale, tool config, …); `"*"` is the exec wildcard. Not hashed.
    config.insert(
        "runtime_pass_env".to_string(),
        Value::List(vec![Value::String("*".to_string())]),
    );
    TargetSpec {
        addr,
        driver: "exec".to_string(),
        config,
        labels: vec![],
        transitive: Default::default(),
    }
}

pub struct RequestState {}

pub struct Provider {
    pub root: std::path::PathBuf,
    pub build_file_patterns: Vec<glob::Pattern>,
    /// Directories pruned during the BUILD-file walk: engine skip dirs/globs plus
    /// this provider's own `skip` option. See [`crate::htwalk::Ignore`].
    pub skip: Arc<Ignore>,
    /// Driver applied to targets that omit `driver` in their `target(...)` call.
    /// Set via the `defaultDriver` provider option. `None` means a target with no
    /// driver is an error.
    pub default_driver: Option<String>,
    pub requests: Mutex<HashMap<String, RequestState>>,
    /// Cache: pkg name → parsed BUILD file result. Avoids re-parsing the Starlark
    /// AST on every `list`/`get`/`probe` call for the same package (3+ calls per
    /// pkg in a typical run), and dedupes concurrent in-flight parses on the same
    /// pkg. Caches errors too — a failed parse stays failed for the lifetime of
    /// the provider (BUILD file contents don't change mid-session).
    pub(crate) pkg_cache: Memoizer<String, Result<Arc<RunResult>, Arc<anyhow::Error>>>,
    /// Cache: full BUILD-file walk of `root`. `find_packages_sync` does a recursive
    /// readdir of the workspace tree; once per provider lifetime is enough since the
    /// layout doesn't change mid-session. `()` key — single global entry.
    pub(crate) packages_cache: Memoizer<(), Result<Arc<Vec<String>>, Arc<anyhow::Error>>>,
    /// Sync cache: resolved BUILD-file path → parsed result. Populated during Starlark
    /// evaluation (both top-level `run_pkg` and transitive `load(...)` resolution share
    /// the same cache, so a file is parsed at most once per provider lifetime).
    pub(crate) file_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    /// Sync cache: package directory → merged result across every matching BUILD file
    /// in that dir. Loaded once and reused by both `run_pkg` and `load("//pkg", ...)`.
    pub(crate) dir_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    /// Aggregated provider functions, injected once by the engine. Drives the
    /// `heph.<provider>.<fn>` Starlark namespace. Empty until injected (some unit
    /// tests run the provider without an engine).
    pub(crate) function_registry: OnceLock<Arc<ProviderFunctionRegistry>>,
    /// Lazily-built Starlark globals (built from `function_registry` on first eval),
    /// shared with every `BuildFileLoader` so the namespace is built at most once.
    pub(crate) globals: Arc<OnceLock<Globals>>,
    /// Shared cross-run filesystem-walk cache. The package-discovery walk reads
    /// directories through it, so an unchanged tree skips `readdir` entirely (a
    /// BUILD file's *contents* don't change the package set — that's handled by
    /// `pkg_cache`). Disabled until [`with_walker`] is called.
    ///
    /// [`with_walker`]: Provider::with_walker
    pub(crate) walker: Arc<CachedWalker>,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            root: std::path::PathBuf::from("/"),
            build_file_patterns: vec![glob::Pattern::new("BUILD").expect("BUILD literal")],
            skip: Arc::new(Ignore::default()),
            default_driver: None,
            requests: Mutex::new(HashMap::new()),
            pkg_cache: Memoizer::with_tag("buildfile_pkg"),
            packages_cache: Memoizer::with_tag("buildfile_packages"),
            file_cache: Arc::new(Mutex::new(HashMap::new())),
            dir_cache: Arc::new(Mutex::new(HashMap::new())),
            function_registry: OnceLock::new(),
            globals: Arc::new(OnceLock::new()),
            walker: Arc::new(CachedWalker::disabled()),
        }
    }
}

impl Provider {
    pub fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }

    /// Use `walker` (the shared cross-run fs-walk cache) for package discovery, so
    /// an unchanged tree skips `readdir`. Without it the provider walks the tree
    /// live every run (the in-process `packages_cache` only dedupes within a run).
    pub fn with_walker(mut self, walker: Arc<CachedWalker>) -> Self {
        self.walker = walker;
        self
    }

    pub fn from_options(
        root: std::path::PathBuf,
        skip_dirs: &[std::path::PathBuf],
        skip_globs: &[String],
        opts: &crate::engine::config_yaml::Options,
    ) -> anyhow::Result<Self> {
        crate::engine::config_yaml::deny_unknown(
            "buildfile provider",
            opts,
            &["patterns", "skip", "defaultDriver"],
        )?;
        let patterns: Vec<String> =
            crate::engine::config_yaml::decode_opt(opts, "buildfile provider", "patterns")?
                .unwrap_or_else(|| vec!["BUILD".to_string()]);
        let compiled = patterns
            .into_iter()
            .map(|p| {
                glob::Pattern::new(&p).with_context(|| format!("invalid buildfile pattern `{p}`"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        // Engine-wide `fs.skip` globs are merged ahead of this provider's own
        // `skip` option so both prune the same workspace-relative paths.
        let mut globs = skip_globs.to_vec();
        let user_skip: Vec<String> =
            crate::engine::config_yaml::decode_opt(opts, "buildfile provider", "skip")?
                .unwrap_or_default();
        globs.extend(user_skip);
        let skip = Ignore::new(skip_dirs, &globs)?;
        let default_driver: Option<String> =
            crate::engine::config_yaml::decode_opt(opts, "buildfile provider", "defaultDriver")?;
        Ok(Self {
            root,
            build_file_patterns: compiled,
            skip: Arc::new(skip),
            default_driver,
            ..Self::default()
        })
    }
}

/// Recursively discover packages under `path`, reading each directory through
/// the shared [`CachedWalker`] (so an unchanged tree skips `readdir`). Filtering
/// (build-file pattern, skip-dir pruning) is applied here.
fn find_packages_sync(
    walker: &CachedWalker,
    path: &std::path::Path,
    root: &std::path::Path,
    patterns: &[glob::Pattern],
    skip: &Ignore,
    packages: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let listing = walker.read_dir(path)?;
    let mut has_build_file = false;
    for entry in &listing.entries {
        match entry.kind {
            crate::htwalk::EntryKind::File | crate::htwalk::EntryKind::Symlink => {
                if patterns.iter().any(|p| p.matches(&entry.name)) {
                    has_build_file = true;
                }
            }
            crate::htwalk::EntryKind::Dir => {
                let entry_path = path.join(&entry.name);
                let rel = entry_path.strip_prefix(root).unwrap_or(&entry_path);
                if skip.prune_dir(&entry_path, rel) {
                    continue;
                }
                find_packages_sync(walker, &entry_path, root, patterns, skip, packages)?;
            }
            crate::htwalk::EntryKind::Other => {}
        }
    }

    if has_build_file {
        let mut current = path;
        while let Ok(rel) = current.strip_prefix(root) {
            let pkg_name = rel.to_string_lossy().to_string();
            packages.insert(pkg_name);

            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                break;
            }
        }
    }

    Ok(())
}

impl Provider {}

impl EProvider for Provider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "buildfile".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            // The synthetic LSP package lists exactly its one target.
            if req.package == LSP_PKG {
                let item = Ok(ListResponse {
                    addr: Addr::new(
                        req.package.clone(),
                        LSP_NAME.to_string(),
                        Default::default(),
                    ),
                });
                return Ok(Box::new(std::iter::once(item))
                    as Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>);
            }
            // A package inside a skipped subtree lists nothing — matching what the
            // package walk would have surfaced.
            if self
                .skip
                .prunes_package(&self.root, std::path::Path::new(req.package.as_str()))
            {
                return Ok(Box::new(std::iter::empty())
                    as Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>);
            }
            let res = self.run_pkg(req.package.as_str()).await?;

            let items: Vec<anyhow::Result<ListResponse>> = res
                .targets
                .iter()
                .map(|p| {
                    Ok(ListResponse {
                        addr: Addr::new(req.package.clone(), p.name.clone(), Default::default()),
                    })
                })
                .collect();

            Ok(Box::new(items.into_iter())
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
            let packages = self
                .packages_cache
                .once(
                    (),
                    enclose!((self.root => root, self.build_file_patterns => patterns, self.skip => skip, self.walker => walker) move || async move {
                        let packages = crate::process_supervisor::block_or_inline(move || {
                            // Recursion reads dirs through the shared walker, so an
                            // unchanged tree is served from the cross-run fswalk cache.
                            let mut packages = std::collections::HashSet::new();
                            find_packages_sync(&walker, &root, &root, &patterns, &skip, &mut packages)?;
                            Ok::<_, anyhow::Error>(packages.into_iter().collect::<Vec<String>>())
                        })?;
                        Ok(Arc::new(packages))
                    }),
                )
                .await
                .map_err(crate::hmemoizer::unwrap_arc_err)?;

            let items: Vec<anyhow::Result<ListPackageResponse>> = packages
                .iter()
                .map(|p| {
                    Ok(ListPackageResponse {
                        pkg: PkgBuf::from(p.as_str()),
                    })
                })
                // Always surface the synthetic LSP package alongside the walked ones.
                .chain(std::iter::once(Ok(ListPackageResponse {
                    pkg: PkgBuf::from(LSP_PKG),
                })))
                .collect();

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
            // The synthetic LSP package never hits the filesystem: only `:lsp`
            // resolves, any other name in it is NotFound.
            if req.addr.package == LSP_PKG {
                if req.addr.name == LSP_NAME {
                    return Ok(GetResponse {
                        target_spec: synth_lsp_spec(req.addr.clone()),
                    });
                }
                return Err(NotFound);
            }
            // A target inside a skipped subtree does not resolve.
            if self
                .skip
                .prunes_package(&self.root, std::path::Path::new(req.addr.package.as_str()))
            {
                return Err(NotFound);
            }
            let res = self
                .run_pkg(req.addr.package.as_str())
                .await
                .map_err(|e: anyhow::Error| GetError::Other(e))?;

            for p in res.targets.iter() {
                if p.name == req.addr.name {
                    let driver = if p.driver.is_empty() {
                        self.default_driver.clone().ok_or_else(|| {
                            GetError::Other(anyhow::anyhow!(
                                "target {} has no driver and no defaultDriver is configured for the buildfile provider",
                                req.addr.format()
                            ))
                        })?
                    } else {
                        p.driver.clone()
                    };
                    return Ok(GetResponse {
                        target_spec: TargetSpec {
                            addr: req.addr.clone(),
                            driver,
                            config: p.config.clone(),
                            labels: p.labels.clone(),
                            transitive: p.transitive.clone(),
                        },
                    });
                }
            }

            Err(NotFound)
        })
    }

    fn probe<'a>(
        &'a self,
        req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move {
            let res = self.run_pkg(req.package.as_str()).await?;

            Ok(ProbeResponse {
                states: res
                    .states
                    .iter()
                    .map(|p| State {
                        package: req.package.clone(),
                        provider: p.provider.clone(),
                        state: p.args.clone(),
                    })
                    .collect(),
            })
        })
    }

    fn set_function_registry(&self, reg: Arc<ProviderFunctionRegistry>) {
        // First injection wins; the engine wires exactly once, so a later set
        // (already-injected) is a harmless no-op.
        if self.function_registry.set(reg).is_err() {
            // Registry was already injected; keep the first one.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::config_yaml::Options;
    use crate::engine::provider::GetRequest;
    use crate::hasync::StdCancellationToken;
    use crate::htaddr::parse_addr;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_synth_lsp_target_get_list_and_packages() {
        let tmp = tempdir().unwrap();
        let provider = Provider {
            root: tmp.path().to_path_buf(),
            ..Provider::default()
        };
        let ctoken = StdCancellationToken::new();

        // get(//@heph/build:lsp) → exec driver, cache off, runs `tool build-lsp`.
        let res = provider
            .get(
                GetRequest {
                    request_id: "t".to_string(),
                    addr: parse_addr("//@heph/build:lsp").unwrap(),
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ctoken,
            )
            .await
            .unwrap();
        let spec = res.target_spec;
        assert_eq!(spec.driver, "exec");
        assert_eq!(
            spec.config.get("cache"),
            Some(&crate::htvalue::Value::Bool(false))
        );
        let run = match spec.config.get("run") {
            Some(crate::htvalue::Value::List(v)) => v.clone(),
            other => panic!("expected run list, got {other:?}"),
        };
        let run_strs: Vec<&str> = run
            .iter()
            .map(|v| match v {
                crate::htvalue::Value::String(s) => s.as_str(),
                _ => panic!("run entries must be strings"),
            })
            .collect();
        assert_eq!(&run_strs[run_strs.len() - 2..], &["tool", "build-lsp"]);
        // Server inherits the editor's full environment at runtime.
        assert_eq!(
            spec.config.get("runtime_pass_env"),
            Some(&crate::htvalue::Value::List(vec![
                crate::htvalue::Value::String("*".to_string())
            ]))
        );

        // Wrong name in the synthetic package is NotFound.
        let miss = provider
            .get(
                GetRequest {
                    request_id: "t".to_string(),
                    addr: parse_addr("//@heph/build:nope").unwrap(),
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ctoken,
            )
            .await;
        assert!(matches!(miss, Err(GetError::NotFound)));

        // list(@heph/build) yields exactly `lsp`.
        let listed: Vec<String> = provider
            .list(
                ListRequest {
                    request_id: "t".to_string(),
                    package: PkgBuf::from(LSP_PKG),
                    states: vec![],
                },
                &ctoken,
            )
            .await
            .unwrap()
            .map(|r| r.unwrap().addr.name.clone())
            .collect();
        assert_eq!(listed, vec!["lsp".to_string()]);

        // list_packages includes the synthetic package.
        let pkgs: Vec<String> = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap()
            .map(|r| r.unwrap().pkg.to_string())
            .collect();
        assert!(pkgs.iter().any(|p| p == LSP_PKG));
    }

    /// Package discovery is cached across runs through the shared walker: a fresh
    /// provider sharing the fswalk db reuses the discovered set for an unchanged
    /// tree, and a newly-added package (which bumps a recorded dir's mtime) is
    /// re-discovered.
    #[tokio::test]
    async fn test_list_packages_cross_run_cache() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        // fswalk db outside the walked tree (in production it's under pruned
        // `.heph3`), so its writes don't bump the discovered dirs' mtimes.
        let dbdir = tempdir().unwrap();
        let db = dbdir.path().join("fswalk.db");
        fs::write(root.join("BUILD"), "").unwrap();
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("BUILD"), "").unwrap();

        let list = |p: Provider| async move {
            let ctoken = StdCancellationToken::new();
            let res = p
                .list_packages(
                    ListPackagesRequest {
                        prefix: PkgBuf::from(""),
                    },
                    &ctoken,
                )
                .await
                .unwrap();
            // Drop the always-present synthetic LSP package; this test covers the
            // filesystem walk.
            let mut v: Vec<String> = res
                .map(|r| r.unwrap().pkg.to_string())
                .filter(|p| p != LSP_PKG)
                .collect();
            v.sort();
            v
        };
        let provider = || {
            Provider {
                root: root.to_path_buf(),
                ..Provider::default()
            }
            .with_walker(Arc::new(CachedWalker::open(&db)))
        };

        assert_eq!(
            list(provider()).await,
            vec!["".to_string(), "a".to_string()]
        );

        // Fresh provider sharing the walker db (new run) → same set, served from
        // the cross-run readdir cache for the unchanged tree.
        assert_eq!(
            list(provider()).await,
            vec!["".to_string(), "a".to_string()]
        );

        // Add a new package; bump root mtime so the recorded dir invalidates.
        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("BUILD"), "").unwrap();
        std::fs::File::open(root)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(7200))
            .unwrap();

        assert_eq!(
            list(provider()).await,
            vec!["".to_string(), "a".to_string(), "b".to_string()],
            "a newly-added package is re-discovered"
        );
    }

    #[test]
    fn from_options_defaults_to_build() {
        let dir = tempdir().expect("tempdir");
        let p = Provider::from_options(dir.path().to_path_buf(), &[], &[], &Options::new())
            .expect("from_options");
        let names: Vec<&str> = p.build_file_patterns.iter().map(|p| p.as_str()).collect();
        assert_eq!(names, vec!["BUILD"]);
    }

    #[test]
    fn from_options_reads_patterns() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert(
            "patterns".to_string(),
            serde_yaml::from_str("[BUILD2, \"*.BUILD2\"]").expect("yaml"),
        );
        let p = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts)
            .expect("from_options");
        let names: Vec<&str> = p.build_file_patterns.iter().map(|p| p.as_str()).collect();
        assert_eq!(names, vec!["BUILD2", "*.BUILD2"]);
    }

    #[test]
    fn from_options_rejects_invalid_glob() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert(
            "patterns".to_string(),
            serde_yaml::from_str("[\"[bad\"]").expect("yaml"),
        );
        let err = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts)
            .err()
            .expect("must error");
        assert!(err.to_string().contains("[bad"), "{err}");
    }

    #[test]
    fn from_options_rejects_unknown_key() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert("bogus".to_string(), serde_yaml::Value::Bool(true));
        let err = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts)
            .err()
            .expect("must error");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn from_options_rejects_wrong_type() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert(
            "patterns".to_string(),
            serde_yaml::Value::String("not a list".to_string()),
        );
        let err = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts)
            .err()
            .expect("must error");
        assert!(err.to_string().contains("patterns"), "{err}");
    }

    struct NoopExecutor;
    impl crate::engine::provider::ProviderExecutor for NoopExecutor {
        fn result<'a>(
            &'a self,
            _addr: &'a Addr,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Arc<crate::engine::EResult>>> {
            Box::pin(async { anyhow::bail!("noop") })
        }

        fn query<'a>(
            &'a self,
            _m: &'a crate::htmatcher::Matcher,
            _extra_skip: &'a [String],
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
            Box::pin(async { anyhow::bail!("noop") })
        }
    }

    fn get_req(pkg: &str, name: &str) -> GetRequest {
        GetRequest {
            request_id: "test".to_string(),
            addr: Addr::new(PkgBuf::from(pkg), name.to_string(), Default::default()),
            states: vec![],
            executor: Arc::new(NoopExecutor),
        }
    }

    #[test]
    fn from_options_reads_default_driver() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert(
            "defaultDriver".to_string(),
            serde_yaml::Value::String("exec".to_string()),
        );
        let p = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts)
            .expect("from_options");
        assert_eq!(p.default_driver.as_deref(), Some("exec"));
    }

    #[test]
    fn from_options_default_driver_absent_is_none() {
        let dir = tempdir().expect("tempdir");
        let p = Provider::from_options(dir.path().to_path_buf(), &[], &[], &Options::new())
            .expect("from_options");
        assert!(p.default_driver.is_none());
    }

    #[tokio::test]
    async fn get_applies_default_driver_when_omitted() {
        let tmp_dir = tempdir().unwrap();
        let pkg_path = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), r#"target(name = "t")"#).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            default_driver: Some("exec".to_string()),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let res = provider.get(get_req("p", "t"), &ctoken).await.expect("get");
        assert_eq!(res.target_spec.driver, "exec");
    }

    #[tokio::test]
    async fn get_explicit_driver_overrides_default() {
        let tmp_dir = tempdir().unwrap();
        let pkg_path = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(
            pkg_path.join("BUILD"),
            r#"target(name = "t", driver = "bash")"#,
        )
        .unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            default_driver: Some("exec".to_string()),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let res = provider.get(get_req("p", "t"), &ctoken).await.expect("get");
        assert_eq!(res.target_spec.driver, "bash");
    }

    #[tokio::test]
    async fn get_errors_when_no_driver_and_no_default() {
        let tmp_dir = tempdir().unwrap();
        let pkg_path = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), r#"target(name = "t")"#).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let err = provider
            .get(get_req("p", "t"), &ctoken)
            .await
            .err()
            .expect("must error");
        let msg = format!("{err:?}");
        assert!(msg.contains("no driver"), "{msg}");
    }

    #[tokio::test]
    async fn list_packages_skips_core_dirs_and_globs() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // root/BUILD, root/.heph3/BUILD (core skip), root/vendor/BUILD (glob skip),
        // root/src/BUILD (kept).
        fs::write(root.join("BUILD"), "").unwrap();
        let heph = root.join(".heph3");
        fs::create_dir_all(&heph).unwrap();
        fs::write(heph.join("BUILD"), "").unwrap();
        let vendor = root.join("vendor");
        fs::create_dir_all(&vendor).unwrap();
        fs::write(vendor.join("BUILD"), "").unwrap();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("BUILD"), "").unwrap();

        let mut opts = Options::new();
        opts.insert(
            "skip".to_string(),
            serde_yaml::from_str("[vendor]").expect("yaml"),
        );
        let provider = Provider::from_options(root.to_path_buf(), &[heph.clone()], &[], &opts)
            .expect("provider");

        let ctoken = StdCancellationToken::new();
        let res = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap();
        let packages: Vec<String> = res
            .map(|r| r.unwrap().pkg.to_string())
            // Exclude the always-present synthetic LSP package; this test covers
            // filesystem-based package discovery.
            .filter(|p| p != LSP_PKG)
            .collect();

        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"src".to_string()));
        assert!(
            !packages.contains(&".heph3".to_string()),
            "core dir not pruned"
        );
        assert!(!packages.contains(&"vendor".to_string()), "glob not pruned");
    }

    #[tokio::test]
    async fn list_packages_skips_engine_skip_dirs() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        fs::write(root.join("BUILD"), "").unwrap();
        let vendor = root.join("vendor");
        fs::create_dir_all(&vendor).unwrap();
        fs::write(vendor.join("BUILD"), "").unwrap();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("BUILD"), "").unwrap();

        // `vendor` comes in as an engine skip dir (the resolved `fs.skip`), not
        // the provider's own `skip` option — proving the engine threads it in.
        let provider =
            Provider::from_options(root.to_path_buf(), &[vendor.clone()], &[], &Options::new())
                .expect("provider");

        let ctoken = StdCancellationToken::new();
        let res = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap();
        let packages: Vec<String> = res
            .map(|r| r.unwrap().pkg.to_string())
            // Exclude the always-present synthetic LSP package; this test covers
            // filesystem-based package discovery.
            .filter(|p| p != LSP_PKG)
            .collect();

        assert!(packages.contains(&"src".to_string()));
        assert!(
            !packages.contains(&"vendor".to_string()),
            "engine skip dir not pruned: {packages:?}"
        );
    }

    #[tokio::test]
    async fn get_and_list_skip_pruned_packages() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let vendor_dep = root.join("vendor/dep");
        fs::create_dir_all(&vendor_dep).unwrap();
        fs::write(
            vendor_dep.join("BUILD"),
            r#"target(name = "t", driver = "d")"#,
        )
        .unwrap();

        // `vendor` is an engine skip dir; a target directly addressed under it
        // must not resolve, and listing it yields nothing.
        let provider = Provider::from_options(
            root.to_path_buf(),
            &[root.join("vendor")],
            &[],
            &Options::new(),
        )
        .expect("provider");

        let ctoken = StdCancellationToken::new();
        let got = provider.get(get_req("vendor/dep", "t"), &ctoken).await;
        assert!(
            matches!(got, Err(NotFound)),
            "expected NotFound for skipped pkg"
        );

        let listed = provider
            .list(
                ListRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from("vendor/dep"),
                    states: vec![],
                },
                &ctoken,
            )
            .await
            .unwrap();
        assert_eq!(listed.count(), 0);
    }

    #[tokio::test]
    async fn test_list_packages() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   BUILD (root package "")
        //   a/
        //     b/
        //       BUILD
        //   c/
        //     BUILD

        fs::write(root.join("BUILD"), "").unwrap();
        let ab = root.join("a").join("b");
        fs::create_dir_all(&ab).unwrap();
        fs::write(ab.join("BUILD"), "").unwrap();

        let c = root.join("c");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("BUILD"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            ..Provider::default()
        };

        let req = ListPackagesRequest {
            prefix: PkgBuf::from(""),
        };
        let ctoken = StdCancellationToken::new();
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res
            .map(|r| r.unwrap().pkg.to_string())
            // Exclude the always-present synthetic LSP package; this test covers
            // filesystem-based package discovery.
            .filter(|p| p != LSP_PKG)
            .collect();

        assert_eq!(packages.len(), 4);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a/b".to_string()));
        assert!(packages.contains(&"a".to_string())); // parent of a/b
        assert!(packages.contains(&"c".to_string()));
    }

    #[tokio::test]
    async fn test_list_packages_custom_pattern() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   BUILD.heph
        //   a/
        //     BUILD.heph

        fs::write(root.join("BUILD.heph"), "").unwrap();
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("BUILD.heph"), "").unwrap();

        // Should NOT be found
        fs::write(root.join("BUILD"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("BUILD.heph").unwrap()],
            ..Provider::default()
        };

        let req = ListPackagesRequest {
            prefix: PkgBuf::from(""),
        };
        let ctoken = StdCancellationToken::new();
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res
            .map(|r| r.unwrap().pkg.to_string())
            // Exclude the always-present synthetic LSP package; this test covers
            // filesystem-based package discovery.
            .filter(|p| p != LSP_PKG)
            .collect();

        assert_eq!(packages.len(), 2);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a".to_string()));
    }

    #[tokio::test]
    async fn test_list_packages_multiple_patterns() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   BUILD
        //   a/
        //     BUILD.heph
        //   b/
        //     BUILD.other

        fs::write(root.join("BUILD"), "").unwrap();
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("BUILD.heph"), "").unwrap();
        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("BUILD.other"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![
                glob::Pattern::new("BUILD").unwrap(),
                glob::Pattern::new("BUILD.heph").unwrap(),
                glob::Pattern::new("BUILD.other").unwrap(),
            ],
            ..Provider::default()
        };

        let req = ListPackagesRequest {
            prefix: PkgBuf::from(""),
        };
        let ctoken = StdCancellationToken::new();
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res
            .map(|r| r.unwrap().pkg.to_string())
            // Exclude the always-present synthetic LSP package; this test covers
            // filesystem-based package discovery.
            .filter(|p| p != LSP_PKG)
            .collect();

        assert_eq!(packages.len(), 3);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a".to_string()));
        assert!(packages.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn test_list_packages_glob_pattern() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   foo.BUILD
        //   a/
        //     bar.BUILD
        //   b/
        //     notabuild.txt   (must NOT match)

        fs::write(root.join("foo.BUILD"), "").unwrap();
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("bar.BUILD"), "").unwrap();
        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("notabuild.txt"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("*.BUILD").unwrap()],
            ..Provider::default()
        };

        let req = ListPackagesRequest {
            prefix: PkgBuf::from(""),
        };
        let ctoken = StdCancellationToken::new();
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res
            .map(|r| r.unwrap().pkg.to_string())
            // Exclude the always-present synthetic LSP package; this test covers
            // filesystem-based package discovery.
            .filter(|p| p != LSP_PKG)
            .collect();

        assert_eq!(packages.len(), 2);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a".to_string()));
        assert!(!packages.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn test_run_pkg_glob_pattern() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
target(
    name = "globtarget",
    driver = "mydriver",
)
"#;
        fs::write(pkg_path.join("my.BUILD"), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("*.BUILD").unwrap()],
            ..Provider::default()
        };

        let result = provider.run_pkg(&pkg_name).await.unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].name, "globtarget");
    }

    #[tokio::test]
    async fn probe_returns_provider_states_from_build_file() {
        use crate::htvalue::Value;

        let tmp_dir = tempdir().unwrap();
        let pkg_name = "p";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
provider_state(provider = "go", root = "src", strict = True)
"#;
        fs::write(pkg_path.join("BUILD"), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let res = provider
            .probe(
                ProbeRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from(pkg_name),
                },
                &ctoken,
            )
            .await
            .unwrap();

        assert_eq!(res.states.len(), 1);
        let s = &res.states[0];
        assert_eq!(s.package, PkgBuf::from(pkg_name));
        assert_eq!(s.provider, "go");
        assert_eq!(s.state.get("root"), Some(&Value::String("src".to_string())));
        assert_eq!(s.state.get("strict"), Some(&Value::Bool(true)));
        assert!(!s.state.contains_key("provider"));
    }

    #[tokio::test]
    async fn probe_missing_provider_kwarg_errors() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "p";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), "provider_state(root=\"x\")").unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };
        let ctoken = StdCancellationToken::new();
        let err = match provider
            .probe(
                ProbeRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from(pkg_name),
                },
                &ctoken,
            )
            .await
        {
            Ok(_) => panic!("missing provider must error"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("missing provider"), "{msg}");
    }
}
