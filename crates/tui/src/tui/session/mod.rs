//! Sessions: every heph process records what it is doing to
//! `~/.heph/sessions/<id>/`, so another terminal can list, show and attach to it.
//!
//! This module is the recording side and the shared on-disk types:
//!
//! - [`format`]: the [`SessionMeta`], the [`SessionRecord`] envelope, the
//!   [`SessionSnapshot`] status, and their versions.
//! - [`store`]: `session.db`, the one SQLite file a session records into —
//!   its schema, the owner's writer, the seal, and the
//!   [`RecordCursor`](store::RecordCursor) readers use.
//! - [`derive`]: the explorer rows derived from the records.
//! - [`id`]: [`SessionId`], 10 Crockford base32 characters sorting by start.
//! - [`recorder`]: the recorder thread, its [`SessionTap`], and the
//!   [`TappedReceiver`] both backends consume.
//! - [`cleanup`]: removing ended sessions, at session start only, at most
//!   hourly per root.
//! - [`run_logs`]: finding a running target's live log through the sessions
//!   that recorded it ([`find_run_log`]).
//! - [`SessionSlot`]: the process's one session. `main` builds it and hands it
//!   to whoever needs it — every command, which passes it to `new_engine` and
//!   `run_app` as their own argument; the [`LogSink`](crate::tui::LogSink),
//!   which only tees log lines into it; the diag sweeper; the error renderer —
//!   and finishes it with the exit code. `run_app` opens it lazily.
//!
//! The types live here, in `tui`, rather than in `hcore`, because the snapshot
//! is built from [`crate::tui::BuildState`] and every consumer (the root crate's
//! commands, a future daemon client) already depends on `tui`.
//!
//! Nothing here reaches a hash, a target's argv, env or files (I1): the session
//! root is injected by the front end, and the id and directory are never put in
//! the environment — the session runner and `pass_env = ["*"]` would carry them
//! into targets.

pub mod cleanup;
pub mod derive;
pub mod export;
pub mod feed;
pub mod format;
pub mod hephsession;
pub mod holder;
pub mod id;
pub mod lockfile;
pub mod recorder;
pub mod recording;
pub mod run_logs;
pub mod source;
pub mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;

#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod view_tests;

use anyhow::Context as _;
use std::ffi::OsStr;
use std::fs::{DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

pub use feed::{Routed, SessionFeed};
pub use format::{
    ApprovalPrompt, ApprovalPromptNotice, ApprovalQueued, ApprovalStanding, CAPABILITY_INFLIGHT,
    HeaderKind, META_FORMAT, NoticeUnavailable, RunLogRecord, SNAPSHOT_VERSION, STATUS_LIST_CAP,
    SessionHeader, SessionMeta, SessionRecord, SessionRecordKind, SessionSnapshot, files,
};
pub use holder::{HolderNames, holder_session};
pub use id::SessionId;
pub use recorder::{FlushFailed, Recorder, SessionTap, StopNotice, TappedReceiver};
pub use run_logs::{
    FeedRunLogLocator, FoundRunLog, RunLogLookup, SessionRunLog, SessionRunLogLocator,
    find_run_log, lookup_run_log, session_run_log,
};
pub use source::{EntryMeta, FileSource, SessionEntry, SessionItem, SessionSource};

use cleanup::Retention;
use recorder::{Faults, RecorderStart};

/// The directory under `~/.heph` holding every session.
pub const SESSIONS_DIR: &str = "sessions";

/// The directory under `~/.heph` holding the private sessions of runs with
/// `HEPH_NO_SESSION` set. No viewer reads it.
pub const PRIVATE_SESSIONS_DIR: &str = "private-sessions";

/// The directory sessions are recorded under (`~/.heph/sessions` in the
/// binary; a temp dir in a test), or the private one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoot {
    path: PathBuf,
    private: bool,
}

impl SessionRoot {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            private: false,
        }
    }

    /// A root for private sessions (`HEPH_NO_SESSION=1`): what is recorded
    /// there serves only this process — its replay of its own lines after a
    /// full screen — and is removed at exit. `heph session list` and
    /// `attach` never read it, and an ended one is swept by the next session
    /// that starts, whatever its age.
    pub fn private(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            private: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_private(&self) -> bool {
        self.private
    }

    /// Where private sessions go beside this public root, to be swept.
    fn private_sibling(&self) -> Option<PathBuf> {
        if self.private {
            return None;
        }
        self.path
            .parent()
            .map(|parent| parent.join(PRIVATE_SESSIONS_DIR))
    }

    /// The root from the two environment values the front end read:
    /// `HEPH_NO_SESSION` and `HOME`. `HEPH_NO_SESSION` makes it a private
    /// root (or `Ok(None)` without `HOME`, which then records nothing).
    /// `Err`: it cannot be recorded (no `HOME`), said once at the first run.
    ///
    /// `HEPH_NO_SESSION` is off when unset, empty, `0`, or `false`/`no`/`off` in
    /// any case, and on for anything else — like `HEPH_NO_LISTED_FACTS`.
    pub fn from_env(
        no_session: Option<&OsStr>,
        home: Option<&OsStr>,
    ) -> Result<Option<Self>, String> {
        if switched_on(no_session) {
            return Ok(hcore::userdir::heph_user_dir_from(home)
                .ok()
                .map(|dir| Self::private(dir.join(PRIVATE_SESSIONS_DIR))));
        }
        hcore::userdir::heph_user_dir_from(home)
            .map(|dir| Some(Self::new(dir.join(SESSIONS_DIR))))
            .map_err(|e| format!("{e:#}"))
    }

    /// The root other sessions are read from, from `HOME` alone:
    /// `HEPH_NO_SESSION` turns recording off, not looking. `None` without
    /// `HOME`.
    pub fn for_reading(home: Option<&OsStr>) -> Option<Self> {
        hcore::userdir::heph_user_dir_from(home)
            .ok()
            .map(|dir| Self::new(dir.join(SESSIONS_DIR)))
    }
}

fn switched_on(v: Option<&OsStr>) -> bool {
    let Some(v) = v else { return false };
    let Some(s) = v.to_str() else { return true };
    let s = s.trim();
    !(s.is_empty()
        || s == "0"
        || ["false", "no", "off"]
            .iter()
            .any(|off| s.eq_ignore_ascii_case(off)))
}

/// What a session's `meta.json` says beyond its id and start.
#[derive(Debug, Clone, Default)]
pub struct SessionStart {
    pub header: SessionHeader,
    pub interactive: bool,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub workspace_root: Option<String>,
    pub home_dir: Option<String>,
    /// Where the session also writes `<id>.hephsession` once it ends
    /// (`HEPH_SESSION_EXPORT_DIR`). Never for a private session.
    pub export: Option<SealExport>,
}

/// The seal-time export (D7): once the session has ended and the recorder let
/// go of the recording, write `<dir>/<id>.hephsession`, within `bound`.
#[derive(Debug, Clone)]
pub struct SealExport {
    /// An absolute directory.
    pub dir: PathBuf,
    /// Reads the approval notices into the export (the engine's
    /// `read_notice_from_store`).
    pub read: hcore::approval_notice::ReadNotice,
    /// [`export::EXPORT_DIR_BOUND`] outside a test.
    pub bound: std::time::Duration,
    /// A test's slow export: it holds before compressing until this opens.
    /// Always `None` outside tests.
    pub(crate) gate: Option<recorder::Gate>,
}

impl SealExport {
    /// Export the ended session `id` in `dir` under the sessions `root`, on
    /// a thread this waits at most `bound` for. Silent on success; one
    /// `warn!` otherwise, and the exit code is never touched. Past the bound
    /// the export is stopped and its temp file removed.
    fn run(&self, root: &Path, dir: &Path, id: SessionId) {
        let abort = export::Abort::with_gate(self.gate.clone());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let (root, dir, export_dir, read, thread_abort) = (
            root.to_path_buf(),
            dir.to_path_buf(),
            self.dir.clone(),
            self.read,
            abort.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("heph-session-export".to_owned())
            .spawn(move || {
                let _dispatch = tracing::dispatcher::set_default(&dispatch);
                let written = export::to_dir(&root, &dir, id, &export_dir, read, &thread_abort);
                // Past the bound nobody waits: the result is dropped.
                if tx.send(written).is_err() {
                    tracing::debug!("the session export finished past its bound");
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "cannot export the session");
            return;
        }
        match rx.recv_timeout(self.bound) {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!(
                dir = %self.dir.display(),
                error = %format!("{e:#}"),
                "cannot export the session"
            ),
            // Not stopped: it wrote its file just as the bound passed.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !abort.abort() => {
                tracing::debug!("the session export finished at its bound");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    dir = %self.dir.display(),
                    bound_s = self.bound.as_secs(),
                    "the session export did not finish in time: nothing written"
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!(dir = %self.dir.display(), "the session export stopped");
            }
        }
    }
}

/// An open session: its directory, and the recorder writing it.
#[derive(Debug)]
pub struct Session {
    id: SessionId,
    /// The sessions root it is under.
    root: PathBuf,
    dir: PathBuf,
    recorder: Recorder,
    /// Under a private root: removed when finished.
    private: bool,
    export: Option<SealExport>,
}

/// What a new session's recorder cleans before it records.
struct Cleanups {
    /// For the session's own root.
    retention: Retention,
    /// The private root beside a public one, swept of every ended session: a
    /// killed `HEPH_NO_SESSION` run's directory goes at the next cleanup.
    private_root: Option<PathBuf>,
}

impl Session {
    /// Create the session's directory under `root`, take the lock, write
    /// `meta.json`, and start the recorder (which first cleans ended sessions).
    /// On a failure after the directory is made, it is removed again: a run
    /// that cannot record leaves nothing behind.
    pub fn open(root: &SessionRoot, start: SessionStart) -> anyhow::Result<Self> {
        Self::open_with(root, start, Retention::default(), Faults::default())
    }

    pub(crate) fn open_with(
        root: &SessionRoot,
        start: SessionStart,
        retention: Retention,
        faults: Faults,
    ) -> anyhow::Result<Self> {
        let root_dir = root.path();
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root_dir)
            .with_context(|| format!("creating {}", root_dir.display()))?;
        // An existing root keeps whatever mode it had; it is ours, make it
        // user-only (I6).
        std::fs::set_permissions(root_dir, Permissions::from_mode(0o700))
            .with_context(|| format!("making {} user-only", root_dir.display()))?;
        let started = hcore::events::now_unix_ms();
        let (id, dir) =
            create_session_dir(root_dir, started, id::random_byte).with_context(|| {
                format!("creating a session directory under {}", root_dir.display())
            })?;
        let cleanups = Cleanups {
            retention: if root.is_private() {
                Retention::PRIVATE
            } else {
                retention
            },
            private_root: root.private_sibling(),
        };
        Self::populate(root, id, dir.clone(), start, cleanups, faults).inspect_err(|_| {
            // Nothing holds it: the lock (if taken) was dropped with the error.
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::debug!(dir = %dir.display(), error = %e, "cannot remove a session directory");
            }
        })
    }

    /// Fill a freshly created session directory: the lock first, so a viewer
    /// never sees `session.db` with a free lock and no `End` (a "lost"
    /// session) for one being created.
    fn populate(
        root: &SessionRoot,
        id: SessionId,
        dir: PathBuf,
        start: SessionStart,
        cleanups: Cleanups,
        faults: Faults,
    ) -> anyhow::Result<Self> {
        if faults.fail_lock {
            anyhow::bail!("taking the session's lock: injected failure");
        }
        let lock = lockfile::acquire_owner_lock(&dir.join(files::LOCK))
            .context("taking the session's lock")?;
        let meta = SessionMeta {
            format: META_FORMAT,
            id,
            owner_pid: std::process::id(),
            started_unix_ms: id.started_unix_ms(),
            heph_version: hcore::version::current().to_owned(),
            argv: start.argv,
            cwd: start.cwd,
            workspace_root: start.workspace_root,
            home_dir: start.home_dir,
            label: start.header.label,
            header: start.header.kind,
            interactive: start.interactive,
            capabilities: vec![CAPABILITY_INFLIGHT.to_owned()],
        };
        // A private session keeps no recording to export.
        let export = start.export.filter(|_| !root.is_private());
        let (conn, rw) =
            store::create(&dir, &meta, &store::Provenance::detect(), faults.refuse_wal)
                .context("creating the session's recording")?;
        let recorder = Recorder::spawn(RecorderStart {
            root: root.path().to_path_buf(),
            lock,
            conn,
            rw,
            session_id: id,
            retention: cleanups.retention,
            private_root: cleanups.private_root,
            faults,
        })
        .context("starting the session recorder")?;
        Ok(Self {
            id,
            root: root.path().to_path_buf(),
            dir,
            recorder,
            private: root.is_private(),
            export,
        })
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn tap(&self) -> SessionTap {
        self.recorder.tap()
    }

    /// Write `End` and release the lock. See [`Recorder::finish`]. Then the
    /// seal-time export, when asked for and the recorder let go in time. A
    /// private session's directory is then removed: nothing reads it after
    /// its owner.
    pub fn finish(&self, exit_code: Option<i32>, panicked: bool) {
        let finished = self
            .recorder
            .finish(exit_code, panicked, recorder::FINISH_TIMEOUT);
        if let Some(export) = &self.export {
            match finished {
                recorder::Finished::Ended => export.run(&self.root, &self.dir, self.id),
                recorder::Finished::Late => tracing::warn!(
                    dir = %export.dir.display(),
                    "the session's recording did not end in time: it is not exported"
                ),
                recorder::Finished::Again => {}
            }
        }
        if self.private
            && let Err(e) = std::fs::remove_dir_all(&self.dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            // The next session's start sweeps it.
            tracing::debug!(dir = %self.dir.display(), error = %e, "cannot remove a private session");
        }
    }

    /// Whether this session is private (`HEPH_NO_SESSION`).
    pub fn is_private(&self) -> bool {
        self.private
    }
}

/// Create `root/<id>` with `create_dir` — never `create_dir_all` — drawing a new
/// id while the name is taken. Uniqueness comes from the filesystem, not from
/// the id's random bits.
pub(crate) fn create_session_dir(
    root: &Path,
    start_unix_ms: u64,
    mut random: impl FnMut() -> u8,
) -> io::Result<(SessionId, PathBuf)> {
    const ATTEMPTS: u32 = 64;
    for _ in 0..ATTEMPTS {
        let id = SessionId::new(start_unix_ms, random());
        let dir = root.join(id.encode());
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free session id after {ATTEMPTS} attempts"),
    ))
}

/// How the front end configured recording.
#[derive(Debug, Default)]
enum Setup {
    /// Nobody configured it (a test, an embedder): nothing is recorded.
    #[default]
    Unconfigured,
    Off,
    /// Cannot record, for this reason; said once, at the first run.
    Unavailable(String),
    Root(SessionRoot),
}

#[derive(Debug, Default)]
struct SlotInner {
    setup: Setup,
    argv: Vec<String>,
    cwd: Option<String>,
    workspace_root: Option<String>,
    home_dir: Option<String>,
    /// What the session's cleanup keeps (`.hephconfig` `session:`).
    retention: Retention,
    session: Option<std::sync::Arc<Session>>,
    /// Where other sessions are read from, to name a lock's holder. Set even
    /// when this process records nothing.
    lookup: Option<SessionRoot>,
    /// The open was attempted (successfully or not): a process opens at most
    /// one session, and warns at most once.
    attempted: bool,
    /// Injected by a test ([`SessionSlot::set_faults`]); default otherwise.
    faults: Faults,
    /// The seal-time export ([`SessionSlot::configure_export`]).
    export: ExportSetup,
}

/// How the front end configured the seal-time export.
#[derive(Debug, Default)]
enum ExportSetup {
    #[default]
    Off,
    On(SealExport),
    /// A value refused, for this reason: warned when the session opens.
    Refused(String),
}

/// The process's session (design amendment A2): built and configured by the
/// front end, opened lazily by the first `run_app` that records, finished by
/// `main` with the exit code. There is no process-wide instance: `main` owns
/// it and hands it out.
///
/// Nothing may log while holding `inner`: the log sink tees `info!` lines
/// into the open session through [`record_log`](Self::record_log), which
/// takes the same lock.
#[derive(Debug)]
pub struct SessionSlot {
    inner: Mutex<Option<SlotInner>>,
}

/// What [`SessionSlot::open`] hands a backend.
#[derive(Debug, Clone)]
pub struct OpenedSession {
    pub id: SessionId,
    pub tap: SessionTap,
    /// This call opened it (the first `run_app` of the process).
    pub fresh: bool,
    /// A private session (`HEPH_NO_SESSION`): no other terminal can attach
    /// to it, so its id is not worth printing.
    pub private: bool,
}

impl Default for SessionSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionSlot {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut SlotInner) -> T) -> T {
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        f(guard.get_or_insert_with(SlotInner::default))
    }

    /// The front end's one call: where sessions go (from
    /// [`SessionRoot::from_env`]), and the command line to record.
    pub fn configure(&self, root: Result<Option<SessionRoot>, String>, argv: Vec<String>) {
        self.with(|s| {
            s.setup = match root {
                Ok(Some(root)) => Setup::Root(root),
                Ok(None) => Setup::Off,
                Err(why) => Setup::Unavailable(why),
            };
            s.argv = argv;
        });
    }

    /// The seal-time export: `HEPH_SESSION_EXPORT_DIR` as the front end read
    /// it (an absolute directory, else refused with a warning when the
    /// session opens), and the reader that fills the export's notices.
    pub fn configure_export(&self, dir: Option<&OsStr>, read: hcore::approval_notice::ReadNotice) {
        let export = match export::export_dir_from_env(dir) {
            Ok(None) => ExportSetup::Off,
            Ok(Some(dir)) => ExportSetup::On(SealExport {
                dir,
                read,
                bound: export::EXPORT_DIR_BOUND,
                gate: None,
            }),
            Err(why) => ExportSetup::Refused(why),
        };
        self.with(|s| s.export = export);
    }

    /// Where to read other sessions from to name a lock's holder (from
    /// [`SessionRoot::for_reading`]). Unset, the recording root is used.
    pub fn configure_lookup(&self, root: Option<SessionRoot>) {
        self.with(|s| s.lookup = root);
    }

    /// Names for lock holders' pids, excluding this process's own session.
    /// Call it after [`open`](Self::open), so the own session is known.
    pub fn holder_names(&self) -> holder::HolderNames {
        self.with(|s| {
            let root = s.lookup.clone().or_else(|| match &s.setup {
                Setup::Root(root) if !root.is_private() => Some(root.clone()),
                Setup::Root(_) | Setup::Unconfigured | Setup::Off | Setup::Unavailable(_) => None,
            });
            holder::HolderNames::new(root, s.session.as_ref().map(|x| x.id()))
        })
    }

    /// The workspace, once bootstrap has resolved it. Recorded in `meta.json`
    /// of a session opened after this. `cwd` is the directory heph works from
    /// (`HEPH_CWD` when set), not the process's own cwd.
    pub fn set_workspace(&self, cwd: &Path, root: &Path, home_dir: &Path) {
        self.with(|s| {
            s.cwd = Some(cwd.to_string_lossy().into_owned());
            s.workspace_root = Some(root.to_string_lossy().into_owned());
            s.home_dir = Some(home_dir.to_string_lossy().into_owned());
        });
    }

    /// What the cleanup of a session opened after this keeps: the
    /// workspace's `.hephconfig` `session.keep` and `session.keepBytes`.
    /// Unset, [`Retention::default`]. A private session keeps nothing,
    /// whatever this says.
    pub fn set_retention(&self, retention: Retention) {
        self.with(|s| s.retention = retention);
    }

    /// What [`Self::set_retention`] set, else [`Retention::default`]: what a
    /// viewer names when a session it is asked for is gone.
    pub fn retention(&self) -> Retention {
        self.with(|s| s.retention)
    }

    /// The faults the session opened after this injects.
    #[cfg(test)]
    pub(crate) fn set_faults(&self, faults: Faults) {
        self.with(|s| s.faults = faults);
    }

    /// The session, opening it on the first call. `None` when recording is off,
    /// unavailable or failed to open (each said at most once).
    pub fn open(&self, header: SessionHeader, interactive: bool) -> Option<OpenedSession> {
        // `Err(Err(why))`: unavailable, warned below — outside the lock, which
        // the log sink's tee takes.
        let prepared = self.with(|s| {
            if let Some(session) = &s.session {
                return Err(Ok(Some(OpenedSession {
                    id: session.id(),
                    tap: session.tap(),
                    fresh: false,
                    private: session.is_private(),
                })));
            }
            if s.attempted {
                return Err(Ok(None));
            }
            s.attempted = true;
            let (export, refused) = match &s.export {
                ExportSetup::Off => (None, None),
                ExportSetup::On(e) => (Some(e.clone()), None),
                ExportSetup::Refused(why) => (None, Some(why.clone())),
            };
            match &s.setup {
                Setup::Unconfigured | Setup::Off => Err(Ok(None)),
                Setup::Unavailable(why) => Err(Err(why.clone())),
                Setup::Root(root) => Ok((
                    root.clone(),
                    SessionStart {
                        header,
                        interactive,
                        argv: s.argv.clone(),
                        cwd: s.cwd.clone(),
                        workspace_root: s.workspace_root.clone(),
                        home_dir: s.home_dir.clone(),
                        export,
                    },
                    s.retention,
                    s.faults.clone(),
                    refused,
                )),
            }
        });
        let (root, start, retention, faults, refused) = match prepared {
            Ok(prepared) => prepared,
            Err(Ok(already)) => return already,
            Err(Err(why)) => {
                tracing::warn!(reason = %why, "this run records no session");
                return None;
            }
        };
        // Opened outside the slot's lock: it does IO.
        let session = match Session::open_with(&root, start, retention, faults) {
            Ok(s) => std::sync::Arc::new(s),
            Err(e) => {
                // Once per run: the slot opens at most once.
                tracing::warn!(
                    dir = %root.path().display(),
                    error = %format!("{e:#}"),
                    "cannot record this run's session"
                );
                return None;
            }
        };
        // Once per run, and only for a session there would be to export.
        if let Some(why) = refused
            && !session.is_private()
        {
            tracing::warn!(reason = %why, "this run's session is not exported");
        }
        let opened = OpenedSession {
            id: session.id(),
            tap: session.tap(),
            fresh: true,
            private: session.is_private(),
        };
        self.with(|s| s.session = Some(session));
        Some(opened)
    }

    /// Hold `session` as [`open`](Self::open) would: a test's session opened
    /// with injected faults.
    #[cfg(test)]
    pub(crate) fn install(&self, session: Session) {
        self.with(|s| {
            s.attempted = true;
            s.session = Some(std::sync::Arc::new(session));
        });
    }

    /// The open session's directory, if any.
    pub fn dir(&self) -> Option<PathBuf> {
        self.with(|s| s.session.as_ref().map(|x| x.dir().to_path_buf()))
    }

    /// The open session's tap, if any.
    fn tap(&self) -> Option<SessionTap> {
        self.with(|s| s.session.as_ref().map(|x| x.tap()))
    }

    /// Record that the in-flight report was just rewritten.
    pub fn record_inflight_written(&self) {
        if let Some(tap) = self.tap() {
            tap.inflight_written();
        }
    }

    /// Record line `seq` of the log sink, formatted, of `level` (empty for a
    /// diagnostic; design amendment A11). Lossy UTF-8, verbatim otherwise: a
    /// viewer escapes control bytes when it prints. Nothing when no session
    /// is open or its recording stopped. Called inside the sink's lock, so it
    /// never logs: a stop it latched is returned for the sink to report.
    #[must_use = "a latched stop must be reported outside the sink's lock"]
    pub fn record_log(&self, seq: u64, level: &'static str, line: &[u8]) -> Option<StopNotice> {
        let tap = self.tap()?;
        if tap.is_stopped() {
            return None;
        }
        let text = String::from_utf8_lossy(line);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        tap.log(seq, level, text.to_owned())
    }

    /// The open session's tap and its directory, for a replay of the sink's
    /// lines. `None` when no session is open.
    pub(crate) fn replay_source(&self) -> Option<(SessionTap, PathBuf)> {
        self.with(|s| s.session.as_ref().map(|x| (x.tap(), x.dir().to_path_buf())))
    }

    /// Record text the process just wrote straight to its terminal (the
    /// failure report), so an attached viewer prints it too. One channel push;
    /// nothing when no session is open or its recording stopped. Cut to
    /// [`OUTPUT_CAP`] bytes with a marker: `Output` is written past the event
    /// log's cap, so this keeps that overshoot bounded.
    pub fn record_output(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(tap) = self.tap()
            && !tap.is_stopped()
        {
            tap.view(
                hcore::events::now_unix_ms(),
                SessionRecordKind::Output {
                    text: capped_output(text),
                },
            );
        }
    }

    /// Finish the session with the process's exit code: `End`, then the lock.
    /// Bounded; idempotent. Call it before the process exits.
    pub fn finish(&self, exit_code: Option<i32>) {
        if let Some(session) = self.with(|s| s.session.clone()) {
            session.finish(exit_code, false);
        }
    }

    /// Finish as panicked. For the drop guard that runs while a panic unwinds
    /// through `run_app`.
    pub fn finish_panicked(&self) {
        if let Some(session) = self.with(|s| s.session.clone()) {
            session.finish(None, true);
        }
    }
}

/// The engine announces each run's live log here
/// (`Engine::set_run_log_announcer`); it is recorded as a `RunLog` record of
/// the open session, past the cap too. One channel push; nothing when no
/// session is open or its recording stopped. Carries no session id: the
/// engine never learns it (I1).
impl hcore::session_record::RunLogAnnouncer for SessionSlot {
    fn run_log_created(&self, record: RunLogRecord) {
        if let Some(tap) = self.tap() {
            tap.run_log(record);
        }
    }
}

/// The most bytes of text one `Output` record keeps, so one failure report
/// cannot take a large share of [`recorder::FULL_CAP`] on its own. Past that
/// cap `Output` is dropped.
pub(crate) const OUTPUT_CAP: usize = 1024 * 1024;

/// `text` cut to its first [`OUTPUT_CAP`] bytes (at a character boundary),
/// with a line saying how much was left out.
fn capped_output(text: &str) -> String {
    if text.len() <= OUTPUT_CAP {
        return text.to_owned();
    }
    let kept = text
        .get(..text.floor_char_boundary(OUTPUT_CAP))
        .unwrap_or_default();
    let mut out = String::with_capacity(kept.len() + 96);
    out.push_str(kept);
    if !kept.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "[session: output cut at 1 MiB; {} more bytes were printed but not recorded]\n",
        text.len() - kept.len()
    ));
    out
}

/// Finishes the slot's session as panicked if dropped while unwinding.
#[derive(Debug)]
pub(crate) struct PanicGuard<'a>(pub(crate) &'a SessionSlot);

impl Drop for PanicGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.finish_panicked();
        }
    }
}
