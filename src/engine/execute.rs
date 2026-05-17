use crate::engine::Engine;
use crate::engine::driver::inputartifact::{InputArtifact, Type};
use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::driver::{RunInput, RunRequest};
use crate::engine::link::{LinkedTargetDef, LinkedTargetDefInput};
use crate::engine::provider::TargetSpec;
use crate::engine::request_state::RequestState;
use crate::engine::result::{OutputMatcher, ResultOptions};
use crate::htaddr::Addr;
use anyhow::Context;
use async_recursion::async_recursion;
use enclose::enclose;
use futures::future::try_join_all;
use std::path::PathBuf;
use std::sync::Arc;
use std::{fs, io};

impl Engine {
    #[async_recursion]
    pub(crate) async fn execute(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
        spec: &TargetSpec,
        def: &LinkedTargetDef,
        hashin: &str,
    ) -> anyhow::Result<(Vec<OutputArtifact>, PathBuf)> {
        let driver = self
            .drivers_by_name
            .get(&spec.driver)
            .ok_or_else(|| anyhow::anyhow!("driver not found"))
            .cloned()?;

        let deps_result = self
            .clone()
            .inputs_result_exec(rs.clone(), &def.inputs)
            .await?;

        // Acquire semaphore AFTER dep resolution so no permit is held while waiting for
        // deps — prevents the classic diamond deadlock where mid-nodes hold permits while
        // waiting for a leaf that also needs a permit.
        let semaphore = Arc::clone(&self.result_semaphore);
        let _permit = semaphore
            .acquire()
            .await
            .context("result semaphore closed")?;

        let sandbox_dir = {
            let mut dir = self.home.join("sandbox");
            for c in addr.package.components() {
                dir = dir.join(c);
            }
            if addr.args.is_empty() {
                dir.join(format!("__target_{}", addr.name))
            } else {
                dir.join(format!("__target_{}_{}", addr.name, addr.hash_str()))
            }
        };
        match fs::remove_dir_all(&sandbox_dir) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }?;
        fs::create_dir_all(&sandbox_dir)?;

        if def.target.cache {
            eprintln!("[{}] run: {}", driver.name, addr);
        }

        let hashin_owned = hashin.to_owned();
        let res = driver
            .driver
            .run(
                RunRequest {
                    request_id: rs.request_id(),
                    target: &def.target,
                    tree_root_path: self.cfg.root.clone(),
                    inputs: deps_result,
                    hashin: &hashin_owned,
                    stdin: None,
                    stdout: None,
                    stderr: None,
                    sandbox_dir: sandbox_dir.clone(),
                },
                rs.ctoken(),
            )
            .await
            .with_context(|| "run")?;

        Ok((res.artifacts, sandbox_dir))
    }

    async fn inputs_result_exec(
        self: Arc<Self>,
        rc: Arc<RequestState>,
        inputs: &[LinkedTargetDefInput],
    ) -> anyhow::Result<Vec<RunInput>> {
        let futs = inputs.iter().map(|input| {
            enclose!((self => engine, rc, input) async move {
                let res = engine.result_addr(rc, &input.target.addr, OutputMatcher::Exact(input.output_names), &ResultOptions::default()).await?;
                let run_inputs: Vec<RunInput> = res.artifacts.into_iter().map(|art| RunInput {
                    artifact: InputArtifact {
                        r#type: Type::Dep,
                        origin_id: input.origin_id.clone(),
                        content: art,
                    },
                    origin_id: input.origin_id.clone(),
                    source_addr: input.target.addr.clone(),
                    filters: input.filters.clone(),
                }).collect();
                anyhow::Ok(run_inputs)
            })
        });

        let results = try_join_all(futs).await?;
        Ok(results.into_iter().flatten().collect())
    }
}
