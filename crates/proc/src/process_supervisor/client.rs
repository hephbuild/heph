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

/// The supervisor process, started on first need rather than at heph startup.
///
/// The socketpair is made at startup (see [`super::init`]); only the spawn of
/// the `__supervisor` process waits until something needs it. Every path that
/// could leave work behind on a hard kill starts it *before* that work exists:
/// `proc_exec` before it spawns a child, the plugin loader before a cdylib can
/// spawn one, and any `TRACK`/`FUSEROOT` write before it is sent. So the
/// kill-on-death guarantee is the eager one's: a child that exists has a live
/// supervisor holding the socket, and — exactly as before — only the
/// microseconds between `spawn` returning and its `TRACK` line are uncovered.
///
/// **The binary is re-read at the first child, not at startup.** The sidecar is
/// `posix_spawn`ed from the path `current_exe` reported at startup. If that file
/// was replaced or unlinked in between — an upgrade landing mid-run — the start
/// runs a different binary or fails with `ENOENT`. Accepted: the window is one
/// invocation's lifetime, a failed start fails the spawn loudly rather than
/// leaving a child untracked, and the behaviour is the same on every OS (no
/// `/proc/self/exe` special case on Linux).
///
/// **A failed start is not sticky.** It fails the spawn that needed it and is
/// latched for a backoff window ([`RETRY_INITIAL`], doubling up to
/// [`RETRY_MAX`]); a spawn inside the window gets the latched error without a
/// new attempt, and the first one after it tries again. So a transient failure
/// (`EAGAIN` from a process limit, `EMFILE`) heals, and a permanent one (the
/// binary gone) costs one `posix_spawn` per window rather than one per child.
#[derive(Debug)]
pub(super) struct Sidecar {
    exe: std::path::PathBuf,
    /// The supervisor's pid, set once a start succeeds — the lock-free fast path
    /// every later spawn and `TRACK` takes.
    started: std::sync::OnceLock<i32>,
    /// Serialises start attempts; concurrent callers wait for the one in flight.
    attempt: Mutex<Attempt>,
}

/// The first wait after a failed supervisor start before the next attempt.
pub(super) const RETRY_INITIAL: std::time::Duration = std::time::Duration::from_millis(100);
/// The longest wait between attempts; the backoff doubles up to here.
pub(super) const RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
struct Attempt {
    /// The supervisor's end of the socketpair, `CLOEXEC` in this process the
    /// whole time; handed to the supervisor by a `dup2` in its own spawn. Kept
    /// across a failed start (the retry needs it) and closed after a successful
    /// one, when the supervisor holds its own copy.
    child_end: Option<std::os::fd::OwnedFd>,
    /// The last failed start, latched until `retry_at`.
    failed: Option<Failure>,
}

#[derive(Debug)]
struct Failure {
    error: String,
    retry_at: std::time::Instant,
    /// The wait that produced `retry_at`; the next failure doubles it.
    backoff: std::time::Duration,
}

impl Sidecar {
    pub(super) fn new(exe: std::path::PathBuf, child_end: std::os::fd::OwnedFd) -> Self {
        Self {
            exe,
            started: std::sync::OnceLock::new(),
            attempt: Mutex::new(Attempt {
                child_end: Some(child_end),
                failed: None,
            }),
        }
    }

    /// Start the supervisor if it is not running yet. Concurrent callers wait
    /// for the one spawn.
    fn ensure_started(&self) -> anyhow::Result<()> {
        self.ensure_started_at(std::time::Instant::now())
    }

    /// [`Self::ensure_started`] at a given instant — the clock is a parameter so
    /// the backoff can be tested without sleeping.
    pub(super) fn ensure_started_at(&self, now: std::time::Instant) -> anyhow::Result<()> {
        if self.started.get().is_some() {
            return Ok(());
        }
        let mut attempt = self
            .attempt
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("supervisor start mutex poisoned"))?;
        // Another caller may have started it while this one waited for the lock.
        if self.started.get().is_some() {
            return Ok(());
        }
        if let Some(failed) = &attempt.failed
            && now < failed.retry_at
        {
            anyhow::bail!(
                "starting the process supervisor: {} (next attempt in {}ms)",
                failed.error,
                failed.retry_at.saturating_duration_since(now).as_millis()
            );
        }
        let child_end = attempt
            .child_end
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("supervisor socket already handed off"))?;
        match super::spawn_sidecar(&self.exe, child_end) {
            Ok(pid) => {
                // The supervisor holds its own copy now; ours must close so the
                // supervisor is the only reader of its end.
                attempt.child_end = None;
                attempt.failed = None;
                // Set under the lock, so a waiter that takes it next sees it.
                let first = self.started.set(pid).is_ok();
                debug_assert!(first, "supervisor started twice under the attempt lock");
                Ok(())
            }
            Err(e) => {
                let error = format!("{e:#}");
                let backoff = attempt.failed.as_ref().map_or(RETRY_INITIAL, |f| {
                    f.backoff.saturating_mul(2).min(RETRY_MAX)
                });
                tracing::warn!(
                    exe = %self.exe.display(),
                    error = %error,
                    retry_in_ms = backoff.as_millis(),
                    "process supervisor failed to start"
                );
                attempt.failed = Some(Failure {
                    error: error.clone(),
                    retry_at: now + backoff,
                    backoff,
                });
                Err(anyhow::anyhow!("starting the process supervisor: {error}"))
            }
        }
    }

    pub(super) fn pid(&self) -> Option<i32> {
        self.started.get().copied()
    }
}

/// Where a tracker's messages go.
#[derive(Debug)]
enum Backend {
    /// Owns the supervisor socketpair end and writes the wire protocol; starts
    /// the supervisor process before the first message.
    Socket(Mutex<Option<std::os::unix::net::UnixStream>>, Sidecar),
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
    /// The socket-owning tracker: writes to `s`, whose peer `sidecar` hands to
    /// the supervisor process when it is first needed.
    pub(super) fn from_stream(s: std::os::unix::net::UnixStream, sidecar: Sidecar) -> Self {
        Self {
            backend: Backend::Socket(Mutex::new(Some(s)), sidecar),
            alive: AtomicBool::new(true),
        }
    }

    /// Make sure a supervisor is running to receive this tracker's messages.
    ///
    /// Call before creating anything that must die with heph — a child process
    /// above all. A no-op for a sink-backed tracker (the host's tracker owns the
    /// supervisor; see `process_supervisor::ensure_started`) and for a no-op one.
    pub fn ensure_started(&self) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Socket(_, sidecar) => sidecar.ensure_started(),
            Backend::Sink(_) | Backend::Noop => Ok(()),
        }
    }

    /// The supervisor process's pid, once this tracker has started one.
    pub(super) fn sidecar_pid(&self) -> Option<i32> {
        match &self.backend {
            Backend::Socket(_, sidecar) => sidecar.pid(),
            Backend::Sink(_) | Backend::Noop => None,
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

    /// Register a sandboxfuse root with the supervisor. On parent EOF the
    /// supervisor will `umount -f <root>/lower` so a crash doesn't leave
    /// the FUSE kext wedged. One-shot: registrations accumulate; there
    /// is no unregister verb (the dir is per-pid and reaped on exit).
    pub fn register_fuse_root(&self, root: std::path::PathBuf) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Sink(sink) => sink.register_fuse_root(&root),
            _ => self.send(Msg::FuseRoot(root)),
        }
    }

    fn send(&self, msg: Msg) -> anyhow::Result<()> {
        if !self.is_alive() {
            anyhow::bail!("process supervisor unavailable");
        }
        let Backend::Socket(sock, sidecar) = &self.backend else {
            anyhow::bail!("process supervisor unavailable");
        };
        // Normally already started by the caller (before the child existed);
        // this is the backstop for any path that reports without having done so.
        sidecar.ensure_started()?;
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
