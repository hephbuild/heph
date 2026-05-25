use crate::debug_hash::DebugHasher;
use crate::engine::driver::targetdef::TargetDef;
use crate::engine::request_state::RequestState;
use crate::engine::{EResult, Engine, OutputMatcher, ResultOptions};
use crate::hmemoizer::unwrap_arc_err;
use crate::htaddr::Addr;
use anyhow::Context;
use async_recursion::async_recursion;
use enclose::enclose;
use itertools::Itertools;
use std::hash::Hasher;
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

#[derive(Clone)]
pub struct ResultMeta {
    pub hashin: String,
}

impl Engine {
    pub async fn meta(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<ResultMeta> {
        let res = rs
            .data
            .mem_meta
            .once(
                addr.clone(),
                enclose!((self => engine, rs, addr) move || async move {
                    engine.inner_meta(rs, &addr).await
                }),
            )
            .await
            .map_err(unwrap_arc_err)?;
        Ok(res)
    }

    #[async_recursion]
    async fn inner_meta(
        self: Arc<Self>,
        rs: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<ResultMeta> {
        // _no_track: meta is invoked from inner_result_addr after parent was
        // already set to addr — tracked get_def would record addr→addr.
        let def = Arc::clone(&self).get_def_no_track(rs.clone(), addr).await?;
        let results = self.clone().inputs_result_meta(rs.clone(), addr).await?;

        let hashouts: Vec<String> = results
            .iter()
            .flat_map(|res| res.artifacts_meta.iter().map(|m| m.hashout.clone()))
            .sorted()
            .collect();

        let hashin = self
            .hashin(&def.target_def, Box::new(hashouts.into_iter()))
            .with_context(|| "hashin")?;

        Ok(ResultMeta { hashin })
    }

    fn hashin(
        &self,
        def: &TargetDef,
        results: Box<dyn Iterator<Item = String>>,
    ) -> anyhow::Result<String> {
        let mut h = DebugHasher::new(
            Xxh3Default::new(),
            format!("hashin_{}", def.addr.format()).as_str(),
        );

        Hasher::write(&mut h, &def.hash);

        for hashout in results {
            Hasher::write(&mut h, hashout.as_bytes());
        }

        Ok(format!("{:x}", h.finish()))
    }

    async fn inputs_result_meta(
        self: Arc<Self>,
        rc: Arc<RequestState>,
        addr: &Addr,
    ) -> anyhow::Result<Vec<Arc<EResult>>> {
        let inputs = Arc::clone(&self)
            .expanded_inputs_for(rc.clone(), addr)
            .await?;

        let fail_fast = rc.fail_fast();
        // Only `hashed=true` inputs contribute their hashout to the parent's
        // hashin. `runtime_deps` (hashed=false) are still materialized via
        // the link/execute path, but must not alter the cache key.
        let futures = inputs.iter().filter(|input| input.hashed).map(|input| {
            enclose!((self => engine, rc, input) async move {
                engine
                    .clone()
                    .result_addr(
                        rc,
                        &input.r#ref.r#ref,
                        OutputMatcher::None,
                        &ResultOptions::default(),
                    )
                    .await
            })
        });

        crate::engine::fanout::join_all_failable(futures, fail_fast).await
    }
}
