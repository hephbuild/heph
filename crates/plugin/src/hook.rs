//! The `Hook` contract: a build-event consumer plugin kind, alongside
//! `Provider`/`Driver`. A hook observes the engine's [`BuildEvent`] stream — it
//! does not produce targets or run them. Used for auxiliary reporting (a CI
//! summary, an external dashboard, …).
//!
//! The trait is **symmetric**, exactly like `Provider`: the same trait is
//! implemented by the author (guest side, in the plugin) and by the host-side
//! bridge that forwards events across the cdylib seam (see
//! `plugin-stabby::load_stable::StableRemoteHook`). The engine holds registered
//! hooks and fans every emitted event out to each via [`Hook::on_event`].
//!
//! The surface is deliberately tiny: one event at a time (`on_event`), an
//! end-of-stream signal (`on_close`), and an async [`Hook::drain`] the host awaits
//! at teardown so an out-of-process hook's final write completes before exit.

use futures::future::BoxFuture;
use hcore::events::BuildEvent;

/// A build-event consumer. Implementations must be cheap and non-blocking in
/// [`on_event`](Hook::on_event) — it is called from the engine's `emit()`
/// chokepoint on the hot path. Fold/forward and return; do slow work (file
/// writes, network) off-thread or on a timer the implementation owns.
pub trait Hook: Send + Sync {
    /// The hook's stable name (for diagnostics / dedup).
    fn name(&self) -> String;

    /// Process one build event. Called for every event the engine emits, in
    /// emit order, possibly concurrently from multiple tasks — implementations
    /// must be `Sync`-safe. Best-effort: errors have no channel, so swallow or
    /// log them internally.
    fn on_event(&self, ev: &BuildEvent);

    /// The event stream for this request has ended (the request's state is being
    /// dropped). A hook flushes any final state here. Called at most once per
    /// opened stream.
    fn on_close(&self);

    /// Await any in-flight delivery/flush this hook still owes (e.g. an
    /// out-of-process hook waiting on the plugin to acknowledge it consumed the
    /// full stream and wrote its final summary). The engine awaits this at
    /// teardown, after [`on_close`](Hook::on_close), so a process exit never
    /// races a hook's final write. Default: nothing to await.
    fn drain(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}
