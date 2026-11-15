//! A plugin's side of a run's log: where the output of the processes it runs
//! goes, and which of that output is log at all.
//!
//! Two hosts, one call site. On a host that sets `RunRequest::log_capture`, the
//! request's `stdout`/`stderr` writers **are** the run's log: the engine scrubs
//! them, appends them to its live file and promotes that file to the cache entry
//! or the failure slot, so the plugin writes there and nowhere else. On an older
//! host the plugin still keeps its own log file in the sandbox and returns its
//! path, scrubbed here with the same [`crate::redact`] scrubber the engine uses,
//! while the writers stay whatever live sinks that host handed over.
//!
//! A process's stdout is sometimes an output, not a log — `docker image inspect`
//! prints a digest, `go list -json` prints the package graph. [`Streams`] names
//! which streams are log; the others are left to the caller, never written to
//! either destination. A log path or handle never reaches a hash, the sandbox or
//! a target's env, argv or files: the only file this module names is the
//! plugin's own log, and only when the host asked for one.

use crate::redact::{Needles, Redactor};
use anyhow::Context as _;
use hcore::hasync::Cancellable;
use hplugin::driver::RunRequest;
use hplugin::driver::outputartifact::OutputArtifact;
use hproc::proc_exec::{OutputReader, StreamId};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;

/// The writer type `RunRequest` carries for `stdout` / `stderr`.
pub type Writer<'io> = &'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin);

/// Which of a child's streams are the run's log.
///
/// A stream left out is the process's data: [`RunLog::pump`] hands it to the
/// caller's observer and writes it nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Streams {
    pub stdout: bool,
    pub stderr: bool,
}

impl Streams {
    /// Both streams are log: a build tool's output.
    pub const BOTH: Self = Self {
        stdout: true,
        stderr: true,
    };
    /// Only stderr is log; stdout is the process's answer.
    pub const STDERR: Self = Self {
        stdout: false,
        stderr: true,
    };

    fn includes(self, stream: StreamId) -> bool {
        match stream {
            StreamId::Stdout => self.stdout,
            StreamId::Stderr => self.stderr,
        }
    }
}

/// The plugin's own log file, kept only when the host did not set
/// `log_capture`.
struct OwnLog {
    /// `None` once a write to it failed, or when it could not be opened: losing
    /// the log must not fail the build.
    file: Option<std::fs::File>,
    path: PathBuf,
    /// The run's credential secrets; `None` when it has none.
    needles: Option<Arc<Needles>>,
}

/// Where a run's log goes. Built once per run with [`RunLog::from_request`] and
/// shared by every process the run spawns, so a multi-command run writes one
/// log in order.
pub struct RunLog<'io> {
    stdout: Option<Writer<'io>>,
    stderr: Option<Writer<'io>>,
    own: Option<OwnLog>,
    /// Whether a writer has refused output, per stream. Warned once, at the
    /// first refusal: a log that lost its tail with no note reads as a run that
    /// printed nothing.
    refused_out: bool,
    refused_err: bool,
}

impl std::fmt::Debug for RunLog<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLog")
            .field("captured", &self.is_captured())
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .field("own", &self.own.as_ref().map(|o| &o.path))
            .finish()
    }
}

impl<'io> RunLog<'io> {
    /// Take the writers out of a run request — they are single-owner, so this
    /// consumes them for the run — and, on a host that did not set
    /// `log_capture`, open `<sandbox_dir>/<file_name>` for appending.
    pub fn from_request(req: &mut RunRequest<'_, 'io>, file_name: &str) -> Self {
        let (stdout, stderr) = (req.stdout.take(), req.stderr.take());
        if req.log_capture {
            return Self::captured(stdout, stderr);
        }
        let needles = Needles::new(req.credentials.iter().flat_map(|m| m.redact.iter()));
        Self::own_file(stdout, stderr, req.sandbox_dir.join(file_name), needles)
    }

    /// The writers are the log (`log_capture`): the engine scrubs and keeps
    /// them, so nothing is scrubbed or written here.
    pub fn captured(stdout: Option<Writer<'io>>, stderr: Option<Writer<'io>>) -> Self {
        Self {
            stdout,
            stderr,
            own: None,
            refused_out: false,
            refused_err: false,
        }
    }

    /// The plugin keeps its own log at `path` (appended to, so one run's several
    /// processes share it), scrubbed of `needles`; the writers are live sinks
    /// and get the same scrubbed bytes.
    pub fn own_file(
        stdout: Option<Writer<'io>>,
        stderr: Option<Writer<'io>>,
        path: PathBuf,
        needles: Option<Arc<Needles>>,
    ) -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                tracing::warn!(error = %e, path = %path.display(), "could not open the run's log");
            })
            .ok();
        Self {
            stdout,
            stderr,
            own: Some(OwnLog {
                file,
                path,
                needles,
            }),
            refused_out: false,
            refused_err: false,
        }
    }

    /// Whether the host's writers are the log (`log_capture`).
    pub fn is_captured(&self) -> bool {
        self.own.is_none()
    }

    /// The plugin's own log as a `Log` artifact, for a run to return (on
    /// failure, through `hplugin::error::with_failure_artifacts`). `None` under
    /// `log_capture` — the engine already holds the log, and no path crosses the
    /// plugin boundary — and when the file could not be opened.
    pub fn log_artifact(&self) -> Option<OutputArtifact> {
        let own = self.own.as_ref()?;
        own.file.as_ref()?;
        hplugin::error::log_file_artifact(&own.path)
    }

    /// Write one stream's whole output, for a process whose output is only
    /// available once it exits (`hexecrunner::output`). Call it only for a
    /// stream that is log.
    pub async fn write(&mut self, stream: StreamId, bytes: &[u8]) {
        let mut redactor = self.redactor();
        let scrubbed = redactor.as_mut().map(|r| {
            let mut out = r.push(bytes);
            out.extend_from_slice(&r.finish());
            out
        });
        self.emit(stream, scrubbed.as_deref().unwrap_or(bytes))
            .await;
    }

    /// Copy a running child's output to the log as it arrives, in arrival order.
    ///
    /// Only the streams in `streams` are written; every chunk of both streams,
    /// log or data, is first passed to `observe` unchanged (to keep a tail for an
    /// error message, or to collect the data stream). Returns when both streams
    /// have ended. A read error on one stream is warned and the other is still
    /// read: stopping would truncate the log and SIGPIPE the child.
    ///
    /// The wait for the child must run on another task (`Handle::spawn_wait`),
    /// joined with this future, or a child that fills a pipe never exits.
    pub async fn pump(
        &mut self,
        reader: Option<OutputReader>,
        streams: Streams,
        mut observe: impl FnMut(StreamId, &[u8]),
    ) {
        let Some(mut reader) = reader else { return };
        // One redactor per stream, never shared: the held-back tail belongs to
        // one byte sequence, and two streams through one buffer would splice.
        let (mut red_out, mut red_err) = (self.redactor(), self.redactor());
        loop {
            let (stream, chunk) = match reader.recv().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "stopped reading one of a child's output streams");
                    continue;
                }
            };
            observe(stream, &chunk);
            if !streams.includes(stream) {
                continue;
            }
            let redactor = match stream {
                StreamId::Stdout => red_out.as_mut(),
                StreamId::Stderr => red_err.as_mut(),
            };
            let scrubbed = redactor.map(|r| r.push(&chunk));
            self.emit(stream, scrubbed.as_deref().unwrap_or(&chunk))
                .await;
        }
        // The held-back tails, or the last bytes of the output disappear.
        for (stream, red) in [(StreamId::Stdout, red_out), (StreamId::Stderr, red_err)] {
            if let Some(mut r) = red {
                let tail = r.finish();
                self.emit(stream, &tail).await;
            }
        }
    }

    /// `hexecrunner::output`, with the `streams` that are log written as they
    /// arrive: spawn `spec` under `runner`, [`pump`](Self::pump) it while it
    /// runs, and return its status and both streams whole, as `output` would.
    ///
    /// For a tool whose stdout is its answer (a store path, an environment) and
    /// whose stderr is progress worth watching while it runs.
    pub async fn output(
        &mut self,
        runner: hexecrunner::RunnerRef<'_>,
        spec: hproc::proc_exec::Spec,
        streams: Streams,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<std::process::Output> {
        let (spawned, _) = hexecrunner::spawn_io(runner, spec, ctoken).await?;
        // The spawn's `io::Error` stays in the chain, as `output` keeps it.
        let mut handle = spawned?;
        let reader = handle.take_output();
        // The wait runs on its own task: it parks until the child exits, and a
        // reader sharing its task would stop draining a pipe the child fills.
        let wait = handle.spawn_wait(ctoken.clone_arc());
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let pumped = self.pump(reader, streams, |stream, chunk| match stream {
            StreamId::Stdout => stdout.extend_from_slice(chunk),
            StreamId::Stderr => stderr.extend_from_slice(chunk),
        });
        let (status, ()) = tokio::join!(wait, pumped);
        let status = status.context("wait task for the child")??;
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    }

    /// A fresh scrubber for one stream, on the own-file path only: under
    /// `log_capture` the engine scrubs, and scrubbing twice would only cost.
    fn redactor(&self) -> Option<Redactor> {
        let needles = self.own.as_ref()?.needles.clone()?;
        Some(Redactor::new(needles))
    }

    /// Append already-scrubbed bytes to the own file (if any), then the writer.
    async fn emit(&mut self, stream: StreamId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(own) = self.own.as_mut()
            && let Some(file) = own.file.as_mut()
            && let Err(e) = std::io::Write::write_all(file, bytes)
        {
            tracing::warn!(error = %e, path = %own.path.display(), "stopped writing the run's log");
            own.file = None;
        }
        let (sink, refused) = match stream {
            StreamId::Stdout => (self.stdout.as_deref_mut(), &mut self.refused_out),
            StreamId::Stderr => (self.stderr.as_deref_mut(), &mut self.refused_err),
        };
        let Some(sink) = sink else { return };
        // Flushed per chunk so a live reader sees it as it is produced.
        if let Err(e) = async {
            sink.write_all(bytes).await?;
            sink.flush().await
        }
        .await
            && !*refused
        {
            *refused = true;
            tracing::warn!(error = %e, %stream, "a run's log writer refused output");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::{Cancellable, StdCancellationToken};
    use hproc::proc_exec;
    use std::ffi::OsString;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    /// A writer whose bytes the test can read while it is still borrowed.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("lock")).into_owned()
        }
    }

    impl tokio::io::AsyncWrite for Shared {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Run `script` under `/bin/sh`, pumping its output into `log`; returns
    /// what `observe` saw of each stream.
    async fn pump_script(log: &mut RunLog<'_>, script: &str, streams: Streams) -> (String, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = proc_exec::Spec {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(script)],
            env: vec![],
            cwd: dir.path().to_path_buf(),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        };
        let ctoken = StdCancellationToken::new();
        let (spawned, _) = hexecrunner::spawn_io(hexecrunner::RunnerRef::local(), spec, &ctoken)
            .await
            .expect("spawn");
        let mut handle = spawned.expect("spawned");
        let reader = handle.take_output();
        let wait = handle.spawn_wait(ctoken.clone_arc());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let pumped = log.pump(reader, streams, |s, c| match s {
            StreamId::Stdout => out.extend_from_slice(c),
            StreamId::Stderr => err.extend_from_slice(c),
        });
        let (status, ()) = tokio::join!(wait, pumped);
        assert!(status.expect("join").expect("wait").success());
        (
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    /// Under `log_capture` the log streams reach the writers while the child is
    /// still running, and a data stream reaches only the observer.
    #[tokio::test]
    async fn pump_streams_log_live_and_keeps_data_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = dir.path().join("gate");
        let (out_w, err_w) = (Shared::default(), Shared::default());
        let (mut o, mut e) = (out_w.clone(), err_w.clone());
        let mut log = RunLog::captured(Some(&mut o), Some(&mut e));
        // The child prints a log line, then blocks until the test has seen it
        // in the writer: a pump that buffered to the end would hang here.
        let script = format!(
            "echo DATA-1; echo progress-1 >&2; while [ ! -e '{g}' ]; do sleep 0.01; done; \
             echo DATA-2; echo progress-2 >&2",
            g = gate.display()
        );
        let watcher = {
            let err_w = err_w.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                while !err_w.text().contains("progress-1") {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                std::fs::write(&gate, b"").expect("open gate");
            })
        };
        let (seen_out, _) = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            pump_script(&mut log, &script, Streams::STDERR),
        )
        .await
        .expect("the log line must reach the writer before the child exits");
        watcher.await.expect("watcher");
        drop(log);
        assert_eq!(err_w.text(), "progress-1\nprogress-2\n");
        assert_eq!(out_w.text(), "", "a data stream never reaches the log");
        assert_eq!(
            seen_out, "DATA-1\nDATA-2\n",
            "the caller still gets its data"
        );
        assert!(!dir.path().join("log.txt").exists());
    }

    /// `output` answers as `hexecrunner::output` does (status, both streams
    /// whole) while only the log stream reaches the writers; a missing program
    /// keeps its `NotFound` in the chain.
    #[tokio::test]
    async fn output_returns_both_streams_and_logs_only_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (out_w, err_w) = (Shared::default(), Shared::default());
        let (mut o, mut e) = (out_w.clone(), err_w.clone());
        let mut log = RunLog::captured(Some(&mut o), Some(&mut e));
        let spec = |program: &str, script: &str| proc_exec::Spec {
            program: PathBuf::from(program),
            args: vec![OsString::from("-c"), OsString::from(script)],
            env: vec![],
            cwd: dir.path().to_path_buf(),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        };
        let ctoken = StdCancellationToken::new();
        let out = log
            .output(
                hexecrunner::RunnerRef::local(),
                spec("/bin/sh", "echo /nix/store/abc; echo building >&2; exit 3"),
                Streams::STDERR,
                &ctoken,
            )
            .await
            .expect("output");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"/nix/store/abc\n");
        assert_eq!(out.stderr, b"building\n");

        let missing = log
            .output(
                hexecrunner::RunnerRef::local(),
                spec("/nonexistent/heph-no-such-tool", ""),
                Streams::STDERR,
                &ctoken,
            )
            .await
            .expect_err("a missing program fails the spawn");
        assert!(
            missing
                .chain()
                .filter_map(|c| c.downcast_ref::<std::io::Error>())
                .any(|e| e.kind() == std::io::ErrorKind::NotFound),
            "{missing:#}"
        );
        drop(log);
        assert_eq!(err_w.text(), "building\n");
        assert_eq!(out_w.text(), "", "a data stream never reaches the log");
    }

    /// Both log streams land in one log in the order they arrived, and the
    /// captured path writes no file of its own.
    #[tokio::test]
    async fn pump_keeps_arrival_order_across_streams() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.txt");
        let mut log = RunLog::own_file(None, None, path.clone(), None);
        // Each line waits for the previous to be written, so arrival order is
        // the script's order whatever the scheduler does.
        pump_script(
            &mut log,
            "echo a; sleep 0.05; echo b >&2; sleep 0.05; echo c",
            Streams::BOTH,
        )
        .await;
        assert_eq!(std::fs::read_to_string(&path).expect("log"), "a\nb\nc\n");
        assert!(log.log_artifact().is_some());

        let mut captured = RunLog::captured(None, None);
        pump_script(&mut captured, "echo x", Streams::BOTH).await;
        assert!(
            captured.log_artifact().is_none(),
            "no path crosses under log_capture"
        );
    }

    /// Without `log_capture` the plugin's own file and its live sinks are
    /// scrubbed by the shared scrubber, including a secret split across writes.
    #[tokio::test]
    async fn own_file_path_is_scrubbed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.txt");
        let secret = "s3cr3t-token-value-0123456789";
        let sink = Shared::default();
        let mut s = sink.clone();
        let needles = Needles::new([secret]);
        let mut log = RunLog::own_file(None, Some(&mut s), path.clone(), needles);
        let (head, tail) = secret.split_at(10);
        pump_script(
            &mut log,
            &format!("printf 'a {head}' >&2; sleep 0.05; printf '{tail} b' >&2"),
            Streams::BOTH,
        )
        .await;
        log.write(StreamId::Stderr, format!(" {secret}\n").as_bytes())
            .await;
        drop(log);
        let file = std::fs::read_to_string(&path).expect("log");
        assert_eq!(file, "a [redacted] b [redacted]\n");
        assert_eq!(sink.text(), file);
    }

    /// Under `log_capture` the plugin does not scrub: the engine's writers do,
    /// and the bytes reach them as written.
    #[tokio::test]
    async fn captured_write_reaches_the_writer() {
        let sink = Shared::default();
        let mut s = sink.clone();
        let mut log = RunLog::captured(None, Some(&mut s));
        log.write(StreamId::Stderr, b"compile error\n").await;
        log.write(StreamId::Stdout, b"dropped: no stdout writer\n")
            .await;
        drop(log);
        assert_eq!(sink.text(), "compile error\n");
    }
}
