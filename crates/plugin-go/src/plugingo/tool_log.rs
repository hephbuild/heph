//! Where the output of the tools a Go run spawns goes.
//!
//! Every Go tool runs through `hexecrunner::output`, so its output is only
//! available once it exits and is written once, then. Two hosts:
//!
//! - **`log_capture`** (a host that keeps the log itself): the request's
//!   writers are the run's log. Each process's *log* streams are written there,
//!   whether it succeeded or failed, and a failure carries no `Log` artifact —
//!   no path crosses the plugin boundary.
//! - **An older host**: as before, nothing is written on success, and a failure
//!   appends both streams to the target's own `log.txt` and returns it
//!   ([`ProcessFailed::from_output`]).
//!
//! A *data* stream (`go list -json`'s package graph, `heph-govet -json`'s
//! report, `go mod download -json`'s record, `-format -check`'s file list) is
//! never written to the log on either host while it is the process's answer.
//! Once the process has failed it has no answer, and what it printed is the only
//! account of why, so [`ToolLog::failed`] writes both streams.
//!
//! A process that runs under the module fetch environment (`go list`, `go mod
//! download`) logs nothing on success, under either host: that environment's
//! values (a `GOPROXY` with userinfo, `GOAUTH`) are not scrubbed, and a
//! success's log is cached and uploaded. A failure is not cached, so it keeps
//! both streams.

use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};
use hdriver_support::run_log::{RunLog, Streams};
use hplugin::driver::RunRequest;
use hplugin::error::ProcessFailed;
use hproc::proc_exec::{self, StreamId};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;

/// The log name an older host's failure appends to.
const LOG_NAME: &str = "log.txt";

/// Which process group a Go tool runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessGroup {
    /// Its own (`setsid`), so a cancel's `killpg` reaches whatever it forks
    /// (`go mod download` forks `git`).
    Own,
    /// The supervisor's. Right for a single-process tool: the supervisor's
    /// direct-pid kill is enough.
    Inherit,
}

/// Run a Go tool through the target's exec `runner`: stdin closed, both
/// streams captured, the whole output returned once it exits. Never fails on a
/// non-zero exit; the caller reads the status, and adds its own context to an
/// error.
pub(crate) async fn run_captured(
    program: &Path,
    args: Vec<OsString>,
    env: &HashMap<String, String>,
    cwd: &Path,
    group: ProcessGroup,
    runner: hexecrunner::RunnerRef<'_>,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<Output> {
    let spec = proc_exec::Spec {
        program: program.to_path_buf(),
        args,
        env: env
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect(),
        cwd: cwd.to_path_buf(),
        stdin: proc_exec::StdioSpec::Null,
        stdout: proc_exec::StdioSpec::Piped,
        stderr: proc_exec::StdioSpec::Piped,
        setsid: group == ProcessGroup::Own,
        ctty: false,
    };
    hexecrunner::output(runner, spec, ctoken).await
}

/// A failed run's output goes to the target's log, whose tail the failure box
/// renders, rather than into the message — and, on a host without
/// `log_capture`, to the live stderr of an interactive run, since it was
/// captured rather than streamed (under `log_capture` the writers already have
/// it, and an error with no `Log` artifact echoes nothing).
pub(crate) async fn echo_failure(
    req: &mut ManagedRunRequest<'_, '_>,
    res: anyhow::Result<()>,
) -> anyhow::Result<ManagedRunResponse> {
    if let Err(e) = &res {
        hdriver_support::driver_managed::echo_failure_log(&mut req.request, e).await;
    }
    res.map(|()| ManagedRunResponse { artifacts: vec![] })
}

/// A Go run's log, for the processes it spawns. Built once per run: under
/// `log_capture` it takes the request's writers.
pub(crate) enum ToolLog<'io> {
    /// The host's writers are the log.
    Captured(RunLog<'io>),
    /// The target's own `log.txt`, written only on failure.
    OwnFile(PathBuf),
}

impl<'io> ToolLog<'io> {
    pub(crate) fn new(req: &mut RunRequest<'_, 'io>) -> Self {
        if req.log_capture {
            Self::Captured(RunLog::from_request(req, LOG_NAME))
        } else {
            Self::OwnFile(req.sandbox_dir.join(LOG_NAME))
        }
    }

    /// A process that produced its answer: its `streams` reach the log under
    /// `log_capture`; an older host keeps no log of a success, as before.
    pub(crate) async fn write(&mut self, output: &Output, streams: Streams) {
        let Self::Captured(log) = self else { return };
        for (id, on, bytes) in [
            (StreamId::Stdout, streams.stdout, &output.stdout),
            (StreamId::Stderr, streams.stderr, &output.stderr),
        ] {
            if on && !bytes.is_empty() {
                log.write(id, bytes).await;
            }
        }
    }

    /// A process that failed: both its streams reach the log, and the error is
    /// its exit — with the own `log.txt` attached on an older host, nothing
    /// under `log_capture`.
    pub(crate) async fn failed(&mut self, output: &Output) -> anyhow::Error {
        match self {
            Self::Captured(_) => {
                self.write(output, Streams::BOTH).await;
                anyhow::Error::new(ProcessFailed::from_status(output.status))
            }
            Self::OwnFile(path) => ProcessFailed::from_output(path, output),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};

    /// An `AsyncWrite` that keeps what it was given, for a test to read back.
    #[derive(Clone, Default)]
    pub(crate) struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Shared {
        pub(crate) fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
        }

        /// A `'static` writer: leaked, which a test can afford.
        pub(crate) fn writer(
            &self,
        ) -> &'static mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin) {
            Box::leak(Box::new(self.clone()))
        }
    }

    impl tokio::io::AsyncWrite for Shared {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
}
