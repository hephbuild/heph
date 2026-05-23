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
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::{CHUNK_SIZE, Spec};

/// Granularity for `recv_timeout` polls during `wait_or_cancel`. 100ms keeps
/// CPU idle while still giving sub-second cancel response. Independent of
/// the watcher's own 1s backstop poll.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    drains: Vec<JoinHandle<()>>,
    reaped: bool,
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
        let mut rx = process_watcher::register(self.pid);
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
        // Drain threads must finish — they have copies of the pipe fds; joining
        // ensures EOF was reached so no chunks are lost.
        block_or_inline(|| {
            for h in self.drains.drain(..) {
                drop(h.join());
            }
        });
        Ok(status)
    }

    /// Wait for exit, but cancel by `SIGKILL`-ing the child if `cancel`
    /// fires. Still consumes the final exit status before returning so the
    /// pid is reaped (no zombies).
    pub async fn wait_or_cancel(
        mut self,
        cancel: &(dyn Cancellable + Send + Sync),
    ) -> io::Result<ExitStatus> {
        let rx = process_watcher::register(self.pid);
        let pid = self.pid;
        drop(self.child.take());
        // Multi-thread: block_in_place + recv_timeout poll; no tokio waker.
        // Current-thread (tests): async try_recv + tokio::time::sleep poll
        // so sibling tasks can make progress on the single thread.
        let status = if is_multi_thread() {
            let cancel_ref = cancel;
            tokio::task::block_in_place(move || -> io::Result<ExitStatus> {
                let mut killed = false;
                loop {
                    match rx.recv_timeout(CANCEL_POLL_INTERVAL) {
                        Ok(Ok(s)) => return Ok(s),
                        Ok(Err(e)) => return Err(e),
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if !killed && cancel_ref.is_cancelled() {
                                process_supervisor::kill_child(pid);
                                killed = true;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return Err(io::Error::other("watcher dropped sender"));
                        }
                    }
                }
            })?
        } else {
            let mut killed = false;
            loop {
                match rx.try_recv() {
                    Ok(Ok(s)) => break s,
                    Ok(Err(e)) => return Err(e),
                    Err(mpsc::TryRecvError::Empty) => {
                        if !killed && cancel.is_cancelled() {
                            process_supervisor::kill_child(pid);
                            killed = true;
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
        block_or_inline(|| {
            for h in self.drains.drain(..) {
                drop(h.join());
            }
        });
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
        // Killed-without-wait path: SIGKILL the child and let the watcher's
        // backstop poll reap it. We can't synchronously block here without
        // risking a deadlock on a runtime worker, so this is best-effort.
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

    Ok(Handle {
        pid,
        child: Some(child),
        stdin: stdin_pump,
        stdout: stdout_reader,
        stderr: stderr_reader,
        drains,
        reaped: false,
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
) -> (mpsc::Receiver<io::Result<Vec<u8>>>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let jh = std::thread::Builder::new()
        .name("rheph-proc-drain".into())
        .spawn(move || {
            let mut buf = vec![0u8; CHUNK_SIZE];
            loop {
                let n = match src.read(&mut buf) {
                    Ok(0) => return, // EOF: drop tx → receiver sees Disconnected
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        drop(tx.send(Err(e)));
                        return;
                    }
                };
                #[expect(
                    clippy::indexing_slicing,
                    reason = "n <= buf.len() by Read::read contract"
                )]
                let slice = buf[..n].to_vec();
                if tx.send(Ok(slice)).is_err() {
                    return; // receiver dropped: stop reading
                }
            }
        })
        .expect("spawn rheph-proc-drain thread");
    (rx, jh)
}

fn collect_chunks(reader: Option<ChunkReader>) -> JoinHandle<io::Result<Vec<u8>>> {
    std::thread::Builder::new()
        .name("rheph-proc-collect".into())
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
        .expect("spawn rheph-proc-collect thread")
}
