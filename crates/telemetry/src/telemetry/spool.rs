//! SQLite-backed telemetry spool.
//!
//! Commands never pay network latency at exit: the event is appended to a
//! machine-local SQLite spool (a single fast insert), and once a flush is due
//! (see `flush_due`) the command spawns a detached `heph __telemetry-flush` and
//! returns. That flusher sends the pending rows to PostHog in batches.
//!
//! The protocol, which is what makes the flush safe to run detached:
//! - **One flusher at a time.** It holds an exclusive `flock` on
//!   `telemetry-flush.lock` for its whole run, so two flushers never read and
//!   send the same rows.
//! - **Delete only what was confirmed.** A flusher reads a batch by row id,
//!   sends it, and only after PostHog confirms deletes exactly those ids, in one
//!   transaction. Rows appended meanwhile have higher ids and are untouched; a
//!   send that fails leaves the batch spooled for the next flush. Nothing is
//!   ever lost to a crash.
//! - **A resend is a no-op.** The one window the protocol cannot close is a
//!   crash after PostHog accepted a batch and before the delete commits:
//!   exactly-once delivery over HTTP does not exist. Those rows are sent again
//!   by the next flush, carrying the same per-event UUID, which PostHog dedupes
//!   on. At-least-once spool, exactly-once ingestion.
//! - **Bounded.** Past [`MAX_ROWS`] the oldest rows are dropped at enqueue.

use anyhow::Context;
use rusqlite::Connection;
use std::path::Path;

/// One spooled event, exactly as it will be sent.
#[derive(Debug, Clone, PartialEq)]
pub struct SpooledEvent {
    /// Stable per-event id for server-side dedup across flush retries.
    pub uuid: String,
    /// Anonymous install id (or `"ci"`).
    pub distinct_id: String,
    /// Event name, e.g. `cli_command`.
    pub event: String,
    /// Full property map, JSON-serialized.
    pub props: serde_json::Map<String, serde_json::Value>,
    /// Wall-clock capture time (ms since epoch), reported as the event
    /// timestamp so flush lag doesn't skew the data.
    pub created_ms: i64,
}

/// Keep at most this many rows; oldest beyond the cap are dropped at enqueue.
/// Bounds the spool for a machine that is offline (or opted out of nothing but
/// has no network) for a long stretch.
pub const MAX_ROWS: i64 = 200;

/// How many rows one flush attempt sends in a single batch request.
pub const FLUSH_BATCH: usize = 50;

/// What the flush decision looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpoolState {
    /// Rows waiting to be sent.
    pub count: u64,
    /// Capture time (ms since epoch) of the oldest of them.
    pub oldest_ms: Option<i64>,
    /// When a command last spawned a flusher (ms since epoch).
    pub last_spawn_ms: Option<i64>,
}

/// `meta` key holding [`SpoolState::last_spawn_ms`].
const LAST_SPAWN_KEY: &str = "last_flush_spawn_ms";
/// `meta` key holding [`LastFlush::at_ms`].
const LAST_FLUSH_KEY: &str = "last_flush_ms";
/// `meta` key holding [`LastFlush::error`]; absent after a flush that succeeded.
const LAST_FLUSH_ERROR_KEY: &str = "last_flush_error";

/// The spool schema's version, stored in the file's `PRAGMA user_version`.
///
/// A spool at this version has its tables and WAL mode, so opening it runs no
/// DDL — the open sits on the exit path of every command, and re-running the
/// `CREATE … IF NOT EXISTS` batch made each one parse the schema on a
/// read-write connection. A spool below it (a fresh file, or one written
/// before the version existed: `0`) gets the idempotent DDL once. Bump it with
/// any schema change.
const SCHEMA_VERSION: i64 = 1;

/// What the last flush that ran to completion did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastFlush {
    /// When it finished (ms since epoch).
    pub at_ms: i64,
    /// Why it failed; `None` when everything it read was sent.
    pub error: Option<String>,
}

/// The spool file and the two SQLite WAL sidecars that belong to it.
fn spool_files(path: &Path) -> [std::path::PathBuf; 3] {
    let with = |suffix: &str| {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        std::path::PathBuf::from(p)
    };
    [path.to_path_buf(), with("-wal"), with("-shm")]
}

/// Delete the spool at `path` (and its WAL sidecars), unsent events and all.
/// `Ok(false)` when there was nothing to delete. A process that still has it
/// open keeps writing to the unlinked file, which is then lost with it; the
/// next open starts a fresh, empty spool.
pub fn remove(path: &Path) -> anyhow::Result<bool> {
    let mut removed = false;
    for file in spool_files(path) {
        match std::fs::remove_file(&file) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("removing {}", file.display()));
            }
        }
    }
    Ok(removed)
}

pub struct Spool {
    conn: Connection,
}

impl Spool {
    /// Open (creating if needed) the spool at `path`. The parent directory must
    /// already exist. Short busy timeout: concurrent heph processes touch the
    /// spool only for an insert or a flush, both brief.
    ///
    /// A spool already at [`SCHEMA_VERSION`] costs one header read; only a
    /// fresh or older one runs the DDL.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening telemetry spool {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(500))
            .context("setting telemetry spool busy timeout")?;
        // Per connection, not persisted: every open sets it. Cheap — it reads
        // no schema.
        conn.execute_batch("PRAGMA synchronous = OFF;")
            .context("setting telemetry spool synchronous mode")?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("reading telemetry spool schema version")?;
        if version < SCHEMA_VERSION {
            Self::migrate(&conn)?;
        }
        Ok(Self { conn })
    }

    /// Bring a spool below [`SCHEMA_VERSION`] up to it. Idempotent, so two
    /// processes doing it at once both end at the same schema; at worst the
    /// loser of the WAL switch (which takes the database's exclusive lock
    /// without consulting the busy handler) fails this one open, and that
    /// command's event is dropped — telemetry is best-effort.
    fn migrate(conn: &Connection) -> anyhow::Result<()> {
        // Outside the transaction: the journal mode cannot change inside one.
        // It persists in the file. (No `page_size` is set; one would have to
        // come first, since it cannot change once the file is in WAL mode.)
        conn.execute_batch("PRAGMA journal_mode = WAL;")
            .context("switching telemetry spool to WAL")?;
        // `meta.v` is declared without a type: it holds integers (timestamps)
        // and text (the last flush error) alike. A `meta` table created by an
        // earlier build with `v INTEGER` stores either just the same — a
        // non-numeric string keeps TEXT storage under INTEGER affinity.
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS events (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 uuid        TEXT    NOT NULL,
                 distinct_id TEXT    NOT NULL,
                 event       TEXT    NOT NULL,
                 props       TEXT    NOT NULL,
                 created_ms  INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS meta (
                 k TEXT PRIMARY KEY,
                 v NOT NULL
             );
             PRAGMA user_version = {SCHEMA_VERSION};
             COMMIT;"
        ))
        .context("initializing telemetry spool schema")
    }

    /// One `meta` value, `None` when the key is absent.
    fn meta(&self, key: &str) -> anyhow::Result<Option<rusqlite::types::Value>> {
        match self
            .conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [key], |r| r.get(0))
        {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading telemetry spool meta {key}")),
        }
    }

    /// Record the outcome of a flush that finished at `at_ms`: `error` is why
    /// it failed, `None` when it sent everything it read.
    pub fn record_flush(&self, at_ms: i64, error: Option<&str>) -> anyhow::Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .context("starting telemetry flush record")?;
        tx.execute(
            "INSERT INTO meta (k, v) VALUES (?1, ?2)
             ON CONFLICT (k) DO UPDATE SET v = excluded.v",
            rusqlite::params![LAST_FLUSH_KEY, at_ms],
        )
        .context("recording telemetry flush time")?;
        match error {
            Some(error) => tx.execute(
                "INSERT INTO meta (k, v) VALUES (?1, ?2)
                 ON CONFLICT (k) DO UPDATE SET v = excluded.v",
                rusqlite::params![LAST_FLUSH_ERROR_KEY, error],
            ),
            None => tx.execute("DELETE FROM meta WHERE k = ?1", [LAST_FLUSH_ERROR_KEY]),
        }
        .context("recording telemetry flush error")?;
        tx.commit().context("committing telemetry flush record")
    }

    /// The outcome [`Self::record_flush`] last stored; `None` before the first.
    pub fn last_flush(&self) -> anyhow::Result<Option<LastFlush>> {
        use rusqlite::types::Value;
        let Some(Value::Integer(at_ms)) = self.meta(LAST_FLUSH_KEY)? else {
            return Ok(None);
        };
        let error = match self.meta(LAST_FLUSH_ERROR_KEY)? {
            None | Some(Value::Null) => None,
            Some(Value::Text(s)) => Some(s),
            Some(Value::Integer(n)) => Some(n.to_string()),
            Some(Value::Real(f)) => Some(f.to_string()),
            Some(Value::Blob(b)) => Some(String::from_utf8_lossy(&b).into_owned()),
        };
        Ok(Some(LastFlush { at_ms, error }))
    }

    /// Row count, oldest capture time, and the last flusher spawn.
    pub fn state(&self) -> anyhow::Result<SpoolState> {
        let (count, oldest_ms): (i64, Option<i64>) = self
            .conn
            .query_row("SELECT count(*), min(created_ms) FROM events", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .context("reading telemetry spool state")?;
        let last_spawn_ms = match self.meta(LAST_SPAWN_KEY)? {
            Some(rusqlite::types::Value::Integer(v)) => Some(v),
            _ => None,
        };
        Ok(SpoolState {
            count: u64::try_from(count).unwrap_or(0),
            oldest_ms,
            last_spawn_ms,
        })
    }

    /// Record a flusher spawn at `now_ms` — but only if the last one recorded
    /// is still `seen` (what [`Self::state`] returned). One statement, so of
    /// several commands that all decided a flush was due, exactly one wins and
    /// spawns; the rest see `false`.
    pub fn claim_spawn(&self, seen: Option<i64>, now_ms: i64) -> anyhow::Result<bool> {
        let changed = self
            .conn
            .execute(
                "INSERT INTO meta (k, v) VALUES (?1, ?2)
                 ON CONFLICT (k) DO UPDATE SET v = excluded.v WHERE meta.v IS ?3",
                rusqlite::params![LAST_SPAWN_KEY, now_ms, seen],
            )
            .context("claiming telemetry flush spawn")?;
        Ok(changed == 1)
    }

    /// Append one event and prune the oldest rows past [`MAX_ROWS`].
    pub fn enqueue(&self, ev: &SpooledEvent) -> anyhow::Result<()> {
        let props =
            serde_json::to_string(&ev.props).context("serializing telemetry event props")?;
        self.conn
            .execute(
                "INSERT INTO events (uuid, distinct_id, event, props, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![ev.uuid, ev.distinct_id, ev.event, props, ev.created_ms],
            )
            .context("inserting telemetry event")?;
        self.conn
            .execute(
                "DELETE FROM events WHERE id NOT IN
                 (SELECT id FROM events ORDER BY id DESC LIMIT ?1)",
                [MAX_ROWS],
            )
            .context("pruning telemetry spool")?;
        Ok(())
    }

    /// Read up to `limit` pending rows, oldest first, with their row ids.
    pub fn take_batch(&self, limit: usize) -> anyhow::Result<Vec<(i64, SpooledEvent)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, uuid, distinct_id, event, props, created_ms
                 FROM events ORDER BY id ASC LIMIT ?1",
            )
            .context("preparing telemetry spool read")?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .context("reading telemetry spool rows")?;

        let mut out = Vec::new();
        for row in rows {
            let (id, uuid, distinct_id, event, props, created_ms) =
                row.context("decoding telemetry spool row")?;
            let props = serde_json::from_str(&props).context("parsing telemetry event props")?;
            out.push((
                id,
                SpooledEvent {
                    uuid,
                    distinct_id,
                    event,
                    props,
                    created_ms,
                },
            ));
        }
        Ok(out)
    }

    /// Delete the given rows — called only after a successful send.
    pub fn delete(&self, ids: &[i64]) -> anyhow::Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .context("starting telemetry spool delete")?;
        for id in ids {
            tx.execute("DELETE FROM events WHERE id = ?1", [id])
                .context("deleting flushed telemetry event")?;
        }
        tx.commit().context("committing telemetry spool delete")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(n: u64) -> SpooledEvent {
        let mut props = serde_json::Map::new();
        props.insert("n".into(), n.into());
        SpooledEvent {
            uuid: format!("uuid-{n}"),
            distinct_id: "install-1".into(),
            event: "cli_command".into(),
            props,
            created_ms: 1000 + n as i64,
        }
    }

    #[test]
    fn enqueue_take_delete_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = Spool::open(&dir.path().join("spool.db")).expect("open");

        spool.enqueue(&ev(1)).expect("enqueue");
        spool.enqueue(&ev(2)).expect("enqueue");

        let batch = spool.take_batch(10).expect("take");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].1, ev(1), "oldest first");
        assert_eq!(batch[1].1, ev(2));

        // Rows survive a read (delete only after successful send).
        assert_eq!(spool.take_batch(10).expect("take").len(), 2);

        let ids: Vec<i64> = batch.iter().map(|(id, _)| *id).collect();
        spool.delete(&ids).expect("delete");
        assert!(spool.take_batch(10).expect("take").is_empty());
    }

    #[test]
    fn take_batch_respects_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = Spool::open(&dir.path().join("spool.db")).expect("open");
        for n in 0..5 {
            spool.enqueue(&ev(n)).expect("enqueue");
        }
        assert_eq!(spool.take_batch(3).expect("take").len(), 3);
    }

    #[test]
    fn prunes_oldest_past_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = Spool::open(&dir.path().join("spool.db")).expect("open");
        for n in 0..(MAX_ROWS as u64 + 10) {
            spool.enqueue(&ev(n)).expect("enqueue");
        }
        let batch = spool.take_batch(usize::MAX).expect("take");
        assert_eq!(batch.len(), MAX_ROWS as usize);
        // The dropped rows are the oldest ones.
        assert_eq!(batch[0].1.uuid, "uuid-10");
    }

    fn user_version(path: &Path) -> i64 {
        Connection::open(path)
            .expect("open")
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("user_version")
    }

    fn has_table(path: &Path, name: &str) -> bool {
        Connection::open(path)
            .expect("open")
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .expect("sqlite_master")
            == 1
    }

    /// A fresh spool is created in WAL mode at the current schema version,
    /// and a spool already at that version is opened without running the DDL
    /// again: a table dropped behind its back stays dropped.
    #[test]
    fn a_current_spool_opens_without_ddl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spool.db");
        drop(Spool::open(&path).expect("create"));
        assert_eq!(user_version(&path), SCHEMA_VERSION);
        let mode: String = Connection::open(&path)
            .expect("open")
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("journal_mode");
        assert_eq!(mode, "wal");

        Connection::open(&path)
            .expect("open")
            .execute_batch("DROP TABLE meta;")
            .expect("drop meta");
        drop(Spool::open(&path).expect("reopen"));
        assert!(
            !has_table(&path, "meta"),
            "a spool at the current version must not re-run its DDL"
        );
    }

    /// A spool written before the version existed (`user_version` 0, the
    /// `events` table only) is brought up on open, keeping its rows.
    #[test]
    fn a_pre_version_spool_is_migrated_with_its_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spool.db");
        Connection::open(&path)
            .expect("open")
            .execute_batch(
                "CREATE TABLE events (
                     id          INTEGER PRIMARY KEY AUTOINCREMENT,
                     uuid        TEXT    NOT NULL,
                     distinct_id TEXT    NOT NULL,
                     event       TEXT    NOT NULL,
                     props       TEXT    NOT NULL,
                     created_ms  INTEGER NOT NULL
                 );
                 INSERT INTO events (uuid, distinct_id, event, props, created_ms)
                 VALUES ('uuid-old', 'install-1', 'cli_command', '{}', 1);",
            )
            .expect("old schema");
        assert_eq!(user_version(&path), 0);

        let spool = Spool::open(&path).expect("open");
        assert_eq!(user_version(&path), SCHEMA_VERSION);
        assert!(has_table(&path, "meta"));
        let batch = spool.take_batch(10).expect("take");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].1.uuid, "uuid-old");
    }

    /// The last flush outcome round-trips, and a success clears the error a
    /// failure left — including over a `meta` table an earlier build created
    /// with an INTEGER-typed value column.
    #[test]
    fn last_flush_records_the_latest_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spool.db");
        Connection::open(&path)
            .expect("open")
            .execute_batch("CREATE TABLE meta (k TEXT PRIMARY KEY, v INTEGER NOT NULL);")
            .expect("old meta");
        let spool = Spool::open(&path).expect("open");
        assert_eq!(spool.last_flush().expect("read"), None);

        spool
            .record_flush(10, Some("connection refused"))
            .expect("record failure");
        assert_eq!(
            spool.last_flush().expect("read"),
            Some(LastFlush {
                at_ms: 10,
                error: Some("connection refused".into())
            })
        );
        spool.record_flush(20, None).expect("record success");
        assert_eq!(
            spool.last_flush().expect("read"),
            Some(LastFlush {
                at_ms: 20,
                error: None
            })
        );
        // The spawn stamp shares the table and is untouched by either.
        assert!(spool.claim_spawn(None, 5).expect("claim"));
        assert_eq!(spool.state().expect("state").last_spawn_ms, Some(5));
    }

    /// `remove` deletes the spool and its WAL sidecars, and says whether
    /// there was anything to delete.
    #[test]
    fn remove_deletes_the_spool_and_its_sidecars() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spool.db");
        let spool = Spool::open(&path).expect("open");
        spool.enqueue(&ev(1)).expect("enqueue");
        assert!(dir.path().join("spool.db-wal").exists());
        drop(spool);
        std::fs::write(dir.path().join("spool.db-wal"), b"x").expect("wal");

        assert!(remove(&path).expect("remove"));
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("ls").count(),
            0,
            "spool and sidecars all gone"
        );
        assert!(!remove(&path).expect("remove again"));
        assert!(
            Spool::open(&path)
                .expect("reopen")
                .take_batch(10)
                .expect("take")
                .is_empty()
        );
    }

    #[test]
    fn reopen_persists_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("spool.db");
        Spool::open(&path)
            .expect("open")
            .enqueue(&ev(7))
            .expect("enqueue");
        let batch = Spool::open(&path)
            .expect("reopen")
            .take_batch(10)
            .expect("take");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].1, ev(7));
    }
}
