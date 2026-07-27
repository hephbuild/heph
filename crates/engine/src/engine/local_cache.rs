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
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::{io, time};

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
/// The per-blob `exists` in [`Engine::artifacts_from_manifest`] is not free. It
/// takes a pooled sqlite connection, and — because a write is only *queued* to
/// the single writer thread, not committed — a key written moments ago is still
/// pending, so the probe parks on an untimed condvar until the writer's next
/// batch drains. On the remote-hit path that probe runs inline on a tokio
/// worker, so `n` concurrent hits park `n` workers on a batch commit, and the
/// reactor, the timer wheel, the in-flight transfers and the TUI stop with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobResidency {
    /// Nothing has looked yet — probe each needed blob, and degrade the hit to a
    /// miss if one is gone. The state of a plain local hit, where the manifest
    /// may well outlive the blobs a GC reclaimed.
    Unknown,
    /// Every needed blob was just confirmed present or pulled, by
    /// `Engine::materialize_from_remote` over exactly this artifact set
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

pub trait LocalCache: Send + Sync {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<SizedReader>;
    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Box<dyn io::Write>>;
    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool>;
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
        // Writing a revision tars and copies every output — the heaviest
        // synchronous work in a build, once per target. It runs on the dedicated
        // blocking pool: not inline (that parks a runtime worker with the runtime
        // unaware, and enough concurrent writes stop the reactor entirely) and not
        // `spawn_blocking` (whose JoinHandle wake-up rides tokio's cross-thread
        // waker, observed to drop wakeups on macOS under load — see
        // `RCA_MACOS_WAKER.md`). See `hcore::blocking`.
        hcore::blocking::run(enclose!((cache => local_cache, addr, artifact) move || {
            let open_writer =
                |name: &str| -> anyhow::Result<Box<dyn io::Write>> {
                    local_cache.writer(&addr, &hashin, name)
                };
            let type_prefix = match artifact.r#type {
                outputartifact::Type::Output => "out",
                outputartifact::Type::Log => "log",
                outputartifact::Type::SupportFile => "support",
            };

            let (size, content_type, name) = match &artifact.content {
                outputartifact::Content::Raw(raw) => {
                    let name = format!("{}_{}.tar", type_prefix, artifact.name);
                    let mut cw = CountingWriter::new(
                        open_writer(&name).with_context(|| {
                            format!("open cache writer for {addr} {name}")
                        })?,
                    );
                    let mut p = hartifactcontent::tar::TarPacker::new();
                    p.create_raw(raw.data.clone(), raw.path.clone(), raw.x);
                    p.pack(&mut cw)
                        .with_context(|| format!("pack raw artifact into {addr} {name}"))?;
                    (cw.bytes_written(), hartifactcontent::Type::Tar, name)
                }
                outputartifact::Content::File(file) => {
                    let name = format!("{}_{}.tar", type_prefix, artifact.name);
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
                    (cw.bytes_written(), hartifactcontent::Type::Tar, name)
                }
                outputartifact::Content::TarPath(path) => {
                    let name = format!("{}_{}", type_prefix, artifact.name);
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
                    (size, hartifactcontent::Type::Tar, name)
                }
                outputartifact::Content::CpioPath(path) => {
                    let name = format!("{}_{}", type_prefix, artifact.name);
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
                    (size, hartifactcontent::Type::Cpio, name)
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
        let mut res_artifacts = Vec::with_capacity(artifacts.len());
        let mut manifest_artifacts = Vec::with_capacity(artifacts.len());

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

        for artifact in artifacts {
            let artifact_name = artifact.name.clone();
            let (cached_artifact, manifest_artifact) = self
                .cache_artifact_locally(ctoken, cache, addr, &key, &artifact)
                .await
                .with_context(|| format!("cache artifact {artifact_name} for {addr}"))?;
            res_artifacts.push(cached_artifact);
            manifest_artifacts.push(manifest_artifact);
        }

        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: addr.format(),
            created_at_nanos: Utc::now().timestamp_nanos_opt().unwrap_or(0),
            hashin: hashin.to_string(),
            artifacts: manifest_artifacts,
        };

        let mut manifest_writer = cache
            .writer(addr, &key, MANIFEST_V1)
            .with_context(|| format!("open manifest writer for {addr}"))?;
        borsh::to_writer(&mut manifest_writer, &manifest)
            .with_context(|| format!("write manifest for {addr}"))?;

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
    /// groups plus every SupportFile (which travels with the target wherever it is
    /// referenced), and never a Log — logs are written to the cache but no read
    /// path surfaces them.
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
        manifest.artifacts.iter().filter(|a| match a.r#type {
            ManifestArtifactType::Output => outputs.contains(&a.group),
            ManifestArtifactType::SupportFile => true,
            ManifestArtifactType::Log => false,
        })
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
        let local_cache = &self.local_cache;
        // Deliberately still inline, unlike the write path: this is a walk of an
        // already-parsed manifest plus one `exists` stat per needed artifact — no
        // bytes read, no compression. Moving it to the blocking pool would mean a
        // `'static` job, so `manifest` and `outputs` would have to be cloned or
        // re-plumbed as `Arc`s through the whole read path, to take work off the
        // worker that barely occupies it.
        hproc::process_supervisor::block_or_inline(move || {
            let mut missing = Vec::new();
            for artifact in Self::needed_artifacts(manifest, outputs) {
                if !local_cache
                    .exists(addr, hashin, &artifact.name)
                    .with_context(|| {
                        format!("probe local blob {} for {addr} {hashin}", artifact.name)
                    })?
                {
                    missing.push(artifact.name.clone());
                }
            }
            anyhow::Ok(missing)
        })
    }

    /// Build this caller's artifact set from an already-parsed `manifest`, gating
    /// Output groups to `outputs` (SupportFiles always travel). Returns `None`
    /// when a required blob is missing — treat as a miss. Splitting this from
    /// [`read_manifest`](Self::read_manifest) lets a confirmed hit reuse the parsed
    /// manifest instead of re-reading + re-deserializing it for each caller.
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
        // Inline for the same reason as `missing_local_blobs`: stats and struct
        // building over a manifest already in memory.
        hproc::process_supervisor::block_or_inline(move || {
            let mut results: Vec<CacheArtifact> = Vec::with_capacity(manifest.artifacts.len());
            let mut result_meta: Vec<ArtifactMeta> = Vec::with_capacity(manifest.artifacts.len());

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

                // Outputs are gated on the caller's requested output groups.
                // SupportFiles travel with the target wherever it's referenced.
                if artifact.r#type == ManifestArtifactType::Output
                    && !outputs.contains(&artifact.group)
                {
                    continue;
                }

                if residency == BlobResidency::Unknown
                    && !local_cache.exists(addr, hashin, artifact.name.as_ref())?
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

            anyhow::Ok(Some((results, result_meta)))
        })
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

    /// After a remote materialization, the per-blob `exists` in
    /// `artifacts_from_manifest` has nothing left to learn — the same artifact
    /// set was just probed and pulled. It is not merely redundant: those keys are
    /// freshly *queued* to the single sqlite writer, so re-probing parks on the
    /// pending slot's untimed condvar until the batch drains, on a path that runs
    /// inline on a tokio worker.
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
}
