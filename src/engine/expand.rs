use crate::engine::Engine;
use crate::engine::driver::targetdef::{Input, InputMode};
use crate::engine::request_state::RequestState;
use crate::hmemoizer::unwrap_arc_err;
use crate::htaddr::Addr;
use async_recursion::async_recursion;
use enclose::enclose;
use rustc_hash::FxHashSet;
use std::sync::Arc;

impl Engine {
    /// Memoized expansion of `addr`'s inputs. Called from both `link` and `meta`
    /// for every result — without memoization the whole transparent-group walk
    /// runs twice per addr per request.
    pub(crate) async fn expanded_inputs_for(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Arc<Vec<Input>>> {
        rs.data
            .mem_expanded_inputs
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    // _no_track: result_addr (the upstream entry point) already set
                    // parent=addr before getting here, so tracked get_def would
                    // record addr→addr (spurious self-cycle).
                    let def = Arc::clone(&engine)
                        .get_def_no_track(rs.clone(), &addr)
                        .await?;
                    let mut visited = FxHashSet::default();
                    let expanded = Arc::clone(&engine)
                        .expand_inputs(
                            rs,
                            def.target_def.inputs.clone(),
                            None,
                            &mut visited,
                        )
                        .await?;
                    Ok(Arc::new(expanded))
                }),
            )
            .await
            .map_err(unwrap_arc_err)
    }

    /// Recursively expands transparent (group) inputs, replacing each group with
    /// its constituent deps. Non-transparent inputs pass through unchanged.
    ///
    /// `parent_override` propagates the origin_id and mode from the group's
    /// reference in the parent's input list so that exec driver grouping (SRC_*)
    /// reflects the parent's intent, not the group's internal ids.
    ///
    /// Top-level callers (`parent_override = None`) use an allocate-on-write
    /// `result`: the original `inputs` Vec is returned untouched when no input
    /// is transparent, and we only allocate (and copy the prior pass-throughs)
    /// the first time we actually need to splice expanded deps in. Recursive
    /// callers (`parent_override = Some(...)`) always rewrite every input, so
    /// the result Vec is allocated up front.
    ///
    /// `visited` is borrowed mutably across recursion — expansion is purely
    /// sequential within a single task, so no shared-state synchronisation is
    /// needed (previously held behind `Arc<Mutex<…>>` for no benefit).
    #[async_recursion]
    pub(crate) async fn expand_inputs(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        inputs: Vec<Input>,
        parent_override: Option<(String, InputMode)>,
        visited: &mut FxHashSet<String>,
    ) -> anyhow::Result<Vec<Input>> {
        let mut result: Option<Vec<Input>> = parent_override
            .as_ref()
            .map(|_| Vec::with_capacity(inputs.len()));

        for (idx, input) in inputs.iter().enumerate() {
            let def = Arc::clone(&self)
                .get_def(rs.clone(), &input.r#ref.r#ref)
                .await?;

            if def.target_def.transparent {
                let out = result.get_or_insert_with(|| {
                    let mut v = Vec::with_capacity(inputs.len());
                    if let Some(prefix) = inputs.get(..idx) {
                        v.extend_from_slice(prefix);
                    }
                    v
                });

                let addr_str = input.r#ref.r#ref.format();
                if !visited.insert(addr_str.clone()) {
                    anyhow::bail!("circular group dependency at '{}'", addr_str);
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
                        visited,
                    )
                    .await?;

                visited.remove(&addr_str);

                out.extend(nested);
            } else if let Some((oid, mode)) = parent_override.as_ref() {
                // Recursive call: result Vec was allocated up front.
                result
                    .as_mut()
                    .expect("recursive call allocated result up front")
                    .push(Input {
                        r#ref: input.r#ref.clone(),
                        origin_id: oid.clone(),
                        mode: mode.clone(),
                        annotations: input.annotations.clone(),
                    });
            } else if let Some(out) = result.as_mut() {
                // Top-level, but an earlier transparent input already triggered
                // allocation — copy this passthrough into the new Vec.
                out.push(input.clone());
            }
            // else: top-level, no transparent seen yet, original `inputs` still
            // valid — skip.
        }

        Ok(result.unwrap_or(inputs))
    }
}
