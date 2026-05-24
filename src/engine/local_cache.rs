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

pub trait LocalCache: Send + Sync {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<SizedReader>;
    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Box<dyn io::Write>>;
    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool>;
    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<()>;
}

#[derive(Debug, thiserror::Error)]
#[error("not found")]
pub struct NotFoundError;

const MANIFEST_V1: &str = "manifest-v1.borsh";

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
}

impl Engine {
    pub async fn cache_artifact_locally(
        &self,
        _ctoken: &dyn Cancellable,
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
            enclose!((self.local_cache => local_cache, addr, artifact) move || {
                let type_prefix = match artifact.r#type {
                    outputartifact::Type::Output => "out",
                    outputartifact::Type::Log => "log",
                    outputartifact::Type::SupportFile => "support",
                };

                let (size, content_type, name) = match &artifact.content {
                    outputartifact::Content::Raw(raw) => {
                        let name = format!("{}_{}.tar", type_prefix, artifact.name);
                        let mut cw = CountingWriter::new(
                            local_cache.writer(&addr, &hashin, &name).with_context(|| {
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
                            local_cache.writer(&addr, &hashin, &name).with_context(|| {
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
                        let mut w = local_cache
                            .writer(&addr, &hashin, &name)
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
                        let mut w = local_cache
                            .writer(&addr, &hashin, &name)
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

        for artifact in artifacts {
            let artifact_name = artifact.name.clone();
            let (cached_artifact, manifest_artifact) = self
                .cache_artifact_locally(ctoken, addr, &key, &artifact)
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

        let mut manifest_writer = self
            .local_cache
            .writer(addr, &key, MANIFEST_V1)
            .with_context(|| format!("open manifest writer for {addr}"))?;
        borsh::to_writer(&mut manifest_writer, &manifest)
            .with_context(|| format!("write manifest for {addr}"))?;

        Ok(res_artifacts)
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
                    });
                }

                anyhow::Ok(Some((results, result_meta)))
            }),
        )
    }
}
