use crate::engine::provider::GetError::NotFound;
use crate::engine::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    Provider as EProvider, State, TargetSpec,
};
use crate::hasync::Cancellable;
use crate::hmemoizer::Memoizer;
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;
use crate::pluginbuildfile::run_file::RunResult;
use anyhow::Context;
use enclose::enclose;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct RequestState {}

pub struct Provider {
    pub root: std::path::PathBuf,
    pub build_file_patterns: Vec<glob::Pattern>,
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
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            root: std::path::PathBuf::from("/"),
            build_file_patterns: vec![glob::Pattern::new("BUILD").expect("BUILD literal")],
            requests: Mutex::new(HashMap::new()),
            pkg_cache: Memoizer::with_tag("buildfile_pkg"),
            packages_cache: Memoizer::with_tag("buildfile_packages"),
            file_cache: Arc::new(Mutex::new(HashMap::new())),
            dir_cache: Arc::new(Mutex::new(HashMap::new())),
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

    pub fn from_options(
        root: std::path::PathBuf,
        opts: &crate::engine::config_file::Options,
    ) -> anyhow::Result<Self> {
        crate::engine::config_file::deny_unknown("buildfile provider", opts, &["patterns"])?;
        let patterns: Vec<String> =
            crate::engine::config_file::decode_opt(opts, "buildfile provider", "patterns")?
                .unwrap_or_else(|| vec!["BUILD".to_string()]);
        let compiled = patterns
            .into_iter()
            .map(|p| {
                glob::Pattern::new(&p).with_context(|| format!("invalid buildfile pattern `{p}`"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            root,
            build_file_patterns: compiled,
            ..Self::default()
        })
    }
}

fn find_packages_sync(
    path: &std::path::Path,
    root: &std::path::Path,
    patterns: &[glob::Pattern],
    packages: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let mut has_build_file = false;
    for entry in std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .unwrap_or("")
            .starts_with(".heph")
        {
            // TODO: fix it better
            continue;
        }

        let Ok(ft) = entry.file_type() else { continue };
        let entry_path = entry.path();

        if ft.is_file() {
            if entry_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| patterns.iter().any(|p| p.matches(n)))
                .unwrap_or(false)
            {
                has_build_file = true;
            }
        } else if ft.is_dir() {
            find_packages_sync(&entry_path, root, patterns, packages)?;
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
                    enclose!((self.root => root, self.build_file_patterns => patterns) move || async move {
                        let packages = crate::process_supervisor::block_or_inline(move || {
                            let mut packages = std::collections::HashSet::new();
                            find_packages_sync(&root, &root, &patterns, &mut packages)?;
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
            let res = self
                .run_pkg(req.addr.package.as_str())
                .await
                .map_err(|e: anyhow::Error| GetError::Other(e))?;

            for p in res.targets.iter() {
                if p.name == req.addr.name {
                    return Ok(GetResponse {
                        target_spec: TargetSpec {
                            addr: req.addr.clone(),
                            driver: p.driver.clone(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::config_file::Options;
    use crate::hasync::StdCancellationToken;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn from_options_defaults_to_build() {
        let dir = tempdir().expect("tempdir");
        let p = Provider::from_options(dir.path().to_path_buf(), &Options::new())
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
        let p = Provider::from_options(dir.path().to_path_buf(), &opts).expect("from_options");
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
        let err = Provider::from_options(dir.path().to_path_buf(), &opts)
            .err()
            .expect("must error");
        assert!(err.to_string().contains("[bad"), "{err}");
    }

    #[test]
    fn from_options_rejects_unknown_key() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert("bogus".to_string(), serde_yaml::Value::Bool(true));
        let err = Provider::from_options(dir.path().to_path_buf(), &opts)
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
        let err = Provider::from_options(dir.path().to_path_buf(), &opts)
            .err()
            .expect("must error");
        assert!(err.to_string().contains("patterns"), "{err}");
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
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

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
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

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
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

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
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

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
        use crate::loosespecparser::TargetSpecValue;

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
        assert_eq!(
            s.state.get("root"),
            Some(&TargetSpecValue::String("src".to_string()))
        );
        assert_eq!(s.state.get("strict"), Some(&TargetSpecValue::Bool(true)));
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
