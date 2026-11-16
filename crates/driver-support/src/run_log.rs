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
use hproc::proc_exec::{self, OutputReader, StreamId};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;

/// A child's merged output, one chunk at a time, a read error naming its
/// stream. [`OutputReader`] in a run; a scripted sequence in a test, since a
/// pipe that fails to read or never reaches EOF cannot be had on demand.
trait Chunks {
    async fn next_chunk(&mut self) -> Result<Option<(StreamId, Vec<u8>)>, ReadError>;
}

/// A failed read, and the stream it ended.
type ReadError = (StreamId, std::io::Error);

impl Chunks for OutputReader {
    async fn next_chunk(&mut self) -> Result<Option<(StreamId, Vec<u8>)>, ReadError> {
        self.recv_tagged().await
    }
}

/// One scrubber per stream, never shared: the held-back tail belongs to one
/// byte sequence, and two streams through one buffer would splice. `None` under
/// `log_capture`.
struct Redactors {
    out: Option<Redactor>,
    err: Option<Redactor>,
}

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

    /// Wait for a spawned child while copying its output to the log as it
    /// arrives, in arrival order, and return the wait's result.
    ///
    /// Only the streams in `streams` are written; every chunk of both streams,
    /// log or data, is first passed to `observe` unchanged (to keep a tail for an
    /// error message, or to collect the data stream).
    ///
    /// The drain is bounded the way `proc_exec::output` bounds it, on both OSes:
    /// once the child has exited, the streams get [`proc_exec::DRAIN_DEADLINE`]
    /// more to reach EOF, after which they are abandoned with a warning — a
    /// descendant that inherited the pipe (an ssh `ControlPersist` master, a
    /// shell's background job) would otherwise hold the run open for as long as
    /// it lives. A cancelled wait skips the drain altogether.
    ///
    /// A read error on a log stream is warned and the other stream is still
    /// read (stopping would SIGPIPE the child). One on a data stream fails the
    /// call: the caller's answer would be truncated, and a truncated answer
    /// must never pass for a whole one.
    ///
    /// The outer error is a failed read of a data stream or a lost wait task;
    /// the inner one is the wait's own (a cancellation among them), left for the
    /// caller to describe.
    pub async fn drive(
        &mut self,
        mut handle: proc_exec::Handle,
        streams: Streams,
        ctoken: &(dyn Cancellable + Send + Sync),
        mut observe: impl FnMut(StreamId, &[u8]),
    ) -> anyhow::Result<std::io::Result<ExitStatus>> {
        let reader = handle.take_output();
        // The wait runs on its own task: it parks until the child exits, and a
        // reader sharing its task would stop draining a pipe the child fills.
        let wait = handle.spawn_wait(ctoken.clone_arc());
        match reader {
            Some(mut reader) => {
                self.drive_chunks(&mut reader, wait, streams, ctoken, &mut observe)
                    .await
            }
            None => wait.await.context("wait task for the child"),
        }
    }

    /// [`drive`](Self::drive) over any chunk source and wait, so the drain's
    /// bound and its error handling are testable without a real pipe.
    async fn drive_chunks(
        &mut self,
        chunks: &mut impl Chunks,
        wait: impl Future<Output = Result<std::io::Result<ExitStatus>, tokio::task::JoinError>>,
        streams: Streams,
        ctoken: &(dyn Cancellable + Send + Sync),
        observe: &mut impl FnMut(StreamId, &[u8]),
    ) -> anyhow::Result<std::io::Result<ExitStatus>> {
        let mut reds = Redactors {
            out: self.redactor(),
            err: self.redactor(),
        };
        let mut data_error = None;
        let (status, abandoned) = {
            let pumped = self.pump_chunks(chunks, streams, observe, &mut reds, &mut data_error);
            let mut pumped = std::pin::pin!(pumped);
            let mut wait = std::pin::pin!(wait);
            let mut drained = false;
            let status = tokio::select! {
                status = &mut wait => status,
                () = &mut pumped => {
                    drained = true;
                    wait.await
                }
            };
            // The child is reaped: anything it wrote is read or sitting in the
            // pipe, so EOF is moments away unless a descendant holds the write
            // end. The timer is armed only when the run was not cancelled, as in
            // `proc_exec::output`, which keeps `tokio::time` off a cancelled
            // run's teardown.
            if !drained && !ctoken.is_cancelled() {
                drained = tokio::time::timeout(proc_exec::DRAIN_DEADLINE, &mut pumped)
                    .await
                    .is_ok();
            }
            (status, !drained && !ctoken.is_cancelled())
        };
        if abandoned {
            tracing::warn!(
                deadline = ?proc_exec::DRAIN_DEADLINE,
                "a process exited but its output pipe is still open; abandoning the drain \
                 (a surviving descendant holds the write end)"
            );
        }
        // The held-back tails, after the drain however it ended, or the last
        // bytes of the output disappear.
        self.flush(reds).await;
        let status = status.context("wait task for the child")?;
        if status.is_ok()
            && let Some((stream, e)) = data_error
        {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("reading the child's {stream}, which is its answer"));
        }
        Ok(status)
    }

    /// Copy chunks to the log until both streams end (see [`drive`](Self::drive)
    /// for which are written and what a read error does). Records the first
    /// data-stream read error in `data_error`. Leaves the redactors' held-back
    /// tails for [`flush`](Self::flush), so a drain cut short still writes them.
    async fn pump_chunks(
        &mut self,
        chunks: &mut impl Chunks,
        streams: Streams,
        observe: &mut impl FnMut(StreamId, &[u8]),
        reds: &mut Redactors,
        data_error: &mut Option<ReadError>,
    ) {
        loop {
            let (stream, chunk) = match chunks.next_chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err((stream, e)) if streams.includes(stream) => {
                    tracing::warn!(error = %e, %stream, "stopped reading a child's log stream");
                    continue;
                }
                Err((stream, e)) => {
                    tracing::debug!(error = %e, %stream, "a child's data stream failed to read");
                    data_error.get_or_insert((stream, e));
                    continue;
                }
            };
            observe(stream, &chunk);
            if !streams.includes(stream) {
                continue;
            }
            let redactor = match stream {
                StreamId::Stdout => reds.out.as_mut(),
                StreamId::Stderr => reds.err.as_mut(),
            };
            let scrubbed = redactor.map(|r| r.push(&chunk));
            self.emit(stream, scrubbed.as_deref().unwrap_or(&chunk))
                .await;
        }
    }

    /// Write the redactors' held-back tails.
    async fn flush(&mut self, reds: Redactors) {
        for (stream, red) in [(StreamId::Stdout, reds.out), (StreamId::Stderr, reds.err)] {
            if let Some(mut r) = red {
                let tail = r.finish();
                self.emit(stream, &tail).await;
            }
        }
    }

    /// `hexecrunner::output`, with the `streams` that are log written as they
    /// arrive: spawn `spec` under `runner`, [`drive`](Self::drive) it, and
    /// return its status and both streams whole, as `output` would.
    ///
    /// For a tool whose stdout is its answer (a store path, an environment) and
    /// whose stderr is progress worth watching while it runs.
    pub async fn output(
        &mut self,
        runner: hexecrunner::RunnerRef<'_>,
        spec: proc_exec::Spec,
        streams: Streams,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<std::process::Output> {
        let (spawned, spawned_as) = hexecrunner::spawn_io(runner, spec, ctoken).await?;
        // The spawn's `io::Error` stays in the chain, as `output` keeps it, and
        // a named runner says what it actually executed, as `output` does.
        let handle = spawned.map_err(|e| match runner.addr {
            Some(addr) => anyhow::Error::new(e).context(format!(
                "exec runner {} spawned {:?}",
                addr.format(),
                spawned_as.program
            )),
            None => anyhow::Error::new(e),
        })?;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let status = self
            .drive(handle, streams, ctoken, |stream, chunk| match stream {
                StreamId::Stdout => stdout.extend_from_slice(chunk),
                StreamId::Stderr => stderr.extend_from_slice(chunk),
            })
            .await??;
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
    use hcore::hasync::StdCancellationToken;
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

    fn sh(dir: &std::path::Path, script: &str) -> proc_exec::Spec {
        proc_exec::Spec {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(script)],
            env: vec![],
            cwd: dir.to_path_buf(),
            stdin: proc_exec::StdioSpec::Null,
            stdout: proc_exec::StdioSpec::Piped,
            stderr: proc_exec::StdioSpec::Piped,
            setsid: false,
            ctty: false,
        }
    }

    /// Run `script` under `/bin/sh`, driving its output into `log`; returns
    /// the wait's result and what `observe` saw of each stream.
    async fn drive_script_with(
        log: &mut RunLog<'_>,
        script: &str,
        streams: Streams,
        ctoken: &StdCancellationToken,
    ) -> (std::io::Result<ExitStatus>, String, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (spawned, _) = hexecrunner::spawn_io(
            hexecrunner::RunnerRef::local(),
            sh(dir.path(), script),
            ctoken,
        )
        .await
        .expect("spawn");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = log
            .drive(spawned.expect("spawned"), streams, ctoken, |s, c| match s {
                StreamId::Stdout => out.extend_from_slice(c),
                StreamId::Stderr => err.extend_from_slice(c),
            })
            .await
            .expect("drive");
        (
            status,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    async fn pump_script(log: &mut RunLog<'_>, script: &str, streams: Streams) -> (String, String) {
        let ctoken = StdCancellationToken::new();
        let (status, out, err) = drive_script_with(log, script, streams, &ctoken).await;
        assert!(status.expect("wait").success());
        (out, err)
    }

    /// A scripted chunk source: its items in order, then EOF — or, with
    /// `hang`, a pipe some descendant never closes.
    struct Script {
        items: std::collections::VecDeque<Result<(StreamId, Vec<u8>), ReadError>>,
        hang: bool,
    }

    impl Script {
        fn new(
            items: impl IntoIterator<Item = Result<(StreamId, &'static str), StreamId>>,
            hang: bool,
        ) -> Self {
            let items = items
                .into_iter()
                .map(|i| match i {
                    Ok((s, text)) => Ok((s, text.as_bytes().to_vec())),
                    Err(s) => Err((s, std::io::Error::other("injected read error"))),
                })
                .collect();
            Self { items, hang }
        }
    }

    impl Chunks for Script {
        async fn next_chunk(&mut self) -> Result<Option<(StreamId, Vec<u8>)>, ReadError> {
            match self.items.pop_front() {
                Some(item) => item.map(Some),
                None if self.hang => std::future::pending().await,
                None => Ok(None),
            }
        }
    }

    fn exited(code: i32) -> std::io::Result<ExitStatus> {
        Ok(std::os::unix::process::ExitStatusExt::from_raw(code << 8))
    }

    /// Drive `script` with a wait that has already resolved to `status`;
    /// returns the result, the time it took and what the observer saw.
    async fn drive_scripted(
        log: &mut RunLog<'_>,
        mut script: Script,
        streams: Streams,
        status: std::io::Result<ExitStatus>,
        ctoken: &StdCancellationToken,
    ) -> (
        anyhow::Result<std::io::Result<ExitStatus>>,
        std::time::Duration,
        String,
    ) {
        let mut seen = Vec::new();
        let started = std::time::Instant::now();
        let res = log
            .drive_chunks(
                &mut script,
                std::future::ready(Ok(status)),
                streams,
                ctoken,
                &mut |_, c: &[u8]| seen.extend_from_slice(c),
            )
            .await;
        (
            res,
            started.elapsed(),
            String::from_utf8_lossy(&seen).into_owned(),
        )
    }

    /// Once the child has exited, a pipe that never reaches EOF is read for
    /// `DRAIN_DEADLINE` more and then abandoned, on every OS — the bytes read
    /// before are kept, the redactor's held-back tail included.
    #[tokio::test]
    async fn drive_abandons_a_pipe_left_open_after_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.txt");
        let mut log = RunLog::own_file(None, None, path.clone(), Needles::new(["s3cr3t-x"]));
        let ctoken = StdCancellationToken::new();
        let script = Script::new([Ok((StreamId::Stderr, "before s3cr"))], true);
        let (res, took, seen) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            drive_scripted(&mut log, script, Streams::BOTH, exited(0), &ctoken).await
        })
        .await
        .expect("the drain after exit must be bounded");
        assert!(res.expect("drive").expect("wait").success());
        assert!(took >= proc_exec::DRAIN_DEADLINE, "took {took:?}");
        assert!(took < proc_exec::DRAIN_DEADLINE * 4, "took {took:?}");
        assert_eq!(seen, "before s3cr");
        drop(log);
        assert_eq!(std::fs::read_to_string(&path).expect("log"), "before s3cr");
    }

    /// A cancelled run skips the drain: the pipe a descendant holds is not
    /// waited on at all.
    #[tokio::test]
    async fn drive_skips_the_drain_on_cancel() {
        let mut log = RunLog::captured(None, None);
        let ctoken = StdCancellationToken::new();
        ctoken.cancel();
        let script = Script::new([Ok((StreamId::Stdout, "partial"))], true);
        let (res, took, _) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            drive_scripted(
                &mut log,
                script,
                Streams::STDERR,
                Err(std::io::Error::other("cancelled")),
                &ctoken,
            )
            .await
        })
        .await
        .expect("a cancelled run must not drain");
        assert!(
            res.expect("drive").is_err(),
            "the wait's cancellation is the caller's"
        );
        assert!(took < proc_exec::DRAIN_DEADLINE, "took {took:?}");
    }

    /// A real child that exits while a background job holds its stdout
    /// returns within the drain deadline (plus whatever the OS's own reap
    /// takes), not when the job ends; what it printed before exit is kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn drive_returns_when_a_descendant_holds_the_pipe() {
        let mut log = RunLog::captured(None, None);
        let ctoken = StdCancellationToken::new();
        let started = std::time::Instant::now();
        let (status, out, _) = tokio::time::timeout(
            std::time::Duration::from_secs(25),
            drive_script_with(
                &mut log,
                "echo before; sleep 30 & exit 0",
                Streams::STDERR,
                &ctoken,
            ),
        )
        .await
        .expect("drive must not wait for the background job");
        assert!(status.expect("wait").success());
        assert_eq!(out, "before\n");
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(10), "took {took:?}");
    }

    /// Cancelling a real child whose descendant holds the pipe returns once
    /// the cancellation escalation is done, not when the descendant ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn drive_cancel_returns_promptly() {
        let out_w = Shared::default();
        let mut o = out_w.clone();
        let mut log = RunLog::captured(None, Some(&mut o));
        let ctoken = StdCancellationToken::new();
        let canceller = {
            let (out_w, ctoken) = (out_w.clone(), ctoken.clone());
            tokio::spawn(async move {
                while !out_w.text().contains("started") {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                ctoken.cancel();
            })
        };
        let started = std::time::Instant::now();
        let (status, _, _) = tokio::time::timeout(
            std::time::Duration::from_secs(25),
            drive_script_with(
                &mut log,
                "sleep 30 & echo started >&2; wait",
                Streams::BOTH,
                &ctoken,
            ),
        )
        .await
        .expect("a cancelled drive must return");
        canceller.await.expect("canceller");
        assert!(status.is_err(), "a cancelled wait is an error");
        let took = started.elapsed();
        assert!(
            took < proc_exec::CANCEL_GRACE + std::time::Duration::from_secs(8),
            "took {took:?}"
        );
    }

    /// A read error on a data stream fails the call — the answer would be
    /// truncated — while one on a log stream is only warned.
    #[tokio::test]
    async fn a_data_stream_read_error_fails_and_a_log_one_does_not() {
        let ctoken = StdCancellationToken::new();
        let err_w = Shared::default();
        let mut e = err_w.clone();
        let mut log = RunLog::captured(None, Some(&mut e));
        let script = Script::new(
            [
                Ok((StreamId::Stdout, "PATH=/nix/st")),
                Err(StreamId::Stdout),
                Ok((StreamId::Stderr, "progress\n")),
            ],
            false,
        );
        let (res, _, _) =
            drive_scripted(&mut log, script, Streams::STDERR, exited(0), &ctoken).await;
        let err = res.expect_err("a truncated data stream must not pass for a whole one");
        assert!(format!("{err:#}").contains("stdout"), "{err:#}");
        assert!(
            format!("{err:#}").contains("injected read error"),
            "{err:#}"
        );

        let script = Script::new(
            [
                Ok((StreamId::Stdout, "/nix/store/abc\n")),
                Ok((StreamId::Stderr, "progress\n")),
                Err(StreamId::Stderr),
            ],
            false,
        );
        let (res, _, seen) =
            drive_scripted(&mut log, script, Streams::STDERR, exited(0), &ctoken).await;
        assert!(
            res.expect("a log stream's read error is a warning")
                .expect("wait")
                .success()
        );
        assert_eq!(seen, "/nix/store/abc\nprogress\n");
        drop(log);
        assert_eq!(
            err_w.text(),
            "progress\nprogress\n",
            "the log stream was still written"
        );
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
        // The secret split across two chunks, deterministically.
        let script = Script::new(
            [
                Ok((StreamId::Stderr, "a s3cr3t-tok")),
                Ok((StreamId::Stderr, "en-value-0123456789 b")),
            ],
            false,
        );
        let ctoken = StdCancellationToken::new();
        let (res, _, _) = drive_scripted(&mut log, script, Streams::BOTH, exited(0), &ctoken).await;
        assert!(res.expect("drive").expect("wait").success());
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
