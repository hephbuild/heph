use anyhow::Context;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::process_supervisor::protocol::Msg;

/// A sink a [`ProcessTracker`] forwards its messages to instead of writing the
/// supervisor socket itself.
///
/// Exists for the dlopen'd cdylib plugins: a plugin statically links its OWN copy
/// of this crate, so its `TRACKER` static is a *different* `OnceLock` that
/// [`super::init`] never touches — and its children would go unregistered. The
/// host hands each plugin a sink backed by the host's socket-owning tracker, so
/// every plugin-spawned child lands in the same supervisor as a host-spawned one.
pub trait SupervisorSink: std::fmt::Debug + Send + Sync {
    fn track(&self, pgid: i32) -> anyhow::Result<()>;
    fn untrack(&self, pgid: i32) -> anyhow::Result<()>;
    fn register_fuse_root(&self, root: &std::path::Path) -> anyhow::Result<()>;
}

/// Where a tracker's messages go.
#[derive(Debug)]
enum Backend {
    /// Owns the supervisor socketpair end and writes the wire protocol.
    Socket(Mutex<Option<std::os::unix::net::UnixStream>>),
    /// Forwards to the host's tracker across the plugin ABI.
    Sink(Box<dyn SupervisorSink>),
    /// Supervisor was never initialised (unit tests, the supervisor child itself).
    Noop,
}

/// Client-side handle for talking to the supervisor sidecar.
///
/// Methods are cheap, non-async, and safe to call from any thread. A poisoned
/// mutex or `EPIPE` flips `alive` to `false` permanently — once the supervisor
/// dies there is no recovery, only an early failure on the next `track` call.
#[derive(Debug)]
pub struct ProcessTracker {
    backend: Backend,
    alive: AtomicBool,
}

impl ProcessTracker {
    pub(super) fn from_stream(s: std::os::unix::net::UnixStream) -> Self {
        Self {
            backend: Backend::Socket(Mutex::new(Some(s))),
            alive: AtomicBool::new(true),
        }
    }

    /// A tracker that forwards to `sink` — the plugin-side handle. Liveness is
    /// owned by whoever holds the socket: a dead supervisor surfaces as an `Err`
    /// from the sink, so this handle stays `alive` and never latches.
    pub fn from_sink(sink: Box<dyn SupervisorSink>) -> Self {
        Self {
            backend: Backend::Sink(sink),
            alive: AtomicBool::new(true),
        }
    }

    /// A tracker that does nothing — used when supervisor init was skipped
    /// (e.g. inside the supervisor child process itself, or in unit tests).
    pub fn noop() -> Self {
        Self {
            backend: Backend::Noop,
            alive: AtomicBool::new(false),
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn track(&self, pgid: i32) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Sink(sink) => sink.track(pgid),
            _ => self.send(Msg::Track(pgid)),
        }
    }

    pub fn untrack(&self, pgid: i32) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Sink(sink) => sink.untrack(pgid),
            _ => self.send(Msg::Untrack(pgid)),
        }
    }

    /// Register a sandboxfuse mountpoint with the supervisor. On parent EOF
    /// the supervisor will `umount -f <mountpoint>` so a crash doesn't leave
    /// the FUSE mount wedged. The kernel/kext mountpoint is `<root>/lower`;
    /// the FSKit mountpoint lives under `/Volumes`. One-shot: registrations
    /// accumulate; there is no unregister verb (the dir is per-pid and reaped
    /// on exit).
    pub fn register_fuse_root(&self, mountpoint: std::path::PathBuf) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Sink(sink) => sink.register_fuse_root(&mountpoint),
            _ => self.send(Msg::FuseRoot(mountpoint)),
        }
    }

    fn send(&self, msg: Msg) -> anyhow::Result<()> {
        if !self.is_alive() {
            anyhow::bail!("process supervisor unavailable");
        }
        let Backend::Socket(sock) = &self.backend else {
            anyhow::bail!("process supervisor unavailable");
        };
        let mut guard = sock
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
