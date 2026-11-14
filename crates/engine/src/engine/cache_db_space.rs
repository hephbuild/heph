//! Giving `cache.db`'s free pages back to the filesystem.
//!
//! Artifacts up to the spill threshold live inline in sqlite, so every
//! `cache.history` trim deletes rows, and sqlite reuses the pages it frees but
//! never shrinks the file on its own. One measured cache was a 21 GB file
//! holding 11.6 GiB of live data.
//!
//! Everything here runs on the cache's single writer connection (the writer
//! thread, `local_cache_sqlite::writer_loop`), so it never races a cache write:
//!
//! - A database created by this binary has `auto_vacuum = INCREMENTAL` (set by
//!   `migrate_schema`). On such a database the end of a request that trimmed
//!   something runs [`MaintainKind::AfterTrim`]: when the [`ReclaimPolicy`] says
//!   it is due, `PRAGMA incremental_vacuum` in bounded chunks, queued cache
//!   writes committed between chunks, then `wal_checkpoint(TRUNCATE)`.
//! - Every outcome is a [`CompactReport`] with the file's [`CacheDbSpace`]
//!   before and after.
//!
//! Nothing here reaches a hash or a cache key: it moves pages, never rows.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MIB: u64 = 1024 * 1024;

/// How long after a successful vacuum the end-of-request reclaim waits before
/// vacuuming again, unless the free pages reach the [`RECLAIM_CAP_BYTES`] cap.
pub(crate) const RECLAIM_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Below this many free bytes, a due interval vacuums nothing: the file is
/// close enough to its live size.
pub(crate) const RECLAIM_MIN_FREE_BYTES: u64 = 64 * MIB;
/// Free bytes at which the end-of-request reclaim runs whatever the interval:
/// this, or [`RECLAIM_CAP_PERCENT`] of the file, whichever is smaller.
pub(crate) const RECLAIM_CAP_BYTES: u64 = 2048 * MIB;
pub(crate) const RECLAIM_CAP_PERCENT: u64 = 25;
/// Pages freed per `incremental_vacuum` transaction, in bytes. Each chunk is its
/// own transaction, and queued cache writes commit between two of them.
pub(crate) const RECLAIM_CHUNK_BYTES: u64 = 64 * MIB;
/// How long one end-of-request reclaim may run. It is charged to the
/// background drain that gates process exit, so it stops at the first chunk
/// boundary past this; the rest is taken by the next request (the interval
/// only restarts once a vacuum completes).
pub(crate) const RECLAIM_TIME_BUDGET: Duration = Duration::from_secs(5);
/// The writer connection's `journal_size_limit`: after a large batch or a
/// vacuum, the WAL is truncated back to this when it is next reset rather than
/// keeping its high-water size.
pub(crate) const JOURNAL_SIZE_LIMIT_BYTES: u64 = 64 * MIB;
/// The writer connection's busy timeout (set in `spawn_writer`), restored after
/// the end-of-request checkpoint's shorter one.
const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the end-of-request `wal_checkpoint(TRUNCATE)` waits for readers
/// still on an older snapshot. It runs in the exit drain; a checkpoint that
/// cannot finish is retried by the next one.
const AFTER_TRIM_CHECKPOINT_BUSY: Duration = Duration::from_secs(1);

/// The database's `PRAGMA auto_vacuum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VacuumMode {
    #[default]
    None,
    Full,
    Incremental,
}

impl VacuumMode {
    fn from_pragma(v: i64) -> Self {
        match v {
            1 => Self::Full,
            2 => Self::Incremental,
            _ => Self::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
            Self::Incremental => "incremental",
        }
    }
}

/// Where `cache.db`'s bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheDbSpace {
    /// The main database file's size on disk.
    pub file_bytes: u64,
    /// The `-wal` file's size on disk (0 when absent).
    pub wal_bytes: u64,
    /// Pages holding data: `(page_count − freelist_count) × page_size`.
    pub live_bytes: u64,
    /// Free pages: `freelist_count × page_size`. Reused by later writes, but
    /// only returned to the filesystem by a vacuum.
    pub free_bytes: u64,
    pub mode: VacuumMode,
}

impl CacheDbSpace {
    /// The database's size in pages × page size, i.e. what the file will be once
    /// every committed page is checkpointed into it.
    fn logical_bytes(&self) -> u64 {
        self.live_bytes.saturating_add(self.free_bytes)
    }
}

/// What a maintenance pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactAction {
    /// Nothing to do: the reclaim was not due, the database has no free pages,
    /// or (end of request) it is not `INCREMENTAL`.
    Unchanged,
    /// `incremental_vacuum` ran. `complete` is false when it stopped at a chunk
    /// boundary (time budget, cancellation, the cache closing) with free pages
    /// left.
    Vacuumed { freed_bytes: u64, complete: bool },
}

/// One maintenance pass: the file before and after, and what was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReport {
    pub before: CacheDbSpace,
    pub after: CacheDbSpace,
    pub action: CompactAction,
}

/// When the end-of-request reclaim runs. See [`ReclaimPolicy::due`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimPolicy {
    pub interval: Duration,
    pub min_free_bytes: u64,
    pub cap_bytes: u64,
    pub cap_percent: u64,
    pub chunk_bytes: u64,
    pub budget: Duration,
}

impl Default for ReclaimPolicy {
    fn default() -> Self {
        Self {
            interval: RECLAIM_INTERVAL,
            min_free_bytes: RECLAIM_MIN_FREE_BYTES,
            cap_bytes: RECLAIM_CAP_BYTES,
            cap_percent: RECLAIM_CAP_PERCENT,
            chunk_bytes: RECLAIM_CHUNK_BYTES,
            budget: RECLAIM_TIME_BUDGET,
        }
    }
}

impl ReclaimPolicy {
    /// The free bytes at which a reclaim runs whatever the interval.
    fn cap(&self, space: &CacheDbSpace) -> u64 {
        let pct = space.logical_bytes().saturating_mul(self.cap_percent) / 100;
        self.cap_bytes.min(pct)
    }

    /// Whether the end-of-request reclaim should run: free pages at the cap, or
    /// the interval elapsed since the last successful vacuum and free pages at
    /// the floor. No record of a last vacuum, or one in the future (the clock
    /// moved back), counts as elapsed.
    pub(crate) fn due(
        &self,
        space: &CacheDbSpace,
        last_vacuum: Option<SystemTime>,
        now: SystemTime,
    ) -> bool {
        if space.free_bytes == 0 {
            return false;
        }
        if space.free_bytes >= self.cap(space) {
            return true;
        }
        let elapsed = last_vacuum.is_none_or(|t| {
            now.duration_since(t)
                .map_or(true, |since| since >= self.interval)
        });
        elapsed && space.free_bytes >= self.min_free_bytes
    }
}

/// What the writer thread is asked to do.
#[derive(Debug, Clone)]
pub(crate) enum MaintainKind {
    /// End of request, after the history trim: vacuum an `INCREMENTAL`
    /// database if `policy` says it is due, within `policy.budget`.
    AfterTrim(ReclaimPolicy),
}

/// The file recording the last successful vacuum, as unix seconds.
pub(crate) fn sidecar_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.as_os_str().to_owned();
    p.push(".vacuumed");
    PathBuf::from(p)
}

/// When the last vacuum completed; `None` when unrecorded or unreadable.
pub(crate) fn last_vacuum(db_path: &Path) -> Option<SystemTime> {
    let text = std::fs::read_to_string(sidecar_path(db_path)).ok()?;
    let secs: u64 = text.trim().parse().ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(secs))
}

/// Record `at` as the last completed vacuum, atomically: a reader sees the old
/// timestamp or the new one, never a torn file.
fn record_vacuum(db_path: &Path, at: SystemTime) -> Result<()> {
    let path = sidecar_path(db_path);
    let dir = path
        .parent()
        .with_context(|| format!("{path:?} has no parent directory"))?;
    let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {dir:?}"))?;
    std::io::Write::write_all(&mut tmp, format!("{secs}\n").as_bytes())
        .with_context(|| format!("writing {:?}", tmp.path()))?;
    tmp.persist(&path)
        .with_context(|| format!("replacing {path:?}"))?;
    Ok(())
}

fn pragma_u64(conn: &Connection, name: &str) -> Result<u64> {
    let v: i64 = conn
        .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
        .with_context(|| format!("reading PRAGMA {name}"))?;
    u64::try_from(v).with_context(|| format!("PRAGMA {name} is negative: {v}"))
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn wal_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.as_os_str().to_owned();
    p.push("-wal");
    PathBuf::from(p)
}

/// The database's space accounting, read on `conn`.
pub(crate) fn space(conn: &Connection, db_path: &Path) -> Result<CacheDbSpace> {
    let page_size = pragma_u64(conn, "page_size")?;
    let page_count = pragma_u64(conn, "page_count")?;
    let freelist = pragma_u64(conn, "freelist_count")?;
    let mode = VacuumMode::from_pragma(
        conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .context("reading PRAGMA auto_vacuum")?,
    );
    Ok(CacheDbSpace {
        file_bytes: file_len(db_path),
        wal_bytes: file_len(&wal_path(db_path)),
        live_bytes: page_count
            .saturating_sub(freelist)
            .saturating_mul(page_size),
        free_bytes: freelist.saturating_mul(page_size),
        mode,
    })
}

/// Free pages to `incremental_vacuum` in chunks of `chunk_bytes`, until none
/// are left (`Ok(true)`) or `keep_going` says stop (`Ok(false)`). Between two
/// chunks `between` runs on the connection — the writer commits queued cache
/// writes there — and returns false to abandon (the cache is closing).
fn incremental(
    conn: &mut Connection,
    chunk_bytes: u64,
    keep_going: &dyn Fn() -> bool,
    between: &mut dyn FnMut(&mut Connection) -> bool,
) -> Result<bool> {
    let page_size = pragma_u64(conn, "page_size")?.max(1);
    let chunk_pages = (chunk_bytes / page_size).max(1);
    loop {
        if pragma_u64(conn, "freelist_count")? == 0 {
            return Ok(true);
        }
        if !keep_going() {
            return Ok(false);
        }
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("starting an incremental_vacuum transaction")?;
        tx.execute_batch(&format!("PRAGMA incremental_vacuum({chunk_pages});"))
            .context("running PRAGMA incremental_vacuum")?;
        tx.commit()
            .context("committing an incremental_vacuum transaction")?;
        if !between(conn) {
            return Ok(false);
        }
    }
}

/// `wal_checkpoint(TRUNCATE)`, waiting at most `busy` for readers on an older
/// snapshot. A checkpoint that could not finish is not an error: the next one
/// takes the rest.
fn checkpoint_truncate(conn: &Connection, busy: Duration) -> Result<()> {
    conn.busy_timeout(busy)
        .context("setting the checkpoint busy timeout")?;
    let res = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    });
    conn.busy_timeout(WRITER_BUSY_TIMEOUT)
        .context("restoring the writer busy timeout")?;
    let (busy, log, checkpointed) = res.context("running PRAGMA wal_checkpoint(TRUNCATE)")?;
    if busy != 0 {
        tracing::debug!(
            log,
            checkpointed,
            "cache.db checkpoint did not finish: a reader holds an older snapshot"
        );
    }
    Ok(())
}

/// Run one maintenance pass on the writer connection. `keep_going` is polled
/// at every chunk boundary; `between` runs after each chunk (see
/// [`incremental`]).
pub(crate) fn run(
    conn: &mut Connection,
    db_path: &Path,
    kind: &MaintainKind,
    keep_going: &dyn Fn() -> bool,
    between: &mut dyn FnMut(&mut Connection) -> bool,
) -> Result<CompactReport> {
    let before = space(conn, db_path)?;
    let action = match kind {
        MaintainKind::AfterTrim(policy) => {
            let now = SystemTime::now();
            if before.mode != VacuumMode::Incremental
                || !policy.due(&before, last_vacuum(db_path), now)
            {
                return Ok(CompactReport {
                    before,
                    after: before,
                    action: CompactAction::Unchanged,
                });
            }
            let deadline = Instant::now() + policy.budget;
            let complete = incremental(
                conn,
                policy.chunk_bytes,
                &|| keep_going() && Instant::now() < deadline,
                between,
            )?;
            checkpoint_truncate(conn, AFTER_TRIM_CHECKPOINT_BUSY)?;
            if complete {
                record_vacuum(db_path, SystemTime::now())?;
            }
            CompactAction::Vacuumed {
                freed_bytes: 0,
                complete,
            }
        }
    };
    let after = space(conn, db_path)?;
    let freed = before
        .file_bytes
        .saturating_add(before.wal_bytes)
        .saturating_sub(after.file_bytes.saturating_add(after.wal_bytes));
    let action = match action {
        CompactAction::Vacuumed { complete, .. } => CompactAction::Vacuumed {
            freed_bytes: freed,
            complete,
        },
        other => other,
    };
    Ok(CompactReport {
        before,
        after,
        action,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space_with(free: u64, live: u64) -> CacheDbSpace {
        CacheDbSpace {
            live_bytes: live,
            free_bytes: free,
            mode: VacuumMode::Incremental,
            ..CacheDbSpace::default()
        }
    }

    /// The trigger's rules, case by case. The end-to-end form of each (file
    /// shrinks, sidecar moves) is in `local_cache_sqlite`'s tests.
    #[test]
    fn reclaim_is_due_on_the_interval_or_the_cap() {
        let p = ReclaimPolicy::default();
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let recent = Some(now - Duration::from_secs(60));
        let old = Some(now - RECLAIM_INTERVAL);
        let future = Some(now + Duration::from_secs(3600));
        let big = 100 * 1024 * MIB;

        // Interval not elapsed, below the cap.
        assert!(!p.due(&space_with(512 * MIB, big), recent, now));
        // Interval elapsed, at the floor.
        assert!(p.due(&space_with(RECLAIM_MIN_FREE_BYTES, big), old, now));
        // Interval elapsed, under the floor.
        assert!(!p.due(&space_with(RECLAIM_MIN_FREE_BYTES - 1, big), old, now));
        // Interval not elapsed, at the 2 GiB cap of a large file.
        assert!(p.due(&space_with(RECLAIM_CAP_BYTES, big), recent, now));
        // Interval not elapsed, at 25% of a small file.
        assert!(p.due(&space_with(100 * MIB, 300 * MIB), recent, now));
        // No record, or one from the future, is due.
        assert!(p.due(&space_with(RECLAIM_MIN_FREE_BYTES, big), None, now));
        assert!(p.due(&space_with(RECLAIM_MIN_FREE_BYTES, big), future, now));
        // Nothing free is never due.
        assert!(!p.due(&space_with(0, 0), None, now));
    }

    #[test]
    fn the_sidecar_round_trips_and_a_bad_one_reads_as_absent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("cache.db");
        assert_eq!(last_vacuum(&db), None);
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        record_vacuum(&db, at)?;
        assert_eq!(last_vacuum(&db), Some(at));
        std::fs::write(sidecar_path(&db), b"garbage")?;
        assert_eq!(last_vacuum(&db), None);
        Ok(())
    }
}
