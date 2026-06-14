use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The contract for any cancellation mechanism.
pub trait Cancellable: Send + Sync {
    /// Non-blocking check for cancellation.
    fn is_cancelled(&self) -> bool;

    /// Returns a future that resolves when cancellation is triggered.
    /// Returns a pinned Box to remain object-safe and runtime-agnostic.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Produce an owned, `'static` handle to the same underlying signal so it
    /// can be moved into a detached task (e.g. `tokio::spawn`) where the
    /// borrowed `&dyn Cancellable` can't go.
    fn clone_arc(&self) -> Arc<dyn Cancellable + Send + Sync>;
}
