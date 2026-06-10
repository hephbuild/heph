//! SQLite-backed telemetry spool.
//!
//! Commands never pay network latency at exit: the event is appended to a
//! machine-local SQLite spool (a single fast insert), and a detached background
//! thread on the *next* invocation flushes pending rows to PostHog in one batch
//! while that command does its real work. Rows are deleted only after a
//! successful send, and every event carries a stable UUID so PostHog dedupes a
//! row that gets re-sent when the process exits mid-flush (at-least-once spool,
//! exactly-once ingestion).

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
const MAX_ROWS: i64 = 200;

/// How many rows one flush attempt sends in a single batch request.
pub const FLUSH_BATCH: usize = 50;

pub struct Spool {
    conn: Connection,
}

impl Spool {
    /// Open (creating if needed) the spool at `path`. The parent directory must
    /// already exist. Short busy timeout: concurrent heph processes touch the
    /// spool only for an insert or a flush, both brief.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening telemetry spool {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(500))
            .context("setting telemetry spool busy timeout")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = OFF;
             CREATE TABLE IF NOT EXISTS events (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 uuid        TEXT    NOT NULL,
                 distinct_id TEXT    NOT NULL,
                 event       TEXT    NOT NULL,
                 props       TEXT    NOT NULL,
                 created_ms  INTEGER NOT NULL
             );",
        )
        .context("initializing telemetry spool schema")?;
        Ok(Self { conn })
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
