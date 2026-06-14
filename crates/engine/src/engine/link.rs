use crate::engine::Engine;
use crate::engine::driver::targetdef::{InputMode, TargetDef, path::Content};
use crate::engine::request_state::RequestState;
use enclose::enclose;
use std::sync::Arc;

#[derive(Clone)]
pub struct LinkedTargetDefInput {
    pub target: Arc<TargetDef>,
    pub output_names: Vec<String>,
    pub origin_id: String,
    pub filters: Vec<String>,
    pub annotations: std::collections::BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct LinkedTargetDef {
    pub target: Arc<TargetDef>,
    pub inputs: Vec<LinkedTargetDefInput>,
}

impl Engine {
    pub async fn link(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: Arc<TargetDef>,
    ) -> anyhow::Result<LinkedTargetDef> {
        let expanded_inputs = Arc::clone(&self)
            .expanded_inputs_for(rs.clone(), &def.addr)
            .await?;

        let fail_fast = rs.fail_fast();
        // Only `runtime=true` inputs get materialized into the sandbox.
        // `hash_deps` (runtime=false) are resolved for their hashout via meta
        // but are not extracted nor wired through SRC_*/LIST_*.
        let futures = expanded_inputs.iter().filter(|input| input.runtime).map(|input| {
            enclose!((self => engine, rs, input) async move {
                let input_def: Arc<TargetDef> = Arc::clone(&engine.get_def(rs, &input.r#ref.r#ref).await?.target_def);

                let output_names = if let Some(ref output_name) = input.r#ref.output {
                    if !input_def.outputs.iter().any(|output| &output.group == output_name) {
                        anyhow::bail!("Output '{output_name}' not found in target '{}'", input.r#ref.r#ref)
                    }
                    vec![output_name.clone()]
                } else {
                    input_def.output_names()
                };

                if input.mode == InputMode::Tool {
                    // Each selected output must resolve to exactly 1 FilePath.
                    // Multi-output tool targets (e.g. a `nix` env exposing
                    // `node`/`npm`/`npx`/`yarn` as 4 separate outputs) are fine:
                    // pluginexec symlinks every produced file into the bin dir,
                    // so each output's single file becomes one entry on PATH.
                    for output in input_def
                        .outputs
                        .iter()
                        .filter(|o| output_names.contains(&o.group))
                    {
                        let file_count = output
                            .paths
                            .iter()
                            .filter(|p| matches!(p.content, Content::FilePath(_)))
                            .count();
                        if file_count != 1 {
                            anyhow::bail!(
                                "tool target '{}' output '{}' must produce exactly 1 FilePath, found {}",
                                input.r#ref.r#ref,
                                output.group,
                                file_count
                            );
                        }
                    }
                }

                Ok(LinkedTargetDefInput {
                    target: input_def,
                    output_names,
                    origin_id: input.origin_id.clone(),
                    filters: input.r#ref.filters.clone(),
                    annotations: input.annotations.clone(),
                })
            })
        });

        let inputs = crate::engine::fanout::join_all_failable(futures, fail_fast).await?;

        Ok(LinkedTargetDef {
            target: def,
            inputs,
        })
    }
}
