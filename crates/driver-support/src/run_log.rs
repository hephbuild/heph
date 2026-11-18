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

/// The longest a drain may run once the child is reaped, however much of it
/// was spent waiting. The waiting budget alone ([`proc_exec::DRAIN_DEADLINE`])
/// never runs out against a descendant that keeps the pipe full (`yes >&2 &`):
/// every read is ready at once, and the drain would copy its output forever.
const DRAIN_WALL_CAP: std::time::Duration = proc_exec::DRAIN_DEADLINE.saturating_mul(10);

/// Why a drain after the reap was cut short.
#[derive(Debug, Clone, Copy)]
enum Abandoned {
    /// The pipe stayed open and quiet for the whole waiting budget.
    Quiet,
    /// The pipe kept producing past [`DRAIN_WALL_CAP`].
    Flooded,
}

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
/// A stream left out is the process's data: [`RunLog::drive`] hands it to the
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
    /// it lives. The deadline counts only time spent waiting for output, not
    /// time spent writing it to the log; a descendant that keeps the pipe full
    /// is cut off instead by a wall-clock cap of ten deadlines. A wait that ends in cancellation (or
    /// fails) skips the drain altogether; one that returned a status always
    /// drains, even if the token was cancelled after the reap.
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
                self.drive_chunks(&mut reader, wait, streams, &mut observe)
                    .await
            }
            None => wait.await.context("wait task for the child"),
        }
    }

    /// [`drive`](Self::drive) over any chunk source and wait, so the drain's
    /// bound and its error handling are testable without a real pipe.
    ///
    /// Whether to drain after the reap is decided by the wait's result, never
    /// by the token: a cancel that lands after the wait returned a status must
    /// not cut the answer short. The deadline bounds the time spent waiting for
    /// the next chunk, not writing one to the log, so a slow writer cannot cost
    /// the data stream either.
    async fn drive_chunks(
        &mut self,
        chunks: &mut impl Chunks,
        wait: impl Future<Output = Result<std::io::Result<ExitStatus>, tokio::task::JoinError>>,
        streams: Streams,
        observe: &mut impl FnMut(StreamId, &[u8]),
    ) -> anyhow::Result<std::io::Result<ExitStatus>> {
        let mut reds = Redactors {
            out: self.redactor(),
            err: self.redactor(),
        };
        let mut data_error = None;
        let mut wait = std::pin::pin!(wait);
        let mut status = None;
        // What is left of the drain deadline once the child is reaped.
        let mut budget = proc_exec::DRAIN_DEADLINE;
        // When the drain after the reap began, for the wall-clock cap.
        let mut drain_started = None;
        let mut abandoned = None;
        loop {
            let next = match &status {
                // `next_chunk` is cancel-safe on both OSes, so the wait winning
                // the race loses no chunk.
                None => tokio::select! {
                    s = &mut wait => {
                        status = Some(s);
                        continue;
                    }
                    next = chunks.next_chunk() => next,
                },
                // The child is reaped: anything it wrote is read or sitting in
                // the pipe, so EOF is moments away unless a descendant holds the
                // write end. `timeout` polls the read before the delay, so what
                // is already available is read even once the budget is spent.
                Some(Ok(Ok(_))) => {
                    let started = std::time::Instant::now();
                    if drain_started.get_or_insert(started).elapsed() >= DRAIN_WALL_CAP {
                        abandoned = Some(Abandoned::Flooded);
                        break;
                    }
                    match tokio::time::timeout(budget, chunks.next_chunk()).await {
                        Ok(next) => {
                            budget = budget.saturating_sub(started.elapsed());
                            next
                        }
                        Err(_) => {
                            abandoned = Some(Abandoned::Quiet);
                            break;
                        }
                    }
                }
                // A wait that returned no status (a cancellation, a failed wait,
                // a lost task) skips the drain, as in `proc_exec::output`, which
                // keeps `tokio::time` off a cancelled run's teardown.
                Some(_) => break,
            };
            match next {
                Ok(Some((stream, chunk))) => {
                    self.take_chunk(stream, &chunk, streams, observe, &mut reds)
                        .await;
                }
                Ok(None) => break,
                Err((stream, e)) if streams.includes(stream) => {
                    tracing::warn!(error = %e, %stream, "stopped reading a child's log stream");
                }
                Err((stream, e)) => {
                    tracing::debug!(error = %e, %stream, "a child's data stream failed to read");
                    data_error.get_or_insert((stream, e));
                }
            }
        }
        let status = match status {
            Some(status) => status,
            None => wait.await,
        };
        match abandoned {
            Some(Abandoned::Quiet) => tracing::warn!(
                deadline = ?proc_exec::DRAIN_DEADLINE,
                "a process exited but its output pipe is still open; abandoning the drain \
                 (a surviving descendant holds the write end)"
            ),
            Some(Abandoned::Flooded) => tracing::warn!(
                cap = ?DRAIN_WALL_CAP,
                "a process exited but its output pipe kept producing; abandoning the drain \
                 (a surviving descendant is still writing to it)"
            ),
            None => {}
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

    /// Hand one chunk to `observe` and, when its stream is log, to the log.
    /// Leaves the redactors' held-back tails for [`flush`](Self::flush), so a
    /// drain cut short still writes them.
    async fn take_chunk(
        &mut self,
        stream: StreamId,
        chunk: &[u8],
        streams: Streams,
        observe: &mut impl FnMut(StreamId, &[u8]),
        reds: &mut Redactors,
    ) {
        observe(stream, chunk);
        if !streams.includes(stream) {
            return;
        }
        let redactor = match stream {
            StreamId::Stdout => reds.out.as_mut(),
            StreamId::Stderr => reds.err.as_mut(),
        };
        let scrubbed = redactor.map(|r| r.push(chunk));
        self.emit(stream, scrubbed.as_deref().unwrap_or(chunk))
            .await;
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
        /// Yield once before the first item, so a wait that is already
        /// resolved wins the race and every item is still queued after it.
        gated: bool,
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
            Self {
                items,
                hang,
                gated: false,
            }
        }

        fn gated(self) -> Self {
            Self {
                gated: true,
                ..self
            }
        }
    }

    impl Chunks for Script {
        async fn next_chunk(&mut self) -> Result<Option<(StreamId, Vec<u8>)>, ReadError> {
            if std::mem::take(&mut self.gated) {
                tokio::task::yield_now().await;
            }
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
        let script = Script::new([Ok((StreamId::Stderr, "before s3cr"))], true);
        let (res, took, seen) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            drive_scripted(&mut log, script, Streams::BOTH, exited(0)).await
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

    /// A pipe that is always ready after exit (a descendant keeping it full)
    /// spends none of the waiting budget, so only the wall-clock cap ends the
    /// drain. Without the cap this drained forever: every read was ready at
    /// once and the time went to writing the log.
    #[tokio::test]
    async fn a_pipe_that_keeps_producing_after_exit_is_cut_off() {
        let mut slow = SlowWriter::default();
        let mut log = RunLog::captured(None, Some(&mut slow));
        let mut bytes = 0usize;
        let started = std::time::Instant::now();
        let res = tokio::time::timeout(
            DRAIN_WALL_CAP * 6,
            log.drive_chunks(
                &mut Flood,
                std::future::ready(Ok(exited(0))),
                Streams::STDERR,
                &mut |_, c: &[u8]| bytes += c.len(),
            ),
        )
        .await
        .expect("a flooding pipe must not drain forever");
        let took = started.elapsed();
        assert!(res.expect("drive").expect("wait").success());
        assert!(bytes > 0);
        assert!(took >= DRAIN_WALL_CAP, "took {took:?}");
        assert!(
            took < DRAIN_WALL_CAP + proc_exec::DRAIN_DEADLINE * 4,
            "took {took:?}"
        );
    }

    /// The same against a real child: a background `yes` that keeps the
    /// stderr pipe full after the shell exits does not hold the run open.
    #[test]
    fn drive_returns_when_a_descendant_floods_the_pipe() {
        // The drive runs on its own thread and runtime, watched from here: a
        // drain whose every read is ready never yields to a tokio timeout, so
        // a regression would hang rather than fail.
        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let res = rt.block_on(async {
                let ctoken = StdCancellationToken::new();
                let dir = tempfile::tempdir().expect("tempdir");
                let (spawned, _) = hexecrunner::spawn_io(
                    hexecrunner::RunnerRef::local(),
                    sh(dir.path(), "yes >&2 & exit 0"),
                    &ctoken,
                )
                .await
                .expect("spawn");
                let mut log = RunLog::captured(None, None);
                let mut bytes = 0usize;
                let status = log
                    .drive(
                        spawned.expect("spawned"),
                        Streams::STDERR,
                        &ctoken,
                        |_, c| bytes += c.len(),
                    )
                    .await
                    .expect("drive");
                (status, bytes)
            });
            // The watcher gone means it already failed the test.
            if tx.send(res).is_err() {
                tracing::debug!("the watcher gave up on the drive");
            }
        });
        let (status, bytes) = rx
            .recv_timeout(DRAIN_WALL_CAP * 6)
            .expect("drive must not copy the background job's output forever");
        let took = started.elapsed();
        assert!(status.expect("wait").success());
        assert!(bytes > 0);
        assert!(
            took < DRAIN_WALL_CAP + std::time::Duration::from_secs(5),
            "took {took:?}"
        );
    }

    /// A chunk source that is always ready, as a pipe a descendant keeps full.
    struct Flood;

    impl Chunks for Flood {
        async fn next_chunk(&mut self) -> Result<Option<(StreamId, Vec<u8>)>, ReadError> {
            Ok(Some((StreamId::Stderr, b"y\n".repeat(64))))
        }
    }

    /// A writer that takes a millisecond per write, discarding what it is given.
    #[derive(Default)]
    struct SlowWriter {
        sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    }

    impl tokio::io::AsyncWrite for SlowWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let sleep = self.sleep.get_or_insert_with(|| {
                Box::pin(tokio::time::sleep(std::time::Duration::from_millis(1)))
            });
            std::task::ready!(sleep.as_mut().poll(cx));
            self.sleep = None;
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A wait that ended in cancellation skips the drain: the pipe a
    /// descendant holds is not waited on at all.
    #[tokio::test]
    async fn drive_skips_the_drain_on_cancel() {
        let mut log = RunLog::captured(None, None);
        let script = Script::new([Ok((StreamId::Stdout, "partial"))], true);
        let (res, took, _) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            drive_scripted(
                &mut log,
                script,
                Streams::STDERR,
                Err(std::io::Error::other("cancelled")),
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

    /// A cancel that lands after the child was reaped does not skip the drain:
    /// the wait returned a status, so every chunk still queued is read. Skipping
    /// on the token returned `Ok` with a truncated answer.
    #[tokio::test]
    async fn a_cancel_after_the_reap_still_drains() {
        let mut log = RunLog::captured(None, None);
        let ctoken = StdCancellationToken::new();
        let mut script = Script::new(
            [
                Ok((StreamId::Stdout, "KEY=a\0")),
                Ok((StreamId::Stdout, "LAST=b\0")),
            ],
            false,
        )
        .gated();
        let mut seen = Vec::new();
        let wait = async {
            ctoken.cancel();
            Ok(exited(0))
        };
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            log.drive_chunks(&mut script, wait, Streams::STDERR, &mut |_, c: &[u8]| {
                seen.extend_from_slice(c)
            }),
        )
        .await
        .expect("the drain must end at EOF");
        assert!(res.expect("drive").expect("wait").success());
        assert_eq!(String::from_utf8_lossy(&seen), "KEY=a\0LAST=b\0");
    }

    /// A log writer that stalls for longer than the drain deadline does not
    /// cost the data stream: the deadline bounds waiting for the next chunk,
    /// not writing one to the log.
    #[tokio::test]
    async fn a_slow_log_writer_does_not_truncate_the_data_stream() {
        let err_w = Shared::default();
        let mut stall = Stall {
            sleep: None,
            armed: false,
            out: err_w.clone(),
        };
        let mut log = RunLog::captured(None, Some(&mut stall));
        let script = Script::new(
            [
                Ok((StreamId::Stderr, "progress\n")),
                Ok((StreamId::Stdout, "answer-1 ")),
                Ok((StreamId::Stdout, "answer-2")),
            ],
            false,
        )
        .gated();
        let (res, _, seen) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            drive_scripted(&mut log, script, Streams::STDERR, exited(0)).await
        })
        .await
        .expect("the drain must end at EOF");
        assert!(res.expect("drive").expect("wait").success());
        assert_eq!(seen, "progress\nanswer-1 answer-2");
        drop(log);
        assert_eq!(err_w.text(), "progress\n");
    }

    /// A writer whose first write stalls for twice the drain deadline.
    struct Stall {
        sleep: Option<Pin<Box<tokio::time::Sleep>>>,
        armed: bool,
        out: Shared,
    }

    impl tokio::io::AsyncWrite for Stall {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if !std::mem::replace(&mut self.armed, true) {
                self.sleep = Some(Box::pin(tokio::time::sleep(proc_exec::DRAIN_DEADLINE * 2)));
            }
            if let Some(sleep) = self.sleep.as_mut() {
                std::task::ready!(sleep.as_mut().poll(cx));
                self.sleep = None;
            }
            Pin::new(&mut self.out).poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
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
        let (res, _, _) = drive_scripted(&mut log, script, Streams::STDERR, exited(0)).await;
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
        let (res, _, seen) = drive_scripted(&mut log, script, Streams::STDERR, exited(0)).await;
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
        let (res, _, _) = drive_scripted(&mut log, script, Streams::BOTH, exited(0)).await;
        assert!(res.expect("drive").expect("wait").success());
        log.write(StreamId::Stderr, format!(" {secret}\n").as_bytes())
            .await;
        drop(log);
        let file = std::fs::read_to_string(&path).expect("log");
        assert_eq!(file, "a [redacted] b [redacted]\n");
        assert_eq!(sink.text(), file);
    }

    /// A writer that refuses every write.
    struct Refusing;

    impl tokio::io::AsyncWrite for Refusing {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::other("writer gone")))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A script of raw byte chunks, for output that is not UTF-8.
    fn raw(items: &[(StreamId, &[u8])]) -> Script {
        Script {
            items: items.iter().map(|(s, b)| Ok((*s, b.to_vec()))).collect(),
            hang: false,
            gated: false,
        }
    }

    /// A writer that refuses output does not fail the run: the own file still
    /// gets every byte, the other stream's writer still gets its own, and a
    /// last line with no trailing newline is kept as it came.
    #[tokio::test]
    async fn a_refusing_writer_fails_nothing_and_the_own_file_is_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.txt");
        let out_w = Shared::default();
        let (mut o, mut refusing) = (out_w.clone(), Refusing);
        let mut log = RunLog::own_file(Some(&mut o), Some(&mut refusing), path.clone(), None);
        let script = Script::new(
            [
                Ok((StreamId::Stderr, "a\n")),
                Ok((StreamId::Stdout, "b\n")),
                Ok((StreamId::Stderr, "c")),
            ],
            false,
        );
        let (res, _, seen) = drive_scripted(&mut log, script, Streams::BOTH, exited(0)).await;
        assert!(res.expect("drive").expect("wait").success());
        assert_eq!(seen, "a\nb\nc");
        log.write(StreamId::Stderr, b"d").await;
        drop(log);
        assert_eq!(std::fs::read_to_string(&path).expect("log"), "a\nb\ncd");
        assert_eq!(out_w.text(), "b\n");
    }

    /// Bytes reach the log as the child wrote them: invalid UTF-8 untouched,
    /// and a multibyte character split across reads rejoined, on the scrubbed
    /// own-file path as under `log_capture`.
    #[tokio::test]
    async fn non_utf8_and_split_multibyte_output_reach_the_log_unchanged() {
        let chunks: [(StreamId, &[u8]); 3] = [
            (StreamId::Stderr, b"\xff\xfe bad\n"),
            (StreamId::Stderr, b"caf\xc3"),
            (StreamId::Stderr, b"\xa9\n"),
        ];
        let want: &[u8] = b"\xff\xfe bad\ncaf\xc3\xa9\n";

        let sink = Shared::default();
        let mut s = sink.clone();
        let mut log = RunLog::captured(None, Some(&mut s));
        let (res, _, _) = drive_scripted(&mut log, raw(&chunks), Streams::BOTH, exited(0)).await;
        assert!(res.expect("drive").expect("wait").success());
        drop(log);
        assert_eq!(sink.0.lock().expect("lock").as_slice(), want);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.txt");
        let needles = Needles::new(["a-secret-that-never-appears"]);
        let mut log = RunLog::own_file(None, None, path.clone(), needles);
        let (res, _, _) = drive_scripted(&mut log, raw(&chunks), Streams::BOTH, exited(0)).await;
        assert!(res.expect("drive").expect("wait").success());
        drop(log);
        assert_eq!(std::fs::read(&path).expect("log"), want);
    }

    /// A child that prints nothing leaves an empty log and an untouched
    /// writer; an empty write is no write.
    #[tokio::test]
    async fn empty_output_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.txt");
        let sink = Shared::default();
        let mut s = sink.clone();
        let mut log = RunLog::own_file(None, Some(&mut s), path.clone(), None);
        let (res, _, seen) = drive_scripted(&mut log, raw(&[]), Streams::BOTH, exited(0)).await;
        assert!(res.expect("drive").expect("wait").success());
        assert_eq!(seen, "");
        log.write(StreamId::Stderr, b"").await;
        drop(log);
        assert_eq!(std::fs::read(&path).expect("log"), b"");
        assert_eq!(sink.text(), "");
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
