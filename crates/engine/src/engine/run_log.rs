//! A run's live log: the one copy of a target's output, owned by the engine.
//!
//! A captured run's driver writes its output to the [`RunLogWriter`]s it is
//! handed as `RunRequest.stdout` / `stderr`. Each chunk is scrubbed of the
//! run's credential secrets ([`hdriver_support::redact`]) and appended to a
//! live file, `<home>/runs/<engine-id>/<n>.log`, opened `O_APPEND` and written
//! with `write(2)`: the file is the buffer, so a writer never waits on anyone
//! reading it. The writers tee into whatever sinks the caller had in those
//! slots (the TUI, `--raw`), never replacing them.
//!
//! When the run ends the engine [`finalize`](RunLog::finalize)s the log, which
//! flushes both streams' held-back bytes and takes the file: anything written
//! afterwards (an abandoned plugin's drain thread) is counted and discarded,
//! and never blocks. The file then becomes the cache entry's `Log` artifact on
//! success, or is copied into the failure slot through its open descriptor on
//! failure, and is unlinked once that copy is in place ([`LiveGuard`]).
//!
//! **Diagnostics only.** Nothing about a log, a [`RunLog`] or a `runs/` path
//! reaches a cache key, a def hash, a sandbox, or a target's env, argv or
//! files; the `Log` artifact type stays filtered out of every output hash.
//!
//! **Lazy.** A run's `<n>.log`, and `runs/<engine-id>/` with it, is created
//! by the first byte the run writes: a run that writes nothing (an `fs` copy,
//! a quiet tool) creates no file and unlinks none, and a warm all-hit build
//! never touches `runs/`. A file that cannot be created is the log's sticky
//! error, as a failed write is.
//!
//! **Liveness.** `runs/<engine-id>/` holds a `lock` file locked for the
//! engine's life: another heph's sweep removes a directory whose lock is
//! free, never one whose lock is held. The engine removes its own directory
//! when it is dropped.

use hdriver_support::redact::{Needles, Redactor};
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

/// The directory under the home holding every engine's run logs.
pub(crate) const RUNS_DIR: &str = "runs";
/// The file in `runs/<engine-id>/` whose lock says the directory is in use.
pub(crate) const LOCK_FILE: &str = "lock";
/// The prefix of a `runs/<engine-id>/` directory still being set up.
pub(crate) const NEW_PREFIX: &str = ".new-";

/// One engine's run logs, under `<home>/runs/<id>/`.
pub(crate) struct RunLogs {
    dir: Arc<RunsDir>,
    next: AtomicU64,
}

impl std::fmt::Debug for RunLogs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLogs")
            .field("dir", &self.dir.path)
            .finish_non_exhaustive()
    }
}

/// `runs/<id>/`, made by the first byte any run writes.
struct RunsDir {
    runs: PathBuf,
    id: String,
    path: PathBuf,
    made: Mutex<Made>,
}

enum Made {
    Not,
    /// Made, and the lock file held for its life.
    Yes(File),
    /// The engine removed it: a late first write creates nothing.
    Removed,
}

impl RunsDir {
    /// Make the directory unless it is made.
    fn ensure(&self) -> io::Result<()> {
        let mut made = self.made.lock().unwrap_or_else(PoisonError::into_inner);
        match *made {
            Made::Yes(_) => Ok(()),
            Made::Removed => Err(io::Error::other("this heph's run logs were removed")),
            Made::Not => {
                *made = Made::Yes(self.make()?);
                Ok(())
            }
        }
    }

    /// Set up `runs/<id>/` atomically: built under a `.new-` name with its
    /// lock already held, then renamed into place, so a sweep never sees the
    /// directory without its lock. Returns the held lock file.
    fn make(&self) -> io::Result<File> {
        std::fs::create_dir_all(&self.runs)?;
        let tmp = self
            .runs
            .join(format!("{NEW_PREFIX}{}-{}", self.id, std::process::id()));
        std::fs::create_dir(&tmp)?;
        let made = (|| {
            let lock = File::create(tmp.join(LOCK_FILE))?;
            lock.try_lock().map_err(|e| match e {
                std::fs::TryLockError::Error(e) => e,
                std::fs::TryLockError::WouldBlock => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "a new runs directory's lock is held",
                ),
            })?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(lock)
        })();
        if made.is_err() {
            drop(std::fs::remove_dir_all(&tmp));
        }
        made
    }
}

impl RunLogs {
    pub(crate) fn new(home: &Path) -> Self {
        let runs = home.join(RUNS_DIR);
        let id = uuid::Uuid::new_v4().as_simple().to_string();
        Self {
            dir: Arc::new(RunsDir {
                path: runs.join(&id),
                runs,
                id,
                made: Mutex::new(Made::Not),
            }),
            next: AtomicU64::new(0),
        }
    }

    /// This engine's `runs/<id>/`, whether or not it exists yet.
    #[cfg(test)]
    pub(crate) fn dir_path(&self) -> PathBuf {
        self.dir.path.clone()
    }

    /// A new live log for one run, and the guard that unlinks it. Touches no
    /// file: the log's first byte creates it.
    pub(crate) fn create(&self) -> (Arc<RunLog>, LiveGuard) {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.path.join(format!("{n}.log"));
        let log = Arc::new(RunLog::new(path, Arc::clone(&self.dir)));
        let guard = LiveGuard {
            log: Arc::clone(&log),
            unlink: true,
        };
        (log, guard)
    }
}

impl Drop for RunLogs {
    fn drop(&mut self) {
        let made = std::mem::replace(
            &mut *self.dir.made.lock().unwrap_or_else(PoisonError::into_inner),
            Made::Removed,
        );
        if let Made::Yes(lock) = made {
            // Removed while the lock is still held, so no sweep races it.
            if let Err(e) = std::fs::remove_dir_all(&self.dir.path) {
                tracing::debug!(dir = %self.dir.path.display(), error = %e, "removing this heph's run logs");
            }
            drop(lock);
        }
    }
}

/// Which of a run's streams a chunk came from. Each has its own redaction
/// carry: one buffer for both would splice them together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

/// How a chunk reaches the live file. A seam, so a test can inject `ENOSPC`
/// (macOS has no `/dev/full`).
type WriteFn = fn(&mut File, &[u8]) -> io::Result<()>;

fn write_file(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)
}

/// One run's live log.
pub(crate) struct RunLog {
    path: PathBuf,
    dir: Arc<RunsDir>,
    state: Mutex<State>,
    /// Set (Release) after the last write to the file; a follower loads it
    /// (Acquire) before its final read to EOF.
    ended: AtomicBool,
    /// Bytes written after the log was finalized, and dropped.
    discarded: AtomicU64,
}

struct State {
    file: Live,
    /// Whether the file was ever created, so the guard unlinks only a file
    /// that exists.
    created: bool,
    needles: Option<Arc<Needles>>,
    out: Option<Redactor>,
    err: Option<Redactor>,
    /// The first error creating or writing the live file. Kept, because
    /// neither a plugin's tee nor the cdylib relay reports a write error: the
    /// engine fails the run with it once the driver returned `Ok`.
    error: Option<io::Error>,
    write: WriteFn,
}

/// Where the live file is in its life.
enum Live {
    /// No byte written yet: no file.
    Unopened,
    Open(File),
    /// Finalized: the file, if any, was taken.
    Ended,
}

impl std::fmt::Debug for RunLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLog")
            .field("path", &self.path)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl RunLog {
    fn new(path: PathBuf, dir: Arc<RunsDir>) -> Self {
        Self {
            path,
            dir,
            state: Mutex::new(State {
                file: Live::Unopened,
                created: false,
                needles: None,
                out: None,
                err: None,
                error: None,
                write: write_file,
            }),
            ended: AtomicBool::new(false),
            discarded: AtomicU64::new(0),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The live file's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the run's last byte is written.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the log API (unit 2)"))]
    pub(crate) fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Bytes written after the log was finalized, and dropped.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the log API (unit 2)"))]
    pub(crate) fn discarded(&self) -> u64 {
        self.discarded.load(Ordering::Relaxed)
    }

    /// The secrets to scrub, known once the run's credentials are presented.
    /// Set before the first write.
    pub(crate) fn set_needles(&self, needles: Option<Arc<Needles>>) {
        let mut st = self.state();
        st.out = needles.clone().map(Redactor::new);
        st.err = needles.clone().map(Redactor::new);
        st.needles = needles;
    }

    /// Append one chunk of `stream`, scrubbed. What reached the file is added
    /// to `forward` when given, for the sink the writer tees into.
    fn write_chunk(&self, stream: Stream, chunk: &[u8], forward: Option<&mut Vec<u8>>) {
        let mut st = self.state();
        let st = &mut *st;
        if matches!(st.file, Live::Ended) {
            self.discarded
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            return;
        }
        let redactor = match stream {
            Stream::Stdout => st.out.as_mut(),
            Stream::Stderr => st.err.as_mut(),
        };
        let scrubbed = redactor.map(|r| r.push(chunk));
        let bytes: &[u8] = scrubbed.as_deref().unwrap_or(chunk);
        if bytes.is_empty() {
            return;
        }
        if st.error.is_none()
            && let Err(e) = self.append(st, bytes)
        {
            st.error = Some(e);
        }
        if let Some(forward) = forward {
            forward.extend_from_slice(bytes);
        }
    }

    /// Create the live file (and `runs/<id>/`) unless it exists.
    fn create_file(&self) -> io::Result<File> {
        self.dir.ensure()?;
        std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&self.path)
    }

    /// Append `bytes`, creating the file on the first of them. Blocking, as
    /// the `write(2)` itself is: one `open` (and one `mkdir`, for the
    /// engine's first log) per run that writes anything.
    fn append(&self, st: &mut State, bytes: &[u8]) -> io::Result<()> {
        if let Live::Unopened = st.file {
            st.file = Live::Open(self.create_file()?);
            st.created = true;
        }
        match &mut st.file {
            Live::Open(file) => (st.write)(file, bytes),
            Live::Unopened | Live::Ended => Ok(()),
        }
    }

    /// End the run's log: flush both streams' held-back bytes, take the file
    /// and set `ended`. Every later write is counted and discarded. `None`
    /// when it was already finalized, or when the run wrote no byte.
    pub(crate) fn finalize(&self) -> Option<File> {
        let mut st = self.state();
        let st = &mut *st;
        if matches!(st.file, Live::Ended) {
            return None;
        }
        let tails = [st.out.as_mut(), st.err.as_mut()].map(|r| r.map(Redactor::finish));
        for tail in tails.iter().flatten() {
            if !tail.is_empty()
                && st.error.is_none()
                && let Err(e) = self.append(st, tail)
            {
                st.error = Some(e);
            }
        }
        let file = match std::mem::replace(&mut st.file, Live::Ended) {
            Live::Open(file) => Some(file),
            Live::Unopened | Live::Ended => None,
        };
        self.ended.store(true, Ordering::Release);
        file
    }

    /// Whether the live file was ever created.
    fn created(&self) -> bool {
        self.state().created
    }

    /// The first error creating or writing the live file, taken.
    pub(crate) fn take_error(&self) -> Option<io::Error> {
        self.state().error.take()
    }

    /// Replace the finalized live file's bytes with an older plugin's own
    /// log, scrubbed: the returned file is complete, so it wins over what the
    /// plugin streamed. A refused or unreadable source imports nothing and
    /// leaves the live bytes as they are. `file` is `None` when the run
    /// streamed nothing; a returned log with any bytes creates it, and a file
    /// that cannot be created is the log's sticky error.
    pub(crate) fn import(
        &self,
        file: &mut Option<File>,
        source: Returned,
        roots: &[PathBuf],
    ) -> Imported {
        let needles = self.state().needles.clone();
        let mut redactor = needles.map(Redactor::new);
        let replaced = match source {
            Returned::Bytes(bytes) => self.replace(file, &mut redactor, &mut bytes.as_slice()),
            Returned::File(path) => {
                let mut src = match open_contained(&path, roots) {
                    Ok(Some(src)) => src,
                    Ok(None) => return Imported::Unavailable,
                    Err(e) => {
                        tracing::debug!(path = %path.display(), error = %e, "reading a returned log");
                        return Imported::Unavailable;
                    }
                };
                self.replace(file, &mut redactor, &mut src)
            }
        };
        match replaced {
            Ok(()) => Imported::Replaced,
            Err(e) => Imported::Failed(e),
        }
    }

    /// Truncate `file` and write `src` into it, through `redactor`, creating
    /// the file on the first byte when there is none.
    fn replace(
        &self,
        file: &mut Option<File>,
        redactor: &mut Option<Redactor>,
        src: &mut dyn io::Read,
    ) -> io::Result<()> {
        if let Some(file) = file.as_mut() {
            file.set_len(0)?;
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let chunk = buf.get(..n).unwrap_or_default();
            match redactor.as_mut() {
                Some(r) => self.write_late(file, &r.push(chunk))?,
                None => self.write_late(file, chunk)?,
            }
        }
        if let Some(r) = redactor.as_mut() {
            self.write_late(file, &r.finish())?;
        }
        Ok(())
    }

    /// Write into the finalized log's file, creating it for the first byte.
    fn write_late(&self, file: &mut Option<File>, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let file = match file {
            Some(file) => file,
            None => {
                let created = self.create_file().inspect_err(|e| {
                    let mut st = self.state();
                    if st.error.is_none() {
                        st.error = Some(io::Error::new(e.kind(), e.to_string()));
                    }
                })?;
                self.state().created = true;
                file.insert(created)
            }
        };
        file.write_all(bytes)
    }

    /// Test seam: how chunks reach the live file.
    #[cfg(test)]
    fn set_write(&self, write: WriteFn) {
        self.state().write = write;
    }
}

/// A log a run returned itself, as an older plugin does.
#[derive(Debug)]
pub(crate) enum Returned {
    File(PathBuf),
    Bytes(Vec<u8>),
}

/// What [`RunLog::import`] did.
#[derive(Debug)]
pub(crate) enum Imported {
    Replaced,
    /// Outside the sandbox, a symlink, not a regular file, or unreadable:
    /// "log unavailable", and nothing imported.
    Unavailable,
    /// Reading or writing failed part way.
    Failed(io::Error),
}

/// `path`, opened, if it is a regular file (not a symlink) inside one of
/// `roots`. `None` for anything else.
fn open_contained(path: &Path, roots: &[PathBuf]) -> io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    let real = std::fs::canonicalize(path)?;
    let inside = roots.iter().any(|root| {
        std::fs::canonicalize(root)
            .map(|r| real.starts_with(r))
            .unwrap_or(false)
    });
    if !inside {
        tracing::debug!(path = %path.display(), "a returned log outside the sandbox; not read");
        return Ok(None);
    }
    // No-follow, so a file swapped for a link after the check is refused.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&real)?;
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    Ok(Some(file))
}

/// Unlinks a run's live file once what replaces it is in place, and
/// finalizes the log if nothing did (a dropped run). A run that wrote no
/// byte has no file, and nothing is unlinked.
pub(crate) struct LiveGuard {
    log: Arc<RunLog>,
    unlink: bool,
}

impl std::fmt::Debug for LiveGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveGuard")
            .field("path", &self.log.path)
            .field("unlink", &self.unlink)
            .finish()
    }
}

impl LiveGuard {
    /// Keep the file until the engine removes its `runs/` directory: a
    /// `cache = false` success has no entry to replace it.
    pub(crate) fn retain(mut self) {
        self.unlink = false;
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        drop(self.log.finalize());
        // Finalized: no later write creates the file, so `created` is final.
        if self.unlink
            && self.log.created()
            && let Err(e) = std::fs::remove_file(&self.log.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(path = %self.log.path.display(), error = %e, "removing a run's live log");
        }
    }
}

/// Fail an `Ok` run with the live file's sticky write error, if any.
pub(crate) fn fail_on_write_error<T>(log: &RunLog, run: anyhow::Result<T>) -> anyhow::Result<T> {
    match (run, log.take_error()) {
        (Ok(_), Some(e)) => Err(anyhow::Error::new(e)
            .context(format!("writing the run's log {}", log.path().display()))),
        (run, _) => run,
    }
}

/// One stream's writer: scrubs, appends to the live file, and tees into the
/// sink that was in its slot.
///
/// A chunk always reaches the live file before anything is reported: an
/// error from the inner sink is returned only once the chunk is on disk, so a
/// caller that drops the chunk on error loses it from the sink, never from
/// the log. The inner sink's backpressure holds the *next* chunk back, so at
/// most one chunk waits for it.
pub(crate) struct RunLogWriter<'io> {
    log: Arc<RunLog>,
    stream: Stream,
    inner: Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    /// Scrubbed bytes the inner sink has not taken yet, from `sent` on.
    pending: Vec<u8>,
    sent: usize,
}

impl<'io> RunLogWriter<'io> {
    pub(crate) fn new(
        log: Arc<RunLog>,
        stream: Stream,
        inner: Option<&'io mut (dyn tokio::io::AsyncWrite + Send + Sync + Unpin)>,
    ) -> Self {
        Self {
            log,
            stream,
            inner,
            pending: Vec::new(),
            sent: 0,
        }
    }

    /// Push the pending bytes into the inner sink. On an error they are
    /// dropped: the sink refused them, the log has them.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(inner) = self.inner.as_deref_mut() else {
            self.pending.clear();
            self.sent = 0;
            return Poll::Ready(Ok(()));
        };
        while let Some(rest) = self.pending.get(self.sent..).filter(|r| !r.is_empty()) {
            match Pin::new(&mut *inner).poll_write(cx, rest) {
                Poll::Ready(Ok(0)) => {
                    self.pending.clear();
                    self.sent = 0;
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                Poll::Ready(Ok(n)) => self.sent += n,
                Poll::Ready(Err(e)) => {
                    self.pending.clear();
                    self.sent = 0;
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
        self.pending.clear();
        self.sent = 0;
        Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for RunLogWriter<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let drained = match this.poll_drain(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(r) => r,
        };
        let forward = this.inner.is_some().then_some(&mut this.pending);
        this.log.write_chunk(this.stream, buf, forward);
        let pushed = match drained {
            Ok(()) => match this.poll_drain(cx) {
                Poll::Ready(r) => r,
                // Taken on the next write or flush.
                Poll::Pending => Ok(()),
            },
            Err(e) => Err(e),
        };
        Poll::Ready(pushed.map(|()| buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_drain(cx))?;
        match this.inner.as_deref_mut() {
            Some(inner) => Pin::new(inner).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    /// Flushes only: the inner sink is the caller's, not this run's to close.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

/// Read a live file through its descriptor, from the start.
#[cfg(test)]
fn read_all(file: &File) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt as _;
    let len = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
    let mut out = vec![0u8; len];
    let mut at = 0usize;
    while let Some(rest) = out.get_mut(at..).filter(|r| !r.is_empty()) {
        match file.read_at(rest, at as u64)? {
            0 => break,
            n => at += n,
        }
    }
    out.truncate(at);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

    fn logs() -> (tempfile::TempDir, RunLogs) {
        let home = tempfile::tempdir().expect("tempdir");
        let logs = RunLogs::new(home.path());
        (home, logs)
    }

    fn enospc(_: &mut File, _: &[u8]) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }

    /// A sink that never takes a byte: the stalled reader.
    struct Stalled;

    impl tokio::io::AsyncWrite for Stalled {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Pending
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn run_log_never_blocks() {
        const MB: usize = 1 << 20;
        let (_home, logs) = logs();
        let (log, guard) = logs.create();
        let path = log.path().to_path_buf();
        assert!(path.starts_with(logs.dir_path()), "{}", path.display());

        // 8 MB streamed with a follower that opened the file after the first
        // chunk and never reads: nothing the writer does waits on it.
        let chunk = vec![b'x'; 64 * 1024];
        let mut w = RunLogWriter::new(Arc::clone(&log), Stream::Stdout, None);
        w.write_all(&chunk).await.expect("write");
        let _follower = File::open(&path).expect("follower");
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            for _ in 1..(8 * MB / chunk.len()) {
                w.write_all(&chunk).await.expect("write");
            }
        })
        .await
        .expect("the writer never waits on a reader");
        assert_eq!(
            std::fs::metadata(&path).expect("stat").len(),
            (8 * MB) as u64
        );

        // A stalled inner sink holds back the sink, not the log: the first
        // chunk lands in the file even though the sink never takes it.
        let mut stalled = Stalled;
        let mut tee = RunLogWriter::new(Arc::clone(&log), Stream::Stderr, Some(&mut stalled));
        let mut write = std::pin::pin!(tee.write(b"to the stalled sink"));
        let wrote = futures::poll!(write.as_mut());
        assert!(matches!(wrote, Poll::Ready(Ok(19))), "{wrote:?}");
        assert_eq!(
            std::fs::metadata(&path).expect("stat").len(),
            (8 * MB + 19) as u64
        );
        drop(tee);

        // Finalized: writes return Ok at once, are counted and discarded.
        let file = log.finalize().expect("first finalize takes the file");
        assert!(log.ended());
        assert!(log.finalize().is_none(), "finalize takes the file once");
        w.write_all(b"after the end")
            .await
            .expect("a write after the end is Ok");
        assert_eq!(log.discarded(), 13);

        // An abandoned plugin's drain thread writing after finalize appends
        // nothing, and never blocks.
        let late = Arc::clone(&log);
        std::thread::spawn(move || {
            for _ in 0..1000 {
                late.write_chunk(Stream::Stdout, b"late", None);
            }
        })
        .join()
        .expect("drain thread");
        assert_eq!(log.discarded(), 13 + 4000);
        assert_eq!(file.metadata().expect("stat").len(), (8 * MB + 19) as u64);
        assert!(log.take_error().is_none());
        drop(guard);
        assert!(!path.exists(), "the guard unlinks the live file");

        // ENOSPC through the write seam: kept, and it fails a run that
        // returned Ok — never hangs, never reported by the writer.
        let (log, _guard) = logs.create();
        log.set_write(enospc);
        let mut w = RunLogWriter::new(Arc::clone(&log), Stream::Stdout, None);
        w.write_all(b"no space")
            .await
            .expect("the writer reports nothing");
        w.write_all(b"still none")
            .await
            .expect("the writer reports nothing");
        drop(log.finalize());
        let failed = fail_on_write_error(&log, Ok(()));
        let err = failed.expect_err("the sticky error fails an Ok run");
        assert!(
            err.chain().any(|c| c
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.raw_os_error() == Some(libc::ENOSPC))),
            "{err:#}"
        );
        // A run that already failed keeps its own error.
        let (log, _guard) = logs.create();
        log.set_write(enospc);
        log.write_chunk(Stream::Stdout, b"x", None);
        let own = fail_on_write_error::<()>(&log, Err(anyhow::anyhow!("the tool failed")));
        assert_eq!(format!("{:#}", own.expect_err("failed")), "the tool failed");
    }

    #[tokio::test]
    async fn run_log_redacts_split_secret() {
        const SECRET: &str = "s3cr3t-T0KEN-value";
        let needles = || Needles::new([SECRET]);
        let (_home, logs) = logs();

        // Split across two stdout writes at every offset, a stderr write
        // between the halves, then repeated whole.
        for at in 1..SECRET.len() {
            let (head, tail) = SECRET.split_at(at);
            let (log, _guard) = logs.create();
            log.set_needles(needles());
            let mut sink = Vec::new();
            {
                let mut out = RunLogWriter::new(Arc::clone(&log), Stream::Stdout, Some(&mut sink));
                out.write_all(format!("before {head}").as_bytes())
                    .await
                    .expect("w");
                log.write_chunk(Stream::Stderr, b"[err]", None);
                out.write_all(format!("{tail} after {SECRET}{SECRET}\n").as_bytes())
                    .await
                    .expect("w");
                out.flush().await.expect("flush");
            }
            let file = log.finalize().expect("file");
            let bytes = String::from_utf8(read_all(&file).expect("read")).expect("utf8");
            assert!(!bytes.contains(SECRET), "offset {at}: {bytes:?}");
            assert_eq!(
                bytes, "before [err][redacted] after [redacted][redacted]\n",
                "offset {at}: text around the secret is kept, in order"
            );
            let teed = String::from_utf8(sink).expect("utf8");
            assert!(
                !teed.contains(SECRET),
                "offset {at}: the tee is scrubbed too: {teed:?}"
            );
        }

        // As a stream's last bytes, a prefix of the secret is held back until
        // finalize, which flushes it rather than dropping it. Every end goes
        // through `finalize`: the run's end, a cancellation or an abandoned
        // run (the engine, or the guard of a dropped run), and ENOSPC (the
        // sticky error is read only after it).
        let (log, _guard) = logs.create();
        log.set_needles(needles());
        log.write_chunk(Stream::Stdout, b"ends with s3cr3t", None);
        log.write_chunk(Stream::Stderr, b"; err ", None);
        log.write_chunk(Stream::Stderr, SECRET.as_bytes(), None);
        let file = log.finalize().expect("file");
        let bytes = String::from_utf8(read_all(&file).expect("read")).expect("utf8");
        assert_eq!(
            bytes, "ends with ; err [redacted]s3cr3t",
            "no held-back byte is lost"
        );

        // A dropped run's guard finalizes it, so a later write is discarded.
        let (log, guard) = logs.create();
        log.set_needles(needles());
        drop(guard);
        assert!(log.ended());
        log.write_chunk(Stream::Stdout, SECRET.as_bytes(), None);
        assert_eq!(log.discarded(), SECRET.len() as u64);

        // A stream that is only a held-back prefix creates its file at
        // finalize, which writes it.
        let (log, _guard) = logs.create();
        log.set_needles(needles());
        log.write_chunk(Stream::Stdout, b"s3cr", None);
        assert!(!log.path().exists(), "held back: nothing written yet");
        let file = log.finalize().expect("finalize writes the held-back bytes");
        assert_eq!(read_all(&file).expect("read"), b"s3cr");

        // An imported log, file or inline, is scrubbed the same way; into a
        // run that streamed nothing, it creates the file.
        let (log, _guard) = logs.create();
        log.set_needles(needles());
        let mut file = log.finalize();
        assert!(file.is_none(), "nothing streamed, no file");
        let imported = log.import(
            &mut file,
            Returned::Bytes(format!("inline {SECRET}").into_bytes()),
            &[],
        );
        assert!(matches!(imported, Imported::Replaced), "{imported:?}");
        let file = file.expect("the import created the file");
        assert_eq!(read_all(&file).expect("read"), b"inline [redacted]");
    }

    #[tokio::test]
    async fn run_log_file_is_created_by_its_first_byte() {
        let (home, logs) = logs();

        // A run that writes nothing creates nothing, and its guard unlinks
        // nothing.
        let (log, guard) = logs.create();
        let mut empty = RunLogWriter::new(Arc::clone(&log), Stream::Stdout, None);
        empty.write_all(b"").await.expect("write");
        empty.flush().await.expect("flush");
        assert!(log.finalize().is_none());
        drop(guard);
        assert!(log.take_error().is_none());
        assert!(
            !home.path().join(RUNS_DIR).exists(),
            "a run that writes no byte creates no file"
        );

        // A refused import into it creates nothing either; an empty inline
        // one neither.
        let (log, _guard) = logs.create();
        let mut file = log.finalize();
        let refused = log.import(
            &mut file,
            Returned::File(home.path().join("missing")),
            &[home.path().to_path_buf()],
        );
        assert!(matches!(refused, Imported::Unavailable), "{refused:?}");
        let empty = log.import(&mut file, Returned::Bytes(Vec::new()), &[]);
        assert!(matches!(empty, Imported::Replaced), "{empty:?}");
        assert!(file.is_none());
        assert!(!home.path().join(RUNS_DIR).exists());

        // The first byte creates the directory and the file.
        let (log, guard) = logs.create();
        log.write_chunk(Stream::Stdout, b"x", None);
        assert!(log.path().exists());
        drop(guard);
        assert!(!log.path().exists(), "the guard unlinks a created file");
    }

    #[tokio::test]
    async fn run_log_create_failure_is_sticky() {
        let home = tempfile::tempdir().expect("tempdir");
        // `runs` is a file: no directory can be made under it.
        std::fs::write(home.path().join(RUNS_DIR), "not a directory").expect("write");
        let logs = RunLogs::new(home.path());

        let (log, _guard) = logs.create();
        let mut sink = Vec::new();
        {
            let mut w = RunLogWriter::new(Arc::clone(&log), Stream::Stdout, Some(&mut sink));
            w.write_all(b"to the sink")
                .await
                .expect("the writer reports nothing");
            w.flush().await.expect("flush");
        }
        assert_eq!(sink, b"to the sink", "the sink still gets the bytes");
        assert!(log.finalize().is_none());
        let err = fail_on_write_error(&log, Ok(())).expect_err("creation failure fails the run");
        assert!(
            format!("{err:#}").contains("writing the run's log"),
            "{err:#}"
        );

        // An import that has to create the file fails the same way.
        let (log, _guard) = logs.create();
        let mut file = log.finalize();
        let imported = log.import(&mut file, Returned::Bytes(b"returned".to_vec()), &[]);
        assert!(matches!(imported, Imported::Failed(_)), "{imported:?}");
        assert!(fail_on_write_error(&log, Ok(())).is_err());
    }

    #[test]
    fn import_refuses_what_is_not_a_contained_regular_file() {
        let (home, logs) = logs();
        let sandbox = home.path().join("sandbox");
        std::fs::create_dir(&sandbox).expect("mkdir");
        let outside = home.path().join("outside.log");
        std::fs::write(&outside, "outside").expect("write");
        let inside = sandbox.join("log.txt");
        std::fs::write(&inside, "returned").expect("write");
        let link = sandbox.join("link.txt");
        std::os::unix::fs::symlink(&inside, &link).expect("symlink");
        let roots = [sandbox.clone()];

        for refused in [outside, link, sandbox.join("missing.txt"), sandbox.clone()] {
            let (log, _guard) = logs.create();
            log.write_chunk(Stream::Stdout, b"live", None);
            let mut file = log.finalize();
            let imported = log.import(&mut file, Returned::File(refused.clone()), &roots);
            assert!(
                matches!(imported, Imported::Unavailable),
                "{refused:?}: {imported:?}"
            );
            assert_eq!(
                read_all(file.as_ref().expect("file")).expect("read"),
                b"live",
                "{refused:?}: live bytes kept"
            );
        }

        let (log, _guard) = logs.create();
        log.write_chunk(Stream::Stdout, b"streamed, partly", None);
        let mut file = log.finalize();
        let imported = log.import(&mut file, Returned::File(inside), &roots);
        assert!(matches!(imported, Imported::Replaced), "{imported:?}");
        assert_eq!(
            read_all(file.as_ref().expect("file")).expect("read"),
            b"returned",
            "the returned file wins"
        );
    }

    #[test]
    fn runs_dir_is_created_lazily_locked_and_removed_on_drop() {
        let (home, logs) = logs();
        let (log, guard) = logs.create();
        assert!(
            !home.path().join(RUNS_DIR).exists(),
            "nothing before the first byte"
        );
        log.write_chunk(Stream::Stdout, b"x", None);
        let dir = logs.dir_path();
        let lock = File::open(dir.join(LOCK_FILE)).expect("lock file");
        assert!(
            matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "the directory's lock is held while the engine lives"
        );
        drop((log, guard));
        let (late, _late_guard) = logs.create();
        drop(logs);
        assert!(!dir.exists(), "the engine removes its own directory");
        assert!(lock.try_lock().is_ok());
        // A run that outlives the engine writes its first byte: nothing is
        // made again, and the log keeps the error.
        late.write_chunk(Stream::Stdout, b"late", None);
        assert!(!dir.exists(), "a removed directory is not made again");
        assert!(late.take_error().is_some());
    }
}
