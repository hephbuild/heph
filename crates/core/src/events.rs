//! Build-progress event stream types emitted by the engine core and consumed by
//! the TUI + telemetry. Shared here (the lowest crate) so neither consumer needs
//! to depend on the engine.
//!
//! Events are serde-serializable and transport-ready for a future client/server
//! process split. They carry the target address as a `String` (`//pkg:name`),
//! never the internal `Arc`-backed `Addr`. The server (engine) stamps every event
//! with a wall-clock timestamp at emit time (`at_unix_ms`). The `emit_scope`
//! helper that pairs Start/End events lives in the engine (it needs the engine's
//! `RequestState`).

use serde::{Deserialize, Serialize};

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
    /// Start of writing a target's artifacts to the local cache. Paired with
    /// `LocalCacheWriteEnd` via `emit_scope` (fires on completion, `?`, or
    /// cancellation). The client groups this span under the target alongside
    /// `Execute*` as one entry in the per-target operation timeline.
    LocalCacheWriteStart {
        addr: String,
    },
    LocalCacheWriteEnd {
        addr: String,
        error: Option<String>,
    },
    /// Acquiring the per-addr result lock has been blocked past the notice
    /// threshold. `holder_pid` is the process believed to hold the lock
    /// (best-effort; `None` if unknown). Paired one-to-one with
    /// `ResultLockWaitEnd` (which fires on acquire **or** cancellation), so a
    /// consumer can show the notice for exactly the duration of the wait.
    ResultLockWaitStart {
        addr: String,
        holder_pid: Option<u32>,
    },
    /// The execute-lock wait ended (lock acquired or the wait was cancelled).
    ResultLockWaitEnd {
        addr: String,
    },
    RemoteCacheHit {
        addr: String,
    },
    RemoteCacheMiss {
        addr: String,
    },
    /// Start of pulling a target's revision from the remote cache(s) into the
    /// local cache (one span per target, covering all of its blobs from the one
    /// cache that had the manifest). Surfaced as one `↓` op in the per-target
    /// timeline, marked slow if it runs long. Paired with `RemoteCacheReadEnd`.
    RemoteCacheReadStart {
        addr: String,
    },
    RemoteCacheReadEnd {
        addr: String,
        error: Option<String>,
    },
    /// Start of pushing a target's artifacts to the remote cache(s). Runs on a
    /// background task after the build's critical path, so it appears in the
    /// per-target op timeline (and is surfaced as "slow" if it runs long).
    /// Paired with `RemoteCacheWriteEnd`.
    RemoteCacheWriteStart {
        addr: String,
    },
    RemoteCacheWriteEnd {
        addr: String,
        error: Option<String>,
    },
    /// One target finished garbage collection: how many cache revisions it
    /// dropped and how many bytes those revisions freed (summed from manifest
    /// artifact sizes). Emitted once per target the `heph gc` sweep visits —
    /// including targets that dropped nothing — so a consumer can show both the
    /// count of targets explored and the total data reclaimed.
    GcTargetSwept {
        revisions_removed: usize,
        bytes_removed: u64,
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
