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
//!
//! Set `RHEPH_DISABLE_REAPER=1` to bypass the sidecar and the polling
//! `waitpid` thread — [`init`] becomes a no-op, [`register_child`] returns
//! `None`, and [`wait_polling`] (plus [`wait_with_output_polling`] via it)
//! delegates to `tokio::process::Child::wait().await`. Use for bisecting
//! whether a hang lives in this layer.

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

/// `RHEPH_DISABLE_REAPER=1` short-circuits every supervisor entry point so
/// subprocess execution falls through to tokio's built-in waker. Cached in
/// a `OnceLock` because `std::env::var` takes a global libc mutex; consistent
/// with the `stall_threshold` / `cycle_detection_enabled` pattern in
/// `src/hmemoizer/mod.rs`.
fn reaper_disabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| matches!(std::env::var("RHEPH_DISABLE_REAPER").as_deref(), Ok("1")))
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

/// Wait for `child` to exit and reap the zombie.
///
/// **Implementation**: `std::thread` running a WNOHANG poll loop with
/// `std::thread::sleep` for backoff, signaling completion via a
/// `tokio::sync::oneshot`. The async caller only awaits the oneshot.
///
/// Why this design (after iterating through several alternatives that
/// each broke in specific macOS edge cases):
///   1. `tokio::process::Child::wait().await` — SIGCHLD-based, unreliable
///      on macOS once the child is a session leader (PTY/shell mode) and
///      under heavy runtime load even for non-setsid children.
///   2. `std::thread` running blocking `waitpid(pid, 0)` — suffers the
///      macOS waitpid race: when tokio's internal `waitpid(-1, WNOHANG)`
///      reaps the same pid first, the kernel does NOT wake other threads
///      parked in `waitpid(pid, 0)`. Parked forever.
///   3. Pure async WNOHANG polling with `tokio::time::sleep` — under
///      heavy concurrent spawn/spawn_blocking load on macOS, tokio's
///      timer driver and blocking-pool JoinHandle wake-up stop firing
///      (worker threads stay parked in `__psynch_cvwait`/`kevent`); even
///      `tokio::time::sleep` doesn't wake.
///   4. `child.try_wait()` polling — reads tokio's internal reaper cache
///      which is updated by the same starved task; never surfaces status.
///
/// This design uses three primitives that survive the macOS pathologies:
///   - `std::thread` for the wait loop — kernel-scheduled, not blocked
///     by tokio runtime sickness.
///   - `WNOHANG waitpid` — non-blocking; either reaps the zombie (real
///     status), sees the child still running (sleep + retry), or hits
///     ECHILD (already reaped by tokio; we synthesize success since
///     status is lost).
///   - `std::thread::sleep` — kernel sleep, independent of tokio timer.
///   - `tokio::sync::oneshot` — the most fundamental tokio wake-up;
///     uses task notification, not timer/IO driver/signal driver. If
///     this is broken the runtime is unrecoverable.
pub async fn wait_polling(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    if reaper_disabled() {
        return child.wait().await;
    }
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("child has no pid (already waited)"))?
        as i32;

    crate::hmemoizer::set_phase("wait_polling:rx_await");
    if is_multi_thread_runtime() {
        // Production: block one worker, others keep scheduling tasks.
        // macOS waker delivery is broken; we cannot reliably yield and
        // be polled back.
        tokio::task::block_in_place(|| sync_wait_poll(pid))
    } else {
        // Tests (current_thread runtime): low concurrency, tokio's
        // native `child.wait()` is reliable here. block_in_place would
        // panic on current_thread.
        child.wait().await
    }
}

/// Run `f` synchronously, using `block_in_place` when the current tokio
/// runtime is multi-threaded (so other workers keep progressing) and a
/// direct call otherwise. Tests run under the current-thread runtime
/// where block_in_place panics; production uses multi-thread.
pub fn block_or_inline<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    if is_multi_thread_runtime() {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

fn is_multi_thread_runtime() -> bool {
    tokio::runtime::Handle::try_current()
        .map(|h| matches!(h.runtime_flavor(), tokio::runtime::RuntimeFlavor::MultiThread))
        .unwrap_or(false)
}

fn sync_wait_poll(pid: i32) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt as _;
    let mut delay_ms: u64 = 1;
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: WNOHANG waitpid is non-blocking; side-effect is
        // reaping the zombie (intentional).
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r > 0 {
            return Ok(std::process::ExitStatus::from_raw(status));
        }
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                tracing::debug!(
                    pid,
                    "wait_polling: ECHILD — child reaped by tokio; \
                     synthesizing successful exit (real status lost)"
                );
                return Ok(std::process::ExitStatus::from_raw(0));
            }
            return Err(err);
        }
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        delay_ms = (delay_ms * 2).min(100);
    }
}

/// Polling-based replacement for `tokio::process::Child::wait_with_output`.
/// Drains piped stdout/stderr while polling `try_wait` for exit. Avoids
/// tokio's SIGCHLD-based waker (see [`wait_polling`]).
///
/// Borrows the child by `&mut` so callers in a `tokio::select!` can fall back
/// to [`wait_polling`] on cancellation — owning the child here would mean the
/// dropped future takes the Child with it without ever calling `waitpid`,
/// leaving a `Z` entry in the kernel process table.
pub async fn wait_with_output_polling(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::Output> {
    use tokio::io::AsyncReadExt as _;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_fut = async {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            s.read_to_end(&mut buf).await?;
        }
        Ok::<_, std::io::Error>(buf)
    };
    let stderr_fut = async {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            s.read_to_end(&mut buf).await?;
        }
        Ok::<_, std::io::Error>(buf)
    };
    let wait_fut = wait_polling(child);
    let (status, stdout_buf, stderr_buf) = tokio::try_join!(wait_fut, stdout_fut, stderr_fut)?;
    Ok(std::process::Output {
        status,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Send `SIGKILL` to a child by id. Always issues both `killpg` and `kill`:
///   - `killpg` reaps the whole tree if the child is a session leader
///     (i.e. it called `setsid` in `pre_exec` — e.g. pluginexec shell mode).
///   - `kill` covers drivers that did not `setsid` (plugingo, pluginnix).
///
/// PIDs are unique system-wide, so `killpg(pid)` is a safe no-op when `pid`
/// is not a group leader. `ESRCH` from either call is ignored.
pub fn kill_child(pid: i32) {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "killpg + kill are paired best-effort reap of the same pid"
    )]
    // SAFETY: SIGKILL via killpg/kill on a pid we spawned and own.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
        libc::kill(pid, libc::SIGKILL);
    }
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
