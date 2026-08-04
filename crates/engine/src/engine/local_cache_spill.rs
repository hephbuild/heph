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
    EntryWriter, Existence, LocalCache, MANIFEST_V1, NotFoundError, SizedReader, TargetStream,
};
use crate::engine::local_cache_fs::LocalCacheFS;
use anyhow::{Context, Result};
use hcore::hartifactcontent;
use hmodel::htaddr::Addr;
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

    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Box<dyn EntryWriter>> {
        // The manifest is the GC index — keep it in the primary unconditionally.
        if Self::is_manifest(name) {
            return self.primary.writer(addr, hashin, name);
        }
        // Drop any stale FS copy before writing, so this key ends up in exactly
        // one backend — the invariant the module doc asserts and every read path
        // relies on. It is not self-maintaining: `spill` deletes the primary's
        // staged prefix when a blob is promoted, but nothing deleted the FS copy
        // when a rewrite of the same key stays *under* the threshold, which
        // `cache.spillThresholdBytes` being user-tunable makes reachable. Dual
        // residency would make `reader` (primary first) and `file_path` (FS only)
        // resolve to different bytes for one key. Cheap and on the cold path: one
        // `stat` per blob write, and an unlink only in the rare stale case.
        if self.blobs.file_path(addr, hashin, name).is_some() {
            self.blobs
                .delete(addr, hashin, name)
                .with_context(|| format!("drop stale spilled blob for {addr} {name}"))?;
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

    /// Adopt only what would have spilled anyway.
    ///
    /// The primary is sqlite — there is no file to rename into — so an adoption
    /// can only land in the FS store, and routing a *small* artifact there
    /// because it happened to arrive as a file would put it in a different
    /// backend than the identical artifact packed from memory. Size decides,
    /// against the same threshold and the same comparison [`SpillWriter`] uses,
    /// so a blob's home does not depend on which way it was written.
    fn adopt_file(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
        src: &std::path::Path,
    ) -> Result<bool> {
        // The manifest is the GC index and always lives in the primary.
        if Self::is_manifest(name) {
            return Ok(false);
        }
        let size = std::fs::metadata(src)
            .with_context(|| format!("stat {src:?} for adoption into {addr} {name}"))?
            .len();
        if size <= self.spill_threshold as u64 {
            return Ok(false);
        }
        if !self.blobs.adopt_file(addr, hashin, name, src)? {
            return Ok(false);
        }
        // The key now lives in the FS store, so drop anything the primary still
        // holds for it — the same "exactly one backend" invariant `writer`
        // maintains from the other side, seen from this one. The probe runs only
        // for a genuinely large artifact, and finds nothing for the fresh
        // revision that is the normal case.
        if self
            .primary
            .exists_committed(addr, hashin, name)
            .with_context(|| format!("probe primary for superseded blob {addr} {name}"))?
        {
            self.primary
                .delete(addr, hashin, name)
                .with_context(|| format!("drop stale primary blob for {addr} {name}"))?;
        }
        Ok(true)
    }

    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        Ok(self.primary.exists(addr, hashin, name)? || self.blobs.exists(addr, hashin, name)?)
    }

    fn existence(&self, addr: &Addr, hashin: &str, name: &str) -> Result<Existence> {
        // A queue reported by the primary settles it: the blob is on its way in
        // and neither backend has a committed answer yet. Otherwise fall back to
        // the two-sided probe — `blobs` is an FS cache and commits inline, so it
        // never queues.
        match self.primary.existence(addr, hashin, name)? {
            queued @ Existence::Queued(_) => Ok(queued),
            Existence::Committed(true) => Ok(Existence::Committed(true)),
            Existence::Committed(false) => self.blobs.existence(addr, hashin, name),
        }
    }

    fn exists_committed(&self, addr: &Addr, hashin: &str, name: &str) -> Result<bool> {
        // Mirrors `exists`: a blob lives in exactly one backend and the caller
        // does not know which, so both are asked. Neither wait on a queue.
        Ok(self.primary.exists_committed(addr, hashin, name)?
            || self.blobs.exists_committed(addr, hashin, name)?)
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

    /// Only spilled blobs have a file: the primary is sqlite, whose blobs live in
    /// rows. Ask the FS store first — it answers `Some` only for a path that
    /// exists, and for anything under the threshold (nearly every artifact) that
    /// single `stat` ends it.
    ///
    /// **Then confirm the primary does not also hold the key**, so this agrees
    /// with [`reader`](Self::reader), which tries the primary *first*. The two
    /// would otherwise resolve to different bytes for one key whenever both
    /// backends hold it, and this method has no error to report it with — the
    /// caller would just silently open the wrong blob.
    ///
    /// [`writer`](Self::writer) stops that state being *created*, but cannot
    /// repair a cache that already has it: `spillThresholdBytes` is user-tunable
    /// and nothing swept a still-live revision, so a directory written before
    /// that fix can carry a superseded FS blob indefinitely — and GC only
    /// reclaims trimmed or orphaned revisions, never audits a live one. Deferring
    /// to the primary here makes the read side correct on those caches too,
    /// without a migration.
    ///
    /// The probe costs nothing in the common case: it runs *only* when an FS blob
    /// exists, which for a healthy cache means a genuinely spilled (large)
    /// artifact, not the per-call sqlite query that asking primary-first would be.
    fn file_path(&self, addr: &Addr, hashin: &str, name: &str) -> Option<std::path::PathBuf> {
        let path = self.blobs.file_path(addr, hashin, name)?;
        match self.primary.exists_committed(addr, hashin, name) {
            // Sole resident: the ordinary spilled-blob case.
            Ok(false) => Some(path),
            // Both hold it. `reader` serves the primary, so this must not offer
            // the FS copy; no direct-open beats a fast route to the wrong bytes.
            Ok(true) => {
                tracing::debug!(
                    %addr, hashin, name,
                    "cache key in both spill backends; superseded blob left for the next write"
                );
                None
            }
            // Cannot establish it is the only copy — decline rather than guess.
            Err(e) => {
                tracing::debug!(%addr, hashin, name, error = %e, "probe primary for spilled blob");
                None
            }
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
    primary_writer: Option<Box<dyn EntryWriter>>,
    /// `Some` once spilled; further writes stream directly into it.
    blob_writer: Option<Box<dyn EntryWriter>>,
}

impl SpillWriter {
    /// Promote the blob from the primary to the FS store: commit the staged
    /// prefix, copy it into a fresh FS writer, drop it from the primary, and
    /// retain the FS writer for the remaining bytes.
    fn spill(&mut self) -> io::Result<()> {
        // Commit the staged prefix to the primary so it can be read back — the
        // one mid-stream commit in the protocol, and it is deleted again below
        // once copied. The sqlite writer's PendingTracker makes the following
        // reader/delete wait for that write to land.
        let mut pw = self
            .primary_writer
            .take()
            .expect("spill called without an open primary writer");
        pw.flush()?;
        pw.commit().map_err(io::Error::other)?;

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
        htelemetry::telemetry::record_cache_spill();
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

impl EntryWriter for SpillWriter {
    fn commit(mut self: Box<Self>) -> Result<()> {
        // Whichever backend writer is open owns the bytes; committing it makes
        // the blob durable in its final home. Nothing is staged in `self`.
        // Dropped without commit, that same writer discards its staging — an
        // abandoned blob lands in neither backend.
        let (w, backend) = match (self.blob_writer.take(), self.primary_writer.take()) {
            (Some(w), _) => (w, "spilled"),
            (None, Some(w)) => (w, "primary"),
            // `spill` always leaves exactly one backend writer open, and commit
            // consumes `self` — so this is unreachable short of a logic bug.
            (None, None) => anyhow::bail!(
                "spill writer for {} {} has no open backend writer to commit",
                self.addr,
                self.name
            ),
        };
        w.commit()
            .with_context(|| format!("commit {backend} blob {} for {}", self.name, self.addr))
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
            hmodel::htpkg::PkgBuf::from("pkg"),
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
        w.commit().expect("commit");
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

    /// Adoption follows the same routing as `writer`, on the same threshold: a
    /// file large enough to spill is taken by the FS store (and is gone from
    /// where it was), a small one is declined so the caller copies it into
    /// sqlite. A blob's home must not depend on which way it was written.
    #[test]
    fn adopt_routes_on_the_same_threshold_as_writer() {
        let dir = tempdir().expect("tempdir");
        let (cache, sqlite, fs) = spill(dir.path(), 64);
        let a = addr();

        let small_src = dir.path().join("small.tar");
        let large_src = dir.path().join("large.tar");
        std::fs::write(&small_src, vec![1u8; 32]).expect("write small");
        std::fs::write(&large_src, vec![2u8; 256]).expect("write large");

        // Under the threshold: declined, and the file is left for the caller.
        assert!(
            !cache
                .adopt_file(&a, "h", "small", &small_src)
                .expect("adopt small"),
            "a blob that belongs in sqlite must not be adopted onto the fs"
        );
        assert!(small_src.exists(), "a declined adoption leaves the source");

        // Over it: taken, into the same backend `writer` would have spilled to.
        assert!(
            cache
                .adopt_file(&a, "h", "large", &large_src)
                .expect("adopt large"),
            "a blob over the threshold is the fs store's to take"
        );
        assert!(!large_src.exists(), "an adopted file is moved, not copied");
        assert!(fs.exists(&a, "h", "large").expect("ex"));
        assert!(!sqlite.exists(&a, "h", "large").expect("ex"));
        assert_eq!(read(&cache, &a, "large"), vec![2u8; 256]);
    }

    /// A rewrite by adoption leaves the key in exactly one backend, the same
    /// invariant `writer` maintains from the other side: the earlier copy in the
    /// primary has to go, or `reader` (primary first) and `file_path` (fs only)
    /// answer with different bytes for one key.
    #[test]
    fn adopt_drops_a_superseded_primary_copy() {
        let dir = tempdir().expect("tempdir");
        let (cache, sqlite, fs) = spill(dir.path(), 64);
        let a = addr();

        // Landed in sqlite under a smaller threshold / an earlier, smaller build.
        write(&cache, &a, "blob", &[9u8; 16]);
        assert!(sqlite.exists(&a, "h", "blob").expect("ex"));

        let src = dir.path().join("blob.tar");
        std::fs::write(&src, vec![7u8; 256]).expect("write");
        assert!(cache.adopt_file(&a, "h", "blob", &src).expect("adopt"));

        assert!(fs.exists(&a, "h", "blob").expect("ex"));
        assert!(
            !sqlite.exists(&a, "h", "blob").expect("ex"),
            "the superseded primary copy must be dropped"
        );
        assert_eq!(read(&cache, &a, "blob"), vec![7u8; 256]);
        assert!(cache.file_path(&a, "h", "blob").is_some());
    }

    /// The manifest is the GC index and lives in the primary unconditionally —
    /// size does not enter into it, so it is never adopted onto the filesystem.
    #[test]
    fn adopt_never_takes_the_manifest() {
        let dir = tempdir().expect("tempdir");
        let (cache, _sqlite, _fs) = spill(dir.path(), 64);
        let a = addr();

        let src = dir.path().join("manifest.bin");
        std::fs::write(&src, vec![3u8; 256]).expect("write");

        assert!(
            !cache.adopt_file(&a, "h", MANIFEST_V1, &src).expect("adopt"),
            "the manifest must stay in the primary whatever its size"
        );
        assert!(src.exists());
    }

    /// The direct-open fast path tracks the routing: a spilled blob is a real
    /// file and names it, a primary-resident one is a sqlite row and has no
    /// path. Getting the second case wrong is worse than having no fast path at
    /// all — a consumer opens what it is handed and never falls back to the
    /// stream.
    #[test]
    fn file_path_answers_for_spilled_blobs_only() {
        let dir = tempdir().expect("tempdir");
        let (cache, _sqlite, _fs) = spill(dir.path(), 64);
        let a = addr();

        let large = vec![2u8; 256];
        write(&cache, &a, "small", &[1u8; 32]);
        write(&cache, &a, "large", &large);

        assert!(
            cache.file_path(&a, "h", "small").is_none(),
            "sqlite-resident blob has no file"
        );
        let path = cache
            .file_path(&a, "h", "large")
            .expect("spilled blob is a real file");
        assert_eq!(std::fs::read(&path).expect("read"), large);

        // The manifest always stays in the primary, however big.
        write(&cache, &a, MANIFEST_V1, &[9u8; 256]);
        assert!(
            cache.file_path(&a, "h", MANIFEST_V1).is_none(),
            "manifest never spills, so it never has a path"
        );
    }

    /// A cache that *already* carries a superseded spilled blob must not serve it
    /// through the direct-open path.
    ///
    /// `writer` stops this state being created, but cannot repair a directory
    /// written before that fix, and GC never audits a still-live revision — so
    /// the read side has to be correct on its own or those caches silently hand
    /// out the wrong bytes. Both backends are written directly here, bypassing
    /// `writer`, because that is exactly the state an older heph could leave.
    ///
    /// `reader` resolves primary-first, so the primary is the answer and
    /// `file_path` must decline rather than offer a faster route to the stale
    /// copy.
    #[test]
    fn file_path_declines_when_the_primary_also_holds_the_key() {
        let dir = tempdir().expect("tempdir");
        let (cache, sqlite, fs) = spill(dir.path(), 64);
        let a = addr();

        // Pre-existing dual residency: superseded blob on disk, live row in sqlite.
        write(&*fs, &a, "out", &[9u8; 256]);
        write(&*sqlite, &a, "out", &[4u8; 32]);
        assert!(fs.exists(&a, "h", "out").expect("ex"));
        assert!(sqlite.exists(&a, "h", "out").expect("ex"));

        assert_eq!(
            read(&cache, &a, "out"),
            vec![4u8; 32],
            "reader resolves primary-first"
        );
        assert!(
            cache.file_path(&a, "h", "out").is_none(),
            "direct open must not disagree with reader"
        );
    }

    /// A rewrite that lands under the threshold must not leave the previous
    /// spilled copy behind. `spill` deletes the primary's staged prefix when a
    /// blob is promoted, but the reverse — a key that used to spill and no longer
    /// does — had no cleanup, and `cache.spillThresholdBytes` is user-tunable, so
    /// lowering it and raising it back is enough to create it. Dual residency
    /// makes `reader` (primary first) and `file_path` (FS only) resolve to
    /// *different bytes for one key*: the read path would serve the new blob
    /// while a plugin opening the path got the old one.
    #[test]
    fn a_rewrite_below_the_threshold_drops_the_stale_spilled_copy() {
        let dir = tempdir().expect("tempdir");
        let a = addr();

        // Threshold 64: the 256-byte blob spills to the FS store.
        let (low, _sqlite, fs) = spill(dir.path(), 64);
        write(&low, &a, "out", &[1u8; 256]);
        assert!(
            fs.exists(&a, "h", "out").expect("ex"),
            "precondition: spilled"
        );

        // Threshold raised; the same key is rewritten and now stays in sqlite.
        let (high, sqlite2, fs2) = spill(dir.path(), 4096);
        write(&high, &a, "out", &[2u8; 256]);

        assert!(
            sqlite2.exists(&a, "h", "out").expect("ex"),
            "the rewrite belongs in the primary now"
        );
        assert!(
            !fs2.exists(&a, "h", "out").expect("ex"),
            "the stale spilled copy must be gone, not shadowed"
        );
        // The two read routes must not disagree.
        assert_eq!(read(&high, &a, "out"), vec![2u8; 256]);
        assert!(
            high.file_path(&a, "h", "out").is_none(),
            "file_path must not hand out the superseded blob"
        );
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
        w.commit().expect("commit");

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
