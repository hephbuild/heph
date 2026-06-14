use crate::htpkg::PkgBuf;
use rustc_hash::{FxHashSet, FxHasher};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{Arc, OnceLock, RwLock};
use xxhash_rust::xxh3::Xxh3Default;

#[derive(Debug, PartialEq, Eq)]
pub struct AddrInner {
    pub package: PkgBuf,
    pub name: String,
    pub args: BTreeMap<String, String>,
}

impl Hash for AddrInner {
    fn hash<H: Hasher>(&self, h: &mut H) {
        Hasher::write(h, self.package.as_bytes());
        Hasher::write(h, self.name.as_bytes());
        for (k, v) in &self.args {
            Hasher::write(h, k.as_bytes());
            Hasher::write(h, v.as_bytes());
        }
    }
}

#[derive(Clone)]
pub struct Addr(Arc<AddrInner>);

// Sharded intern table. Pre-hash the inner once, route to one of `SHARDS`
// `RwLock<FxHashSet>` shards by the low bits of the hash. Reads take the
// shard's read lock; insertions take its write lock with a double-check.
// Eliminates the global-mutex contention that dominated `Addr::new` under
// many workers.
const SHARDS: usize = 64;

static INTERNED: OnceLock<[RwLock<FxHashSet<Arc<AddrInner>>>; SHARDS]> = OnceLock::new();

fn intern_shards() -> &'static [RwLock<FxHashSet<Arc<AddrInner>>>; SHARDS] {
    INTERNED.get_or_init(|| std::array::from_fn(|_| RwLock::new(FxHashSet::default())))
}

fn intern(inner: AddrInner) -> Arc<AddrInner> {
    let mut h = FxHasher::default();
    inner.hash(&mut h);
    let hash = h.finish();
    let shard = intern_shards()
        .get((hash as usize) & (SHARDS - 1))
        .expect("shard index masked to SHARDS range");

    {
        let r = shard.read().expect("addr intern shard poisoned");
        if let Some(existing) = r.get(&inner) {
            return existing.clone();
        }
    }

    let mut w = shard.write().expect("addr intern shard poisoned");
    if let Some(existing) = w.get(&inner) {
        return existing.clone();
    }
    let arc = Arc::new(inner);
    w.insert(arc.clone());
    arc
}

impl Addr {
    pub fn new(package: PkgBuf, name: String, args: BTreeMap<String, String>) -> Self {
        Self(intern(AddrInner {
            package,
            name,
            args,
        }))
    }

    pub fn format(&self) -> String {
        format!("{}", self)
    }

    pub fn hash_str(&self) -> String {
        let mut h = Xxh3Default::new();
        self.hash(&mut h);
        format!("{:x}", h.digest())
    }
}

impl Deref for Addr {
    type Target = AddrInner;

    #[inline]
    fn deref(&self) -> &AddrInner {
        &self.0
    }
}

impl Debug for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&*self.0, f)
    }
}

impl PartialEq for Addr {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        // Interning guarantees content-equal Addrs share the same Arc.
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Addr {}

impl Hash for Addr {
    #[inline]
    fn hash<H: Hasher>(&self, h: &mut H) {
        // Content-based hash — Addr hashes flow into target def.hash which is
        // persisted as a cache key, so the hash must be stable across runs.
        // Pointer hash would change between processes and break the disk cache.
        self.0.hash(h);
    }
}

impl Serialize for Addr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.format())
    }
}

impl Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "//{}:{}", self.package, self.name)?;
        if !self.args.is_empty() {
            write!(f, "@")?;
            for (i, (k, v)) in self.args.iter().enumerate() {
                if i > 0 {
                    write!(f, ",")?;
                }
                // Quote values that contain chars that would break bare_value parsing
                if v.is_empty() || v.contains([' ', ',', '|', '"']) {
                    write!(f, "{}=\"{}\"", k, v)?;
                } else {
                    write!(f, "{}={}", k, v)?;
                }
            }
        }
        Ok(())
    }
}

impl Default for Addr {
    fn default() -> Self {
        Addr::new(PkgBuf::from(""), String::new(), BTreeMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_returns_same_arc_for_equal_addrs() {
        let a = Addr::new(PkgBuf::from("foo/bar"), "baz".to_string(), BTreeMap::new());
        let b = Addr::new(PkgBuf::from("foo/bar"), "baz".to_string(), BTreeMap::new());
        assert!(Arc::ptr_eq(&a.0, &b.0));
        assert_eq!(a, b);
    }

    #[test]
    fn intern_distinguishes_different_addrs() {
        let a = Addr::new(PkgBuf::from("foo"), "a".to_string(), BTreeMap::new());
        let b = Addr::new(PkgBuf::from("foo"), "b".to_string(), BTreeMap::new());
        assert!(!Arc::ptr_eq(&a.0, &b.0));
        assert_ne!(a, b);
    }

    #[test]
    fn intern_concurrent_inserts_share_arc() {
        let pkg = "concurrent/pkg";
        let name = "tgt";
        let threads: Vec<_> = (0..16)
            .map(|_| {
                std::thread::spawn(move || {
                    Addr::new(PkgBuf::from(pkg), name.to_string(), BTreeMap::new())
                })
            })
            .collect();
        let addrs: Vec<Addr> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        let first = &addrs[0];
        for a in &addrs[1..] {
            assert!(Arc::ptr_eq(&first.0, &a.0));
        }
    }
}
