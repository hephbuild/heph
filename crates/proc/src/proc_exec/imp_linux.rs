//! Linux subprocess pipeline: vanilla `tokio::process`.
//!
//! Linux uses `epoll` + `pidfd` and is unaffected by the macOS
//! `EVFILT_USER` wake reliability bug documented in `RCA_MACOS_WAKER.md`.
//! No workarounds — plain `tokio::process::Command` + `.wait().await`.

use crate::process_supervisor;
use hcore::hasync::Cancellable;
use std::io;
use std::process::{ExitStatus, Output};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin, Command};

use super::{CHUNK_SIZE, Spec};

pub struct ChunkReader {
    src: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    buf: Vec<u8>,
}

impl ChunkReader {
    pub async fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.buf.is_empty() {
            self.buf.resize(CHUNK_SIZE, 0);
        }
        let n = self.src.read(&mut self.buf).await?;
        if n == 0 {
            return Ok(None);
        }
        #[expect(
            clippy::indexing_slicing,
            reason = "n <= buf.len() by AsyncRead::read contract"
        )]
        let chunk = self.buf[..n].to_vec();
        Ok(Some(chunk))
    }
}

pub struct StdinPump {
    inner: Option<ChildStdin>,
}

impl StdinPump {
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self.inner.as_mut() {
            Some(w) => w.write_all(data).await,
            None => Err(io::Error::other("stdin pump closed")),
        }
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        if let Some(mut w) = self.inner.take() {
            w.shutdown().await
        } else {
            Ok(())
        }
    }
}

pub struct Handle {
    pid: i32,
    child: Child,
    stdin: Option<StdinPump>,
    stdout: Option<ChunkReader>,
    stderr: Option<ChunkReader>,
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

    pub async fn wait(mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    pub async fn wait_or_cancel(
        mut self,
        cancel: &(dyn Cancellable + Send + Sync),
    ) -> io::Result<ExitStatus> {
        tokio::select! {
            res = self.child.wait() => res,
            _ = cancel.cancelled() => {
                // Graceful: SIGINT the child (and its pgid) first, then give
                // it a grace window to exit before escalating to SIGKILL.
                process_supervisor::interrupt_child(self.pid);
                // The grace deadline is a blocking-pool `thread::sleep`, NOT
                // `tokio::time` — `child.wait()` rides the pidfd/epoll IO
                // driver, so nothing here touches the time driver. A Ctrl-C
                // that races runtime teardown therefore can't poll a timer on a
                // shutting-down runtime (the "context found, but it is being
                // shutdown" panic).
                let grace = tokio::task::spawn_blocking(|| {
                    std::thread::sleep(super::CANCEL_GRACE);
                });
                tokio::select! {
                    res = self.child.wait() => drop(res),
                    _ = grace => {
                        process_supervisor::kill_child(self.pid);
                        drop(self.child.wait().await);
                    }
                }
                Err(io::Error::other("cancelled"))
            }
        }
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
        .kill_on_drop(true)
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
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("spawned child has no pid"))? as i32;

    let stdin_pump = child.stdin.take().map(|s| StdinPump { inner: Some(s) });
    let stdout_reader = child.stdout.take().map(make_reader);
    let stderr_reader = child.stderr.take().map(make_reader);

    let track_guard = process_supervisor::register_child(pid);

    Ok(Handle {
        pid,
        child,
        stdin: stdin_pump,
        stdout: stdout_reader,
        stderr: stderr_reader,
        _track_guard: track_guard,
    })
}

fn make_reader<R: tokio::io::AsyncRead + Send + Unpin + 'static>(s: R) -> ChunkReader {
    ChunkReader {
        src: Box::new(s),
        buf: Vec::new(),
    }
}

/// Read `reader` to EOF, appending into `out` and recording the outcome in
/// `res`.
///
/// Both are borrowed rather than owned so that abandoning this future on the
/// post-exit deadline still leaves every byte read so far — and any error a
/// stream hit before the other one stalled — in the caller's hands.
///
/// This deliberately stays on `ChunkReader::recv` rather than calling
/// `AsyncReadExt::read_to_end` directly. It costs one 8 KiB allocation and
/// one extra copy per chunk, and buys two things: `output` reads through the
/// exact same path as the streaming consumer (`pluginexec`), and
/// partial-data-on-drop is a property of this loop rather than an internal
/// detail of tokio's `read_to_end` that a version bump could change under a
/// correctness argument that depends on it.
async fn read_into(
    reader: Option<&mut ChunkReader>,
    out: &mut Vec<u8>,
    res: &mut Option<io::Result<()>>,
) {
    *res = Some(
        async {
            let Some(r) = reader else { return Ok(()) };
            while let Some(chunk) = r.recv().await? {
                out.extend_from_slice(&chunk);
            }
            Ok(())
        }
        .await,
    );
}

pub(super) async fn output(
    spec: Spec,
    cancel: &(dyn Cancellable + Send + Sync),
) -> io::Result<Output> {
    let mut handle = spawn(spec)?;
    let pid = handle.pid();
    let mut stdout_reader = handle.take_stdout();
    let mut stderr_reader = handle.take_stderr();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    // Per-stream, so a stream that finished still reports its error even when
    // the other one is stalled behind a stray descendant. A single combined
    // result would only exist once *both* halves completed, which is exactly
    // the case abandonment rules out.
    let mut stdout_res: Option<io::Result<()>> = None;
    let mut stderr_res: Option<io::Result<()>> = None;

    let (status, abandoned) = {
        // Both pipes must be drained *while* the child runs: the kernel pipe
        // buffer is 64 KiB, and a child that outruns it blocks in `write`
        // forever if nobody is reading.
        let readers = async {
            tokio::join!(
                read_into(stdout_reader.as_mut(), &mut stdout, &mut stdout_res),
                read_into(stderr_reader.as_mut(), &mut stderr, &mut stderr_res),
            );
        };
        tokio::pin!(readers);
        let mut all_drained = false;

        let status = {
            let wait = handle.wait_or_cancel(cancel);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    // The readers reaching EOF does not end the wait, and the
                    // wait ending does not (yet) end the readers — whichever
                    // lands first, we keep polling the other. The guard is
                    // load-bearing: polling `readers` again after it resolved
                    // panics with "`async fn` resumed after completion".
                    res = &mut wait => break res?,
                    () = &mut readers, if !all_drained => all_drained = true,
                }
            }
        };

        // The child is reaped. Anything it wrote is already read or sitting
        // in the pipe, so EOF is moments away — unless a descendant it
        // double-forked inherited the write end, in which case it never
        // comes. Bound the wait and abandon the readers if it does not.
        //
        // The timer is armed only on the success path. A cancelled wait
        // returns above, which keeps `tokio::time` off the runtime-teardown
        // path for the same reason `wait_or_cancel` sleeps on the blocking
        // pool rather than the time driver.
        let abandoned = if all_drained {
            false
        } else {
            // `Timeout` polls the inner future before it polls the delay, and
            // `read_into` loops until `recv` is `Pending`. So the poll that
            // precedes `Elapsed` has already drained the pipe to `EAGAIN`:
            // bytes lost here can only be bytes written *after* the child was
            // reaped and the pipe ran dry — i.e. a descendant's, never the
            // child's.
            tokio::time::timeout(super::DRAIN_DEADLINE, &mut readers)
                .await
                .is_err()
        };

        (status, abandoned)
        // The borrows end here. The read ends themselves close a few
        // statements later when `stdout_reader` / `stderr_reader` drop at
        // function exit, at which point a descendant still holding the write
        // end gets EPIPE on its next write.
    };

    if abandoned {
        tracing::warn!(
            pid,
            stdout_len = stdout.len(),
            stderr_len = stderr.len(),
            stdout_open = stdout_res.is_none(),
            stderr_open = stderr_res.is_none(),
            "proc_exec: pipe still open after child exit; abandoning the drain \
             (a surviving descendant holds the write end)"
        );
    }

    // Surface a genuine read error from either stream that ran to completion.
    // An abandoned stream has no verdict to report; it is covered by the
    // warning above rather than by failing the child.
    if let Some(res) = stdout_res {
        res?;
    }
    if let Some(res) = stderr_res {
        res?;
    }

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
