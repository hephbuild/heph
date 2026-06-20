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
    /// Start of building the target's sandbox: creating the sandbox dir and
    /// materializing every declared input into it (unpack / hardlink-stage /
    /// FUSE-slot register) before the subprocess spawns. Emitted by the driver
    /// bridge, which owns the create step. Surfaced as one op in the per-target
    /// timeline and marked slow if it runs long (large inputs, cold stage).
    /// Paired with `SandboxCreateEnd` (fires on completion, `?`, or cancel).
    SandboxCreateStart {
        addr: String,
    },
    SandboxCreateEnd {
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

/// Stamp + send one event on `tx`. A closed receiver (consumer gone, e.g. TUI
/// shut down) is expected — events are best-effort, so the send result is
/// dropped intentionally.
fn send_stamped(tx: &EventSender, kind: BuildEventKind) {
    drop(tx.send(BuildEvent {
        at_unix_ms: now_unix_ms(),
        kind,
    }));
}

/// Drop-guard so the `*End` event fires on early-return (`?`) **and** on
/// cancellation (the awaited future is dropped mid-flight). Once armed, it emits
/// exactly one end event when dropped.
struct EndGuard {
    tx: Option<EventSender>,
    make_end: Option<Box<dyn FnOnce(Option<String>) -> BuildEventKind + Send>>,
    error: Option<String>,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        if let (Some(tx), Some(make_end)) = (self.tx.take(), self.make_end.take()) {
            send_stamped(&tx, make_end(self.error.take()));
        }
    }
}

/// Emit `start`, run `fut`, then emit `make_end(error)` on completion,
/// early-return (`?`), or cancellation — given a raw `EventSender` rather than
/// the engine's `RequestState`. Used by layers below the engine (the driver
/// bridge) that hold an `Option<EventSender>` plumbed through the request. A
/// `None` sender makes this a transparent pass-through (no events, no overhead).
///
/// The end event is produced by an internal drop-guard armed before the await,
/// so it fires even if `fut` is cancelled mid-await. `at_unix_ms` is stamped on
/// both the start and end events.
pub async fn emit_scope_tx<T>(
    events: Option<EventSender>,
    start: BuildEventKind,
    make_end: impl FnOnce(Option<String>) -> BuildEventKind + Send + 'static,
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    if let Some(tx) = &events {
        send_stamped(tx, start);
    }
    let mut guard = EndGuard {
        tx: events,
        make_end: Some(Box::new(make_end)),
        error: None,
    };
    let out = fut.await; // guard still armed if this is cancelled mid-await
    if let Err(e) = &out {
        guard.error = Some(format!("{e:#}"));
    }
    out // guard drops here → emits *End
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(rx: &mut EventReceiver) -> Vec<BuildEventKind> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev.kind);
        }
        out
    }

    #[tokio::test]
    async fn emit_scope_tx_emits_start_then_end_on_success() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out: anyhow::Result<u32> = emit_scope_tx(
            Some(tx),
            BuildEventKind::SandboxCreateStart {
                addr: "//a:b".into(),
            },
            |error| BuildEventKind::SandboxCreateEnd {
                addr: "//a:b".into(),
                error,
            },
            async { Ok(7) },
        )
        .await;
        assert_eq!(out.unwrap(), 7);
        match kinds(&mut rx).as_slice() {
            [
                BuildEventKind::SandboxCreateStart { addr: a },
                BuildEventKind::SandboxCreateEnd {
                    addr: b,
                    error: None,
                },
            ] => {
                assert_eq!(a, "//a:b");
                assert_eq!(b, "//a:b");
            }
            other => panic!("unexpected events: {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_scope_tx_captures_error_in_end_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out: anyhow::Result<()> = emit_scope_tx(
            Some(tx),
            BuildEventKind::SandboxCreateStart {
                addr: "//a:b".into(),
            },
            |error| BuildEventKind::SandboxCreateEnd {
                addr: "//a:b".into(),
                error,
            },
            async { Err(anyhow::anyhow!("boom")) },
        )
        .await;
        assert!(out.is_err());
        let end = kinds(&mut rx).pop().expect("end event");
        match end {
            BuildEventKind::SandboxCreateEnd {
                error: Some(msg), ..
            } => assert!(msg.contains("boom"), "{msg}"),
            other => panic!("expected end-with-error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_scope_tx_emits_end_on_cancellation() {
        // Dropping the future mid-await must still fire the end event via the
        // armed drop-guard — the slow-sandbox case where the build is cancelled.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fut = emit_scope_tx(
            Some(tx),
            BuildEventKind::SandboxCreateStart {
                addr: "//a:b".into(),
            },
            |error| BuildEventKind::SandboxCreateEnd {
                addr: "//a:b".into(),
                error,
            },
            std::future::pending::<anyhow::Result<()>>(),
        );
        // Poll once to emit Start + arm the guard, then drop without completing.
        let mut boxed = Box::pin(fut);
        let _ = futures::poll!(boxed.as_mut());
        drop(boxed);
        let observed = kinds(&mut rx);
        assert!(
            matches!(
                observed.as_slice(),
                [
                    BuildEventKind::SandboxCreateStart { .. },
                    BuildEventKind::SandboxCreateEnd { error: None, .. },
                ]
            ),
            "expected start+end on cancel, got {observed:?}"
        );
    }
}
