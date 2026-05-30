//! Build-progress event stream emitted by the engine core.
//!
//! Events are serde-serializable and transport-ready for a future client/server
//! process split. They carry the target address as a `String` (`//pkg:name`),
//! never the internal `Arc`-backed `Addr`. The server (engine) stamps every event
//! with a wall-clock timestamp at emit time (`at_unix_ms`), so elapsed times stay
//! correct even when the client and server are split across a channel.

use crate::engine::request_state::RequestState;
use serde::{Deserialize, Serialize};
use std::future::Future;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildEvent {
    /// Server-stamped at emit time (`SystemTime::now()` since epoch, milliseconds).
    pub at_unix_ms: u64,
    pub kind: BuildEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BuildEventKind {
    /// Declares the engine's worker capacity (the `result` semaphore size) for
    /// this request. Emitted once at the start of a `result` batch so the client
    /// can render a fixed worker-slot indicator. `count` is the maximum number of
    /// targets that can execute concurrently.
    MaxWorkers {
        count: usize,
    },
    /// Incremental notice of matched top-level targets. `addrs` are newly
    /// matched addresses to add to the set; `complete` is false while the
    /// matcher is still resolving (client renders a provisional `X / ~N`) and
    /// true on the final event once the full set is known (drops the `~`).
    Matched {
        addrs: Vec<String>,
        complete: bool,
    },
    ResultStart {
        addr: String,
    },
    ResultEnd {
        addr: String,
        error: Option<String>,
    },
    ExecuteStart {
        addr: String,
        driver: String,
        cache: bool,
    },
    ExecuteEnd {
        addr: String,
        error: Option<String>,
    },
    LocalCacheHit {
        addr: String,
    },
    LocalCacheMiss {
        addr: String,
    },
    /// Acquiring the per-addr execute lock has been blocked past the notice
    /// threshold. `holder_pid` is the process believed to hold the lock
    /// (best-effort; `None` if unknown). Paired one-to-one with
    /// `ExecuteLockWaitEnd` (which fires on acquire **or** cancellation), so a
    /// consumer can show the notice for exactly the duration of the wait.
    ExecuteLockWaitStart {
        addr: String,
        holder_pid: Option<u32>,
    },
    /// The execute-lock wait ended (lock acquired or the wait was cancelled).
    ExecuteLockWaitEnd {
        addr: String,
    },
    // Defined for the future process-split / remote cache work; NOT emitted yet.
    RemoteCacheRead {
        addr: String,
    },
    RemoteCacheWrite {
        addr: String,
    },
    RemoteCacheHit {
        addr: String,
    },
    RemoteCacheMiss {
        addr: String,
    },
}

pub type EventSender = tokio::sync::mpsc::UnboundedSender<BuildEvent>;
pub type EventReceiver = tokio::sync::mpsc::UnboundedReceiver<BuildEvent>;

/// Wall-clock milliseconds since the Unix epoch. Stamped once at emit time.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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
