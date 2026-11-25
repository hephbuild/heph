//! `catalog.db`: what ran this week (and up to 90 days back) without
//! opening every recording.
//!
//! One row per session in `sessions`, every resolved target in `targets`
//! (stored through `addrs`, so a row is ~16 bytes), and the explorer rows of
//! what *ran*: the session's own spans, the `heph.result` spans of targets
//! that were not cache hits with their phase spans, and every metric point.
//!
//! The catalog holds nothing a recording does not, so it can always be
//! rebuilt from the recordings still kept (I8). Every write copies from a
//! recording by the same SQL ([`copy_session`]), whether the owner writes at
//! start or seal, a command heals a dropped write, or a repair rebuilds it.
//!
//! Locking: every catalog connection holds `catalog.lock` shared for its
//! whole life; a repair takes it exclusively. An owner that cannot take it at
//! once drops its write, and any owner write that fails or passes
//! [`WRITE_BOUND`] is dropped with a `debug!`: it never fails or slows the
//! run past that bound (I1). A build never repairs anything.

use std::collections::{HashMap, HashSet};
use std::fs::{DirBuilder, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use rusqlite::config::DbConfig;
use rusqlite::{Connection, ErrorCode, OpenFlags, params};

use super::id::SessionId;
use super::{lockfile, store};

/// The catalog, in the sessions root.
pub const FILE: &str = "catalog.db";
/// Held shared by every catalog connection, exclusively by a repair.
pub const LOCK: &str = "catalog.lock";
/// What a corrupt catalog (and its `-wal`, `-shm`) is moved aside to,
/// followed by the unix-ms of the move.
pub const CORRUPT_PREFIX: &str = "catalog.db.corrupt-";
/// `application_id` of a catalog: `HEPC`.
pub const APPLICATION_ID: u32 = 0x4845_5043;
/// The catalog's own format (`user_version`).
pub const FORMAT: u32 = 1;
/// SQLite's wait on a locked catalog.
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(500);
/// The bound on one whole owner write.
pub const WRITE_BOUND: Duration = Duration::from_secs(1);
/// How long a catalog row is kept.
pub const KEEP: Duration = Duration::from_secs(90 * DAY_S);
/// How long a moved-aside corrupt catalog is kept.
pub const CORRUPT_KEEP: Duration = Duration::from_secs(7 * DAY_S);
/// The catalog's size cap.
pub const CAP: u64 = 1024 * 1024 * 1024;
/// How often a recording whose heal failed is tried again.
pub const HEAL_RETRY: Duration = Duration::from_secs(DAY_S);
/// How long a command waits for `catalog.lock` (a repair elsewhere).
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// Pages one cleanup `incremental_vacuum` gives back (4 MiB at 4 KiB
/// pages): one short write transaction, not the whole freelist in one.
const VACUUM_STEP: u32 = 1024;
/// Cleanup's pause between two of its write transactions: longer than an
/// owner's busy-handler poll, so an owner waiting on the catalog gets in.
const CLEAN_YIELD: Duration = Duration::from_millis(10);
const DAY_S: u64 = 24 * 60 * 60;
const DAY_MS: u64 = DAY_S * 1000;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS info (id INTEGER PRIMARY KEY CHECK (id = 1), heph_version TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (
  sid INTEGER PRIMARY KEY, session_id TEXT NOT NULL UNIQUE,
  schema_version INTEGER, format INTEGER, heph_version TEXT, owner_pid INTEGER, host TEXT, user TEXT,
  actor_kind TEXT, actor_name TEXT, ci_run_url TEXT, command TEXT, argv TEXT, cwd TEXT,
  workspace_root TEXT, home TEXT, git_head TEXT, label TEXT, interactive INTEGER,
  started_ms INTEGER NOT NULL, ended_ms INTEGER, exit_code INTEGER, panicked INTEGER,
  truncated INTEGER, truncated_seq INTEGER, sealed INTEGER, recorded INTEGER NOT NULL, snapshot INTEGER,
  complete INTEGER NOT NULL DEFAULT 0, derivation INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_started ON sessions (started_ms);
CREATE TABLE IF NOT EXISTS addrs (id INTEGER PRIMARY KEY, addr TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS target_rows (
  sid INTEGER NOT NULL, addr_id INTEGER NOT NULL, outcome, duration_us INTEGER,
  PRIMARY KEY (sid, addr_id)
) WITHOUT ROWID;
CREATE VIEW IF NOT EXISTS targets AS
  SELECT s.session_id AS session_id, a.addr AS addr,
    CASE t.outcome WHEN 0 THEN 'hit' WHEN 1 THEN 'ran' WHEN 2 THEN 'failed'
      WHEN 3 THEN 'cancelled' ELSE t.outcome END AS outcome,
    t.duration_us AS duration_us
  FROM target_rows t JOIN sessions s ON s.sid = t.sid JOIN addrs a ON a.id = t.addr_id;
CREATE TABLE IF NOT EXISTS spans (
  session_id TEXT, span_id INTEGER, parent_span_id INTEGER,
  name TEXT, class TEXT, addr TEXT, driver TEXT, outcome TEXT,
  start_us INTEGER, end_us INTEGER, duration_us INTEGER,
  status TEXT, attributes TEXT,
  PRIMARY KEY (session_id, span_id)
);
CREATE TABLE IF NOT EXISTS span_events (session_id TEXT, span_id INTEGER, name TEXT, time_us INTEGER, attributes TEXT);
CREATE INDEX IF NOT EXISTS span_events_session ON span_events (session_id, span_id);
CREATE TABLE IF NOT EXISTS metrics (session_id TEXT, name TEXT, kind TEXT, unit TEXT, time_us INTEGER, value REAL, attributes TEXT);
CREATE INDEX IF NOT EXISTS metrics_session ON metrics (session_id);
CREATE TABLE IF NOT EXISTS heal_failures (session_id TEXT PRIMARY KEY, at_ms INTEGER NOT NULL);
";

/// The `sessions` columns a copy writes, in order.
const SESSION_COLUMNS: &[&str] = &[
    "session_id",
    "schema_version",
    "format",
    "heph_version",
    "owner_pid",
    "host",
    "user",
    "actor_kind",
    "actor_name",
    "ci_run_url",
    "command",
    "argv",
    "cwd",
    "workspace_root",
    "home",
    "git_head",
    "label",
    "interactive",
    "started_ms",
    "ended_ms",
    "exit_code",
    "panicked",
    "truncated",
    "truncated_seq",
    "sealed",
    "recorded",
    "snapshot",
    "complete",
    "derivation",
];

/// The recording's `session` row as [`SESSION_COLUMNS`]; `?1` is
/// `recorded`, `?2` `complete`.
const SESSION_SELECT: &str = "SELECT id, 1, format, heph_version, owner_pid, host, user, \
    actor_kind, actor_name, ci_run_url, command, argv, cwd, workspace_root, home, git_head, \
    label, interactive, started_ms, ended_ms, exit_code, panicked, truncated, truncated_seq, \
    sealed, ?1, snapshot, ?2, derivation FROM rec.session WHERE true";

/// One row per resolved target of the attached recording, as its `targets`
/// view picks it — spelled out against `rec.spans`, so no view of the file
/// runs.
const REC_TARGETS: &str = "SELECT addr, outcome, duration_us FROM (
    SELECT addr, outcome, duration_us, row_number() OVER (
      PARTITION BY addr
      ORDER BY CASE outcome WHEN 'failed' THEN 0 WHEN 'ran' THEN 1
        WHEN 'cancelled' THEN 2 WHEN 'hit' THEN 3 ELSE 4 END, span_id
    ) AS n
    FROM rec.spans WHERE name = 'heph.result' AND addr IS NOT NULL
  ) WHERE n = 1";

/// The result spans the catalog keeps: every target that was not a hit.
const RAN_RESULTS: &str =
    "SELECT span_id FROM rec.spans WHERE name = 'heph.result' AND coalesce(outcome, '') <> 'hit'";

/// What an owner writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rows {
    /// The `sessions` row with `complete=0`, unless one is there.
    Start,
    /// Every row of the session, replaced, with `complete=1`. `recorded`
    /// false: a private session (`HEPH_NO_SESSION`), which keeps no
    /// recording, writes only `sessions` and `targets`. `explorer` false: an
    /// owner's write into a catalog over its cap, which keeps the recording
    /// but writes only `sessions` and `targets` too.
    Full { recorded: bool, explorer: bool },
}

impl Rows {
    /// Every row of a kept recording: what a command's heal or replace
    /// writes.
    const ALL: Self = Self::Full {
        recorded: true,
        explorer: true,
    };
}

/// Copy the recording at `rec` into the catalog on `conn`: one transaction,
/// so a heal racing the owner's own write never doubles a row.
///
/// Attached `immutable` only when its header says rollback mode: a live
/// recording, or one whose seal a reader cut short, has its last
/// transactions (`End`, `sealed=1`, the derived rows) in the WAL, which an
/// immutable attach never reads.
fn copy_session(conn: &Connection, rec: &Path, copy: Rows) -> rusqlite::Result<()> {
    let sealed = matches!(store::read_header(rec), Ok(Some(h)) if !h.wal);
    let query = if sealed {
        "mode=ro&immutable=1"
    } else {
        "mode=ro"
    };
    conn.execute("ATTACH DATABASE ?1 AS rec", [store::file_uri(rec, query)])?;
    let res = copy_attached(conn, copy);
    if res.is_err() && !conn.is_autocommit() {
        rollback(conn);
    }
    let detached = conn.execute_batch("DETACH DATABASE rec");
    res.and(detached)
}

/// Roll back a failed transaction; the failure that caused it is the error
/// that matters.
fn rollback(conn: &Connection) {
    if let Err(e) = conn.execute_batch("ROLLBACK") {
        tracing::debug!(error = %e, "cannot roll back a catalog transaction");
    }
}

/// Every catalog table is named `main.`: the recording attached as `rec`
/// has views of the same names, and a connection whose schema is stale (it
/// read the catalog empty before another one made it) resolves a bare name
/// into `rec`. A qualified name that is not found instead makes SQLite
/// reload the schema and prepare again.
fn copy_attached(conn: &Connection, copy: Rows) -> rusqlite::Result<()> {
    let columns = SESSION_COLUMNS.join(", ");
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match copy {
        Rows::Start => {
            conn.execute(
                &format!(
                    "INSERT INTO main.sessions ({columns}) {SESSION_SELECT} ON CONFLICT(session_id) DO NOTHING"
                ),
                params![1, 0],
            )?;
        }
        Rows::Full { recorded, explorer } => {
            let set = SESSION_COLUMNS
                .iter()
                .skip(1)
                .map(|c| format!("{c} = excluded.{c}"))
                .collect::<Vec<_>>()
                .join(", ");
            conn.execute(
                &format!(
                    "INSERT INTO main.sessions ({columns}) {SESSION_SELECT} ON CONFLICT(session_id) DO UPDATE SET {set}"
                ),
                params![i64::from(recorded), 1],
            )?;
            let (sid, id): (i64, String) = conn.query_row(
                "SELECT sid, session_id FROM main.sessions WHERE session_id = (SELECT id FROM rec.session)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            conn.execute("DELETE FROM main.target_rows WHERE sid = ?1", [sid])?;
            conn.execute_batch(&format!(
                "INSERT OR IGNORE INTO main.addrs (addr) SELECT addr FROM ({REC_TARGETS})"
            ))?;
            conn.execute(
                &format!(
                    "INSERT INTO main.target_rows (sid, addr_id, outcome, duration_us)
                     SELECT ?1, a.id, CASE t.outcome WHEN 'hit' THEN 0 WHEN 'ran' THEN 1
                       WHEN 'failed' THEN 2 WHEN 'cancelled' THEN 3 ELSE t.outcome END,
                       t.duration_us
                     FROM ({REC_TARGETS}) t JOIN main.addrs a ON a.addr = t.addr"
                ),
                [sid],
            )?;
            for table in ["spans", "span_events", "metrics"] {
                conn.execute(
                    &format!("DELETE FROM main.{table} WHERE session_id = ?1"),
                    [&id],
                )?;
            }
            if recorded && explorer {
                conn.execute_batch(&format!(
                    "INSERT INTO main.spans SELECT session_id, span_id, parent_span_id, name, class,
                       addr, driver, outcome, start_us, end_us, duration_us, status, attributes
                     FROM rec.spans WHERE addr IS NULL OR span_id IN ({RAN_RESULTS})
                       OR parent_span_id IN ({RAN_RESULTS})"
                ))?;
                conn.execute(
                    "INSERT INTO main.span_events SELECT session_id, span_id, name, time_us, attributes
                     FROM rec.span_events
                     WHERE span_id IN (SELECT span_id FROM main.spans WHERE session_id = ?1)",
                    [&id],
                )?;
                conn.execute_batch(
                    "INSERT INTO main.metrics SELECT session_id, name, kind, unit, time_us, value,
                       attributes FROM rec.metrics",
                )?;
            }
            conn.execute(
                "DELETE FROM main.heal_failures WHERE session_id = ?1",
                [&id],
            )?;
        }
    }
    conn.execute_batch("COMMIT")
}

/// Why a catalog cannot be used as it is.
#[derive(Debug)]
enum Unusable {
    /// Written by a newer heph (`user_version`), named when it said.
    Newer { writer: Option<String> },
    /// Not a heph catalog.
    Foreign,
    /// A heph catalog in an older format (`user_version`): never stamped
    /// current over its old tables, moved aside and rebuilt like a corrupt
    /// one.
    Older { version: u32 },
    /// Any SQLite error: [`is_corrupt`] says whether it means corrupt.
    Sql(rusqlite::Error),
}

impl Unusable {
    /// Why a repair moves it aside and rebuilds it; `None`: it is left as
    /// it is.
    fn rebuilt_because(&self) -> Option<String> {
        match self {
            Self::Older { version } => Some(format!("was in an older format ({version})")),
            Self::Sql(e) if is_corrupt(e) => Some("was corrupt".to_owned()),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Unusable {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Newer { writer: Some(w) } => {
                write!(f, "it was written by heph {w}, newer than this one")
            }
            Self::Newer { writer: None } => write!(f, "it was written by a newer heph"),
            Self::Foreign => write!(f, "it is not a heph session catalog"),
            Self::Older { version } => write!(f, "it is in an older format ({version})"),
            Self::Sql(e) => write!(f, "{e}"),
        }
    }
}

/// Only `SQLITE_CORRUPT` and `SQLITE_NOTADB` mean corrupt: BUSY, FULL, a
/// permission error or anything else never moves a catalog aside.
fn is_corrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

/// Open `root`'s catalog read-write, creating it (`0600`) when `create`.
fn open_rw(root: &Path, create: bool) -> rusqlite::Result<Connection> {
    let path = root.join(FILE);
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    if create {
        // Made here, so it is user-only from its first byte; SQLite gives its
        // -wal and -shm the same mode. An empty file is an empty database.
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            tracing::debug!(path = %path.display(), error = %e, "cannot create the session catalog");
        }
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let conn = Connection::open_with_flags(&path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// What [`classify`] found a usable catalog to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// This heph's format.
    Ready,
    /// No table yet: the schema is to be made.
    Empty,
}

/// Read the header and the table count in the caller's transaction, so both
/// are one snapshot: a catalog another process made between two separate
/// reads looked foreign (no application id, tables).
///
/// Another application's id is foreign whatever its `user_version`; heph's
/// with a newer one is newer; with an older one and tables, older.
fn classify(conn: &Connection) -> Result<Shape, Unusable> {
    let app: u32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if app == APPLICATION_ID {
        match version.cmp(&FORMAT) {
            std::cmp::Ordering::Equal => return Ok(Shape::Ready),
            std::cmp::Ordering::Greater => {
                let writer = conn
                    .query_row("SELECT heph_version FROM info", [], |r| r.get(0))
                    .ok();
                return Err(Unusable::Newer { writer });
            }
            std::cmp::Ordering::Less => {}
        }
    } else if app != 0 {
        return Err(Unusable::Foreign);
    }
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
    match (tables, app) {
        (0, _) => Ok(Shape::Empty),
        (_, APPLICATION_ID) => Err(Unusable::Older { version }),
        _ => Err(Unusable::Foreign),
    }
}

/// Check the catalog's header and create its schema when it is new.
fn ensure_schema(conn: &Connection, in_memory: bool) -> Result<(), Unusable> {
    conn.execute_batch("BEGIN")?;
    let shape = classify(conn);
    if !conn.is_autocommit() {
        // A read: ending it is all a rollback does.
        rollback(conn);
    }
    if shape? == Shape::Ready {
        return Ok(());
    }
    if !in_memory {
        // Before the first table, and before WAL mode, which writes the
        // header and leaves `auto_vacuum` at NONE: a cap enforced by deleting
        // rows must be able to give the pages back.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Unusable::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some(format!("the catalog's filesystem refused WAL ({mode})")),
            )));
        }
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let made = (|| -> Result<(), Unusable> {
        // Another process may have made it meanwhile.
        if classify(conn)? == Shape::Ready {
            return Ok(());
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "application_id", APPLICATION_ID)?;
        conn.pragma_update(None, "user_version", FORMAT)?;
        conn.execute(
            "INSERT OR REPLACE INTO info (id, heph_version) VALUES (1, ?1)",
            [hcore::version::current()],
        )?;
        Ok(())
    })();
    match made {
        Ok(()) => Ok(conn.execute_batch("COMMIT")?),
        Err(e) => {
            rollback(conn);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------- owner

/// The owner's start-of-session write: its `sessions` row with
/// `complete=0`, from the live recording in `dir`. Never fails the run.
pub fn record_start(root: &Path, dir: &Path) {
    owner_write(root, dir, Rows::Start, OwnerLimits::DEFAULT);
}

/// The owner's seal-time write: every row of its session, replaced, with
/// `complete=1`. `recorded` false for a private session, whose recording is
/// removed right after. Over [`CAP`], only `sessions` and `targets`: the
/// exit path does not grow a catalog the hourly cleanup has yet to trim.
/// Never fails the run.
pub fn record_seal(root: &Path, dir: &Path, recorded: bool) {
    record_seal_within(root, dir, recorded, OwnerLimits::DEFAULT);
}

/// What bounds an owner's write.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnerLimits {
    /// [`WRITE_BOUND`].
    pub(crate) bound: Duration,
    /// [`CAP`].
    pub(crate) cap: u64,
}

impl OwnerLimits {
    pub(crate) const DEFAULT: Self = Self {
        bound: WRITE_BOUND,
        cap: CAP,
    };
}

/// [`record_seal`] with `limits` in place of the defaults, so a test
/// reaches them with a small recording.
pub(crate) fn record_seal_within(root: &Path, dir: &Path, recorded: bool, limits: OwnerLimits) {
    let copy = Rows::Full {
        recorded,
        explorer: true,
    };
    owner_write(root, dir, copy, limits);
}

fn owner_write(root: &Path, dir: &Path, copy: Rows, limits: OwnerLimits) {
    match try_owner_write(root, dir, copy, limits) {
        Ok(true) => {}
        Ok(false) => tracing::debug!(
            root = %root.display(),
            "the session catalog is locked: this session's catalog write is dropped"
        ),
        Err(e) => tracing::debug!(
            root = %root.display(),
            error = %format!("{e:#}"),
            "this session's catalog write is dropped"
        ),
    }
}

/// Keep an owner's catalog write inside `deadline`, the run's exit path
/// waiting on it:
/// - its VM steps, by a progress handler that interrupts the statement;
/// - its waits on a locked catalog, by a busy handler that gives up at the
///   deadline rather than after a fixed timeout;
/// - its commit, by `synchronous=NORMAL` (no fsync on a WAL commit; the
///   catalog is rebuildable, I8), no auto-checkpoint, and no checkpoint when
///   it closes: the next session's start ([`fold_wal`]), a command's
///   connection or the next cleanup checkpoints.
fn bound_owner(conn: &mut Connection, deadline: Instant) -> rusqlite::Result<()> {
    conn.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
    conn.busy_handler(Some(move |_tries: i32| {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(Duration::from_millis(5)));
        true
    }))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "wal_autocheckpoint", 0)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    Ok(())
}

/// `Ok(false)`: `catalog.lock` was not free at once (a repair). Past
/// `limits.bound`, the write is interrupted and its transaction rolled
/// back; past `limits.cap`, a full write leaves out the explorer rows.
fn try_owner_write(
    root: &Path,
    dir: &Path,
    mut copy: Rows,
    limits: OwnerLimits,
) -> anyhow::Result<bool> {
    let lock_path = root.join(LOCK);
    let lock = lockfile::open_shared_file(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    if !lockfile::lock_within(&lock, false, Duration::ZERO)
        .with_context(|| format!("locking {}", lock_path.display()))?
    {
        return Ok(false);
    }
    let deadline = Instant::now() + limits.bound;
    let mut conn = open_rw(root, true).context("opening the session catalog")?;
    bound_owner(&mut conn, deadline).context("bounding the catalog write")?;
    ensure_schema(&conn, false)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("checking the session catalog")?;
    if let Rows::Full { explorer, .. } = &mut copy
        && used_bytes(&conn).context("sizing the session catalog")? > limits.cap
    {
        tracing::debug!(
            root = %root.display(),
            "the session catalog is over its cap: this session's spans and metrics are left out"
        );
        *explorer = false;
    }
    let rec = dir.join(store::FILE);
    copy_session(&conn, &rec, copy)
        .with_context(|| format!("copying {} into the catalog", rec.display()))?;
    if copy == Rows::Start
        && let Err(e) = fold_wal(&conn, deadline)
    {
        tracing::debug!(
            root = %root.display(),
            error = %e,
            "the session catalog's WAL was not folded in"
        );
    }
    drop(conn);
    drop(lock);
    Ok(true)
}

/// Fold the catalog's WAL in at a session's start, so the WAL the owners
/// leave unchecked ([`bound_owner`]) stays about one session's writes long.
/// A passive checkpoint waits on no reader or writer; past `deadline`, it is
/// interrupted. Only the start thread runs it, off the build path: the
/// seal's exit path never checkpoints.
fn fold_wal(conn: &Connection, deadline: Instant) -> rusqlite::Result<()> {
    let Some(left) = deadline.checked_duration_since(Instant::now()) else {
        return Ok(());
    };
    // The progress handler is not called inside the checkpoint's one VM
    // step; an interrupt is, between the pages it copies.
    let interrupt = conn.get_interrupt_handle();
    let (done, wait) = mpsc::channel::<()>();
    std::thread::scope(|s| {
        let watchdog = std::thread::Builder::new()
            .name("heph-session-catalog-fold".to_owned())
            .spawn_scoped(s, move || {
                if wait.recv_timeout(left) == Err(RecvTimeoutError::Timeout) {
                    interrupt.interrupt();
                }
            });
        if let Err(e) = watchdog {
            tracing::debug!(error = %e, "cannot bound the catalog's WAL fold: skipped");
            return Ok(());
        }
        let folded = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        drop(done);
        folded
    })
}

// -------------------------------------------------------------- command

/// The catalog as a `heph session` command opens it, repaired: healed of
/// dropped writes, rebuilt when lost or corrupt, or — when it cannot be used
/// (busy, full, unwritable, newer) — an in-memory catalog filled from the
/// recordings kept, which the command answers from and says so.
#[derive(Debug)]
pub struct Catalog {
    pub conn: Connection,
    /// Held shared for the connection's life.
    _lock: Option<File>,
    /// What the command says on stderr, each a full line.
    pub notes: Vec<String>,
    /// Answering from an in-memory catalog filled from the recordings.
    pub in_memory: bool,
    pub heal: HealReport,
}

/// What a heal did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HealReport {
    /// Recordings whose rows were written.
    pub healed: usize,
    /// Recordings whose heal failed now (each tried again in a day).
    pub failed: usize,
    /// Recordings skipped because their heal failed within the day.
    pub deferred: usize,
    /// Recordings in a format this heph does not know, skipped.
    pub unknown: usize,
}

/// Open `root`'s catalog for one command, as of `now_ms`. Fails only when
/// not even an in-memory catalog can be made.
pub fn open(root: &Path, now_ms: u64) -> anyhow::Result<Catalog> {
    match open_on_disk(root, now_ms) {
        Ok(catalog) => Ok(catalog),
        Err(reason) => {
            tracing::debug!(root = %root.display(), reason, "answering without the session catalog");
            in_memory(root, now_ms, &reason)
        }
    }
}

/// An in-memory catalog of every kept recording.
fn in_memory(root: &Path, now_ms: u64, reason: &str) -> anyhow::Result<Catalog> {
    let conn = Connection::open_in_memory_with_flags(
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_MEMORY,
    )
    .context("opening an in-memory session catalog")?;
    ensure_schema(&conn, true)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("creating an in-memory session catalog")?;
    let heal = heal(&conn, root, now_ms).context("reading the recordings")?;
    Ok(Catalog {
        conn,
        _lock: None,
        notes: vec![format!(
            "heph: the session catalog cannot be used ({reason}): answering from the recordings kept\n"
        )],
        in_memory: true,
        heal,
    })
}

/// `Err`: why the command answers from the recordings instead.
fn open_on_disk(root: &Path, now_ms: u64) -> Result<Catalog, String> {
    let lock_path = root.join(LOCK);
    let lock = lockfile::open_shared_file(&lock_path).map_err(|e| e.to_string())?;
    if !lockfile::lock_within(&lock, false, LOCK_WAIT).map_err(|e| e.to_string())? {
        return Err("it is busy being repaired".to_owned());
    }
    let mut notes = Vec::new();
    // A rebuild's own heal: the command does not heal twice.
    let mut rebuilt = None;
    let path = root.join(FILE);
    let conn = if path.exists() {
        let opened = open_rw(root, false)
            .map_err(Unusable::Sql)
            .and_then(|c| ensure_schema(&c, false).map(|()| c));
        match opened {
            Ok(conn) => conn,
            Err(e) if e.rebuilt_because().is_some() => {
                repair(root, &lock, now_ms, &mut notes, &mut rebuilt)?
            }
            Err(e) => return Err(e.to_string()),
        }
    } else {
        repair(root, &lock, now_ms, &mut notes, &mut rebuilt)?
    };
    let (conn, heal) = match rebuilt.map_or_else(|| heal(&conn, root, now_ms), Ok) {
        Ok(h) => (conn, h),
        Err(e) if is_corrupt(&e) => {
            drop(conn);
            let conn = repair(root, &lock, now_ms, &mut notes, &mut rebuilt)?;
            (conn, rebuilt.unwrap_or_default())
        }
        Err(e) => return Err(e.to_string()),
    };
    Ok(Catalog {
        conn,
        _lock: Some(lock),
        notes,
        in_memory: false,
        heal,
    })
}

/// Repair a missing, corrupt or older-format catalog: drop the shared lock (a
/// shared to exclusive `flock` conversion is not atomic), take it exclusively
/// — once no other connection holds it — and check again; then move a
/// corrupt or older one aside (`-wal` and `-shm` before the file, under
/// [`CORRUPT_PREFIX`] so the sweep removes it), delete an orphan `-wal`, and
/// rebuild it from every kept recording. Ends holding the lock shared again.
fn repair(
    root: &Path,
    lock: &File,
    now_ms: u64,
    notes: &mut Vec<String>,
    rebuilt: &mut Option<HealReport>,
) -> Result<Connection, String> {
    lockfile::unlock(lock).map_err(|e| e.to_string())?;
    if !lockfile::lock_within(lock, true, LOCK_WAIT).map_err(|e| e.to_string())? {
        return Err("another command holds it".to_owned());
    }
    let path = root.join(FILE);
    let moved = if path.exists() {
        let still = open_rw(root, false)
            .map_err(Unusable::Sql)
            .and_then(|c| ensure_schema(&c, false).map(|()| c));
        match still {
            // Another command repaired it meanwhile.
            Ok(conn) => {
                downgrade(lock)?;
                return Ok(conn);
            }
            Err(e) => match e.rebuilt_because() {
                Some(why) => {
                    let to = move_aside(root, now_ms).map_err(|e| format!("{e:#}"))?;
                    Some((why, to))
                }
                None => return Err(e.to_string()),
            },
        }
    } else {
        None
    };
    for suffix in ["-wal", "-shm"] {
        let orphan = store::sibling(&path, suffix);
        match std::fs::remove_file(&orphan) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("removing {}: {e}", orphan.display())),
        }
    }
    let conn = open_rw(root, true).map_err(|e| e.to_string())?;
    ensure_schema(&conn, false).map_err(|e| e.to_string())?;
    let heal = heal(&conn, root, now_ms).map_err(|e| e.to_string())?;
    let oldest: Option<i64> = conn
        .query_row("SELECT min(started_ms) FROM sessions", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let covers = match oldest.and_then(|ms| u64::try_from(ms).ok()) {
        Some(ms) => {
            let days = now_ms.saturating_sub(ms).div_ceil(DAY_MS).max(1);
            format!(
                "it covers the last {days} day{}",
                if days == 1 { "" } else { "s" }
            )
        }
        None => "no recording is kept".to_owned(),
    };
    let what = match &moved {
        Some((why, to)) => format!("the session catalog {why}, moved to {}", to.display()),
        None => "the session catalog was missing".to_owned(),
    };
    notes.push(format!(
        "heph: {what}: rebuilt from {} recording{} kept; {covers}\n",
        heal.healed,
        if heal.healed == 1 { "" } else { "s" }
    ));
    *rebuilt = Some(heal);
    downgrade(lock)?;
    Ok(conn)
}

fn downgrade(lock: &File) -> Result<(), String> {
    match lockfile::lock_within(lock, false, LOCK_WAIT) {
        Ok(true) => Ok(()),
        Ok(false) => Err("cannot hold the catalog lock".to_owned()),
        Err(e) => Err(e.to_string()),
    }
}

/// Move a corrupt catalog aside: its `-wal` and `-shm` first, so no
/// connection pairs a fresh file with an old journal. Returns the new path
/// of the file.
fn move_aside(root: &Path, now_ms: u64) -> anyhow::Result<PathBuf> {
    let path = root.join(FILE);
    let to = root.join(format!("{CORRUPT_PREFIX}{now_ms}"));
    for suffix in ["-wal", "-shm"] {
        let from = store::sibling(&path, suffix);
        match std::fs::rename(&from, store::sibling(&to, suffix)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("moving {} aside", from.display())),
        }
    }
    std::fs::rename(&path, &to).with_context(|| format!("moving {} aside", path.display()))?;
    Ok(to)
}

/// Write the rows of every kept, sealed recording whose `sessions` row is
/// missing or has `complete=0`, or whose `derivation` is older than its
/// recording's (the recording was re-derived and the `replace` after it was
/// dropped). No watermark: a write dropped before a later session's
/// succeeded is still found. A recording whose heal fails is noted and tried
/// again at most once a [`HEAL_RETRY`]. `Err` only for a failure of the
/// catalog itself.
fn heal(conn: &Connection, root: &Path, now_ms: u64) -> rusqlite::Result<HealReport> {
    let mut report = HealReport::default();
    // Complete rows, by the derivation they were written with.
    let complete: HashMap<String, i64> = {
        let mut stmt =
            conn.prepare("SELECT session_id, derivation FROM sessions WHERE complete = 1")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let retry_after = i64::try_from(now_ms.saturating_sub(retry_ms())).unwrap_or(i64::MAX);
    let failed: HashSet<String> = {
        let mut stmt = conn.prepare("SELECT session_id FROM heal_failures WHERE at_ms > ?1")?;
        stmt.query_map([retry_after], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(root = %root.display(), error = %e, "cannot read the sessions to heal");
            return Ok(report);
        }
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(id) = name.to_str().filter(|s| SessionId::parse(s).is_ok()) else {
            continue;
        };
        let rec = entry.path().join(store::FILE);
        // Only a row older than this heph's derivation can be behind its
        // recording: one more small read for those, none for the rest.
        if let Some(&derivation) = complete.get(id)
            && (derivation >= i64::from(store::DERIVATION)
                || !recording_is_newer(conn, &rec, derivation))
        {
            continue;
        }
        if failed.contains(id) {
            report.deferred += 1;
            continue;
        }
        // Live, lost and not yet sealed, or gone: not ended, nothing to heal.
        let header = match store::read_header(&rec) {
            Ok(Some(h)) => h,
            Ok(None) | Err(_) => continue,
        };
        if header.wal {
            continue;
        }
        if !header.is_known() {
            report.unknown += 1;
            continue;
        }
        match copy_session(conn, &rec, Rows::ALL) {
            Ok(()) => report.healed += 1,
            Err(e) if catalog_failed(conn, &e) => return Err(e),
            Err(e) => {
                tracing::debug!(session = id, error = %e, "cannot heal a session's catalog rows");
                report.failed += 1;
                conn.execute(
                    "INSERT OR REPLACE INTO heal_failures (session_id, at_ms) VALUES (?1, ?2)",
                    params![id, i64::try_from(now_ms).unwrap_or(i64::MAX)],
                )?;
            }
        }
    }
    Ok(report)
}

/// Whether the sealed recording `rec` was derived by a newer derivation than
/// `than`. A recording that cannot be read is not: the row stays as it is.
fn recording_is_newer(conn: &Connection, rec: &Path, than: i64) -> bool {
    match store::read_header(rec) {
        Ok(Some(h)) if !h.wal && h.is_known() => {}
        _ => return false,
    }
    let read = || -> rusqlite::Result<i64> {
        conn.execute(
            "ATTACH DATABASE ?1 AS rec",
            [store::file_uri(rec, "mode=ro&immutable=1")],
        )?;
        let derivation = conn.query_row("SELECT derivation FROM rec.session", [], |r| r.get(0));
        let detached = conn.execute_batch("DETACH DATABASE rec");
        let derivation = derivation?;
        detached.map(|()| derivation)
    };
    match read() {
        Ok(d) => d > than,
        Err(e) => {
            tracing::debug!(recording = %rec.display(), error = %e, "cannot read a recording's derivation");
            false
        }
    }
}

fn retry_ms() -> u64 {
    u64::try_from(HEAL_RETRY.as_millis()).unwrap_or(u64::MAX)
}

/// Whether a failed copy is the catalog's failure rather than the
/// recording's: busy, full, read-only, an I/O error, or a catalog that no
/// longer reads.
fn catalog_failed(conn: &Connection, e: &rusqlite::Error) -> bool {
    match e.sqlite_error_code() {
        Some(
            ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked
            | ErrorCode::DiskFull
            | ErrorCode::ReadOnly
            | ErrorCode::SystemIoFailure,
        ) => true,
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => conn
            .query_row("SELECT count(*) FROM main.sqlite_schema", [], |r| {
                r.get::<_, i64>(0)
            })
            .is_err(),
        _ => false,
    }
}

/// After a command re-derived the recording in `dir`: replace its catalog
/// rows. A missing or unusable catalog is left for the next `list` to
/// repair.
pub fn replace(root: &Path, dir: &Path) -> anyhow::Result<()> {
    let lock_path = root.join(LOCK);
    let lock = lockfile::open_shared_file(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    if !lockfile::lock_within(&lock, false, LOCK_WAIT)
        .with_context(|| format!("locking {}", lock_path.display()))?
    {
        anyhow::bail!("the session catalog is being repaired");
    }
    if !root.join(FILE).exists() {
        return Ok(());
    }
    let conn = open_rw(root, false).context("opening the session catalog")?;
    ensure_schema(&conn, false)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("checking the session catalog")?;
    let rec = dir.join(store::FILE);
    copy_session(&conn, &rec, Rows::ALL)
        .with_context(|| format!("copying {} into the catalog", rec.display()))
}

// ----------------------------------------------------------------- list

/// The window and target `heph session list` asks the catalog for. The
/// other filters apply to a recording without a catalog row too, so `list`
/// applies them itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct ListQuery<'a> {
    /// Sessions started at or after this (unix ms).
    pub since_ms: Option<u64>,
    /// Sessions started at or before this (unix ms).
    pub until_ms: Option<u64>,
    /// Only sessions that resolved this target, each with its row.
    pub addr: Option<&'a str>,
}

/// One catalog `sessions` row, as `list` reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listed {
    pub session_id: String,
    pub owner_pid: Option<u32>,
    pub user: Option<String>,
    /// `human`, `agent` or `ci`; an open set.
    pub actor_kind: Option<String>,
    pub actor_name: Option<String>,
    /// The heph subcommand (`run`, `query`, ...).
    pub command: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub workspace_root: Option<String>,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub panicked: bool,
    pub truncated: bool,
    /// False for a private session (`HEPH_NO_SESSION`), which kept no
    /// recording.
    pub recorded: bool,
    /// With [`ListQuery::addr`]: that target's row in this session.
    pub target: Option<ListedTarget>,
}

/// A session's row for one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedTarget {
    pub addr: String,
    /// `ran`, `hit`, `failed` or `cancelled`; an open set.
    pub outcome: String,
    pub duration_us: Option<u64>,
}

/// The catalog's sessions in `q`'s window, newest first.
pub fn list_sessions(conn: &Connection, q: &ListQuery<'_>) -> anyhow::Result<Vec<Listed>> {
    let ms = |v: Option<u64>| v.map(|v| i64::try_from(v).unwrap_or(i64::MAX));
    let (target_cols, target_join) = if q.addr.is_some() {
        // One seek per session in the window, as the `targets` view's
        // outcome mapping spells it.
        (
            ", CASE t.outcome WHEN 0 THEN 'hit' WHEN 1 THEN 'ran' WHEN 2 THEN 'failed' \
               WHEN 3 THEN 'cancelled' ELSE t.outcome END, t.duration_us",
            " JOIN target_rows t ON t.sid = s.sid \
               AND t.addr_id = (SELECT id FROM addrs WHERE addr = ?3)",
        )
    } else {
        // `?3` is still bound (NULL), so it must appear.
        (", NULL, NULL", " WHERE ?3 IS NULL")
    };
    let sql = format!(
        "SELECT s.session_id, s.owner_pid, s.user, s.actor_kind, s.actor_name, s.command, \
           s.argv, s.cwd, s.workspace_root, s.started_ms, s.ended_ms, s.exit_code, \
           coalesce(s.panicked, 0), coalesce(s.truncated, 0), s.recorded{target_cols} \
         FROM sessions s{target_join} \
         AND (?1 IS NULL OR s.started_ms >= ?1) AND (?2 IS NULL OR s.started_ms <= ?2) \
         ORDER BY s.started_ms DESC, s.session_id DESC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("preparing the session catalog's list")?;
    let row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Listed> {
        let argv: Option<String> = r.get(6)?;
        let outcome: Option<rusqlite::types::Value> = r.get(15)?;
        let target = match (q.addr, outcome) {
            (Some(addr), Some(o)) => Some(ListedTarget {
                addr: addr.to_owned(),
                outcome: match o {
                    rusqlite::types::Value::Text(t) => t,
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    _ => "unknown".to_owned(),
                },
                duration_us: r
                    .get::<_, Option<i64>>(16)?
                    .and_then(|d| u64::try_from(d).ok()),
            }),
            _ => None,
        };
        Ok(Listed {
            session_id: r.get(0)?,
            owner_pid: r
                .get::<_, Option<i64>>(1)?
                .and_then(|p| u32::try_from(p).ok()),
            user: r.get(2)?,
            actor_kind: r.get::<_, Option<String>>(3)?.filter(|k| !k.is_empty()),
            actor_name: r.get(4)?,
            command: r.get(5)?,
            argv: argv
                .and_then(|a| serde_json::from_str(&a).ok())
                .unwrap_or_default(),
            cwd: r.get(7)?,
            workspace_root: r.get(8)?,
            started_ms: u64::try_from(r.get::<_, i64>(9)?).unwrap_or(0),
            ended_ms: r
                .get::<_, Option<i64>>(10)?
                .and_then(|e| u64::try_from(e).ok()),
            exit_code: r
                .get::<_, Option<i64>>(11)?
                .and_then(|c| i32::try_from(c).ok()),
            panicked: r.get::<_, i64>(12)? != 0,
            truncated: r.get::<_, i64>(13)? != 0,
            recorded: r.get::<_, i64>(14)? != 0,
            target,
        })
    };
    let rows = stmt
        .query_map(params![ms(q.since_ms), ms(q.until_ms), q.addr], row)
        .context("listing the session catalog")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("reading the session catalog")
}

/// When the catalog's oldest session started (unix ms): how far back it
/// answers. `None` when it holds no session.
pub fn oldest_started_ms(conn: &Connection) -> anyhow::Result<Option<u64>> {
    let oldest: Option<i64> = conn
        .query_row("SELECT min(started_ms) FROM sessions", [], |r| r.get(0))
        .context("reading the session catalog's oldest session")?;
    Ok(oldest.and_then(|o| u64::try_from(o).ok()))
}

// -------------------------------------------------------------- cleanup

/// What a catalog cleanup did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanReport {
    /// Sessions older than [`KEEP`] deleted.
    pub expired: usize,
    /// Moved-aside corrupt files removed.
    pub corrupt: usize,
    /// Sessions whose explorer rows went to the cap.
    pub capped_rows: usize,
    /// Whole sessions that went to the cap.
    pub capped_sessions: usize,
}

/// The catalog half of the start-of-session cleanup: rows older than
/// [`KEEP`], moved-aside corrupt files older than [`CORRUPT_KEEP`], and the
/// [`CAP`]. Takes `catalog.lock` shared without waiting, and does nothing
/// when it cannot.
pub fn clean(root: &Path, now: SystemTime) -> anyhow::Result<CleanReport> {
    clean_with(root, now, CAP)
}

pub(crate) fn clean_with(root: &Path, now: SystemTime, cap: u64) -> anyhow::Result<CleanReport> {
    clean_between(root, now, cap, &mut || std::thread::sleep(CLEAN_YIELD))
}

/// [`clean_with`], calling `between` after each of its write transactions,
/// with none open: where an owner waiting on the catalog gets in.
pub(crate) fn clean_between(
    root: &Path,
    now: SystemTime,
    cap: u64,
    between: &mut dyn FnMut(),
) -> anyhow::Result<CleanReport> {
    let now_ms = u64::try_from(
        now.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let mut report = CleanReport {
        corrupt: sweep_corrupt(root, now_ms),
        ..CleanReport::default()
    };
    if !root.join(FILE).exists() {
        return Ok(report);
    }
    let lock_path = root.join(LOCK);
    let lock = lockfile::open_shared_file(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    if !lockfile::lock_within(&lock, false, Duration::ZERO)
        .with_context(|| format!("locking {}", lock_path.display()))?
    {
        return Ok(report);
    }
    let conn = open_rw(root, false).context("opening the session catalog")?;
    ensure_schema(&conn, false)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("checking the session catalog")?;
    let keep_ms = u64::try_from(KEEP.as_millis()).unwrap_or(u64::MAX);
    let cutoff = i64::try_from(now_ms.saturating_sub(keep_ms)).unwrap_or(i64::MAX);
    report.expired = delete_sessions(&conn, "started_ms < ?1", [cutoff])
        .context("deleting expired catalog rows")?;
    if report.expired > 0 {
        between();
    }
    let (rows, sessions) =
        enforce_cap(&conn, cap, between).context("capping the session catalog")?;
    report.capped_rows = rows;
    report.capped_sessions = sessions;
    if report.expired + sessions > 0 {
        // Once, not per session deleted: a scan of every target row.
        conn.execute_batch("DELETE FROM addrs WHERE id NOT IN (SELECT addr_id FROM target_rows)")
            .context("pruning the catalog's addresses")?;
        between();
    }
    if report.expired + rows + sessions > 0 {
        vacuum(&conn, between).context("giving the catalog's free pages back")?;
    }
    Ok(report)
}

/// Give the free pages back [`VACUUM_STEP`] at a time, each step its own
/// transaction, calling `between` after each.
fn vacuum(conn: &Connection, between: &mut dyn FnMut()) -> rusqlite::Result<()> {
    let free = || conn.pragma_query_value(None, "freelist_count", |r| r.get::<_, i64>(0));
    let mut left = free()?;
    let mut step = conn.prepare(&format!("PRAGMA incremental_vacuum({VACUUM_STEP})"))?;
    while left > 0 {
        // Each step of the statement gives one page back: run it to its
        // end (`execute_batch` would step it once).
        let mut pages = step.query([])?;
        while pages.next()?.is_some() {}
        drop(pages);
        between();
        let now = free()?;
        if now >= left {
            // Not an incremental catalog: nothing more to give back.
            break;
        }
        left = now;
    }
    Ok(())
}

/// Delete the sessions `filter` selects and every row of theirs, in one
/// transaction. The addresses they leave unused stay for the caller to
/// prune once.
fn delete_sessions(
    conn: &Connection,
    filter: &str,
    p: impl rusqlite::Params,
) -> rusqlite::Result<usize> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let work = || -> rusqlite::Result<usize> {
        conn.execute_batch("DROP TABLE IF EXISTS temp.gone")?;
        conn.execute(
            &format!(
                "CREATE TEMP TABLE gone AS SELECT sid, session_id FROM sessions WHERE {filter}"
            ),
            p,
        )?;
        let gone: i64 = conn.query_row("SELECT count(*) FROM temp.gone", [], |r| r.get(0))?;
        let gone = usize::try_from(gone).unwrap_or(0);
        if gone > 0 {
            conn.execute_batch(
                "DELETE FROM target_rows WHERE sid IN (SELECT sid FROM temp.gone);
                 DELETE FROM spans WHERE session_id IN (SELECT session_id FROM temp.gone);
                 DELETE FROM span_events WHERE session_id IN (SELECT session_id FROM temp.gone);
                 DELETE FROM metrics WHERE session_id IN (SELECT session_id FROM temp.gone);
                 DELETE FROM heal_failures WHERE session_id IN (SELECT session_id FROM temp.gone);
                 DELETE FROM sessions WHERE sid IN (SELECT sid FROM temp.gone);",
            )?;
        }
        conn.execute_batch("DROP TABLE temp.gone")?;
        Ok(gone)
    };
    match work() {
        Ok(n) => conn.execute_batch("COMMIT").map(|()| n),
        Err(e) => {
            rollback(conn);
            Err(e)
        }
    }
}

/// The pages in use, in bytes.
fn used_bytes(conn: &Connection) -> rusqlite::Result<u64> {
    let get = |p: &str| -> rusqlite::Result<u64> {
        conn.pragma_query_value(None, p, |r| r.get::<_, i64>(0))
            .map(|n| u64::try_from(n).unwrap_or(0))
    };
    Ok(get("page_count")?.saturating_sub(get("freelist_count")?) * get("page_size")?)
}

/// Past `cap`: drop the span and metric rows of the oldest sessions first,
/// keeping `sessions` and `targets`, then whole sessions by age. Returns how
/// many sessions lost their explorer rows, and how many went whole. One
/// transaction per session, `between` after each.
fn enforce_cap(
    conn: &Connection,
    cap: u64,
    between: &mut dyn FnMut(),
) -> rusqlite::Result<(usize, usize)> {
    let (mut rows, mut whole) = (0, 0);
    while used_bytes(conn)? > cap {
        let oldest: Option<String> = conn
            .query_row(
                "SELECT session_id FROM sessions s
                 WHERE EXISTS (SELECT 1 FROM spans WHERE session_id = s.session_id)
                    OR EXISTS (SELECT 1 FROM metrics WHERE session_id = s.session_id)
                    OR EXISTS (SELECT 1 FROM span_events WHERE session_id = s.session_id)
                 ORDER BY started_ms LIMIT 1",
                [],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        if let Some(id) = oldest {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            for table in ["spans", "span_events", "metrics"] {
                if let Err(e) =
                    conn.execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [&id])
                {
                    rollback(conn);
                    return Err(e);
                }
            }
            conn.execute_batch("COMMIT")?;
            between();
            rows += 1;
            continue;
        }
        let gone = delete_sessions(
            conn,
            "sid = (SELECT sid FROM sessions ORDER BY started_ms LIMIT 1)",
            [],
        )?;
        between();
        if gone == 0 {
            break;
        }
        whole += gone;
    }
    Ok((rows, whole))
}

/// Remove `catalog.db.corrupt-<ms>*` moved aside [`CORRUPT_KEEP`] or longer
/// ago, by the time in the name.
fn sweep_corrupt(root: &Path, now_ms: u64) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let keep_ms = u64::try_from(CORRUPT_KEEP.as_millis()).unwrap_or(u64::MAX);
    let mut removed = 0;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(CORRUPT_PREFIX)) else {
            continue;
        };
        let digits = rest.split('-').next().unwrap_or_default();
        let Ok(at) = digits.parse::<u64>() else {
            continue;
        };
        if now_ms.saturating_sub(at) < keep_ms {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::debug!(path = %entry.path().display(), error = %e, "cannot remove a corrupt catalog")
            }
        }
    }
    removed
}

/// Make `root` (a private session's catalog root, the public sessions
/// directory beside it) when it is missing, user-only.
pub(crate) fn ensure_root(root: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner's seal write lands on a catalog its start thread made while
    /// the seal's connection still held it as empty: a short build's start
    /// and seal race on a new catalog. Without `main.`, the stale schema
    /// resolved `sessions` into the attached recording's `sessions` view and
    /// the seal was dropped ("cannot modify sessions because it is a view").
    #[test]
    fn a_seal_on_a_catalog_made_meanwhile_lands() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let id = SessionId::new(1_700_000_000_000, 1);
        let dir = super::super::catalog_tests::sealed(root, id);
        let stale = open_rw(root, true).expect("open");
        let tables: i64 = stale
            .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
            .expect("schema");
        assert_eq!(tables, 0, "the seal's connection caches an empty schema");
        record_start(root, &dir);
        ensure_schema(&stale, false).expect("made meanwhile");
        copy_session(&stale, &dir.join(store::FILE), Rows::ALL).expect("the seal write lands");
        let complete: i64 = stale
            .query_row("SELECT complete FROM main.sessions", [], |r| r.get(0))
            .expect("the session's row");
        assert_eq!(complete, 1);
    }
}
