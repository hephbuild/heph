use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::Engine;
use crate::hasync::StdCancellationToken;
use crate::hmemoizer::{Memoizer, WrappedError};
use std::sync::{Arc, Weak};

pub struct RequestState {
    pub engine: Weak<Engine>,
    pub request_id: String,
    pub ctoken: StdCancellationToken,
    pub mem_result: Memoizer<String, Result<crate::engine::result::EResult, WrappedError>>,
    pub mem_execute: Memoizer<String, Result<Vec<OutputArtifact>, WrappedError>>,
}

impl Drop for RequestState {
    fn drop(&mut self) {
        self.ctoken.cancel();
        if let Some(engine) = self.engine.upgrade() {
            if let Ok(mut requests) = engine.requests.lock() {
                requests.remove(&self.request_id);
            }
        }
    }
}

impl Engine {
    pub fn new_state(self: &Arc<Self>) -> Arc<RequestState> {
        let request_id = "".to_string();
        let state = Arc::new(RequestState {
            engine: Arc::downgrade(self),
            request_id: request_id.clone(),
            ctoken: StdCancellationToken::new(),
            mem_execute: Memoizer::new(),
            mem_result: Memoizer::new(),
        });

        if let Ok(mut requests) = self.requests.lock() {
            requests.insert(request_id, Arc::downgrade(&state));
        }

        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_request_state_tracking() -> anyhow::Result<()> {
        let engine = Arc::new(Engine::new(Config {
            root: PathBuf::from("/tmp"),
        })?);

        let rs = engine.new_state();
        let request_id = rs.request_id.clone();

        {
            let requests = engine.requests.lock().unwrap();
            assert!(requests.contains_key(&request_id));
            let weak = requests.get(&request_id).unwrap();
            assert!(weak.upgrade().is_some());
        }

        drop(rs);

        {
            let requests = engine.requests.lock().unwrap();
            assert!(!requests.contains_key(&request_id));
        }

        Ok(())
    }
}
