use crate::engine::Engine;
use crate::engine::error::{CycleError, TargetNotFoundError};
use crate::engine::provider::ListRequest;
use crate::engine::request_state::RequestState;
use crate::hmemoizer::downcast_chain_ref;
use crate::htaddr::Addr;
use crate::htmatcher;
use crate::htmatcher::MatchResult;
use crate::htpkg::PkgBuf;
use futures::Stream;
use std::sync::Arc;

impl Engine {
    pub fn query<'a>(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        m: &'a htmatcher::Matcher,
    ) -> impl Stream<Item = anyhow::Result<Addr>> + 'a {
        async_stream::try_stream! {
            for pkg_result in self.packages(m, &rs).await? {
                let pkg = PkgBuf::from(pkg_result?);

                let states = Arc::clone(&self).probe_segments(&rs, &pkg).await?;

                for provider in &self.providers {
                    let it = provider.provider.list(ListRequest {
                        request_id: rs.request_id().to_string(),
                        package: pkg.clone(),
                        states: states
                            .iter()
                            .filter(|s| s.provider == provider.name)
                            .cloned()
                            .collect(),
                    }, rs.ctoken()).await?;

                    for item in it {
                        let addr = item?.addr;

                        if addr.package != pkg {
                            continue;
                        }

                        match m.matches_addr(&addr) {
                            MatchResult::MatchYes => yield addr,
                            MatchResult::MatchNo => {}
                            MatchResult::MatchShrug => {
                                let spec = match Arc::clone(&self).get_spec(rs.clone(), &addr).await {
                                    Ok(spec) => Ok(spec),
                                    Err(e) if downcast_chain_ref::<TargetNotFoundError>(&e).is_some() => continue,
                                    Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => continue,
                                    res => res,
                                }?;

                                match m.matches_spec(&spec) {
                                    MatchResult::MatchYes => yield addr,
                                    MatchResult::MatchNo => {}
                                    MatchResult::MatchShrug => {
                                        let def = match Arc::clone(&self).get_def(rs.clone(), &addr).await {
                                            Ok(def) => def,
                                            // Cycle means this candidate transitively depends on the
                                            // query caller — it cannot be a result. Skip it.
                                            Err(e) if downcast_chain_ref::<CycleError>(&e).is_some() => continue,
                                            Err(e) => Err(e)?,
                                        };

                                        if m.matches(&def.target_def) == MatchResult::MatchYes {
                                            yield addr;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::htmatcher::Matcher;
    use crate::pluginstatictarget;
    use futures::TryStreamExt;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn target(pkg: &str, name: &str, labels: &[&str]) -> pluginstatictarget::Target {
        pluginstatictarget::Target {
            addr: format!("//{pkg}:{name}"),
            driver: "exec".to_string(),
            run: None,
            out: HashMap::new(),
            codegen: None,
            deps: HashMap::new(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn make_engine(targets: Vec<pluginstatictarget::Target>) -> anyhow::Result<Arc<Engine>> {
        let root = tempdir()?;
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let provider = pluginstatictarget::Provider::new(targets)?;
        engine.register_provider(move |_| Box::new(provider))?;
        Ok(Arc::new(engine))
    }

    #[tokio::test]
    async fn query_by_package() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo/bar", "a", &[]),
            target("foo/bar", "b", &[]),
            target("other", "c", &[]),
        ])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Package(PkgBuf::from("foo/bar")))
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), 2);
        assert!(addrs.iter().any(|a| a.name == "a"));
        assert!(addrs.iter().any(|a| a.name == "b"));
        Ok(())
    }

    #[tokio::test]
    async fn query_by_addr() -> anyhow::Result<()> {
        let engine = make_engine(vec![target("foo", "a", &[]), target("foo", "b", &[])])?;

        let rs = engine.new_state();
        let target_addr = Addr::new(PkgBuf::from("foo"), "a".to_string(), Default::default());
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Addr(target_addr))
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn query_by_label_calls_get_spec() -> anyhow::Result<()> {
        let engine = make_engine(vec![
            target("foo", "a", &["//labels:lint"]),
            target("foo", "b", &[]),
        ])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Label("//labels:lint".to_string()))
            .try_collect()
            .await?;

        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, "a");
        Ok(())
    }

    #[tokio::test]
    async fn list_request_receives_probed_states() -> anyhow::Result<()> {
        use crate::engine::provider::{
            ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
            ListPackagesRequest, ListResponse, ProbeRequest, ProbeResponse, State,
        };
        use crate::hasync::Cancellable;
        use futures::future::BoxFuture;
        use std::sync::Mutex;

        struct Recorder {
            list_states: Arc<Mutex<Vec<Vec<State>>>>,
        }
        impl crate::engine::provider::Provider for Recorder {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "rec".to_string(),
                })
            }
            fn list<'a>(
                &'a self,
                req: ListRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>,
            > {
                let states = req.states.clone();
                let rec = Arc::clone(&self.list_states);
                Box::pin(async move {
                    rec.lock().unwrap().push(states);
                    Ok(Box::new(std::iter::empty()) as Box<dyn Iterator<Item = _> + Send>)
                })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ctoken: &'a (dyn Cancellable + Send + Sync),
            ) -> BoxFuture<
                'a,
                anyhow::Result<
                    Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>,
                >,
            > {
                Box::pin(async {
                    let items: Vec<anyhow::Result<ListPackageResponse>> =
                        vec![Ok(ListPackageResponse {
                            pkg: PkgBuf::from("a/b/c"),
                        })];
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
                        states: vec![State {
                            package: pkg,
                            provider: "rec".to_string(),
                            state: Default::default(),
                        }],
                    })
                })
            }
        }

        let root = tempdir()?;
        let list_states = Arc::new(Mutex::new(Vec::<Vec<State>>::new()));
        let list_states_clone = Arc::clone(&list_states);
        let mut engine = Engine::new(Config {
            root: root.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        engine.register_provider(move |_| {
            Box::new(Recorder {
                list_states: Arc::clone(&list_states_clone),
            })
        })?;
        let engine = Arc::new(engine);
        let rs = engine.new_state();

        let _: Vec<Addr> = engine
            .query(rs, &Matcher::Package(PkgBuf::from("a/b/c")))
            .try_collect()
            .await?;

        let recorded = list_states.lock().unwrap();
        assert_eq!(recorded.len(), 1, "list called once");
        let pkgs: Vec<String> = recorded[0]
            .iter()
            .map(|s| s.package.as_str().to_string())
            .collect();
        assert_eq!(pkgs, vec!["a/b/c", "a/b", "a", ""]);
        Ok(())
    }

    #[tokio::test]
    async fn query_empty_when_no_match() -> anyhow::Result<()> {
        let engine = make_engine(vec![target("foo", "a", &[])])?;

        let rs = engine.new_state();
        let addrs: Vec<Addr> = engine
            .query(rs, &Matcher::Package(PkgBuf::from("nonexistent")))
            .try_collect()
            .await?;

        assert!(addrs.is_empty());
        Ok(())
    }
}
