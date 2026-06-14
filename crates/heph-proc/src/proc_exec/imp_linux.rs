//! Linux subprocess pipeline: vanilla `tokio::process`.
//!
//! Linux uses `epoll` + `pidfd` and is unaffected by the macOS
//! `EVFILT_USER` wake reliability bug documented in `RCA_MACOS_WAKER.md`.
//! No workarounds — plain `tokio::process::Command` + `.wait().await`.

use crate::process_supervisor;
use heph_core::hasync::Cancellable;
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

pub(super) async fn output(
    spec: Spec,
    cancel: &(dyn Cancellable + Send + Sync),
) -> io::Result<Output> {
    let mut handle = spawn(spec)?;
    let mut stdout_reader = handle.take_stdout();
    let mut stderr_reader = handle.take_stderr();

    let stdout_fut = async move {
        let mut out = Vec::new();
        if let Some(r) = stdout_reader.as_mut() {
            while let Some(chunk) = r.recv().await? {
                out.extend_from_slice(&chunk);
            }
        }
        Ok::<Vec<u8>, io::Error>(out)
    };
    let stderr_fut = async move {
        let mut out = Vec::new();
        if let Some(r) = stderr_reader.as_mut() {
            while let Some(chunk) = r.recv().await? {
                out.extend_from_slice(&chunk);
            }
        }
        Ok::<Vec<u8>, io::Error>(out)
    };

    let (status_res, stdout_res, stderr_res) =
        tokio::join!(handle.wait_or_cancel(cancel), stdout_fut, stderr_fut);
    let status = status_res?;
    let stdout = stdout_res?;
    let stderr = stderr_res?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
