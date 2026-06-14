use crate::engine::Engine;
use crate::engine::provider::ListPackagesRequest;
use crate::engine::request_state::RequestState;
use enclose::enclose;
use hcore::hmemoizer::unwrap_arc_err;
use hmodel::htmatcher;
use hmodel::htpkg::PkgBuf;
use rustc_hash::FxHashSet;
use std::sync::Arc;

/// Walk the matcher and pick the most specific `Package` / `PackagePrefix`
/// constraint that any path-to-MatchYes must satisfy. Used to narrow the
/// `list_packages` scan; the per-addr matcher still runs afterwards, so
/// over-broad prefixes only cost perf, never correctness.
///
/// `And` arms are intersected — pick the longest narrowing arm. `Or` / `Not`
/// can't be narrowed in general (any arm could match a different prefix),
/// so they fall back to empty.
fn narrowing_prefix(m: &htmatcher::Matcher) -> PkgBuf {
    match m {
        htmatcher::Matcher::Package(p) | htmatcher::Matcher::PackagePrefix(p) => p.clone(),
        htmatcher::Matcher::And(ms) => ms
            .iter()
            .map(narrowing_prefix)
            .max_by_key(|p| p.as_str().len())
            .unwrap_or_else(|| PkgBuf::from("")),
        _ => PkgBuf::from(""),
    }
}

impl Engine {
    pub async fn packages(
        &self,
        m: &htmatcher::Matcher,
        rs: &Arc<RequestState>,
    ) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<String>> + Send>> {
        let prefix = narrowing_prefix(m);

        let mut all_packages = Vec::new();
        // Different providers can list the same package; dedup so callers
        // (e.g. `query`) don't scan a package more than once. First-seen
        // order is preserved for determinism.
        let mut seen: FxHashSet<String> = FxHashSet::default();

        for provider in &self.providers {
            let req = ListPackagesRequest {
                prefix: prefix.clone(),
            };
            let key = format!("{}:{}", provider.name, prefix);

            let pkgs = rs
                .data
                .mem_packages
                .once(
                    key,
                    enclose!((provider, rs) move || async move {
                        let it = provider
                            .provider
                            .list_packages(req, rs.ctoken())
                            .await?;
                        let mut pkgs = Vec::new();
                        for res in it {
                            pkgs.push(res?.pkg.to_string());
                        }
                        Ok(Arc::new(pkgs))
                    }),
                )
                .await
                .map_err(unwrap_arc_err)?;

            for p in pkgs.iter() {
                if seen.insert(p.clone()) {
                    all_packages.push(p.clone());
                }
            }
        }

        Ok(Box::new(all_packages.into_iter().map(Ok)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::provider::{
        ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
        ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    };
    use futures::future::BoxFuture;
    use hcore::hasync::Cancellable;
    use hmodel::htmatcher::Matcher;

    fn pkg(s: &str) -> PkgBuf {
        PkgBuf::from(s)
    }

    // Lists `foo` twice; also used to register two instances under distinct names.
    struct DupPkgs(&'static str);
    impl crate::engine::provider::Provider for DupPkgs {
        fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: self.0.to_string(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<
            'a,
            anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
        > {
            Box::pin(async {
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
            Box::pin(async {
                let items: Vec<anyhow::Result<ListPackageResponse>> = vec![
                    Ok(ListPackageResponse {
                        pkg: PkgBuf::from("foo"),
                    }),
                    Ok(ListPackageResponse {
                        pkg: PkgBuf::from("foo"),
                    }),
                ];
                Ok(Box::new(items.into_iter())
                    as Box<
                        dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                    >)
            })
        }
        fn get<'a>(
            &'a self,
            _req: GetRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
            Box::pin(async { Err(GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            req: ProbeRequest,
            _ctoken: &'a (dyn Cancellable + Send + Sync),
        ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
            let pkg = req.package.clone();
            Box::pin(async move {
                Ok(ProbeResponse {
                    states: vec![crate::engine::provider::State {
                        package: pkg,
                        provider: "dp".to_string(),
                        state: Default::default(),
                    }],
                })
            })
        }
    }

    #[tokio::test]
    async fn packages_dedups_within_and_across_providers() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        // Each provider lists `foo` twice; two providers list it again.
        engine.register_provider(move |_| Box::new(DupPkgs("p1")))?;
        engine.register_provider(move |_| Box::new(DupPkgs("p2")))?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        let pkgs: Vec<String> = engine
            .packages(&Matcher::PackagePrefix(pkg("")), &rs)
            .await?
            .collect::<anyhow::Result<Vec<_>>>()?;

        // The always-on built-in `fs` provider also advertises its `@heph/fs`
        // package; it sorts first because it is registered before `p1`/`p2`.
        // `foo` still appears exactly once despite four listings across providers.
        assert_eq!(pkgs, vec!["@heph/fs".to_string(), "foo".to_string()]);
        Ok(())
    }

    #[test]
    fn package_returns_pkg() {
        assert_eq!(
            narrowing_prefix(&Matcher::Package(pkg("foo/bar"))),
            pkg("foo/bar")
        );
    }

    #[test]
    fn package_prefix_returns_pkg() {
        assert_eq!(
            narrowing_prefix(&Matcher::PackagePrefix(pkg("foo"))),
            pkg("foo")
        );
    }

    #[test]
    fn label_returns_empty() {
        assert_eq!(narrowing_prefix(&Matcher::Label("l".into())), pkg(""));
    }

    #[test]
    fn and_picks_package_arm() {
        let m = Matcher::And(vec![
            Matcher::Package(pkg("foo/bar")),
            Matcher::Label("go_test_data".into()),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("foo/bar"));
    }

    #[test]
    fn and_picks_longest_prefix_arm() {
        let m = Matcher::And(vec![
            Matcher::PackagePrefix(pkg("foo")),
            Matcher::PackagePrefix(pkg("foo/bar")),
            Matcher::Label("l".into()),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("foo/bar"));
    }

    #[test]
    fn and_with_no_pkg_arms_returns_empty() {
        let m = Matcher::And(vec![Matcher::Label("a".into()), Matcher::Label("b".into())]);
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn or_returns_empty() {
        // Or arms could each match different prefixes; can't narrow safely.
        let m = Matcher::Or(vec![
            Matcher::Package(pkg("foo")),
            Matcher::Package(pkg("bar")),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn not_returns_empty() {
        let m = Matcher::Not(Box::new(Matcher::Package(pkg("foo"))));
        assert_eq!(narrowing_prefix(&m), pkg(""));
    }

    #[test]
    fn nested_and_descends() {
        let m = Matcher::And(vec![
            Matcher::Label("l".into()),
            Matcher::And(vec![
                Matcher::Label("m".into()),
                Matcher::Package(pkg("deep/inner")),
            ]),
        ]);
        assert_eq!(narrowing_prefix(&m), pkg("deep/inner"));
    }
}
