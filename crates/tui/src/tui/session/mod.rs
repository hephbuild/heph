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
//! - [`SessionSlot`]: the process's one session, opened lazily by `run_app`
//!   and finished by `main` with the exit code.
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
pub mod format;
pub mod id;
pub mod lockfile;
pub mod reader;
pub mod recorder;
pub mod source;

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

pub use format::{
    CAPABILITY_INFLIGHT, HeaderKind, META_FORMAT, SNAPSHOT_VERSION, SessionHeader, SessionMeta,
    SessionRecord, SessionRecordKind, SessionSnapshot, files,
};
pub use id::SessionId;
pub use reader::RecordReader;
pub use recorder::{Recorder, SessionTap, TappedReceiver};
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
    /// Create the session's directory under `root`, write `meta.json`, take the
    /// lock, and start the recorder (which first cleans ended sessions).
    pub fn open(root: &SessionRoot, start: SessionStart) -> io::Result<Self> {
        Self::open_with(root, start, Retention::default(), Faults::default())
    }

    pub(crate) fn open_with(
        root: &SessionRoot,
        start: SessionStart,
        retention: Retention,
        faults: Faults,
    ) -> io::Result<Self> {
        let root_dir = root.path();
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root_dir)?;
        // An existing root keeps whatever mode it had; it is ours, make it
        // user-only (I6).
        std::fs::set_permissions(root_dir, Permissions::from_mode(0o700))?;
        let started = hcore::events::now_unix_ms();
        let (id, dir) = create_session_dir(root_dir, started, id::random_byte)?;
        let meta = SessionMeta {
            format: META_FORMAT,
            id,
            owner_pid: std::process::id(),
            started_unix_ms: started,
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
        recorder::write_atomically(&dir, files::META, &meta)?;
        let lock = lockfile::acquire_owner_lock(&dir.join(files::LOCK))?;
        let events = OpenOptions::new()
            .create_new(true)
            .append(true)
            .mode(0o600)
            .open(dir.join(files::EVENTS))?;
        let recorder = Recorder::spawn(RecorderStart {
            dir: dir.clone(),
            root: root_dir.to_path_buf(),
            lock,
            events: Box::new(events),
            retention,
            faults,
        })?;
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
    /// The open was attempted (successfully or not): a process opens at most
    /// one session, and warns at most once.
    attempted: bool,
}

/// The process's session (design amendment A2): configured by the front end,
/// opened lazily by the first `run_app` that records, finished by `main` with
/// the exit code.
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
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        self.with(|s| {
            s.setup = match root {
                Ok(Some(root)) => Setup::Root(root),
                Ok(None) => Setup::Off,
                Err(why) => Setup::Unavailable(why),
            };
            s.argv = argv;
            s.cwd = cwd;
        });
    }

    /// The workspace, once bootstrap has resolved it. Recorded in `meta.json`
    /// of a session opened after this.
    pub fn set_workspace(&self, root: &Path, home_dir: &Path) {
        self.with(|s| {
            s.workspace_root = Some(root.to_string_lossy().into_owned());
            s.home_dir = Some(home_dir.to_string_lossy().into_owned());
        });
    }

    /// The session, opening it on the first call. `None` when recording is off,
    /// unavailable or failed to open (each said at most once).
    pub fn open(&self, header: SessionHeader, interactive: bool) -> Option<OpenedSession> {
        let prepared = self.with(|s| {
            if let Some(session) = &s.session {
                return Err(Some(OpenedSession {
                    id: session.id(),
                    tap: session.tap(),
                    fresh: false,
                }));
            }
            if s.attempted {
                return Err(None);
            }
            s.attempted = true;
            match &s.setup {
                Setup::Unconfigured | Setup::Off => Err(None),
                Setup::Unavailable(why) => {
                    tracing::warn!(reason = %why, "this run records no session");
                    Err(None)
                }
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
            Err(already) => return already,
        };
        // Opened outside the slot's lock: it does IO.
        let session = match Session::open(&root, start) {
            Ok(s) => std::sync::Arc::new(s),
            Err(e) => {
                tracing::warn!(
                    dir = %root.path().display(),
                    error = %e,
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

    /// The open session's id, if any.
    pub fn id(&self) -> Option<SessionId> {
        self.with(|s| s.session.as_ref().map(|x| x.id()))
    }

    /// Record that the in-flight report was just rewritten.
    pub fn record_inflight_written(&self) {
        if let Some(tap) = self.with(|s| s.session.as_ref().map(|x| x.tap())) {
            tap.inflight_written();
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

/// The process's slot.
pub fn process() -> &'static SessionSlot {
    static PROCESS: SessionSlot = SessionSlot::new();
    &PROCESS
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
