use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::driver::targetdef::TargetDef;
use crate::engine::driver::{ParseRequest, RunRequest};
use crate::engine::provider::{GetError, GetRequest, GetResponse, TargetSpec};
use crate::engine::request_state::RequestState;
use crate::engine::Engine;
use crate::htaddr::Addr;
use std::io;
use std::sync::Arc;

pub trait Artifact {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>>;
}

pub struct Result {
    pub artifacts: Vec<Box<dyn Artifact>>,
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
    pub async fn result(&self, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<Result> {
        self.inner_result(rs, addr).await
    }

    async fn inner_result(&self, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<Result> {
        let spec = self.get_spec(rs.clone(), addr).await?;
        let def = self.get_def(rs.clone(), addr).await?;

        let hashin = "".to_string();

        self.execute_and_cache(rs, addr, &ExecuteOptions{
            hashin: &hashin,
            spec: &spec,
            def: &def,
        }).await
    }

    async fn execute_and_cache(&self, rs: Arc<RequestState>, addr: &Addr, opts: &ExecuteOptions<'_>) -> anyhow::Result<Result> {
        let artifacts = self.execute(rs.clone(), &opts).await?;

        if !opts.def.cache {
            return anyhow::Ok(Result {
                artifacts: artifacts.into_iter().map(|a| Box::new(a) as Box<dyn Artifact>).collect(),
            })
        }

        let cached_artifacts = self.cache_locally(&rs.ctoken, addr, opts.hashin.as_str(), artifacts).await?;

        anyhow::Ok(Result {
            artifacts: cached_artifacts.into_iter().map(|a| Box::new(a) as Box<dyn Artifact>).collect(),
        })
    }

    async fn execute(&self, rs: Arc<RequestState>, opts: &ExecuteOptions<'_>) -> anyhow::Result<Vec<OutputArtifact>> {
        let driver = match self.drivers_by_name.get(&opts.spec.driver) {
            Some(driver) => driver,
            None => anyhow::bail!("driver not found: {}", opts.spec.driver),
        };

        let res = driver.driver.run(RunRequest{
            request_id: &rs.request_id,
            target: &opts.def,
            sandbox_path: "".to_string(),
            tree_root_path: self.cfg.root.to_str().unwrap().to_string(),
            inputs: vec![],
            hashin: &opts.hashin,
            stdin: None,
            stdout: None,
            stderr: None,
        }, &rs.ctoken).await?;

        Ok(res.artifacts)
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

        anyhow::bail!("target not found")
    }
}