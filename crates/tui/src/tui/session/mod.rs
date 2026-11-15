//! Sessions: every heph process records what it is doing to
//! `~/.heph/sessions/<id>/`, so another terminal can list, show and attach to it.
//!
//! This module is the recording side and the shared on-disk types:
//!
//! - [`format`]: `meta.json`, the `events.ndjson` record envelope, the
//!   `status.json` [`SessionSnapshot`], and their versions.
//! - [`id`]: [`SessionId`], 10 Crockford base32 characters sorting by start.
//! - [`recorder`]: the recorder thread, its [`SessionTap`], and the
//!   [`TappedReceiver`] both backends consume.
//! - [`reader`]: [`RecordReader`], reading `events.ndjson` back (torn lines
//!   wait, bad lines are skipped and counted).
//! - [`cleanup`]: removing ended sessions, at session start only.
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
pub mod feed;
pub mod format;
pub mod holder;
pub mod id;
pub mod lockfile;
pub mod reader;
pub mod recorder;
pub mod run_logs;
pub mod source;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod view_tests;

use anyhow::Context as _;
use std::ffi::OsStr;
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

pub use feed::{Routed, SessionFeed};
pub use format::{
    ApprovalPrompt, ApprovalPromptNotice, CAPABILITY_INFLIGHT, CAPABILITY_RUN_LOGS, HeaderKind,
    META_FORMAT, RunLogRecord, SNAPSHOT_VERSION, STATUS_LIST_CAP, SessionHeader, SessionMeta,
    SessionRecord, SessionRecordKind, SessionSnapshot, files,
};
pub use holder::{HolderNames, holder_session};
pub use id::SessionId;
pub use reader::RecordReader;
pub use recorder::{Recorder, SessionTap, TappedReceiver};
pub use run_logs::{FoundRunLog, SessionRunLog, find_run_log, session_run_log};
pub use source::{EntryMeta, FileSource, SessionEntry, SessionItem, SessionSource};

use cleanup::Retention;
use recorder::{Faults, RecorderStart};

/// The directory under `~/.heph` holding every session.
pub const SESSIONS_DIR: &str = "sessions";

/// The directory sessions are recorded under (`~/.heph/sessions` in the
/// binary; a temp dir in a test).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoot(PathBuf);

impl SessionRoot {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The root from the two environment values the front end read:
    /// `HEPH_NO_SESSION` and `HOME`. `Ok(None)`: recording is switched off.
    /// `Err`: it cannot be recorded (no `HOME`), said once at the first run.
    ///
    /// `HEPH_NO_SESSION` is off when unset, empty, `0`, or `false`/`no`/`off` in
    /// any case, and on for anything else — like `HEPH_NO_LISTED_FACTS`.
    pub fn from_env(
        no_session: Option<&OsStr>,
        home: Option<&OsStr>,
    ) -> Result<Option<Self>, String> {
        if switched_on(no_session) {
            return Ok(None);
        }
        hcore::userdir::heph_user_dir_from(home)
            .map(|dir| Some(Self(dir.join(SESSIONS_DIR))))
            .map_err(|e| format!("{e:#}"))
    }

    /// The root other sessions are read from, from `HOME` alone:
    /// `HEPH_NO_SESSION` turns recording off, not looking. `None` without
    /// `HOME`.
    pub fn for_reading(home: Option<&OsStr>) -> Option<Self> {
        hcore::userdir::heph_user_dir_from(home)
            .ok()
            .map(|dir| Self(dir.join(SESSIONS_DIR)))
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
}

/// An open session: its directory, and the recorder writing it.
#[derive(Debug)]
pub struct Session {
    id: SessionId,
    dir: PathBuf,
    recorder: Recorder,
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
        Self::populate(root_dir, id, dir.clone(), start, retention, faults).inspect_err(|_| {
            // Nothing holds it: the lock (if taken) was dropped with the error.
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::debug!(dir = %dir.display(), error = %e, "cannot remove a session directory");
            }
        })
    }

    /// Fill a freshly created session directory: the lock first, so a viewer
    /// never sees `meta.json` with a free lock and no `End` (a "lost" session)
    /// for one being created.
    fn populate(
        root_dir: &Path,
        id: SessionId,
        dir: PathBuf,
        start: SessionStart,
        retention: Retention,
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
            capabilities: vec![
                CAPABILITY_INFLIGHT.to_owned(),
                CAPABILITY_RUN_LOGS.to_owned(),
            ],
        };
        recorder::write_atomically(&dir, files::META, &meta).context("writing meta.json")?;
        let events = OpenOptions::new()
            .create_new(true)
            .append(true)
            .mode(0o600)
            .open(dir.join(files::EVENTS))
            .context("creating events.ndjson")?;
        let recorder = Recorder::spawn(RecorderStart {
            dir: dir.clone(),
            root: root_dir.to_path_buf(),
            lock,
            events: Box::new(events),
            retention,
            faults,
        })
        .context("starting the session recorder")?;
        Ok(Self { id, dir, recorder })
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

    /// Write `End` and release the lock. See [`Recorder::finish`].
    pub fn finish(&self, exit_code: Option<i32>, panicked: bool) {
        self.recorder
            .finish(exit_code, panicked, recorder::FINISH_TIMEOUT);
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
    session: Option<std::sync::Arc<Session>>,
    /// Where other sessions are read from, to name a lock's holder. Set even
    /// when this process records nothing.
    lookup: Option<SessionRoot>,
    /// The open was attempted (successfully or not): a process opens at most
    /// one session, and warns at most once.
    attempted: bool,
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
                Setup::Root(root) => Some(root.clone()),
                Setup::Unconfigured | Setup::Off | Setup::Unavailable(_) => None,
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
                })));
            }
            if s.attempted {
                return Err(Ok(None));
            }
            s.attempted = true;
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
                    },
                )),
            }
        });
        let (root, start) = match prepared {
            Ok(prepared) => prepared,
            Err(Ok(already)) => return already,
            Err(Err(why)) => {
                tracing::warn!(reason = %why, "this run records no session");
                return None;
            }
        };
        // Opened outside the slot's lock: it does IO.
        let session = match Session::open(&root, start) {
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
        let opened = OpenedSession {
            id: session.id(),
            tap: session.tap(),
            fresh: true,
        };
        self.with(|s| s.session = Some(session));
        Some(opened)
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

    /// Record a formatted log line of `level` (design amendment A11). Lossy
    /// UTF-8, verbatim otherwise: a viewer escapes control bytes when it
    /// prints. Nothing when no session is open or its recording stopped.
    pub fn record_log(&self, level: &'static str, line: &[u8]) {
        if let Some(tap) = self.tap()
            && !tap.is_stopped()
        {
            let text = String::from_utf8_lossy(line);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            tap.log(level, text.to_owned());
        }
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

/// The most bytes of text one `Output` record keeps. A failure report is
/// written past the event log's cap (it is what a viewer needs at the end),
/// so without this a huge report would make the cap no bound at all.
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
