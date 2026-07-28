//! Off-tokio subprocess pipeline.
//!
//! On macOS, every step of the subprocess lifecycle (spawn, stdin pump,
//! stdout/stderr drain, wait, kill) runs on `std::process` + `std::thread` +
//! `std::sync::mpsc`. The only point that touches the tokio runtime is the
//! final boundary, where the calling task synchronously parks via
//! `block_in_place(|| std_rx.recv())` on a kernel condvar. This bypasses
//! tokio's cross-thread waker (`mio::Waker` → `EVFILT_USER`), which is
//! observed to silently drop wake-ups on macOS under heavy concurrent load.
//! See `RCA_MACOS_WAKER.md`.
//!
//! On Linux, the entire bug class is absent (`epoll` + `pidfd`, no
//! `EVFILT_USER`), so we use `tokio::process` directly with no workarounds.
//!
//! Public surface:
//! - [`Spec`] — declarative description of a child to spawn.
//! - [`output`] — batch: spawn, capture stdout/stderr, wait, return `Output`.
//! - [`spawn`] — low-level: returns a [`Handle`] for streaming + stdin pump.

use hcore::hasync::Cancellable;
use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::{Output, Stdio};

#[cfg(target_os = "macos")]
mod imp_macos;
#[cfg(target_os = "macos")]
use imp_macos as imp;

#[cfg(not(target_os = "macos"))]
mod imp_linux;
#[cfg(not(target_os = "macos"))]
use imp_linux as imp;

/// Standard chunk size for pipe drains in streaming mode. Matches the
/// previous `tee_stream` buffer size for byte-for-byte compatibility.
pub const CHUNK_SIZE: usize = 8192;

/// Grace window granted to a child after a cancellation `SIGINT` before we
/// escalate to `SIGKILL`. Mirrors a terminal Ctrl-C: well-behaved children
/// (and their descendants) get a chance to unwind and exit cleanly; anything
/// still alive after this is hard-killed so the runtime can't be parked
/// waiting on a child that ignores the interrupt.
pub const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Grace window granted to the stdout/stderr drains *after* the child has
/// been reaped.
///
/// Everything the child itself wrote is, by the time it exits, either
/// already read or sitting in the pipe buffer (64 KiB), so this window only
/// ever needs to cover the tail of a pipe that is guaranteed to hit EOF.
/// What it protects against is the opposite case: a descendant the child
/// double-forked (`go list` → `git` → `git credential-cache--daemon`, which
/// lingers for 900 s) inherited the write end and never closes it, so the
/// drain never sees EOF. Without a bound the caller parks on a read that
/// only the daemon's own lifetime can end, and no cancellation token is in
/// that path — Ctrl-C cannot unstick it either.
///
/// Past the window the drains are abandoned and whatever was collected is
/// returned, with a `tracing::warn!` carrying the pid and the byte counts.
///
/// How strong "abandoning loses nothing of the child's" is differs by
/// backend, and the difference is worth knowing before relying on it:
///
/// - **Linux** — the guarantee is exact. `tokio::time::timeout` polls the
///   readers before it polls the delay, and a reader loops until the pipe
///   returns `EAGAIN`, so the poll immediately preceding the timeout has
///   already drained the pipe dry. Anything lost was written after the child
///   was reaped *and* after the pipe emptied: a descendant's bytes.
/// - **macOS** — the guarantee is statistical. Reading happens on drain
///   threads and completion is observed through an `AtomicBool` on a wall
///   clock, so a drain thread starved for the whole window would look
///   identical to one blocked on a stray descendant. It gets two windows
///   (500 ms, `killpg`, 500 ms) against a 64 KiB pipe tail that needs a
///   handful of `read`s, so the margin is ~4 orders of magnitude — but it is
///   a margin, not a proof. Closing it means moving that backend off drain
///   threads, which is its own change.
pub const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_millis(500);

/// Stdio configuration variants supported by [`Spec`]. Mirrors
/// `std::process::Stdio` but is `Clone` so a `Spec` can be inspected /
/// retried without consuming inherited fds.
pub enum StdioSpec {
    Null,
    Inherit,
    Piped,
    /// Take ownership of an existing fd (used for PTY slave inheritance).
    Fd(OwnedFd),
}

impl StdioSpec {
    fn into_stdio(self) -> Stdio {
        match self {
            StdioSpec::Null => Stdio::null(),
            StdioSpec::Inherit => Stdio::inherit(),
            StdioSpec::Piped => Stdio::piped(),
            StdioSpec::Fd(fd) => Stdio::from(fd),
        }
    }
}

/// Declarative spec for spawning a child process.
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Cleared environment (`env_clear`) populated from this list. The driver
    /// is responsible for selecting which host env vars to pass through.
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub stdin: StdioSpec,
    pub stdout: StdioSpec,
    pub stderr: StdioSpec,
    /// If true, `pre_exec` calls `setsid()` so the child becomes session
    /// leader (pgid == pid). Required for PTY ctty assignment and for the
    /// supervisor's `killpg` to reap the whole tree.
    pub setsid: bool,
    /// If true, `pre_exec` calls `ioctl(0, TIOCSCTTY, 0)` to make the child's
    /// controlling terminal point at the inherited stdin fd. Only meaningful
    /// when `stdin` was set to a PTY slave fd.
    pub ctty: bool,
}

/// Batch run: spawn, capture stdout/stderr to `Vec<u8>`, wait, return.
///
/// `cancel` aborts the wait by sending `SIGKILL` to the child (and its pgid
/// if `spec.setsid` is set). The function still waits for the kernel to
/// confirm the exit before returning the cancel error.
pub async fn output(spec: Spec, cancel: &(dyn Cancellable + Send + Sync)) -> io::Result<Output> {
    imp::output(spec, cancel).await
}

/// Low-level spawn returning a [`Handle`] with per-stream chunked readers
/// and an optional stdin pump. Used for streaming output (pluginexec) where
/// the caller wants chunks delivered to a TUI as the child writes them.
pub fn spawn(spec: Spec) -> io::Result<Handle> {
    imp::spawn(spec)
}

pub use imp::{ChunkReader, Handle, StdinPump};

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use std::time::{Duration, Instant};

    /// Whether the child gets its own session (`setsid`), which is what makes
    /// the supervisor's `killpg` able to reach a descendant. Every real
    /// caller of [`output`] passes `Inherited`; only `pluginexec`'s streaming
    /// path uses `Own`. Named rather than a bare `bool` because which one a
    /// test picks decides whether `killpg` can rescue it.
    #[derive(Clone, Copy)]
    enum Session {
        Own,
        Inherited,
    }

    /// Locate a tool without assuming an FHS layout. `/bin/sh` is guaranteed
    /// everywhere we support (including NixOS, where the dev shell lives),
    /// but `/bin/sleep` is not — so resolve it and skip rather than fail with
    /// a bare `ENOENT` that reads like a broken test.
    fn find_tool(name: &str) -> Option<PathBuf> {
        let direct = PathBuf::from("/bin").join(name);
        if direct.exists() {
            return Some(direct);
        }
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join(name))
            .find(|p| p.exists())
    }

    fn sh_spec(session: Session, script: &str) -> Spec {
        Spec {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(script)],
            env: Vec::new(),
            cwd: PathBuf::from("/"),
            stdin: StdioSpec::Null,
            stdout: StdioSpec::Piped,
            stderr: StdioSpec::Piped,
            setsid: matches!(session, Session::Own),
            ctty: false,
        }
    }

    /// How long the backgrounded descendant holds the pipe. Must exceed
    /// [`STRAY_DAEMON_CAP`] by enough that a genuine regression is
    /// unambiguous rather than a near miss.
    const STRAY_LIFETIME_SECS: u64 = 30;

    /// Worst-case bounded drain is two windows plus a `killpg` on macOS
    /// (~1s), one window on Linux (~0.5s). 5s leaves room for a loaded CI
    /// runner while staying far below the [`STRAY_LIFETIME_SECS`] a
    /// regression would take.
    const STRAY_DAEMON_CAP: Duration = Duration::from_secs(5);

    /// `sh` backgrounds a long-lived descendant that inherits stdout/stderr,
    /// runs `body`, then exits. The descendant keeps the pipe write end open
    /// for its full lifetime, so any drain that waits for EOF is pinned to
    /// the descendant's clock rather than the child's.
    fn stray_daemon_script(sleep: &std::path::Path, body: &str) -> String {
        format!(
            "( {} {STRAY_LIFETIME_SECS} ) & {body}; exit 0",
            sleep.display()
        )
    }

    macro_rules! sleep_tool {
        () => {
            match find_tool("sleep") {
                Some(p) => p,
                None => {
                    eprintln!("skipping: no `sleep` on this host");
                    return;
                }
            }
        };
    }

    /// Regression: a child that backgrounds a long-lived descendant
    /// inheriting stdout/stderr must not park the wait indefinitely.
    ///
    /// Targets `wait_or_cancel`, not `wait`: that is the method `pluginexec`
    /// actually calls, and on macOS it is the one that owns
    /// `drain_with_deadline`. (`Handle::wait` has no production caller, and
    /// on Linux neither method touches the readers at all — they are moved
    /// out by `take_stdout`/`take_stderr` — so this asserts a bound that only
    /// the macOS backend can currently violate.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stray_daemon_does_not_park_wait() {
        let sleep = sleep_tool!();
        let spec = sh_spec(Session::Own, &stray_daemon_script(&sleep, ":"));
        let handle = spawn(spec).expect("spawn");
        let cancel = StdCancellationToken::new();
        let started = Instant::now();
        let status = handle
            .wait_or_cancel(&cancel)
            .await
            .expect("wait should return");
        let elapsed = started.elapsed();

        assert!(status.success(), "child should exit 0; got {status:?}");
        assert!(
            elapsed < STRAY_DAEMON_CAP,
            "wait_or_cancel took {elapsed:?} — the drain join must be bounded",
        );
    }

    /// Regression for the batch `output()` API, which plugin-go and
    /// plugin-nix use. It has no cancellation token on the drain side, so an
    /// unbounded post-exit drain is unreachable by Ctrl-C: the run is stuck
    /// until the stray descendant exits on its own.
    ///
    /// `Session::Inherited` mirrors every real caller — which also means the
    /// `killpg` escalation cannot rescue this path, so the deadline is the
    /// only thing that ends the wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stray_daemon_does_not_park_output() {
        let sleep = sleep_tool!();
        let script = stray_daemon_script(&sleep, "echo hello; echo problem >&2");
        let spec = sh_spec(Session::Inherited, &script);
        let cancel = StdCancellationToken::new();
        let started = Instant::now();
        let out = output(spec, &cancel).await.expect("output should return");
        let elapsed = started.elapsed();

        assert!(
            out.status.success(),
            "child should exit 0; got {:?}",
            out.status
        );
        // Everything the child itself wrote must survive the bounded drain —
        // on both streams. Abandoning the readers must not cost us the output
        // we came for.
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "problem\n");
        assert!(
            elapsed < STRAY_DAEMON_CAP,
            "output() took {elapsed:?} — the post-exit drain must be bounded",
        );
    }

    /// The sharp edge of a bounded drain: a payload far past the 64 KiB pipe
    /// buffer *and* a descendant holding the pipe open. Every byte is the
    /// child's, so the deadline must not cost us any of it — this is the test
    /// that would catch a bound that abandons the drain while real data is
    /// still queued.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stray_daemon_does_not_truncate_large_output() {
        const PAYLOAD: usize = 1_048_576;
        let sleep = sleep_tool!();
        let script = stray_daemon_script(
            &sleep,
            &format!("yes hello | head -c {PAYLOAD}; yes nope | head -c {PAYLOAD} >&2"),
        );
        let spec = sh_spec(Session::Inherited, &script);
        let cancel = StdCancellationToken::new();
        let started = Instant::now();
        let out = output(spec, &cancel).await.expect("output should return");
        let elapsed = started.elapsed();

        assert_eq!(
            out.stdout.len(),
            PAYLOAD,
            "stdout truncated by the drain bound"
        );
        assert_eq!(
            out.stderr.len(),
            PAYLOAD,
            "stderr truncated by the drain bound"
        );
        assert!(
            out.stdout.iter().all(|&b| b != b'n'),
            "stdout got stderr's bytes"
        );
        assert!(
            elapsed < STRAY_DAEMON_CAP,
            "output() took {elapsed:?} — the post-exit drain must be bounded",
        );
    }

    /// A failing child must still hand back both streams along with its
    /// status — nothing may short-circuit the collection on a non-zero exit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn output_returns_both_streams_on_failure() {
        let spec = sh_spec(Session::Inherited, "echo out; echo err >&2; exit 3");
        let cancel = StdCancellationToken::new();
        let out = output(spec, &cancel).await.expect("output should return");

        assert_eq!(out.status.code(), Some(3), "status {:?}", out.status);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    }

    /// Cancelling `output()` must surface `Err` within the escalation budget
    /// rather than riding the child's own lifetime. This is the contract both
    /// backends have to agree on, and the reason the item exists: before the
    /// bound, the drain could outlive the cancel indefinitely.
    ///
    /// The token is fired from a *separate task* on purpose. On macOS
    /// `wait_or_cancel` parks its worker in `block_in_place`, so a canceller
    /// `join!`ed onto the same task would never be polled and the run would
    /// ride the child to its natural exit — which is how the real callers
    /// work anyway (the Ctrl-C handler lives elsewhere), but it is a trap
    /// worth naming.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_output_returns_within_budget() {
        let sleep = sleep_tool!();
        let spec = sh_spec(
            Session::Inherited,
            &format!("{} {STRAY_LIFETIME_SECS}", sleep.display()),
        );
        let cancel = std::sync::Arc::new(StdCancellationToken::new());
        let canceller = tokio::spawn({
            let cancel = std::sync::Arc::clone(&cancel);
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                cancel.cancel();
            }
        });

        let started = Instant::now();
        let res = output(spec, cancel.as_ref()).await;
        let elapsed = started.elapsed();
        canceller.await.expect("canceller task");

        assert!(res.is_err(), "cancelled output must not return Ok");
        // SIGINT, then CANCEL_GRACE, then SIGKILL, then at worst two drain
        // windows on macOS — plus slack for a loaded runner.
        let budget = CANCEL_GRACE + DRAIN_DEADLINE * 2 + Duration::from_secs(3);
        assert!(
            elapsed < budget,
            "cancelled output took {elapsed:?}, budget {budget:?}",
        );
    }
}
