use crate::engine::local_cache::{LocalCache, NotFoundError, SizedReader, TargetStream};
use crate::hartifactcontent;
use crate::htaddr::Addr;
use anyhow::{Context, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::io::{self, Seek};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use tempfile::SpooledTempFile;

const SPOOL_MEM_THRESHOLD: usize = 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_PIPES: usize = 64;
const WRITE_BATCH_MAX: usize = 64;

type Key = (String, String, String);

struct PipeSemaphore {
    count: Mutex<usize>,
    condvar: Condvar,
}

impl PipeSemaphore {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            count: Mutex::new(limit),
            condvar: Condvar::new(),
        })
    }

    /// Blocks the calling thread until a permit is available.
    ///
    /// **Contract:** callers must be on a thread that may block — either a
    /// dedicated OS thread (e.g. the sqlite writer thread, rayon pool) or a
    /// tokio worker that has handed off its core via
    /// `crate::process_supervisor::block_or_inline` /
    /// `tokio::task::block_in_place`. Calling this directly from a tokio task
    /// parks the worker and can starve the runtime.
    fn acquire(self: &Arc<Self>) -> PipePermit {
        let mut count = self.count.lock().expect("pipe semaphore mutex poisoned");
        while *count == 0 {
            count = self
                .condvar
                .wait(count)
                .expect("pipe semaphore condvar wait failed");
        }
        *count -= 1;
        PipePermit { sem: self.clone() }
    }
}

struct PipePermit {
    sem: Arc<PipeSemaphore>,
}

impl Drop for PipePermit {
    fn drop(&mut self) {
        let mut count = self
            .sem
            .count
            .lock()
            .expect("pipe semaphore mutex poisoned in drop");
        *count += 1;
        self.sem.condvar.notify_one();
    }
}

struct GuardedReader<R: io::Read> {
    inner: R,
    _permit: PipePermit,
}

impl<R: io::Read> io::Read for GuardedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

struct PendingSlot {
    done: Mutex<bool>,
    cond: Condvar,
}

impl PendingSlot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            done: Mutex::new(false),
            cond: Condvar::new(),
        })
    }

    fn wait(&self) {
        let mut done = self.done.lock().expect("pending slot mutex poisoned");
        while !*done {
            done = self
                .cond
                .wait(done)
                .expect("pending slot condvar wait failed");
        }
    }

    fn complete(&self) {
        let mut done = self.done.lock().expect("pending slot mutex poisoned");
        *done = true;
        self.cond.notify_all();
    }
}

#[derive(Default)]
struct PendingTracker {
    // Key → most recently registered slot for that key. Bg-thread processing is FIFO,
    // so the latest slot is also the last to complete.
    map: Mutex<HashMap<Key, Arc<PendingSlot>>>,
}

impl PendingTracker {
    fn register(&self, key: Key) -> Arc<PendingSlot> {
        let slot = PendingSlot::new();
        let mut m = self.map.lock().expect("pending tracker poisoned");
        m.insert(key, slot.clone());
        slot
    }

    fn wait_if_pending(&self, addr: &str, hashin: &str, name: &str) {
        let slot_opt = {
            let m = self.map.lock().expect("pending tracker poisoned");
            // Map holds only in-flight writers/deletes (typically empty on the
            // read hot path), so a borrowed scan beats allocating an owned Key
            // tuple purely to call `get`.
            m.iter()
                .find(|(k, _)| k.0 == addr && k.1 == hashin && k.2 == name)
                .map(|(_, slot)| slot.clone())
        };
        if let Some(slot) = slot_opt {
            slot.wait();
        }
    }

    fn complete(&self, key: &Key, slot: &Arc<PendingSlot>) {
        {
            let mut m = self.map.lock().expect("pending tracker poisoned");
            // Only remove if this slot is still the latest registered. Otherwise a newer
            // write has superseded ours and owns the map entry.
            if let Some(current) = m.get(key)
                && Arc::ptr_eq(current, slot)
            {
                m.remove(key);
            }
        }
        slot.complete();
    }
}

struct WriteJob {
    key: Key,
    buf: SpooledTempFile,
    size: i64,
    slot: Arc<PendingSlot>,
}

struct DeleteJob {
    key: Key,
    slot: Arc<PendingSlot>,
}

enum WriterCmd {
    Write(WriteJob),
    Delete(DeleteJob),
}

pub struct LocalCacheSQLite {
    read_pool: r2d2::Pool<SqliteConnectionManager>,
    writer_tx: Option<mpsc::Sender<WriterCmd>>,
    writer_handle: Option<JoinHandle<()>>,
    pending: Arc<PendingTracker>,
    pipe_sem: Arc<PipeSemaphore>,
    inline_threshold: usize,
}

impl LocalCacheSQLite {
    pub fn with_pipe_limit(
        db_path: PathBuf,
        inline_threshold: usize,
        pipe_limit: usize,
    ) -> Result<Self> {
        let pipe_limit = pipe_limit.max(DEFAULT_MAX_CONCURRENT_PIPES);
        let read_pool_size = u32::try_from(pipe_limit).unwrap_or(u32::MAX);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating sqlite cache dir {parent:?}"))?;
        }

        let mut write_conn = Connection::open(&db_path)
            .with_context(|| format!("opening sqlite cache at {db_path:?}"))?;

        write_conn
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA busy_timeout = 10000;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA temp_store = MEMORY;
                 PRAGMA auto_vacuum = INCREMENTAL;
                 PRAGMA page_size = 8192;
                 PRAGMA cache_size = -64000;
                 PRAGMA mmap_size = 268435456;
                 CREATE TABLE IF NOT EXISTS artifacts (
                     addr   TEXT NOT NULL,
                     hashin TEXT NOT NULL,
                     name   TEXT NOT NULL,
                     data   BLOB NOT NULL,
                     PRIMARY KEY (addr, hashin, name)
                 );
                 CREATE INDEX IF NOT EXISTS idx_artifacts_addr_hashin ON artifacts (addr, hashin);",
            )
            .context("initialising sqlite cache schema")?;

        let manager = SqliteConnectionManager::file(&db_path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_init(|conn| {
                conn.execute_batch(
                    "PRAGMA busy_timeout = 10000;
                     PRAGMA synchronous = NORMAL;
                     PRAGMA temp_store = MEMORY;
                     PRAGMA cache_size = -64000;
                     PRAGMA mmap_size = 268435456;",
                )
            });

        let read_pool = r2d2::Pool::builder()
            .max_size(read_pool_size)
            .min_idle(Some(1))
            .build(manager)
            .context("building sqlite read connection pool")?;

        let pending = Arc::new(PendingTracker::default());
        let (writer_tx, writer_rx) = mpsc::channel::<WriterCmd>();
        let pending_bg = pending.clone();
        let writer_handle = std::thread::Builder::new()
            .name("rheph-sqlite-writer".to_string())
            .spawn(move || writer_loop(&mut write_conn, &writer_rx, &pending_bg))
            .context("spawning sqlite writer thread")?;

        Ok(Self {
            read_pool,
            writer_tx: Some(writer_tx),
            writer_handle: Some(writer_handle),
            pending,
            pipe_sem: PipeSemaphore::new(pipe_limit),
            inline_threshold,
        })
    }

    fn key(addr: &Addr) -> String {
        addr.format()
    }

    fn writer_tx(&self) -> Result<&mpsc::Sender<WriterCmd>> {
        self.writer_tx
            .as_ref()
            .context("sqlite cache writer thread has shut down")
    }
}

impl Drop for LocalCacheSQLite {
    fn drop(&mut self) {
        // Close the channel so the writer thread observes a Disconnected and exits cleanly.
        self.writer_tx = None;
        if let Some(handle) = self.writer_handle.take() {
            // If join fails (panic in bg thread), there's nothing useful to do here.
            drop(handle.join());
        }
    }
}

fn writer_loop(conn: &mut Connection, rx: &mpsc::Receiver<WriterCmd>, pending: &PendingTracker) {
    loop {
        let first = match rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => return,
        };

        let mut batch = Vec::with_capacity(WRITE_BATCH_MAX);
        batch.push(first);
        while batch.len() < WRITE_BATCH_MAX {
            match rx.try_recv() {
                Ok(cmd) => batch.push(cmd),
                Err(_) => break,
            }
        }

        if let Err(e) = process_batch(conn, &mut batch) {
            tracing::error!(error = %format!("{e:#}"), "sqlite cache writer: batch failed");
        }

        // Whether the batch succeeded or not, the pending slots must be released so that
        // readers don't hang. On failure the readers will simply observe NotFound from the
        // DB, which is the correct behavior for a write that didn't land.
        for cmd in &batch {
            match cmd {
                WriterCmd::Write(j) => pending.complete(&j.key, &j.slot),
                WriterCmd::Delete(j) => pending.complete(&j.key, &j.slot),
            }
        }
    }
}

fn process_batch(conn: &mut Connection, batch: &mut [WriterCmd]) -> Result<()> {
    let tx = conn
        .transaction()
        .context("starting sqlite write transaction")?;

    for cmd in batch.iter_mut() {
        match cmd {
            WriterCmd::Write(job) => {
                tx.execute(
                    "INSERT OR REPLACE INTO artifacts (addr, hashin, name, data) \
                     VALUES (?1, ?2, ?3, zeroblob(?4))",
                    rusqlite::params![job.key.0, job.key.1, job.key.2, job.size],
                )
                .with_context(|| {
                    format!(
                        "inserting artifact {addr}/{hashin}/{name}",
                        addr = job.key.0,
                        hashin = job.key.1,
                        name = job.key.2
                    )
                })?;
                let row_id = tx.last_insert_rowid();
                let mut blob = tx
                    .blob_open(
                        rusqlite::DatabaseName::Main,
                        "artifacts",
                        "data",
                        row_id,
                        false,
                    )
                    .with_context(|| format!("opening blob for {}", job.key.2))?;
                job.buf
                    .seek(io::SeekFrom::Start(0))
                    .with_context(|| format!("rewinding spool for {}", job.key.2))?;
                io::copy(&mut job.buf, &mut blob)
                    .with_context(|| format!("writing blob for {}", job.key.2))?;
            }
            WriterCmd::Delete(job) => {
                tx.execute(
                    "DELETE FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3",
                    rusqlite::params![job.key.0, job.key.1, job.key.2],
                )
                .with_context(|| {
                    format!(
                        "deleting artifact {addr}/{hashin}/{name}",
                        addr = job.key.0,
                        hashin = job.key.1,
                        name = job.key.2
                    )
                })?;
            }
        }
    }

    tx.commit().context("committing sqlite write transaction")?;
    Ok(())
}

impl LocalCache for LocalCacheSQLite {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<SizedReader> {
        let addr_key = Self::key(addr);
        self.pending.wait_if_pending(&addr_key, hashin, name);

        let conn = self
            .read_pool
            .get()
            .context("acquiring read connection from pool")?;

        let mut stmt = conn
            .prepare_cached(
                "SELECT rowid, length(data) FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3",
            )
            .context("preparing reader lookup")?;
        let (row_id, blob_len): (i64, usize) = match stmt.query_row(
            rusqlite::params![addr_key, hashin, name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ) {
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(anyhow::anyhow!(NotFoundError));
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("looking up {name} hashin={hashin} in sqlite cache"));
            }
            Ok(v) => v,
        };

        let size = blob_len as u64;

        if blob_len <= self.inline_threshold {
            let mut blob = conn
                .blob_open(
                    rusqlite::DatabaseName::Main,
                    "artifacts",
                    "data",
                    row_id,
                    true,
                )
                .with_context(|| format!("opening blob for {name}"))?;
            let mut buf = Vec::with_capacity(blob_len);
            io::copy(&mut blob, &mut buf)
                .with_context(|| format!("reading small blob for {name}"))?;
            let arc: Arc<[u8]> = Arc::from(buf);
            return Ok(SizedReader {
                size,
                reader: Box::new(io::Cursor::new(arc.clone())),
                bytes: Some(arc),
            });
        }

        // Release the SELECT connection before acquiring semaphore + a fresh pipe connection.
        drop(stmt);
        drop(conn);

        // Semaphore acquired before pool to bound concurrent open pipes (= open FDs).
        let permit = self.pipe_sem.acquire();
        let conn = self
            .read_pool
            .get()
            .context("acquiring read connection from pool")?;

        let (pipe_reader, mut pipe_writer) =
            io::pipe().with_context(|| format!("creating pipe for sqlite blob read of {name}"))?;

        // Move the pooled connection into the rayon pool; returns to pool on drop.
        rayon::spawn(move || {
            let mut blob = match conn.blob_open(
                rusqlite::DatabaseName::Main,
                "artifacts",
                "data",
                row_id,
                true,
            ) {
                Ok(b) => b,
                Err(_) => return,
            };
            drop(io::copy(&mut blob, &mut pipe_writer));
        });

        Ok(SizedReader {
            size,
            reader: Box::new(GuardedReader {
                inner: pipe_reader,
                _permit: permit,
            }),
            bytes: None,
        })
    }

    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
        let key = (Self::key(addr), hashin.to_string(), name.to_string());
        let slot = self.pending.register(key.clone());
        Ok(Box::new(SqliteCacheWriter {
            writer_tx: self.writer_tx()?.clone(),
            pending: self.pending.clone(),
            key: Some(key),
            slot: Some(slot),
            buf: Some(SpooledTempFile::new(SPOOL_MEM_THRESHOLD)),
            size: 0,
        }))
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        let addr_key = Self::key(addr);
        self.pending.wait_if_pending(&addr_key, hashin, name);

        let conn = self
            .read_pool
            .get()
            .context("acquiring read connection from pool")?;

        let mut stmt = conn
            .prepare_cached(
                "SELECT 1 FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3 LIMIT 1",
            )
            .context("preparing exists lookup")?;
        let found = match stmt.query_row(rusqlite::params![addr_key, hashin, name], |_| Ok(())) {
            Ok(()) => true,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(e) => {
                return Err(e).context("checking artifact existence in sqlite cache");
            }
        };

        Ok(found)
    }

    fn list_targets(&self) -> Result<TargetStream> {
        // Stream distinct addrs over a bounded channel: the producer holds one
        // pooled connection and a `SELECT DISTINCT addr` cursor on a dedicated
        // thread, the consumer pulls one addr at a time. Bounded so a slow
        // consumer (GC locking/resolving each target) applies backpressure
        // instead of buffering every target in memory.
        let conn = self
            .read_pool
            .get()
            .context("acquiring read connection from pool")?;
        let (tx, rx) = mpsc::sync_channel::<Result<String>>(256);
        // Dedicated thread (not rayon) so a saturated rayon pool can't starve
        // this long-lived producer and deadlock the consumer on `recv`.
        std::thread::Builder::new()
            .name("rheph-gc-list-targets".to_string())
            .spawn(move || {
                let mut stmt = match conn.prepare("SELECT DISTINCT addr FROM artifacts") {
                    Ok(s) => s,
                    Err(e) => {
                        drop(tx.send(Err(anyhow::Error::from(e).context("prepare list_targets"))));
                        return;
                    }
                };
                match stmt.query_map([], |row| row.get::<_, String>(0)) {
                    Ok(rows) => {
                        for row in rows {
                            let item = row.context("read target addr row");
                            if tx.send(item).is_err() {
                                break; // consumer dropped
                            }
                        }
                    }
                    Err(e) => {
                        drop(tx.send(Err(anyhow::Error::from(e).context("query list_targets"))));
                    }
                }
            })
            .context("spawning gc list-targets thread")?;
        Ok(Box::new(rx.into_iter()))
    }

    fn list_target_entries(&self, addr: &Addr) -> Result<Vec<String>> {
        let key = Self::key(addr);
        let conn = self
            .read_pool
            .get()
            .context("acquiring read connection from pool")?;
        let mut stmt = conn
            .prepare_cached("SELECT DISTINCT hashin FROM artifacts WHERE addr=?1")
            .context("preparing list_target_entries query")?;
        let rows = stmt
            .query_map([&key], |row| row.get::<_, String>(0))
            .with_context(|| format!("listing entries for {addr}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .with_context(|| format!("collecting entries for {addr}"))
    }

    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
        let key = (Self::key(addr), hashin.to_string(), name.to_string());
        let slot = self.pending.register(key.clone());
        self.writer_tx()?
            .send(WriterCmd::Delete(DeleteJob {
                key,
                slot: slot.clone(),
            }))
            .map_err(|e| anyhow::anyhow!("sqlite cache writer thread is gone: {e}"))?;
        slot.wait();
        Ok(())
    }

    fn seekable_reader(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
        let addr_key = Self::key(addr);
        self.pending.wait_if_pending(&addr_key, hashin, name);

        let conn = self
            .read_pool
            .get()
            .context("acquiring read connection from pool")?;

        let mut stmt = conn
            .prepare_cached("SELECT rowid FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3")
            .context("preparing seekable_reader lookup")?;
        let row_id: i64 = match stmt.query_row(rusqlite::params![addr_key, hashin, name], |row| {
            row.get(0)
        }) {
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(anyhow::anyhow!(NotFoundError));
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("looking up {name} hashin={hashin} for seekable read")
                });
            }
            Ok(v) => v,
        };
        drop(stmt);

        Ok(Some(Box::new(OwnedBlob::new(conn, row_id)?)))
    }
}

/// Owns both a pooled sqlite connection and a `Blob` opened against it.
///
/// `rusqlite::blob::Blob<'conn>` borrows its connection; lifetime extension
/// to `'static` is sound because the blob is dropped before `_conn` (Rust
/// drops struct fields in declaration order).
struct OwnedBlob {
    blob: rusqlite::blob::Blob<'static>,
    _conn: r2d2::PooledConnection<SqliteConnectionManager>,
}

// SAFETY: rusqlite::Connection is Send. The blob holds a raw sqlite3
// statement pointer whose ownership transfers with the connection. Both
// fields are Send-compatible; the borrow we extended to 'static is local
// to this struct and never observed externally.
unsafe impl Send for OwnedBlob {}

impl OwnedBlob {
    fn new(conn: r2d2::PooledConnection<SqliteConnectionManager>, row_id: i64) -> Result<Self> {
        let conn_ref: &Connection = &conn;
        let blob = conn_ref
            .blob_open(
                rusqlite::DatabaseName::Main,
                "artifacts",
                "data",
                row_id,
                true,
            )
            .context("opening seekable sqlite blob")?;
        // SAFETY: `blob` borrows from `conn` which is owned alongside it in
        // the returned struct; struct field drop order (blob before _conn)
        // guarantees the borrow outlives no longer than the connection.
        let blob_static: rusqlite::blob::Blob<'static> = unsafe { std::mem::transmute(blob) };
        Ok(Self {
            blob: blob_static,
            _conn: conn,
        })
    }
}

impl io::Read for OwnedBlob {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.blob.read(buf)
    }
}

impl io::Seek for OwnedBlob {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.blob.seek(pos)
    }
}

struct SqliteCacheWriter {
    writer_tx: mpsc::Sender<WriterCmd>,
    pending: Arc<PendingTracker>,
    key: Option<Key>,
    slot: Option<Arc<PendingSlot>>,
    buf: Option<SpooledTempFile>,
    size: usize,
}

impl io::Write for SqliteCacheWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self
            .buf
            .as_mut()
            .expect("writer buffer missing")
            .write(buf)?;
        self.size += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.buf.as_mut().expect("writer buffer missing").flush()
    }
}

impl Drop for SqliteCacheWriter {
    fn drop(&mut self) {
        let (Some(key), Some(slot), Some(buf)) =
            (self.key.take(), self.slot.take(), self.buf.take())
        else {
            return;
        };

        let Ok(size) = i64::try_from(self.size) else {
            // Pathological size; release the slot so readers don't hang.
            self.pending.complete(&key, &slot);
            return;
        };

        let job = WriteJob {
            key,
            buf,
            size,
            slot,
        };

        if let Err(mpsc::SendError(WriterCmd::Write(j))) =
            self.writer_tx.send(WriterCmd::Write(job))
        {
            // Writer thread is gone — unblock waiters so they observe NotFound.
            self.pending.complete(&j.key, &j.slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::tempdir;

    fn make_addr(pkg: &str, name: &str) -> crate::htaddr::Addr {
        crate::htaddr::Addr::new(
            crate::htpkg::PkgBuf::from(pkg),
            name.to_string(),
            Default::default(),
        )
    }

    #[test]
    fn test_local_cache_sqlite() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;

        let addr = make_addr("test_pkg", "test_target");
        let hashin = "abc123hash";
        let name = "output.txt";

        assert!(!cache.exists(&addr, hashin, name)?);

        let mut writer = cache.writer(&addr, hashin, name)?;
        writer.write_all(b"hello sqlite cache")?;
        drop(writer);

        assert!(cache.exists(&addr, hashin, name)?);

        let sized = cache.reader(&addr, hashin, name)?;
        assert_eq!(sized.size, b"hello sqlite cache".len() as u64);
        let mut reader = sized.reader;
        let mut content = String::new();
        reader.read_to_string(&mut content)?;
        assert_eq!(content, "hello sqlite cache");

        cache.delete(&addr, hashin, name)?;
        assert!(!cache.exists(&addr, hashin, name)?);

        Ok(())
    }

    #[test]
    fn test_seekable_reader_pread_in_middle() -> Result<()> {
        use io::{Read, Seek, SeekFrom};
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;

        let addr = make_addr("pkg", "t");
        let payload: Vec<u8> = (0..1024u16).map(|i| (i & 0xff) as u8).collect();
        let mut w = cache.writer(&addr, "h", "blob")?;
        w.write_all(&payload)?;
        drop(w);

        let mut r = cache
            .seekable_reader(&addr, "h", "blob")?
            .expect("sqlite must support seekable_reader");
        r.seek(SeekFrom::Start(100))?;
        let mut buf = vec![0u8; 50];
        r.read_exact(&mut buf)?;
        assert_eq!(buf, payload[100..150]);

        r.seek(SeekFrom::Start(0))?;
        let mut head = vec![0u8; 4];
        r.read_exact(&mut head)?;
        assert_eq!(head, payload[..4]);
        Ok(())
    }

    #[test]
    fn test_seekable_reader_missing_returns_not_found() {
        let dir = tempdir().expect("tempdir");
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )
        .expect("cache");
        let addr = make_addr("pkg", "t");
        let err = match cache.seekable_reader(&addr, "missing", "blob") {
            Ok(_) => panic!("must error"),
            Err(e) => e,
        };
        assert!(err.is::<NotFoundError>(), "{err:#}");
    }

    #[test]
    fn test_local_cache_sqlite_concurrent_readers() -> Result<()> {
        use std::sync::Arc;

        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?);

        let addr = make_addr("test_pkg", "concurrent");
        let hashin = "hashcon";
        let name = "data.bin";

        let mut writer = cache.writer(&addr, hashin, name)?;
        writer.write_all(b"concurrent read data")?;
        drop(writer);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let c = cache.clone();
                let a = addr.clone();
                std::thread::spawn(move || {
                    let mut reader = c.reader(&a, hashin, name).expect("reader").reader;
                    let mut buf = String::new();
                    reader.read_to_string(&mut buf).expect("read");
                    assert_eq!(buf, "concurrent read data")
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        Ok(())
    }

    #[test]
    fn test_list_targets_and_entries() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?;
        let a = make_addr("pkg", "t");
        let b = make_addr("pkg", "u");
        for (addr, h) in [(&a, "h1"), (&a, "h2"), (&b, "h9")] {
            let mut w = cache.writer(addr, h, "out.tar")?;
            w.write_all(b"x")?;
            drop(w);
            assert!(cache.exists(addr, h, "out.tar")?); // barrier
        }

        let mut targets = cache.list_targets()?.collect::<Result<Vec<_>>>()?;
        targets.sort();
        assert_eq!(targets, vec![a.format(), b.format()]);

        let mut a_entries = cache.list_target_entries(&a)?;
        a_entries.sort();
        assert_eq!(a_entries, vec!["h1".to_string(), "h2".to_string()]);
        assert_eq!(cache.list_target_entries(&b)?, vec!["h9".to_string()]);
        Ok(())
    }

    #[test]
    fn test_local_cache_sqlite_read_after_pending_write() -> Result<()> {
        use std::sync::Arc;

        // Reader started before the writer Drop returns from enqueue must still observe
        // the write once it lands. This exercises the PendingTracker wait path.
        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::with_pipe_limit(
            dir.path().join("cache.db"),
            16 * 1024,
            DEFAULT_MAX_CONCURRENT_PIPES,
        )?);
        let addr = make_addr("pkg", "tgt");
        let hashin = "h1";
        let name = "out.bin";

        for i in 0..16 {
            let mut writer = cache.writer(&addr, hashin, name)?;
            writer.write_all(format!("iter-{i}").as_bytes())?;
            drop(writer);

            // Right after drop, the write is enqueued but may not be persisted yet.
            // exists() must wait until the bg thread completes the slot.
            assert!(cache.exists(&addr, hashin, name)?);

            let mut reader = cache.reader(&addr, hashin, name)?.reader;
            let mut got = String::new();
            reader.read_to_string(&mut got)?;
            assert_eq!(got, format!("iter-{i}"));
        }

        Ok(())
    }
}
