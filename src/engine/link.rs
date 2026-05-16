use crate::engine::Engine;
use crate::engine::driver::targetdef::{InputMode, TargetDef, path::Content};
use crate::engine::request_state::RequestState;
use enclose::enclose;
use futures::future::try_join_all;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct LinkedTargetDefInput {
    pub target: TargetDef,
    pub output_names: Vec<String>,
    pub origin_id: String,
    pub filters: Vec<String>,
}

#[derive(Clone)]
pub struct LinkedTargetDef {
    pub target: TargetDef,
    pub inputs: Vec<LinkedTargetDefInput>,
}

impl Engine {
    pub(crate) async fn link(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        def: TargetDef,
    ) -> anyhow::Result<LinkedTargetDef> {
        let expanded_inputs = Arc::clone(&self)
            .expand_inputs(
                rs.clone(),
                def.inputs.clone(),
                None,
                Arc::new(Mutex::new(HashSet::new())),
            )
            .await?;

        let futures = expanded_inputs.iter().map(|input| {
            enclose!((self => engine, rs, input) async move {
                let input_def = engine.get_def(rs, &input.r#ref.r#ref).await?.target_def.clone();

                let output_names = if let Some(ref output_name) = input.r#ref.output {
                    if !input_def.outputs.iter().any(|output| &output.group == output_name) {
                        anyhow::bail!("Output '{output_name}' not found in target '{:?}'", input.r#ref.r#ref)
                    }
                    vec![output_name.clone()]
                } else {
                    input_def.output_names()
                };

                if input.mode == InputMode::Tool {
                    let file_count = input_def
                        .outputs
                        .iter()
                        .filter(|o| output_names.contains(&o.group))
                        .flat_map(|o| &o.paths)
                        .filter(|p| matches!(p.content, Content::FilePath(_)))
                        .count();
                    if file_count != 1 {
                        anyhow::bail!(
                            "tool target '{:?}' must produce exactly 1 FilePath output, found {}",
                            input.r#ref.r#ref,
                            file_count
                        );
                    }
                }

                Ok(LinkedTargetDefInput {
                    target: input_def,
                    output_names,
                    origin_id: input.origin_id.clone(),
                    filters: input.r#ref.filters.clone(),
                })
            })
        });

        let inputs = try_join_all(futures).await?;

        Ok(LinkedTargetDef {
            target: def,
            inputs,
        })
    }
}
