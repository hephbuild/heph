//! macOS subprocess pipeline: `std::process` + `std::thread` + `std::sync::mpsc`.
//!
//! No tokio types touch the spawn/drain/wait path. The only point where tokio
//! is involved is `block_or_inline` at the async boundary so the
//! calling task synchronously parks on `std::sync::mpsc::Receiver::recv`
//! (kernel condvar). Tokio's cross-thread waker (`mio::Waker` → `EVFILT_USER`)
//! is never used.

use crate::hasync::Cancellable;
use crate::process_supervisor;
use crate::process_watcher;
use std::io::{self, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, ExitStatus, Output};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{CHUNK_SIZE, Spec};

/// Granularity for `recv_timeout` polls during `wait_or_cancel`. 100ms keeps
/// CPU idle while still giving sub-second cancel response. Independent of
/// the watcher's own 1s backstop poll.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Budget for joining drain threads after `exit_rx` has resolved. A drain
/// `read()` still blocking after this strongly implies a surviving
/// descendant (e.g. a daemon double-forked by the immediate child and
/// reparented to pid 1) is holding the pipe write end. We escalate with
/// `killpg` on the pgid (relies on the spawning driver setting
/// `setsid: true`) and grant a second budget.
const DRAIN_JOIN_BUDGET: Duration = Duration::from_millis(500);

/// Polling interval while waiting for drain threads' "done" flags after
/// `exit_rx`. Short enough to keep latency low; large enough not to spin.
const DRAIN_JOIN_POLL: Duration = Duration::from_millis(10);

fn is_multi_thread() -> bool {
    matches!(
        tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread)
    )
}

/// Receive one value from a `std::sync::mpsc::Receiver` asynchronously.
///
/// On multi-thread runtimes, parks the calling worker via `block_in_place`
/// on the channel's internal condvar — kernel wake, bypasses tokio's
/// macOS-flaky cross-thread waker.
///
/// On current-thread runtimes (unit tests only), polls with `try_recv` +
/// `tokio::time::sleep`. `block_in_place` would panic; calling `recv()`
/// directly would block the only thread and starve every sibling task.
/// The polling path is CPU-acceptable under test load and tolerates the
/// macOS waker bug because tests don't reach the concurrency that triggers
/// it.
async fn recv_async<T: Send + 'static>(
    rx: &mut std::sync::mpsc::Receiver<T>,
) -> Result<T, mpsc::RecvError> {
    if is_multi_thread() {
        tokio::task::block_in_place(|| rx.recv())
    } else {
        loop {
            match rx.try_recv() {
                Ok(v) => return Ok(v),
                Err(mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(mpsc::TryRecvError::Disconnected) => return Err(mpsc::RecvError),
            }
        }
    }
}

/// Synchronously park the calling worker on `f` (multi-thread) or call `f`
/// inline (current-thread). Used for short non-blocking work (closing
/// stdin, joining drain threads). Callers MUST NOT pass `f` that performs
/// an indefinite blocking wait on current-thread — use [`recv_async`] for
/// channel waits instead.
fn block_or_inline<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    if is_multi_thread() {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// Async chunk reader backed by a `std::sync::mpsc::Receiver`. Each
/// `recv()` parks the calling worker via `block_in_place` on the condvar
/// inside the channel; never goes through tokio's waker.
pub struct ChunkReader {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
}

/// One spawned drain thread plus a flag the thread flips to `true` right
/// before returning. Lets `Handle::wait` poll for completion under a
/// deadline without calling `JoinHandle::join` (which has no timeout).
struct DrainHandle {
    join: JoinHandle<()>,
    done: Arc<AtomicBool>,
}

impl DrainHandle {
    fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

/// Poll all drain `done` flags until they're set or `budget` elapses.
/// Returns `true` if every drain finished. Uses tokio's timer; the caller
/// must be in async context.
async fn poll_drains(drains: &[DrainHandle], budget: Duration) -> bool {
    if drains.is_empty() {
        return true;
    }
    let deadline = Instant::now() + budget;
    loop {
        if drains.iter().all(DrainHandle::finished) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let sleep = DRAIN_JOIN_POLL.min(deadline - now);
        tokio::time::sleep(sleep).await;
    }
}

/// Join every drain thread, dropping the result. The threads have set
/// `done` so the join is essentially non-blocking — this is just OS-level
/// cleanup of the thread resources.
fn join_finished(drains: Vec<DrainHandle>) {
    block_or_inline(|| {
        for d in drains {
            drop(d.join.join());
        }
    });
}

/// Wait for drain threads to reach EOF after the child has exited.
///
/// Happy path: every drain returns within `DRAIN_JOIN_BUDGET`. Bug-B
/// path: an orphaned descendant (reparented to pid 1 because the
/// immediate child double-forked) is still holding the pipe write end.
/// We escalate with `process_supervisor::kill_child(pid)` — `killpg` on
/// the pgid reaps the whole tree if `setsid: true` was used at spawn.
/// If even that fails (fd dup'd into a process outside our pgid), we
/// log and detach the still-running threads rather than parking the
/// runtime forever.
async fn drain_with_deadline(pid: i32, drains: Vec<DrainHandle>) {
    if drains.is_empty() {
        return;
    }
    if poll_drains(&drains, DRAIN_JOIN_BUDGET).await {
        join_finished(drains);
        return;
    }

    tracing::warn!(
        pid,
        "proc_exec: drain threads still reading after child exit; killpg on pgid"
    );
    process_supervisor::kill_child(pid);

    if poll_drains(&drains, DRAIN_JOIN_BUDGET).await {
        join_finished(drains);
        return;
    }

    let leaked = drains.iter().filter(|d| !d.finished()).count();
    let finished: Vec<DrainHandle> = drains.into_iter().filter(DrainHandle::finished).collect();
    join_finished(finished);
    tracing::warn!(
        pid,
        leaked,
        "proc_exec: drain threads still blocked after killpg; detaching"
    );
    // Stuck thread JoinHandles fall out of scope here; the OS threads
    // remain alive until their read() finally returns. Acceptable leak
    // — the alternative is parking a tokio worker indefinitely.
}

impl ChunkReader {
    /// Wait for the next chunk. Returns `Ok(None)` on EOF (drain thread
    /// finished without error), `Ok(Some(chunk))` for data, or `Err(_)` if
    /// the drain thread reported an io error.
    pub async fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        match recv_async(&mut self.rx).await {
            Ok(Ok(chunk)) => Ok(Some(chunk)),
            Ok(Err(e)) => Err(e),
            Err(_disconnected) => Ok(None),
        }
    }
}

/// Async stdin writer wrapping `std::process::ChildStdin`. Each write goes
/// through `block_in_place` so the caller can sit in a tokio task while the
/// underlying write is a sync `std::io::Write`.
pub struct StdinPump {
    inner: Arc<Mutex<Option<ChildStdin>>>,
}

impl StdinPump {
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let inner = Arc::clone(&self.inner);
        let data = data.to_vec();
        block_or_inline(move || {
            let mut guard = inner
                .lock()
                .map_err(|e| io::Error::other(format!("stdin pump mutex poisoned: {e}")))?;
            if let Some(w) = guard.as_mut() {
                w.write_all(&data)
            } else {
                Err(io::Error::other("stdin pump closed"))
            }
        })
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        let inner = Arc::clone(&self.inner);
        block_or_inline(move || {
            let mut guard = inner
                .lock()
                .map_err(|e| io::Error::other(format!("stdin pump mutex poisoned: {e}")))?;
            // Dropping ChildStdin closes the write end of the pipe; the child
            // sees EOF on its stdin.
            drop(guard.take());
            Ok(())
        })
    }
}

/// Live child handle. Internally owns the `std::process::Child` (whose Drop
/// would orphan the process), plus drain thread join handles and per-stream
/// channels. Consume via [`wait`](Self::wait) or [`wait_or_cancel`] to reap.
pub struct Handle {
    pid: i32,
    child: Option<Child>,
    stdin: Option<StdinPump>,
    stdout: Option<ChunkReader>,
    stderr: Option<ChunkReader>,
    drains: Vec<DrainHandle>,
    reaped: bool,
    /// Receiver registered with `process_watcher` at spawn time. The matching
    /// sender lives in the watcher's `pending` map; that registration is what
    /// guarantees `waitpid` runs even when the Handle is dropped without
    /// `wait*` being called (e.g. caller returns Err between spawn and wait).
    /// Without this, Drop's `kill_child` would SIGKILL the pid but nobody
    /// would reap it — permanent zombie.
    exit_rx: Option<mpsc::Receiver<io::Result<ExitStatus>>>,
    /// Auto-untracks the pid on the supervisor sidecar when the Handle is
    /// dropped. `None` if the supervisor was not initialized (e.g. tests).
    _track_guard: Option<process_supervisor::TrackGuard>,
}

impl Handle {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn take_stdin(&mut self) -> Option<StdinPump> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChunkReader> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChunkReader> {
        self.stderr.take()
    }

    /// Wait for the child to exit, reaping the zombie via the global
    /// `process_watcher` (kqueue `EVFILT_PROC NOTE_EXIT` + `waitpid(WNOHANG)`
    /// backstop). Joins drain threads before returning so callers can rely on
    /// all chunks having been delivered.
    pub async fn wait(mut self) -> io::Result<ExitStatus> {
        let mut rx = self.exit_rx.take().expect("exit_rx must be set by spawn()");
        // The std::process::Child drop would call waitpid blocking; we want
        // the watcher to own the reap. Forget the Child wrapper so its Drop
        // doesn't try to wait. We still close pipe fds (stdin/stdout/stderr)
        // by dropping them inside Child, but the pid itself stays with the
        // watcher.
        // Drop the Child wrapper: `std::process::Child::drop` does NOT call
        // waitpid (documented stdlib behavior), so the pid stays alive for
        // the watcher to reap. Any remaining stdin/stdout/stderr handles
        // (if not already `take`n) are auto-closed via their Drop impls.
        drop(self.child.take());
        let status = recv_async(&mut rx)
            .await
            .map_err(|e| io::Error::other(format!("watcher dropped sender: {e}")))??;
        self.reaped = true;
        // Drain threads should reach EOF naturally now that the child's
        // pipe write ends are gone, but a surviving descendant (e.g. a
        // double-forked daemon reparented to pid 1) may still hold them.
        // `drain_with_deadline` polls, escalates with killpg, then
        // detaches anything still stuck so the runtime can't be parked
        // forever by an orphaned writer.
        let drains = std::mem::take(&mut self.drains);
        drain_with_deadline(self.pid, drains).await;
        Ok(status)
    }

    /// Wait for exit, but cancel by `SIGKILL`-ing the child if `cancel`
    /// fires. Still consumes the final exit status before returning so the
    /// pid is reaped (no zombies).
    pub async fn wait_or_cancel(
        mut self,
        cancel: &(dyn Cancellable + Send + Sync),
    ) -> io::Result<ExitStatus> {
        let rx = self.exit_rx.take().expect("exit_rx must be set by spawn()");
        let pid = self.pid;
        drop(self.child.take());
        // Multi-thread: block_in_place + recv_timeout poll; no tokio waker.
        // Current-thread (tests): async try_recv + tokio::time::sleep poll
        // so sibling tasks can make progress on the single thread.
        let status = if is_multi_thread() {
            let cancel_ref = cancel;
            tokio::task::block_in_place(move || -> io::Result<ExitStatus> {
                // Cancellation escalates: SIGINT first, then SIGKILL if the
                // child outlives the grace window. `interrupted_at` records
                // when we sent SIGINT so the same poll loop can time the grace.
                let mut interrupted_at: Option<Instant> = None;
                let mut killed = false;
                loop {
                    match rx.recv_timeout(CANCEL_POLL_INTERVAL) {
                        Ok(Ok(s)) => return Ok(s),
                        Ok(Err(e)) => return Err(e),
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if cancel_ref.is_cancelled() {
                                match interrupted_at {
                                    None => {
                                        process_supervisor::interrupt_child(pid);
                                        interrupted_at = Some(Instant::now());
                                    }
                                    Some(at) if !killed && at.elapsed() >= super::CANCEL_GRACE => {
                                        process_supervisor::kill_child(pid);
                                        killed = true;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return Err(io::Error::other("watcher dropped sender"));
                        }
                    }
                }
            })?
        } else {
            let mut interrupted_at: Option<Instant> = None;
            let mut killed = false;
            loop {
                match rx.try_recv() {
                    Ok(Ok(s)) => break s,
                    Ok(Err(e)) => return Err(e),
                    Err(mpsc::TryRecvError::Empty) => {
                        if cancel.is_cancelled() {
                            match interrupted_at {
                                None => {
                                    process_supervisor::interrupt_child(pid);
                                    interrupted_at = Some(Instant::now());
                                }
                                Some(at) if !killed && at.elapsed() >= super::CANCEL_GRACE => {
                                    process_supervisor::kill_child(pid);
                                    killed = true;
                                }
                                _ => {}
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return Err(io::Error::other("watcher dropped sender"));
                    }
                }
            }
        };
        self.reaped = true;
        let drains = std::mem::take(&mut self.drains);
        drain_with_deadline(self.pid, drains).await;
        if cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        Ok(status)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // Killed-without-wait path: SIGKILL the child. The pid was registered
        // with `process_watcher` at spawn time, so the watcher's NOTE_EXIT
        // handler (or the 1s WNOHANG backstop) will call `waitpid` and reap
        // the zombie. We do NOT block on the exit_rx here — that would risk a
        // deadlock on a runtime worker — but dropping it is safe because the
        // watcher reaps before it tries to send on the sender.
        process_supervisor::kill_child(self.pid);
    }
}

pub(super) fn spawn(spec: Spec) -> io::Result<Handle> {
    let Spec {
        program,
        args,
        env,
        cwd,
        stdin,
        stdout,
        stderr,
        setsid,
        ctty,
    } = spec;
    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(&cwd)
        .stdin(stdin.into_stdio())
        .stdout(stdout.into_stdio())
        .stderr(stderr.into_stdio());

    if setsid || ctty {
        #[expect(
            clippy::multiple_unsafe_ops_per_block,
            reason = "pre_exec + setsid + ioctl must share one unsafe context"
        )]
        // SAFETY: pre_exec runs between fork and exec; only async-signal-safe
        // syscalls (setsid, ioctl) are invoked.
        unsafe {
            cmd.pre_exec(move || {
                if setsid && libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if ctty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;

    let (stdin_pump, stdin_drain) = match child.stdin.take() {
        Some(s) => {
            let p = StdinPump {
                inner: Arc::new(Mutex::new(Some(s))),
            };
            (Some(p), None)
        }
        None => (None, None),
    };

    let (stdout_reader, stdout_drain) = match child.stdout.take() {
        Some(s) => {
            let (rx, jh) = spawn_drain_thread(s);
            (Some(ChunkReader { rx }), Some(jh))
        }
        None => (None, None),
    };

    let (stderr_reader, stderr_drain) = match child.stderr.take() {
        Some(s) => {
            let (rx, jh) = spawn_drain_thread(s);
            (Some(ChunkReader { rx }), Some(jh))
        }
        None => (None, None),
    };

    let mut drains = Vec::new();
    drains.extend(stdin_drain);
    drains.extend(stdout_drain);
    drains.extend(stderr_drain);

    let track_guard = process_supervisor::register_child(pid);
    // Register with the kqueue watcher *before* returning the Handle. This
    // guarantees `waitpid` will eventually run on this pid even if the caller
    // drops the Handle without calling `wait*` (e.g. an error path between
    // spawn and wait in pluginexec). Without this registration, Drop's
    // `kill_child` would SIGKILL the pid but nobody would reap it → permanent
    // zombie observed under PPID of main heph.
    let exit_rx = process_watcher::register(pid);

    Ok(Handle {
        pid,
        child: Some(child),
        stdin: stdin_pump,
        stdout: stdout_reader,
        stderr: stderr_reader,
        drains,
        reaped: false,
        exit_rx: Some(exit_rx),
        _track_guard: track_guard,
    })
}

pub(super) async fn output(
    spec: Spec,
    cancel: &(dyn Cancellable + Send + Sync),
) -> io::Result<Output> {
    let mut handle = spawn(spec)?;
    let stdout_rx = handle.take_stdout();
    let stderr_rx = handle.take_stderr();
    // Collect stdout/stderr on dedicated buffer threads. We can't await the
    // `ChunkReader::recv` here because we also want to race the wait/cancel
    // — and `ChunkReader::recv` requires block_in_place. Collect on threads
    // so the wait poll loop has a clean single-shot signal to wait on.
    let stdout_handle = collect_chunks(stdout_rx);
    let stderr_handle = collect_chunks(stderr_rx);

    let status = handle.wait_or_cancel(cancel).await?;

    let stdout = block_or_inline(|| {
        stdout_handle.join().map_err(|panic| {
            io::Error::other(format!("stdout collect thread panicked: {panic:?}"))
        })?
    })?;
    let stderr = block_or_inline(|| {
        stderr_handle.join().map_err(|panic| {
            io::Error::other(format!("stderr collect thread panicked: {panic:?}"))
        })?
    })?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn spawn_drain_thread<R: io::Read + Send + 'static>(
    mut src: R,
) -> (mpsc::Receiver<io::Result<Vec<u8>>>, DrainHandle) {
    let (tx, rx) = mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let done_for_thread = Arc::clone(&done);
    let jh = std::thread::Builder::new()
        .name("heph-proc-drain".into())
        .spawn(move || {
            let mut buf = vec![0u8; CHUNK_SIZE];
            // Single exit point so `done` is always flipped to true on
            // any return path (EOF, send-error, read-error). The
            // bounded-join in `drain_with_deadline` relies on this flag.
            loop {
                let n = match src.read(&mut buf) {
                    Ok(0) => break, // EOF: drop tx → receiver sees Disconnected
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        drop(tx.send(Err(e)));
                        break;
                    }
                };
                #[expect(
                    clippy::indexing_slicing,
                    reason = "n <= buf.len() by Read::read contract"
                )]
                let slice = buf[..n].to_vec();
                if tx.send(Ok(slice)).is_err() {
                    break; // receiver dropped: stop reading
                }
            }
            done_for_thread.store(true, Ordering::Release);
        })
        .expect("spawn heph-proc-drain thread");
    (rx, DrainHandle { join: jh, done })
}

fn collect_chunks(reader: Option<ChunkReader>) -> JoinHandle<io::Result<Vec<u8>>> {
    std::thread::Builder::new()
        .name("heph-proc-collect".into())
        .spawn(move || -> io::Result<Vec<u8>> {
            let Some(reader) = reader else {
                return Ok(Vec::new());
            };
            // ChunkReader holds an mpsc::Receiver — drain it synchronously here
            // since this thread is std (not in the tokio runtime).
            let mut out = Vec::new();
            loop {
                match reader.rx.recv() {
                    Ok(Ok(chunk)) => out.extend_from_slice(&chunk),
                    Ok(Err(e)) => return Err(e),
                    Err(_disconnected) => return Ok(out),
                }
            }
        })
        .expect("spawn heph-proc-collect thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::Instant;

    /// Regression: a child that backgrounds a long-lived descendant
    /// inheriting stdout/stderr must not park `Handle::wait` indefinitely.
    /// The bounded drain join + `killpg` escalation reaps the descendant
    /// once `setsid: true` puts the child in its own process group.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stray_daemon_does_not_park_wait() {
        // sh forks a 10s sleep into the background (inheriting stdout +
        // stderr fds), then the immediate child exits. Without
        // `setsid: true` + the bounded drain-join, the sleep would keep
        // the pipe write end open and `Handle::wait` would block on the
        // unbounded drain `join()` for the full 10 seconds.
        let spec = super::super::Spec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from("( /bin/sleep 10 ) & exit 0"),
            ],
            env: Vec::new(),
            cwd: PathBuf::from("/"),
            stdin: super::super::StdioSpec::Null,
            stdout: super::super::StdioSpec::Piped,
            stderr: super::super::StdioSpec::Piped,
            setsid: true,
            ctty: false,
        };

        let handle = spawn(spec).expect("spawn child");
        let started = Instant::now();
        let status = handle.wait().await.expect("wait should return");
        let elapsed = started.elapsed();

        assert!(status.success(), "child should exit 0; got {status:?}");
        // Generous cap: two drain budgets (≈1s) + scheduling slack. The
        // backgrounded sleep would extend this to ~10s without the
        // killpg escalation.
        assert!(
            elapsed < Duration::from_secs(3),
            "Handle::wait took {elapsed:?} — bounded drain join should escape via killpg",
        );
    }
}
