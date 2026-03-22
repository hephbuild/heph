use std::future::Future;
use std::pin::Pin;

/// The contract for any cancellation mechanism.
pub trait Cancellable: Send + Sync {
    /// Non-blocking check for cancellation.
    fn is_cancelled(&self) -> bool;

    /// Returns a future that resolves when cancellation is triggered.
    /// Returns a pinned Box to remain object-safe and runtime-agnostic.
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}
