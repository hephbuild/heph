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
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
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
/// Two layers:
///   1. [`crate::process_watcher`] — dedicated OS thread driving a
///      `kqueue EVFILT_PROC` (macOS) or `pidfd + epoll` (Linux) loop.
///      Reaps via `waitpid(WNOHANG)` and signals completion through a
///      `tokio::sync::oneshot`.
///   2. `block_in_place` + `Receiver::blocking_recv` — synchronously
///      blocks the calling worker on the oneshot. Crucially this uses
///      kernel `thread::park`, NOT tokio's cross-thread waker
///      (`mio::Waker` → `EVFILT_USER`), which is observed to silently
///      drop wakeups on macOS under heavy concurrent spawn load. The
///      multi-thread runtime tolerates one blocked worker; tokio
///      compensates by growing the blocking pool.
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
    let rx = crate::process_watcher::register(pid);
    if is_multi_thread_runtime() {
        // Multi-thread (production): block on the oneshot synchronously
        // via `block_in_place` + `blocking_recv`. Uses kernel `thread::park`
        // wake — bypasses tokio's broken cross-thread waker.
        tokio::task::block_in_place(move || rx.blocking_recv()).map_err(|recv_err| {
            std::io::Error::other(format!("process watcher dropped sender: {recv_err}"))
        })?
    } else {
        // Current-thread (tests): `blocking_recv` panics inside a runtime
        // and `block_in_place` panics on current-thread. Plain `.await`
        // is fine — the waker bug only manifests under high concurrency
        // which tests don't reach.
        rx.await.map_err(|recv_err| {
            std::io::Error::other(format!("process watcher dropped sender: {recv_err}"))
        })?
    }
}

/// Run `f` synchronously, using `block_in_place` when the current tokio
/// runtime is multi-threaded (so other workers keep progressing) and a
/// direct call otherwise. Used for filesystem ops on the hot path where
/// `tokio::fs::*` (which routes through `spawn_blocking`) has been
/// observed to lose wake-ups on macOS under heavy load. Process waits
/// use `process_watcher` instead.
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
        .map(|h| {
            matches!(
                h.runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            )
        })
        .unwrap_or(false)
}

/// Polling-based replacement for `tokio::process::Child::wait_with_output`.
/// Drains piped stdout/stderr on dedicated OS threads while polling for exit.
///
/// Why OS threads, not `tokio::io::AsyncReadExt::read_to_end` joined with the
/// wait future: [`wait_polling`] enters `block_in_place(|| blocking_recv())`,
/// which parks the worker synchronously inside the calling task's `poll()`.
/// Any sibling future joined on the same task (`try_join!`, `select!`) never
/// gets polled while that block is active — so the child's stdout pipe fills
/// (64KiB on macOS), the child blocks on `write()`, and the wait never
/// resolves. Draining on threads decouples the two completely.
///
/// Why dup the pipe fds: `tokio::process::ChildStdout` owns its fd via a
/// `PollEvented` and only exposes `AsRawFd`. We dup to get an independent
/// `OwnedFd` for the reader thread, then drop the tokio wrapper. Both ends
/// point at the same pipe; tokio's drop closes its copy, our copy keeps the
/// read end open until the thread finishes.
///
/// Borrows the child by `&mut` so callers in a `tokio::select!` can fall back
/// to [`wait_polling`] on cancellation — owning the child here would mean the
/// dropped future takes the Child with it without ever calling `waitpid`,
/// leaving a `Z` entry in the kernel process table.
pub async fn wait_with_output_polling(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::Output> {
    let stdout_thread = spawn_pipe_drain(child.stdout.take().as_ref())?;
    let stderr_thread = spawn_pipe_drain(child.stderr.take().as_ref())?;

    let status = wait_polling(child).await?;

    let (stdout_res, stderr_res) = tokio::task::block_in_place(|| {
        // `unwrap` on `join`: the drain closure cannot panic — `Read` errors
        // are propagated via `io::Result`. A panic here would indicate a bug
        // in `std::io` or the kernel returning something `File::read_to_end`
        // refuses to handle; propagating it is the correct behavior.
        #[expect(
            clippy::unwrap_used,
            reason = "drain thread cannot panic; surfaces real bugs"
        )]
        let stdout = stdout_thread.join().unwrap();
        #[expect(clippy::unwrap_used, reason = "same as above")]
        let stderr = stderr_thread.join().unwrap();
        (stdout, stderr)
    });

    Ok(std::process::Output {
        status,
        stdout: stdout_res?,
        stderr: stderr_res?,
    })
}

/// Dup a tokio pipe handle's fd into an `OwnedFd` and spawn an OS thread that
/// reads it to EOF. Returns a `JoinHandle` even when the input is `None` (the
/// closure short-circuits with an empty buffer) so the caller doesn't branch.
///
/// Tokio sets `O_NONBLOCK` on its pipe fds. `dup` shares the open file
/// description, so the flag is shared too — clear it before reading or
/// `File::read` returns `EAGAIN` immediately. Clearing also affects tokio's
/// view, but the caller drops the tokio wrapper before the thread starts
/// reading, so nothing else cares.
fn spawn_pipe_drain<F: AsRawFd>(
    pipe: Option<&F>,
) -> std::io::Result<std::thread::JoinHandle<std::io::Result<Vec<u8>>>> {
    use std::io::Read as _;

    let owned_fd: Option<OwnedFd> = match pipe {
        Some(p) => {
            // SAFETY: dup of a valid fd we hold via the tokio wrapper. The
            // returned fd is independent and owned exclusively by us.
            let raw = unsafe { libc::dup(p.as_raw_fd()) };
            if raw < 0 {
                return Err(std::io::Error::last_os_error());
            }
            clear_nonblocking(raw)?;
            // SAFETY: dup just produced this fd; we own it.
            Some(unsafe { OwnedFd::from_raw_fd(raw) })
        }
        None => None,
    };

    Ok(std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        if let Some(fd) = owned_fd {
            std::fs::File::from(fd).read_to_end(&mut buf)?;
        }
        Ok(buf)
    }))
}

fn clear_nonblocking(fd: RawFd) -> std::io::Result<()> {
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "F_GETFL and F_SETFL must share one unsafe context"
    )]
    // SAFETY: fcntl(F_GETFL/F_SETFL) on a fd we just dup'd and own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
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
