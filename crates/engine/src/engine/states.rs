use crate::engine::Engine;
use crate::engine::provider::State;
use crate::engine::request_state::RequestState;
use enclose::enclose;
use futures::StreamExt;
use futures::stream::FuturesOrdered;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
use std::sync::Arc;

/// What [`Engine::states`] should collect.
#[derive(Debug, Clone, Default)]
pub struct StatesOptions {
    /// Also report the states a package inherits from its ancestors — the full
    /// chain a provider is handed. When false, only the declarations the package
    /// makes itself are kept.
    pub inherited: bool,
    /// Keep only the states addressed to this provider.
    pub provider: Option<String>,
}

/// The `provider_state(...)` declarations visible to one package.
#[derive(Debug, Clone)]
pub struct PackageStates {
    /// The package the states were resolved for.
    pub package: PkgBuf,
    /// Ordered shallow->deep: the root package's declarations first, `package`'s
    /// own last. Each `State::package` records where it was declared. Ordering
    /// is the declaration order a provider sees; precedence between two states
    /// carrying the same key is a provider-defined policy, not an engine one.
    pub states: Vec<State>,
}

impl Engine {
    /// Resolve the `provider_state(...)` declarations for every package matching `m`.
    ///
    /// An exact `Matcher::Package` is probed directly rather than discovered via
    /// `list_packages`: a package that declares state for its subtree need not
    /// itself hold any target, and asking "what applies at `//foo`" must answer
    /// even then. Broader matchers walk the discovered package set.
    ///
    /// Packages are returned in lexical order, each with its states — including
    /// packages that have none, so callers can distinguish "no state here" from
    /// "package not matched".
    ///
    /// `packages()` already answers within `m`'s package scope, so there is no
    /// second filter here. Its tri-state prune is the inclusive one this needs:
    /// states are declared per package and carry no target name, so an arm that
    /// needs one (`Label`, `TreeOutputTo`) shrugs rather than rejecting, and the
    /// package is kept rather than silently dropping its declarations.
    pub async fn states(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &Matcher,
        opts: &StatesOptions,
    ) -> anyhow::Result<Vec<PackageStates>> {
        let pkgs = match m {
            Matcher::Package(p) => vec![p.clone()],
            _ => {
                let mut pkgs: Vec<PkgBuf> = Vec::new();
                for p in self.packages(m, &rs).await? {
                    pkgs.push(PkgBuf::from(p?.as_str()));
                }
                pkgs.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                pkgs
            }
        };

        // `probe_segments` memoizes per (provider, package) within the request,
        // so overlapping ancestor chains are probed once no matter how many
        // packages ask. Keep the fan-out ordered so output is deterministic.
        let mut probes: FuturesOrdered<_> = pkgs
            .into_iter()
            .map(|pkg| {
                enclose!((self => engine, rs) async move {
                    let states = engine.probe_segments(&rs, &pkg).await?;
                    anyhow::Ok((pkg, states))
                })
            })
            .collect();

        let mut out = Vec::new();
        while let Some(res) = probes.next().await {
            let (pkg, states) = res?;
            let mut states: Vec<State> = states
                .iter()
                .filter(|s| opts.inherited || s.package == pkg)
                .filter(|s| opts.provider.as_ref().is_none_or(|p| &s.provider == p))
                .cloned()
                .collect();
            // `probe_segments` walks the chain deepest-first; flip to root-first
            // so output reads down the tree. Ancestors form a chain, so the
            // declaring package's length totally orders them, and the stable
            // sort keeps per-package provider order intact.
            states.sort_by_key(|s| s.package.as_str().len());
            out.push(PackageStates {
                package: pkg,
                states,
            });
        }

        Ok(out)
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
    use hcore::htvalue::Value;

    fn pkg(s: &str) -> PkgBuf {
        PkgBuf::from(s)
    }

    /// Lists `a`, `a/b`, `other`; every package declares one state naming itself,
    /// under the provider name it was registered with.
    struct StatingProvider(&'static str);

    impl crate::engine::provider::Provider for StatingProvider {
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
                let items: Vec<anyhow::Result<ListPackageResponse>> = ["a", "a/b", "other"]
                    .into_iter()
                    .map(|p| {
                        Ok(ListPackageResponse {
                            pkg: PkgBuf::from(p),
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
            let name = self.0.to_string();
            Box::pin(async move {
                Ok(ProbeResponse {
                    states: vec![State {
                        package: pkg.clone(),
                        provider: name,
                        state: [("at".to_string(), Value::String(pkg.to_string()))]
                            .into_iter()
                            .collect(),
                    }],
                })
            })
        }
    }

    fn make_engine(names: &[&'static str]) -> anyhow::Result<Arc<Engine>> {
        let root = tempfile::tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        for name in names {
            engine.register_provider(move |_| Box::new(StatingProvider(name)))?;
        }
        Ok(Arc::new(engine))
    }

    fn declared_in(ps: &PackageStates) -> Vec<&str> {
        ps.states.iter().map(|s| s.package.as_str()).collect()
    }

    #[tokio::test]
    async fn declared_only_keeps_the_packages_own_states() -> anyhow::Result<()> {
        let engine = make_engine(&["p1"])?;
        let rs = engine.new_state();

        let out = engine
            .states(rs, &Matcher::Package(pkg("a/b")), &StatesOptions::default())
            .await?;

        assert_eq!(out.len(), 1);
        assert_eq!(declared_in(&out[0]), vec!["a/b"]);
        Ok(())
    }

    #[tokio::test]
    async fn inherited_returns_the_ancestor_chain_root_first() -> anyhow::Result<()> {
        let engine = make_engine(&["p1"])?;
        let rs = engine.new_state();

        let out = engine
            .states(
                rs,
                &Matcher::Package(pkg("a/b")),
                &StatesOptions {
                    inherited: true,
                    provider: None,
                },
            )
            .await?;

        assert_eq!(out.len(), 1);
        // Root first, the queried package last — the order a reader walks down
        // the tree, the reverse of the deepest-first probe walk.
        assert_eq!(declared_in(&out[0]), vec!["", "a", "a/b"]);
        Ok(())
    }

    #[tokio::test]
    async fn provider_filter_drops_other_providers_states() -> anyhow::Result<()> {
        let engine = make_engine(&["p1", "p2"])?;
        let rs = engine.new_state();

        let out = engine
            .states(
                rs,
                &Matcher::Package(pkg("a")),
                &StatesOptions {
                    inherited: true,
                    provider: Some("p2".to_string()),
                },
            )
            .await?;

        let providers: Vec<&str> = out[0].states.iter().map(|s| s.provider.as_str()).collect();
        assert_eq!(providers, vec!["p2", "p2"]);
        Ok(())
    }

    #[tokio::test]
    async fn prefix_matcher_scans_the_discovered_packages() -> anyhow::Result<()> {
        let engine = make_engine(&["p1"])?;
        let rs = engine.new_state();

        let out = engine
            .states(
                rs,
                &Matcher::PackagePrefix(pkg("a")),
                &StatesOptions::default(),
            )
            .await?;

        // `other` is outside the prefix; `@heph/fs` (the always-on built-in
        // provider's package) is too. Lexical order.
        let pkgs: Vec<&str> = out.iter().map(|p| p.package.as_str()).collect();
        assert_eq!(pkgs, vec!["a", "a/b"]);
        Ok(())
    }

    #[tokio::test]
    async fn exact_matcher_answers_for_a_package_no_provider_lists() -> anyhow::Result<()> {
        let engine = make_engine(&["p1"])?;
        let rs = engine.new_state();

        // `zz` is never listed by `list_packages`, but a state declared at the
        // root still applies to it — the query must say so.
        let out = engine
            .states(
                rs,
                &Matcher::Package(pkg("zz")),
                &StatesOptions {
                    inherited: true,
                    provider: None,
                },
            )
            .await?;

        assert_eq!(declared_in(&out[0]), vec!["", "zz"]);
        Ok(())
    }

    /// A label says nothing about a package, and states carry no target name to
    /// test it against, so every discovered package must survive the scan. The
    /// scoping now happens inside `Engine::packages`, so this asserts the
    /// inclusive half of that prune through the behavior rather than through the
    /// local helper it replaced.
    #[tokio::test]
    async fn label_matcher_keeps_every_package() -> anyhow::Result<()> {
        let engine = make_engine(&["p1"])?;
        let rs = engine.new_state();

        let out = engine
            .states(
                rs,
                &Matcher::Label("l".to_string()),
                &StatesOptions::default(),
            )
            .await?;

        // `@heph/fs` comes from the always-on built-in provider; it is a package
        // like any other and a label cannot rule it out either.
        let scanned: Vec<&str> = out.iter().map(|ps| ps.package.as_str()).collect();
        assert_eq!(scanned, vec!["@heph/fs", "a", "a/b", "other"]);
        Ok(())
    }
}
