//! Shared, on-demand, cross-run cached filesystem walker.
//!
//! Every tree-walking plugin (the `fs` glob driver, the buildfile package
//! discovery) repeats the same `readdir` + `stat` + content-hash work each run
//! over a tree that rarely changes. This walker caches that work *per path*,
//! independent of who asks: two consumers reading the same directory share one
//! cached listing, and one consumer hashing a file shares it with another.
//!
//! It is **consumer-agnostic** and **on-demand**: the walker only answers
//! "what's in this directory?" ([`read_dir`]) and "what's this file's content
//! hash?" ([`file_hash`]). Filtering (globs, excludes, skip dirs) and the
//! decision of whether to recurse belong to the *consumer* — so a requester that
//! stops shallow and one that recurses deep both reuse the dirs they share and
//! independently cache the ones they don't.
//!
//! Validation is mtime/size based (a fast-path proxy for content identity, the
//! same tradeoff accepted elsewhere): a directory's listing is reused while its
//! mtime is unchanged (mtime bumps on any entry add/remove/rename); a file's
//! hash is reused while its `(size, mtime)` is unchanged. Everything is
//! correct-by-fallback — a missing/locked db, a decode error, or any mismatch
//! just re-reads from disk.
//!
//! Backed by a dedicated `fswalk.db` (separate from the artifact cache so it can
//! be pruned independently). Rows carry a last-access stamp; [`prune`] drops
//! stale and orphaned rows so the db cannot grow without bound.
//!
//! [`read_dir`]: CachedWalker::read_dir
//! [`file_hash`]: CachedWalker::file_hash
//! [`prune`]: CachedWalker::prune

use anyhow::{Context, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use parking_lot::Mutex;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::Xxh3;

/// Default time-to-live for fswalk rows: entries untouched for this long are
/// dropped by [`CachedWalker::prune`].
pub const DEFAULT_TTL: std::time::Duration = std::time::Duration::from_secs(14 * 24 * 60 * 60);

/// Escape hatch: set `HEPH_DEBUG_CACHED_WALKER=0` to bypass caching entirely and
/// fall back to reading every directory listing and file hash straight from disk
/// (no in-process front, no durable store). Any other value (or unset) keeps the
/// cache on. Cached in a `OnceLock` because `std::env::var` takes a global libc
/// mutex; the value can't change within a run.
fn cache_bypassed() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        matches!(
            std::env::var("HEPH_DEBUG_CACHED_WALKER").as_deref(),
            Ok("0")
        )
    })
}

/// The kind of a directory entry, from the platform `d_type` (no extra stat).
#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One entry in a directory listing: its file name (not a full path) and kind.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
}

/// A cached directory listing. Entries are sorted by name for determinism.
#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
}

/// A cached file content hash plus the stat fields that validate it.
#[derive(Clone, Debug)]
pub struct FileHash {
    pub size: u64,
    pub mtime_ns: i64,
    pub exec: bool,
    /// xxh3 of the file content with the exec bit folded in (see [`file_hashout`]).
    pub hashout: String,
}

/// Content identity for a sourced file: a hash of its bytes plus the executable
/// marker. Deliberately ignores size and mtime — only the content and the `x`
/// bit determine the artifact, so a file rewritten with identical bytes (new
/// mtime, same content) hashes the same and stays a cache hit. (mtime/size are
/// used only as a *cache validation* fast-path, never as the identity.)
pub fn file_hashout(path: &Path, x: bool) -> Result<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)
        .with_context(|| format!("open file for hashing '{}'", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut h = Xxh3::new();
    // Stream in chunks so large inputs never load wholesale into memory.
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("read file for hashing '{}'", path.display()))?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            h.update(chunk);
        }
    }
    h.update(&[x as u8]);
    Ok(format!("{:x}", h.digest()))
}

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_meta: &std::fs::Metadata) -> bool {
    false
}

fn mtime_ns(meta: &std::fs::Metadata) -> Option<i64> {
    let d = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(d.as_nanos()).ok()
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

fn entry_kind(ft: std::fs::FileType) -> EntryKind {
    if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    }
}

/// Shared cached filesystem walker. Cheap to clone (`Arc` the whole thing).
pub struct CachedWalker {
    /// When set, all caching is skipped: every call reads straight from disk.
    /// Transparent to consumers — the API is identical, only slower.
    bypass: bool,
    store: Option<FsWalkStore>,
    /// In-process front for directory listings, validated against live mtime.
    dirs: Mutex<FxHashMap<PathBuf, (i64, Arc<DirListing>)>>,
    /// In-process front for file hashes, validated against live (size, mtime).
    files: Mutex<FxHashMap<PathBuf, Arc<FileHash>>>,
    /// Paths read-served from the durable store this process; their last-access
    /// stamps are refreshed in one batch on drop (so reads stay write-free).
    touched_dirs: Mutex<Vec<String>>,
    touched_files: Mutex<Vec<String>>,
}

impl CachedWalker {
    /// Open (or create) the walker backed by the sqlite db at `db_path`. On any
    /// db-open failure the walker still works — it just degrades to always
    /// re-reading from disk (no cross-run cache).
    pub fn open(db_path: &Path) -> Self {
        if cache_bypassed() {
            tracing::warn!(
                "cached walker bypassed (HEPH_DEBUG_CACHED_WALKER=0): reading from disk"
            );
            return Self::bypassing();
        }
        let store = match FsWalkStore::open(db_path) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "fswalk cache disabled");
                None
            }
        };
        Self {
            bypass: false,
            store,
            dirs: Mutex::new(FxHashMap::default()),
            files: Mutex::new(FxHashMap::default()),
            touched_dirs: Mutex::new(Vec::new()),
            touched_files: Mutex::new(Vec::new()),
        }
    }

    /// A disabled walker (no durable store, in-process front only) for tests /
    /// contexts without a db.
    pub fn disabled() -> Self {
        Self {
            bypass: false,
            store: None,
            dirs: Mutex::new(FxHashMap::default()),
            files: Mutex::new(FxHashMap::default()),
            touched_dirs: Mutex::new(Vec::new()),
            touched_files: Mutex::new(Vec::new()),
        }
    }

    /// A fully bypassing walker: no in-process front, no durable store — every
    /// call reads straight from disk. Used when `HEPH_DEBUG_CACHED_WALKER=0`.
    pub fn bypassing() -> Self {
        Self {
            bypass: true,
            store: None,
            dirs: Mutex::new(FxHashMap::default()),
            files: Mutex::new(FxHashMap::default()),
            touched_dirs: Mutex::new(Vec::new()),
            touched_files: Mutex::new(Vec::new()),
        }
    }

    /// The cached listing of directory `dir` (absolute). A missing directory
    /// lists empty. The caller filters entries and decides whether to recurse.
    pub fn read_dir(&self, dir: &Path) -> Result<Arc<DirListing>> {
        if self.bypass {
            return Ok(Arc::new(read_dir_uncached(dir)?));
        }
        let meta = match std::fs::metadata(dir) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Arc::new(DirListing::default()));
            }
            Err(e) => return Err(e).with_context(|| format!("stat dir '{}'", dir.display())),
        };
        if !meta.is_dir() {
            return Ok(Arc::new(DirListing::default()));
        }
        let live_mtime = mtime_ns(&meta);

        // In-process front: reuse while the live mtime matches.
        if let Some(mt) = live_mtime
            && let Some((cached_mt, listing)) = self.dirs.lock().get(dir).cloned()
            && cached_mt == mt
        {
            return Ok(listing);
        }

        // Durable store: reuse while the recorded mtime matches.
        if let Some(mt) = live_mtime
            && let Some(store) = self.store.as_ref()
            && let Some((stored_mtime, blob)) = store.get_dir(dir)
            && stored_mtime == mt
            && let Ok(listing) = borsh::from_slice::<DirListing>(&blob)
        {
            let listing = Arc::new(listing);
            self.dirs
                .lock()
                .insert(dir.to_path_buf(), (mt, listing.clone()));
            self.touched_dirs.lock().push(path_key(dir));
            return Ok(listing);
        }

        // Miss: read the directory and cache it.
        let listing = Arc::new(read_dir_uncached(dir)?);
        if let (Some(mt), Some(store)) = (live_mtime, self.store.as_ref()) {
            if let Ok(blob) = borsh::to_vec(&*listing) {
                store.put_dir(dir, mt, &blob, now_ns());
            }
            self.dirs
                .lock()
                .insert(dir.to_path_buf(), (mt, listing.clone()));
        }
        Ok(listing)
    }

    /// The cached content hash (and exec bit) of file `file` (absolute), following
    /// symlinks. Validated by `(size, mtime)`; re-hashed on a mismatch.
    pub fn file_hash(&self, file: &Path) -> Result<Arc<FileHash>> {
        let meta =
            std::fs::metadata(file).with_context(|| format!("stat file '{}'", file.display()))?;
        // A directory (e.g. a symlink resolving to one) is not hashable — error so
        // the caller skips it rather than trying to read it.
        anyhow::ensure!(!meta.is_dir(), "'{}' is a directory", file.display());
        let size = meta.len();
        let live_mtime = mtime_ns(&meta);
        let exec = is_exec(&meta);

        if self.bypass {
            return Ok(Arc::new(FileHash {
                size,
                mtime_ns: live_mtime.unwrap_or(-1),
                exec,
                hashout: file_hashout(file, exec)?,
            }));
        }

        // In-process front.
        if let Some(mt) = live_mtime
            && let Some(found) = self.files.lock().get(file).cloned()
            && found.size == size
            && found.mtime_ns == mt
        {
            return Ok(found);
        }

        // Durable store.
        if let Some(mt) = live_mtime
            && let Some(store) = self.store.as_ref()
            && let Some(fh) = store.get_file(file)
            && fh.size == size
            && fh.mtime_ns == mt
        {
            let fh = Arc::new(fh);
            self.files.lock().insert(file.to_path_buf(), fh.clone());
            self.touched_files.lock().push(path_key(file));
            return Ok(fh);
        }

        // Miss: hash the file and cache it.
        let hashout = file_hashout(file, exec)?;
        let fh = Arc::new(FileHash {
            size,
            mtime_ns: live_mtime.unwrap_or(-1),
            exec,
            hashout,
        });
        if let (Some(_mt), Some(store)) = (live_mtime, self.store.as_ref()) {
            store.put_file(file, &fh, now_ns());
            self.files.lock().insert(file.to_path_buf(), fh.clone());
        }
        Ok(fh)
    }

    /// Drop rows untouched for longer than `ttl`, and (when `check_orphans`)
    /// rows whose path no longer exists on disk. Returns the number removed.
    pub fn prune(&self, ttl: std::time::Duration, check_orphans: bool) -> Result<usize> {
        match self.store.as_ref() {
            Some(store) => store.prune(ttl, check_orphans),
            None => Ok(0),
        }
    }

    /// Flush this process's accumulated last-access stamps. Called on drop;
    /// exposed for tests.
    fn flush_touches(&self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let now = now_ns();
        let dirs = std::mem::take(&mut *self.touched_dirs.lock());
        let files = std::mem::take(&mut *self.touched_files.lock());
        if !dirs.is_empty() {
            store.touch("dirs", &dirs, now);
        }
        if !files.is_empty() {
            store.touch("files", &files, now);
        }
    }
}

impl Default for CachedWalker {
    fn default() -> Self {
        Self::disabled()
    }
}

impl Drop for CachedWalker {
    fn drop(&mut self) {
        self.flush_touches();
    }
}

/// Read a directory directly (no cache), returning a sorted [`DirListing`].
fn read_dir_uncached(dir: &Path) -> Result<DirListing> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DirListing::default()),
        Err(e) => return Err(e).with_context(|| format!("read dir '{}'", dir.display())),
    };
    let mut entries = Vec::new();
    for entry in rd {
        let entry = entry.with_context(|| format!("read dir entry in '{}'", dir.display()))?;
        let Ok(ft) = entry.file_type() else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        entries.push(DirEntry {
            name,
            kind: entry_kind(ft),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(DirListing { entries })
}

/// A path's db key. Lossy is fine — a non-UTF-8 path just gets a stable lossy
/// key; the worst case is a cache miss, never an incorrect hit (the on-disk
/// mtime/size still gate every reuse).
fn path_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ── Durable store ────────────────────────────────────────────────────────────

struct FsWalkStore {
    read_pool: r2d2::Pool<SqliteConnectionManager>,
    write: Mutex<Connection>,
}

impl FsWalkStore {
    fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating fswalk dir {parent:?}"))?;
        }
        let write = Connection::open(db_path)
            .with_context(|| format!("opening fswalk db at {db_path:?}"))?;
        write
            // This db is a pure optimization cache: every row is reconstructable
            // from disk, so corruption is tolerable (a bad db just re-reads and
            // rebuilds). We trade crash durability for speed.
            //   - WAL stays: required so multiple processes read concurrently
            //     while one writes. busy_timeout handles cross-process write
            //     contention. Neither is a durability knob, so both are kept.
            //   - synchronous = OFF (vs NORMAL): drops the remaining checkpoint
            //     fsyncs. An app crash is still safe (the OS flushes the pages);
            //     only an OS crash / power loss can corrupt — which we accept.
            //   synchronous affects only crash durability, never concurrent-access
            //   correctness, so multi-process behavior is unchanged.
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA busy_timeout = 10000;
                 PRAGMA synchronous = OFF;
                 PRAGMA temp_store = MEMORY;
                 PRAGMA mmap_size = 268435456;
                 CREATE TABLE IF NOT EXISTS dirs (
                     path        TEXT PRIMARY KEY,
                     mtime_ns    INTEGER NOT NULL,
                     entries     BLOB NOT NULL,
                     accessed_ns INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS files (
                     path        TEXT PRIMARY KEY,
                     size        INTEGER NOT NULL,
                     mtime_ns    INTEGER NOT NULL,
                     exec        INTEGER NOT NULL,
                     hashout     TEXT NOT NULL,
                     accessed_ns INTEGER NOT NULL
                 );",
            )
            .context("initialising fswalk schema")?;

        let manager = SqliteConnectionManager::file(db_path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_init(|c| {
                c.execute_batch(
                    "PRAGMA busy_timeout = 10000;
                     PRAGMA temp_store = MEMORY;
                     PRAGMA mmap_size = 268435456;",
                )
            });
        let read_pool = r2d2::Pool::builder()
            .max_size(16)
            .min_idle(Some(1))
            .build(manager)
            .context("building fswalk read pool")?;

        Ok(Self {
            read_pool,
            write: Mutex::new(write),
        })
    }

    fn get_dir(&self, path: &Path) -> Option<(i64, Vec<u8>)> {
        let conn = self.read_pool.get().ok()?;
        let mut stmt = conn
            .prepare_cached("SELECT mtime_ns, entries FROM dirs WHERE path = ?1")
            .ok()?;
        stmt.query_row([path_key(path)], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok()
    }

    fn put_dir(&self, path: &Path, mtime_ns: i64, entries: &[u8], now: i64) {
        let conn = self.write.lock();
        drop(conn.execute(
            "INSERT OR REPLACE INTO dirs (path, mtime_ns, entries, accessed_ns) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![path_key(path), mtime_ns, entries, now],
        ));
    }

    fn get_file(&self, path: &Path) -> Option<FileHash> {
        let conn = self.read_pool.get().ok()?;
        let mut stmt = conn
            .prepare_cached("SELECT size, mtime_ns, exec, hashout FROM files WHERE path = ?1")
            .ok()?;
        stmt.query_row([path_key(path)], |r| {
            Ok(FileHash {
                size: u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                mtime_ns: r.get(1)?,
                exec: r.get::<_, i64>(2)? != 0,
                hashout: r.get(3)?,
            })
        })
        .ok()
    }

    fn put_file(&self, path: &Path, fh: &FileHash, now: i64) {
        let conn = self.write.lock();
        drop(conn.execute(
            "INSERT OR REPLACE INTO files (path, size, mtime_ns, exec, hashout, accessed_ns) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                path_key(path),
                i64::try_from(fh.size).unwrap_or(i64::MAX),
                fh.mtime_ns,
                fh.exec as i64,
                fh.hashout,
                now
            ],
        ));
    }

    /// Refresh last-access for a batch of paths in `table` (chunked under the
    /// sqlite variable limit). Best-effort.
    fn touch(&self, table: &str, paths: &[String], now: i64) {
        let conn = self.write.lock();
        for chunk in paths.chunks(400) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("UPDATE {table} SET accessed_ns = ? WHERE path IN ({placeholders})");
            let Ok(mut stmt) = conn.prepare(&sql) else {
                continue;
            };
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
            params.push(&now);
            for p in chunk {
                params.push(p);
            }
            drop(stmt.execute(params.as_slice()));
        }
    }

    fn prune(&self, ttl: std::time::Duration, check_orphans: bool) -> Result<usize> {
        let cutoff = now_ns().saturating_sub(i64::try_from(ttl.as_nanos()).unwrap_or(i64::MAX));
        let conn = self.write.lock();
        let mut removed = 0usize;
        removed += conn
            .execute("DELETE FROM dirs WHERE accessed_ns < ?1", [cutoff])
            .context("pruning stale fswalk dirs")?;
        removed += conn
            .execute("DELETE FROM files WHERE accessed_ns < ?1", [cutoff])
            .context("pruning stale fswalk files")?;

        if check_orphans {
            for table in ["dirs", "files"] {
                let paths: Vec<String> = {
                    let mut stmt = conn.prepare(&format!("SELECT path FROM {table}"))?;
                    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                    rows.filter_map(|r| r.ok())
                        .filter(|p| !Path::new(p).exists())
                        .collect()
                };
                for chunk in paths.chunks(400) {
                    let placeholders = std::iter::repeat_n("?", chunk.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!("DELETE FROM {table} WHERE path IN ({placeholders})");
                    let params: Vec<&dyn rusqlite::ToSql> =
                        chunk.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
                    removed += conn.execute(&sql, params.as_slice()).unwrap_or(0);
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn walker(dir: &Path) -> CachedWalker {
        CachedWalker::open(&dir.join("fswalk.db"))
    }

    #[test]
    fn read_dir_caches_and_revalidates() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg").join("a.txt"), b"a").unwrap();

        let w = walker(root);
        let l1 = w.read_dir(&root.join("pkg")).unwrap();
        assert_eq!(l1.entries.len(), 1);
        assert_eq!(l1.entries[0].name, "a.txt");
        assert_eq!(l1.entries[0].kind, EntryKind::File);

        // A fresh walker sharing the db reloads the listing without re-reading
        // (we can't observe the readdir directly, but the content must match).
        drop(w);
        let w2 = walker(root);
        let l2 = w2.read_dir(&root.join("pkg")).unwrap();
        assert_eq!(l2.entries.len(), 1);

        // Add a file → dir mtime bumps → listing refreshes.
        std::fs::write(root.join("pkg").join("b.txt"), b"b").unwrap();
        std::fs::File::open(root.join("pkg"))
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(7200))
            .unwrap();
        let l3 = w2.read_dir(&root.join("pkg")).unwrap();
        assert_eq!(l3.entries.len(), 2);
    }

    #[test]
    fn missing_dir_lists_empty() {
        let tmp = tempdir().unwrap();
        let w = walker(tmp.path());
        let l = w.read_dir(&tmp.path().join("nope")).unwrap();
        assert!(l.entries.is_empty());
    }

    #[test]
    fn file_hash_caches_and_revalidates() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        let f = root.join("f");
        std::fs::write(&f, b"hello").unwrap();

        let w = walker(root);
        let h1 = w.file_hash(&f).unwrap();
        // Matches the direct hash.
        assert_eq!(h1.hashout, file_hashout(&f, h1.exec).unwrap());

        // Fresh walker reloads the same hash from the db.
        drop(w);
        let w2 = walker(root);
        let h2 = w2.file_hash(&f).unwrap();
        assert_eq!(h1.hashout, h2.hashout);

        // Content change (different size) → re-hash, different value.
        std::fs::write(&f, b"a different, longer body").unwrap();
        let h3 = w2.file_hash(&f).unwrap();
        assert_ne!(h1.hashout, h3.hashout);
    }

    #[test]
    fn prune_drops_stale_rows() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("d")).unwrap();
        let w = walker(root);
        w.read_dir(&root.join("d")).unwrap();
        w.flush_touches();

        // TTL of zero ⇒ everything is stale ⇒ pruned.
        let removed = w.prune(std::time::Duration::from_secs(0), false).unwrap();
        assert!(removed >= 1, "stale dir row pruned");
    }

    #[test]
    fn prune_drops_orphaned_rows() {
        let tmp = tempdir().unwrap();
        let dbdir = tempdir().unwrap();
        let gone = tmp.path().join("gone");
        std::fs::create_dir(&gone).unwrap();

        let w = walker(dbdir.path());
        w.read_dir(&gone).unwrap();

        // Delete the directory, then prune with a long TTL: the row survives TTL
        // (just written) but is removed as an orphan (its path no longer exists).
        std::fs::remove_dir_all(&gone).unwrap();
        let removed = w.prune(DEFAULT_TTL, true).unwrap();
        assert!(removed >= 1, "orphaned dir row pruned");
        // And a second prune finds nothing.
        assert_eq!(w.prune(DEFAULT_TTL, true).unwrap(), 0);
    }

    #[test]
    fn disabled_walker_still_reads() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        std::fs::write(tmp.path().join("d").join("x"), b"").unwrap();
        let w = CachedWalker::disabled();
        let l = w.read_dir(&tmp.path().join("d")).unwrap();
        assert_eq!(l.entries.len(), 1);
    }

    #[test]
    fn bypassing_walker_reads_disk_without_caching() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("d")).unwrap();
        let f = root.join("d").join("x");
        std::fs::write(&f, b"hello").unwrap();

        let w = CachedWalker::bypassing();
        // Reads and hashes correctly, matching the uncached primitives.
        let l = w.read_dir(&root.join("d")).unwrap();
        assert_eq!(l.entries.len(), 1);
        let h = w.file_hash(&f).unwrap();
        assert_eq!(h.hashout, file_hashout(&f, h.exec).unwrap());

        // Bypass keeps no in-process front: a content change is seen immediately,
        // with no stale cached value (and the dir's mtime is never consulted).
        std::fs::write(&f, b"a different, longer body").unwrap();
        let h2 = w.file_hash(&f).unwrap();
        assert_ne!(h.hashout, h2.hashout);

        // No durable store ⇒ nothing to prune.
        assert_eq!(
            w.prune(std::time::Duration::from_secs(0), false).unwrap(),
            0
        );
    }
}
