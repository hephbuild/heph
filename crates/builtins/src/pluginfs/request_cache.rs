//! The fs plugin's per-request caches (compiled excludes, glob walk results),
//! and how a bucket leaves once its request is gone.
//!
//! They are keyed by `request_id`, not by the engine's cache scope: an
//! in-place codegen target is resolved under a fresh request of its own so
//! that its globs re-stat the tree the write-back changed. A request id is
//! never reused, so a bucket whose request ended is never read again — it is
//! only memory. Nothing tells the fs plugin a request ended, so each insert
//! asks the engine which requests are still live ([`LiveRequests`]):
//!
//! - an insert for a request that is no longer live stores nothing (a glob
//!   walk can finish after its request dropped);
//! - each insert that opens a new bucket checks a few existing buckets, in
//!   the order they were opened, and drops the ones whose request ended.
//!
//! A bucket opened without a [`LiveRequests`] (a driver built without an
//! engine: unit tests) is never dropped, as before.

use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::sync::Arc;

/// Which requests are still live: the engine's request registry. Lets the fs
/// plugin drop what it cached for a request once the request is gone.
pub trait LiveRequests: Send + Sync {
    /// Whether the request `request_id` has not ended yet.
    fn is_live(&self, request_id: &str) -> bool;
}

/// How many existing buckets an insert that opens a new one checks. Every new
/// request opens at most one bucket per cache, so the dead ones are swept
/// faster than they are made.
const SWEEP_PER_INSERT: usize = 4;

/// One cache: `request_id` → its entries.
pub(crate) struct RequestCache<V> {
    inner: RwLock<Inner<V>>,
}

struct Inner<V> {
    buckets: FxHashMap<String, Bucket<V>>,
    /// Every bucket's request id, in sweep order: a checked bucket that is
    /// still live goes to the back.
    order: VecDeque<String>,
}

struct Bucket<V> {
    /// The registry the request belongs to; `None` is never swept.
    live: Option<Arc<dyn LiveRequests>>,
    entries: FxHashMap<String, V>,
}

impl<V: Clone> RequestCache<V> {
    pub(crate) fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                buckets: FxHashMap::default(),
                order: VecDeque::new(),
            }),
        }
    }

    /// The entry `key` cached for `request_id`.
    pub(crate) fn get(&self, request_id: &str, key: &str) -> Option<V> {
        self.inner
            .read()
            .buckets
            .get(request_id)
            .and_then(|b| b.entries.get(key))
            .cloned()
    }

    /// The entry `key` of `request_id`, stored from `value` unless a
    /// concurrent insert landed first (every caller then sees one value). A
    /// request `live` says has ended stores nothing and gets `value` back.
    pub(crate) fn insert(
        &self,
        live: Option<&Arc<dyn LiveRequests>>,
        request_id: &str,
        key: String,
        value: V,
    ) -> V {
        if let Some(live) = live
            && !live.is_live(request_id)
        {
            tracing::debug!(request_id, "fs cache write for an ended request not stored");
            // Its bucket, if any, goes too.
            let gone = {
                let mut inner = self.inner.write();
                let gone = inner.buckets.remove(request_id);
                inner.order.retain(|id| id != request_id);
                gone
            };
            drop(gone);
            return value;
        }
        let mut swept = Vec::new();
        let stored = {
            let mut inner = self.inner.write();
            if !inner.buckets.contains_key(request_id) {
                inner.sweep(&mut swept);
                inner.order.push_back(request_id.to_owned());
                inner.buckets.insert(
                    request_id.to_owned(),
                    Bucket {
                        live: live.cloned(),
                        entries: FxHashMap::default(),
                    },
                );
            }
            match inner.buckets.get_mut(request_id) {
                Some(bucket) => bucket.entries.entry(key).or_insert(value).clone(),
                None => value,
            }
        };
        // Freed after the lock: a bucket can hold a request's walk results.
        drop(swept);
        stored
    }

    /// Whether a bucket for `request_id` exists.
    #[cfg(test)]
    pub(crate) fn has_bucket(&self, request_id: &str) -> bool {
        self.inner.read().buckets.contains_key(request_id)
    }
}

impl<V> Inner<V> {
    /// Check up to [`SWEEP_PER_INSERT`] buckets from the front of the order,
    /// moving the dropped ones into `swept`.
    fn sweep(&mut self, swept: &mut Vec<Bucket<V>>) {
        for _ in 0..SWEEP_PER_INSERT.min(self.order.len()) {
            let Some(id) = self.order.pop_front() else {
                break;
            };
            let ended = self
                .buckets
                .get(&id)
                .and_then(|b| b.live.as_ref())
                .is_some_and(|live| !live.is_live(&id));
            if ended {
                swept.extend(self.buckets.remove(&id));
            } else if self.buckets.contains_key(&id) {
                self.order.push_back(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// A request registry: the ids in the set are live.
    #[derive(Default)]
    struct Registry(Mutex<HashSet<String>>);

    impl Registry {
        fn start(&self, id: &str) {
            self.0.lock().unwrap().insert(id.to_owned());
        }
        fn end(&self, id: &str) {
            self.0.lock().unwrap().remove(id);
        }
    }

    impl LiveRequests for Registry {
        fn is_live(&self, request_id: &str) -> bool {
            self.0.lock().unwrap().contains(request_id)
        }
    }

    /// An ended request's buckets go on later inserts — the root request's
    /// and a fresh in-place state's (a request id of its own, nested in the
    /// root) alike — while a live request's stay, and a bucket opened without
    /// a registry (a driver with no engine) is never dropped.
    #[test]
    fn fs_glob_caches_pruned_on_scope_end() {
        let registry = Arc::new(Registry::default());
        let live: Arc<dyn LiveRequests> = registry.clone();
        let cache = RequestCache::<Arc<u32>>::new();

        for id in ["root", "in-place", "other"] {
            registry.start(id);
            cache.insert(Some(&live), id, "k".into(), Arc::new(1));
        }
        cache.insert(None, "no-engine", "k".into(), Arc::new(1));
        assert_eq!(cache.get("in-place", "k").as_deref(), Some(&1));

        // The in-place state ends first, then the root; `other` stays live.
        registry.end("in-place");
        registry.end("root");
        for n in 0..32 {
            let id = format!("next-{n}");
            registry.start(&id);
            cache.insert(Some(&live), &id, "k".into(), Arc::new(2));
            registry.end(&id);
        }
        assert!(!cache.has_bucket("root"), "the root request's bucket");
        assert!(!cache.has_bucket("in-place"), "the in-place state's bucket");
        assert!(cache.has_bucket("other"), "a live request keeps its bucket");
        assert!(cache.has_bucket("no-engine"), "no registry: never swept");
        assert!(
            cache.inner.read().buckets.len() <= SWEEP_PER_INSERT + 2,
            "ended buckets do not accumulate",
        );
    }

    /// A glob walk that finishes after its request ended stores nothing, and
    /// takes the request's bucket with it.
    #[test]
    fn late_glob_write_after_request_end_is_not_stored() {
        let registry = Arc::new(Registry::default());
        let live: Arc<dyn LiveRequests> = registry.clone();
        let cache = RequestCache::<Arc<u32>>::new();

        registry.start("r");
        cache.insert(Some(&live), "r", "early".into(), Arc::new(1));
        registry.end("r");
        let late = cache.insert(Some(&live), "r", "late".into(), Arc::new(2));
        assert_eq!(*late, 2, "the caller still gets its walk");
        assert_eq!(cache.get("r", "late"), None, "but it is not stored");
        assert!(!cache.has_bucket("r"), "and the ended bucket is gone");
        assert!(cache.inner.read().order.is_empty());
    }
}
