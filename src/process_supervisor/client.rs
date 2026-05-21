use anyhow::Context;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::process_supervisor::protocol::Msg;

/// Client-side handle for talking to the supervisor sidecar.
///
/// Methods are cheap, non-async, and safe to call from any thread. A poisoned
/// mutex or `EPIPE` flips `alive` to `false` permanently — once the supervisor
/// dies there is no recovery, only an early failure on the next `track` call.
#[derive(Debug)]
pub struct ProcessTracker {
    sock: Mutex<Option<UnixStream>>,
    alive: AtomicBool,
}

impl ProcessTracker {
    pub(super) fn from_stream(s: UnixStream) -> Self {
        Self {
            sock: Mutex::new(Some(s)),
            alive: AtomicBool::new(true),
        }
    }

    /// A tracker that does nothing — used when supervisor init was skipped
    /// (e.g. inside the supervisor child process itself, or in unit tests).
    pub fn noop() -> Self {
        Self {
            sock: Mutex::new(None),
            alive: AtomicBool::new(false),
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn track(&self, pgid: i32) -> anyhow::Result<()> {
        self.send(Msg::Track(pgid))
    }

    pub fn untrack(&self, pgid: i32) -> anyhow::Result<()> {
        self.send(Msg::Untrack(pgid))
    }

    fn send(&self, msg: Msg) -> anyhow::Result<()> {
        if !self.is_alive() {
            anyhow::bail!("process supervisor unavailable");
        }
        let mut guard = self
            .sock
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("supervisor socket mutex poisoned"))?;
        let sock = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("supervisor socket closed"))?;
        let line = msg.encode();
        if let Err(e) = sock.write_all(line.as_bytes()) {
            self.alive.store(false, Ordering::Release);
            *guard = None;
            return Err(e).context("write to process supervisor");
        }
        Ok(())
    }
}

/// RAII guard that sends `UNTRACK` when dropped.
///
/// Driver code uses this to ensure a child's pgid is released back to the
/// supervisor even on panic, error return, or cancellation paths.
#[derive(Debug)]
pub struct TrackGuard {
    tracker: std::sync::Arc<ProcessTracker>,
    pgid: i32,
}

impl TrackGuard {
    pub fn new(tracker: std::sync::Arc<ProcessTracker>, pgid: i32) -> Self {
        Self { tracker, pgid }
    }
}

impl Drop for TrackGuard {
    fn drop(&mut self) {
        // Best-effort: if the supervisor is already dead, untrack will Err;
        // harmless to ignore.
        drop(self.tracker.untrack(self.pgid));
    }
}
