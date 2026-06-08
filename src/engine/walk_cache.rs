//! Cross-run cache of filesystem-walk results.
//!
//! Several plugins re-walk the workspace tree on every run because their targets
//! are intentionally uncacheable: the fs `Driver` re-globs source files, the
//! buildfile `Provider` re-discovers packages. The tree rarely changes between
//! runs, so these walks are repeated work.
//!
//! [`WalkCache`] memoizes a walk's result across runs in the durable cache's
//! namespaced KV store (see [`LocalCache::kv_get`]). Each entry pairs a
//! [`WalkSignature`] — directory mtimes (the matched *set*) plus optional
//! per-file `(size, mtime)` (file *content*) — with a borsh value. A lookup
//! returns the value only when the signature still validates against the live
//! tree; otherwise the caller walks and re-inserts.
//!
//! `mtime+size` is a fast-path proxy for content identity (heph otherwise hashes
//! content precisely); a same-size in-place rewrite within the filesystem's mtime
//! granularity can be missed — an accepted tradeoff. Everything is
//! correct-by-fallback: a missing/disabled store, a decode error, or any
//! validation mismatch simply makes the caller re-walk.

use crate::engine::local_cache::LocalCache;
use borsh::{BorshDeserialize, BorshSerialize};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::path::Path;
use std::sync::Arc;

/// Nanoseconds since the unix epoch for `meta`'s mtime, or `None` if unreadable
/// (pre-epoch or unsupported) — a walk with any unreadable mtime is not cached.
pub fn mtime_ns(meta: &std::fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(d.as_nanos()).ok()
}

/// Validation fingerprint for a filesystem walk: the directories it descended
/// (by mtime) and, optionally, the files it read (by size + mtime).
#[derive(Clone, Default, Debug, BorshSerialize, BorshDeserialize)]
pub struct WalkSignature {
    /// `(path relative to root, mtime_ns)` for every directory descended. A
    /// directory's mtime bumps on any entry add/remove/rename, so matching all of
    /// them proves the matched file *set* is unchanged without re-reading them.
    pub dirs: Vec<(String, i64)>,
    /// `(path relative to root, size, mtime_ns)` for content-sensitive walks.
    /// Empty when only the directory *set* matters (e.g. discovering which dirs
    /// contain a marker file).
    pub files: Vec<(String, u64, i64)>,
}

impl WalkSignature {
    /// Record a directory's mtime under `root`. Returns `false` if the mtime is
    /// unreadable (⇒ the caller should mark the walk non-persistable).
    pub fn push_dir(&mut self, rel: impl Into<String>, meta: &std::fs::Metadata) -> bool {
        match mtime_ns(meta) {
            Some(mt) => {
                self.dirs.push((rel.into(), mt));
                true
            }
            None => false,
        }
    }

    /// Record a file's `(size, mtime)` under `root`. Returns `false` if the mtime
    /// is unreadable.
    pub fn push_file(&mut self, rel: impl Into<String>, meta: &std::fs::Metadata) -> bool {
        match mtime_ns(meta) {
            Some(mt) => {
                self.files.push((rel.into(), meta.len(), mt));
                true
            }
            None => false,
        }
    }

    /// True iff the tree under `root` still matches: every recorded directory
    /// mtime and every recorded file `(size, mtime)` is unchanged.
    pub fn is_valid(&self, root: &Path) -> bool {
        for (rel, mt) in &self.dirs {
            match std::fs::metadata(root.join(rel)) {
                Ok(m) if m.is_dir() && mtime_ns(&m) == Some(*mt) => {}
                _ => return false,
            }
        }
        for (rel, size, mt) in &self.files {
            match std::fs::metadata(root.join(rel)) {
                Ok(m) if !m.is_dir() && m.len() == *size && mtime_ns(&m) == Some(*mt) => {}
                _ => return false,
            }
        }
        true
    }
}

const WALK_CACHE_VERSION: u32 = 1;

#[derive(BorshSerialize, BorshDeserialize)]
struct StoredEntry<T> {
    version: u32,
    sig: WalkSignature,
    value: T,
}

/// Cross-run, in-memory-fronted cache of walk results keyed by an arbitrary
/// string, backed by a [`LocalCache`] KV namespace.
///
/// The KV namespace is scanned once (lazily, on first access) into an in-memory
/// map; lookups then serve from memory. Inserts write-through to the KV
/// incrementally, so a pure cache-hit run performs no writes. Constructed with
/// `None` (or a backend whose KV is a no-op) it degrades to always-miss.
pub struct WalkCache<T> {
    cache: Option<Arc<dyn LocalCache>>,
    ns: String,
    inner: Mutex<Inner<T>>,
}

struct Inner<T> {
    loaded: bool,
    map: FxHashMap<String, Arc<StoredEntry<T>>>,
}

impl<T> WalkCache<T>
where
    T: BorshSerialize + BorshDeserialize + Clone,
{
    /// A cache backed by `cache`'s KV namespace `ns`. `None` disables it
    /// (always-miss, no writes).
    pub fn new(cache: Option<Arc<dyn LocalCache>>, ns: impl Into<String>) -> Self {
        Self {
            cache,
            ns: ns.into(),
            inner: Mutex::new(Inner {
                loaded: false,
                map: FxHashMap::default(),
            }),
        }
    }

    fn ensure_loaded(&self, cache: &dyn LocalCache, inner: &mut Inner<T>) {
        if inner.loaded {
            return;
        }
        inner.loaded = true;
        let Ok(rows) = cache.kv_list(&self.ns) else {
            return;
        };
        for (k, bytes) in rows {
            if let Ok(entry) = borsh::from_slice::<StoredEntry<T>>(&bytes)
                && entry.version == WALK_CACHE_VERSION
            {
                inner.map.insert(k, Arc::new(entry));
            }
        }
    }

    /// Returns the cached value for `key` if its [`WalkSignature`] still validates
    /// against the tree at `root`; otherwise `None` (the caller should walk and
    /// [`insert`](Self::insert)).
    pub fn get(&self, key: &str, root: &Path) -> Option<T> {
        let cache = self.cache.as_deref()?;
        let entry = {
            let mut inner = self.inner.lock();
            self.ensure_loaded(cache, &mut inner);
            inner.map.get(key).cloned()
        }?;
        entry.sig.is_valid(root).then(|| entry.value.clone())
    }

    /// Records a fresh walk result: updates the in-memory map and write-through
    /// to the KV (best-effort). No-op when the cache is disabled.
    pub fn insert(&self, key: impl Into<String>, sig: WalkSignature, value: T) {
        let Some(cache) = self.cache.as_deref() else {
            return;
        };
        let key = key.into();
        let entry = Arc::new(StoredEntry {
            version: WALK_CACHE_VERSION,
            sig,
            value,
        });
        if let Ok(bytes) = borsh::to_vec(&*entry) {
            drop(cache.kv_put(&self.ns, &key, &bytes));
        }
        self.inner.lock().map.insert(key, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Minimal `LocalCache` exposing only the KV methods over an in-memory map.
    #[derive(Default)]
    struct KvMock {
        kv: Mutex<HashMap<(String, String), Vec<u8>>>,
    }
    impl LocalCache for KvMock {
        fn reader(
            &self,
            _a: &crate::htaddr::Addr,
            _h: &str,
            _n: &str,
        ) -> anyhow::Result<crate::engine::local_cache::SizedReader> {
            unimplemented!()
        }
        fn writer(
            &self,
            _a: &crate::htaddr::Addr,
            _h: &str,
            _n: &str,
        ) -> anyhow::Result<Box<dyn std::io::Write>> {
            unimplemented!()
        }
        fn exists(&self, _a: &crate::htaddr::Addr, _h: &str, _n: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn delete(&self, _a: &crate::htaddr::Addr, _h: &str, _n: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn kv_get(&self, ns: &str, k: &str) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(self.kv.lock().get(&(ns.to_owned(), k.to_owned())).cloned())
        }
        fn kv_list(&self, ns: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
            Ok(self
                .kv
                .lock()
                .iter()
                .filter(|((n, _), _)| n == ns)
                .map(|((_, k), v)| (k.clone(), v.clone()))
                .collect())
        }
        fn kv_put(&self, ns: &str, k: &str, v: &[u8]) -> anyhow::Result<()> {
            self.kv
                .lock()
                .insert((ns.to_owned(), k.to_owned()), v.to_vec());
            Ok(())
        }
    }

    #[test]
    fn signature_validates_and_detects_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/f"), b"abc").unwrap();

        let mut sig = WalkSignature::default();
        sig.push_dir("d", &std::fs::metadata(root.join("d")).unwrap());
        sig.push_file("d/f", &std::fs::metadata(root.join("d/f")).unwrap());
        assert!(sig.is_valid(root), "unchanged tree validates");

        // Content change (different size) invalidates.
        std::fs::write(root.join("d/f"), b"a longer body").unwrap();
        assert!(!sig.is_valid(root), "changed file size invalidates");
    }

    #[test]
    fn signature_detects_dir_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("d")).unwrap();
        let mut sig = WalkSignature::default();
        sig.push_dir("d", &std::fs::metadata(root.join("d")).unwrap());
        assert!(sig.is_valid(root));

        std::fs::File::open(root.join("d"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(7200))
            .unwrap();
        assert!(!sig.is_valid(root), "bumped dir mtime invalidates");
    }

    #[test]
    fn cache_roundtrips_through_kv_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("d")).unwrap();

        let backend: Arc<dyn LocalCache> = Arc::new(KvMock::default());
        let mut sig = WalkSignature::default();
        sig.push_dir("d", &std::fs::metadata(root.join("d")).unwrap());

        // First WalkCache populates the KV.
        {
            let wc: WalkCache<Vec<String>> = WalkCache::new(Some(backend.clone()), "test");
            assert!(wc.get("k", root).is_none(), "cold miss");
            wc.insert(
                "k",
                sig.clone(),
                vec!["pkg/a".to_string(), "pkg/b".to_string()],
            );
            assert_eq!(
                wc.get("k", root).unwrap().len(),
                2,
                "warm hit, same process"
            );
        }

        // A fresh WalkCache loads from the shared KV (simulates a new run).
        let wc2: WalkCache<Vec<String>> = WalkCache::new(Some(backend.clone()), "test");
        assert_eq!(
            wc2.get("k", root).unwrap(),
            vec!["pkg/a".to_string(), "pkg/b".to_string()],
            "fresh cache reloads from KV and validates"
        );

        // After a dir mtime bump the entry no longer validates.
        std::fs::File::open(root.join("d"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(7200))
            .unwrap();
        let wc3: WalkCache<Vec<String>> = WalkCache::new(Some(backend), "test");
        assert!(wc3.get("k", root).is_none(), "stale entry invalidates");
    }

    #[test]
    fn disabled_cache_always_misses() {
        let dir = tempfile::tempdir().unwrap();
        let wc: WalkCache<Vec<String>> = WalkCache::new(None, "test");
        wc.insert("k", WalkSignature::default(), vec!["x".to_string()]);
        assert!(wc.get("k", dir.path()).is_none());
    }
}
