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

use hdriver_support::run_log::{RunLog, Streams};
use hplugin::driver::RunRequest;
use hplugin::error::ProcessFailed;
use hproc::proc_exec::StreamId;
use std::path::PathBuf;
use std::process::Output;

/// The log name an older host's failure appends to.
const LOG_NAME: &str = "log.txt";

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
