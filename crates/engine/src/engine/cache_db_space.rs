//! Giving `cache.db`'s free pages back to the filesystem.
//!
//! Artifacts up to the spill threshold live inline in sqlite, so every
//! `cache.history` trim deletes rows, and sqlite reuses the pages it frees but
//! never shrinks the file on its own. One measured cache was a 21 GB file
//! holding 11.6 GiB of live data.
//!
//! Three pieces, all run on the cache's single writer connection (the writer
//! thread, `local_cache_sqlite::writer_loop`), so they never race a cache write:
//!
//! - A database created by this binary has `auto_vacuum = INCREMENTAL` (set by
//!   `migrate_schema`). On such a database the end of a request that trimmed
//!   something runs [`MaintainKind::AfterTrim`]: when the [`ReclaimPolicy`] says
//!   it is due, `PRAGMA incremental_vacuum` in bounded chunks, queued cache
//!   writes committed between chunks, then `wal_checkpoint(TRUNCATE)`.
//! - `heph tool gc` runs [`MaintainKind::Gc`]: it converts a `NONE` database
//!   (`auto_vacuum = INCREMENTAL` + `VACUUM`) when the disk has room for the
//!   copies `VACUUM` makes, and vacuums an `INCREMENTAL` one fully.
//! - Every outcome is a [`CompactReport`] with the file's [`CacheDbSpace`]
//!   before and after, which `heph tool gc` prints.
//!
//! A conversion rewrites the whole file — minutes, on a large one — holding
//! its write lock and the schema migration lock throughout. Meanwhile another
//! heph process's cache writes wait: its writer retries the batches the lock
//! keeps out for up to `local_cache_sqlite::WRITE_BUSY_RETRY_TOTAL` in all (see
//! `commit_with_retry`), then gives each later one a single attempt until one
//! commits. A batch dropped — at cache close, past that deadline, or on an
//! error other than `SQLITE_BUSY` — is normally a miss: the residency probe
//! catches a manifest without its blob. On a `--force` rebuild of a
//! non-reproducible target whose revision already has rows, though, it can
//! leave a mixed revision, the pre-existing gap documented in
//! `Engine::cache_locally` (`local_cache.rs`). The conversion's temporary
//! copy goes to sqlite's temp directory
//! (`SQLITE_TMPDIR`, `TMPDIR`, then `/var/tmp`, `/usr/tmp`, `/tmp`), which on
//! Linux may be a tmpfs, i.e. RAM; the free-space check counts that
//! filesystem as well as the cache's.
//!
//! Nothing here reaches a hash or a cache key: it moves pages, never rows.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, TransactionBehavior};
use std::os::unix::fs::MetadataExt as _;
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
/// The writer connection's busy timeout, set in `spawn_writer`. Each attempt
/// of a write batch waits this long for another process's write lock; the
/// writer retries a batch that still gets `SQLITE_BUSY` (see
/// `local_cache_sqlite::WRITE_BUSY_RETRY_TOTAL`).
pub(crate) const WRITER_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a `wal_checkpoint(TRUNCATE)` after a vacuum waits for readers still
/// on an older snapshot, at the end of a request and in `heph tool gc` alike.
/// The end-of-request one runs in the exit drain; a checkpoint that cannot
/// finish is not an error, and the next one takes the rest.
const CHECKPOINT_BUSY: Duration = Duration::from_secs(1);
/// Headroom `heph tool gc` keeps on top of what a conversion `VACUUM` writes:
/// this, or a tenth of it, whichever is larger.
const CONVERT_MARGIN_MIN_BYTES: u64 = 256 * MIB;

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

/// A filesystem short of the space a conversion needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceShortfall {
    pub dir: PathBuf,
    pub needed_bytes: u64,
    pub available_bytes: u64,
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
    /// A `NONE` database was switched to `INCREMENTAL` by a full `VACUUM`.
    Converted { freed_bytes: u64 },
    /// A `NONE` database was left as is: the copies `VACUUM` writes would not
    /// fit.
    SkippedNeedsSpace { short: Vec<SpaceShortfall> },
    /// A `NONE` database was left as is: the migration lock could not be
    /// taken — another process holds it, i.e. is migrating or converting the
    /// same database, or the lock file failed. `reason` is that error.
    SkippedLocked { reason: String },
}

/// One maintenance pass: the file before and after, and what was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReport {
    pub before: CacheDbSpace,
    pub after: CacheDbSpace,
    pub action: CompactAction,
    /// Steps after the pages moved that failed without undoing the pass: the
    /// sidecar not recorded (the next request may vacuum again), the closing
    /// checkpoint not run (the next one takes it). Each is also logged.
    pub notes: Vec<String>,
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

    /// Whether the end-of-request reclaim should run: free pages at the floor,
    /// and either at the cap or the interval elapsed since the last successful
    /// vacuum. No record of a last vacuum, or one in the future (the clock
    /// moved back), counts as elapsed. The floor holds on the cap path too: on
    /// a small file 25 % can be a few pages, not worth a pass per request.
    pub(crate) fn due(
        &self,
        space: &CacheDbSpace,
        last_vacuum: Option<SystemTime>,
        now: SystemTime,
    ) -> bool {
        if space.free_bytes == 0 || space.free_bytes < self.min_free_bytes {
            return false;
        }
        if space.free_bytes >= self.cap(space) {
            return true;
        }
        last_vacuum.is_none_or(|t| {
            now.duration_since(t)
                .map_or(true, |since| since >= self.interval)
        })
    }
}

/// What the writer thread is asked to do.
#[derive(Debug, Clone)]
pub(crate) enum MaintainKind {
    /// End of request, after the history trim: vacuum an `INCREMENTAL`
    /// database if `policy` says it is due, within `policy.budget`.
    AfterTrim(ReclaimPolicy),
    /// `heph tool gc`: convert a `NONE` database (when the caller holds the
    /// migration lock and the disk has room), or vacuum an `INCREMENTAL` one
    /// completely.
    Gc {
        /// Why the caller does not hold the migration lock (its error), which
        /// skips a conversion; `None` when it does.
        convert_blocked: Option<String>,
        chunk_bytes: u64,
        /// Test hook: the free bytes every filesystem reports.
        free_space_override: Option<u64>,
    },
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
    let chunk_pages = i64::try_from((chunk_bytes / page_size).max(1)).unwrap_or(i64::MAX);
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
        // Stepped to completion: each step of `incremental_vacuum(N)` frees
        // one page, so `execute_batch` (one step) freed one page per chunk.
        // `pragma` iterates the statement's rows until SQLITE_DONE.
        tx.pragma(None, "incremental_vacuum", chunk_pages, |_| Ok(()))
            .context("running PRAGMA incremental_vacuum")?;
        tx.commit()
            .context("committing an incremental_vacuum transaction")?;
        if !between(conn) {
            return Ok(false);
        }
    }
}

/// `wal_checkpoint(TRUNCATE)`, waiting at most `busy` for readers on an older
/// snapshot, then putting the connection's own busy timeout back. A checkpoint
/// that could not finish is not an error: the next one takes the rest.
fn checkpoint_truncate(conn: &Connection, busy: Duration) -> Result<()> {
    let own_ms = pragma_u64(conn, "busy_timeout")?;
    conn.busy_timeout(busy)
        .context("setting the checkpoint busy timeout")?;
    let res = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    });
    conn.busy_timeout(Duration::from_millis(own_ms))
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
    let unchanged = |action| CompactReport {
        before,
        after: before,
        action,
        notes: Vec::new(),
    };
    let mut notes = Vec::new();
    let action = match kind {
        MaintainKind::AfterTrim(policy) => {
            let now = SystemTime::now();
            if before.mode != VacuumMode::Incremental
                || !policy.due(&before, last_vacuum(db_path), now)
            {
                return Ok(unchanged(CompactAction::Unchanged));
            }
            let deadline = Instant::now() + policy.budget;
            let complete = incremental(
                conn,
                policy.chunk_bytes,
                &|| keep_going() && Instant::now() < deadline,
                between,
            )?;
            finish(conn, db_path, complete, &mut notes);
            CompactAction::Vacuumed {
                freed_bytes: 0,
                complete,
            }
        }
        MaintainKind::Gc {
            convert_blocked,
            chunk_bytes,
            free_space_override,
        } => match before.mode {
            VacuumMode::Incremental => {
                let complete = incremental(conn, *chunk_bytes, keep_going, between)?;
                finish(conn, db_path, complete, &mut notes);
                CompactAction::Vacuumed {
                    freed_bytes: 0,
                    complete,
                }
            }
            VacuumMode::None | VacuumMode::Full => {
                if let Some(reason) = convert_blocked {
                    return Ok(unchanged(CompactAction::SkippedLocked {
                        reason: reason.clone(),
                    }));
                }
                let short = convert_shortfall(
                    &ConvertPlacement::probe(db_path)?,
                    before.live_bytes,
                    &|dir| available_bytes(dir, *free_space_override),
                )?;
                if !short.is_empty() {
                    return Ok(unchanged(CompactAction::SkippedNeedsSpace { short }));
                }
                convert(conn)?;
                finish(conn, db_path, true, &mut notes);
                CompactAction::Converted { freed_bytes: 0 }
            }
        },
    };
    // The pages have moved; failing to measure the result does not undo that.
    let after = space(conn, db_path).unwrap_or_else(|e| {
        note(&mut notes, "measuring cache.db after the pass", &e);
        before
    });
    let freed = before
        .file_bytes
        .saturating_add(before.wal_bytes)
        .saturating_sub(after.file_bytes.saturating_add(after.wal_bytes));
    let action = match action {
        CompactAction::Vacuumed { complete, .. } => CompactAction::Vacuumed {
            freed_bytes: freed,
            complete,
        },
        CompactAction::Converted { .. } => CompactAction::Converted { freed_bytes: freed },
        other => other,
    };
    Ok(CompactReport {
        before,
        after,
        action,
        notes,
    })
}

/// Record a step that failed after the pages moved: logged, and kept for the
/// report.
fn note(notes: &mut Vec<String>, what: &str, e: &anyhow::Error) {
    let msg = format!("{what} failed: {e:#}");
    tracing::warn!(error = %format!("{e:#}"), "cache.db maintenance: {what} failed");
    notes.push(msg);
}

/// The steps after a pass moved pages: truncate the WAL, and record the
/// vacuum when it `complete`d. Neither undoes the pass, so a failure is a
/// note, not an error: without the checkpoint the next one takes the WAL,
/// without the sidecar the next request may vacuum again.
fn finish(conn: &Connection, db_path: &Path, complete: bool, notes: &mut Vec<String>) {
    if let Err(e) = checkpoint_truncate(conn, CHECKPOINT_BUSY) {
        note(notes, "truncating the WAL", &e);
    }
    if complete && let Err(e) = record_vacuum(db_path, SystemTime::now()) {
        note(notes, "recording the vacuum", &e);
    }
}

/// Switch the database to `INCREMENTAL` with a full `VACUUM`.
///
/// `VACUUM` builds the new database as a temporary file, so the connection's
/// `temp_store = MEMORY` would put the whole database in RAM; it is switched to
/// `FILE` for the statement and back afterwards, whether it succeeded or not.
fn convert(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA temp_store = FILE;")
        .context("switching temp_store to FILE for VACUUM")?;
    let res = conn
        .execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")
        .context("converting cache.db to auto_vacuum = INCREMENTAL (VACUUM)");
    let restored = conn
        .execute_batch("PRAGMA temp_store = MEMORY;")
        .context("restoring temp_store = MEMORY after VACUUM");
    // A failed VACUUM changed nothing on disk (it is one transaction) and is
    // the error worth reporting; the restore's comes second.
    res.and(restored)
}

/// `bytes` plus the conversion headroom.
fn with_margin(bytes: u64) -> u64 {
    bytes.saturating_add(CONVERT_MARGIN_MIN_BYTES.max(bytes / 10))
}

/// The directory sqlite puts its temporary files in, resolved as its unix VFS
/// does (`unixTempFileDir`): `SQLITE_TMPDIR`, `TMPDIR`, then `/var/tmp`,
/// `/usr/tmp`, `/tmp`, the first that is a writable directory.
fn sqlite_temp_dir() -> Option<PathBuf> {
    let env = ["SQLITE_TMPDIR", "TMPDIR"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from);
    let fixed = ["/var/tmp", "/usr/tmp", "/tmp"]
        .into_iter()
        .map(PathBuf::from);
    env.chain(fixed).find(|d| {
        d.is_dir()
            && rustix::fs::access(
                d,
                rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
            )
            .is_ok()
    })
}

fn available_bytes(dir: &Path, override_bytes: Option<u64>) -> Result<u64> {
    if let Some(b) = override_bytes {
        return Ok(b);
    }
    let st = rustix::fs::statvfs(dir).with_context(|| format!("statvfs {dir:?}"))?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

/// Where a conversion `VACUUM` writes, and on which filesystem: the cache
/// directory (the `-wal`) and sqlite's temp directory (its temporary copy).
#[derive(Debug, Clone)]
struct ConvertPlacement {
    cache_dir: PathBuf,
    cache_dev: u64,
    /// sqlite's temp directory and its device; `None` when no candidate is a
    /// writable directory (`VACUUM` then fails on its own and says so).
    temp: Option<(PathBuf, u64)>,
}

impl ConvertPlacement {
    fn probe(db_path: &Path) -> Result<Self> {
        let cache_dir = db_path
            .parent()
            .with_context(|| format!("{db_path:?} has no parent directory"))?
            .to_path_buf();
        let cache_dev = std::fs::metadata(&cache_dir)
            .with_context(|| format!("stat {cache_dir:?}"))?
            .dev();
        let temp = sqlite_temp_dir().and_then(|d| std::fs::metadata(&d).ok().map(|m| (d, m.dev())));
        Ok(Self {
            cache_dir,
            cache_dev,
            temp,
        })
    }
}

/// The filesystems a conversion `VACUUM` of `live_bytes` would run out of
/// space on, given what `available` reports for a directory; empty when it
/// fits.
///
/// `VACUUM` writes the database twice: a temporary copy in sqlite's temp
/// directory, then (in WAL mode) every page again into the `-wal` next to the
/// database, which the closing checkpoint folds back. The old file is only
/// shrunk at the end, so neither write can reuse its space.
///
/// The temp copy lands wherever sqlite's temp directory is, and on Linux that
/// is often a tmpfs, i.e. RAM: then the check is against the tmpfs's free
/// space, and a conversion too large for it is skipped and says so. sqlite
/// has no per-connection setting to move it next to the database —
/// `PRAGMA temp_store_directory` and `SQLITE_TMPDIR` are process-global —
/// so `SQLITE_TMPDIR` set for the gc command is the way to point it elsewhere.
fn convert_shortfall(
    placement: &ConvertPlacement,
    live_bytes: u64,
    available: &dyn Fn(&Path) -> Result<u64>,
) -> Result<Vec<SpaceShortfall>> {
    let mut needs: Vec<(&Path, u64)> = Vec::with_capacity(2);
    match &placement.temp {
        Some((t, dev)) if *dev != placement.cache_dev => {
            needs.push((&placement.cache_dir, with_margin(live_bytes)));
            needs.push((t, with_margin(live_bytes)));
        }
        // Same filesystem (or none sqlite would use): both copies land on the
        // cache's.
        _ => needs.push((
            &placement.cache_dir,
            with_margin(live_bytes.saturating_mul(2)),
        )),
    }
    let mut short = Vec::new();
    for (dir, needed) in needs {
        let available = available(dir)?;
        if available < needed {
            short.push(SpaceShortfall {
                dir: dir.to_path_buf(),
                needed_bytes: needed,
                available_bytes: available,
            });
        }
    }
    Ok(short)
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
        // A tiny file mostly free, but under the floor: the cap needs it too.
        assert!(!p.due(&space_with(MIB, MIB), recent, now));
        assert!(!p.due(&space_with(RECLAIM_MIN_FREE_BYTES - 1, MIB), recent, now));
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

    #[test]
    fn statvfs_reports_free_space_for_a_real_directory() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(available_bytes(dir.path(), None)? > 0);
        assert_eq!(available_bytes(dir.path(), Some(7))?, 7);
        Ok(())
    }

    fn placement(temp: Option<(&str, u64)>) -> ConvertPlacement {
        ConvertPlacement {
            cache_dir: PathBuf::from("/cache"),
            cache_dev: 1,
            temp: temp.map(|(d, dev)| (PathBuf::from(d), dev)),
        }
    }

    /// `available` for [`convert_shortfall`]: `cache` free in `/cache`, `tmp`
    /// anywhere else.
    fn avail(cache: u64, tmp: u64) -> impl Fn(&Path) -> Result<u64> {
        move |d| Ok(if d == Path::new("/cache") { cache } else { tmp })
    }

    /// Temp directory on the cache's filesystem (or none found): both copies
    /// are charged to it, twice the live bytes plus the margin.
    #[test]
    fn a_conversion_on_one_filesystem_needs_twice_the_live_bytes_there() -> Result<()> {
        let live = 10 * 1024 * MIB;
        let needed = with_margin(2 * live);
        for p in [placement(Some(("/tmp", 1))), placement(None)] {
            assert_eq!(convert_shortfall(&p, live, &avail(needed, 0))?, []);
            assert_eq!(
                convert_shortfall(&p, live, &avail(needed - 1, u64::MAX))?,
                [SpaceShortfall {
                    dir: PathBuf::from("/cache"),
                    needed_bytes: needed,
                    available_bytes: needed - 1,
                }],
                "{p:?}"
            );
        }
        Ok(())
    }

    /// Temp directory on another filesystem (a tmpfs, say): each is charged
    /// one copy, and each can be the one short.
    #[test]
    fn a_conversion_across_filesystems_needs_one_copy_on_each() -> Result<()> {
        let live = 10 * 1024 * MIB;
        let one = with_margin(live);
        let p = placement(Some(("/tmpfs", 2)));
        assert_eq!(convert_shortfall(&p, live, &avail(one, one))?, []);
        let short = convert_shortfall(&p, live, &avail(one, one - 1))?;
        assert_eq!(
            short,
            [SpaceShortfall {
                dir: PathBuf::from("/tmpfs"),
                needed_bytes: one,
                available_bytes: one - 1,
            }]
        );
        let short = convert_shortfall(&p, live, &avail(one - 1, one))?;
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].dir, PathBuf::from("/cache"));
        Ok(())
    }

    /// A database at `path` in `mode` with ~1.3 MiB of rows written then
    /// deleted (their pages free) and one row, `keep`, that stays.
    fn db_with_free_pages(path: &Path, mode: &str) -> Result<Connection> {
        let conn = Connection::open(path)?;
        conn.execute_batch(&format!(
            "PRAGMA page_size = 8192;
             PRAGMA auto_vacuum = {mode};
             PRAGMA journal_mode = WAL;
             PRAGMA temp_store = MEMORY;
             CREATE TABLE artifacts (
                 addr TEXT NOT NULL, hashin TEXT NOT NULL, name TEXT NOT NULL,
                 data BLOB NOT NULL, PRIMARY KEY (addr, hashin, name));
             INSERT INTO artifacts VALUES ('//p:t', 'h', 'keep', x'00');"
        ))?;
        let blob = vec![0x5a_u8; 32 * 1024];
        for i in 0..40 {
            conn.execute(
                "INSERT INTO artifacts VALUES ('//p:t', 'h', ?1, ?2)",
                rusqlite::params![format!("b{i}"), blob],
            )?;
        }
        conn.execute("DELETE FROM artifacts WHERE name != 'keep'", [])?;
        assert!(pragma_u64(&conn, "freelist_count")? > 100);
        Ok(conn)
    }

    fn gc_kind() -> MaintainKind {
        MaintainKind::Gc {
            convert_blocked: None,
            chunk_bytes: 64 * 1024,
            free_space_override: Some(u64::MAX),
        }
    }

    /// A vacuum whose sidecar cannot be written still reports the vacuum —
    /// the pages moved — with the failure as a note, not an error.
    #[test]
    fn an_unwritable_sidecar_still_reports_the_vacuum() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("cache.db");
        let mut conn = db_with_free_pages(&path, "INCREMENTAL")?;
        std::fs::create_dir(sidecar_path(&path))?;

        let report = run(&mut conn, &path, &gc_kind(), &|| true, &mut |_| true)?;

        assert!(
            matches!(
                report.action,
                CompactAction::Vacuumed { complete: true, .. }
            ),
            "{report:?}"
        );
        assert_eq!(report.after.free_bytes, 0, "{report:?}");
        assert!(
            matches!(report.notes.as_slice(), [n] if n.contains("recording the vacuum")),
            "{report:?}"
        );
        assert!(sidecar_path(&path).is_dir(), "left as it was");
        Ok(())
    }

    /// A `VACUUM` that fails (here: another connection holds the write lock
    /// past the busy timeout) changes nothing: the database is still `NONE`
    /// with its rows, the connection is back on `temp_store = MEMORY`, and its
    /// next write commits.
    #[test]
    fn a_failed_conversion_leaves_the_database_and_connection_as_they_were() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("cache.db");
        let mut conn = db_with_free_pages(&path, "NONE")?;
        conn.busy_timeout(Duration::from_millis(50))?;
        let holder = Connection::open(&path)?;
        holder.execute_batch("BEGIN IMMEDIATE;")?;

        let err = run(&mut conn, &path, &gc_kind(), &|| true, &mut |_| true)
            .expect_err("VACUUM cannot take the write lock");
        assert!(format!("{err:#}").contains("VACUUM"), "{err:#}");

        holder.execute_batch("ROLLBACK;")?;
        drop(holder);
        let fresh = Connection::open(&path)?;
        assert_eq!(pragma_u64(&fresh, "auto_vacuum")?, 0, "still NONE");
        let rows: i64 = fresh.query_row("SELECT count(*) FROM artifacts", [], |r| r.get(0))?;
        assert_eq!(rows, 1, "the kept row is intact");
        assert_eq!(pragma_u64(&conn, "temp_store")?, 2, "temp_store = MEMORY");
        assert!(conn.is_autocommit(), "no transaction left open");
        conn.execute(
            "INSERT INTO artifacts VALUES ('//p:t', 'h', 'after', x'01')",
            [],
        )?;
        assert!(last_vacuum(&path).is_none(), "nothing recorded");
        Ok(())
    }
}
