//! Per-call context handed to plugin methods by the SDK.

use hcore::hasync::Cancellable;
use std::sync::Arc;

/// Context for one in-flight plugin call. Wraps the request id and the
/// cancellation signal so authors can do `ctx.is_cancelled()` / `ctx.cancelled().await`
/// without touching the transport. The SDK fires the signal when a `Cancel`
/// frame arrives for this request (or, for the wasm tier, when the host's
/// `cancelled()` import returns true).
#[derive(Clone)]
pub struct Ctx {
    request_id: String,
    cancel: Arc<dyn Cancellable + Send + Sync>,
}

impl Ctx {
    /// Build a context from a request id and a cancellation signal.
    pub fn new(request_id: impl Into<String>, cancel: Arc<dyn Cancellable + Send + Sync>) -> Self {
        Self {
            request_id: request_id.into(),
            cancel,
        }
    }

    /// The request id this call belongs to (used to correlate callbacks).
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Non-blocking cancellation check.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolves when this call is cancelled.
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// Borrow the underlying signal to pass into engine/SDK helpers that take
    /// `&dyn Cancellable`.
    pub fn cancellable(&self) -> &(dyn Cancellable + Send + Sync) {
        &*self.cancel
    }
}
