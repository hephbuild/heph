use crate::engine::local_cache::{LocalCache, SizedReader, TargetStream};
use heph_core::hartifactcontent;
use heph_model::htaddr::Addr;
use anyhow::{Context, Result};
use quick_cache::Weighter;
use quick_cache::sync::Cache;
use std::io;
use std::sync::Arc;

type Key = (String, String, String);

#[derive(Clone)]
struct ByteWeighter;

impl Weighter<Key, Arc<[u8]>> for ByteWeighter {
    fn weight(&self, _key: &Key, val: &Arc<[u8]>) -> u64 {
        val.len() as u64
    }
}

pub struct LocalCacheMem {
    inner: Arc<dyn LocalCache>,
    cache: Cache<Key, Arc<[u8]>, ByteWeighter>,
    per_entry_bytes: usize,
}

impl LocalCacheMem {
    pub fn new(inner: Arc<dyn LocalCache>, per_entry_bytes: usize, capacity_bytes: u64) -> Self {
        // Rough item-slot estimate; quick_cache uses it to size internal shards.
        let est_items = (capacity_bytes / per_entry_bytes.max(1) as u64).max(16) as usize;
        Self {
            inner,
            cache: Cache::with_weighter(est_items, capacity_bytes, ByteWeighter),
            per_entry_bytes,
        }
    }

    fn key(addr: &Addr, hashin: &str, name: &str) -> Key {
        (addr.format(), hashin.to_string(), name.to_string())
    }
}

impl LocalCache for LocalCacheMem {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<SizedReader> {
        let key = Self::key(addr, hashin, name);

        // Hit: skip the inner backend entirely.
        if let Some(buf) = self.cache.get(&key) {
            let size = buf.len() as u64;
            return Ok(SizedReader {
                size,
                reader: Box::new(io::Cursor::new(buf.clone())),
                bytes: Some(buf),
            });
        }

        // Miss: ask inner. Size is reported up front by the trait contract.
        let sized = self.inner.reader(addr, hashin, name)?;

        // Too big to cache: pass straight through, no buffering.
        if sized.size > self.per_entry_bytes as u64 {
            return Ok(sized);
        }

        // Fast path: inner already produced an in-memory buffer (e.g. SQLite
        // inline-read path). Cache the Arc directly, no drain.
        if let Some(arc) = sized.bytes.clone() {
            self.cache.insert(key, arc);
            return Ok(sized);
        }

        // Streaming path: drain into Arc<[u8]>, populate cache, hand back a Cursor.
        let size_usize = sized.size as usize;
        let mut buf: Vec<u8> = Vec::with_capacity(size_usize);
        let mut rdr = sized.reader;
        io::copy(&mut rdr, &mut buf).with_context(|| format!("draining {name} into mem cache"))?;
        let arc: Arc<[u8]> = Arc::from(buf);
        self.cache.insert(key, arc.clone());
        Ok(SizedReader {
            size: arc.len() as u64,
            reader: Box::new(io::Cursor::new(arc.clone())),
            bytes: Some(arc),
        })
    }

    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
        let key = Self::key(addr, hashin, name);
        // Invalidate before forwarding. A reader arriving after this point misses the mem
        // cache and falls through to the inner backend, which is responsible for its own
        // read-after-write ordering (e.g. LocalCacheSQLite's PendingTracker).
        self.cache.remove(&key);
        self.inner.writer(addr, hashin, name)
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        let key = Self::key(addr, hashin, name);
        // peek avoids bumping the cache recency for a presence check.
        if self.cache.peek(&key).is_some() {
            return Ok(true);
        }
        self.inner.exists(addr, hashin, name)
    }

    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
        let key = Self::key(addr, hashin, name);
        self.cache.remove(&key);
        self.inner.delete(addr, hashin, name)
    }

    fn list_targets(&self) -> Result<TargetStream> {
        // Enumeration is authoritative at the durable backend; the mem layer
        // only fronts reads, so delegate.
        self.inner.list_targets()
    }

    fn list_target_entries(&self, addr: &Addr) -> Result<Vec<String>> {
        self.inner.list_target_entries(addr)
    }

    fn seekable_reader(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
        let key = Self::key(addr, hashin, name);
        // Cache hit: serve a Cursor over the cached Arc<[u8]>. Read+Seek+Send.
        if let Some(buf) = self.cache.get(&key) {
            return Ok(Some(Box::new(io::Cursor::new(buf))));
        }
        // Miss: delegate. Inner cache (SQLite) returns a connection-bound
        // blob; FS cache returns a File. We don't populate the mem cache
        // here — FUSE reads are random-access and may target only a
        // fraction of the artifact, so draining the whole blob would waste
        // memory. The next `reader()` call will populate the cache for
        // streaming consumers.
        self.inner.seekable_reader(addr, hashin, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::local_cache::NotFoundError;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory LocalCache backing used to instrument call counts.
    /// Internals are wrapped in Arcs so writer closures can capture them
    /// without needing back-references to the outer cache.
    #[derive(Default, Clone)]
    struct CountingCache {
        store: Arc<Mutex<HashMap<Key, Vec<u8>>>>,
        reader_calls: Arc<AtomicUsize>,
        writer_calls: Arc<AtomicUsize>,
        delete_calls: Arc<AtomicUsize>,
        exists_calls: Arc<AtomicUsize>,
    }

    impl CountingCache {
        fn key(addr: &Addr, hashin: &str, name: &str) -> Key {
            (addr.format(), hashin.to_string(), name.to_string())
        }
    }

    struct VecWriter {
        store: Arc<Mutex<HashMap<Key, Vec<u8>>>>,
        key: Key,
        buf: Vec<u8>,
    }

    impl io::Write for VecWriter {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for VecWriter {
        fn drop(&mut self) {
            self.store
                .lock()
                .insert(self.key.clone(), std::mem::take(&mut self.buf));
        }
    }

    impl LocalCache for CountingCache {
        fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<SizedReader> {
            self.reader_calls.fetch_add(1, Ordering::Relaxed);
            let key = CountingCache::key(addr, hashin, name);
            let g = self.store.lock();
            match g.get(&key) {
                None => Err(anyhow::anyhow!(NotFoundError)),
                Some(v) => {
                    let size = v.len() as u64;
                    Ok(SizedReader {
                        size,
                        reader: Box::new(io::Cursor::new(v.clone())),
                        bytes: None,
                    })
                }
            }
        }

        fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
            self.writer_calls.fetch_add(1, Ordering::Relaxed);
            let key = CountingCache::key(addr, hashin, name);
            Ok(Box::new(VecWriter {
                store: self.store.clone(),
                key,
                buf: Vec::new(),
            }))
        }

        fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
            self.exists_calls.fetch_add(1, Ordering::Relaxed);
            let key = CountingCache::key(addr, hashin, name);
            Ok(self.store.lock().contains_key(&key))
        }

        fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
            self.delete_calls.fetch_add(1, Ordering::Relaxed);
            let key = CountingCache::key(addr, hashin, name);
            self.store.lock().remove(&key);
            Ok(())
        }

        fn seekable_reader(
            &self,
            addr: &Addr,
            hashin: &str,
            name: &str,
        ) -> Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
            let key = CountingCache::key(addr, hashin, name);
            let g = self.store.lock();
            match g.get(&key) {
                None => Err(anyhow::anyhow!(NotFoundError)),
                Some(v) => Ok(Some(Box::new(io::Cursor::new(v.clone())))),
            }
        }
    }

    fn make_addr() -> Addr {
        Addr::new(
            heph_model::htpkg::PkgBuf::from("pkg"),
            "tgt".to_string(),
            Default::default(),
        )
    }

    fn drain(mut r: Box<dyn io::Read>) -> Vec<u8> {
        let mut out = Vec::new();
        r.read_to_end(&mut out).expect("read");
        out
    }

    fn write_blob(cache: &dyn LocalCache, addr: &Addr, name: &str, data: &[u8]) {
        let mut w = cache.writer(addr, "h1", name).expect("writer");
        w.write_all(data).expect("write");
        drop(w);
    }

    #[test]
    fn hit_skips_inner_after_first_read() {
        let inner = Arc::new(CountingCache::default());
        let dec = LocalCacheMem::new(inner.clone(), 1024, 64 * 1024);
        let addr = make_addr();

        write_blob(&dec, &addr, "small", b"hello");

        // First read: miss → inner called once.
        let r = dec.reader(&addr, "h1", "small").expect("r1");
        assert_eq!(r.size, 5);
        assert_eq!(drain(r.reader), b"hello");
        assert_eq!(inner.reader_calls.load(Ordering::Relaxed), 1);

        // Second read: hit → inner not called again.
        let r = dec.reader(&addr, "h1", "small").expect("r2");
        assert_eq!(drain(r.reader), b"hello");
        assert_eq!(inner.reader_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn miss_too_big_does_not_populate() {
        let inner = Arc::new(CountingCache::default());
        let dec = LocalCacheMem::new(inner.clone(), 8, 64 * 1024);
        let addr = make_addr();

        write_blob(&dec, &addr, "big", b"0123456789"); // 10 bytes > 8

        let _ = drain(dec.reader(&addr, "h1", "big").expect("r1").reader);
        let _ = drain(dec.reader(&addr, "h1", "big").expect("r2").reader);

        // Both reads fall through to inner — no caching.
        assert_eq!(inner.reader_calls.load(Ordering::Relaxed), 2);
    }

    // FUSE sandbox path needs random-access reads; LocalCacheMem must
    // expose seekable_reader for both cache hits (Cursor over the cached
    // Arc<[u8]>) and misses (passthrough to inner). Regression: if it
    // defaults to None, the bridge can't mount and `fuse: on` errors with
    // "no seekable reader" even when the underlying backend supports it.
    #[test]
    fn seekable_reader_serves_cache_hit() {
        use std::io::{Read, Seek, SeekFrom};
        let inner = Arc::new(CountingCache::default());
        let dec = LocalCacheMem::new(inner.clone(), 1024, 64 * 1024);
        let addr = make_addr();

        write_blob(&dec, &addr, "k", b"0123456789");
        // Prime the mem cache with a regular read.
        let _ = drain(dec.reader(&addr, "h1", "k").expect("r1").reader);
        let inner_before = inner.reader_calls.load(Ordering::Relaxed);

        let mut sk = dec
            .seekable_reader(&addr, "h1", "k")
            .expect("ok")
            .expect("some");
        sk.seek(SeekFrom::Start(4)).expect("seek");
        let mut buf = [0u8; 3];
        sk.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"456");
        // Hit: inner not consulted.
        assert_eq!(inner.reader_calls.load(Ordering::Relaxed), inner_before);
    }

    #[test]
    fn seekable_reader_delegates_on_miss() {
        use std::io::{Read, Seek, SeekFrom};
        let inner = Arc::new(CountingCache::default());
        let dec = LocalCacheMem::new(inner.clone(), 1024, 64 * 1024);
        let addr = make_addr();

        write_blob(&dec, &addr, "k", b"abcdef");
        // No prior reader() call — cache cold.
        let mut sk = dec
            .seekable_reader(&addr, "h1", "k")
            .expect("ok")
            .expect("some");
        sk.seek(SeekFrom::Start(3)).expect("seek");
        let mut buf = [0u8; 3];
        sk.read_exact(&mut buf).expect("read");
        assert_eq!(&buf, b"def");
    }

    #[test]
    fn writer_invalidates_cache() {
        let inner = Arc::new(CountingCache::default());
        let dec = LocalCacheMem::new(inner.clone(), 1024, 64 * 1024);
        let addr = make_addr();

        write_blob(&dec, &addr, "k", b"A");
        let r = dec.reader(&addr, "h1", "k").expect("rA");
        assert_eq!(drain(r.reader), b"A"); // populates

        // Overwrite — must invalidate.
        write_blob(&dec, &addr, "k", b"BB");

        let r = dec.reader(&addr, "h1", "k").expect("rB");
        assert_eq!(drain(r.reader), b"BB");
    }

    /// Inner that always returns a pre-built `Arc<[u8]>` via `bytes: Some(...)`.
    /// Models the `LocalCacheSQLite` inline-read path.
    struct BytesInner {
        arc: Arc<[u8]>,
    }

    impl LocalCache for BytesInner {
        fn reader(&self, _addr: &Addr, _hashin: &str, _name: &str) -> Result<SizedReader> {
            Ok(SizedReader {
                size: self.arc.len() as u64,
                reader: Box::new(io::Cursor::new(self.arc.clone())),
                bytes: Some(self.arc.clone()),
            })
        }
        fn writer(&self, _: &Addr, _: &str, _: &str) -> Result<Box<dyn io::Write>> {
            unreachable!()
        }
        fn exists(&self, _: &Addr, _: &str, _: &str) -> Result<bool> {
            Ok(true)
        }
        fn delete(&self, _: &Addr, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fast_path_reuses_inner_bytes_arc() {
        let arc: Arc<[u8]> = Arc::from(b"payload".as_slice());
        let inner = Arc::new(BytesInner { arc: arc.clone() });
        let dec = LocalCacheMem::new(inner, 1024, 64 * 1024);
        let addr = make_addr();

        // First read: miss path with bytes=Some → cache the inner Arc directly.
        let r1 = dec.reader(&addr, "h1", "k").expect("r1");
        let b1 = r1.bytes.expect("bytes set by inner");
        assert!(Arc::ptr_eq(&b1, &arc), "miss path must reuse the inner Arc");

        // Second read: hit → same Arc.
        let r2 = dec.reader(&addr, "h1", "k").expect("r2");
        let b2 = r2.bytes.expect("bytes set on hit");
        assert!(Arc::ptr_eq(&b2, &arc), "hit path must return cached Arc");
    }

    #[test]
    fn exists_short_circuits_on_cache_hit() {
        let inner = Arc::new(CountingCache::default());
        let dec = LocalCacheMem::new(inner.clone(), 1024, 64 * 1024);
        let addr = make_addr();

        write_blob(&dec, &addr, "k", b"v");

        // Populate the cache.
        let _ = drain(dec.reader(&addr, "h1", "k").expect("r").reader);

        // exists must answer from the cache; inner.exists is not invoked.
        let before = inner.exists_calls.load(Ordering::Relaxed);
        assert!(dec.exists(&addr, "h1", "k").expect("exists"));
        assert_eq!(inner.exists_calls.load(Ordering::Relaxed), before);
    }

    #[test]
    fn delete_invalidates_cache() {
        let inner = Arc::new(CountingCache::default());
        let dec = LocalCacheMem::new(inner.clone(), 1024, 64 * 1024);
        let addr = make_addr();

        write_blob(&dec, &addr, "k", b"x");
        let _ = drain(dec.reader(&addr, "h1", "k").expect("r").reader); // populate
        dec.delete(&addr, "h1", "k").expect("delete");

        assert!(!dec.exists(&addr, "h1", "k").expect("exists"));
        match dec.reader(&addr, "h1", "k") {
            Ok(_) => panic!("expected NotFoundError after delete"),
            Err(e) => assert!(e.is::<NotFoundError>(), "{e:#}"),
        }
    }
}
