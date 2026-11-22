//! `session.db`: one SQLite file per session, the recording.
//!
//! The owner's recorder thread is the only writer while the session's `lock`
//! is held ([`Writer`]); after that only a cleanup holding the lock
//! exclusively writes it, to [`seal`] it. `records` is the source of truth;
//! `status` and the explorer tables (`spans`, `span_events`, `metrics`, and
//! the `logs`, `sessions` and `targets` views) are derived from it
//! ([`super::derive`]).
//!
//! A reader decides how to open a file from its header alone
//! ([`read_header`]): `application_id` says it is a heph recording,
//! `user_version` is its format, and bytes 18–19 say whether it is still in
//! WAL mode (live, or a seal cut short) or sealed (rollback mode, opened
//! `immutable=1`). See `docs/SESSIONS.md`.

use std::collections::VecDeque;
use std::io::{self, Read as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use hcore::events::BuildEventKind;
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

use super::derive::{Deriver, Row};
use super::format::{HeaderKind, SessionMeta, SessionRecord, SessionRecordKind};
use super::id::SessionId;
use super::lockfile;

/// The recording's file name in a session directory.
pub const FILE: &str = "session.db";
/// The name a recording is created under, renamed to [`FILE`] once its schema
/// is in place, so no viewer ever opens a half-made file.
pub(crate) const CREATING: &str = ".session.db.creating";
/// `PRAGMA application_id` of every heph recording ("HEPH").
pub const APPLICATION_ID: u32 = 0x4845_5048;
/// `PRAGMA user_version`: the recording's format. A reader reads every format
/// back to 1 and refuses a newer one by name.
pub const FORMAT: u32 = 1;
/// The version of the code that derives the explorer rows (D14).
pub const DERIVATION: u32 = 1;
/// The derivation of a lost recording cleanup sealed: older than any real
/// one, so a session command re-derives it before reading its explorer rows
/// (D14), as it does a recording an older heph derived.
pub const STALE_DERIVATION: u32 = 0;
/// The explorer contract's version, `sessions.schema_version` (I9).
pub const SCHEMA_VERSION: u32 = 1;
/// How long a connection waits on a transient `SQLITE_BUSY`.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(1);
/// How long the owner retries a seal a viewer is in the way of.
pub const SEAL_TIMEOUT: Duration = Duration::from_secs(3);

const SCHEMA: &str = r"
CREATE TABLE session (
  id TEXT PRIMARY KEY, format INTEGER NOT NULL, heph_version TEXT NOT NULL,
  owner_pid INTEGER, host TEXT, user TEXT,
  actor_kind TEXT, actor_name TEXT, ci_run_url TEXT,
  command TEXT, argv TEXT, cwd TEXT, workspace_root TEXT, home TEXT, git_head TEXT,
  label TEXT, header TEXT, interactive INTEGER, capabilities TEXT, lossy INTEGER NOT NULL DEFAULT 0,
  started_ms INTEGER NOT NULL, ended_ms INTEGER,
  exit_code INTEGER, panicked INTEGER,
  truncated INTEGER NOT NULL DEFAULT 0, truncated_seq INTEGER,
  derivation INTEGER NOT NULL,
  sealed INTEGER NOT NULL DEFAULT 0,
  snapshot INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE records (
  seq INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL,
  kind TEXT NOT NULL, addr TEXT, body TEXT NOT NULL
);
CREATE TABLE status (id INTEGER PRIMARY KEY CHECK (id = 1), at_ms INTEGER, snapshot TEXT);
CREATE TABLE notices (addr TEXT, seq INTEGER, name TEXT, text TEXT);
CREATE TABLE failure_logs (addr TEXT, nth INTEGER, log BLOB, cut_bytes INTEGER);
CREATE TABLE spans (
  session_id TEXT, span_id INTEGER, parent_span_id INTEGER,
  name TEXT, class TEXT, addr TEXT, driver TEXT, outcome TEXT,
  start_us INTEGER, end_us INTEGER, duration_us INTEGER,
  status TEXT, attributes TEXT,
  PRIMARY KEY (session_id, span_id)
);
CREATE TABLE span_events (session_id TEXT, span_id INTEGER, name TEXT, time_us INTEGER, attributes TEXT);
CREATE TABLE metrics (session_id TEXT, name TEXT, kind TEXT, unit TEXT, time_us INTEGER, value REAL, attributes TEXT);
CREATE VIEW logs AS
  SELECT s.id AS session_id, r.seq AS seq, r.at_ms * 1000 AS time_us,
    CASE r.kind WHEN 'Output' THEN 'info'
      ELSE coalesce(nullif(lower(json_extract(r.body, '$.session.level')), ''), 'info') END AS severity,
    CASE r.kind WHEN 'Output' THEN json_extract(r.body, '$.session.text')
      ELSE json_extract(r.body, '$.session.msg') END AS body
  FROM records r, session s WHERE r.kind IN ('Log', 'Output');
CREATE VIEW sessions AS
  SELECT id AS session_id, 1 AS schema_version, format, heph_version, owner_pid, host, user,
    actor_kind, actor_name, ci_run_url, command, argv, cwd, workspace_root, home, git_head,
    label, interactive, started_ms, ended_ms, exit_code, panicked, truncated, truncated_seq,
    sealed, 1 AS recorded, snapshot
  FROM session;
CREATE VIEW targets AS
  SELECT session_id, addr, outcome, duration_us FROM (
    SELECT session_id, addr, outcome, duration_us, row_number() OVER (
      PARTITION BY session_id, addr
      ORDER BY CASE outcome WHEN 'failed' THEN 0 WHEN 'ran' THEN 1
        WHEN 'cancelled' THEN 2 WHEN 'hit' THEN 3 ELSE 4 END, span_id
    ) AS n
    FROM spans WHERE name = 'heph.result'
  ) WHERE n = 1;
";

/// What a recording's header says, read from its first 100 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub application_id: u32,
    pub format: u32,
    /// Bytes 18 and 19 are 2: the file is in WAL mode (live, or not sealed).
    pub wal: bool,
}

impl Header {
    pub fn is_heph(&self) -> bool {
        self.application_id == APPLICATION_ID
    }

    /// A heph recording in a format this heph reads.
    pub fn is_known(&self) -> bool {
        self.is_heph() && (1..=FORMAT).contains(&self.format)
    }
}

/// The header of the SQLite file at `path`, or `None` when it is shorter than
/// a header (zero bytes: a recording killed while being created) or not a
/// SQLite file at all.
pub fn read_header(path: &Path) -> io::Result<Option<Header>> {
    let mut buf = [0u8; 100];
    let mut file = std::fs::File::open(path)?;
    let mut filled = 0;
    while filled < buf.len() {
        let Some(rest) = buf.get_mut(filled..) else {
            break;
        };
        match file.read(rest)? {
            0 => return Ok(None),
            n => filled += n,
        }
    }
    let be = |at: usize| {
        buf.get(at..at + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map_or(0, u32::from_be_bytes)
    };
    if buf.get(..16) != Some(b"SQLite format 3\0".as_slice()) {
        return Ok(None);
    }
    Ok(Some(Header {
        application_id: be(68),
        format: be(60),
        wal: buf.get(18) == Some(&2) || buf.get(19) == Some(&2),
    }))
}

/// Why a file cannot be read as a recording.
#[derive(Debug)]
pub enum OpenError {
    /// No recording in the directory (yet: a session being created; or
    /// removed meanwhile).
    Missing,
    /// Not a heph recording (another SQLite file, or no SQLite file at all).
    NotASession,
    /// A recording in a format newer than this heph reads.
    TooNew {
        found: u32,
    },
    Other(anyhow::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("no recording"),
            Self::NotASession => f.write_str("not a heph session"),
            Self::TooNew { found } => write!(
                f,
                "recorded in session format {found}, newer than this heph reads ({FORMAT}); upgrade heph to read it"
            ),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<anyhow::Error> for OpenError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Other(e.into())
    }
}

/// `path` as a `file:` URI, `?`, `#` and `%` percent-encoded.
fn file_uri(path: &Path, query: &str) -> String {
    let mut uri = String::from("file:");
    for c in path.to_string_lossy().chars() {
        match c {
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            '%' => uri.push_str("%25"),
            c => uri.push(c),
        }
    }
    uri.push('?');
    uri.push_str(query);
    uri
}

fn read_only_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI
}

/// A read-only connection on a live (WAL-mode) recording. Never creates the
/// file.
pub fn open_live(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(file_uri(path, "mode=ro"), read_only_flags())?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// A connection on a sealed recording: `immutable=1`, so no `-shm`, no locks,
/// and it works on a read-only copy. Only for a rollback-mode header.
pub fn open_sealed(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(file_uri(path, "mode=ro&immutable=1"), read_only_flags())
}

/// A connection that may write an existing recording (a seal, a
/// re-derivation). Never creates the file.
fn open_existing_rw(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// A recording opened for reading, with whatever keeps it readable: a copy
/// in the temp directory, removed when this drops.
#[derive(Debug)]
pub struct Opened {
    pub conn: Connection,
    /// The session's lock was held when it was opened.
    pub live: bool,
    copy: Option<TempCopy>,
}

impl Opened {
    /// Whether this reads a copy of a lost or unsealed recording.
    pub fn is_copy(&self) -> bool {
        self.copy.is_some()
    }
}

#[derive(Debug)]
struct TempCopy {
    files: Vec<PathBuf>,
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        for f in &self.files {
            if let Err(e) = std::fs::remove_file(f)
                && e.kind() != io::ErrorKind::NotFound
            {
                tracing::debug!(file = %f.display(), error = %e, "cannot remove a session copy");
            }
        }
    }
}

/// Where readers put their copies: `<sessions root>/.tmp/`, swept by cleanup
/// of anything older than 10 minutes.
pub fn tmp_dir(root: &Path) -> PathBuf {
    root.join(TMP_DIR)
}

/// The directory name of [`tmp_dir`] under a sessions root.
pub const TMP_DIR: &str = ".tmp";

/// Open the local recording in `dir` for reading, choosing how from the lock
/// and the header (`docs/SESSIONS.md`, "Reading"):
///
/// - lock held: the owner is writing it; a read-only WAL connection. (A held
///   lock over a rollback-mode header is a cleanup sealing it: read as ended.
///   So is a held lock whose session row already says `sealed=1`: the
///   holder is sealing, and this waits for it rather than hold a connection
///   that would cut the seal short.)
/// - lock free, rollback-mode header: sealed; `immutable=1`.
/// - lock free, WAL-mode header (lost, or a seal cut short): take the lock
///   shared while copying the file and its `-wal` into [`tmp_dir`], and open
///   the copy. The original is never written.
pub fn open_local(root: &Path, dir: &Path) -> Result<Opened, OpenError> {
    let path = dir.join(FILE);
    let lock = dir.join(super::format::files::LOCK);
    let live = lockfile::is_held(&lock).with_context(|| format!("probing {}", lock.display()))?;
    let header = match read_header(&path) {
        Ok(h) => h.ok_or(OpenError::NotASession)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(OpenError::Missing),
        Err(e) => {
            return Err(OpenError::Other(
                anyhow::Error::new(e).context(format!("reading {}", path.display())),
            ));
        }
    };
    check_header(header)?;
    if !header.wal {
        return Ok(Opened {
            conn: open_sealed(&path)?,
            live: false,
            copy: None,
        });
    }
    if live {
        let conn = open_live(&path)?;
        if !session_sealed(&conn)? {
            return Ok(Opened {
                conn,
                live: true,
                copy: None,
            });
        }
        // `sealed=1` under a held lock: whoever holds it (the owner after
        // `End`, a cleanup sealing a lost session) is folding the WAL in. The
        // session has ended, and an open connection would cut the seal short.
        drop(conn);
        return open_after_seal(root, dir, &lock, &path);
    }
    open_copy(root, dir, &lock)
}

/// The session row's `sealed` column: `End` (or a cleanup's end) is
/// committed. One read, no transaction left open.
fn session_sealed(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT sealed FROM session", [], |r| r.get(0))
}

/// A recording whose lock holder is sealing it: wait, holding nothing open,
/// for the header to turn rollback (read it sealed) or the lock to free (a
/// seal cut short or abandoned: read a copy), as long as a seal may retry.
/// Past that, read it in place, as ended.
fn open_after_seal(root: &Path, dir: &Path, lock: &Path, path: &Path) -> Result<Opened, OpenError> {
    let deadline = Instant::now() + SEAL_TIMEOUT;
    loop {
        match read_header(path) {
            Ok(Some(h)) if !h.wal => {
                return Ok(Opened {
                    conn: open_sealed(path)?,
                    live: false,
                    copy: None,
                });
            }
            Ok(_) => {}
            Err(e) => {
                return Err(OpenError::Other(
                    anyhow::Error::new(e).context(format!("reading {}", path.display())),
                ));
            }
        }
        if !lockfile::is_held(lock).with_context(|| format!("probing {}", lock.display()))? {
            return open_copy(root, dir, lock);
        }
        if Instant::now() >= deadline {
            return Ok(Opened {
                conn: open_live(path)?,
                live: false,
                copy: None,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn check_header(header: Header) -> Result<(), OpenError> {
    if !header.is_heph() {
        return Err(OpenError::NotASession);
    }
    if header.format > FORMAT {
        return Err(OpenError::TooNew {
            found: header.format,
        });
    }
    if header.format == 0 {
        return Err(OpenError::NotASession);
    }
    Ok(())
}

fn open_copy(root: &Path, dir: &Path, lock: &Path) -> Result<Opened, OpenError> {
    let tmp = tmp_dir(root);
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("making {} user-only", tmp.display()))?;
    // Shared: a cleanup's exclusive seal waits, and every other probe still
    // reads the session as not live. A lock held since the probe (the owner
    // came back? no: a cleanup sealing it) means read it again, sealed.
    let guard =
        lockfile::try_lock_shared(lock).with_context(|| format!("locking {}", lock.display()))?;
    let path = dir.join(FILE);
    if guard.is_none() {
        // A cleanup holds it exclusively: it is sealing. Read what it left.
        return match read_header(&path) {
            Ok(Some(h)) if !h.wal => Ok(Opened {
                conn: open_sealed(&path)?,
                live: false,
                copy: None,
            }),
            _ => Err(OpenError::Other(anyhow::anyhow!(
                "{} is being sealed; try again",
                path.display()
            ))),
        };
    }
    let base = tmp.join(format!(
        "copy-{}-{}.db",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut copy = TempCopy { files: Vec::new() };
    copy.files.push(base.clone());
    std::fs::copy(&path, &base).with_context(|| format!("copying {}", path.display()))?;
    let wal = sibling(&path, "-wal");
    let wal_copy = sibling(&base, "-wal");
    copy.files.push(wal_copy.clone());
    copy.files.push(sibling(&base, "-shm"));
    match std::fs::copy(&wal, &wal_copy) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(OpenError::Other(
                anyhow::Error::new(e).context(format!("copying {}", wal.display())),
            ));
        }
    }
    drop(guard);
    let conn = open_existing_rw(&base)?;
    Ok(Opened {
        conn,
        live: false,
        copy: Some(copy),
    })
}

/// `path` with `suffix` appended to its file name (`-wal`, `-shm`).
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// The session row's identity fields beyond what [`SessionMeta`] carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    pub host: Option<String>,
    pub user: Option<String>,
    pub actor: Actor,
}

/// Who ran the session, from the environment alone (O5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actor {
    /// `human`, `agent` or `ci`.
    pub kind: &'static str,
    pub name: Option<&'static str>,
    pub ci_run_url: Option<String>,
}

impl Provenance {
    /// From the process environment and host name. Reads only; never sets.
    pub fn detect() -> Self {
        Self {
            host: host_name(),
            user: std::env::var("USER").ok().filter(|u| !u.is_empty()),
            actor: Actor::from_env(
                std::env::vars_os()
                    .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))),
            ),
        }
    }
}

impl Actor {
    /// The actor the environment describes: an agent first (it may run in
    /// CI), then CI, then a human.
    pub fn from_env(env: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut claude = false;
        let mut github = false;
        let mut ci = false;
        let (mut server, mut repo, mut run) = (None, None, None);
        for (k, v) in env {
            match k.as_str() {
                "CLAUDECODE" => claude = v == "1",
                "GITHUB_ACTIONS" => github = v == "true",
                "CI" => {
                    let v = v.trim();
                    ci = !(v.is_empty()
                        || v == "0"
                        || v.eq_ignore_ascii_case("false")
                        || v.eq_ignore_ascii_case("no"));
                }
                "GITHUB_SERVER_URL" => server = Some(v),
                "GITHUB_REPOSITORY" => repo = Some(v),
                "GITHUB_RUN_ID" => run = Some(v),
                _ => {}
            }
        }
        let ci_run_url = if github {
            match (server, repo, run) {
                (Some(s), Some(r), Some(id)) => Some(format!("{s}/{r}/actions/runs/{id}")),
                _ => None,
            }
        } else {
            None
        };
        if claude {
            Self {
                kind: "agent",
                name: Some("claude-code"),
                ci_run_url,
            }
        } else if github {
            Self {
                kind: "ci",
                name: Some("github-actions"),
                ci_run_url,
            }
        } else if ci {
            Self {
                kind: "ci",
                name: Some("unknown"),
                ci_run_url: None,
            }
        } else {
            Self {
                kind: "human",
                name: None,
                ci_run_url: None,
            }
        }
    }
}

fn host_name() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is valid for `buf.len()` bytes for the duration of the call.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    buf.get(..end)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .filter(|h| !h.is_empty())
}

/// Create `dir/session.db` with its schema and session row: under a temp
/// name, WAL checked, closed, renamed, and reopened by its final name (SQLite
/// names `-wal` after the path a connection opened).
pub(crate) fn create(
    dir: &Path,
    meta: &SessionMeta,
    provenance: &Provenance,
    refuse_wal: bool,
) -> anyhow::Result<Connection> {
    let tmp = dir.join(CREATING);
    {
        let conn = Connection::open_with_flags(
            &tmp,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("creating {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("making {} user-only", tmp.display()))?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .context("switching the recording to WAL")?;
        if refuse_wal || !mode.eq_ignore_ascii_case("wal") {
            anyhow::bail!("the filesystem refused WAL mode (journal_mode is {mode})");
        }
        // Runs on the main thread of every command: one unsynced commit, not
        // one synced commit per statement. A crash before the rename leaves
        // only the temp name, which nothing reads.
        conn.execute_batch("PRAGMA synchronous=OFF;")
            .context("configuring the new recording")?;
        init(&conn, meta, provenance)?;
        conn.close()
            .map_err(|(_, e)| e)
            .with_context(|| format!("closing {}", tmp.display()))?;
    }
    let path = dir.join(FILE);
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    let conn = open_existing_rw(&path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch("PRAGMA synchronous=OFF;")
        .context("configuring the recording")?;
    Ok(conn)
}

/// The schema, the format stamp and the session row, in one transaction: a
/// recording has all of them or none.
fn init(conn: &Connection, meta: &SessionMeta, provenance: &Provenance) -> anyhow::Result<()> {
    let tx = conn
        .unchecked_transaction()
        .context("starting the recording's schema")?;
    tx.execute_batch(SCHEMA)
        .context("creating the recording's schema")?;
    tx.execute_batch(&format!(
        "PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version={FORMAT};"
    ))
    .context("stamping the recording's format")?;
    insert_session_row(&tx, meta, provenance).context("writing the session row")?;
    tx.commit().context("committing the recording's schema")
}

/// A lossy string, and whether it lost anything (a NUL counts).
fn lossy(s: &str, lost: &mut bool) -> String {
    if s.contains('\0') {
        *lost = true;
        return s.replace('\0', "\u{fffd}");
    }
    if s.contains('\u{fffd}') {
        *lost = true;
    }
    s.to_owned()
}

fn insert_session_row(
    conn: &Connection,
    meta: &SessionMeta,
    provenance: &Provenance,
) -> rusqlite::Result<()> {
    let mut lost = false;
    let argv: Vec<String> = meta.argv.iter().map(|a| lossy(a, &mut lost)).collect();
    let cwd = meta.cwd.as_deref().map(|c| lossy(c, &mut lost));
    let command = argv.get(1).cloned();
    let header = match meta.header {
        HeaderKind::Gc => "gc",
        HeaderKind::Build | HeaderKind::Unknown => "build",
    };
    conn.execute(
        "INSERT INTO session (id, format, heph_version, owner_pid, host, user, actor_kind,
           actor_name, ci_run_url, command, argv, cwd, workspace_root, home, label, header,
           interactive, capabilities, lossy, started_ms, derivation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
           ?18, ?19, ?20, ?21)",
        params![
            meta.id.encode(),
            FORMAT,
            meta.heph_version,
            meta.owner_pid,
            provenance.host,
            provenance.user,
            provenance.actor.kind,
            provenance.actor.name,
            provenance.actor.ci_run_url,
            command,
            serde_json::to_string(&argv).unwrap_or_default(),
            cwd,
            meta.workspace_root,
            meta.home_dir,
            meta.label,
            header,
            meta.interactive,
            serde_json::to_string(&meta.capabilities).unwrap_or_default(),
            lost,
            i64::try_from(meta.started_unix_ms).unwrap_or(i64::MAX),
            DERIVATION,
        ],
    )?;
    Ok(())
}

/// The session row's view of who the session is, as [`SessionMeta`].
pub fn read_meta(conn: &Connection) -> rusqlite::Result<SessionMeta> {
    conn.query_row(
        "SELECT id, format, owner_pid, started_ms, heph_version, argv, cwd, workspace_root,
           home, label, header, interactive, capabilities FROM session",
        [],
        |r| {
            let id: String = r.get(0)?;
            let argv: Option<String> = r.get(5)?;
            let caps: Option<String> = r.get(12)?;
            let header: Option<String> = r.get(10)?;
            Ok(SessionMeta {
                format: r.get(1)?,
                id: SessionId::parse(&id).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?,
                owner_pid: r.get::<_, Option<u32>>(2)?.unwrap_or(0),
                started_unix_ms: r.get::<_, i64>(3)?.try_into().unwrap_or(0),
                heph_version: r.get(4)?,
                argv: argv
                    .and_then(|a| serde_json::from_str(&a).ok())
                    .unwrap_or_default(),
                cwd: r.get(6)?,
                workspace_root: r.get(7)?,
                home_dir: r.get(8)?,
                label: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
                header: match header.as_deref() {
                    Some("gc") => HeaderKind::Gc,
                    _ => HeaderKind::Build,
                },
                interactive: r.get::<_, Option<bool>>(11)?.unwrap_or(false),
                capabilities: caps
                    .and_then(|c| serde_json::from_str(&c).ok())
                    .unwrap_or_default(),
            })
        },
    )
}

/// The session row of the local recording in `dir` (under the sessions
/// `root`), opened as [`open_local`] does.
pub fn read_meta_in(root: &Path, dir: &Path) -> Result<SessionMeta, OpenError> {
    let opened = open_local(root, dir)?;
    Ok(read_meta(&opened.conn)?)
}

/// The `seq` of the last committed record, 0 when there is none.
pub fn last_seq(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT coalesce(max(seq), 0) FROM records", [], |r| {
        r.get(0)
    })
}

/// What the session row says about how the recording ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowEnd {
    pub sealed: bool,
    pub truncated: bool,
    pub truncated_seq: Option<i64>,
    pub ended_ms: Option<i64>,
}

pub fn read_row_end(conn: &Connection) -> rusqlite::Result<RowEnd> {
    conn.query_row(
        "SELECT sealed, truncated, truncated_seq, ended_ms FROM session",
        [],
        |r| {
            Ok(RowEnd {
                sealed: r.get(0)?,
                truncated: r.get(1)?,
                truncated_seq: r.get(2)?,
                ended_ms: r.get(3)?,
            })
        },
    )
}

/// The latest `status` snapshot, as JSON.
pub fn read_status(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT snapshot FROM status WHERE id = 1", [], |r| r.get(0))
        .optional()
        .map(Option::flatten)
}

/// One bounded pool's use over a session (D30): the session-wide
/// `heph.session.pool.*` rows derivation writes when the session finishes,
/// and the pool's last `heph.pool.capacity`. Amounts are in `unit` (`{cpu}`,
/// `By` or `1`). Part of `heph session show --json`'s `pools`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PoolSummary {
    pub pool: String,
    pub unit: String,
    /// The pool's capacity at its last row; `None` when no row gave one.
    pub capacity: Option<f64>,
    /// Held, time-weighted over the pool's in-use window: from its first
    /// `PoolUsage` row to the session's end (not from the session's start).
    pub held_mean: f64,
    pub held_peak: f64,
    /// Time with at least one request waiting for the pool (the queue was
    /// not empty), not the sum of every request's wait.
    pub waiting_us: u64,
}

/// Every pool with session-wide rows, by name. Empty for a session still
/// running (the rows come at its end), and for one that admitted nothing.
pub fn read_pools(conn: &Connection) -> rusqlite::Result<Vec<PoolSummary>> {
    let mut stmt = conn.prepare(
        "SELECT json_extract(attributes, '$.\"heph.pool\"'), name, unit, value FROM metrics \
         WHERE name IN ('heph.pool.capacity', 'heph.session.pool.held.mean', \
           'heph.session.pool.held.peak', 'heph.session.pool.waiting_us') \
         ORDER BY time_us",
    )?;
    let mut rows = stmt.query([])?;
    let mut capacity: std::collections::BTreeMap<String, f64> = Default::default();
    let mut pools: std::collections::BTreeMap<String, PoolSummary> = Default::default();
    while let Some(r) = rows.next()? {
        let Some(pool) = r.get::<_, Option<String>>(0)? else {
            continue;
        };
        let name: String = r.get(1)?;
        let unit: String = r.get(2)?;
        let value: f64 = r.get(3)?;
        if name == "heph.pool.capacity" {
            // Ordered by time: the last row wins.
            capacity.insert(pool, value);
            continue;
        }
        let summary = pools.entry(pool.clone()).or_insert_with(|| PoolSummary {
            pool,
            unit: String::new(),
            capacity: None,
            held_mean: 0.0,
            held_peak: 0.0,
            waiting_us: 0,
        });
        match name.as_str() {
            "heph.session.pool.held.mean" => {
                summary.held_mean = value;
                summary.unit = unit;
            }
            "heph.session.pool.held.peak" => summary.held_peak = value,
            // A counter of whole microseconds, stored as REAL.
            _ => summary.waiting_us = u64::try_from(value as i64).unwrap_or(0),
        }
    }
    Ok(pools
        .into_values()
        .map(|mut p| {
            p.capacity = capacity.get(&p.pool).copied();
            p
        })
        .collect())
}

/// The record's `kind` column, and its `addr` when it is about one target.
/// Exhaustive on purpose: a new event kind must say what it is about.
pub fn kind_and_addr(record: &SessionRecord) -> (&'static str, Option<&str>) {
    if let Some(kind) = &record.kind {
        return event_kind_and_addr(kind);
    }
    let name = match &record.session {
        Some(SessionRecordKind::Log { .. }) => "Log",
        Some(SessionRecordKind::End { .. }) => "End",
        Some(SessionRecordKind::Truncated) => "Truncated",
        Some(SessionRecordKind::InflightWritten) => "InflightWritten",
        Some(SessionRecordKind::Label { .. }) => "Label",
        Some(SessionRecordKind::Approval { .. }) => "Approval",
        Some(SessionRecordKind::Finished { .. }) => "Finished",
        Some(SessionRecordKind::Output { .. }) => "Output",
        Some(SessionRecordKind::RunLog(run)) => return ("RunLog", Some(run.addr.as_str())),
        Some(SessionRecordKind::LockHolder { .. }) => "LockHolder",
        Some(SessionRecordKind::Unknown) | None => "Unknown",
    };
    (name, None)
}

/// [`kind_and_addr`] for a build event: its wire tag and target.
pub fn event_kind_and_addr(kind: &BuildEventKind) -> (&'static str, Option<&str>) {
    use BuildEventKind as K;
    match kind {
        K::RequestConfig { .. } => ("RequestConfig", None),
        K::Matched { .. } => ("Matched", None),
        K::SelectionIncomplete { .. } => ("SelectionIncomplete", None),
        K::ResultStart { addr } => ("ResultStart", Some(addr)),
        K::ResultOutputs { addr, .. } => ("ResultOutputs", Some(addr)),
        K::ResultEnd { addr, .. } => ("ResultEnd", Some(addr)),
        K::ExecuteStart { addr, .. } => ("ExecuteStart", Some(addr)),
        K::ExecuteEnd { addr, .. } => ("ExecuteEnd", Some(addr)),
        K::LocalCacheHit { addr } => ("LocalCacheHit", Some(addr)),
        K::LocalCacheMiss { addr } => ("LocalCacheMiss", Some(addr)),
        K::CacheOutcome { addr, .. } => ("CacheOutcome", Some(addr)),
        K::CacheHitRebuilt { addr, .. } => ("CacheHitRebuilt", Some(addr)),
        K::LocalCacheWriteStart { addr } => ("LocalCacheWriteStart", Some(addr)),
        K::LocalCacheWriteEnd { addr, .. } => ("LocalCacheWriteEnd", Some(addr)),
        K::ResultLockWaitStart { addr, .. } => ("ResultLockWaitStart", Some(addr)),
        K::ResultLockWaitEnd { addr } => ("ResultLockWaitEnd", Some(addr)),
        K::AdmissionWaitStart { addr, .. } => ("AdmissionWaitStart", Some(addr)),
        K::AdmissionWaitEnd { addr } => ("AdmissionWaitEnd", Some(addr)),
        K::PoolUsage { .. } => ("PoolUsage", None),
        K::ScratchLockWaitStart { addr, .. } => ("ScratchLockWaitStart", Some(addr)),
        K::ScratchLockWaitEnd { addr, .. } => ("ScratchLockWaitEnd", Some(addr)),
        K::ScratchPrepareStart { addr, .. } => ("ScratchPrepareStart", Some(addr)),
        K::ScratchPrepareEnd { addr, .. } => ("ScratchPrepareEnd", Some(addr)),
        K::RemoteCacheHit { addr, .. } => ("RemoteCacheHit", Some(addr)),
        K::RemoteCacheMiss { addr, .. } => ("RemoteCacheMiss", Some(addr)),
        K::RemoteCacheReadStart { addr, .. } => ("RemoteCacheReadStart", Some(addr)),
        K::RemoteCacheReadEnd { addr, .. } => ("RemoteCacheReadEnd", Some(addr)),
        K::RemoteCacheWriteStart { addr, .. } => ("RemoteCacheWriteStart", Some(addr)),
        K::RemoteCacheWriteEnd { addr, .. } => ("RemoteCacheWriteEnd", Some(addr)),
        K::GcTargetSwept { .. } => ("GcTargetSwept", None),
        K::GcFailureLogsSwept { .. } => ("GcFailureLogsSwept", None),
        K::GcCacheDb { .. } => ("GcCacheDb", None),
        K::CodegenExcluded { addr, .. } => ("CodegenExcluded", addr.as_deref()),
        K::CodegenImported { .. } => ("CodegenImported", None),
        K::Unknown => ("Unknown", None),
    }
}

/// The owner's write side: one connection, one open transaction per flush
/// tick, the explorer rows derived as records land.
pub(crate) struct Writer {
    conn: Connection,
    in_tx: bool,
    next_seq: i64,
    deriver: Deriver,
    deriving: bool,
    rows: Vec<Row>,
    /// Bytes of explorer rows written, toward the ceiling.
    pub(crate) explorer_bytes: u64,
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("next_seq", &self.next_seq)
            .finish_non_exhaustive()
    }
}

impl Writer {
    pub(crate) fn new(conn: Connection, session_id: SessionId) -> Self {
        Self {
            conn,
            in_tx: false,
            next_seq: 1,
            deriver: Deriver::new(&session_id.encode(), session_id.started_unix_ms()),
            deriving: true,
            rows: Vec::new(),
            explorer_bytes: 0,
        }
    }

    fn begin(&mut self) -> rusqlite::Result<()> {
        if !self.in_tx {
            self.conn.execute_batch("BEGIN")?;
            self.in_tx = true;
        }
        Ok(())
    }

    /// Insert one record (its JSON `body` already encoded) and the explorer
    /// rows it completes. Returns the record's `seq`.
    pub(crate) fn insert(&mut self, record: &SessionRecord, body: &str) -> rusqlite::Result<i64> {
        self.begin()?;
        let seq = self.next_seq;
        let (kind, addr) = kind_and_addr(record);
        self.conn
            .prepare_cached(
                "INSERT INTO records (seq, at_ms, kind, addr, body) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                seq,
                i64::try_from(record.at_unix_ms).unwrap_or(i64::MAX),
                kind,
                addr,
                body
            ])?;
        self.next_seq += 1;
        if self.deriving {
            self.deriver.apply(
                record.at_unix_ms,
                record.kind.as_ref(),
                record.session.as_ref(),
                body,
                &mut self.rows,
            );
            self.flush_rows()?;
        }
        Ok(seq)
    }

    /// Stop deriving explorer rows: the recording reached its ceiling, and
    /// records stop at the same record (I4).
    pub(crate) fn stop_deriving(&mut self, at_seq: i64) -> rusqlite::Result<()> {
        self.deriving = false;
        self.begin()?;
        self.conn.execute(
            "UPDATE session SET truncated = 1, truncated_seq = ?1",
            params![at_seq],
        )?;
        Ok(())
    }

    fn flush_rows(&mut self) -> rusqlite::Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let bytes = write_rows(&self.conn, self.deriver.session_id(), &mut self.rows)?;
        self.explorer_bytes = self.explorer_bytes.saturating_add(bytes);
        Ok(())
    }

    /// Upsert the `status` row inside the open transaction.
    pub(crate) fn upsert_status(&mut self, at_ms: u64, snapshot: &str) -> rusqlite::Result<()> {
        self.begin()?;
        self.conn
            .prepare_cached(
                "INSERT INTO status (id, at_ms, snapshot) VALUES (1, ?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET at_ms = excluded.at_ms, snapshot = excluded.snapshot",
            )?
            .execute(params![i64::try_from(at_ms).unwrap_or(i64::MAX), snapshot])?;
        Ok(())
    }

    /// The `seq` the next inserted record gets.
    pub(crate) fn next_seq(&self) -> i64 {
        self.next_seq
    }

    /// Commit the open transaction, if any.
    pub(crate) fn commit(&mut self) -> rusqlite::Result<()> {
        if self.in_tx {
            self.in_tx = false;
            self.conn.execute_batch("COMMIT")?;
        }
        Ok(())
    }

    /// Roll back the open transaction after a failed write, so nothing half
    /// a batch lands.
    pub(crate) fn rollback(&mut self) {
        if self.in_tx {
            self.in_tx = false;
            if let Err(e) = self.conn.execute_batch("ROLLBACK") {
                tracing::debug!(error = %e, "rolling back the session's batch");
            }
        }
    }

    /// The end of the run: the session-wide rows, the session row's end
    /// fields and `sealed=1`, in the last WAL transaction. The caller then
    /// [`seal`]s the connection.
    ///
    /// The session-wide rows are derived even past the ceiling, where
    /// per-record derivation stopped: [`rederive`] always derives them, and
    /// the two must agree (I4).
    pub(crate) fn finish(
        mut self,
        ended_ms: u64,
        exit_code: Option<i32>,
        panicked: bool,
    ) -> rusqlite::Result<Connection> {
        self.begin()?;
        self.deriver.finish(ended_ms, &mut self.rows);
        self.flush_rows()?;
        self.conn.execute(
            "UPDATE session SET ended_ms = ?1, exit_code = ?2, panicked = ?3, sealed = 1",
            params![
                i64::try_from(ended_ms).unwrap_or(i64::MAX),
                exit_code,
                panicked
            ],
        )?;
        self.commit()?;
        Ok(self.conn)
    }
}

/// Insert derived rows; the bytes they add, roughly, toward the ceiling.
fn write_rows(conn: &Connection, session_id: &str, rows: &mut Vec<Row>) -> rusqlite::Result<u64> {
    let mut bytes = 0u64;
    for row in rows.drain(..) {
        bytes = bytes.saturating_add(row.approx_bytes());
        match row {
            Row::Span(s) => {
                conn.prepare_cached(
                    "INSERT INTO spans (session_id, span_id, parent_span_id, name, class, addr,
                       driver, outcome, start_us, end_us, duration_us, status, attributes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                )?
                .execute(params![
                    session_id,
                    s.span_id,
                    s.parent_span_id,
                    s.name.as_ref(),
                    s.class,
                    s.addr,
                    s.driver,
                    s.outcome,
                    s.start_us,
                    s.end_us,
                    s.end_us.saturating_sub(s.start_us),
                    s.status,
                    s.attributes,
                ])?;
            }
            Row::Event(e) => {
                conn.prepare_cached(
                    "INSERT INTO span_events (session_id, span_id, name, time_us, attributes)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(params![
                    session_id,
                    e.span_id,
                    e.name,
                    e.time_us,
                    e.attributes
                ])?;
            }
            Row::Metric(m) => {
                conn.prepare_cached(
                    "INSERT INTO metrics (session_id, name, kind, unit, time_us, value, attributes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?
                .execute(params![
                    session_id,
                    m.name,
                    m.kind,
                    m.unit,
                    m.time_us,
                    m.value,
                    m.attributes
                ])?;
            }
        }
    }
    Ok(bytes)
}

/// Derive the explorer tables again from `records` and the session row
/// alone, replacing what is there (I4). Stops where the recording's ceiling
/// stopped derivation (`truncated_seq`). A session command's repair (D14);
/// a build never calls it (I1).
pub fn rederive(conn: &Connection) -> rusqlite::Result<()> {
    let (id, started, ended, stop): (String, i64, Option<i64>, Option<i64>) = conn.query_row(
        "SELECT id, started_ms, ended_ms, truncated_seq FROM session",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    conn.execute_batch("DELETE FROM spans; DELETE FROM span_events; DELETE FROM metrics;")?;
    let mut deriver = Deriver::new(&id, u64::try_from(started).unwrap_or(0));
    let mut rows = Vec::new();
    let mut last_at = u64::try_from(started).unwrap_or(0);
    {
        let mut stmt = conn.prepare("SELECT seq, at_ms, body FROM records ORDER BY seq")?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let seq: i64 = r.get(0)?;
            if stop.is_some_and(|s| seq >= s) {
                break;
            }
            let body: String = r.get(2)?;
            let Ok(record) = serde_json::from_str::<SessionRecord>(&body) else {
                continue;
            };
            last_at = record.at_unix_ms;
            deriver.apply(
                record.at_unix_ms,
                record.kind.as_ref(),
                record.session.as_ref(),
                &body,
                &mut rows,
            );
            write_rows(conn, &id, &mut rows)?;
        }
    }
    let end = ended.and_then(|e| u64::try_from(e).ok()).unwrap_or(last_at);
    deriver.finish(end, &mut rows);
    write_rows(conn, &id, &mut rows)?;
    conn.execute("UPDATE session SET derivation = ?1", params![DERIVATION])?;
    Ok(())
}

/// How a seal ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sealed {
    /// Rollback mode, no `-wal`: one self-contained file.
    Done,
    /// A reader kept the checkpoint or the mode switch from finishing within
    /// the bound: the file stays in WAL mode, and the next cleanup seals it.
    CutShort,
}

/// Fold the WAL into the file and switch it to rollback mode, then close.
/// Shared by the owner, cleanup and re-derivation (D16). The caller has
/// written `sealed=1` in the last WAL transaction; nothing writes the file
/// after the switch. Checks the checkpoint's busy column and the mode
/// `journal_mode` returns, retrying while a viewer is in the way until
/// `timeout`.
///
/// A viewer merely attached (a connection open, no read in progress) lets
/// the checkpoint finish but makes the mode switch fail with `SQLITE_BUSY`:
/// that is retried like a busy checkpoint, not returned, so a follower that
/// closes on `End` lets the seal finish.
pub fn seal(conn: Connection, timeout: Duration) -> rusqlite::Result<Sealed> {
    let deadline = Instant::now() + timeout;
    // This loop is the wait: SQLite's busy handler would overrun `timeout`.
    conn.busy_timeout(Duration::ZERO)?;
    loop {
        match seal_once(&conn) {
            Ok(true) => {
                drop(conn);
                return Ok(Sealed::Done);
            }
            Ok(false) => {}
            Err(e)
                if matches!(
                    e.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ) => {}
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            return Ok(Sealed::CutShort);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One attempt at [`seal`]: `true` once the file is in rollback mode.
fn seal_once(conn: &Connection) -> rusqlite::Result<bool> {
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    if busy != 0 {
        return Ok(false);
    }
    let mode: String = conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    Ok(mode.eq_ignore_ascii_case("delete"))
}

/// Seal a lost session's recording (cleanup; the caller holds its lock
/// exclusively). Only a known format; never creates the file; refuses beside
/// a `-journal`.
///
/// It runs during a build, so it repairs nothing (I1, D14): one small
/// transaction (the end time, `sealed`, and [`STALE_DERIVATION`] in place of
/// the derivation, since a crash never wrote the session-wide rows), then the
/// checkpoint and mode switch. The explorer rows stay as the crash left them
/// until a session command re-derives the recording.
pub(crate) fn seal_lost(path: &Path) -> anyhow::Result<Sealed> {
    let header = read_header(path)
        .with_context(|| format!("reading {}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("{} is not a heph recording", path.display()))?;
    if !header.is_known() {
        anyhow::bail!("{} is in an unknown format", path.display());
    }
    if sibling(path, "-journal").exists() {
        anyhow::bail!("{} has a rollback journal beside it", path.display());
    }
    if !header.wal {
        return Ok(Sealed::Done);
    }
    let conn = open_existing_rw(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let sealed: bool = conn.query_row("SELECT sealed FROM session", [], |r| r.get(0))?;
    if !sealed {
        // Its last record's time stands in for the end a crash never wrote.
        conn.execute(
            "UPDATE session SET ended_ms = (SELECT max(at_ms) FROM records) WHERE ended_ms IS NULL",
            [],
        )?;
        conn.execute(
            "UPDATE session SET sealed = 1, derivation = ?1",
            params![STALE_DERIVATION],
        )?;
    }
    conn.execute_batch("COMMIT")?;
    Ok(seal(conn, SEAL_TIMEOUT)?)
}

/// Reads records back in `seq` order from a recording, live or sealed.
#[derive(Debug)]
pub struct RecordCursor {
    opened: Opened,
    last: i64,
    skipped: u64,
    buf: VecDeque<SessionRecord>,
    data_version: Option<i64>,
    exhausted: bool,
}

const PAGE: i64 = 512;

impl RecordCursor {
    pub fn new(opened: Opened) -> Self {
        Self {
            opened,
            last: 0,
            skipped: 0,
            buf: VecDeque::new(),
            data_version: None,
            exhausted: false,
        }
    }

    /// Start after everything recorded so far: only later records are read.
    pub fn skip_to_end(&mut self) -> rusqlite::Result<()> {
        self.last = last_seq(&self.opened.conn)?;
        Ok(())
    }

    /// Start after record `seq`: only records written after it are read.
    pub fn start_after(&mut self, seq: i64) {
        self.last = seq;
        self.buf.clear();
        self.exhausted = false;
    }

    pub fn conn(&self) -> &Connection {
        &self.opened.conn
    }

    pub fn is_live(&self) -> bool {
        self.opened.live
    }

    /// The next record, or `None` when everything committed so far was read.
    /// A body that does not decode is skipped and counted.
    pub fn next_record(&mut self) -> io::Result<Option<SessionRecord>> {
        self.next_where(None)
    }

    /// The next record of `kind` (the `kind` column), skipping the rest
    /// without decoding them.
    pub fn next_of_kind(&mut self, kind: &str) -> io::Result<Option<SessionRecord>> {
        self.next_where(Some(kind))
    }

    fn next_where(&mut self, kind: Option<&str>) -> io::Result<Option<SessionRecord>> {
        if let Some(r) = self.buf.pop_front() {
            return Ok(Some(r));
        }
        if self.exhausted && !self.changed().map_err(io::Error::other)? {
            return Ok(None);
        }
        // A full page can decode to nothing (every body skipped): read on
        // until a record turns up or the table runs out.
        loop {
            self.fill(kind).map_err(io::Error::other)?;
            if let Some(r) = self.buf.pop_front() {
                return Ok(Some(r));
            }
            if self.exhausted {
                return Ok(None);
            }
        }
    }

    /// Whether another connection committed since the last read.
    fn changed(&mut self) -> rusqlite::Result<bool> {
        let v: i64 = self
            .opened
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?;
        let changed = self.data_version != Some(v);
        self.data_version = Some(v);
        Ok(changed)
    }

    fn fill(&mut self, kind: Option<&str>) -> rusqlite::Result<()> {
        if self.data_version.is_none() {
            self.changed()?;
        }
        let sql = if kind.is_some() {
            "SELECT seq, body FROM records WHERE seq > ?1 AND kind = ?3 ORDER BY seq LIMIT ?2"
        } else {
            "SELECT seq, body FROM records WHERE seq > ?1 AND ?3 IS NULL ORDER BY seq LIMIT ?2"
        };
        let mut stmt = self.opened.conn.prepare_cached(sql)?;
        let mut rows = stmt.query(params![self.last, PAGE, kind])?;
        let mut n = 0;
        while let Some(row) = rows.next()? {
            n += 1;
            self.last = row.get(0)?;
            let body: rusqlite::types::ValueRef<'_> = row.get_ref(1)?;
            let decoded = body
                .as_str()
                .ok()
                .and_then(|b| serde_json::from_str::<SessionRecord>(b).ok());
            match decoded {
                Some(r) if r.kind.is_some() || r.session.is_some() => self.buf.push_back(r),
                Some(_) => {}
                None => self.skipped += 1,
            }
        }
        self.exhausted = n < PAGE;
        Ok(())
    }

    /// Bodies that did not decode, so far.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }
}
