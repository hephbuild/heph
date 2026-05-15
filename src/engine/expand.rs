use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, InputMode};
use crate::engine::request_state::RequestState;
use async_recursion::async_recursion;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

impl Engine {
    /// Recursively expands transparent (group) inputs, replacing each group with
    /// its constituent deps. Non-transparent inputs pass through unchanged.
    ///
    /// `parent_override` propagates the origin_id and mode from the group's
    /// reference in the parent's input list so that exec driver grouping (SRC_*)
    /// reflects the parent's intent, not the group's internal ids.
    #[async_recursion]
    pub(crate) async fn expand_inputs(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        inputs: Vec<Input>,
        parent_override: Option<(String, InputMode)>,
        visited: Arc<Mutex<HashSet<String>>>,
    ) -> anyhow::Result<Vec<Input>> {
        let mut result = Vec::with_capacity(inputs.len());

        for input in inputs {
            let def = self.get_def(rs.clone(), &input.r#ref.r#ref).await?;

            if def.target_def.transparent {
                let addr_str = input.r#ref.r#ref.format();

                {
                    let mut vis = visited.lock().expect("visited lock poisoned");
                    if vis.contains(&addr_str) {
                        anyhow::bail!("circular group dependency at '{}'", addr_str);
                    }
                    vis.insert(addr_str.clone());
                }

                let eff_origin = parent_override
                    .as_ref()
                    .map(|(oid, _)| oid.clone())
                    .unwrap_or_else(|| input.origin_id.clone());
                let eff_mode = parent_override
                    .as_ref()
                    .map(|(_, mode)| mode.clone())
                    .unwrap_or_else(|| input.mode.clone());

                let nested = Arc::clone(&self)
                    .expand_inputs(
                        rs.clone(),
                        def.target_def.inputs.clone(),
                        Some((eff_origin, eff_mode)),
                        Arc::clone(&visited),
                    )
                    .await?;

                visited
                    .lock()
                    .expect("visited lock poisoned")
                    .remove(&addr_str);

                result.extend(nested);
            } else {
                let effective_input = if let Some((ref oid, ref mode)) = parent_override {
                    Input {
                        r#ref: input.r#ref,
                        origin_id: oid.clone(),
                        mode: mode.clone(),
                    }
                } else {
                    input
                };
                result.push(effective_input);
            }
        }

        Ok(result)
    }
}
