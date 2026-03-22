use crate::engine::driver::outputartifact::OutputArtifact;
use crate::engine::Engine;
use crate::hasync::StdCancellationToken;
use crate::hmemoizer::Memoizer;
use std::sync::Arc;

pub struct RequestState {
    pub request_id: String,
    pub ctoken: StdCancellationToken,
    pub mem_execute: Memoizer<String, Arc<Vec<OutputArtifact>>>,
}

impl Drop for RequestState {
    fn drop(&mut self) {
       self.ctoken.cancel();
    }
}

impl Engine {
    pub fn new_state(&self) ->  Arc<RequestState> {
        Arc::new(RequestState{
            request_id: "".to_string(),
            ctoken: StdCancellationToken::new(),
            mem_execute: Memoizer::new(),
        })
    }
}
