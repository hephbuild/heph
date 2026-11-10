use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The contract for any cancellation mechanism.
pub trait Cancellable: Send + Sync {
    /// Non-blocking check for cancellation.
    fn is_cancelled(&self) -> bool;

    /// Whether the token was cancelled because the run it was handed to
    /// outlasted its `timeout`, rather than because its request was
    /// cancelled. Implies [`is_cancelled`](Self::is_cancelled). The first
    /// cancel decides it: a timed-out token cancelled again stays timed out,
    /// and a cancelled one never becomes timed out.
    ///
    /// No default: a wrapper that forwarded the cancel but not this would
    /// read a timeout as a request cancel.
    fn timed_out(&self) -> bool;

    /// Returns a future that resolves when cancellation is triggered.
    /// Returns a pinned Box to remain object-safe and runtime-agnostic.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Produce an owned, `'static` handle to the same underlying signal so it
    /// can be moved into a detached task (e.g. `tokio::spawn`) where the
    /// borrowed `&dyn Cancellable` can't go.
    fn clone_arc(&self) -> Arc<dyn Cancellable + Send + Sync>;
}
