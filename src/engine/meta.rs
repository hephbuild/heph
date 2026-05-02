use async_recursion::async_recursion;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;
use crate::engine::driver::targetdef::Input;
use crate::engine::{EResult, Engine};
use crate::engine::request_state::RequestState;
use crate::htaddr::Addr;

pub struct ResultMeta {
    pub hashin: String,
}

impl Engine {
    #[async_recursion]
    pub async fn meta(self: Arc<Self>, rs: Arc<RequestState>, addr: &Addr) -> anyhow::Result<ResultMeta> {
        let def = self.get_def(rs.clone(), addr).await?;
        let results = self.clone().inputs_result_meta(rs.clone(), &def.inputs).await?;

        let mut h = Xxh3Default::new();
        def.hash(&mut h);

        for res in results {
            for art in res.artifacts {
                Hasher::write(&mut h, art.hashout()?.as_bytes());
            }
        }

        Ok(ResultMeta { hashin: format!("{:x}", h.digest()) })
    }

    async fn inputs_result_meta(self: Arc<Self>, rc: Arc<RequestState>, inputs: &Vec<Input>) -> anyhow::Result<Vec<EResult>> {
        let mut result_metas = Vec::new();

        for input in inputs {
            let result_meta = self.clone().result_addr(rc.clone(), &input.r#ref.r#ref).await?;
            result_metas.push(result_meta);
        }

        Ok(result_metas)
    }
}
