use crate::htpkg::PkgBuf;
use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rustc_hash::FxBuildHasher;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use std::hash::{BuildHasher, Hash, Hasher};
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

// Sharded intern table. Hash the inner once, route to one of `SHARDS`
// `RwLock<HashTable>` shards by that hash, and hand the same hash to the
// table. Reads take the shard's read lock; insertions take its write lock with
// a double-check. Eliminates the global-mutex contention that dominated
// `Addr::new` under many workers.
//
// The in-memory table only: nothing here reaches `Addr`'s `Hash`, which is
// what def hashes see.
const SHARDS: usize = 64;
/// `SHARDS.trailing_zeros()`.
const SHARD_BITS: u32 = 6;
/// The top bits hashbrown keeps as each slot's control byte (its "h2").
const H2_BITS: u32 = 7;

type InternShard = RwLock<HashTable<Arc<AddrInner>>>;

static INTERNED: OnceLock<[InternShard; SHARDS]> = OnceLock::new();

fn intern_shards() -> &'static [InternShard; SHARDS] {
    INTERNED.get_or_init(|| std::array::from_fn(|_| RwLock::new(HashTable::new())))
}

fn intern_hash(inner: &AddrInner) -> u64 {
    FxBuildHasher.hash_one(inner)
}

/// The shard owning a key of hash `hash`.
///
/// Taken from the bits just below the top `H2_BITS`, which no shard table
/// reads: the table picks a key's home bucket from the low bits, so sharding
/// on those would start every key of a shard in the same 1/`SHARDS` of its
/// buckets; and it keeps the top bits as the control byte that filters a probe,
/// so sharding on those would leave every key of a shard with nearly the same
/// control byte.
fn shard_index(hash: u64) -> usize {
    (hash >> (u64::BITS - H2_BITS - SHARD_BITS)) as usize & (SHARDS - 1)
}

fn intern(inner: AddrInner) -> Arc<AddrInner> {
    let hash = intern_hash(&inner);
    let shard = intern_shards()
        .get(shard_index(hash))
        .expect("shard index masked to SHARDS range");
    let is_inner = |e: &Arc<AddrInner>| **e == inner;

    {
        let r = shard.read().expect("addr intern shard poisoned");
        if let Some(existing) = r.find(hash, is_inner) {
            return existing.clone();
        }
    }

    let mut w = shard.write().expect("addr intern shard poisoned");
    match w.entry(hash, is_inner, |e| intern_hash(e)) {
        Entry::Occupied(e) => e.get().clone(),
        Entry::Vacant(e) => e.insert(Arc::new(inner)).get().clone(),
    }
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

    /// Many threads interning an overlapping set of distinct addrs, so the
    /// shards' tables grow (and rehash) while other threads read them: every
    /// equal addr must still come back as the one instance.
    #[test]
    fn intern_concurrent_distinct_inserts_share_arcs() {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..2000)
                        .map(|i| {
                            Addr::new(
                                PkgBuf::from("concurrent/many"),
                                format!("t{i}"),
                                BTreeMap::new(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let runs: Vec<Vec<Addr>> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        for run in &runs[1..] {
            for (a, b) in runs[0].iter().zip(run) {
                assert!(Arc::ptr_eq(&a.0, &b.0), "{a} interned twice");
            }
        }
    }

    /// The shard index shares no bit with what each shard's table reads of
    /// the same hash: the low bits (home bucket) and the top `H2_BITS`
    /// (control byte). Sharing either would confine every key of a shard to a
    /// slice of its buckets or of its control-byte values.
    #[test]
    fn shard_index_is_disjoint_from_the_tables_bits() {
        let low = (1u64 << (u64::BITS - H2_BITS - SHARD_BITS)) - 1;
        let h2 = !0u64 << (u64::BITS - H2_BITS);
        for i in 0..4096u64 {
            let hash = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            assert_eq!(shard_index(hash), shard_index(hash ^ low), "{hash:#x}");
            assert_eq!(shard_index(hash), shard_index(hash ^ h2), "{hash:#x}");
        }
    }

    /// Real addrs spread across the shards, and the keys of one shard spread
    /// across that shard's home buckets: the property the shard index exists
    /// to keep.
    #[test]
    fn intern_hashes_spread_across_shards_and_within_one() {
        let mut shards = std::collections::HashSet::new();
        let mut homes_in_shard0 = std::collections::HashSet::new();
        for i in 0..8192 {
            let inner = AddrInner {
                package: PkgBuf::from(format!("pkg/{}", i % 97)),
                name: format!("target{i}"),
                args: BTreeMap::new(),
            };
            let hash = intern_hash(&inner);
            shards.insert(shard_index(hash));
            if shard_index(hash) == 0 {
                homes_in_shard0.insert(hash & (SHARDS as u64 - 1));
            }
        }
        assert_eq!(shards.len(), SHARDS, "addrs reached only {shards:?}");
        // ~128 keys land in shard 0; with the shard taken from the low bits,
        // they would all share one value here.
        assert!(
            homes_in_shard0.len() > SHARDS / 2,
            "shard 0's keys start in only {} of {SHARDS} bucket positions",
            homes_in_shard0.len()
        );
    }
}
