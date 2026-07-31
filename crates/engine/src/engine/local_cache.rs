use crate::engine::Engine;
use crate::engine::driver::outputartifact;
use crate::engine::link::LinkedTargetDef;
use crate::engine::result::ArtifactMeta;
use anyhow::Context;
use borsh::{BorshDeserialize, BorshSerialize};
use chrono::Utc;
use enclose::enclose;
use hcore::hartifactcontent;
use hcore::hasync::Cancellable;
use hmodel::htaddr::Addr;
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::{fmt, io, time};

struct CountingWriter<W: io::Write> {
    inner: W,
    count: u64,
}

impl<W: io::Write> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, count: 0 }
    }

    fn bytes_written(&self) -> u64 {
        self.count
    }

    /// Hand back the wrapped writer, so a caller that finished a pack can
    /// [`EntryWriter::commit`] it.
    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: io::Write> io::Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq)]
pub enum ManifestArtifactContentType {
    Tar,
    Cpio,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq)]
pub enum ManifestArtifactEncoding {
    None,
    Gzip,
    Zstd,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq)]
pub enum ManifestArtifactType {
    Output,
    Log,
    SupportFile,
}

/// Whether the caller already knows every needed blob is in the local cache.
///
/// The per-blob probe in [`Engine::artifacts_from_manifest`] is not free: a pooled
/// sqlite connection and one point lookup per needed artifact, on the hot path of
/// every cache hit. On the remote path it also has nothing to learn — the pull
/// just walked the same set.
///
/// Note this is a cost, not a hazard. Probing a key with a write still queued used
/// to park the calling thread on the writer thread's next batch commit, and that
/// probe runs on a tokio worker; [`Engine::exists_local`] now awaits the queue
/// instead, so skipping the probe saves lookups rather than rescuing the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobResidency {
    /// Nothing has looked yet — probe each needed blob, and degrade the hit to a
    /// miss if one is gone. The state of a read that did not go through
    /// `Engine::materialize_blobs`, where the manifest may well outlive the blobs
    /// a GC reclaimed.
    Unknown,
    /// Every needed blob was just confirmed present or pulled, by
    /// `Engine::materialize_blobs` over exactly this artifact set
    /// ([`Engine::needed_artifacts`] is shared by both paths). Re-probing has
    /// nothing left to learn, and skipping it does not skip the wait so much as
    /// move it to whoever actually reads the bytes — `reader` does its own
    /// `wait_if_pending`. A caller that only needed the hashouts, the common case
    /// in a fully-cached build, then never waits at all.
    Established,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct ManifestArtifact {
    pub hashout: String,
    pub group: String,
    pub name: String,
    pub size: u64,
    pub r#type: ManifestArtifactType,
    pub content_type: ManifestArtifactContentType,
    pub encoding: ManifestArtifactEncoding,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct Manifest {
    pub version: String,
    pub target: String,
    pub created_at_nanos: i64,
    pub hashin: String,
    pub artifacts: Vec<ManifestArtifact>,
}

pub struct SizedReader {
    pub size: u64,
    pub reader: Box<dyn io::Read>,
    /// Set when `reader` is already backed by an in-memory buffer. Lets a
    /// caching layer skip the drain step and store the buffer directly.
    pub bytes: Option<Arc<[u8]>>,
}

/// Streaming iterator of target address keys. Boxed and `Send` so it can be held
/// across `.await` points by GC; `'static` because backends stream from an owned
/// connection/snapshot, not a borrow of the cache.
pub type TargetStream = Box<dyn Iterator<Item = anyhow::Result<String>> + Send>;

/// Resolves once a queued-but-uncommitted cache write has landed.
///
/// Only a backend with a write-behind queue produces one. The sqlite backend's
/// `writer` *queues* onto a single writer thread rather than committing, so a key
/// written moments ago is not yet readable, and `exists`/`reader` cover the gap by
/// parking the calling thread on an untimed condvar. That is the right thing on an
/// OS thread and ruinous on a tokio worker: `n` concurrent readers park `n`
/// workers on one batch commit, and the reactor, the timer wheel, every in-flight
/// remote transfer and the TUI stop with them — nothing deadlocked, the build
/// merely looks hung. Awaiting this suspends the *task* and leaves the worker
/// polling.
pub struct PendingWrite(Pin<Box<dyn Future<Output = ()> + Send>>);

impl PendingWrite {
    /// Wrap a backend's completion future. Boxed because the trait is used as
    /// `dyn LocalCache`; the allocation is only paid on the rare path where a
    /// write to the probed key is actually in flight.
    pub fn new(fut: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(Box::pin(fut))
    }
}

impl Future for PendingWrite {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        self.0.as_mut().poll(cx)
    }
}

impl fmt::Debug for PendingWrite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingWrite(..)")
    }
}

/// How many times [`Engine::exists_local`] will wait out a queued write for one
/// key before answering from committed state.
///
/// Enough for a key genuinely rewritten while it is being probed; far below what a
/// misbehaving backend would need to burn a worker on a ready-`Queued` spin.
const MAX_QUEUE_WAITS: usize = 8;

/// The answer to a presence check that refuses to wait for anything.
#[derive(Debug)]
pub enum Existence {
    /// Answered from the committed state of the cache.
    Committed(bool),
    /// A write to this key is queued but not committed, so there is no settled
    /// answer yet. Wait on the handle — `.await` from a task, and see
    /// [`PendingWrite`] for why that matters — then ask again.
    Queued(PendingWrite),
}

/// A cache-entry writer. The entry becomes durable only on [`commit`](Self::commit);
/// dropping without commit discards everything written. This is what keeps a
/// mid-stream failure or an abandoned attempt from ever surfacing as (or
/// replacing!) a readable entry: the blocking pool runs jobs to completion even
/// when their awaiting future is dropped, so "the caller stopped" must never
/// imply "the bytes landed".
pub trait EntryWriter: io::Write + Send {
    /// Make the entry durable. Consumes the writer; errors are the write's.
    fn commit(self: Box<Self>) -> anyhow::Result<()>;
}

pub trait LocalCache: Send + Sync {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<SizedReader>;
    fn writer(&self, addr: &Addr, hashin: &str, name: &str)
    -> anyhow::Result<Box<dyn EntryWriter>>;
    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool>;
    /// [`exists`](Self::exists), minus the wait on the backend's write-behind
    /// queue — it reports the queue instead of blocking on it, so it is safe to
    /// call from a tokio worker. [`Engine::exists_local`] is the async wrapper
    /// that drives it to an answer.
    ///
    /// **Required, deliberately.** A default of
    /// `Ok(Existence::Committed(self.exists(..)?))` is correct only for a backend
    /// that commits inline, and wrong in the worst way for one that queues: it
    /// silently reinstates the worker-parking this method exists to remove, with
    /// nothing to see. A decorator that forwards `exists` and forgets this would
    /// inherit that default and re-park the runtime. Making it required means the
    /// compiler asks every impl — including test doubles — which one it is.
    ///
    /// An inline backend answers `Ok(Existence::Committed(self.exists(..)?))`
    /// explicitly; a decorator forwards to the layer that owns the queue.
    fn existence(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Existence>;
    /// [`exists`](Self::exists) answered from the *committed* state alone, with
    /// no regard for the write-behind queue and no wait on it.
    ///
    /// The answer [`existence`](Self::existence) gives as
    /// `Existence::Committed`, available on its own. [`Engine::exists_local`]
    /// needs it for the one case `existence` cannot serve: having spent its
    /// retry budget on a key that keeps reporting `Queued`, it has to answer
    /// *something*, and the only non-waiting answer is the committed one.
    /// Falling back to `exists` there re-parked the caller on the very condvar
    /// the loop exists to avoid.
    ///
    /// **Required, for the same reason as `existence`.** A default of
    /// `self.exists(..)` is right for a backend that commits inline and silently
    /// reinstates the park for one that queues. A decorator must mirror its own
    /// `exists` — including any tier-local short-circuit, since an entry this
    /// layer can serve is committed as far as any caller is concerned — rather
    /// than delegate blind.
    fn exists_committed(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool>;
    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<()>;
    /// Stream the distinct target address keys (`Addr::format()`, parseable via
    /// `htaddr::parse_addr`) present in the cache. Streamed rather than collected
    /// because the target count can be very large; GC processes one at a time so
    /// the full set never has to live in memory. Defaults to empty so
    /// lightweight/test backends need not implement it.
    fn list_targets(&self) -> anyhow::Result<TargetStream> {
        Ok(Box::new(std::iter::empty()))
    }
    /// The distinct cache revisions (input hashes) for a single target. Bounded
    /// per target, so returning a `Vec` is fine. Defaults to empty.
    fn list_target_entries(&self, _addr: &Addr) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Returns a seekable reader when the cache backend supports `O(1)`
    /// pread (sqlite blob, on-disk file). Defaults to `Ok(None)` so backends
    /// can opt in. Used by the FUSE sandbox path to index and read tar
    /// artifacts without copying their bytes to disk first.
    fn seekable_reader(
        &self,
        _addr: &Addr,
        _hashin: &str,
        _name: &str,
    ) -> anyhow::Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
        Ok(None)
    }
    /// The on-disk path of a cached artifact when the backend is a real
    /// filesystem (so an in-process consumer can open it directly). `None` for
    /// non-file backends. Defaults to `None`.
    fn file_path(&self, _addr: &Addr, _hashin: &str, _name: &str) -> Option<std::path::PathBuf> {
        None
    }
}

#[derive(Debug, thiserror::Error)]
#[error("not found")]
pub struct NotFoundError;

/// Name of a revision's manifest blob.
///
/// The manifest records a revision's *identity* — its target, `hashin`, and every
/// artifact's `hashout`/size/type — not which of its blobs are resident. A
/// revision written by [`cache_locally`](Engine::cache_locally) does have all of
/// them (blobs first, manifest last); one mirrored from a remote
/// ([`probe_remote_revision`](Engine::probe_remote_revision)) starts with none of
/// them, and materializes per caller. Read paths therefore check residency
/// explicitly — see [`missing_local_blobs`](Engine::missing_local_blobs).
pub(crate) const MANIFEST_V1: &str = "manifest-v1.borsh";

#[derive(Clone)]
pub struct CacheArtifact {
    pub addr: Addr,
    pub hashin: String,
    pub name: String,
    pub cache: Arc<dyn LocalCache>,
    pub content_type: hartifactcontent::Type,
    pub hashout: String,
    pub group: String,
    pub r#type: ManifestArtifactType,
    /// Stored byte size from the manifest. Used by the engine auto-mode
    /// router to size FUSE vs unpack-copy decisions cheaply.
    pub size: u64,
}

impl hartifactcontent::Content for CacheArtifact {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>> {
        Ok(self
            .cache
            .reader(&self.addr, &self.hashin, &self.name)?
            .reader)
    }

    fn walk(
        &self,
    ) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<hartifactcontent::WalkEntry>> + '_>>
    {
        Ok(match &self.content_type {
            hartifactcontent::Type::Tar => Box::new(hcore::hartifactcontent::tar::TarWalker::new(
                self.reader()?,
            )?),
            #[expect(clippy::unimplemented, reason = "cpio format is not yet implemented")]
            hartifactcontent::Type::Cpio => unimplemented!("cpio is not implemented"),
        })
    }

    fn hashout(&self) -> anyhow::Result<String> {
        Ok(self.hashout.clone())
    }

    fn entry_paths(&self) -> anyhow::Result<Vec<std::path::PathBuf>> {
        match &self.content_type {
            // Header-only: index the tar over the seekable reader (seeks past
            // file data) instead of walking and reading every byte.
            hartifactcontent::Type::Tar => {
                let reader = self.seekable_reader()?.ok_or_else(|| {
                    anyhow::anyhow!("tar cache artifact {} has no seekable reader", self.name)
                })?;
                Ok(hcore::hartifactcontent::tar_index::TarIndex::build(reader)?.entry_paths())
            }
            // No index for cpio yet — fall back to the walk-based default.
            hartifactcontent::Type::Cpio => self.walk()?.map(|r| r.map(|e| e.path)).collect(),
        }
    }

    fn seekable_reader(
        &self,
    ) -> anyhow::Result<Option<Box<dyn hartifactcontent::ReadSeek + Send>>> {
        self.cache
            .seekable_reader(&self.addr, &self.hashin, &self.name)
    }

    fn byte_size(&self) -> Option<u64> {
        Some(self.size)
    }

    fn file_path(&self) -> Option<std::path::PathBuf> {
        self.cache.file_path(&self.addr, &self.hashin, &self.name)
    }
}

/// Whether a caller asking for `outputs` reads this revision's support files.
///
/// Support files materialize into a sandbox *alongside* an output group, so a
/// caller that requested none opens none of them — that is the hashout-only path,
/// which reads no bytes at all.
///
/// The exception is a revision with no Output artifact whatsoever: there, an
/// empty `outputs` is not "I want nothing", it is "there was nothing to ask
/// for" (`OutputMatcher::All` over a target that declares no output group
/// resolves to the same empty vec). Its support files are the only thing it
/// has, so they stay needed — a runtime dep on such a target still gets them
/// staged.
///
/// The rule itself is [`support_files_needed_for`], so the cached read path and
/// the freshly-executed path in `build_eresult` cannot drift apart — a caller
/// must be handed the same support files whether the target hit or built.
fn support_files_needed(manifest: &Manifest, outputs: &[String]) -> bool {
    let has_output = manifest
        .artifacts
        .iter()
        .any(|a| a.r#type == ManifestArtifactType::Output);
    support_files_needed_for(has_output, outputs)
}

/// [`support_files_needed`] over an artifact set that is not a manifest —
/// `has_output_artifact` is "does this revision have any Output at all".
pub(crate) fn support_files_needed_for(has_output_artifact: bool, outputs: &[String]) -> bool {
    !outputs.is_empty() || !has_output_artifact
}

/// Whether one manifest artifact is a blob a caller asking for `outputs` will
/// actually read — the single predicate behind [`Engine::needed_artifacts`] and
/// the per-artifact gate in [`Engine::artifacts_from_manifest`], so residency is
/// never decided against a different set than the one that gets read.
///
/// `support_needed` comes from [`support_files_needed`]; it is passed in rather
/// than recomputed so a per-artifact loop stays linear.
fn artifact_is_needed(a: &ManifestArtifact, outputs: &[String], support_needed: bool) -> bool {
    match a.r#type {
        ManifestArtifactType::Output => outputs.contains(&a.group),
        ManifestArtifactType::SupportFile => support_needed,
        ManifestArtifactType::Log => false,
    }
}

/// Concurrent local pack jobs (tar/copy of one artifact into the cache).
///
/// Same convention as `CODEC_SLOTS` (remote gzip) and `PKG_EVAL_SLOTS`
/// (Starlark eval): a class of hundreds-of-ms jobs on the shared, arrival-fair
/// `hcore::blocking` pool caps itself at the core count so it cannot fill every
/// pool thread and put the sub-millisecond jobs (warm-hit manifest reads) behind
/// a queue of long ones. Before `cache_locally` fanned artifacts out this class
/// was implicitly bounded at one job per running target; the fan-out multiplies
/// that by artifacts-per-target, so the bound has to be explicit. It also caps
/// concurrent spool memory, which is allocated inside the job.
static LOCAL_PACK_SLOTS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(local_pack_slots()));

fn local_pack_slots() -> usize {
    std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(8)
}

/// The cache entry name an artifact's blob is stored under. One place, used by
/// both the write arms and `cache_locally`'s duplicate check, so the two cannot
/// drift.
fn cache_entry_name(artifact: &outputartifact::OutputArtifact) -> String {
    let type_prefix = match artifact.r#type {
        outputartifact::Type::Output => "out",
        outputartifact::Type::Log => "log",
        outputartifact::Type::SupportFile => "support",
    };
    match &artifact.content {
        // Packed on the way in, so the entry carries the container suffix.
        outputartifact::Content::Raw(_) | outputartifact::Content::File(_) => {
            format!("{}_{}.tar", type_prefix, artifact.name)
        }
        // Already a container on disk; copied verbatim.
        outputartifact::Content::TarPath(_) | outputartifact::Content::CpioPath(_) => {
            format!("{}_{}", type_prefix, artifact.name)
        }
    }
}

impl Engine {
    pub async fn cache_artifact_locally(
        &self,
        _ctoken: &dyn Cancellable,
        cache: &Arc<dyn LocalCache>,
        addr: &Addr,
        hashin: &str,
        artifact: &outputartifact::OutputArtifact,
    ) -> anyhow::Result<(CacheArtifact, ManifestArtifact)> {
        let hashin = hashin.to_string();
        // Waited for in async-land, before queueing — parking a pool thread to
        // wait for a pool thread is the deadlock. The permit rides *into* the
        // job (released on the pool thread) so a caller that stops being polled
        // cannot strand it; same discipline as `PKG_EVAL_SLOTS`.
        let slot = LOCAL_PACK_SLOTS
            .acquire()
            .await
            .context("acquiring a local pack slot")?;
        // Writing a revision tars and copies every output — the heaviest
        // synchronous work in a build, once per target. It runs on the dedicated
        // blocking pool: not inline (that parks a runtime worker with the runtime
        // unaware, and enough concurrent writes stop the reactor entirely) and not
        // `spawn_blocking` (whose JoinHandle wake-up rides tokio's cross-thread
        // waker, observed to drop wakeups on macOS under load — see
        // the macOS waker hazard in `hproc::proc_exec`). See `hcore::blocking`.
        hcore::blocking::run(enclose!((cache => local_cache, addr, artifact) move || {
            let _slot = slot;
            let open_writer =
                |name: &str| -> anyhow::Result<Box<dyn EntryWriter>> {
                    local_cache.writer(&addr, &hashin, name)
                };
            let name = cache_entry_name(&artifact);

            let (size, content_type) = match &artifact.content {
                outputartifact::Content::Raw(raw) => {
                    let mut cw = CountingWriter::new(
                        open_writer(&name).with_context(|| {
                            format!("open cache writer for {addr} {name}")
                        })?,
                    );
                    let mut p = hartifactcontent::tar::TarPacker::new();
                    p.create_raw(raw.data.clone(), raw.path.clone(), raw.x);
                    p.pack(&mut cw)
                        .with_context(|| format!("pack raw artifact into {addr} {name}"))?;
                    let size = cw.bytes_written();
                    cw.into_inner().commit().with_context(|| {
                        format!("commit raw artifact {addr} {name}")
                    })?;
                    (size, hartifactcontent::Type::Tar)
                }
                outputartifact::Content::File(file) => {
                    let mut cw = CountingWriter::new(
                        open_writer(&name).with_context(|| {
                            format!("open cache writer for {addr} {name}")
                        })?,
                    );
                    let mut p = hartifactcontent::tar::TarPacker::new();
                    p.create_file(file.source_path.clone(), file.out_path.clone());
                    p.pack(&mut cw).with_context(|| {
                        format!(
                            "pack file artifact {} into {addr} {name}",
                            file.source_path
                        )
                    })?;
                    let size = cw.bytes_written();
                    cw.into_inner().commit().with_context(|| {
                        format!("commit file artifact {addr} {name}")
                    })?;
                    (size, hartifactcontent::Type::Tar)
                }
                outputartifact::Content::TarPath(path) => {
                    let mut f = File::open(path)
                        .with_context(|| format!("open tar artifact {path}"))?;
                    let size = f
                        .metadata()
                        .with_context(|| format!("stat tar artifact {path}"))?
                        .size();
                    let mut w = open_writer(&name)
                        .with_context(|| format!("open cache writer for {addr} {name}"))?;
                    io::copy(&mut f, &mut w).with_context(|| {
                        format!("copy tar artifact {path} into {addr} {name}")
                    })?;
                    w.commit().with_context(|| {
                        format!("commit tar artifact {addr} {name}")
                    })?;
                    (size, hartifactcontent::Type::Tar)
                }
                outputartifact::Content::CpioPath(path) => {
                    let mut f = File::open(path)
                        .with_context(|| format!("open cpio artifact {path}"))?;
                    let size = f
                        .metadata()
                        .with_context(|| format!("stat cpio artifact {path}"))?
                        .size();
                    let mut w = open_writer(&name)
                        .with_context(|| format!("open cache writer for {addr} {name}"))?;
                    io::copy(&mut f, &mut w).with_context(|| {
                        format!("copy cpio artifact {path} into {addr} {name}")
                    })?;
                    w.commit().with_context(|| {
                        format!("commit cpio artifact {addr} {name}")
                    })?;
                    (size, hartifactcontent::Type::Cpio)
                }
            };

            let artifact_type = match artifact.r#type {
                outputartifact::Type::Output => ManifestArtifactType::Output,
                outputartifact::Type::Log => ManifestArtifactType::Log,
                outputartifact::Type::SupportFile => ManifestArtifactType::SupportFile,
            };

            anyhow::Ok((
                CacheArtifact {
                    addr: addr.clone(),
                    hashin: hashin.clone(),
                    name: name.clone(),
                    cache: local_cache.clone(),
                    hashout: artifact.hashout.clone(),
                    content_type,
                    group: artifact.group.clone(),
                    r#type: artifact_type.clone(),
                    size,
                },
                ManifestArtifact {
                    hashout: artifact.hashout.clone(),
                    group: artifact.group.clone(),
                    name: name.clone(),
                    size,
                    r#type: artifact_type,
                    content_type: match content_type {
                        hartifactcontent::Type::Tar => ManifestArtifactContentType::Tar,
                        hartifactcontent::Type::Cpio => ManifestArtifactContentType::Cpio,
                    },
                    encoding: ManifestArtifactEncoding::None,
                },
            ))
        }))
        .await
    }

    /// Persist `artifacts` for `addr` under the input hash `hashin`.
    pub async fn cache_locally(
        &self,
        ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        artifacts: Vec<outputartifact::OutputArtifact>,
        tmp: bool,
    ) -> anyhow::Result<Vec<CacheArtifact>> {
        // Two artifacts mapping to one cache entry name used to resolve
        // deterministically (sequential loop — last one won); under the
        // concurrent fan-out the winner would be pool scheduling, i.e. the
        // committed bytes would vary run to run under a manifest that lists
        // both. A colliding name is a driver bug either way — reject it before
        // writing anything.
        {
            let mut seen = std::collections::HashSet::with_capacity(artifacts.len());
            for artifact in &artifacts {
                let name = cache_entry_name(artifact);
                if !seen.insert(name.clone()) {
                    anyhow::bail!(
                        "duplicate cache entry name `{name}` among the artifacts of {addr}: \
                         artifact names must be unique per (type, name) within a target"
                    );
                }
            }
        }

        let key = if tmp {
            let nanos = time::SystemTime::now()
                .duration_since(time::UNIX_EPOCH)
                .expect("Time went backwards")
                .as_nanos();
            format!("{hashin}_{nanos}")
        } else {
            hashin.to_string()
        };

        // `tmp` (uncacheable/shell) revisions get a unique `{hashin}_{nanos}` key
        // and are never read back across runs, so route them to the mem-only
        // `local_cache_tmp` — small entries stay in memory and skip the SQLite
        // WAL write. `CacheArtifact` carries the cache it was written to, so
        // reads resolve against the same store.
        let cache = if tmp {
            &self.local_cache_tmp
        } else {
            &self.local_cache
        };

        // All artifacts in flight at once, not one at a time: each write is a
        // whole tar/copy on the blocking pool, so a multi-output target was
        // paying its writes back to back while the pool sat idle. Order is
        // preserved (join over an ordered iterator), so the manifest's artifact
        // list stays equal to the input order regardless of which write finishes
        // first — the manifest must not become a function of pool scheduling.
        //
        // Wait-all, **never fail-fast**: a `hcore::blocking` job runs to
        // completion even when its awaiting future is dropped, so a fail-fast
        // join would return an error while sibling tar jobs are still *reading
        // the sandbox* — racing the sandbox cleanup that execute runs right
        // after cache_locally errors. Driving every write to completion keeps
        // "no artifact job outlives this call" for every *polled-to-completion*
        // call, and reports every failure rather than the first. Dropping this
        // future mid-join still detaches up to a pack-slot's worth of submitted
        // jobs — that is inherent to the pool's run-to-completion contract, and
        // it is safe because an uncommitted `EntryWriter` discards on drop: a
        // detached straggler can neither surface a partial blob nor replace a
        // retry's good one.
        let key_ref = &key;
        let written = crate::engine::fanout::join_all_failable(
            artifacts.iter().map(|artifact| async move {
                self.cache_artifact_locally(ctoken, cache, addr, key_ref, artifact)
                    .await
                    .with_context(|| format!("cache artifact {} for {addr}", artifact.name))
            }),
            false,
        )
        .await?;
        let (res_artifacts, manifest_artifacts): (Vec<_>, Vec<_>) = written.into_iter().unzip();

        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: addr.format(),
            created_at_nanos: Utc::now().timestamp_nanos_opt().unwrap_or(0),
            hashin: hashin.to_string(),
            artifacts: manifest_artifacts,
        };

        // Manifest strictly last — it is the revision's commit record, so a
        // reader that finds it finds every artifact it names, or degrades to a
        // miss (same invariant as the remote cache's manifest-last upload). The
        // one exemption: the sqlite writer thread batches transactions, and a
        // failed *earlier* batch completes its slots and moves on — {manifest
        // committed, blob absent} is reachable there, and is exactly the state a
        // remote-mirrored revision starts in; the residency probe
        // (`missing_local_blobs`) covers both by degrading the hit to a miss.
        // Written only when every artifact write above succeeded, and written
        // inline: it lands in the sqlite cache's spooled writer (a memcpy plus a
        // channel send to the writer thread), so queueing it behind the blocking
        // pool's tar jobs would only delay the moment dependents can read this
        // result.
        let mut manifest_writer = cache
            .writer(addr, &key, MANIFEST_V1)
            .with_context(|| format!("open manifest writer for {addr}"))?;
        borsh::to_writer(&mut manifest_writer, &manifest)
            .with_context(|| format!("write manifest for {addr}"))?;
        manifest_writer
            .commit()
            .with_context(|| format!("commit manifest for {addr}"))?;

        // Remote push happens on a background task driven from the execute path
        // (see `Engine::spawn_remote_upload`), not here — it must not block the
        // build's critical path on the network.
        Ok(res_artifacts)
    }

    /// Read and deserialize a group's manifest. `Ok(None)` if it is absent.
    pub(crate) fn read_manifest(
        &self,
        addr: &Addr,
        hashin: &str,
    ) -> anyhow::Result<Option<Manifest>> {
        Self::read_manifest_from(&self.local_cache, addr, hashin)
    }

    /// [`read_manifest`](Self::read_manifest) against a cache handle rather than
    /// `&self`, so it can be moved onto the blocking pool (which needs a `'static`
    /// job — see [`read_manifest_blocking`](Self::read_manifest_blocking)).
    pub(crate) fn read_manifest_from(
        cache: &Arc<dyn LocalCache>,
        addr: &Addr,
        hashin: &str,
    ) -> anyhow::Result<Option<Manifest>> {
        let sized = match cache.reader(addr, hashin, MANIFEST_V1) {
            Ok(s) => s,
            Err(e) if e.is::<NotFoundError>() => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read manifest for {addr} {hashin}")),
        };
        let mut buf = Vec::with_capacity(sized.size as usize);
        sized
            .reader
            .take(sized.size)
            .read_to_end(&mut buf)
            .with_context(|| format!("read manifest bytes for {addr} {hashin}"))?;
        let manifest = borsh::from_slice::<Manifest>(&buf)
            .with_context(|| format!("deserialize manifest for {addr} {hashin}"))?;
        Ok(Some(manifest))
    }

    /// Copy a complete cache revision (manifest + every blob) from `src_key` to
    /// `dst_key`. Returns `false` (no-op) when the keys are equal or the source
    /// manifest is absent. Used by the in_place fixpoint path to register the
    /// just-written entry under the key a subsequent run will compute.
    pub(crate) fn duplicate_cache_revision(
        &self,
        addr: &Addr,
        src_key: &str,
        dst_key: &str,
    ) -> anyhow::Result<bool> {
        if src_key == dst_key {
            return Ok(false);
        }
        let Some(manifest) = self.read_manifest(addr, src_key)? else {
            return Ok(false);
        };
        self.duplicate_cache_entry(addr, src_key, dst_key, &manifest)?;
        Ok(true)
    }

    /// Copy the blobs named in `manifest` from `src_key` to `dst_key`, then write
    /// `manifest` (rewritten with `hashin = dst_key`) under `dst_key`. Blob bytes
    /// are copied verbatim so the duplicate is identical to the primary; only the
    /// manifest's `hashin` differs so a reader keyed by `dst_key` sees a
    /// consistent revision.
    fn duplicate_cache_entry(
        &self,
        addr: &Addr,
        src_key: &str,
        dst_key: &str,
        manifest: &Manifest,
    ) -> anyhow::Result<()> {
        for artifact in &manifest.artifacts {
            let mut reader = self
                .local_cache
                .reader(addr, src_key, &artifact.name)
                .with_context(|| {
                    format!("open source blob {} for {addr} {src_key}", artifact.name)
                })?
                .reader;
            let mut writer = self
                .local_cache
                .writer(addr, dst_key, &artifact.name)
                .with_context(|| {
                    format!("open dest blob {} for {addr} {dst_key}", artifact.name)
                })?;
            io::copy(&mut reader, &mut writer).with_context(|| {
                format!("copy blob {} for {addr} into {dst_key}", artifact.name)
            })?;
            writer.commit().with_context(|| {
                format!("commit blob {} for {addr} into {dst_key}", artifact.name)
            })?;
        }

        // Stamp the duplicate with a freshly-sampled, strictly-newer timestamp.
        // The fixpoint key is the most useful revision for a *subsequent* run
        // (it makes the already-transformed tree hit cache), so it must not be
        // the first thing the post-write history trim (`keep` newest) reclaims:
        // it has to outrank the primary on `created_at_nanos`. The primary is
        // independently protected by the trim, so both survive even at
        // `history = 1`.
        let dup_created = Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .max(manifest.created_at_nanos.saturating_add(1));
        let dup_manifest = Manifest {
            hashin: dst_key.to_string(),
            created_at_nanos: dup_created,
            ..manifest.clone()
        };
        let mut manifest_writer = self
            .local_cache
            .writer(addr, dst_key, MANIFEST_V1)
            .with_context(|| format!("open manifest writer for {addr} {dst_key}"))?;
        borsh::to_writer(&mut manifest_writer, &dup_manifest)
            .with_context(|| format!("write manifest for {addr} {dst_key}"))?;
        manifest_writer
            .commit()
            .with_context(|| format!("commit manifest for {addr} {dst_key}"))?;

        Ok(())
    }

    /// Async wrapper over the sync [`read_manifest`](Self::read_manifest) for the
    /// result hot path. The backend `reader` + `borsh::from_slice` is the expensive
    /// half of a cache lookup, so its result is stashed and reused across the
    /// presence-probe and the per-caller output read (see `LockedResolution::manifest`).
    ///
    /// On the dedicated blocking pool (see `hcore::blocking`, and
    /// `cache_artifact_locally` for why neither inline nor `spawn_blocking` works):
    /// this runs once per target on the hot path, and a backend read plus a borsh
    /// parse is more than a runtime worker should disappear into.
    pub(crate) async fn read_manifest_blocking(
        &self,
        _ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
    ) -> anyhow::Result<Option<Manifest>> {
        // Cloned rather than borrowed: a pool job outlives a dropped caller future
        // (cancellation), so it cannot borrow the caller's frame. An `Addr` plus a
        // hash is a handful of small strings — cheap next to the read itself.
        let (local_cache, addr, hashin) =
            (self.local_cache.clone(), addr.clone(), hashin.to_string());
        hcore::blocking::run(move || Self::read_manifest_from(&local_cache, &addr, &hashin)).await
    }

    /// The blobs a caller asking for `outputs` will actually read: its Output
    /// groups plus — **when it asked for at least one group** — every SupportFile
    /// (which travels with the target wherever its outputs are referenced), and
    /// never a Log — logs are written to the cache but no read path surfaces them.
    ///
    /// A caller that requested **no** output group reads nothing at all. It wants
    /// the revision's `hashout`s (to fold into its own `hashin`), which the
    /// manifest already carries; the bytes behind them are never opened. Support
    /// files only ever materialize into a sandbox alongside an output group, so
    /// with no group requested there is no reader for them either. That is the
    /// whole hashout-only path of a cached build — and the reason it moves no
    /// bytes and, on a manifest mirrored from a remote, makes no network call.
    /// Their `ArtifactMeta` is still reported: see
    /// [`artifacts_from_manifest`](Self::artifacts_from_manifest).
    ///
    /// **One exception, and it costs the network call.** An empty `outputs` is
    /// ambiguous: `OutputMatcher::None` and `OutputMatcher::All` over a target
    /// that declares no output group both arrive here as an empty vec, and only
    /// the second still wants its support files staged. [`support_files_needed`]
    /// resolves it from the manifest, erring towards staging — so a hashout-only
    /// resolve of an **output-less target that carries support files** (the
    /// `go_lint_gate` / `go_format_check` shape) still needs those blobs, and
    /// still pays a lookup for them. Threading the caller's intent down here
    /// instead of inferring it would close that; see the PR that introduced this.
    ///
    /// The single definition of "needed", shared by the read path
    /// ([`artifacts_from_manifest`](Self::artifacts_from_manifest)), the residency
    /// check ([`missing_local_blobs`](Self::missing_local_blobs)) and the remote
    /// presence check — so a lazy pull can never be decided against a different
    /// set than the one that will be read.
    pub(crate) fn needed_artifacts<'a>(
        manifest: &'a Manifest,
        outputs: &'a [String],
    ) -> impl Iterator<Item = &'a ManifestArtifact> {
        let support_needed = support_files_needed(manifest, outputs);
        manifest
            .artifacts
            .iter()
            .filter(move |a| artifact_is_needed(a, outputs, support_needed))
    }

    /// Names of the blobs a caller asking for `outputs` needs that are not in the
    /// local cache yet — exactly what has to be pulled from a remote before
    /// [`artifacts_from_manifest`](Self::artifacts_from_manifest) can serve the
    /// read.
    ///
    /// A non-empty result is the normal state of a revision whose manifest came
    /// from a remote: the manifest records the revision's identity and hashouts,
    /// not which of its blobs happen to be resident.
    pub(crate) async fn missing_local_blobs(
        &self,
        _ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        manifest: &Manifest,
        outputs: &[String],
    ) -> anyhow::Result<Vec<String>> {
        // Stays on the calling worker rather than going to `hcore::blocking`: this
        // is a walk of an already-parsed manifest plus one indexed point lookup per
        // needed artifact — no bytes read, no compression — and a `'static` job
        // would force `manifest` and `outputs` to be cloned or re-plumbed as `Arc`s
        // through the whole read path to offload work that barely occupies the
        // worker. That is only defensible because `exists_local` never blocks; the
        // plain `exists` it replaced could park here for a whole batch commit.
        let mut missing = Vec::new();
        for artifact in Self::needed_artifacts(manifest, outputs) {
            if !self
                .exists_local(addr, hashin, &artifact.name)
                .await
                .with_context(|| {
                    format!("probe local blob {} for {addr} {hashin}", artifact.name)
                })?
            {
                missing.push(artifact.name.clone());
            }
        }
        Ok(missing)
    }

    /// [`LocalCache::exists`] for an async caller: the same answer, without ever
    /// waiting on the backend's write-behind queue.
    ///
    /// `exists` covers a queued-but-uncommitted write by blocking until the batch
    /// commits, which is right on an OS thread and wrong on every caller in this
    /// module — see [`PendingWrite`]. Awaiting the slot suspends the task instead.
    ///
    /// "Never waits on the queue" is the exact claim, not "never blocks": the
    /// probe underneath still checks a connection out of the sqlite read pool, and
    /// `r2d2::Pool::get` waits on a condvar when the pool is exhausted. That is
    /// reachable — the pipe-read path and every open `OwnedBlob` (FUSE) hold a
    /// connection — and it is the remaining bound on this call.
    ///
    /// It re-probes rather than answering after one wait because a *new* write to
    /// the key can be queued between the await and the next check. The retries are
    /// capped: an unbounded loop here would spin a worker at full tilt against a
    /// backend that kept handing back a ready [`Existence::Queued`], which is a
    /// worse failure than the park it replaced because nothing would show it. On
    /// exhaustion the committed answer is the right one to return — a reader has no
    /// happens-before against a write still in flight, so "absent" was always a
    /// legitimate answer, and the `warn!` says the backend is behaving oddly.
    ///
    /// That exhaustion answer comes from [`LocalCache::exists_committed`], not
    /// from `exists`. `exists` waits out the queue, so using it here re-parked
    /// the worker in precisely the case the cap exists to prevent, while the
    /// `warn!` claimed the opposite. The case is reachable rather than
    /// hypothetical: `SpillWriter::spill` re-arms the pending map twice for one
    /// key (a write, then a `delete`) every time a blob crosses the spill
    /// threshold.
    pub(crate) async fn exists_local(
        &self,
        addr: &Addr,
        hashin: &str,
        name: &str,
    ) -> anyhow::Result<bool> {
        for _ in 0..MAX_QUEUE_WAITS {
            match self.local_cache.existence(addr, hashin, name)? {
                Existence::Committed(found) => return Ok(found),
                Existence::Queued(pending) => pending.await,
            }
        }
        tracing::warn!(
            %addr,
            hashin,
            name,
            waits = MAX_QUEUE_WAITS,
            "local cache kept reporting a queued write for one key; answering from committed state"
        );
        self.local_cache.exists_committed(addr, hashin, name)
    }

    /// Build this caller's artifact set from an already-parsed `manifest`, gating
    /// the blobs to [`needed_artifacts`](Self::needed_artifacts). Returns `None`
    /// when a required blob is missing — treat as a miss. Splitting this from
    /// [`read_manifest`](Self::read_manifest) lets a confirmed hit reuse the parsed
    /// manifest instead of re-reading + re-deserializing it for each caller.
    ///
    /// The two returned sets are deliberately not symmetric:
    ///
    /// - the **artifacts** are only the needed ones — what this caller reads;
    /// - the **metas** are *every* Output and SupportFile of the revision,
    ///   regardless of what was requested.
    ///
    /// The meta set is the target's contribution to its dependents' `hashin`, so
    /// it must depend on the revision alone. Were it narrowed to the requested
    /// groups, what a caller asked for would leak into the cache key — two
    /// dependents of the same revision would compute different keys, and the
    /// hit/miss decision would become an input to itself.
    ///
    /// `residency` says whether the per-blob probe is still needed — see
    /// [`BlobResidency`].
    pub(crate) async fn artifacts_from_manifest(
        &self,
        _ctoken: &dyn Cancellable,
        addr: &Addr,
        hashin: &str,
        manifest: &Manifest,
        outputs: &[String],
        residency: BlobResidency,
    ) -> anyhow::Result<Option<(Vec<CacheArtifact>, Vec<ArtifactMeta>)>> {
        let local_cache = &self.local_cache;
        // Stays on the calling worker for the same reason as `missing_local_blobs`:
        // point lookups and struct building over a manifest already in memory, with
        // `exists_local` keeping the probe non-blocking.
        let mut results: Vec<CacheArtifact> = Vec::with_capacity(manifest.artifacts.len());
        let mut result_meta: Vec<ArtifactMeta> = Vec::with_capacity(manifest.artifacts.len());
        let support_needed = support_files_needed(manifest, outputs);

        for artifact in &manifest.artifacts {
            // Outputs and SupportFiles both flow back to dependents — Output
            // populates SRC/list, SupportFile only materializes into the
            // sandbox. Logs and other types are kept in the cache but not
            // surfaced to callers here.
            match artifact.r#type {
                ManifestArtifactType::Output | ManifestArtifactType::SupportFile => {}
                ManifestArtifactType::Log => continue,
            }

            result_meta.push(ArtifactMeta {
                hashout: artifact.hashout.clone(),
            });

            // Past the meta, only what this caller actually reads is probed and
            // handed back — Outputs gated on the requested groups, SupportFiles
            // gated on there being a requested group at all.
            if !artifact_is_needed(artifact, outputs, support_needed) {
                continue;
            }

            if residency == BlobResidency::Unknown
                && !self
                    .exists_local(addr, hashin, artifact.name.as_ref())
                    .await
                    .with_context(|| {
                        format!("probe local blob {} for {addr} {hashin}", artifact.name)
                    })?
            {
                return Ok(None);
            }

            results.push(CacheArtifact {
                addr: addr.clone(),
                hashin: hashin.to_string(),
                name: artifact.name.clone(),
                cache: local_cache.clone(),
                content_type: match artifact.content_type {
                    ManifestArtifactContentType::Tar => hartifactcontent::Type::Tar,
                    ManifestArtifactContentType::Cpio => hartifactcontent::Type::Cpio,
                },
                r#type: artifact.r#type.clone(),
                hashout: artifact.hashout.clone(),
                group: artifact.group.clone(),
                size: artifact.size,
            });
        }

        Ok(Some((results, result_meta)))
    }

    pub async fn artifacts_from_local_cache(
        &self,
        ctoken: &dyn Cancellable,
        def: &LinkedTargetDef,
        hashin: &str,
        outputs: Vec<String>,
    ) -> anyhow::Result<Option<(Vec<CacheArtifact>, Vec<ArtifactMeta>)>> {
        let Some(manifest) = self
            .read_manifest_blocking(ctoken, &def.target.addr, hashin)
            .await?
        else {
            return Ok(None);
        };
        self.artifacts_from_manifest(
            ctoken,
            &def.target.addr,
            hashin,
            &manifest,
            &outputs,
            // Nothing has established residency for this manifest: it may name
            // blobs a GC has since reclaimed.
            BlobResidency::Unknown,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::driver::outputartifact;
    use crate::engine::driver::targetdef::{CacheConfig, TargetDef};
    use crate::engine::link::LinkedTargetDef;
    use hcore::hasync::StdCancellationToken;
    use hmodel::htpkg::PkgBuf;
    use std::collections::BTreeMap;

    fn test_engine() -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })
        .expect("engine");
        (engine, dir)
    }

    /// Engine wired to the remote cache at `remote_uri` (a `file://` dir shared
    /// across engines). Returns an `Arc` so the `self: &Arc<Self>` upload helper
    /// can be called.
    fn engine_with_remote(remote_uri: &str) -> (Arc<Engine>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            remote_caches: vec![crate::engine::RemoteCacheDef {
                name: "shared".to_string(),
                uri: remote_uri.to_string(),
                read: true,
                write: true,
                concurrency: 10,
            }],
            ..Default::default()
        })
        .expect("engine");
        (Arc::new(engine), dir)
    }

    /// End-to-end: a revision cached by one engine (with its own local cache) is
    /// pushed to a shared remote, then pulled into a *second* engine's local
    /// cache on a miss — proving upload-on-write and download-on-miss, with the
    /// whole revision (manifest + blob) coming from the one remote.
    #[tokio::test]
    async fn remote_cache_round_trips_between_engines() {
        let remote = tempfile::tempdir().expect("remote dir");
        let remote_uri = format!("file://{}", remote.path().display());
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();
        let def = linked_def(&addr);

        // Engine A writes a revision locally, then pushes it to the remote
        // (the push the background task performs in production).
        let (engine_a, _a) = engine_with_remote(&remote_uri);
        engine_a
            .cache_locally(
                &ctoken,
                &addr,
                "HASHIN1",
                vec![raw_artifact("a", b"shared payload")],
                false,
            )
            .await
            .expect("cache_locally");
        engine_a.upload_to_remote(&addr, "HASHIN1").await;

        // Engine B has a cold local cache: a direct local read misses.
        let (engine_b, _b) = engine_with_remote(&remote_uri);
        assert!(
            engine_b
                .read_manifest_blocking(&ctoken, &addr, "HASHIN1")
                .await
                .expect("read")
                .is_none(),
            "engine B local cache must start cold"
        );

        // Probing the remote mirrors the manifest — and only the manifest.
        let needed = vec!["out".to_string()];
        let (manifest, rev) = engine_b
            .probe_remote_revision(&ctoken, &addr, "HASHIN1", &needed)
            .await
            .expect("probe")
            .expect("remote hit");
        assert_eq!(manifest.artifacts.len(), 1);
        let blob_name = manifest.artifacts[0].name.clone();
        assert!(
            !engine_b
                .local_cache
                .exists(&addr, "HASHIN1", &blob_name)
                .expect("exists"),
            "probing the remote must not download any output blob"
        );

        // Pulling that one blob is what puts bytes in B's local cache.
        engine_b
            .pull_remote_blobs(&ctoken, &addr, "HASHIN1", &rev, &[blob_name])
            .await
            .expect("pull");

        // The blob is now served from B's *local* cache, byte-identical.
        let (arts, _) = engine_b
            .artifacts_from_local_cache(&ctoken, &def, "HASHIN1", vec!["out".to_string()])
            .await
            .expect("read")
            .expect("present locally after download");
        assert_eq!(arts.len(), 1);
        let bytes = drain_reader(
            engine_b
                .local_cache
                .reader(&addr, "HASHIN1", &arts[0].name)
                .expect("local blob")
                .reader,
        );
        assert!(!bytes.is_empty());
    }

    /// After a remote materialization, the per-blob probe in
    /// `artifacts_from_manifest` has nothing left to learn — the same artifact set
    /// was just probed and pulled, `needed_artifacts` being shared by both paths.
    /// Skipping it saves a pooled connection and a point lookup per needed blob on
    /// the hot path of every remote hit.
    ///
    /// Asserted by the shape that separates the two answers: a manifest whose
    /// blob is *not* in the local cache. `Unknown` has to probe and therefore
    /// reports a miss; `Established` cannot be probing, because it serves the
    /// read.
    #[tokio::test]
    async fn established_residency_does_not_re_probe_the_local_cache() {
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();
        let (engine, _dir) = engine_with_remote("file:///dev/null/unused");

        // A manifest that names a blob nobody ever wrote.
        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: addr.format(),
            created_at_nanos: 0,
            hashin: "HASHRES".to_string(),
            artifacts: vec![ManifestArtifact {
                hashout: "HO".to_string(),
                group: "out".to_string(),
                name: "absent.tar".to_string(),
                size: 1,
                r#type: ManifestArtifactType::Output,
                content_type: ManifestArtifactContentType::Tar,
                encoding: ManifestArtifactEncoding::None,
            }],
        };
        let outputs = vec!["out".to_string()];

        assert!(
            engine
                .artifacts_from_manifest(
                    &ctoken,
                    &addr,
                    "HASHRES",
                    &manifest,
                    &outputs,
                    BlobResidency::Unknown,
                )
                .await
                .expect("read")
                .is_none(),
            "an unprobed manifest whose blob is gone must degrade to a miss"
        );

        let (arts, meta) = engine
            .artifacts_from_manifest(
                &ctoken,
                &addr,
                "HASHRES",
                &manifest,
                &outputs,
                BlobResidency::Established,
            )
            .await
            .expect("read")
            .expect("established residency must serve without probing");
        assert_eq!(arts.len(), 1, "the artifact set is still built in full");
        assert_eq!(meta.len(), 1);
    }

    /// Every blob of a pushed revision is gzipped into its own temp file first.
    /// A real upload therefore creates N of them, and the only thing that
    /// reclaims them is the `TempBlob` guards travelling in `prepared` — there is
    /// no explicit cleanup left to notice if one of them stops being a guard.
    #[tokio::test]
    async fn a_completed_upload_leaves_no_temp_blobs_behind() {
        let remote = tempfile::tempdir().expect("remote dir");
        let remote_uri = format!("file://{}", remote.path().display());
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        let (engine, _dir) = engine_with_remote(&remote_uri);
        engine
            .cache_locally(
                &ctoken,
                &addr,
                "HASHTMP",
                vec![
                    raw_artifact("a", b"first payload"),
                    raw_artifact("b", b"second payload"),
                    raw_artifact("c", b"third payload"),
                ],
                false,
            )
            .await
            .expect("cache_locally");
        engine.upload_to_remote(&addr, "HASHTMP").await;

        let tmp_dir = engine.home.join("cache").join("remote-tmp");
        let leftovers: Vec<_> = std::fs::read_dir(&tmp_dir)
            .expect("the upload must have created the temp dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "blob"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a completed upload must reclaim every encode temp, found {leftovers:?}"
        );
    }

    /// The background upload bumps the request's `bg_pending` counter and drops
    /// it back to zero once the push finishes — the signal the CLI/TUI shutdown
    /// path waits on so it never exits with an upload in flight. The pushed
    /// revision is then visible to a fresh engine.
    #[tokio::test]
    async fn spawn_remote_upload_tracks_bg_pending_and_lands() {
        use std::sync::atomic::Ordering;

        let remote = tempfile::tempdir().expect("remote dir");
        let remote_uri = format!("file://{}", remote.path().display());
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        let (engine, _e) = engine_with_remote(&remote_uri);
        engine
            .cache_locally(
                &ctoken,
                &addr,
                "HASHBG",
                vec![raw_artifact("a", b"bg payload")],
                false,
            )
            .await
            .expect("cache_locally");

        let rs = engine.new_state();
        let bg = rs.bg_pending();
        assert_eq!(bg.load(Ordering::Acquire), 0);
        engine.spawn_remote_upload(&rs, addr.clone(), "HASHBG".to_string());

        // Drains to zero once the detached upload completes.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while bg.load(Ordering::Acquire) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "bg_pending never drained"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // The revision is now on the remote: a cold engine locates it.
        let (engine2, _e2) = engine_with_remote(&remote_uri);
        assert!(
            engine2
                .probe_remote_revision(&ctoken, &addr, "HASHBG", &["out".to_string()])
                .await
                .expect("probe")
                .is_some(),
            "background upload must land on the remote"
        );
    }

    /// A missing remote entry yields `None` (→ execute), not an error.
    #[tokio::test]
    async fn remote_download_miss_is_none() {
        let remote = tempfile::tempdir().expect("remote dir");
        let remote_uri = format!("file://{}", remote.path().display());
        let (engine, _d) = engine_with_remote(&remote_uri);
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();
        assert!(
            engine
                .probe_remote_revision(&ctoken, &addr, "NOPE", &["out".to_string()])
                .await
                .expect("probe")
                .is_none()
        );
    }

    /// A revision whose manifest is on the remote but whose blob has been expired
    /// (an independent object-store lifecycle rule can do exactly that) must read
    /// as a **miss**, so the target executes. Accepting the hit on the manifest
    /// alone would strand the build: the pull would fail after the engine already
    /// decided "already built".
    #[tokio::test]
    async fn remote_probe_rejects_a_revision_whose_blob_is_gone() {
        let remote = tempfile::tempdir().expect("remote dir");
        let remote_uri = format!("file://{}", remote.path().display());
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        let (engine_a, _a) = engine_with_remote(&remote_uri);
        engine_a
            .cache_locally(
                &ctoken,
                &addr,
                "HASHEVICT",
                vec![raw_artifact("a", b"payload")],
                false,
            )
            .await
            .expect("cache_locally");
        engine_a.upload_to_remote(&addr, "HASHEVICT").await;

        // Expire every blob object, leaving the manifest behind.
        let needed = vec!["out".to_string()];
        let (manifest, _) = engine_a
            .probe_remote_revision(&ctoken, &addr, "HASHEVICT", &needed)
            .await
            .expect("probe")
            .expect("remote hit while intact");
        let blob_names: Vec<String> = manifest.artifacts.iter().map(|a| a.name.clone()).collect();
        let removed = evict_objects(remote.path(), &blob_names);
        assert!(removed > 0, "test must actually evict a blob");

        let (engine_b, _b) = engine_with_remote(&remote_uri);
        assert!(
            engine_b
                .probe_remote_revision(&ctoken, &addr, "HASHEVICT", &needed)
                .await
                .expect("probe")
                .is_none(),
            "a revision the remote can no longer serve must read as a miss"
        );
    }

    /// Delete every file under `dir` (recursively) whose file name is in `names`.
    /// Mimics an object-store lifecycle rule expiring blobs while leaving the
    /// revision's manifest object in place. Returns how many were removed.
    fn evict_objects(dir: &std::path::Path, names: &[String]) -> usize {
        let mut removed = 0;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                removed += evict_objects(&path, names);
            } else if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
                && names.iter().any(|n| n == file_name)
            {
                std::fs::remove_file(&path).expect("evict blob");
                removed += 1;
            }
        }
        removed
    }

    /// The artifact writes fan out concurrently, so two properties that were
    /// implicit in the old sequential loop have to be pinned: the manifest's
    /// artifact list keeps the *input* order (not completion order — the
    /// manifest must not become a function of pool scheduling), and every
    /// artifact a manifest names is readable once the manifest is.
    #[tokio::test]
    async fn multi_artifact_revision_keeps_input_order() {
        let (engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        // Enough artifacts to actually overlap on the pool, with sizes varied
        // so completion order differs from input order.
        let artifacts: Vec<_> = (0..16)
            .map(|i| {
                let payload = vec![b'x'; if i % 2 == 0 { 512 * 1024 } else { 8 }];
                raw_artifact(&format!("a{i:02}"), &payload)
            })
            .collect();
        engine
            .cache_locally(&ctoken, &addr, "HASHIN_ORDER", artifacts, false)
            .await
            .expect("cache_locally");

        let manifest = engine
            .read_manifest(&addr, "HASHIN_ORDER")
            .expect("read manifest")
            .expect("manifest present");
        let names: Vec<_> = manifest.artifacts.iter().map(|a| a.name.as_str()).collect();
        let expected: Vec<_> = (0..16).map(|i| format!("out_a{i:02}.tar")).collect();
        assert_eq!(
            names,
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    /// Manifest-last, the strong form: the manifest writer must not even be
    /// *opened* until every artifact's writer has been dropped (write complete).
    /// This is the test that reddens if the manifest write is ever folded into
    /// the artifact fan-out.
    #[tokio::test]
    async fn manifest_opens_only_after_every_artifact_write_completes() {
        let (mut engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        #[derive(Default)]
        struct Log {
            events: std::sync::Mutex<Vec<(String, &'static str)>>,
        }
        let log = Arc::new(Log::default());
        engine.local_cache = Arc::new(
            crate::engine::local_cache_test_double::ForwardingCache::new(Arc::clone(
                &engine.local_cache,
            ))
            .on_writer(enclose!((log) move |_, _, name| {
                log.events.lock().unwrap().push((name.to_string(), "open"));
            }))
            .on_writer_done(enclose!((log) move |_, _, name| {
                log.events.lock().unwrap().push((name.to_string(), "done"));
            })),
        );

        let artifacts: Vec<_> = (0..8)
            .map(|i| raw_artifact(&format!("a{i}"), &vec![b'x'; 256 * 1024]))
            .collect();
        engine
            .cache_locally(&ctoken, &addr, "HASHIN_LAST", artifacts, false)
            .await
            .expect("cache_locally");

        let events = log.events.lock().unwrap();
        let manifest_open = events
            .iter()
            .position(|(name, ev)| name == MANIFEST_V1 && *ev == "open")
            .expect("manifest opened");
        let artifact_dones = events
            .iter()
            .enumerate()
            .filter(|(_, (name, ev))| name != MANIFEST_V1 && *ev == "done")
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        assert_eq!(artifact_dones.len(), 8, "events: {events:?}");
        assert!(
            artifact_dones.iter().all(|&i| i < manifest_open),
            "manifest opened before an artifact write finished: {events:?}"
        );
    }

    /// Wait-all fan-out: when several artifact writes fail, every failure is
    /// reported — not just the first. The behavior change from the old
    /// stop-at-first loop is deliberate and user-visible, so freeze it.
    #[tokio::test]
    async fn all_failing_artifacts_are_reported() {
        let (engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        let missing = |name: &str| outputartifact::OutputArtifact {
            group: "out".to_string(),
            name: name.to_string(),
            r#type: outputartifact::Type::Output,
            content: outputartifact::Content::TarPath(format!("/nonexistent/heph-{name}")),
            hashout: format!("hashout-{name}"),
        };
        let Err(err) = engine
            .cache_locally(
                &ctoken,
                &addr,
                "HASHIN_MULTIFAIL",
                vec![
                    missing("first"),
                    raw_artifact("ok", b"fine"),
                    missing("second"),
                ],
                false,
            )
            .await
        else {
            panic!("missing tar paths must fail the write");
        };
        let rendered = format!("{err:#}");
        assert!(rendered.contains("first"), "got: {rendered}");
        assert!(rendered.contains("second"), "got: {rendered}");
    }

    /// The pack cap gates the path to the blocking pool: with every slot held,
    /// an artifact write must not reach the cache writer; releasing the slots
    /// lets it through. (Borrows a process-wide semaphore, so it briefly delays
    /// any concurrently-running test that writes to a cache.)
    #[tokio::test]
    async fn artifact_write_waits_for_a_pack_slot() {
        let (engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        // The full capacity, not `available_permits()`: a snapshot of what is
        // free right now races other tests' in-flight writes — one of them
        // returning its permit after the snapshot hands this test's gated write
        // a free slot and flakes the assertion. `acquire_many(capacity)` instead
        // waits for every straggler and then genuinely holds the whole class.
        let held = LOCAL_PACK_SLOTS
            .acquire_many(u32::try_from(local_pack_slots()).unwrap())
            .await
            .expect("hold every pack slot");

        let write = engine.cache_locally(
            &ctoken,
            &addr,
            "HASHIN_SLOT",
            vec![raw_artifact("gated", b"payload")],
            false,
        );
        tokio::pin!(write);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut write)
                .await
                .is_err(),
            "a write must not reach the pool while every pack slot is held",
        );

        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(30), write)
            .await
            .expect("releasing the slots must let the write through")
            .expect("cache_locally");
    }

    /// Two artifacts that map to the same cache entry name must be rejected up
    /// front: under the concurrent fan-out the committed bytes would otherwise
    /// depend on pool scheduling.
    #[tokio::test]
    async fn duplicate_entry_names_are_rejected_before_any_write() {
        let (engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        let Err(err) = engine
            .cache_locally(
                &ctoken,
                &addr,
                "HASHIN_DUP",
                vec![raw_artifact("same", b"a"), raw_artifact("same", b"b")],
                false,
            )
            .await
        else {
            panic!("duplicate entry names must fail");
        };
        assert!(format!("{err:#}").contains("same"), "got: {err:#}");
        assert!(
            engine
                .read_manifest(&addr, "HASHIN_DUP")
                .expect("read manifest")
                .is_none(),
            "nothing may be committed for a rejected revision"
        );
    }

    /// An empty artifact set still commits a manifest (an empty revision is a
    /// valid, readable result), unchanged by the fan-out.
    #[tokio::test]
    async fn empty_artifact_set_still_commits_a_manifest() {
        let (engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();
        engine
            .cache_locally(&ctoken, &addr, "HASHIN_EMPTY", vec![], false)
            .await
            .expect("cache_locally");
        let manifest = engine
            .read_manifest(&addr, "HASHIN_EMPTY")
            .expect("read manifest")
            .expect("manifest present");
        assert!(manifest.artifacts.is_empty());
    }

    /// Dropping `cache_locally` mid-fan-out must not wedge anything: already
    /// submitted pool jobs finish on their own, and a subsequent write of the
    /// same revision succeeds.
    #[tokio::test]
    async fn dropping_cache_locally_mid_write_leaves_the_engine_usable() {
        let (engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        let artifacts: Vec<_> = (0..8)
            .map(|i| raw_artifact(&format!("a{i}"), &vec![b'x'; 512 * 1024]))
            .collect();
        {
            let write =
                engine.cache_locally(&ctoken, &addr, "HASHIN_DROP", artifacts.clone(), false);
            tokio::pin!(write);
            // Poll it exactly once to start the fan-out, then drop it.
            let _ = futures::poll!(&mut write);
        }

        engine
            .cache_locally(&ctoken, &addr, "HASHIN_DROP", artifacts, false)
            .await
            .expect("a fresh write after an abandoned one must succeed");
        assert!(
            engine
                .read_manifest(&addr, "HASHIN_DROP")
                .expect("read manifest")
                .is_some()
        );
    }

    /// Manifest-last: a failed artifact write must error out of `cache_locally`
    /// with *no* manifest committed — a reader that finds a manifest must find
    /// every artifact it names, so an incomplete revision has to stay invisible.
    #[tokio::test]
    async fn failed_artifact_write_commits_no_manifest() {
        let (engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();

        let missing = outputartifact::OutputArtifact {
            group: "out".to_string(),
            name: "gone".to_string(),
            r#type: outputartifact::Type::Output,
            content: outputartifact::Content::TarPath("/nonexistent/heph-test-tar".to_string()),
            hashout: "hashout-gone".to_string(),
        };
        let Err(err) = engine
            .cache_locally(
                &ctoken,
                &addr,
                "HASHIN_FAIL",
                vec![raw_artifact("ok", b"fine"), missing],
                false,
            )
            .await
        else {
            panic!("missing tar path must fail the write");
        };
        assert!(
            format!("{err:#}").contains("gone"),
            "error names the artifact: {err:#}"
        );

        assert!(
            engine
                .read_manifest(&addr, "HASHIN_FAIL")
                .expect("read manifest")
                .is_none(),
            "a failed revision must not commit a manifest"
        );
    }

    fn test_addr() -> Addr {
        Addr::new(PkgBuf::from("pkg"), "tgt".to_string(), BTreeMap::new())
    }

    fn linked_def(addr: &Addr) -> LinkedTargetDef {
        let target = Arc::new(TargetDef {
            addr: addr.clone(),
            labels: Vec::new(),
            raw_def: Arc::new(()),
            inputs: Vec::new(),
            outputs: Vec::new(),
            support_files: Vec::new(),
            cache: CacheConfig::on(false),
            pty: false,
            hash: Vec::new(),
            transparent: false,
        });
        LinkedTargetDef {
            target,
            inputs: Vec::new(),
        }
    }

    fn raw_artifact(name: &str, data: &[u8]) -> outputartifact::OutputArtifact {
        outputartifact::OutputArtifact {
            group: "out".to_string(),
            name: name.to_string(),
            r#type: outputartifact::Type::Output,
            content: outputartifact::Content::Raw(outputartifact::ContentRaw {
                data: data.to_vec(),
                path: format!("{name}.txt"),
                x: false,
            }),
            hashout: format!("hashout-{name}"),
        }
    }

    /// `duplicate_cache_revision` (the in_place fixpoint primitive) must copy both
    /// the manifest and every blob under the destination key, so a reader keyed by
    /// it finds a complete revision identical to the source; and it must no-op on
    /// equal keys or a missing source manifest.
    #[tokio::test]
    async fn duplicate_cache_revision_copies_manifest_and_blobs() {
        let (engine, _dir) = test_engine();
        let ctoken = StdCancellationToken::new();
        let addr = test_addr();
        let def = linked_def(&addr);

        engine
            .cache_locally(
                &ctoken,
                &addr,
                "PRIMARYHASH",
                vec![raw_artifact("a", b"hello fixpoint")],
                false,
            )
            .await
            .expect("cache_locally");

        // No-ops: equal keys and a missing source manifest.
        assert!(
            !engine
                .duplicate_cache_revision(&addr, "PRIMARYHASH", "PRIMARYHASH")
                .expect("equal keys")
        );
        assert!(
            !engine
                .duplicate_cache_revision(&addr, "MISSINGHASH", "FIXPOINTKEY")
                .expect("missing source")
        );

        // Real duplication under a derived key.
        assert!(
            engine
                .duplicate_cache_revision(&addr, "PRIMARYHASH", "FIXPOINTKEY")
                .expect("duplicate")
        );

        let (primary_arts, _) = engine
            .artifacts_from_local_cache(&ctoken, &def, "PRIMARYHASH", vec!["out".to_string()])
            .await
            .expect("read primary")
            .expect("primary present");
        let (extra_arts, _) = engine
            .artifacts_from_local_cache(&ctoken, &def, "FIXPOINTKEY", vec!["out".to_string()])
            .await
            .expect("read extra")
            .expect("extra present");

        assert_eq!(extra_arts.len(), 1);
        assert_eq!(extra_arts[0].name, primary_arts[0].name);
        assert_eq!(extra_arts[0].hashout, primary_arts[0].hashout);

        // Blob bytes under the derived key match the primary's exactly.
        let primary_bytes = drain_reader(
            engine
                .local_cache
                .reader(&addr, "PRIMARYHASH", &primary_arts[0].name)
                .expect("primary blob")
                .reader,
        );
        let extra_bytes = drain_reader(
            engine
                .local_cache
                .reader(&addr, "FIXPOINTKEY", &extra_arts[0].name)
                .expect("extra blob")
                .reader,
        );
        assert_eq!(primary_bytes, extra_bytes);
        assert!(!primary_bytes.is_empty());
    }

    fn drain_reader(mut r: Box<dyn io::Read>) -> Vec<u8> {
        let mut out = Vec::new();
        io::Read::read_to_end(&mut r, &mut out).expect("read");
        out
    }

    /// A backend that reports the write queue a fixed number of times before
    /// settling — a key rewritten while it is being probed, or a backend that has
    /// gone wrong and keeps claiming a queue forever.
    ///
    /// `exists` **blocks**, standing in for the real backend's
    /// `wait_if_pending`: `exists_local` must never reach it, and a double that
    /// answered instantly would let a regression through unnoticed.
    struct QueuedTimes {
        queued_answers: std::sync::Mutex<usize>,
        committed: bool,
        existence_calls: Arc<std::sync::atomic::AtomicUsize>,
        exists_calls: Arc<std::sync::atomic::AtomicUsize>,
        exists_committed_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl LocalCache for QueuedTimes {
        fn reader(&self, _: &Addr, _: &str, _: &str) -> anyhow::Result<SizedReader> {
            Err(anyhow::anyhow!(NotFoundError))
        }
        fn writer(&self, _: &Addr, _: &str, _: &str) -> anyhow::Result<Box<dyn EntryWriter>> {
            unreachable!("the probe path never writes")
        }
        fn exists(&self, _: &Addr, _: &str, _: &str) -> anyhow::Result<bool> {
            self.exists_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // The waiting variant: `LocalCacheSQLite::exists` parks on the
            // pending slot's untimed condvar. Sleeping rather than blocking
            // forever so a regression ends with the counters intact instead of
            // hanging the suite — comfortably past the caller's 5s timeout, but
            // not so long that a failing run is painful. Never reached while the
            // fallback goes through `exists_committed`, so it costs nothing.
            std::thread::sleep(std::time::Duration::from_secs(10));
            Ok(self.committed)
        }
        fn exists_committed(&self, _: &Addr, _: &str, _: &str) -> anyhow::Result<bool> {
            self.exists_committed_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.committed)
        }
        fn existence(&self, _: &Addr, _: &str, _: &str) -> anyhow::Result<Existence> {
            self.existence_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut left = self.queued_answers.lock().expect("queued answers");
            if *left > 0 {
                *left -= 1;
                // Ready on the first poll: the point is the loop's control flow,
                // not the waiting, and a ready future is also the shape that would
                // spin a worker without the cap.
                return Ok(Existence::Queued(PendingWrite::new(std::future::ready(()))));
            }
            Ok(Existence::Committed(self.committed))
        }
        fn delete(&self, _: &Addr, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn engine_with_cache(cache: Arc<dyn LocalCache>) -> (Engine, tempfile::TempDir) {
        let (mut engine, dir) = test_engine();
        engine.local_cache = cache;
        (engine, dir)
    }

    /// The invariant the whole probe path rests on: a write that is queued but not
    /// yet committed must be reported *present*. If `exists_local` answered from
    /// committed state while a write was in flight, every freshly-written blob
    /// would look absent and a cache hit would silently become a rebuild.
    #[tokio::test]
    async fn exists_local_reports_a_queued_write_as_present() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (engine, _dir) = engine_with_cache(Arc::new(QueuedTimes {
            queued_answers: std::sync::Mutex::new(2),
            committed: true,
            existence_calls: calls.clone(),
            exists_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            exists_committed_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }));

        assert!(
            engine
                .exists_local(&test_addr(), "h", "blob")
                .await
                .expect("probe"),
            "a queued write must resolve to present, not to the committed miss"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "two queued answers must be waited out, then the settled one taken"
        );
    }

    /// A backend that never settles must not spin a worker. The loop gives up
    /// after a bounded number of waits and answers from committed state, which is
    /// a legitimate answer for a reader with no happens-before against the write.
    ///
    /// And it must reach that answer **without waiting**. `exists` is the
    /// backend's waiting probe — on sqlite it parks on `PendingSlot`'s untimed
    /// condvar — so falling back to it here re-parks the worker in exactly the
    /// case the cap exists to prevent, while the `warn!` claims the opposite.
    /// The double's `exists` therefore blocks: swap `exists_committed` back to
    /// `exists` in `exists_local` and this test fails on the call counters
    /// below. Not on the `timeout` — `#[tokio::test]` is current-thread, so the
    /// double's `sleep` blocks the runtime and the timer never fires; the sleep
    /// is there to make the park *real* rather than to trip a deadline, and the
    /// counters are what catch it.
    #[tokio::test]
    async fn exists_local_gives_up_without_ever_waiting_on_the_queue() {
        let existence_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let exists_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let exists_committed_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (engine, _dir) = engine_with_cache(Arc::new(QueuedTimes {
            queued_answers: std::sync::Mutex::new(usize::MAX),
            committed: false,
            existence_calls: existence_calls.clone(),
            exists_calls: exists_calls.clone(),
            exists_committed_calls: exists_committed_calls.clone(),
        }));

        let found = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.exists_local(&test_addr(), "h", "blob"),
        )
        .await
        .expect("a never-settling backend must not hang the probe")
        .expect("probe");

        assert!(!found);
        assert_eq!(
            existence_calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_QUEUE_WAITS,
            "the retries must be capped"
        );
        assert_eq!(
            exists_committed_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the capped-out answer comes from one committed-state probe"
        );
        assert_eq!(
            exists_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the capped-out path must never call the queue-waiting probe"
        );
    }
}
