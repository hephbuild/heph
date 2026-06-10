//! Size-spilling durable cache.
//!
//! Composes two backends:
//! - a **primary** ([`LocalCacheSQLite`]) holding manifests and small/medium
//!   blobs inline in the single `cache.db`, and
//! - a **blob** store ([`LocalCacheFS`]) holding large blobs as plain files.
//!
//! Routing on write (the writer holds no buffer of its own — see [`SpillWriter`]):
//! - The manifest ([`MANIFEST_V1`]) *always* goes to the primary regardless of
//!   size — GC enumerates revisions by manifest presence in the primary, so the
//!   primary must remain the authoritative index.
//! - Any other blob streams straight to the primary by default. The moment its
//!   running size crosses `spill_threshold`, the prefix written so far is
//!   migrated into the FS blob store and all further bytes stream there. Below
//!   the threshold most artifacts stay in sqlite (fast indexed access, atomic,
//!   mem-cacheable, and written with zero copies); genuinely large artifacts end
//!   up on the filesystem where throughput wins and they don't bloat the DB / WAL.
//!
//! Routing on read/exists/delete: a given `(addr, hashin, name)` lives in
//! exactly one backend, so reads try the primary first (manifests + the common
//! small-blob case hit immediately) and fall back to the FS store on
//! `NotFoundError`; deletes hit both (idempotent) so GC need not know where a
//! blob landed.
//!
//! ## GC integration
//!
//! `Engine::gc_entry` reads the manifest (primary), then calls
//! [`LocalCache::delete`] for each named artifact plus the manifest itself.
//! Because [`delete`](LocalCacheSpill::delete) removes from both backends and
//! the FS backend prunes now-empty revision/target dirs, a trimmed or orphaned
//! revision's large blobs are reclaimed from the filesystem exactly as its
//! sqlite blobs are. Enumeration ([`list_targets`](LocalCacheSpill::list_targets)
//! / [`list_target_entries`](LocalCacheSpill::list_target_entries)) delegates to
//! the primary, which is complete because every revision writes its manifest
//! there.
//!
//! ## Determinism assumption
//!
//! A blob is not cross-invalidated when rewritten: the same `(addr, hashin,
//! name)` is assumed to carry byte-identical content across writes (the engine's
//! reproducibility contract — `hashin` is the input hash), so it never flips
//! size class between the two backends. Non-deterministic targets use the
//! ephemeral tmp store, not this one.

use crate::engine::local_cache::{
    LocalCache, MANIFEST_V1, NotFoundError, SizedReader, TargetStream,
};
use crate::engine::local_cache_fs::LocalCacheFS;
use crate::hartifactcontent;
use crate::htaddr::Addr;
use anyhow::{Context, Result};
use std::io;
use std::sync::Arc;

pub struct LocalCacheSpill {
    /// Manifests + small/medium blobs.
    primary: Arc<dyn LocalCache>,
    /// Large blobs, as plain files.
    blobs: Arc<LocalCacheFS>,
    /// Blobs strictly larger than this spill to `blobs`; at-or-below stay in
    /// `primary`. The manifest ignores this and always lands in `primary`.
    spill_threshold: usize,
}

impl LocalCacheSpill {
    pub fn new(
        primary: Arc<dyn LocalCache>,
        blobs: Arc<LocalCacheFS>,
        spill_threshold: usize,
    ) -> Self {
        Self {
            primary,
            blobs,
            spill_threshold,
        }
    }

    /// True for the manifest blob, which must always live in the primary.
    fn is_manifest(name: &str) -> bool {
        name == MANIFEST_V1
    }
}

impl LocalCache for LocalCacheSpill {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> Result<SizedReader> {
        match self.primary.reader(addr, hashin, name) {
            Err(e) if e.is::<NotFoundError>() => self.blobs.reader(addr, hashin, name),
            other => other,
        }
    }

    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn io::Write>> {
        // The manifest is the GC index — keep it in the primary unconditionally.
        if Self::is_manifest(name) {
            return self.primary.writer(addr, hashin, name);
        }
        // Stream to the primary by default; promote to FS on the first byte past
        // the threshold. Opening the primary writer up front means small blobs
        // (the common case) hit their final home with no buffering and no copy.
        let primary_writer = self
            .primary
            .writer(addr, hashin, name)
            .with_context(|| format!("open primary cache writer for {addr} {name}"))?;
        Ok(Box::new(SpillWriter {
            primary: self.primary.clone(),
            blobs: self.blobs.clone(),
            addr: addr.clone(),
            hashin: hashin.to_string(),
            name: name.to_string(),
            threshold: self.spill_threshold,
            size: 0,
            primary_writer: Some(primary_writer),
            blob_writer: None,
        }))
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        Ok(self.primary.exists(addr, hashin, name)? || self.blobs.exists(addr, hashin, name)?)
    }

    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> Result<()> {
        // A blob lives in exactly one backend, but the deleter (GC) doesn't know
        // which — both deletes are no-ops on the absent side. The FS delete also
        // prunes now-empty revision/target dirs.
        self.primary.delete(addr, hashin, name)?;
        self.blobs.delete(addr, hashin, name)?;
        Ok(())
    }

    fn list_targets(&self) -> Result<TargetStream> {
        // Manifests live in the primary, so its index covers every revision.
        self.primary.list_targets()
    }

    fn list_target_entries(&self, addr: &Addr) -> Result<Vec<String>> {
        self.primary.list_target_entries(addr)
    }

    fn seekable_reader(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
        match self.primary.seekable_reader(addr, hashin, name) {
            Err(e) if e.is::<NotFoundError>() => self.blobs.seekable_reader(addr, hashin, name),
            other => other,
        }
    }
}

/// Streams a blob to the primary, promoting it to the FS store the moment its
/// running size crosses `threshold`. Holds no buffer of its own: the in-flight
/// bytes live in whichever backend writer is currently open. Small blobs (never
/// cross the threshold) reach their final home in sqlite with zero copies; a
/// blob that does cross has its primary-staged prefix migrated to a fresh FS
/// writer once — and only large blobs pay that.
struct SpillWriter {
    primary: Arc<dyn LocalCache>,
    blobs: Arc<LocalCacheFS>,
    addr: Addr,
    hashin: String,
    name: String,
    threshold: usize,
    /// Bytes written so far, used only to detect the threshold crossing.
    size: usize,
    /// Open until the blob spills; `None` afterwards.
    primary_writer: Option<Box<dyn io::Write>>,
    /// `Some` once spilled; further writes stream directly into it.
    blob_writer: Option<Box<dyn io::Write>>,
}

impl SpillWriter {
    /// Promote the blob from the primary to the FS store: commit the staged
    /// prefix, copy it into a fresh FS writer, drop it from the primary, and
    /// retain the FS writer for the remaining bytes.
    fn spill(&mut self) -> io::Result<()> {
        // Commit the staged prefix to the primary so it can be read back. The
        // sqlite writer persists on drop; its PendingTracker makes the following
        // reader/delete wait for that write to land.
        let mut pw = self
            .primary_writer
            .take()
            .expect("spill called without an open primary writer");
        pw.flush()?;
        drop(pw);

        let mut blob_writer = self
            .blobs
            .writer(&self.addr, &self.hashin, &self.name)
            .map_err(io::Error::other)?;
        let mut prefix = self
            .primary
            .reader(&self.addr, &self.hashin, &self.name)
            .map_err(io::Error::other)?
            .reader;
        io::copy(&mut prefix, &mut blob_writer)?;
        drop(prefix);

        // The blob now lives in the FS store; drop the primary's staged copy so
        // a reader resolves to exactly one backend.
        self.primary
            .delete(&self.addr, &self.hashin, &self.name)
            .map_err(io::Error::other)?;

        self.blob_writer = Some(blob_writer);
        Ok(())
    }
}

impl io::Write for SpillWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // Already spilled: stream straight to the FS writer.
        if let Some(w) = self.blob_writer.as_mut() {
            let n = w.write(data)?;
            self.size += n;
            return Ok(n);
        }

        let w = self
            .primary_writer
            .as_mut()
            .expect("primary writer missing before spill");
        let n = w.write(data)?;
        self.size += n;
        if self.size > self.threshold {
            self.spill()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match (self.blob_writer.as_mut(), self.primary_writer.as_mut()) {
            (Some(w), _) => w.flush(),
            (None, Some(w)) => w.flush(),
            (None, None) => Ok(()),
        }
    }
}

impl Drop for SpillWriter {
    fn drop(&mut self) {
        // Whichever backend writer is open owns the bytes; flushing + dropping it
        // persists the blob (the sqlite writer commits on drop, the FS file is
        // already on disk). Nothing is staged in `self`, so there is no
        // finalize-time write that could fail here.
        if let Some(mut w) = self.blob_writer.take() {
            drop(w.flush());
        } else if let Some(mut w) = self.primary_writer.take() {
            drop(w.flush());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::local_cache_sqlite::{DEFAULT_MAX_CONCURRENT_PIPES, LocalCacheSQLite};
    use std::io::{Read, Write};
    use tempfile::tempdir;

    fn addr() -> Addr {
        Addr::new(
            crate::htpkg::PkgBuf::from("pkg"),
            "t".to_string(),
            Default::default(),
        )
    }

    /// Build a spill cache over a real sqlite primary + fs blob store, with a
    /// small threshold so tests can exercise both routes cheaply. Returns the
    /// spill plus the raw backends so tests can assert *where* a blob landed.
    fn spill(
        dir: &std::path::Path,
        threshold: usize,
    ) -> (LocalCacheSpill, Arc<LocalCacheSQLite>, Arc<LocalCacheFS>) {
        let sqlite = Arc::new(
            LocalCacheSQLite::with_pipe_limit(
                dir.join("cache.db"),
                16 * 1024,
                DEFAULT_MAX_CONCURRENT_PIPES,
            )
            .expect("sqlite"),
        );
        let fs = Arc::new(LocalCacheFS::new(dir.join("blobs")).expect("fs"));
        (
            LocalCacheSpill::new(sqlite.clone(), fs.clone(), threshold),
            sqlite,
            fs,
        )
    }

    fn write(cache: &dyn LocalCache, a: &Addr, name: &str, data: &[u8]) {
        let mut w = cache.writer(a, "h", name).expect("writer");
        w.write_all(data).expect("write");
        drop(w);
    }

    fn read(cache: &dyn LocalCache, a: &Addr, name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        cache
            .reader(a, "h", name)
            .expect("reader")
            .reader
            .read_to_end(&mut out)
            .expect("read");
        out
    }

    /// Small blobs land in the primary (sqlite); large blobs land in the FS blob
    /// store. Both read back identically through the spill, regardless of route.
    #[test]
    fn routes_small_to_primary_large_to_fs() {
        let dir = tempdir().expect("tempdir");
        let (cache, sqlite, fs) = spill(dir.path(), 64);
        let a = addr();

        let small = vec![1u8; 32]; // <= threshold
        let large = vec![2u8; 256]; // > threshold
        write(&cache, &a, "small", &small);
        write(&cache, &a, "large", &large);

        // Routing: each blob lives in exactly one backend.
        assert!(sqlite.exists(&a, "h", "small").expect("ex"));
        assert!(!fs.exists(&a, "h", "small").expect("ex"));
        assert!(fs.exists(&a, "h", "large").expect("ex"));
        assert!(!sqlite.exists(&a, "h", "large").expect("ex"));

        // Round-trip through the spill is correct for both.
        assert_eq!(read(&cache, &a, "small"), small);
        assert_eq!(read(&cache, &a, "large"), large);
        assert!(cache.exists(&a, "h", "small").expect("ex"));
        assert!(cache.exists(&a, "h", "large").expect("ex"));
    }

    /// A blob written across many small chunks that *cumulatively* exceed the
    /// threshold must spill — the decision is on running size, not per-write.
    #[test]
    fn spills_on_cumulative_size_across_chunks() {
        let dir = tempdir().expect("tempdir");
        let (cache, sqlite, fs) = spill(dir.path(), 100);
        let a = addr();

        let mut w = cache.writer(&a, "h", "blob").expect("writer");
        for _ in 0..20 {
            w.write_all(&[7u8; 10]).expect("chunk"); // 200 bytes total
        }
        drop(w);

        assert!(
            fs.exists(&a, "h", "blob").expect("ex"),
            "should have spilled"
        );
        assert!(!sqlite.exists(&a, "h", "blob").expect("ex"));
        assert_eq!(read(&cache, &a, "blob"), vec![7u8; 200]);
    }

    /// The manifest always lands in the primary even when it exceeds the spill
    /// threshold — it's the GC index and must stay enumerable there.
    #[test]
    fn manifest_always_in_primary_even_when_large() {
        let dir = tempdir().expect("tempdir");
        let (cache, sqlite, fs) = spill(dir.path(), 16);
        let a = addr();

        let big_manifest = vec![9u8; 1024]; // far over threshold
        write(&cache, &a, MANIFEST_V1, &big_manifest);

        assert!(sqlite.exists(&a, "h", MANIFEST_V1).expect("ex"));
        assert!(!fs.exists(&a, "h", MANIFEST_V1).expect("ex"));
        assert_eq!(read(&cache, &a, MANIFEST_V1), big_manifest);
    }

    /// `delete` reclaims a blob no matter which backend holds it, so GC (which
    /// doesn't know the route) reclaims FS-spilled blobs too.
    #[test]
    fn delete_reclaims_from_both_backends() {
        let dir = tempdir().expect("tempdir");
        let (cache, _sqlite, fs) = spill(dir.path(), 64);
        let a = addr();

        write(&cache, &a, "small", &[1u8; 16]);
        write(&cache, &a, "large", &[2u8; 256]);

        cache.delete(&a, "h", "small").expect("del small");
        cache.delete(&a, "h", "large").expect("del large");

        assert!(!cache.exists(&a, "h", "small").expect("ex"));
        assert!(!cache.exists(&a, "h", "large").expect("ex"));
        // FS revision dir pruned once its last blob is gone.
        assert!(!fs.exists(&a, "h", "large").expect("ex"));
    }
}
