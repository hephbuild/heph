//! The index's sqlite store: the schema, its migrations, the creation of this
//! checkout's db file, and the write transaction every commit goes through.
//!
//! Hygiene (spec Decision 15): the version is `PRAGMA user_version`; the pool
//! comes from [`hsqlite::pool_builder`]; WAL with `synchronous = NORMAL`,
//! never `OFF`; migrations are transactional and additive and run under the
//! [`crate::migrate_lock`] flock. A newer `user_version`, a corrupt or
//! unreadable db, or one still locked after the busy timeout is a loud error
//! naming the file, never an empty index; there is no "writes disabled"
//! fallback.
//!
//! Generations (Decision 22): every commit bumps `meta.generation` inside its
//! transaction and tags the rows it writes with the result in `gen`, so a
//! reader that loaded through generation `G` knows exactly which committed
//! rows it has.

use super::ImportLockGuard;
use super::fold::fold_hash;
use crate::migrate_lock::{MIGRATE_LOCK_TIMEOUT, lock_migration};
use anyhow::{Context as _, Result, bail};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::types::Type;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The schema version this binary writes, in `PRAGMA user_version`: the
/// number of [`MIGRATIONS`] applied.
pub const SCHEMA_VERSION: i64 = 1;

/// The schema, one additive step per version. Step `i` takes a db from
/// version `i` to `i + 1`; a db at version `v` runs steps `v..`.
const MIGRATIONS: &[&str] = &[
    // 1: the files table and the single meta row.
    "CREATE TABLE files (
         path            BLOB PRIMARY KEY NOT NULL,
         fold            INTEGER NOT NULL,
         addr            TEXT NOT NULL,
         output          TEXT,
         kind            TEXT NOT NULL CHECK (kind IN ('file', 'symlink')),
         hash            TEXT,
         size            INTEGER,
         mtime_ns        INTEGER,
         ino             INTEGER,
         dev             INTEGER,
         mode            INTEGER,
         link_target     BLOB,
         pending_hash    TEXT,
         state           TEXT NOT NULL CHECK (state IN ('written', 'imported', 'dropped')),
         seen_in_section INTEGER NOT NULL DEFAULT 0,
         written_at      INTEGER,
         gen             INTEGER NOT NULL
     );
     CREATE INDEX files_fold ON files (fold);
     CREATE INDEX files_addr ON files (addr);
     CREATE INDEX files_gen ON files (gen);
     CREATE TABLE meta (
         id                    INTEGER PRIMARY KEY CHECK (id = 0),
         generation            INTEGER NOT NULL,
         no_section_hint_shown INTEGER NOT NULL DEFAULT 0
     );
     INSERT INTO meta (id, generation) VALUES (0, 0);",
];

/// How long a statement waits on another connection's lock before the store
/// gives up with an error naming the file.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// The name prefix of a db being built, in `<home>/codegen/`. A file with it
/// is only ever live under the import lock, so the next creator deletes any
/// it finds as a crash's leftover.
pub const TMP_PREFIX: &str = "codegen.tmp-";

/// Timeouts of a [`Store`]. The defaults are the product's; tests shorten them.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// `PRAGMA busy_timeout`, and how long the pool waits to open a connection.
    pub busy_timeout: Duration,
    /// How long a migration waits for another process's migration lock.
    pub migrate_lock_timeout: Duration,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            migrate_lock_timeout: MIGRATE_LOCK_TIMEOUT,
        }
    }
}

/// What a row records at its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    File,
    Symlink,
}

impl RowKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Symlink => "symlink",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "file" => Some(Self::File),
            "symlink" => Some(Self::Symlink),
            _ => None,
        }
    }
}

/// How a row came to be, and whether it still counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    /// Bytes a write-back of `addr` is known to have written.
    Written,
    /// Bytes found by the import; never deleted on this record (J5).
    Imported,
    /// A leftover a write-back did not produce, its file kept. Never needs
    /// work again.
    Dropped,
}

impl RowState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::Imported => "imported",
            Self::Dropped => "dropped",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "written" => Some(Self::Written),
            "imported" => Some(Self::Imported),
            "dropped" => Some(Self::Dropped),
            _ => None,
        }
    }
}

/// A row's last verified record: the content hash and the stat taken when it
/// was verified. All of it or none of it is in the db.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub hash: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub ino: u64,
    pub dev: u64,
    /// `st_mode`, file-type bits included.
    pub mode: u32,
    /// When the record was taken (ns since the epoch): a recorded mtime not
    /// strictly older than this is checked by hash, never by stat (git's
    /// racy-clean rule).
    pub written_at_ns: i64,
}

/// One row of `files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Root-relative, normalised (`paths::normalize_rel`).
    pub path: Vec<u8>,
    /// [`fold_hash`] of `path`; computed again on every write.
    pub fold: u64,
    pub addr: String,
    /// The declared output that produced it; `None` after an import.
    pub output: Option<String>,
    pub kind: RowKind,
    /// `None` for a new path's first row, which has only `pending_hash`.
    pub record: Option<Record>,
    /// A symlink's target.
    pub link_target: Option<Vec<u8>>,
    /// Set while a write-back is in flight, or after one crashed: every
    /// reader excludes the path whatever its bytes.
    pub pending_hash: Option<String>,
    pub state: RowState,
    /// A section line has matched the path on some write path. Only ever set.
    pub seen_in_section: bool,
    /// The generation of the commit that last wrote the row. Set by
    /// [`WriteTxn::put`], whatever the caller left here.
    pub generation: u64,
}

impl Row {
    /// A `written` row with no record yet.
    pub fn new(path: impl Into<Vec<u8>>, addr: impl Into<String>, kind: RowKind) -> Self {
        let path = path.into();
        Self {
            fold: fold_hash(&path),
            path,
            addr: addr.into(),
            output: None,
            kind,
            record: None,
            link_target: None,
            pending_hash: None,
            state: RowState::Written,
            seen_in_section: false,
            generation: 0,
        }
    }
}

/// The columns of `files`, in [`row_from_sql`]'s order.
const COLS: &str = "path, fold, addr, output, kind, hash, size, mtime_ns, ino, dev, mode, \
                    link_target, pending_hash, state, seen_in_section, written_at, gen";

/// sqlite integers are signed; `u64`s (hashes, inodes, generations) are
/// stored bit for bit.
fn to_sql_u64(v: u64) -> i64 {
    i64::from_ne_bytes(v.to_ne_bytes())
}

fn from_sql_u64(v: i64) -> u64 {
    u64::from_ne_bytes(v.to_ne_bytes())
}

fn bad_column(idx: usize, ty: Type, what: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, ty, what.into())
}

fn row_from_sql(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let kind: String = r.get(4)?;
    let kind = RowKind::parse(&kind)
        .ok_or_else(|| bad_column(4, Type::Text, format!("unknown row kind {kind:?}")))?;
    let state: String = r.get(13)?;
    let state = RowState::parse(&state)
        .ok_or_else(|| bad_column(13, Type::Text, format!("unknown row state {state:?}")))?;
    let record = match r.get::<_, Option<String>>(5)? {
        None => None,
        Some(hash) => {
            // A hash with no stat is a broken row: `get` on NULL errors.
            let mode: i64 = r.get(10)?;
            Some(Record {
                hash,
                size: from_sql_u64(r.get(6)?),
                mtime_ns: r.get(7)?,
                ino: from_sql_u64(r.get(8)?),
                dev: from_sql_u64(r.get(9)?),
                mode: u32::try_from(mode)
                    .map_err(|e| bad_column(10, Type::Integer, format!("mode {mode}: {e}")))?,
                written_at_ns: r.get(15)?,
            })
        }
    };
    Ok(Row {
        path: r.get(0)?,
        fold: from_sql_u64(r.get(1)?),
        addr: r.get(2)?,
        output: r.get(3)?,
        kind,
        record,
        link_target: r.get(11)?,
        pending_hash: r.get(12)?,
        state,
        seen_in_section: r.get(14)?,
        generation: from_sql_u64(r.get(16)?),
    })
}

fn query_rows(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> rusqlite::Result<Vec<Row>> {
    let mut stmt = conn.prepare_cached(sql)?;
    stmt.query_map(p, row_from_sql)?.collect()
}

/// An open index db. Opened only when the file exists: [`create`] is the one
/// way a db comes into being.
pub struct Store {
    path: PathBuf,
    pool: r2d2::Pool<SqliteConnectionManager>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Open the existing db at `path`, migrating it if it is older than
    /// [`SCHEMA_VERSION`]. Refuses a newer one with no DDL and no write.
    pub fn open(path: &Path, opts: &StoreOptions) -> Result<Self> {
        // The checks run on a connection of their own, not a pooled one: r2d2
        // retries a failing connect until its timeout, so a corrupt file
        // would stall the open that long before it errored.
        let conn = Connection::open_with_flags(path, open_flags())
            .and_then(|c| configure(&c, opts.busy_timeout).map(|()| c))
            .with_context(|| format!("opening the codegen index {}", path.display()))?;
        let version = user_version(&conn, path)?;
        check_not_newer(version, path)?;
        if version < SCHEMA_VERSION {
            migrate(&conn, path, opts)?;
        }
        // Reading the generation reaches the meta table's pages: a db whose
        // header reads but whose tables do not fails here, not at first use.
        read_generation(&conn).with_context(|| {
            format!(
                "reading the generation of the codegen index {}",
                path.display()
            )
        })?;
        drop(conn);
        Ok(Self {
            path: path.to_path_buf(),
            pool: build_pool(path, opts)?,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        self.pool
            .get()
            .with_context(|| format!("connecting to the codegen index {}", self.path.display()))
    }

    /// `meta.generation`: the generation of the last committed write.
    pub fn generation(&self) -> Result<u64> {
        let conn = self.conn()?;
        read_generation(&conn).with_context(|| {
            format!(
                "reading the generation of the codegen index {}",
                self.path.display()
            )
        })
    }

    /// The generation and every row, read in one read transaction, so the
    /// rows are exactly those committed at or below the generation.
    pub fn load_all(&self) -> Result<(u64, Vec<Row>)> {
        let conn = self.conn()?;
        let load = || -> rusqlite::Result<(u64, Vec<Row>)> {
            let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
            let generation = read_generation(&tx)?;
            let rows = query_rows(&tx, &format!("SELECT {COLS} FROM files"), [])?;
            tx.commit()?;
            Ok((generation, rows))
        };
        load().with_context(|| format!("loading the codegen index {}", self.path.display()))
    }

    /// The rows committed above generation `gen`, through the `files(gen)`
    /// index: what a glob's end-of-walk recheck tests.
    pub fn rows_above(&self, generation: u64) -> Result<Vec<Row>> {
        let conn = self.conn()?;
        query_rows(
            &conn,
            &format!("SELECT {COLS} FROM files WHERE gen > ?1"),
            [to_sql_u64(generation)],
        )
        .with_context(|| {
            format!(
                "reading new rows of the codegen index {}",
                self.path.display()
            )
        })
    }

    /// One target's rows, as committed now.
    pub fn rows_for_addr(&self, addr: &str) -> Result<Vec<Row>> {
        let conn = self.conn()?;
        query_rows(
            &conn,
            &format!("SELECT {COLS} FROM files WHERE addr = ?1"),
            [addr],
        )
        .with_context(|| {
            format!(
                "reading {addr}'s rows of the codegen index {}",
                self.path.display()
            )
        })
    }

    /// Run one write: `BEGIN IMMEDIATE`, bump the generation, `f`, read back
    /// the rows tagged with it, commit. The returned [`Committed`] is what the
    /// caller publishes to the in-process snapshot, after this returns.
    ///
    /// Waiting on another writer is bounded by the busy timeout; past it the
    /// error names the file. A failure anywhere rolls the whole write back.
    pub fn write<T>(&self, f: impl FnOnce(&mut WriteTxn<'_>) -> Result<T>) -> Result<Committed<T>> {
        let conn = self.conn()?;
        let path = self.path.display();
        let tx = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)
            .with_context(|| format!("starting a write to the codegen index {path}"))?;
        let generation = tx
            .query_row(
                "UPDATE meta SET generation = generation + 1 WHERE id = 0 RETURNING generation",
                [],
                |r| r.get::<_, i64>(0),
            )
            .with_context(|| format!("bumping the generation of the codegen index {path}"))?;
        let generation = from_sql_u64(generation);
        let mut txn = WriteTxn {
            tx,
            generation,
            tombstones: Vec::new(),
        };
        let value = f(&mut txn).with_context(|| format!("writing the codegen index {path}"))?;
        let WriteTxn { tx, tombstones, .. } = txn;
        let rows = query_rows(
            &tx,
            &format!("SELECT {COLS} FROM files WHERE gen = ?1"),
            [to_sql_u64(generation)],
        )
        .with_context(|| format!("reading back a write to the codegen index {path}"))?;
        tx.commit()
            .with_context(|| format!("committing a write to the codegen index {path}"))?;
        Ok(Committed {
            generation,
            rows,
            tombstones,
            value,
        })
    }
}

/// A write in progress (see [`Store::write`]). Every row it puts is tagged
/// with its generation.
pub struct WriteTxn<'c> {
    tx: Transaction<'c>,
    generation: u64,
    tombstones: Vec<Vec<u8>>,
}

impl std::fmt::Debug for WriteTxn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTxn")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl WriteTxn<'_> {
    /// This commit's generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The transaction's connection, for a statement the helpers do not
    /// cover. A statement that writes a row must set `gen = self.generation()`, or
    /// the row escapes the publish and the end-of-walk recheck.
    pub fn conn(&self) -> &Connection {
        &self.tx
    }

    /// Insert or replace `row` at its path, tagged with this generation.
    pub fn put(&mut self, row: &Row) -> Result<()> {
        let record = row.record.as_ref();
        self.tx
            .prepare_cached(&format!(
                "INSERT OR REPLACE INTO files ({COLS}) VALUES \
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)"
            ))?
            .execute(params![
                row.path,
                to_sql_u64(fold_hash(&row.path)),
                row.addr,
                row.output,
                row.kind.as_str(),
                record.map(|r| &r.hash),
                record.map(|r| to_sql_u64(r.size)),
                record.map(|r| r.mtime_ns),
                record.map(|r| to_sql_u64(r.ino)),
                record.map(|r| to_sql_u64(r.dev)),
                record.map(|r| i64::from(r.mode)),
                row.link_target,
                row.pending_hash,
                row.state.as_str(),
                row.seen_in_section,
                record.map(|r| r.written_at_ns),
                to_sql_u64(self.generation),
            ])
            .with_context(|| format!("recording {}", String::from_utf8_lossy(&row.path)))?;
        Ok(())
    }

    /// Delete the row at `path` if `addr` owns it: every delete names the
    /// addr, so another target's row is never touched. Reported as a
    /// tombstone in the [`Committed`].
    pub fn delete(&mut self, path: &[u8], addr: &str) -> Result<bool> {
        let n = self
            .tx
            .prepare_cached("DELETE FROM files WHERE path = ?1 AND addr = ?2")?
            .execute(params![path, addr])
            .with_context(|| format!("deleting the row of {}", String::from_utf8_lossy(path)))?;
        if n > 0 {
            self.tombstones.push(path.to_vec());
        }
        Ok(n > 0)
    }

    /// One target's rows as this transaction sees them: the reselect a
    /// write-back runs under its lock.
    pub fn rows_for_addr(&self, addr: &str) -> Result<Vec<Row>> {
        query_rows(
            &self.tx,
            &format!("SELECT {COLS} FROM files WHERE addr = ?1"),
            [addr],
        )
        .with_context(|| format!("reading {addr}'s rows"))
    }
}

/// A committed write: its generation, the rows it left tagged with it, the
/// paths it deleted, and the closure's value.
#[derive(Debug)]
pub struct Committed<T> {
    pub generation: u64,
    pub rows: Vec<Row>,
    pub tombstones: Vec<Vec<u8>>,
    pub value: T,
}

/// No `CREATE`: a db that vanished is an error, never a new empty one.
fn open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

/// Every connection's pragmas: the busy timeout, and `synchronous = NORMAL`
/// (never `OFF`: the index is not a cache, a lost row changes answers).
fn configure(c: &Connection, busy: Duration) -> rusqlite::Result<()> {
    c.busy_timeout(busy)?;
    c.execute_batch("PRAGMA synchronous = NORMAL;")
}

fn build_pool(path: &Path, opts: &StoreOptions) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let busy = opts.busy_timeout;
    let manager = SqliteConnectionManager::file(path)
        .with_flags(open_flags())
        .with_init(move |c| configure(c, busy));
    // Its helper thread exits with the pool; see `hsqlite`.
    hsqlite::pool_builder()
        .max_size(8)
        // Opened by the engine's first request; each use opens what it needs.
        .min_idle(Some(0))
        // An open that keeps failing is retried until this; bounded like a
        // statement, so a broken file is an error, not a hang.
        .connection_timeout(opts.busy_timeout.max(Duration::from_millis(500)))
        .test_on_check_out(false)
        .build(manager)
        .with_context(|| format!("building the pool of the codegen index {}", path.display()))
}

fn read_generation(conn: &Connection) -> rusqlite::Result<u64> {
    conn.prepare_cached("SELECT generation FROM meta WHERE id = 0")?
        .query_row([], |r| r.get::<_, i64>(0))
        .map(from_sql_u64)
}

fn user_version(conn: &Connection, path: &Path) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .with_context(|| {
            format!(
                "reading the schema version of the codegen index {}",
                path.display()
            )
        })
}

fn check_not_newer(version: i64, path: &Path) -> Result<()> {
    if version > SCHEMA_VERSION {
        bail!(
            "the codegen index {} has schema version {version}, newer than the {SCHEMA_VERSION} \
             this heph knows: upgrade heph (downgrading a migrated index is unsupported)",
            path.display()
        );
    }
    Ok(())
}

/// Bring the db at `path` up to [`SCHEMA_VERSION`] under its migration lock,
/// re-reading the version under the lock.
fn migrate(conn: &Connection, path: &Path, opts: &StoreOptions) -> Result<()> {
    let _lock = lock_migration(path, opts.migrate_lock_timeout).with_context(|| {
        format!(
            "serializing the migration of the codegen index {}",
            path.display()
        )
    })?;
    let version = user_version(conn, path)?;
    check_not_newer(version, path)?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    set_wal(conn, path)?;
    apply_migrations(conn, version, MIGRATIONS)
        .with_context(|| format!("migrating the codegen index {}", path.display()))
}

fn set_wal(conn: &Connection, path: &Path) -> Result<()> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .with_context(|| format!("switching the codegen index {} to WAL", path.display()))?;
    if !mode.eq_ignore_ascii_case("wal") {
        bail!(
            "the codegen index {} stays in journal mode {mode:?}, not WAL",
            path.display()
        );
    }
    Ok(())
}

/// Apply `steps[from..]` and stamp the version, in one `BEGIN IMMEDIATE`:
/// a failing step leaves the db exactly as it was.
fn apply_migrations(conn: &Connection, from: i64, steps: &[&str]) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("starting the migration")?;
    let first = usize::try_from(from).context("negative schema version")?;
    for (i, step) in steps.iter().enumerate().skip(first) {
        tx.execute_batch(step)
            .with_context(|| format!("applying schema step {}", i + 1))?;
    }
    let to = i64::try_from(steps.len()).context("schema version overflow")?;
    tx.execute_batch(&format!("PRAGMA user_version = {to};"))
        .context("stamping the schema version")?;
    tx.commit().context("committing the migration")
}

/// Create this checkout's db at `db_path`, if it is still missing. The caller
/// holds the import lock (every creator does), so a rename never replaces a
/// db. Returns whether it created one.
///
/// Under the lock: delete stale `codegen.tmp-*` files a crashed creator left,
/// re-check that the db is missing, build a complete migrated db in a temp
/// file, checkpoint and close it so no `-wal` remains, fsync it, rename it
/// onto `db_path`, fsync the directory. No reader ever sees a partial db, and
/// nothing replaces `<home>/codegen/` itself, so another checkout's db in a
/// shared home is untouched.
pub fn create(db_path: &Path, _import: &ImportLockGuard) -> Result<bool> {
    let dir = db_path
        .parent()
        .with_context(|| format!("the codegen index {} has no directory", db_path.display()))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating the codegen index directory {}", dir.display()))?;
    remove_stale_tmp(dir)?;
    if std::fs::symlink_metadata(db_path).is_ok() {
        return Ok(false);
    }
    let tmp = dir.join(tmp_name());
    let built = build_fresh(&tmp).and_then(|()| {
        std::fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .with_context(|| format!("syncing {}", tmp.display()))?;
        std::fs::rename(&tmp, db_path).with_context(|| {
            format!(
                "moving {} onto the codegen index {}",
                tmp.display(),
                db_path.display()
            )
        })
    });
    if let Err(e) = built {
        remove_db_files(&tmp);
        return Err(e);
    }
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("syncing the codegen index directory {}", dir.display()))?;
    Ok(true)
}

/// A temp db name no other creator picks: pid, time and a process counter.
fn tmp_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{TMP_PREFIX}{}-{nanos:x}-{seq}.db", std::process::id())
}

fn build_fresh(tmp: &Path) -> Result<()> {
    let conn = Connection::open(tmp)
        .with_context(|| format!("creating the codegen index {}", tmp.display()))?;
    set_wal(&conn, tmp)?;
    apply_migrations(&conn, 0, MIGRATIONS)
        .with_context(|| format!("initialising the codegen index {}", tmp.display()))?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .with_context(|| format!("checkpointing the codegen index {}", tmp.display()))?;
    // The last connection's close checkpoints again and deletes `-wal` and
    // `-shm`, so the renamed file is the whole db.
    conn.close()
        .map_err(|(_, e)| e)
        .with_context(|| format!("closing the codegen index {}", tmp.display()))
}

/// Remove `<home>/codegen/codegen.tmp-*`: only a creator holding the import
/// lock writes one, so any found by the next holder is a crash's leftover.
fn remove_stale_tmp(dir: &Path) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("listing the codegen index directory {}", dir.display()))?;
    for entry in entries {
        let entry = entry
            .with_context(|| format!("listing the codegen index directory {}", dir.display()))?;
        if entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(TMP_PREFIX.as_bytes())
        {
            let p = entry.path();
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("removing stale {}", p.display()));
                }
            }
        }
    }
    Ok(())
}

/// Best-effort cleanup of a temp db that failed to build.
fn remove_db_files(tmp: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = tmp.as_os_str().to_owned();
        p.push(suffix);
        drop(std::fs::remove_file(PathBuf::from(p)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn guard() -> ImportLockGuard {
        ImportLockGuard::new(Box::new(()))
    }

    fn fresh(dir: &Path) -> PathBuf {
        let db = dir.join("codegen").join("index.db");
        assert!(create(&db, &guard()).unwrap());
        db
    }

    fn opts(busy: Duration) -> StoreOptions {
        StoreOptions {
            busy_timeout: busy,
            migrate_lock_timeout: busy,
        }
    }

    fn schema(db: &Path) -> Vec<(String, String)> {
        let c = Connection::open(db).unwrap();
        let mut s = c
            .prepare("SELECT name, COALESCE(sql, '') FROM sqlite_master ORDER BY name")
            .unwrap();
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn schema_version_is_the_number_of_migrations() {
        assert_eq!(usize::try_from(SCHEMA_VERSION).unwrap(), MIGRATIONS.len());
    }

    #[test]
    fn a_row_round_trips_and_every_write_bumps_the_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&fresh(dir.path()), &StoreOptions::default()).unwrap();
        assert_eq!(store.generation().unwrap(), 0);
        let mut row = Row::new(b"p/a.go".to_vec(), "//p:gen", RowKind::File);
        row.record = Some(Record {
            hash: "h".into(),
            size: u64::MAX,
            mtime_ns: -5,
            ino: u64::MAX - 1,
            dev: 7,
            mode: 0o100_644,
            written_at_ns: 9,
        });
        row.link_target = Some(b"\xff".to_vec());
        row.pending_hash = Some("p".into());
        row.seen_in_section = true;
        let c = store.write(|w| w.put(&row)).unwrap();
        assert_eq!(c.generation, 1);
        row.generation = 1;
        assert_eq!(c.rows, vec![row.clone()]);
        let c = store.write(|w| w.delete(b"p/a.go", "//other:x")).unwrap();
        assert!(!c.value, "another addr's delete touches nothing");
        assert_eq!(c.generation, 2);
        assert_eq!(store.load_all().unwrap(), (2, vec![row]));
        let c = store.write(|w| w.delete(b"p/a.go", "//p:gen")).unwrap();
        assert_eq!(c.tombstones, vec![b"p/a.go".to_vec()]);
        assert_eq!(store.load_all().unwrap(), (3, vec![]));
    }

    #[test]
    fn a_failed_write_commits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&fresh(dir.path()), &StoreOptions::default()).unwrap();
        let err = store
            .write(|w| -> Result<()> {
                w.put(&Row::new(b"a".to_vec(), "//a:a", RowKind::File))?;
                anyhow::bail!("boom")
            })
            .unwrap_err();
        assert!(format!("{err:#}").contains("boom"));
        assert_eq!(store.load_all().unwrap(), (0, vec![]));
    }

    #[test]
    fn corrupt_index_errors_naming_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        std::fs::write(&db, vec![0xa5_u8; 8192]).unwrap();
        let err = Store::open(&db, &opts(Duration::from_millis(500))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&db.display().to_string()), "{msg}");
        assert!(msg.contains("not a database"), "{msg}");
    }

    #[test]
    fn locked_index_errors_after_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh(dir.path());
        let timeout = Duration::from_millis(300);
        let store = Store::open(&db, &opts(timeout)).unwrap();
        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let start = Instant::now();
        let err = store.write(|_| Ok(())).unwrap_err();
        let waited = start.elapsed();
        let msg = format!("{err:#}");
        assert!(msg.contains(&db.display().to_string()), "{msg}");
        assert!(msg.contains("locked"), "{msg}");
        assert!(waited >= timeout, "gave up early: {waited:?}");
        holder.execute_batch("ROLLBACK;").unwrap();
        assert_eq!(store.write(|_| Ok(())).unwrap().generation, 1);
    }

    #[test]
    fn newer_schema_refused_without_ddl() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh(dir.path());
        Connection::open(&db)
            .unwrap()
            .execute_batch("PRAGMA user_version = 2;")
            .unwrap();
        let before = schema(&db);
        let err = Store::open(&db, &StoreOptions::default()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&db.display().to_string()), "{msg}");
        assert!(msg.contains("version 2"), "{msg}");
        assert!(msg.contains("the 1 this heph knows"), "{msg}");
        assert_eq!(schema(&db), before, "no DDL");
        let c = Connection::open(&db).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
        assert_eq!(read_generation(&c).unwrap(), 0, "no write");
        assert!(
            !crate::migrate_lock::migrate_lock_path(&db).exists(),
            "not even the lock"
        );
    }

    #[test]
    fn index_migration_is_transactional_under_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        // A version-0 db (an empty file is one).
        std::fs::write(&db, b"").unwrap();

        // Under another holder's migration lock: no DDL, an error naming the lock.
        let held = lock_migration(&db, MIGRATE_LOCK_TIMEOUT).unwrap();
        let err = Store::open(&db, &opts(Duration::from_millis(200))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&db.display().to_string()), "{msg}");
        assert!(msg.contains("migration lock"), "{msg}");
        assert!(schema(&db).is_empty());
        drop(held);

        // A step that fails rolls back every earlier one and the version.
        let c = Connection::open(&db).unwrap();
        let err = apply_migrations(&c, 0, &[MIGRATIONS[0], "CREATE TABLE files (x);"]).unwrap_err();
        assert!(format!("{err:#}").contains("schema step 2"), "{err:#}");
        assert!(schema(&db).is_empty(), "step 1 rolled back");
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0);
        drop(c);

        // Released: migrated whole.
        let store = Store::open(&db, &StoreOptions::default()).unwrap();
        assert_eq!(store.generation().unwrap(), 0);
        let v: i64 = Connection::open(&db)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn creation_leaves_one_complete_file_and_deletes_stale_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let cg = dir.path().join("codegen");
        std::fs::create_dir_all(&cg).unwrap();
        let stale = cg.join(format!("{TMP_PREFIX}1-2-3.db"));
        let stale_wal = cg.join(format!("{TMP_PREFIX}1-2-3.db-wal"));
        std::fs::write(&stale, b"half").unwrap();
        std::fs::write(&stale_wal, b"half").unwrap();
        let other = cg.join("index-0123456789abcdef.db");
        std::fs::write(&other, b"another checkout's").unwrap();

        let db = cg.join("index.db");
        assert!(create(&db, &guard()).unwrap());
        let mut names: Vec<_> = std::fs::read_dir(&cg)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["index-0123456789abcdef.db", "index.db"],
            "no -wal, no tmp"
        );
        assert_eq!(std::fs::read(&other).unwrap(), b"another checkout's");
        let c = Connection::open(&db).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn creation_rechecks_and_never_replaces_an_existing_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh(dir.path());
        let store = Store::open(&db, &StoreOptions::default()).unwrap();
        store
            .write(|w| w.put(&Row::new(b"a".to_vec(), "//a:a", RowKind::File)))
            .unwrap();
        assert!(!create(&db, &guard()).unwrap(), "the re-check sees the db");
        assert_eq!(store.load_all().unwrap().1.len(), 1);
        assert_eq!(
            Store::open(&db, &StoreOptions::default())
                .unwrap()
                .generation()
                .unwrap(),
            1
        );
    }

    #[test]
    fn a_missing_db_is_an_error_not_a_new_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let err = Store::open(&db, &opts(Duration::from_millis(200))).unwrap_err();
        assert!(
            format!("{err:#}").contains(&db.display().to_string()),
            "{err:#}"
        );
        assert!(!db.exists());
    }
}
