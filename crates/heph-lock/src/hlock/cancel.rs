//! Shared helper to make a blocking acquire respect a [`Cancellable`] token.

use heph_plugin::error::CancelledError;
use heph_core::hasync::Cancellable;
use anyhow::{Context, Result};
use std::future::Future;

/// Race an acquire `fut` against `ctoken`. Returns the future's result, or a
/// [`CancelledError`] (wrapped with `what` context) if the token fires first.
///
/// Cancellation wins deterministically (`biased`) when both are ready, matching
/// the engine's intent that cancellation stops work immediately. A pre-check
/// avoids registering a waker when already cancelled.
pub(crate) async fn acquire_cancellable<F, T>(
    ctoken: &(dyn Cancellable + Send + Sync),
    what: &'static str,
    fut: F,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    if ctoken.is_cancelled() {
        return Err(anyhow::Error::new(CancelledError)).context(what);
    }
    tokio::select! {
        biased;
        () = ctoken.cancelled() => Err(anyhow::Error::new(CancelledError)).context(what),
        res = fut => res.context(what),
    }
}
