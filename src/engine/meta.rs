use async_recursion::async_recursion;
use std::hash::Hasher;
use std::sync::Arc;
use anyhow::Context;
use xxhash_rust::xxh3::Xxh3Default;
use crate::engine::driver::targetdef::{Input, TargetDef};
use crate::engine::{EResult, Engine, ResultOptions};
use crate::engine::request_state::RequestState;
use crate::hmemoizer::WrappedError;
use crate::htaddr::Addr;

#[derive(Clone)]
pub struct ResultMeta {
    pub hashin: String,
}

impl Engine {
    pub async fn meta(self: Arc<Self>, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<ResultMeta> {
        let key = addr.format();
        let res = rs.mem_meta.process_result(key, (self, rs.clone(), addr.clone()), |(engine, rs, addr)| async move {
            engine.inner_meta(rs, &addr).await.map_err(WrappedError::from)
        }).await?;
        Ok(res)
    }

    #[async_recursion]
    async fn inner_meta(self: Arc<Self>, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<ResultMeta> {
        let def = self.get_def(rs.clone(), addr).await?;
        let results = self.clone().inputs_result_meta(rs.clone(), &def.inputs).await?;

        let hashin = self.hashin(def, &results).with_context(|| "hashin")?;

        Ok(ResultMeta {
            hashin,
        })
    }

    fn hashin(&self, def: TargetDef, results: &Vec<EResult>) -> anyhow::Result<String> {
        let mut h = Xxh3Default::new();

        Hasher::write(&mut h, &def.hash);

        for res in results {
            for art in &res.artifacts {
                Hasher::write(&mut h, art.hashout()?.as_bytes());
            }
        }

        Ok(format!("{:x}", h.digest()))
    }

    async fn inputs_result_meta(self: Arc<Self>, rc: Arc<RequestState>, inputs: &Vec<Input>) -> anyhow::Result<Vec<EResult>> {
        let mut result_metas = Vec::new();

        for input in inputs {
            let result_meta = self.clone().result_addr(rc.clone(), &input.r#ref.r#ref, &ResultOptions::default()).await?;
            result_metas.push(result_meta);
        }

        Ok(result_metas)
    }
}
