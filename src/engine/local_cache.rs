use crate::engine::Engine;
use crate::engine::driver::outputartifact;
use crate::engine::link::LinkedTargetDef;
use crate::engine::result::ArtifactMeta;
use crate::hartifactcontent;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use anyhow::Context;
use borsh::{BorshDeserialize, BorshSerialize};
use chrono::Utc;
use enclose::enclose;
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

    // ── Namespaced key→blob store ────────────────────────────────────────────
    //
    // A general scratch store, separate from the (addr, hashin, name) artifact
    // space, used for cross-run plugin state such as filesystem-walk caches
    // (see `crate::engine::walk_cache`). Defaults are no-ops so only the durable
    // backend (and its fronting tiers) need implement it; a no-op backend simply
    // makes those caches always miss (correctness-neutral).
    fn kv_get(&self, _ns: &str, _k: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn kv_list(&self, _ns: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        Ok(Vec::new())
    }
    fn kv_put(&self, _ns: &str, _k: &str, _v: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("not found")]
pub struct NotFoundError;

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
            hartifactcontent::Type::Tar => Box::new(crate::hartifactcontent::tar::TarWalker::new(
                self.reader()?,
            )?),
            #[expect(clippy::unimplemented, reason = "cpio format is not yet implemented")]
            hartifactcontent::Type::Cpio => unimplemented!("cpio is not implemented"),
        })
    }

    fn hashout(&self) -> anyhow::Result<String> {
        Ok(self.hashout.clone())
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
        // `block_or_inline` runs on the current worker via `block_in_place`
        // (multi-thread) or inline (current-thread). Avoids `spawn_blocking`
        // whose JoinHandle wake-up uses tokio's cross-thread waker, which
        // is observed to drop wakeups on macOS under heavy load — see
        // `RCA_MACOS_WAKER.md`.
        crate::process_supervisor::block_or_inline(
            enclose!((cache => local_cache, addr, artifact) move || {
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
            }),
        )
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

        Ok(res_artifacts)
    }

    /// Read and deserialize a group's manifest. `Ok(None)` if it is absent.
    pub(crate) fn read_manifest(
        &self,
        addr: &Addr,
        hashin: &str,
    ) -> anyhow::Result<Option<Manifest>> {
        let sized = match self.local_cache.reader(addr, hashin, MANIFEST_V1) {
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

    pub async fn artifacts_from_local_cache(
        &self,
        _ctoken: &dyn Cancellable,
        def: &LinkedTargetDef,
        hashin: &str,
        outputs: Vec<String>,
    ) -> anyhow::Result<Option<(Vec<CacheArtifact>, Vec<ArtifactMeta>)>> {
        let addr = def.target.addr.clone();
        let hashin = hashin.to_string();
        // See `cache_artifact_locally` for why this is `block_or_inline`
        // and not `spawn_blocking`.
        crate::process_supervisor::block_or_inline(
            enclose!((self.local_cache => local_cache) move || {
                let sized = match local_cache.reader(&addr, &hashin, MANIFEST_V1) {
                    Err(e) if e.is::<NotFoundError>() => return Ok(None),
                    Err(e) => return Err(e),
                    Ok(artifact) => artifact,
                };

                let mut manifest_artifact = sized.reader;
                let mut buf = Vec::with_capacity(sized.size as usize);
                io::Read::read_to_end(&mut manifest_artifact, &mut buf)?;
                let manifest: Manifest = borsh::from_slice(&buf)?;

                let mut results: Vec<CacheArtifact> = vec![];
                let mut result_meta: Vec<ArtifactMeta> = vec![];

                for artifact in manifest.artifacts {
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

                    if !local_cache.exists(&addr, &hashin, artifact.name.as_ref())? {
                        return Ok(None);
                    }

                    results.push(CacheArtifact {
                        addr: addr.clone(),
                        hashin: hashin.clone(),
                        name: artifact.name.clone(),
                        cache: local_cache.clone(),
                        content_type: match artifact.content_type {
                            ManifestArtifactContentType::Tar => hartifactcontent::Type::Tar,
                            ManifestArtifactContentType::Cpio => hartifactcontent::Type::Cpio,
                        },
                        r#type: artifact.r#type,
                        hashout: artifact.hashout,
                        group: artifact.group,
                        size: artifact.size,
                    });
                }

                anyhow::Ok(Some((results, result_meta)))
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use crate::engine::driver::outputartifact;
    use crate::engine::driver::targetdef::{CacheConfig, TargetDef};
    use crate::engine::link::LinkedTargetDef;
    use crate::hasync::StdCancellationToken;
    use crate::htpkg::PkgBuf;
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
