//! Process-wide cache of decoded layer chunks.
//!
//! A layer's opener runs per FUSE read, so no backing handle (a pooled
//! sqlite connection, a seekable zstd decoder) is held for the life of a
//! mount. For a compressed blob that makes every read re-read the seek table
//! and decode a whole frame to return the 128 KiB the kernel asked for: a
//! sequential read of a file decodes each frame about eight times.
//!
//! A layer opted in with [`crate::Layer::with_read_chunk`] reads its source
//! in aligned chunks instead, and keeps the decoded chunks here. The cache
//! holds bytes, not readers, so it pins no backing resource.
//!
//! - **Key**: `(layer id, chunk index)`. The id is a process-wide counter
//!   handed out when the layer is built, never an address, so a dropped
//!   layer's entries cannot be served to a later one at the same address.
//! - **Bound**: a fixed number of chunks, split across shards, each shard
//!   evicting its least recently used chunk. [`ChunkCache::global`] holds
//!   [`GLOBAL_SHARDS`] × [`GLOBAL_CHUNKS_PER_SHARD`] chunks, 32 MiB at the
//!   1 MiB chunk size. A dropped layer's chunks are released at once.
//! - **Locking**: a shard's lock is held only to find or insert an entry.
//!   The chunk is loaded under that entry's own lock, outside the shard's,
//!   so concurrent reads of one chunk decode it once and reads of other
//!   chunks do not wait on the decode.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Shards of [`ChunkCache::global`].
pub(crate) const GLOBAL_SHARDS: usize = 8;
/// Chunks each shard of [`ChunkCache::global`] keeps.
pub(crate) const GLOBAL_CHUNKS_PER_SHARD: usize = 4;

/// A layer's identity in the cache: unique for the life of the process.
pub(crate) fn next_layer_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // Only uniqueness matters; no other memory is published through it.
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ChunkKey {
    layer: u64,
    index: u64,
}

/// One chunk: `None` until its first successful load.
type ChunkSlot = Mutex<Option<Arc<[u8]>>>;

#[derive(Debug)]
struct Entry {
    key: ChunkKey,
    slot: Arc<ChunkSlot>,
    last_used: u64,
}

#[derive(Debug, Default)]
struct Shard {
    entries: Vec<Entry>,
    clock: u64,
}

/// A bounded cache of decoded chunks, shared by every chunked layer.
#[derive(Debug)]
pub(crate) struct ChunkCache {
    shards: Box<[Mutex<Shard>]>,
    per_shard: usize,
}

impl ChunkCache {
    /// A cache of `shards` shards keeping `per_shard` chunks each (both at
    /// least 1).
    pub(crate) fn new(shards: usize, per_shard: usize) -> Self {
        Self {
            shards: (0..shards.max(1))
                .map(|_| Mutex::new(Shard::default()))
                .collect(),
            per_shard: per_shard.max(1),
        }
    }

    /// The cache every layer built with [`crate::Layer::with_read_chunk`]
    /// shares.
    pub(crate) fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<ChunkCache>> = OnceLock::new();
        Arc::clone(
            GLOBAL.get_or_init(|| Arc::new(Self::new(GLOBAL_SHARDS, GLOBAL_CHUNKS_PER_SHARD))),
        )
    }

    fn shard(&self, key: ChunkKey) -> &Mutex<Shard> {
        // Fibonacci hashing: consecutive chunks of one layer land on
        // different shards.
        let mixed = (key.layer.rotate_left(32) ^ key.index).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let i = (mixed >> 32) as usize % self.shards.len();
        #[expect(
            clippy::indexing_slicing,
            reason = "i < shards.len() by the modulo; shards is never empty"
        )]
        &self.shards[i]
    }

    /// The slot for `key`, inserted (evicting the shard's least recently
    /// used chunk when full) if absent.
    fn slot(&self, key: ChunkKey) -> Arc<ChunkSlot> {
        let mut shard = self.shard(key).lock();
        shard.clock += 1;
        let now = shard.clock;
        if let Some(e) = shard.entries.iter_mut().find(|e| e.key == key) {
            e.last_used = now;
            return Arc::clone(&e.slot);
        }
        let slot = Arc::new(Mutex::new(None));
        let entry = Entry {
            key,
            slot: Arc::clone(&slot),
            last_used: now,
        };
        if shard.entries.len() < self.per_shard {
            shard.entries.push(entry);
        } else if let Some(lru) = shard.entries.iter_mut().min_by_key(|e| e.last_used) {
            *lru = entry;
        }
        slot
    }

    /// Chunk `index` of `layer`, from the cache or else from `load`. A
    /// failed load is not cached.
    pub(crate) fn get_or_load(
        &self,
        layer: u64,
        index: u64,
        load: impl FnOnce() -> std::io::Result<Vec<u8>>,
    ) -> std::io::Result<Arc<[u8]>> {
        let slot = self.slot(ChunkKey { layer, index });
        let mut chunk = slot.lock();
        if let Some(bytes) = &*chunk {
            return Ok(Arc::clone(bytes));
        }
        let bytes: Arc<[u8]> = load()?.into();
        *chunk = Some(Arc::clone(&bytes));
        Ok(bytes)
    }

    /// Drop every chunk of `layer`.
    pub(crate) fn forget_layer(&self, layer: u64) {
        for shard in &*self.shards {
            shard.lock().entries.retain(|e| e.key.layer != layer);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().entries.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn load_counted(calls: &AtomicUsize, byte: u8) -> impl FnOnce() -> std::io::Result<Vec<u8>> {
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![byte; 4])
        }
    }

    #[test]
    fn hit_does_not_load_again() {
        let cache = ChunkCache::new(1, 2);
        let calls = AtomicUsize::new(0);
        let a = cache
            .get_or_load(1, 0, load_counted(&calls, 7))
            .expect("load");
        let b = cache
            .get_or_load(1, 0, load_counted(&calls, 9))
            .expect("hit");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(&*a, &*b);
    }

    #[test]
    fn evicts_least_recently_used_at_capacity() {
        let cache = ChunkCache::new(1, 2);
        let calls = AtomicUsize::new(0);
        for index in [0, 1] {
            cache
                .get_or_load(1, index, load_counted(&calls, 0))
                .expect("load");
        }
        // Touch 0 so 1 is the least recently used, then insert 2.
        cache
            .get_or_load(1, 0, load_counted(&calls, 0))
            .expect("hit");
        cache
            .get_or_load(1, 2, load_counted(&calls, 0))
            .expect("load");
        assert_eq!(cache.len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        cache
            .get_or_load(1, 0, load_counted(&calls, 0))
            .expect("hit");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "0 was evicted");
        cache
            .get_or_load(1, 1, load_counted(&calls, 0))
            .expect("load");
        assert_eq!(calls.load(Ordering::SeqCst), 4, "1 was kept");
    }

    #[test]
    fn layers_do_not_share_chunks() {
        let cache = ChunkCache::new(1, 4);
        let calls = AtomicUsize::new(0);
        let a = cache.get_or_load(1, 0, load_counted(&calls, 1)).expect("a");
        let b = cache.get_or_load(2, 0, load_counted(&calls, 2)).expect("b");
        assert_eq!((a[0], b[0]), (1, 2));
        cache.forget_layer(1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn failed_load_is_retried() {
        let cache = ChunkCache::new(1, 2);
        cache
            .get_or_load(1, 0, || Err(std::io::Error::other("boom")))
            .expect_err("load fails");
        let ok = cache.get_or_load(1, 0, || Ok(vec![3])).expect("retry");
        assert_eq!(&*ok, &[3]);
    }
}
