use crate::engine::Engine;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use crate::htmatcher;

impl Engine {
    pub async fn query(&self, _m: &htmatcher::Matcher, _ctoken: &dyn Cancellable) -> anyhow::Result<Vec<Addr>> {
        Ok(vec![])
    }
}
