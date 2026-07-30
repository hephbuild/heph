//! Off-tokio subprocess pipeline.
//!
//! On macOS, every step of the subprocess lifecycle (spawn, stdin pump,
//! stdout/stderr drain, wait, kill) runs on `std::process` + `std::thread` +
//! `std::sync::mpsc`. The only point that touches the tokio runtime is the
//! final boundary, where the calling task synchronously parks via
//! `block_in_place(|| std_rx.recv_timeout(..))` on a kernel condvar.
//!
//! **The macOS waker hazard.** Tokio's cross-thread wake path on macOS is
//! `mio::Waker` → a `kqueue` `EVFILT_USER` trigger. Under heavy concurrent
//! load we observed wake-ups being dropped: a task that returned `Pending`
//! and was later woken from a *different* thread would sometimes never be
//! polled again, hanging a build with no CPU burn and no error. Every design
//! choice in this module follows from avoiding that path — a `std::sync::mpsc`
//! condvar wake is a kernel futex/condvar signal on the same thread that is
//! parked, so it cannot be lost. Nothing in this module may introduce a
//! cross-thread tokio wake.
//!
//! **`yield_now` does not count as "waking from our own thread" here**, which
//! is the trap this module is easiest to get wrong. It is local only while the
//! task still holds a core; after a `block_in_place` the core has usually been
//! handed to another thread, and `Context::defer` then falls through to
//! `wake_by_ref` → `push_remote_task` → `notify_parked_remote` → `mio::Waker`
//! → `EVFILT_USER` — the exact wake above, unretried. So a blocking wait here
//! is never made interruptible by yielding between slices; it is made
//! interruptible by terminating on a flag. See [`OutputReader`].
//!
//! On Linux, the entire bug class is absent (`epoll` + `pidfd`, no
//! `EVFILT_USER`), so we use `tokio::process` directly with no workarounds.
//!
//! Public surface:
//! - [`Spec`] — declarative description of a child to spawn.
//! - [`output`] — batch: spawn, capture stdout/stderr, wait, return `Output`.
//! - [`spawn`] — low-level: returns a [`Handle`] for streaming + stdin pump.
//! - [`OutputReader`] — the streaming consumer: one reader carrying *both*
//!   of the child's output streams, tagged with a [`StreamId`].

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

/// How many [`CHUNK_SIZE`] chunks the *streaming* drain may run ahead of its
/// consumer before the child is made to wait.
///
/// Only the macOS backend has a buffer to bound: it reads the pipes on
/// dedicated threads and hands chunks over a `std::sync::mpsc`. Unbounded,
/// a child that outruns its consumer parks its entire output in the parent's
/// RAM. Bounded, the drain thread blocks in `send`, stops reading, the pipe
/// fills, and the child blocks in `write(2)` — which is exactly what Linux
/// does natively, where the only buffer is the 64 KiB kernel pipe and
/// `ChunkReader` reads straight out of it. So the bound is what makes the two
/// backends agree; the residual difference is buffer *size*, not semantics.
///
/// The bound is on **messages**, not bytes, so the ceiling is 64 chunks of at
/// most [`CHUNK_SIZE`]: 512 KiB per child on top of the kernel pipe, ~10 MiB
/// across `2·ncpu` concurrent targets on a 10-core machine, ~64 MiB on a
/// 64-core one. That is a ceiling, not a resident cost — a consumer that keeps
/// up leaves 0–2 messages in flight, so a healthy run never approaches it. The
/// unbounded worst case it replaces is "every byte the noisiest target ever
/// printed", held whole.
///
/// 512 KiB of slack is ~5 ms of a consumer whose per-chunk work is one write
/// to an open log file, so the number is a memory ceiling rather than a
/// smoothing buffer. A line-buffered child is throttled after 64 small writes
/// regardless of their size, which is the same statement seen from the other
/// side.
///
/// The batch [`output`] path is deliberately **not** bounded — see
/// [`Handle::take_output`].
pub const STREAM_DRAIN_CHUNKS: usize = 64;

/// Which of a child's two output streams a chunk came from.
///
/// Both streams arrive through a single [`OutputReader`], so the tag is how a
/// consumer routes a chunk to the right sink. Merging them is not a
/// convenience: it is what removes head-of-line blocking between the two
/// streams on macOS, where a per-stream reader blocks its whole task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StreamId {
    Stdout,
    Stderr,
}

impl std::fmt::Display for StreamId {
    /// The conventional lowercase names. Diagnostics carry this rather than
    /// the `Debug` form, which is not something callers should be matching on.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match *self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

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

/// Low-level spawn returning a [`Handle`] with a merged [`OutputReader`] and
/// an optional stdin pump. Used for streaming output (pluginexec) where the
/// caller wants chunks delivered to a log file and a TUI as the child writes
/// them.
///
/// The drain is bounded ([`STREAM_DRAIN_CHUNKS`]); a consumer that stalls
/// backpressures the child rather than growing the parent's heap.
pub fn spawn(spec: Spec) -> io::Result<Handle> {
    imp::spawn(spec)
}

pub use imp::{Handle, OutputReader, StdinPump};

impl Handle {
    /// Wait for the child to exit on a task of its own, cancelling via
    /// `cancel` (SIGINT → grace → SIGKILL). Returns the join handle.
    ///
    /// **The spawn is the API.** Waiting and reading the child's output must
    /// not share a task: on macOS both park the worker in `block_in_place`,
    /// and the wait's park only ends when the child does — so a `join!` of the
    /// two resolves to "nothing drains the pipes, the child blocks in
    /// `write(2)` once they fill, the child never exits". A deadlock, not a
    /// slowdown. That was previously a comment every caller had to obey by
    /// hand; making the only public wait spawn itself means it cannot be got
    /// wrong. `wait_or_cancel` stays crate-private for [`output`], which has
    /// no concurrent reader.
    ///
    /// `cancel` is an `Arc` rather than a borrow because the task outlives the
    /// caller's frame.
    pub fn spawn_wait(
        self,
        cancel: std::sync::Arc<dyn Cancellable + Send + Sync>,
    ) -> tokio::task::JoinHandle<io::Result<std::process::ExitStatus>> {
        tokio::spawn(async move { self.wait_or_cancel(cancel.as_ref()).await })
    }
}

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

    /// Both streams must arrive through the one reader, correctly tagged, and
    /// the reader must keep serving the survivor after the first stream ends
    /// rather than reporting EOF for the pair.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn merged_reader_tags_and_outlives_each_stream() {
        // stderr finishes first; stdout keeps going afterwards. A merge that
        // reported EOF on the first `None` would lose "late".
        let spec = sh_spec(
            Session::Own,
            "echo early >&2; exec 2>&-; echo late; echo later",
        );
        let mut handle = spawn(spec).expect("spawn");
        let mut reader = handle.take_output().expect("both streams are piped");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some((stream, chunk)) =
            tokio::time::timeout(Duration::from_secs(5), reader.recv())
                .await
                .expect("merged reader must reach EOF")
                .expect("no read error")
        {
            match stream {
                StreamId::Stdout => stdout.extend_from_slice(&chunk),
                StreamId::Stderr => stderr.extend_from_slice(&chunk),
            }
        }

        assert_eq!(String::from_utf8_lossy(&stderr), "early\n");
        assert_eq!(String::from_utf8_lossy(&stdout), "late\nlater\n");

        let cancel = StdCancellationToken::new();
        let status = handle.wait_or_cancel(&cancel).await.expect("wait");
        assert!(status.success(), "status {status:?}");
    }

    /// The mirror image, so neither "the survivor is stdout" nor "the survivor
    /// is stderr" is the only case the merge is ever asked for. On Linux these
    /// are two different arms of the `select!`; on macOS they are two orders
    /// of sender drop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn merged_reader_outlives_stdout_too() {
        let spec = sh_spec(
            Session::Own,
            "echo early; exec 1>&-; echo late >&2; echo later >&2",
        );
        let mut handle = spawn(spec).expect("spawn");
        let mut reader = handle.take_output().expect("both streams are piped");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some((stream, chunk)) =
            tokio::time::timeout(Duration::from_secs(5), reader.recv())
                .await
                .expect("merged reader must reach EOF")
                .expect("no read error")
        {
            match stream {
                StreamId::Stdout => stdout.extend_from_slice(&chunk),
                StreamId::Stderr => stderr.extend_from_slice(&chunk),
            }
        }

        assert_eq!(String::from_utf8_lossy(&stdout), "early\n");
        assert_eq!(String::from_utf8_lossy(&stderr), "late\nlater\n");

        let cancel = StdCancellationToken::new();
        let status = handle.wait_or_cancel(&cancel).await.expect("wait");
        assert!(status.success(), "status {status:?}");
    }

    /// Payload chosen to dwarf every buffer between the child and the
    /// consumer — the 64 KiB kernel pipe plus the macOS drain channel's
    /// [`STREAM_DRAIN_CHUNKS`] × [`CHUNK_SIZE`] — so a child that manages to
    /// run to completion against a stalled consumer proves the buffering is
    /// unbounded.
    const BACKPRESSURE_PAYLOAD: usize = 8 * 1024 * 1024;

    /// The streaming drain must not run arbitrarily far ahead of its consumer.
    ///
    /// A consumer that takes one chunk and then stalls must leave the child
    /// blocked in `write(2)`, not sitting on a parent-side buffer holding its
    /// entire output. That is what caps heph's memory when a target is far
    /// noisier than the terminal or log sink can absorb.
    ///
    /// Linux gets this for free — `OutputReader` reads the pipe directly, so
    /// the only buffer is the kernel's 64 KiB and the test passes on the
    /// unfixed code too. **macOS is where it discriminates**: the drain
    /// threads there hand chunks over a channel, and unbounded that channel
    /// absorbs the whole 8 MiB while the consumer sleeps.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn streaming_drain_backpressures_a_stalled_consumer() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The child touches this only after the last byte is written, so its
        // absence is proof the child is still blocked rather than merely slow.
        let marker = dir.path().join("payload-written");
        let spec = sh_spec(
            Session::Own,
            &format!(
                "yes heph | head -c {BACKPRESSURE_PAYLOAD}; : > '{}'",
                marker.display()
            ),
        );
        let mut handle = spawn(spec).expect("spawn");
        let mut reader = handle.take_output().expect("stdout is piped");

        let (stream, first) = tokio::time::timeout(Duration::from_secs(5), reader.recv())
            .await
            .expect("first chunk must arrive promptly")
            .expect("no read error")
            .expect("a chunk, not EOF");
        assert_eq!(stream, StreamId::Stdout);

        // Stall. Every buffer in the path is now full and the child is parked
        // in `write`.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !marker.exists(),
            "child wrote all {BACKPRESSURE_PAYLOAD} bytes while the consumer was stalled — \
             the drain is buffering without limit",
        );

        // Resume. Backpressure must only delay the child, never cost a byte.
        let mut total = first.len();
        while let Some((stream, chunk)) =
            tokio::time::timeout(Duration::from_secs(30), reader.recv())
                .await
                .expect("resumed reader must reach EOF")
                .expect("no read error")
        {
            assert_eq!(stream, StreamId::Stdout, "child writes nothing to stderr");
            total += chunk.len();
        }
        assert_eq!(total, BACKPRESSURE_PAYLOAD, "backpressure lost bytes");
        assert!(
            marker.exists(),
            "child never finished after the consumer resumed"
        );

        let cancel = StdCancellationToken::new();
        let status = handle.wait_or_cancel(&cancel).await.expect("wait");
        assert!(status.success(), "status {status:?}");
    }

    /// A reader left waiting on a pipe nobody will ever close must be
    /// endable, and must not lose what it already delivered.
    ///
    /// The child backgrounds a descendant that inherits the pipe write ends
    /// and outlives it, so no EOF is ever coming. On macOS
    /// `drain_with_deadline` spends both windows, fails to reach the
    /// descendant with `killpg` (`Session::Inherited`, as every batch caller
    /// uses), and detaches the drain threads — **with their senders still
    /// alive**, so the merged channel never disconnects either.
    ///
    /// # The two backends end it differently, and that is a real divergence
    ///
    /// The guarantee callers need — "the tee always ends, so the target always
    /// completes" — holds on both. The mechanism does not, because it cannot:
    ///
    /// - **Linux**: `recv` is a genuine `AsyncRead` that stays `Pending`, so
    ///   the caller's `timeout`/`select!` drops it. It never self-terminates;
    ///   `pluginexec`'s 50 ms post-wait drain is what ends it.
    /// - **macOS**: the park is a `block_in_place`, which is never `Pending`,
    ///   so no `timeout` can fire and no `select!` can drop it — the worker
    ///   would be pinned for the descendant's whole lifetime. It therefore has
    ///   to self-terminate, which is what the `abandoned` flag is for.
    ///
    /// Asserting the macOS behaviour on Linux is what an earlier version of
    /// this test did, and CI failed it correctly. The split below is the
    /// honest statement of the contract, not a workaround.
    ///
    /// **The 5 s bound is the test's failure mechanism, not the mechanism
    /// under test**; it and the spawn exist so a regression is a failed
    /// assertion instead of a suite that hangs forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn detached_drain_ends_the_stream_reader() {
        let sleep = sleep_tool!();
        let spec = sh_spec(Session::Inherited, &stray_daemon_script(&sleep, "echo hi"));
        let mut handle = spawn(spec).expect("spawn");
        let mut reader = handle.take_output().expect("stdout is piped");

        // The wait must not share this task with the reader — see `Handle`.
        let waiter = tokio::spawn(async move {
            let cancel = StdCancellationToken::new();
            handle.wait_or_cancel(&cancel).await
        });
        let status = waiter
            .await
            .expect("wait task")
            .expect("wait should return");
        assert!(status.success(), "status {status:?}");

        // Portable half: what the child itself wrote is queued and comes back,
        // whatever ends the reader afterwards. Abandoning must never cost a
        // byte that was already handed over.
        let probe = tokio::spawn(async move {
            let first = reader.recv().await;
            // On macOS this must return by itself; on Linux it never will, so
            // the caller's deadline is what ends it. Either way the *test*
            // must not hang.
            let then = tokio::time::timeout(Duration::from_secs(2), reader.recv()).await;
            (first, then)
        });
        let (first, then) = tokio::time::timeout(Duration::from_secs(5), probe)
            .await
            .expect("the reader pinned its worker: neither it nor a deadline could end it")
            .expect("probe task");

        let (stream, chunk) = first
            .expect("no read error")
            .expect("the queued chunk, not EOF");
        assert_eq!(stream, StreamId::Stdout);
        assert_eq!(String::from_utf8_lossy(&chunk), "hi\n");

        #[cfg(target_os = "macos")]
        assert!(
            then.expect(
                "recv must self-terminate on macOS: a parked `block_in_place` is \
                         never `Pending`, so no deadline can rescue it"
            )
            .expect("no read error")
            .is_none(),
            "the reader must report EOF once the drain is abandoned",
        );
        #[cfg(not(target_os = "macos"))]
        assert!(
            then.is_err(),
            "the reader is expected to stay pending on a pipe held open; ending it is the \
             caller's deadline, got {then:?}",
        );
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
