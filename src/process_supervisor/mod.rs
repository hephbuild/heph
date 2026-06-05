//! Sidecar supervisor process that reaps the entire descendant tree when the
//! main heph process dies, including hard-kill (SIGKILL/OOM) scenarios.
//!
//! See the design notes in this module's git history for the full rationale.
//! Summary: a small `__supervisor` subcommand of heph is forked at startup
//! and holds one end of an `AF_UNIX` socketpair. Each driver-spawned child
//! enters its own session (`setsid` → `pid == pgid`) and is reported to the
//! supervisor via `TRACK <pgid>`. When the main process exits — for any
//! reason — the kernel closes the socket, the supervisor sees EOF, and it
//! `killpg(SIGKILL)`s every tracked pgid before exiting.
//!
//! Known race: between `Command::spawn()` returning and
//! [`ProcessTracker::track`] writing the `TRACK` line, a SIGKILL of the main
//! process leaves that one child unreaped. The window is microseconds; closing
//! it would require writing the pid from inside `pre_exec` over the inherited
//! socket. Deferred.
//!
//! Set `heph_DISABLE_REAPER=1` to bypass the sidecar — [`init`] becomes a
//! no-op and [`register_child`] returns `None`. The actual wait path lives
//! in [`crate::proc_exec`].

use anyhow::Context;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::sync::{Arc, OnceLock};

mod client;
mod protocol;
mod server;

pub use client::{ProcessTracker, TrackGuard};
pub use server::run_supervisor_main;

static TRACKER: OnceLock<Arc<ProcessTracker>> = OnceLock::new();

/// `heph_DISABLE_REAPER=1` short-circuits every supervisor entry point so
/// subprocess execution falls through to tokio's built-in waker. Cached in
/// a `OnceLock` because `std::env::var` takes a global libc mutex; consistent
/// with the `stall_threshold` / `cycle_detection_enabled` pattern in
/// `src/hmemoizer/mod.rs`.
fn reaper_disabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| matches!(std::env::var("heph_DISABLE_REAPER").as_deref(), Ok("1")))
}

/// Returns the process-global tracker handle. If [`init`] has not been called
/// (e.g. inside the supervisor child itself, or in unit tests) a no-op
/// tracker is returned whose `track`/`untrack` calls fail immediately.
pub fn tracker() -> Arc<ProcessTracker> {
    if let Some(t) = TRACKER.get() {
        return Arc::clone(t);
    }
    static NOOP: OnceLock<Arc<ProcessTracker>> = OnceLock::new();
    Arc::clone(NOOP.get_or_init(|| Arc::new(ProcessTracker::noop())))
}

/// Fork the supervisor sidecar and store its client handle in the global
/// tracker slot. Idempotent: subsequent calls return `Ok(())` without forking.
///
/// Must be called early — before any thread spawns its own children — so that
/// every later `Command::spawn` can register its pgid with a live supervisor.
pub fn init() -> anyhow::Result<()> {
    if reaper_disabled() {
        return Ok(());
    }
    if TRACKER.get().is_some() {
        return Ok(());
    }

    // Ignore SIGPIPE in the main process too, so a dead supervisor never
    // raises a signal when a driver writes a `TRACK` line.
    // SAFETY: signal(2) at startup, before driver threads run.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let (parent, child) = UnixStream::pair().context("create supervisor socketpair")?;
    let parent_fd = parent.as_raw_fd();
    set_cloexec(parent_fd, true).context("set CLOEXEC on parent supervisor fd")?;

    // Hand `child` to the supervisor sub-process; its CLOEXEC must be cleared
    // so it survives exec.
    let child_fd: RawFd = child.into_raw_fd();
    set_cloexec(child_fd, false).context("clear CLOEXEC on supervisor child fd")?;

    let current_exe = std::env::current_exe().context("locate current_exe for supervisor")?;
    let mut cmd = std::process::Command::new(&current_exe);
    cmd.arg("__supervisor")
        .arg("--ipc-fd")
        .arg(child_fd.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Defensive: clear CLOEXEC again in the child's pre-exec window, in case
    // some other crate (or future Rust version) tightens fd inheritance.
    let fd_for_child = child_fd;
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "pre_exec installer + fcntl calls must share one unsafe context"
    )]
    // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
    // syscalls (fcntl) are invoked.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(fd_for_child, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(fd_for_child, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let _supervisor = cmd.spawn().context("spawn supervisor sidecar")?;

    // Close the parent's copy of the child end of the socket so EOF semantics
    // are correct. The supervisor process now owns its end exclusively.
    // SAFETY: child_fd was transferred via into_raw_fd above; close once.
    unsafe {
        libc::close(child_fd);
    }

    let tracker = Arc::new(ProcessTracker::from_stream(parent));
    TRACKER
        .set(tracker)
        .map_err(|_already_set| anyhow::anyhow!("process supervisor already initialised"))?;
    Ok(())
}

/// Run `f` synchronously. On macOS, uses `block_in_place` so the calling
/// worker parks on a kernel condvar rather than yielding through tokio's
/// broken cross-thread waker (`mio::Waker` → `EVFILT_USER`). On Linux,
/// calls `f` inline — there's no waker reliability issue and the call
/// sites are short enough that yielding to the scheduler would add more
/// overhead than the inline blocking saves.
#[cfg(target_os = "macos")]
pub fn block_or_inline<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    if matches!(
        tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread)
    ) {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// Linux variant: no waker reliability bug, so inline calls are fine.
#[cfg(not(target_os = "macos"))]
pub fn block_or_inline<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

/// Send `signal` to a child by id. Always issues both `killpg` and `kill`:
///   - `killpg` reaches the whole tree if the child is a session leader
///     (i.e. it called `setsid` in `pre_exec` — e.g. pluginexec shell mode).
///   - `kill` covers drivers that did not `setsid` (plugingo, pluginnix).
///
/// PIDs are unique system-wide, so `killpg(pid)` is a safe no-op when `pid`
/// is not a group leader. `ESRCH` from either call is ignored.
fn signal_child(pid: i32, signal: libc::c_int) {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "killpg + kill are paired best-effort delivery to the same pid"
    )]
    // SAFETY: signal via killpg/kill on a pid we spawned and own.
    unsafe {
        libc::killpg(pid, signal);
        libc::kill(pid, signal);
    }
}

/// Hard-kill a child (and its process group) with `SIGKILL`. Used for the
/// non-graceful paths: Handle drop, drain-thread escalation, supervisor
/// parent-death reap.
pub fn kill_child(pid: i32) {
    signal_child(pid, libc::SIGKILL);
}

/// Politely ask a child (and its process group) to stop with `SIGINT` —
/// the same signal a terminal Ctrl-C delivers. Used by the cancellation
/// path so well-behaved children can clean up before we escalate to
/// `kill_child` (`SIGKILL`) if they ignore it.
pub fn interrupt_child(pid: i32) {
    signal_child(pid, libc::SIGINT);
}

/// Register a freshly-spawned child's pgid with the supervisor, returning a
/// guard that untracks on drop. If the supervisor is unavailable (e.g. in
/// unit tests where [`init`] never ran), logs a warning and returns `None`
/// rather than failing the build — the caller still benefits from `kill_on_drop`
/// as a fallback.
pub fn register_child(pgid: i32) -> Option<TrackGuard> {
    if reaper_disabled() {
        return None;
    }
    let tracker = tracker();
    if let Err(e) = tracker.track(pgid) {
        tracing::warn!(pgid, error = %format!("{e:#}"), "child not registered with process supervisor");
        return None;
    }
    Some(TrackGuard::new(tracker, pgid))
}

/// Tell the supervisor about a sandboxfuse root so it can force-umount on
/// parent death. Best-effort: if the supervisor isn't running (reaper
/// disabled, init never called), this is a no-op. The mount still has the
/// in-process drop watchdog as a fallback.
pub fn register_fuse_root(root: std::path::PathBuf) {
    if reaper_disabled() {
        return;
    }
    let tracker = tracker();
    if let Err(e) = tracker.register_fuse_root(root.clone()) {
        tracing::warn!(
            root = ?root,
            error = %format!("{e:#}"),
            "fuse root not registered with process supervisor"
        );
    }
}

fn set_cloexec(fd: RawFd, on: bool) -> std::io::Result<()> {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "F_GETFD and F_SETFD are paired, must share one unsafe context"
    )]
    // SAFETY: fcntl(F_GETFD/F_SETFD) on a fd we just created and own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let new = if on {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        if libc::fcntl(fd, libc::F_SETFD, new) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
