use std::path::PathBuf;
use std::sync::Arc;
use anyhow::Context;
use async_recursion::async_recursion;
use futures::future::try_join_all;
use crate::engine::driver::{RunInput, RunRequest};
use crate::engine::driver::inputartifact::{InputArtifact, Type};
use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::engine::Driver;
use crate::engine::provider::TargetSpec;
use crate::engine::request_state::RequestState;
use crate::engine::result::ResultOptions;
use crate::engine::Engine;
use crate::engine::link::{LinkedTargetDef, LinkedTargetDefInput};
use crate::hmemoizer::WrappedError;
use crate::htaddr::Addr;

impl Engine {
    pub(crate) async fn execute(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        spec: &TargetSpec,
        def: &LinkedTargetDef,
        hashin: &str,
    ) -> anyhow::Result<Vec<OutputArtifact>> {
        let key = format!("{}:{}", addr.format(), hashin);
        let driver = self.drivers_by_name.get(&spec.driver).cloned();
        let def = def.clone();
        let root = self.cfg.root.clone();
        let hashin = hashin.to_string();
        let res = rs.mem_execute.process_result(key, (self, rs.clone(), driver, def, root, hashin), |(engine, rs, driver, def, root, hashin)| async move {
            engine.execute_inner(rs, driver, def, root, hashin).await.map_err(WrappedError::from)
        }).await?;
        Ok(res)
    }

    #[async_recursion]
    async fn execute_inner(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        driver: Option<Arc<Driver>>,
        def: LinkedTargetDef,
        root: PathBuf,
        hashin: String,
    ) -> anyhow::Result<Vec<OutputArtifact>> {
        let driver = driver.ok_or_else(|| anyhow::anyhow!("driver not found"))?;

        let deps_result = self.clone().inputs_result_exec(rs.clone(), &def.inputs).await?;

        let res = driver.driver.run(RunRequest {
            request_id: &rs.request_id,
            target: &def.target,
            tree_root_path: root.to_str().unwrap().to_string(),
            inputs: deps_result,
            hashin: &hashin,
            stdin: None,
            stdout: None,
            stderr: None,
        }, &rs.ctoken).await.with_context(|| "run")?;

        Ok(res.artifacts)
    }

    async fn inputs_result_exec(self: Arc<Self>, rc: Arc<RequestState>, inputs: &Vec<LinkedTargetDefInput>) -> anyhow::Result<Vec<RunInput>> {
        let futs = inputs.iter().map(|input| {
            let engine = self.clone();
            let rc = rc.clone();
            let input = input.clone();
            async move {
                let res = engine.result_addr(rc, &input.target.addr, &ResultOptions::default()).await?;
                let run_inputs: Vec<RunInput> = res.artifacts.into_iter().map(|art| RunInput {
                    artifact: InputArtifact {
                        r#type: Type::Dep,
                        origin_id: input.origin_id.clone(),
                        content: art,
                    },
                    origin_id: input.origin_id.clone(),
                }).collect();
                anyhow::Ok(run_inputs)
            }
        });

        let results = try_join_all(futs).await?;
        Ok(results.into_iter().flatten().collect())
    }
}
