//! Linux subprocess pipeline: vanilla `tokio::process`.
//!
//! Linux uses `epoll` + `pidfd` and is unaffected by the macOS `EVFILT_USER`
//! wake reliability bug described in the [`super`] module docs. No
//! workarounds — plain `tokio::process::Command` + `.wait().await`.

use crate::process_supervisor;
use hcore::hasync::Cancellable;
use std::io;
use std::process::{ExitStatus, Output};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin, Command};

use super::{CHUNK_SIZE, Spec, StreamId};

/// Single-stream reader. Internal: the streaming consumer takes an
/// [`OutputReader`], which carries both streams. `output` keeps using the two
/// halves separately because it drives them under one `join!` and wants a
/// per-stream verdict.
struct ChunkReader {
    src: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    buf: Vec<u8>,
}

impl ChunkReader {
    async fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
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

/// Async reader over **both** of the child's output streams.
///
/// Same *delivery* contract as the macOS type of the same name, reached
/// differently: here each stream is a genuine `AsyncRead` over its pipe, so a
/// `select!` over the two is all the merge needs. There is no intermediate
/// buffer at all — a consumer that stalls simply stops reading and the 64 KiB
/// kernel pipe backpressures the child, which is the behaviour the macOS
/// backend's bounded drain channel reproduces.
///
/// **Termination differs, and callers that care must know it.** This `recv`
/// stays `Pending` when a stray descendant holds the pipe write end open, so
/// it never ends on its own — the caller's `timeout`/`select!` ends it, which
/// is what `pluginexec`'s post-wait drain does. The macOS reader cannot be
/// dropped mid-park (`block_in_place` is never `Pending`) and so self-
/// terminates on an `abandoned` flag instead. Both give "the tee always
/// ends"; neither mechanism is available on the other platform.
pub struct OutputReader {
    stdout: Option<ChunkReader>,
    stderr: Option<ChunkReader>,
}

impl OutputReader {
    /// Wait for the next chunk from either stream. Returns `Ok(None)` once
    /// **both** streams have hit EOF.
    ///
    /// Cancel-safe: each arm bottoms out in `AsyncReadExt::read`, which loses
    /// nothing when its branch is dropped, so an abandoned `recv` can be
    /// retried without a gap in the stream.
    pub async fn recv(&mut self) -> io::Result<Option<(StreamId, Vec<u8>)>> {
        loop {
            let (id, res) = match (self.stdout.as_mut(), self.stderr.as_mut()) {
                (None, None) => return Ok(None),
                (Some(out), None) => (StreamId::Stdout, out.recv().await),
                (None, Some(err)) => (StreamId::Stderr, err.recv().await),
                (Some(out), Some(err)) => tokio::select! {
                    r = out.recv() => (StreamId::Stdout, r),
                    r = err.recv() => (StreamId::Stderr, r),
                },
            };
            match res {
                Ok(Some(chunk)) => return Ok(Some((id, chunk))),
                // This stream is done; keep serving the other one.
                Ok(None) => self.close(id),
                Err(e) => {
                    self.close(id);
                    return Err(e);
                }
            }
        }
    }

    fn close(&mut self, id: StreamId) {
        match id {
            StreamId::Stdout => self.stdout = None,
            StreamId::Stderr => self.stderr = None,
        }
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

/// Live child handle.
///
/// # Invariant: never poll a wait on the same task as [`OutputReader::recv`]
///
/// Harmless here — both are ordinary futures — but the macOS backend
/// deadlocks on it, and this is the same public API. See the macOS `Handle`
/// docs for the mechanism.
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

    fn take_stdout(&mut self) -> Option<ChunkReader> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<ChunkReader> {
        self.stderr.take()
    }

    /// Take the merged stdout+stderr reader. `None` when neither stream was
    /// piped.
    pub fn take_output(&mut self) -> Option<OutputReader> {
        let stdout = self.stdout.take();
        let stderr = self.stderr.take();
        if stdout.is_none() && stderr.is_none() {
            return None;
        }
        Some(OutputReader { stdout, stderr })
    }

    pub(super) async fn wait_or_cancel(
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
