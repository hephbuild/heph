//! [`LocalCacheTmp`] — a mem-only [`LocalCache`] for `tmp`/uncacheable revisions.
//!
//! Uncacheable targets (and `--shell` runs) still copy their outputs into a
//! cache so the artifacts survive sandbox teardown, but they use a unique
//! `{hashin}_{nanos}` key that is never read back across runs. Routing those
//! writes through the durable SQLite cache means a WAL `INSERT` + commit per
//! throwaway entry — measured at ~10% of CPU on a fully-cached run.
//!
//! This store keeps each such entry in a plain in-memory map instead, bounded by
//! two limits from `tmp_cache` config:
//! - `per_entry_bytes`: an entry larger than this spills to the durable cache.
//! - `capacity_bytes`: once the in-memory total would exceed this, further
//!   entries spill to the durable cache (admission control, not eviction).
//!
//! Spilled entries are served via durable fallthrough, so a reader never misses.
//! Unlike the LRU [`LocalCacheMem`](super::local_cache_mem::LocalCacheMem) tier,
//! admitted entries are never evicted — a tmp entry must stay readable until the
//! parent target that consumes it runs, which can be arbitrarily later in the
//! same request. The map lives for the process lifetime; tmp keys are unique per
//! run, so it grows by at most one entry per uncacheable target (within the
//! capacity budget) and is reclaimed when the process exits.

use crate::engine::local_cache::{Existence, LocalCache, SizedReader, TargetStream};
use anyhow::Result;
use hcore::hartifactcontent;
use hmodel::htaddr::Addr;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

type Key = (String, String, String);

/// Shared in-memory state: the entry map plus the running byte total, shared
/// between the cache and every outstanding writer (so a writer can admit/publish
/// on drop).
struct Store {
    map: RwLock<FxHashMap<Key, Arc<[u8]>>>,
    /// Bytes currently held in `map`. Reserved before insert, released on delete.
    used: AtomicU64,
    /// Per-entry size cap; larger entries spill to durable.
    per_entry_bytes: usize,
    /// Total in-memory budget; entries that would exceed it spill to durable.
    capacity_bytes: u64,
}

impl Store {
    /// Reserve `len` bytes against the capacity budget. Returns `true` (and
    /// reserves) if there was room, `false` if the caller must spill to durable.
    fn try_reserve(&self, len: u64) -> bool {
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            if cur + len > self.capacity_bytes {
                return false;
            }
            match self.used.compare_exchange_weak(
                cur,
                cur + len,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }
}

pub struct LocalCacheTmp {
    /// Spill target for over-cap / over-budget entries and the read fallthrough.
    durable: Arc<dyn LocalCache>,
    store: Arc<Store>,
}

impl LocalCacheTmp {
    /// `per_entry_bytes` / `capacity_bytes` come from `tmp_cache` config
    /// (defaults 1 MiB / 64 MiB). Entries over either limit spill to `durable`.
    pub fn new(durable: Arc<dyn LocalCache>, per_entry_bytes: usize, capacity_bytes: u64) -> Self {
        Self {
            durable,
            store: Arc::new(Store {
                map: RwLock::new(FxHashMap::default()),
                used: AtomicU64::new(0),
                per_entry_bytes,
                capacity_bytes,
            }),
        }
    }

    fn key(addr: &Addr, hashin: &str, name: &str) -> Key {
        (addr.format(), hashin.to_string(), name.to_string())
    }
}

impl LocalCache for LocalCacheTmp {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<SizedReader> {
        let key = Self::key(addr, hashin, name);
        if let Some(buf) = self.store.map.read().get(&key).cloned() {
            return Ok(SizedReader {
                size: buf.len() as u64,
                reader: Box::new(io::Cursor::new(buf.clone())),
                bytes: Some(buf),
            });
        }
        // Spilled entry — served from the durable cache.
        self.durable.reader(addr, hashin, name)
    }

    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
        let key = Self::key(addr, hashin, name);
        // Drop any stale value before the write lands (tmp keys are unique, so
        // this is belt-and-suspenders for repeated writes of the same key).
        if let Some(v) = self.store.map.write().remove(&key) {
            self.store.used.fetch_sub(v.len() as u64, Ordering::Relaxed);
        }
        Ok(Box::new(TmpWriter {
            key,
            addr: addr.clone(),
            hashin: hashin.to_string(),
            name: name.to_string(),
            buf: Vec::new(),
            spilled: None,
            durable: Arc::clone(&self.durable),
            store: Arc::clone(&self.store),
        }))
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        let key = Self::key(addr, hashin, name);
        if self.store.map.read().contains_key(&key) {
            return Ok(true);
        }
        self.durable.exists(addr, hashin, name)
    }

    fn existence(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Existence> {
        let key = Self::key(addr, hashin, name);
        // An admitted tmp entry is committed the moment `TmpWriter` inserts it —
        // no queue behind it, so answer straight away.
        if self.store.map.read().contains_key(&key) {
            return Ok(Existence::Committed(true));
        }
        // Spilled entries live in the durable cache and inherit its write queue.
        self.durable.existence(addr, hashin, name)
    }

    fn exists_committed(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        let key = Self::key(addr, hashin, name);
        // Mirrors `exists`. Unlike the mem tier this map has no eviction, so an
        // admitted entry stays readable from here for the process's life.
        if self.store.map.read().contains_key(&key) {
            return Ok(true);
        }
        self.durable.exists_committed(addr, hashin, name)
    }

    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
        let key = Self::key(addr, hashin, name);
        if let Some(v) = self.store.map.write().remove(&key) {
            self.store.used.fetch_sub(v.len() as u64, Ordering::Relaxed);
        }
        self.durable.delete(addr, hashin, name)
    }

    fn seekable_reader(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
        let key = Self::key(addr, hashin, name);
        if let Some(buf) = self.store.map.read().get(&key).cloned() {
            return Ok(Some(Box::new(io::Cursor::new(buf))));
        }
        self.durable.seekable_reader(addr, hashin, name)
    }

    fn list_targets(&self) -> Result<TargetStream> {
        // tmp entries are ephemeral and must not be enumerated for GC; only the
        // durable cache holds collectable revisions.
        self.durable.list_targets()
    }

    fn list_target_entries(&self, addr: &Addr) -> Result<Vec<String>> {
        self.durable.list_target_entries(addr)
    }

    /// An admitted entry lives only in `map` — there is no file anywhere, so
    /// `None` is the answer, not a gap to fill with an invented temp file.
    /// Anything else spilled, and the durable cache decides. Residency is checked
    /// for the same reason every read path here checks it, but to a different
    /// end: the tier-local answer is negative, so a hit means "no file, and don't
    /// ask" rather than "here it is" — saving a `stat` on a path that by
    /// construction does not exist.
    ///
    /// **Lifetime exemption.** [`Content::file_path`]'s contract says the path
    /// stays valid while the artifact is alive because a cache read guard rides
    /// on it. That does not hold here: `build_eresult` attaches no
    /// `GuardedArtifact` on the uncacheable path (`guard: None`), so a spilled
    /// tmp entry hands out a durable path with *no lock held on its addr*. Its
    /// revision is enumerable, so a concurrent `heph gc` can reclaim the blob
    /// while the path is in flight. The consumer then gets `ENOENT` — a hard
    /// error, not wrong bytes, and the same exposure `reader` already had. Do not
    /// build a consumer that holds a tmp path across an await on the assumption
    /// the guarded contract covers it.
    ///
    /// [`Content::file_path`]: hcore::hartifactcontent::Content::file_path
    fn file_path(&self, addr: &Addr, hashin: &str, name: &str) -> Option<std::path::PathBuf> {
        let key = Self::key(addr, hashin, name);
        if self.store.map.read().contains_key(&key) {
            return None;
        }
        self.durable.file_path(addr, hashin, name)
    }
}

/// Writer that buffers in memory and publishes on drop. It spills to the durable
/// cache when the stream exceeds the per-entry cap, or (on drop) when admitting
/// the buffered entry would exceed the capacity budget.
struct TmpWriter {
    key: Key,
    addr: Addr,
    hashin: String,
    name: String,
    buf: Vec<u8>,
    spilled: Option<Box<dyn io::Write>>,
    durable: Arc<dyn LocalCache>,
    store: Arc<Store>,
}

impl TmpWriter {
    /// Open the durable writer and flush the buffered bytes into it.
    fn begin_spill(&mut self) -> io::Result<&mut Box<dyn io::Write>> {
        let mut w = self
            .durable
            .writer(&self.addr, &self.hashin, &self.name)
            .map_err(io::Error::other)?;
        w.write_all(&self.buf)?;
        self.buf = Vec::new();
        Ok(self.spilled.insert(w))
    }
}

impl io::Write for TmpWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Some(w) = self.spilled.as_mut() {
            return w.write(data);
        }
        if self.buf.len() + data.len() > self.store.per_entry_bytes {
            // Crossed the per-entry cap mid-stream: spill what we have and
            // forward this chunk to the durable writer.
            let w = self.begin_spill()?;
            return w.write(data);
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.spilled.as_mut() {
            Some(w) => w.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for TmpWriter {
    fn drop(&mut self) {
        // Spilled entries are finalized by the durable writer's own Drop.
        if self.spilled.is_some() {
            return;
        }
        let len = self.buf.len() as u64;
        if self.store.try_reserve(len) {
            let arc: Arc<[u8]> = Arc::from(std::mem::take(&mut self.buf));
            self.store.map.write().insert(self.key.clone(), arc);
        } else if let Ok(mut w) = self.durable.writer(&self.addr, &self.hashin, &self.name) {
            // Over the capacity budget: spill to durable instead of mem. Nothing
            // actionable on a write error inside Drop; surface it and move on.
            if let Err(e) = w.write_all(&self.buf) {
                tracing::error!(error = %e, "tmp cache spill-on-drop write failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::local_cache::NotFoundError;
    use std::io::{Read, Write};

    /// Durable backend stand-in that records every write so tests can assert a
    /// tmp write did (or did not) reach durable storage.
    #[derive(Default)]
    struct RecordingCache {
        store: Arc<RwLock<FxHashMap<Key, Arc<[u8]>>>>,
        /// Counts `file_path` delegations, so a test can assert the tmp tier
        /// answered from its own map instead of asking.
        file_path_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct RecordingWriter {
        key: Key,
        buf: Vec<u8>,
        store: Arc<RwLock<FxHashMap<Key, Arc<[u8]>>>>,
    }
    impl io::Write for RecordingWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Drop for RecordingWriter {
        fn drop(&mut self) {
            self.store
                .write()
                .insert(self.key.clone(), Arc::from(std::mem::take(&mut self.buf)));
        }
    }

    impl LocalCache for RecordingCache {
        fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<SizedReader> {
            let key = (addr.format(), hashin.to_string(), name.to_string());
            match self.store.read().get(&key).cloned() {
                Some(buf) => Ok(SizedReader {
                    size: buf.len() as u64,
                    reader: Box::new(io::Cursor::new(buf.clone())),
                    bytes: Some(buf),
                }),
                None => Err(anyhow::anyhow!(NotFoundError)),
            }
        }
        fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
            Ok(Box::new(RecordingWriter {
                key: (addr.format(), hashin.to_string(), name.to_string()),
                buf: Vec::new(),
                store: Arc::clone(&self.store),
            }))
        }
        fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
            let key = (addr.format(), hashin.to_string(), name.to_string());
            Ok(self.store.read().contains_key(&key))
        }
        // In-memory map, committed the instant the writer drops.
        fn existence(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Existence> {
            Ok(Existence::Committed(self.exists(addr, hashin, name)?))
        }
        fn exists_committed(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
            // Commits inline: nothing to distinguish from `exists`.
            self.exists(addr, hashin, name)
        }
        fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
            let key = (addr.format(), hashin.to_string(), name.to_string());
            self.store.write().remove(&key);
            Ok(())
        }
        /// Answers a path for *any* key, whether or not it holds it. Deliberately
        /// unlike a real backend: it makes the tmp tier's residency
        /// short-circuit observable, since delegating would visibly return
        /// `Some` for a mem-resident entry.
        fn file_path(&self, _a: &Addr, _h: &str, name: &str) -> Option<std::path::PathBuf> {
            self.file_path_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(std::path::PathBuf::from("/durable").join(name))
        }
    }

    fn addr() -> Addr {
        hmodel::htaddr::parse_addr("//pkg:t").expect("addr")
    }

    fn write_all(cache: &dyn LocalCache, a: &Addr, h: &str, n: &str, bytes: &[u8]) {
        let mut w = cache.writer(a, h, n).expect("writer");
        w.write_all(bytes).expect("write");
        drop(w);
    }

    fn read_all(cache: &dyn LocalCache, a: &Addr, h: &str, n: &str) -> Vec<u8> {
        let sized = cache.reader(a, h, n).expect("reader");
        let mut out = Vec::new();
        sized
            .reader
            .take(sized.size)
            .read_to_end(&mut out)
            .expect("read");
        out
    }

    fn durable_has(durable: &RecordingCache, a: &Addr, h: &str, n: &str) -> bool {
        durable
            .store
            .read()
            .contains_key(&(a.format(), h.to_string(), n.to_string()))
    }

    #[test]
    fn small_entry_stays_in_mem_and_skips_durable() {
        let durable = Arc::new(RecordingCache::default());
        // per-entry 1 KiB, capacity 1 MiB
        let tmp = LocalCacheTmp::new(durable.clone(), 1024, 1024 * 1024);
        let a = addr();

        write_all(&tmp, &a, "h_1", "out", b"hello");

        assert_eq!(read_all(&tmp, &a, "h_1", "out"), b"hello");
        assert!(tmp.exists(&a, "h_1", "out").unwrap());
        assert!(durable.store.read().is_empty());
    }

    /// An admitted entry lives only in the map — there is no file anywhere, so
    /// `None` is its true answer, not a gap to be filled with an invented temp
    /// file. A spilled one is an ordinary durable blob and must expose the
    /// durable backend's path, or an uncacheable target's artifacts never take
    /// the direct-open path however large they are.
    ///
    /// The durable double answers `Some` for *every* key, including ones it does
    /// not hold. That is what makes the mem-resident half discriminating: against
    /// a real backend (which has no file for a mem-resident key either) the
    /// assertion would hold whether or not the residency short-circuit existed.
    #[test]
    fn file_path_answers_only_for_spilled_entries() {
        let durable = Arc::new(RecordingCache::default());
        let calls = Arc::clone(&durable.file_path_calls);
        // per-entry cap of 8 bytes: the short entry stays in mem, the 64-byte
        // one spills.
        let tmp = LocalCacheTmp::new(durable, 8, 1024 * 1024);
        let a = addr();

        write_all(&tmp, &a, "h_m", "out", b"hi");
        assert!(
            tmp.file_path(&a, "h_m", "out").is_none(),
            "mem-resident tmp entry has no file on disk, whatever durable would say"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a resident entry must be answered locally, not delegated"
        );

        let big = vec![7u8; 64];
        write_all(&tmp, &a, "h_s", "out", &big);
        assert_eq!(
            tmp.file_path(&a, "h_s", "out"),
            Some(std::path::PathBuf::from("/durable/out")),
            "a spilled entry must expose the durable backend's path"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn oversized_entry_spills_to_durable() {
        let durable = Arc::new(RecordingCache::default());
        let tmp = LocalCacheTmp::new(durable.clone(), 8, 1024 * 1024);
        let a = addr();

        let big = vec![7u8; 64];
        write_all(&tmp, &a, "h_2", "out", &big);

        let key = (a.format(), "h_2".to_string(), "out".to_string());
        assert!(!tmp.store.map.read().contains_key(&key));
        assert!(durable_has(&durable, &a, "h_2", "out"));
        assert_eq!(read_all(&tmp, &a, "h_2", "out"), big);
    }

    #[test]
    fn over_capacity_entry_spills_to_durable() {
        let durable = Arc::new(RecordingCache::default());
        // per-entry generous, but total budget only 16 bytes.
        let tmp = LocalCacheTmp::new(durable.clone(), 1024, 16);
        let a = addr();

        // First 10-byte entry fits the 16-byte budget → mem.
        write_all(&tmp, &a, "h_a", "out", &[1u8; 10]);
        assert!(
            tmp.store
                .map
                .read()
                .contains_key(&(a.format(), "h_a".into(), "out".into()))
        );
        assert!(!durable_has(&durable, &a, "h_a", "out"));

        // Second 10-byte entry would exceed 16 → spills to durable.
        write_all(&tmp, &a, "h_b", "out", &[2u8; 10]);
        assert!(
            !tmp.store
                .map
                .read()
                .contains_key(&(a.format(), "h_b".into(), "out".into()))
        );
        assert!(durable_has(&durable, &a, "h_b", "out"));
        // Both still readable.
        assert_eq!(read_all(&tmp, &a, "h_a", "out"), [1u8; 10]);
        assert_eq!(read_all(&tmp, &a, "h_b", "out"), [2u8; 10]);
    }

    #[test]
    fn delete_frees_budget() {
        let durable = Arc::new(RecordingCache::default());
        let tmp = LocalCacheTmp::new(durable.clone(), 1024, 16);
        let a = addr();

        write_all(&tmp, &a, "h_a", "out", &[1u8; 10]);
        assert!(tmp.exists(&a, "h_a", "out").unwrap());

        // Deleting frees the 10 bytes back to the budget.
        tmp.delete(&a, "h_a", "out").unwrap();
        assert!(!tmp.exists(&a, "h_a", "out").unwrap());

        // A new 10-byte entry now fits in mem again (budget was reclaimed).
        write_all(&tmp, &a, "h_c", "out", &[3u8; 10]);
        assert!(
            tmp.store
                .map
                .read()
                .contains_key(&(a.format(), "h_c".into(), "out".into()))
        );
        assert!(!durable_has(&durable, &a, "h_c", "out"));
    }
}
