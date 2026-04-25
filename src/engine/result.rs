use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::driver::targetdef::TargetDef;
use crate::engine::driver::{ParseRequest, RunRequest};
use crate::engine::provider::{GetError, GetRequest, GetResponse, TargetSpec};
use crate::engine::error::TargetNotFoundError;
use crate::engine::request_state::RequestState;
use crate::engine::Engine;
use crate::htaddr::Addr;
use crate::hmemoizer::WrappedError;
use std::io;
use std::sync::Arc;

use std::fmt;
use futures::TryStreamExt;
use tokio::task::JoinSet;
use crate::htmatcher::Matcher;

pub trait Artifact: Send + Sync {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>>;
}

impl fmt::Debug for dyn Artifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Artifact")
    }
}

#[derive(Clone)]
pub struct EResult {
    pub artifacts: Vec<Arc<dyn Artifact>>,
}

struct ExecuteOptions<'a> {
    hashin: &'a String,
    spec: &'a TargetSpec,
    def: &'a TargetDef
}

impl Artifact for OutputArtifact {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>> {
        anyhow::bail!("not implemented")
    }
}

impl Engine {
    pub async fn result_addr(self: Arc<Self>, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<EResult> {
        let key = addr.format();
        let res = rs.mem_result.process(key, (self, rs.clone(), addr.clone()), |(engine, rs, addr)| async move {
            engine.inner_result_addr(rs, &addr).await.map_err(WrappedError::from)
        }).await?;

        Ok(res)
    }

    pub async fn result(self: Arc<Self>, rs: Arc<RequestState>, matcher: &Matcher) -> anyhow::Result<Vec<EResult>> {
        let mut set = JoinSet::new();

        let stream = self.query(rs.clone(), matcher);
        tokio::pin!(stream);
        while let Some(addr) = stream.try_next().await? {
            let engine = self.clone();
            let rs = rs.clone();
            set.spawn(async move {
                engine.result_addr(rs, &addr).await
            });
        }

        let mut all_res: Vec<EResult> = vec!();
        while let Some(res) = set.join_next().await {
            all_res.push(res??)
        }

        Ok(all_res)
    }

    async fn inner_result_addr(&self, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<EResult> {
        let spec = self.get_spec(rs.clone(), addr).await?;
        let def = self.get_def(rs.clone(), addr).await?;

        let hashin = "".to_string();

        self.execute_and_cache(rs, addr, &ExecuteOptions{
            hashin: &hashin,
            spec: &spec,
            def: &def,
        }).await
    }

    async fn execute_and_cache(&self, rs: Arc<RequestState>, addr: &Addr, opts: &ExecuteOptions<'_>) -> anyhow::Result<EResult> {
        let artifacts = self.execute(rs.clone(), opts).await?;

        if !opts.def.cache {
            return anyhow::Ok(EResult {
                artifacts: artifacts.into_iter().map(|a| Arc::new(a) as Arc<dyn Artifact>).collect(),
            })
        }

        let cached_artifacts = self.cache_locally(&rs.ctoken, addr, opts.hashin.as_str(), artifacts).await?;

        anyhow::Ok(EResult {
            artifacts: cached_artifacts.into_iter().map(|a| Arc::new(a) as Arc<dyn Artifact>).collect(),
        })
    }

    async fn execute(&self, rs: Arc<RequestState>, opts: &ExecuteOptions<'_>) -> anyhow::Result<Vec<OutputArtifact>> {
        let key = format!("{}:{}", opts.spec.addr.format(), opts.hashin);
        let res = rs.mem_execute.process_result(key, (rs.clone(), self.drivers_by_name.get(&opts.spec.driver).cloned(), opts.def.clone(), self.cfg.root.clone(), opts.hashin.clone()), |(rs, driver, def, root, hashin)| async move {
            let driver = match driver {
                Some(driver) => driver,
                None => return Err(WrappedError::from(anyhow::anyhow!("driver not found"))),
            };

            let res = driver.driver.run(RunRequest{
                request_id: &rs.request_id,
                target: &def,
                tree_root_path: root.to_str().unwrap().to_string(),
                inputs: vec![],
                hashin: &hashin,
                stdin: None,
                stdout: None,
                stderr: None,
            }, &rs.ctoken).await.map_err(WrappedError::from)?;

            Ok(res.artifacts)
        }).await?;

        Ok(res)
    }

    pub async fn get_def(&self, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<TargetDef> {
        let spec = self.get_spec(rs.clone(), addr).await?;

        let driver = match self.drivers_by_name.get(&spec.driver) {
            Some(driver) => driver,
            None => anyhow::bail!("driver not found: {}", spec.driver),
        };

        let res = driver.driver.parse(ParseRequest{
            request_id: rs.request_id.clone(),
            target_spec: spec,
        }, &rs.ctoken).await?;

        Ok(res.target_def)
    }

    pub async fn get_spec(&self, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<TargetSpec> {
        for provider in self.providers.iter() {
            let spec = match provider.provider.get(GetRequest{
                request_id: rs.request_id.clone(),
                addr: addr.clone(),
                states: vec![], // TODO
            }, &rs.ctoken).await {
                Ok(GetResponse{target_spec}) => {
                    target_spec
                }
                Err(GetError::NotFound) => {
                    continue
                },
                Err(GetError::Other(e)) => anyhow::bail!(e)
            };

            return anyhow::Ok(spec)
        }
        
        Err(TargetNotFoundError { addr: addr.clone() }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::provider::StaticProvider;
    use crate::htpkg::PkgBuf;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_engine_result_static_provider() -> anyhow::Result<()> {
        let root = tempdir()?;
        let cfg = Config {
            root: root.path().to_path_buf(),
        };

        let mut engine = Engine::new(cfg)?;
        engine.register_provider(|_root| Box::new(StaticProvider {
            targets: vec![
                TargetSpec {
                    addr: Addr {
                        package: PkgBuf::from("some"),
                        name: "t".to_string(),
                        args: Default::default(),
                    },
                    driver: "exec".to_string(),
                    config: Default::default(),
                    labels: vec![],
                    transitive: Default::default(),
                },
            ],
            packages: Default::default(),
        }))?;
        engine.register_managed_driver(Box::new(crate::pluginexec::Driver::new_exec()))?;

        let engine = Arc::new(engine);
        let rs = engine.new_state();
        let addr = Addr {
            package: PkgBuf::from("some"),
            name: "t".to_string(),
            args: Default::default(),
        };

        let result = engine.clone().result_addr(rs.clone(), &addr).await?;

        assert!(result.artifacts.is_empty());

        // Test caching (second call)
        let result2 = engine.clone().result_addr(rs, &addr).await?;
        assert!(result2.artifacts.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_engine_result_not_found() -> anyhow::Result<()> {
        let root = tempdir()?;
        let cfg = Config {
            root: root.path().to_path_buf(),
        };

        let engine = Arc::new(Engine::new(cfg)?);
        let rs = engine.new_state();
        let addr = Addr {
            package: PkgBuf::from("non"),
            name: "existent".to_string(),
            args: Default::default(),
        };

        let result = engine.clone().result_addr(rs, &addr).await;
        assert!(result.is_err());
        let err = result.err().unwrap();

        assert_eq!(err.to_string(), "target not found: //non:existent");
        
        use crate::hmemoizer::WrappedError;
        let is_target_not_found = err.downcast_ref::<TargetNotFoundError>().is_some() ||
            err.downcast_ref::<WrappedError>().and_then(|w| w.0.downcast_ref::<TargetNotFoundError>()).is_some();
        assert!(is_target_not_found);

        Ok(())
    }
}