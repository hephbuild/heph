use crate::engine::local_cache::{LocalCache, NotFoundError};
use crate::htaddr::Addr;
use anyhow::{Context, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};
use std::io::{self, Seek};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use tempfile::SpooledTempFile;

const SPOOL_MEM_THRESHOLD: usize = 1024 * 1024;
const INLINE_BLOB_THRESHOLD: usize = 10 * 1024;
const MAX_CONCURRENT_PIPES: usize = 64;
const READ_POOL_SIZE: u32 = MAX_CONCURRENT_PIPES as u32;

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

pub struct LocalCacheSQLite {
    read_pool: r2d2::Pool<SqliteConnectionManager>,
    write_conn: Arc<Mutex<Connection>>,
    pipe_sem: Arc<PipeSemaphore>,
}

impl LocalCacheSQLite {
    pub fn new(db_path: PathBuf) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating sqlite cache dir {parent:?}"))?;
        }

        let write_conn = Connection::open(&db_path)
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
            .max_size(READ_POOL_SIZE)
            .build(manager)
            .context("building sqlite read connection pool")?;

        Ok(Self {
            read_pool,
            write_conn: Arc::new(Mutex::new(write_conn)),
            pipe_sem: PipeSemaphore::new(MAX_CONCURRENT_PIPES),
        })
    }

    fn key(addr: &Addr) -> String {
        addr.format()
    }
}

impl LocalCache for LocalCacheSQLite {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Read>> {
        let key = Self::key(addr);

        let conn = self
            .read_pool
            .get()
            .context("acquiring read connection from pool")?;

        let (row_id, blob_len): (i64, usize) = match conn.query_row(
            "SELECT rowid, length(data) FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3",
            rusqlite::params![key, hashin, name],
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

        if blob_len <= INLINE_BLOB_THRESHOLD {
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
            return Ok(Box::new(io::Cursor::new(buf)));
        }

        // Release the SELECT connection before acquiring semaphore + a fresh pipe connection.
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

        Ok(Box::new(GuardedReader {
            inner: pipe_reader,
            _permit: permit,
        }))
    }

    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
        Ok(Box::new(SqliteCacheWriter {
            write_conn: self.write_conn.clone(),
            addr: Self::key(addr),
            hashin: hashin.to_string(),
            name: name.to_string(),
            buf: SpooledTempFile::new(SPOOL_MEM_THRESHOLD),
            size: 0,
        }))
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        let key = Self::key(addr);
        let conn = self
            .read_pool
            .get()
            .context("acquiring read connection from pool")?;

        let found = match conn.query_row(
            "SELECT 1 FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3 LIMIT 1",
            rusqlite::params![key, hashin, name],
            |_| Ok(()),
        ) {
            Ok(()) => true,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(e) => {
                return Err(e).context("checking artifact existence in sqlite cache");
            }
        };

        Ok(found)
    }

    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
        let key = Self::key(addr);
        let conn = self
            .write_conn
            .lock()
            .map_err(|e| anyhow::anyhow!("sqlite write mutex poisoned: {e}"))?;

        conn.execute(
            "DELETE FROM artifacts WHERE addr=?1 AND hashin=?2 AND name=?3",
            rusqlite::params![key, hashin, name],
        )
        .context("deleting artifact from sqlite cache")?;

        Ok(())
    }
}

struct SqliteCacheWriter {
    write_conn: Arc<Mutex<Connection>>,
    addr: String,
    hashin: String,
    name: String,
    buf: SpooledTempFile,
    size: usize,
}

impl io::Write for SqliteCacheWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.buf.write(buf)?;
        self.size += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.buf.flush()
    }
}

impl Drop for SqliteCacheWriter {
    fn drop(&mut self) {
        if self.buf.seek(io::SeekFrom::Start(0)).is_err() {
            return;
        }
        let Ok(size) = i64::try_from(self.size) else {
            return;
        };
        let conn = match self.write_conn.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        if conn
            .execute(
                "INSERT OR REPLACE INTO artifacts (addr, hashin, name, data) \
                 VALUES (?1, ?2, ?3, zeroblob(?4))",
                rusqlite::params![self.addr, self.hashin, self.name, size],
            )
            .is_err()
        {
            return;
        }
        let row_id = conn.last_insert_rowid();
        let mut blob = match conn.blob_open(
            rusqlite::DatabaseName::Main,
            "artifacts",
            "data",
            row_id,
            false,
        ) {
            Ok(b) => b,
            Err(_) => return,
        };
        drop(io::copy(&mut self.buf, &mut blob));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use tempfile::tempdir;

    fn make_addr(pkg: &str, name: &str) -> crate::htaddr::Addr {
        crate::htaddr::Addr {
            package: crate::htpkg::PkgBuf::from(pkg),
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_local_cache_sqlite() -> Result<()> {
        let dir = tempdir()?;
        let cache = LocalCacheSQLite::new(dir.path().join("cache.db"))?;

        let addr = make_addr("test_pkg", "test_target");
        let hashin = "abc123hash";
        let name = "output.txt";

        assert!(!cache.exists(&addr, hashin, name)?);

        let mut writer = cache.writer(&addr, hashin, name)?;
        writer.write_all(b"hello sqlite cache")?;
        drop(writer);

        assert!(cache.exists(&addr, hashin, name)?);

        let mut reader = cache.reader(&addr, hashin, name)?;
        let mut content = String::new();
        reader.read_to_string(&mut content)?;
        assert_eq!(content, "hello sqlite cache");

        cache.delete(&addr, hashin, name)?;
        assert!(!cache.exists(&addr, hashin, name)?);

        Ok(())
    }

    #[test]
    fn test_local_cache_sqlite_concurrent_readers() -> Result<()> {
        use std::sync::Arc;

        let dir = tempdir()?;
        let cache = Arc::new(LocalCacheSQLite::new(dir.path().join("cache.db"))?);

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
                    let mut reader = c.reader(&a, hashin, name).expect("reader");
                    let mut buf = String::new();
                    reader.read_to_string(&mut buf).expect("read");
                    assert_eq!(buf, "concurrent read data");
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        Ok(())
    }
}
