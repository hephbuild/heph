use std::path::PathBuf;
use std::sync::Arc;
use anyhow::Context;
use async_recursion::async_recursion;
use crate::engine::driver::{RunInput, RunRequest};
use crate::engine::driver::inputartifact::{InputArtifact, Type};
use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::driver::targetdef::{Input, TargetDef};
use crate::engine::engine::Driver;
use crate::engine::provider::TargetSpec;
use crate::engine::request_state::RequestState;
use crate::engine::result::ResultOptions;
use crate::engine::Engine;
use crate::hmemoizer::WrappedError;
use crate::htaddr::Addr;

impl Engine {
    pub(crate) async fn execute(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        spec: &TargetSpec,
        def: &TargetDef,
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
        def: TargetDef,
        root: PathBuf,
        hashin: String,
    ) -> anyhow::Result<Vec<OutputArtifact>> {
        let driver = driver.ok_or_else(|| anyhow::anyhow!("driver not found"))?;

        let deps_result = self.clone().inputs_result_exec(rs.clone(), &def.inputs).await?;

        let res = driver.driver.run(RunRequest {
            request_id: &rs.request_id,
            target: &def,
            tree_root_path: root.to_str().unwrap().to_string(),
            inputs: deps_result,
            hashin: &hashin,
            stdin: None,
            stdout: None,
            stderr: None,
        }, &rs.ctoken).await.with_context(|| "run")?;

        Ok(res.artifacts)
    }

    async fn inputs_result_exec(self: Arc<Self>, rc: Arc<RequestState>, inputs: &[Input]) -> anyhow::Result<Vec<RunInput>> {
        let mut result_metas = Vec::new();

        for input in inputs {
            let res = self.clone().result_addr(rc.clone(), &input.r#ref.r#ref, &ResultOptions::default()).await?;
            for art in res.artifacts {
                result_metas.push(RunInput {
                    artifact: InputArtifact {
                        r#type: Type::Dep,
                        origin_id: input.origin_id.clone(),
                        content: art.clone(),
                    },
                    origin_id: input.origin_id.clone(),
                });
            }
        }

        Ok(result_metas)
    }
}
