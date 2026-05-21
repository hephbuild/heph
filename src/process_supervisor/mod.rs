//! Sidecar supervisor process that reaps the entire descendant tree when the
//! main rheph process dies, including hard-kill (SIGKILL/OOM) scenarios.
//!
//! See the design notes in this module's git history for the full rationale.
//! Summary: a small `__supervisor` subcommand of rheph is forked at startup
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

/// Apply per-child isolation in [`tokio::process::Command::pre_exec`]:
///
/// - `setsid()` so the child becomes a new session leader with `pid == pgid`.
///   This is what lets the supervisor reap the *whole tree* via
///   `killpg(pgid, SIGKILL)`.
/// - On Linux only, `prctl(PR_SET_PDEATHSIG, SIGKILL)` as a defense-in-depth
///   backstop — if the supervisor itself is bypassed for any reason, the
///   kernel still kills the direct child when its parent (main) dies.
///
/// Do **not** call this on a command that already invokes `setsid()` in its
/// own `pre_exec` (e.g. `pluginexec` shell mode): the second `setsid()` will
/// return `EPERM`. Use [`apply_pdeathsig`] instead in that case.
pub fn apply_isolation(cmd: &mut tokio::process::Command) {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "pre_exec installer + setsid must share one unsafe context"
    )]
    // SAFETY: pre_exec runs between fork and exec; setsid and prctl are
    // documented async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            apply_pdeathsig_inline()?;
            Ok(())
        });
    }
}

/// Just the `PR_SET_PDEATHSIG` half of [`apply_isolation`]. Use this for
/// commands that already manage their own session (e.g. PTY-attached shells).
pub fn apply_pdeathsig(cmd: &mut tokio::process::Command) {
    // SAFETY: pre_exec runs between fork and exec; prctl is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            apply_pdeathsig_inline()?;
            Ok(())
        });
    }
}

#[inline]
fn apply_pdeathsig_inline() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl with PR_SET_PDEATHSIG is async-signal-safe.
        let r = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };
        if r < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS has no equivalent; rely on the supervisor socket EOF path.
    }
    Ok(())
}

/// Send `SIGKILL` to a whole process group. Used on cancellation so that
/// grandchildren (e.g. processes a bash target itself forked) are reaped, not
/// just the direct child PID.
///
/// Safe to call with a pgid whose group has already exited — `killpg` returns
/// `ESRCH` which we ignore.
pub fn kill_pgid(pgid: i32) {
    // SAFETY: killpg with SIGKILL; kernel rejects unknown pgids cleanly.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

/// Register a freshly-spawned child's pgid with the supervisor, returning a
/// guard that untracks on drop. If the supervisor is unavailable (e.g. in
/// unit tests where [`init`] never ran), logs a warning and returns `None`
/// rather than failing the build — the caller still benefits from `kill_on_drop`
/// as a fallback.
pub fn register_child(pgid: i32) -> Option<TrackGuard> {
    let tracker = tracker();
    if let Err(e) = tracker.track(pgid) {
        tracing::warn!(pgid, error = %format!("{e:#}"), "child not registered with process supervisor");
        return None;
    }
    Some(TrackGuard::new(tracker, pgid))
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
