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
//! engine's life: another heph's [`sweep`] removes a directory whose lock is
//! free, never one whose lock is held. The engine removes its own directory
//! when it is dropped. `heph tool gc`, `heph tool clean` and every engine's start run
//! the sweep, so a killed heph's directory (a `cache = false` success's
//! retained live file with it) is removed by the next heph to look.

use hcore::session_record::{RunLogAnnouncer, RunLogRecord};
use hdriver_support::redact::{Needles, Redactor};
use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

/// The directory under the home holding every engine's run logs.
pub(crate) const RUNS_DIR: &str = hcore::session_record::RUNS_DIR;
/// The file in `runs/<engine-id>/` whose lock says the directory is in use.
pub(crate) const LOCK_FILE: &str = "lock";
/// The prefix of a `runs/<engine-id>/` directory still being set up.
pub(crate) const NEW_PREFIX: &str = ".new-";
/// The prefix a swept `runs/<engine-id>/` is renamed to before it is deleted.
pub(crate) const TOMB_PREFIX: &str = ".tomb-";
/// How old a `.new-` or tomb directory is before a sweep takes it as a
/// crashed heph's leftover. A live one exists for a rename's length.
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// One engine's run logs, under `<home>/runs/<id>/`.
pub(crate) struct RunLogs {
    dir: Arc<RunsDir>,
    next: AtomicU64,
    /// The sweep this engine started when it was made, until it is awaited.
    start_sweep: Mutex<Option<tokio::task::JoinHandle<usize>>>,
    /// Told of each live file when its first byte creates it (the session).
    announcer: Option<Arc<dyn RunLogAnnouncer>>,
    /// This engine's live files, by address: the log API's in-process source.
    live: Arc<LiveRuns>,
}

/// This engine's runs whose live file was created, one per address (the
/// newest run's). An entry is added when the run's first byte creates its
/// file, and removed when the file is unlinked; a `cache = false` success's
/// retained file keeps its entry. Read by the log API only.
#[derive(Debug, Default)]
pub(crate) struct LiveRuns {
    runs: Mutex<std::collections::HashMap<hmodel::htaddr::Addr, LiveRun>>,
    /// The one retained file per address: retaining a newer run's file
    /// unlinks the previous one, so a `cache = false` target run N times in
    /// one engine (a watch loop, a server) keeps one file, not N.
    retained: Mutex<std::collections::HashMap<hmodel::htaddr::Addr, PathBuf>>,
    /// This engine's runs going on, one per address (the newest run's),
    /// from the moment their log is made, before any byte, to their
    /// guard's drop: how the log API tells a quiet run of this engine from
    /// one that has not run, with no session. In memory only: a run that
    /// writes nothing still touches no file.
    started: Mutex<std::collections::HashMap<hmodel::htaddr::Addr, StartedRun>>,
}

/// One run of this engine going on, its file created or not.
#[derive(Debug, Clone)]
pub(crate) struct StartedRun {
    /// Gone once the run is over and nothing holds its log.
    pub(crate) log: std::sync::Weak<RunLog>,
    pub(crate) hashin: String,
    /// When its log was made, in milliseconds since the epoch.
    pub(crate) at_unix_ms: u64,
    /// Its driver does not stream its log: it returns it as the run ends.
    pub(crate) not_streamed: bool,
}

/// One run's live file, as the log API finds it in-process.
#[derive(Debug, Clone)]
pub(crate) struct LiveRun {
    /// Gone once the run is over and nothing holds its log.
    pub(crate) log: std::sync::Weak<RunLog>,
    pub(crate) path: PathBuf,
    pub(crate) hashin: String,
    /// When the file was created, in milliseconds since the epoch.
    pub(crate) at_unix_ms: u64,
}

impl LiveRuns {
    fn map(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<hmodel::htaddr::Addr, LiveRun>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record `path` as `addr`'s retained file, and return the previous one
    /// unless it is the same.
    fn retain(&self, addr: &hmodel::htaddr::Addr, path: &Path) -> Option<PathBuf> {
        self.retained
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(addr.clone(), path.to_path_buf())
            .filter(|previous| previous != path)
    }

    /// `addr`'s newest live file in this engine, if one exists.
    pub(crate) fn get(&self, addr: &hmodel::htaddr::Addr) -> Option<LiveRun> {
        self.map().get(addr).cloned()
    }

    fn started_map(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<hmodel::htaddr::Addr, StartedRun>>
    {
        self.started.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `addr`'s newest run going on in this engine, if its log is not yet
    /// final, written to or not.
    pub(crate) fn started(&self, addr: &hmodel::htaddr::Addr) -> Option<StartedRun> {
        self.started_map()
            .get(addr)
            .filter(|run| run.log.upgrade().is_some_and(|log| !log.ended()))
            .cloned()
    }
}

/// What a run's log registers in [`LiveRuns`] when its file is created.
struct Registration {
    live: Arc<LiveRuns>,
    addr: hmodel::htaddr::Addr,
    hashin: String,
    me: std::sync::Weak<RunLog>,
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
            start_sweep: Mutex::new(None),
            announcer: None,
            live: Arc::default(),
        }
    }

    /// This engine's live files, by address.
    pub(crate) fn live(&self) -> &LiveRuns {
        &self.live
    }

    /// Announce each live file to `announcer` when it is created.
    pub(crate) fn set_announcer(&mut self, announcer: Arc<dyn RunLogAnnouncer>) {
        self.announcer = Some(announcer);
    }

    /// The [`sweep`] of every other heph's directory, as a job for the
    /// blocking pool. Never touches this engine's own directory.
    pub(crate) fn sweeper(&self) -> impl FnOnce() -> usize + Send + 'static {
        let dir = Arc::clone(&self.dir);
        move || sweep(&dir.runs, Some(&dir.id))
    }

    /// Start the session-start sweep on `rt`'s blocking pool, never waited
    /// for by a build: ended hephs' directories go without the first build
    /// step paying for it.
    pub(crate) fn start_sweep(&self, rt: &tokio::runtime::Handle) {
        let handle = rt.spawn_blocking(self.sweeper());
        *self
            .start_sweep
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    /// Wait for the start sweep; how many directories it removed (0 when it
    /// was already awaited).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn start_sweep_done(&self) -> usize {
        let handle = self
            .start_sweep
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match handle {
            Some(h) => h.await.unwrap_or(0),
            None => 0,
        }
    }

    /// This engine's `runs/<id>/`, whether or not it exists yet.
    #[cfg(test)]
    pub(crate) fn dir_path(&self) -> PathBuf {
        self.dir.path.clone()
    }

    /// A new live log for one run, unannounced.
    #[cfg(test)]
    pub(crate) fn create(&self) -> (Arc<RunLog>, LiveGuard) {
        let addr = hmodel::htaddr::parse_addr("//test:run").expect("addr");
        self.create_for(&addr, "hashin")
    }

    /// [`Self::create_for_driver`], for a driver that streams its log.
    #[cfg(test)]
    pub(crate) fn create_for(
        &self,
        addr: &hmodel::htaddr::Addr,
        hashin: &str,
    ) -> (Arc<RunLog>, LiveGuard) {
        self.create_for_driver(addr, hashin, true)
    }

    /// A new live log for `addr`'s run at `hashin`, and the guard that
    /// unlinks it. Touches no file: the log's first byte creates it, and
    /// announces it as `runs/<id>/<n>.log` (relative to the home) when an
    /// announcer is set. Allocates nothing for the announcement otherwise.
    ///
    /// `streams_log`: whether the run's driver streams its log. One that
    /// does not writes it only as the run ends, and until a byte arrives
    /// the log API says so instead of "no output yet".
    pub(crate) fn create_for_driver(
        &self,
        addr: &hmodel::htaddr::Addr,
        hashin: &str,
        streams_log: bool,
    ) -> (Arc<RunLog>, LiveGuard) {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let name = format!("{n}.log");
        let announce = self.announcer.as_ref().map(|to| Announce {
            to: Arc::clone(to),
            record: RunLogRecord {
                addr: addr.format(),
                hashin: hashin.to_owned(),
                file: format!("{RUNS_DIR}/{}/{name}", self.dir.id),
            },
        });
        let path = self.dir.path.join(name);
        let log = Arc::new_cyclic(|me| {
            let mut log = RunLog::new(path, Arc::clone(&self.dir), announce);
            log.registration = Some(Registration {
                live: Arc::clone(&self.live),
                addr: addr.clone(),
                hashin: hashin.to_owned(),
                me: me.clone(),
            });
            log
        });
        self.live.started_map().insert(
            addr.clone(),
            StartedRun {
                log: Arc::downgrade(&log),
                hashin: hashin.to_owned(),
                at_unix_ms: crate::engine::event::now_unix_ms(),
                not_streamed: !streams_log,
            },
        );
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

/// Remove every `<runs>/<id>/` whose lock is free or missing (a killed heph's,
/// or one whose removal was cut short), except `own`, and every `.new-` or
/// tomb directory older than [`STALE_AFTER`]. A directory whose lock is held
/// is never touched. Best-effort: a failure is logged and the sweep goes on.
/// Returns how many directories it removed.
pub(crate) fn sweep(runs: &Path, own: Option<&str>) -> usize {
    let Ok(entries) = std::fs::read_dir(runs) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        // Not followed: a symlink is not a heph directory.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        let path = entry.path();
        let swept = if bytes.starts_with(NEW_PREFIX.as_bytes())
            || bytes.starts_with(TOMB_PREFIX.as_bytes())
        {
            if !is_stale(&path, now) {
                continue;
            }
            std::fs::remove_dir_all(&path).map(|()| true)
        } else if bytes.starts_with(b".") || own.is_some_and(|o| bytes == o.as_bytes()) {
            continue;
        } else {
            sweep_one(runs, &path, bytes)
        };
        match swept {
            Ok(true) => removed += 1,
            Ok(false) => {}
            // Another sweep took it first.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::debug!(dir = %path.display(), error = %e, "sweeping run logs");
            }
        }
    }
    removed
}

/// Remove `dir` unless its lock is held: take the lock, rename the directory
/// to a tomb, delete the tomb, then let the lock go. `Ok(false)` when it is
/// held.
fn sweep_one(runs: &Path, dir: &Path, name: &[u8]) -> io::Result<bool> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let lock = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(LOCK_FILE))
    {
        Ok(lock) => Some(lock),
        // The lock file is made before the directory gets its name, so one
        // without it is a removal cut short.
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(lock) = &lock {
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    // Renamed first, so a half-deleted directory never sits under a live
    // name: a crash mid-delete leaves a tomb the next sweep removes.
    let tomb = runs.join(format!(
        "{TOMB_PREFIX}{}-{}",
        String::from_utf8_lossy(name),
        std::process::id()
    ));
    std::fs::rename(dir, &tomb)?;
    std::fs::remove_dir_all(&tomb)?;
    drop(lock);
    Ok(true)
}

/// Whether `dir` was last changed at least [`STALE_AFTER`] before `now`.
fn is_stale(dir: &Path, now: SystemTime) -> bool {
    std::fs::symlink_metadata(dir)
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age >= STALE_AFTER))
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
    /// Set (Release) when an imported log was renamed over the live path,
    /// before `ended`: a follower that saw `ended` reads it to tell that the
    /// file it holds is one whole log, streamed or imported, and must not be
    /// continued in the entry.
    replaced: AtomicBool,
    /// Bytes written after the log was finalized, and dropped.
    discarded: AtomicU64,
    /// Where the file is registered for the log API once it is created.
    registration: Option<Registration>,
}

struct State {
    file: Live,
    /// Whether the file was ever created, so the guard unlinks only a file
    /// that exists.
    created: bool,
    /// The guard dropped: the live path is unlinked, or about to be, and an
    /// import still running must not rename a file back over it.
    abandoned: bool,
    needles: Option<Arc<Needles>>,
    out: Option<Redactor>,
    err: Option<Redactor>,
    /// The first error creating or writing the live file. Kept, because
    /// neither a plugin's tee nor the cdylib relay reports a write error: the
    /// engine fails the run with it once the driver returned `Ok`.
    error: Option<io::Error>,
    write: WriteFn,
    /// Sent when the file is created, then gone: one record per run.
    announce: Option<Announce>,
}

/// A live file's announcement, waiting for the byte that creates the file.
struct Announce {
    to: Arc<dyn RunLogAnnouncer>,
    record: RunLogRecord,
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
    fn new(path: PathBuf, dir: Arc<RunsDir>, announce: Option<Announce>) -> Self {
        Self {
            path,
            dir,
            state: Mutex::new(State {
                file: Live::Unopened,
                created: false,
                abandoned: false,
                needles: None,
                out: None,
                err: None,
                error: None,
                write: write_file,
                announce,
            }),
            ended: AtomicBool::new(false),
            replaced: AtomicBool::new(false),
            discarded: AtomicU64::new(0),
            registration: None,
        }
    }

    /// Put the just-created file in [`LiveRuns`], replacing the addr's
    /// previous run.
    fn register(&self) {
        let Some(reg) = &self.registration else {
            return;
        };
        let at_unix_ms = crate::engine::event::now_unix_ms();
        reg.live.map().insert(
            reg.addr.clone(),
            LiveRun {
                log: reg.me.clone(),
                path: self.path.clone(),
                hashin: reg.hashin.clone(),
                at_unix_ms,
            },
        );
    }

    /// Take the unlinked file out of [`LiveRuns`], unless a newer run of the
    /// addr replaced it there.
    fn unregister(&self) {
        let Some(reg) = &self.registration else {
            return;
        };
        let mut map = reg.live.map();
        if map
            .get(&reg.addr)
            .is_some_and(|run| std::sync::Weak::ptr_eq(&run.log, &reg.me))
        {
            map.remove(&reg.addr);
        }
    }

    /// Take the run out of [`LiveRuns`]' runs going on, unless a newer run
    /// of the addr replaced it there.
    fn unregister_started(&self) {
        let Some(reg) = &self.registration else {
            return;
        };
        let mut map = reg.live.started_map();
        if map
            .get(&reg.addr)
            .is_some_and(|run| std::sync::Weak::ptr_eq(&run.log, &reg.me))
        {
            map.remove(&reg.addr);
        }
    }

    /// Keep the created file as its addr's retained one, unlinking the file
    /// an earlier run of the addr retained. That run's [`LiveRuns`] entry is
    /// already this one's: this run's first byte registered it.
    fn keep_retained(&self) {
        let Some(reg) = &self.registration else {
            return;
        };
        if let Some(previous) = reg.live.retain(&reg.addr, &self.path)
            && let Err(e) = std::fs::remove_file(&previous)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(path = %previous.display(), error = %e, "removing a superseded retained run log");
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
    pub(crate) fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Whether an imported log replaced the streamed one. Read after
    /// [`Self::ended`]: stored before it.
    pub(crate) fn replaced(&self) -> bool {
        self.replaced.load(Ordering::Acquire)
    }

    /// Bytes written after the log was finalized, and dropped.
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

    /// End `stream`: append the bytes its redactor held back as a possible
    /// secret's start, and add them to `forward` when given, so the sink the
    /// writer tees into ends where the file does. Nothing once the log is
    /// finalized: [`Self::take_file`] flushed the tail into the file then.
    fn finish_stream(&self, stream: Stream, forward: Option<&mut Vec<u8>>) {
        let mut st = self.state();
        let st = &mut *st;
        if matches!(st.file, Live::Ended) {
            return;
        }
        let redactor = match stream {
            Stream::Stdout => st.out.as_mut(),
            Stream::Stderr => st.err.as_mut(),
        };
        let Some(tail) = redactor.map(Redactor::finish).filter(|t| !t.is_empty()) else {
            return;
        };
        if st.error.is_none()
            && let Err(e) = self.append(st, &tail)
        {
            st.error = Some(e);
        }
        if let Some(forward) = forward {
            forward.extend_from_slice(&tail);
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
    /// engine's first log) per run that writes anything. Creating the file
    /// announces it (one channel send), before its first byte is written.
    fn append(&self, st: &mut State, bytes: &[u8]) -> io::Result<()> {
        if let Live::Unopened = st.file {
            st.file = Live::Open(self.create_file()?);
            st.created = true;
            self.register();
            if let Some(Announce { to, record }) = st.announce.take() {
                to.run_log_created(record);
            }
        }
        match &mut st.file {
            Live::Open(file) => (st.write)(file, bytes),
            Live::Unopened | Live::Ended => Ok(()),
        }
    }

    /// End the run's log: [`Self::take_file`], then [`Self::mark_ended`].
    pub(crate) fn finalize(&self) -> Option<File> {
        let file = self.take_file();
        self.mark_ended();
        file
    }

    /// Set `ended`: the file a follower holds has its last byte. After an
    /// import, so a follower never takes the streamed file for finished while
    /// the imported one is being renamed over it.
    pub(crate) fn mark_ended(&self) {
        self.ended.store(true, Ordering::Release);
    }

    /// Close the run's writes: flush both streams' held-back bytes and take
    /// the file. Every later write is counted and discarded. `None` when it
    /// was already taken, or when the run wrote no byte. Does not set
    /// `ended` ([`Self::mark_ended`]).
    pub(crate) fn take_file(&self) -> Option<File> {
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
        match std::mem::replace(&mut st.file, Live::Ended) {
            Live::Open(file) => Some(file),
            Live::Unopened | Live::Ended => None,
        }
    }

    /// The first error creating or writing the live file, taken.
    pub(crate) fn take_error(&self) -> Option<io::Error> {
        self.state().error.take()
    }

    /// Replace the closed live file with the log the run returned as an
    /// artifact at its end, scrubbed: the returned file is complete, so it
    /// wins over whatever the driver streamed. It is written whole into a sibling `<n>.log.import`, then
    /// renamed over the live path, so the path, and a follower's open file,
    /// is one whole log at every moment: the streamed one or the imported
    /// one, never the streamed head continued by the imported rest.
    ///
    /// A refused or unreadable source imports nothing, and a failure part
    /// way keeps the streamed bytes: the sibling is removed. An empty
    /// returned log over a streamed one still wins. `file` is `None` when the
    /// run streamed nothing; a returned log with any bytes then creates it,
    /// and a file that cannot be created is the log's sticky error. Once
    /// replaced, `file` is the imported file. Between [`Self::take_file`] and
    /// [`Self::mark_ended`].
    pub(crate) fn import(
        &self,
        file: &mut Option<File>,
        source: Returned,
        roots: &[PathBuf],
    ) -> Imported {
        let needles = self.state().needles.clone();
        let mut redactor = needles.map(Redactor::new);
        let mut tmp = ImportFile {
            log: self,
            path: self.import_path(),
            file: None,
            sticky: file.is_none(),
        };
        let copied = match source {
            Returned::Bytes(bytes) => tmp.copy(&mut redactor, &mut bytes.as_slice()),
            Returned::File(path) => {
                let mut src = match open_contained(&path, roots) {
                    Ok(Some(src)) => src,
                    Ok(None) => return Imported::Unavailable,
                    Err(e) => {
                        tracing::debug!(path = %path.display(), error = %e, "reading a returned log");
                        return Imported::Unavailable;
                    }
                };
                tmp.copy(&mut redactor, &mut src)
            }
        };
        // Nothing returned over something streamed: the empty log wins.
        let copied = copied.and_then(|()| match file {
            Some(_) => tmp.file().map(|_| ()),
            None => Ok(()),
        });
        if let Err(e) = copied {
            tmp.discard();
            return Imported::Failed(e);
        }
        let Some(imported) = tmp.file.take() else {
            // Nothing streamed and nothing returned: still no file.
            return Imported::Replaced;
        };
        // Under the lock, against the guard: either it dropped first and
        // unlinked the live path, and nothing is renamed back over it, or the
        // rename lands first and the guard sees `created` and unlinks it.
        let mut st = self.state();
        if st.abandoned {
            drop(st);
            tmp.discard();
            return Imported::Abandoned;
        }
        if let Err(e) = std::fs::rename(&tmp.path, &self.path) {
            drop(st);
            tmp.discard();
            return Imported::Failed(e);
        }
        st.created = true;
        self.replaced.store(true, Ordering::Release);
        drop(st);
        *file = Some(imported);
        Imported::Replaced
    }

    /// Where an import is written before it is renamed over the live path.
    fn import_path(&self) -> PathBuf {
        let mut path = self.path.clone().into_os_string();
        path.push(".import");
        PathBuf::from(path)
    }

    /// Test seam: how chunks reach the live file.
    #[cfg(test)]
    fn set_write(&self, write: WriteFn) {
        self.state().write = write;
    }
}

/// The sibling file an import is written into, created by its first byte.
struct ImportFile<'a> {
    log: &'a RunLog,
    path: PathBuf,
    file: Option<File>,
    /// The run streamed nothing: failing to create this file is the log's
    /// sticky error, as failing to create the live file is.
    sticky: bool,
}

impl ImportFile<'_> {
    /// The file, created (and `runs/<id>/` with it) unless it is.
    fn file(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            let created = self.log.dir.ensure().and_then(|()| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&self.path)
            });
            let created = created.inspect_err(|e| {
                let mut st = self.log.state();
                if self.sticky && st.error.is_none() {
                    st.error = Some(io::Error::new(e.kind(), e.to_string()));
                }
            })?;
            self.file = Some(created);
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("the import file was just created"))
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.file()?.write_all(bytes)
    }

    /// Write `src` through `redactor`.
    fn copy(&mut self, redactor: &mut Option<Redactor>, src: &mut dyn io::Read) -> io::Result<()> {
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
                Some(r) => self.write(&r.push(chunk))?,
                None => self.write(chunk)?,
            }
        }
        if let Some(r) = redactor.as_mut() {
            self.write(&r.finish())?;
        }
        Ok(())
    }

    /// Remove the file, if it was made.
    fn discard(self) {
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::debug!(path = %self.path.display(), error = %e, "removing an unused log import");
        }
    }
}

/// A log a run returned itself: a driver that returns its log as an artifact
/// at the end of the run instead of streaming it.
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
    /// Reading or writing failed part way: the streamed bytes are kept.
    Failed(io::Error),
    /// The run was dropped while its log was imported: its live file is
    /// unlinked, and the import is not put back in its place.
    Abandoned,
}

/// `path` with every symlink and `..` resolved, if that lies inside one of
/// `roots`; `None` if it lies outside. The error is `path`'s own
/// canonicalization failing (`NotFound` for a missing file).
///
/// The check canonicalizes `path` itself rather than trusting the caller to
/// have: a lexical `root/../elsewhere` starts with `root` component-wise.
/// Each root is canonicalized too, so a root reached through a symlink (`/tmp`
/// on macOS) still contains its files. A root that cannot be canonicalized
/// contains nothing.
pub(crate) fn inside_roots(path: &Path, roots: &[PathBuf]) -> io::Result<Option<PathBuf>> {
    let real = std::fs::canonicalize(path)?;
    let inside = roots.iter().any(|root| {
        std::fs::canonicalize(root)
            .map(|r| real.starts_with(r))
            .unwrap_or(false)
    });
    Ok(inside.then_some(real))
}

/// `path`, opened, if it is a regular file (not a symlink) inside one of
/// `roots`. `None` for anything else.
fn open_contained(path: &Path, roots: &[PathBuf]) -> io::Result<Option<File>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    let Some(real) = inside_roots(path, roots)? else {
        tracing::debug!(path = %path.display(), "a returned log outside the sandbox; not read");
        return Ok(None);
    };
    // No-follow, so a file swapped for a link after the check is refused;
    // non-blocking, so one swapped for a FIFO is opened without waiting for a
    // writer, then refused below (the flag changes nothing for a regular
    // file's reads). A directory above it swapped for a link between the
    // check and the open is not caught: the sandbox is the run's own tree,
    // and its run has returned, so only the plugin that wrote the log could
    // race it, and it could have written the bytes it wanted read instead.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
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
        self.log.unregister_started();
        // Finalized, and marked abandoned under the lock an import renames
        // under: no later write or import creates the file, so `created` is
        // final.
        let created = {
            let mut st = self.log.state();
            st.abandoned = true;
            st.created
        };
        let discarded = self.log.discarded();
        if discarded > 0 {
            tracing::warn!(
                path = %self.log.path.display(),
                discarded,
                "bytes written after the run's log was closed were dropped"
            );
        }
        if !created {
            return;
        }
        if self.unlink {
            if let Err(e) = std::fs::remove_file(&self.log.path)
                && e.kind() != io::ErrorKind::NotFound
            {
                tracing::debug!(path = %self.log.path.display(), error = %e, "removing a run's live log");
            }
            self.log.unregister();
        } else {
            self.log.keep_retained();
        }
    }
}

/// Fail an `Ok` run with the live file's sticky write error, if any.
pub(crate) fn fail_on_write_error<T>(log: &RunLog, run: anyhow::Result<T>) -> anyhow::Result<T> {
    match (run, log.take_error()) {
        (Ok(_), Some(e)) => Err(anyhow::Error::new(e)
            .context(format!("writing the run's log {}", log.path().display()))),
        // The run's own error wins, but the log is still incomplete: say so.
        (Err(run), Some(e)) => {
            tracing::warn!(
                path = %log.path().display(),
                error = %e,
                "writing the run's log; the log may be incomplete"
            );
            Err(run)
        }
        (run, None) => run,
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

    /// End this stream once the run returned: its redactor's held-back tail
    /// goes to the file and the inner sink, which is then flushed. Without
    /// it a stream ending in a secret's prefix (`s3cr`) reaches the file at
    /// finalize but never the sink. A sink error is not the run's, and is
    /// dropped: the log has the bytes. Not for an abandoned run, whose
    /// plugin may still write.
    pub(crate) async fn finish(&mut self) {
        let forward = self.inner.is_some().then_some(&mut self.pending);
        self.log.finish_stream(self.stream, forward);
        if let Err(e) = tokio::io::AsyncWriteExt::flush(self).await {
            tracing::debug!(path = %self.log.path.display(), error = %e, "flushing a run's output sink");
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

    /// The containment check both sandbox readers share: on the canonical
    /// path, by path component, through a symlinked root, and never through a
    /// root that is not there.
    #[test]
    fn inside_roots_compares_canonical_components() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        let root = base.join("sb");
        std::fs::create_dir_all(root.join("d")).expect("mkdir");
        std::fs::create_dir_all(base.join("sbx")).expect("mkdir");
        std::fs::write(root.join("d/f"), b"").expect("write");
        std::fs::write(base.join("sbx/f"), b"").expect("write");
        std::os::unix::fs::symlink(&root, base.join("link")).expect("symlink");
        let inside =
            |path: &Path, roots: &[PathBuf]| inside_roots(path, roots).expect("inside_roots");
        let only_root = std::slice::from_ref(&root);

        assert_eq!(inside(&root.join("d/f"), only_root), Some(root.join("d/f")));
        assert_eq!(
            inside(&root.join("d/f"), &[base.join("link")]),
            Some(root.join("d/f")),
            "a root reached through a symlink still contains its files",
        );
        assert_eq!(
            inside(&base.join("link/d/f"), only_root),
            Some(root.join("d/f")),
            "a path reached through a symlink is judged, and returned, resolved",
        );
        assert_eq!(
            inside(&base.join("sbx/f"), only_root),
            None,
            "a sibling sharing the root's name as a prefix is outside",
        );
        assert_eq!(
            inside(&root.join("../sbx/f"), only_root),
            None,
            "a `..` out of the root is resolved before the comparison",
        );
        assert_eq!(
            inside(&root.join("d/f"), &[base.join("gone")]),
            None,
            "a root that cannot be canonicalized contains nothing",
        );
        assert_eq!(inside(&root.join("d/f"), &[]), None);
        assert_eq!(
            inside_roots(&root.join("d/missing"), only_root)
                .expect_err("a missing path")
                .kind(),
            io::ErrorKind::NotFound,
        );
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

    /// A stream that ends in what could be a secret's start holds it back;
    /// the writer's `finish` sends it to the sink as well as the file, so
    /// the terminal does not lose a stream's last bytes that the log keeps.
    #[tokio::test]
    async fn writer_finish_forwards_the_held_back_tail() {
        let (_home, logs) = logs();
        let (log, _guard) = logs.create();
        log.set_needles(Needles::new(["s3cr3t-T0KEN-value"]));
        let mut out_sink = Vec::new();
        let mut err_sink = Vec::new();
        {
            let mut out = RunLogWriter::new(Arc::clone(&log), Stream::Stdout, Some(&mut out_sink));
            let mut err = RunLogWriter::new(Arc::clone(&log), Stream::Stderr, Some(&mut err_sink));
            out.write_all(b"out s3cr").await.expect("w");
            err.write_all(b"err ").await.expect("w");
            out.flush().await.expect("flush");
            // `s3cr` is held back here: a flush does not end the stream.
            out.finish().await;
            err.finish().await;
        }
        assert_eq!(out_sink, b"out s3cr", "the tail reached the sink");
        assert_eq!(err_sink, b"err ");
        let file = log.finalize().expect("file");
        assert_eq!(
            read_all(&file).expect("read"),
            b"out err s3cr",
            "the tail reached the file once, before finalize"
        );
    }

    #[derive(Debug, Default)]
    struct Announced(Mutex<Vec<RunLogRecord>>);

    impl RunLogAnnouncer for Announced {
        fn run_log_created(&self, record: RunLogRecord) {
            self.0.lock().expect("announced").push(record);
        }
    }

    /// A live file is announced once, by the byte that creates it, as its
    /// path relative to the home; a run that writes nothing announces
    /// nothing.
    #[test]
    fn run_log_is_announced_by_its_first_byte() {
        let home = tempfile::tempdir().expect("tempdir");
        let mut logs = RunLogs::new(home.path());
        let announced = Arc::new(Announced::default());
        logs.set_announcer(Arc::clone(&announced) as Arc<dyn RunLogAnnouncer>);
        let addr = hmodel::htaddr::parse_addr("//p:t").expect("addr");

        let (quiet, _quiet_guard) = logs.create_for(&addr, "h0");
        drop(quiet.finalize());
        let (log, _guard) = logs.create_for(&addr, "h1");
        assert!(announced.0.lock().expect("announced").is_empty());
        log.write_chunk(Stream::Stdout, b"a", None);
        log.write_chunk(Stream::Stderr, b"b", None);
        drop(log.finalize());

        let records = announced.0.lock().expect("announced").clone();
        assert_eq!(records.len(), 1, "{records:?}");
        let rec = records.first().expect("one record");
        assert_eq!((rec.addr.as_str(), rec.hashin.as_str()), ("//p:t", "h1"));
        let rel = rec.relative_file().expect("a well-formed file field");
        assert_eq!(home.path().join(rel), log.path());
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
        assert!(log.replaced());
        assert_eq!(std::fs::read(log.path()).expect("read"), b"returned");
        assert!(!log.import_path().exists(), "renamed into place");
    }

    /// The import is renamed over the live path whole: the streamed file a
    /// follower holds keeps the streamed bytes, never a mix; an empty
    /// returned log still wins.
    #[test]
    fn import_replaces_the_live_file_whole() {
        let (_home, logs) = logs();
        let (log, _guard) = logs.create();
        log.write_chunk(Stream::Stdout, b"streamed", None);
        let mut file = log.take_file();
        let held = File::open(log.path()).expect("a follower's open file");
        let imported = log.import(&mut file, Returned::Bytes(b"imported".to_vec()), &[]);
        assert!(matches!(imported, Imported::Replaced), "{imported:?}");
        assert!(!log.ended(), "ended only once marked");
        assert_eq!(read_all(&held).expect("read"), b"streamed");
        assert_eq!(std::fs::read(log.path()).expect("read"), b"imported");

        let (log, _guard) = logs.create();
        log.write_chunk(Stream::Stdout, b"streamed", None);
        let mut file = log.take_file();
        let empty = log.import(&mut file, Returned::Bytes(Vec::new()), &[]);
        assert!(matches!(empty, Imported::Replaced), "{empty:?}");
        assert_eq!(std::fs::read(log.path()).expect("read"), b"");
        assert!(log.replaced());
    }

    /// A `cache = false` target run twice in one engine keeps one retained
    /// file: the newer run's, which the log API finds; another addr's
    /// retained file is untouched.
    #[test]
    fn retaining_a_newer_run_unlinks_the_previous_retained_file() {
        let (_home, logs) = logs();
        let addr = hmodel::htaddr::parse_addr("//pkg:serve").expect("addr");
        let other = hmodel::htaddr::parse_addr("//pkg:other").expect("addr");
        let run = |addr: &hmodel::htaddr::Addr, text: &[u8]| {
            let (log, guard) = logs.create_for(addr, "hashin");
            log.write_chunk(Stream::Stdout, text, None);
            drop(log.finalize());
            guard.retain();
            log.path().to_path_buf()
        };
        let first = run(&addr, b"first");
        let kept = run(&other, b"other");
        let second = run(&addr, b"second");
        assert_ne!(first, second);
        assert!(!first.exists(), "the superseded retained file is unlinked");
        assert_eq!(std::fs::read(&second).expect("read"), b"second");
        assert_eq!(std::fs::read(&kept).expect("read"), b"other");
        assert_eq!(
            logs.live().get(&addr).expect("registered").path,
            second,
            "the log API finds the newer run"
        );
    }

    /// A run dropped while its log is imported: the guard unlinked the live
    /// file, and the import leaves neither it nor its sibling behind.
    #[test]
    fn an_abandoned_import_leaves_no_file() {
        let (_home, logs) = logs();
        let (log, guard) = logs.create();
        log.write_chunk(Stream::Stdout, b"streamed", None);
        let mut file = log.take_file();
        drop(guard);
        assert!(!log.path().exists(), "the guard unlinked it");
        let imported = log.import(&mut file, Returned::Bytes(b"imported".to_vec()), &[]);
        assert!(matches!(imported, Imported::Abandoned), "{imported:?}");
        assert!(!log.path().exists(), "not renamed back");
        assert!(!log.import_path().exists(), "no import file left");
        assert!(!log.replaced());
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
        // `flock` belongs to the open file description, and a `fork` copies
        // every fd: another test's spawn on another thread holds the engine's
        // lock until its child execs. So wait for the lock rather than try it,
        // bounded so a lock the engine never lets go fails instead of hanging.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // A gone receiver already failed the test.
            let _sent = tx.send(lock.lock().map(|()| lock));
        });
        let freed = rx.recv_timeout(std::time::Duration::from_secs(60));
        assert!(
            matches!(freed, Ok(Ok(_))),
            "the engine lets its lock go: {freed:?}"
        );
        // A run that outlives the engine writes its first byte: nothing is
        // made again, and the log keeps the error.
        late.write_chunk(Stream::Stdout, b"late", None);
        assert!(!dir.exists(), "a removed directory is not made again");
        assert!(late.take_error().is_some());
    }
}
