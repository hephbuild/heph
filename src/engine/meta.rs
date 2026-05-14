use crate::debug_hash::DebugHasher;
use crate::engine::driver::targetdef::{Input, TargetDef};
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
        let key = addr.format();
        let res = rs
            .data
            .mem_meta
            .once(
                key,
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
        let def = self.get_def(rs.clone(), addr).await?;
        let results = self
            .clone()
            .inputs_result_meta(rs.clone(), &def.target_def.inputs)
            .await?;

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
        inputs: &[Input],
    ) -> anyhow::Result<Vec<EResult>> {
        let futures = inputs.iter().map(|input| {
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

        futures::future::try_join_all(futures).await
    }
}
