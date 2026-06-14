//! Build-progress event stream emitted by the engine core.
//!
//! Events are serde-serializable and transport-ready for a future client/server
//! process split. They carry the target address as a `String` (`//pkg:name`),
//! never the internal `Arc`-backed `Addr`. The server (engine) stamps every event
//! with a wall-clock timestamp at emit time (`at_unix_ms`), so elapsed times stay
//! correct even when the client and server are split across a channel.

use crate::engine::request_state::RequestState;
use std::future::Future;

// Event types moved to `heph-core::events` (shared by the TUI + telemetry
// without an engine dep); re-exported so `engine::event::BuildEventKind` etc.
// keep resolving. `emit_scope` stays here — it needs `RequestState`.
pub use heph_core::events::{BuildEvent, BuildEventKind, EventReceiver, EventSender, now_unix_ms};

/// Internal drop-guard so the `*End` event fires on early-return (`?`) **and** on
/// cancellation (the awaited future is dropped mid-flight). Once armed, the guard
/// emits exactly one end event when dropped.
struct EndGuard {
    tx: Option<EventSender>,
    make_end: Option<Box<dyn FnOnce(Option<String>) -> BuildEventKind + Send>>,
    error: Option<String>,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        if let (Some(tx), Some(make_end)) = (self.tx.take(), self.make_end.take()) {
            let kind = make_end(self.error.take());
            // A closed receiver (consumer gone, e.g. TUI shut down) is expected;
            // dropping the send result is intentional, events are best-effort.
            drop(tx.send(BuildEvent {
                at_unix_ms: now_unix_ms(),
                kind,
            }));
        }
    }
}

/// Emit `start`, run `fut`, then emit `make_end(error)` on
/// completion, early-return (`?`), or cancellation.
///
/// The end event is produced by an internal drop-guard armed before the await, so
/// it fires even if `fut` is cancelled mid-await. `at_unix_ms` is stamped on both
/// the start and end events. Call sites stay a single wrapping expression.
pub async fn emit_scope<T>(
    rs: &RequestState,
    start: BuildEventKind,
    make_end: impl FnOnce(Option<String>) -> BuildEventKind + Send + 'static,
    fut: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    rs.emit(start);
    let mut guard = EndGuard {
        tx: rs.events_sender(),
        make_end: Some(Box::new(make_end)),
        error: None,
    };
    let out = fut.await; // guard still armed if this is cancelled mid-await
    if let Err(e) = &out {
        guard.error = Some(format!("{e:#}"));
    }
    out // guard drops here → emits *End
}
