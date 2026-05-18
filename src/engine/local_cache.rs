use crate::engine::Engine;
use crate::engine::driver::outputartifact;
use crate::engine::link::LinkedTargetDef;
use crate::engine::result::ArtifactMeta;
use crate::hartifactcontent;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use chrono::Utc;
use enclose::enclose;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ManifestArtifactContentType {
    #[serde(rename = "application/x-tar")]
    Tar,
    #[serde(rename = "application/x-cpio")]
    Cpio,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ManifestArtifactEncoding {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "gzip")]
    Gzip,
    #[serde(rename = "ztsd")]
    Zstd,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ManifestArtifactType {
    #[serde(rename = "output")]
    Output,
    #[serde(rename = "log")]
    Log,
    #[serde(rename = "support_file")]
    SupportFile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestArtifact {
    pub hashout: String,
    pub group: String,
    pub name: String,
    pub size: u64,
    pub r#type: ManifestArtifactType,
    pub content_type: ManifestArtifactContentType,
    pub encoding: ManifestArtifactEncoding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub target: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub hashin: String,
    pub artifacts: Vec<ManifestArtifact>,
}

pub trait LocalCache: Send + Sync {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Box<dyn io::Read>>;
    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Box<dyn io::Write>>;
    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool>;
    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<()>;
}

#[derive(Debug, thiserror::Error)]
#[error("not found")]
pub struct NotFoundError;

const MANIFEST_V1_JSON: &str = "manifest-v1.json";

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
        self.cache.reader(&self.addr, &self.hashin, &self.name)
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
        tokio::task::spawn_blocking(
            enclose!((self.local_cache => local_cache, addr, artifact) move || {
                let type_prefix = match artifact.r#type {
                    outputartifact::Type::Output => "out",
                    outputartifact::Type::Log => "log",
                    outputartifact::Type::SupportFile => "support",
                };

                let (size, content_type, name) = match &artifact.content {
                    outputartifact::Content::Raw(raw) => {
                        let name = format!("{}_{}.tar", type_prefix, artifact.name);
                        let mut cw =
                            CountingWriter::new(local_cache.writer(&addr, &hashin, &name)?);
                        let mut p = hartifactcontent::tar::TarPacker::new();
                        p.create_raw(raw.data.clone(), raw.path.clone(), raw.x);
                        p.pack(&mut cw)?;
                        (cw.bytes_written(), hartifactcontent::Type::Tar, name)
                    }
                    outputartifact::Content::File(file) => {
                        let name = format!("{}_{}.tar", type_prefix, artifact.name);
                        let mut cw =
                            CountingWriter::new(local_cache.writer(&addr, &hashin, &name)?);
                        let mut p = hartifactcontent::tar::TarPacker::new();
                        p.create_file(file.source_path.clone(), file.out_path.clone());
                        p.pack(&mut cw)?;
                        (cw.bytes_written(), hartifactcontent::Type::Tar, name)
                    }
                    outputartifact::Content::TarPath(path) => {
                        let name = format!("{}_{}", type_prefix, artifact.name);
                        let mut f = File::open(path)?;
                        let size = f.metadata()?.size();
                        io::copy(&mut f, &mut local_cache.writer(&addr, &hashin, &name)?)?;
                        (size, hartifactcontent::Type::Tar, name)
                    }
                    outputartifact::Content::CpioPath(path) => {
                        let name = format!("{}_{}", type_prefix, artifact.name);
                        let mut f = File::open(path)?;
                        let size = f.metadata()?.size();
                        io::copy(&mut f, &mut local_cache.writer(&addr, &hashin, &name)?)?;
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
        .await
        .map_err(|e| anyhow::anyhow!("cache_artifact_locally task panicked: {e}"))?
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
            let (cached_artifact, manifest_artifact) = self
                .cache_artifact_locally(ctoken, addr, &key, &artifact)
                .await?;
            res_artifacts.push(cached_artifact);
            manifest_artifacts.push(manifest_artifact);
        }

        let manifest = Manifest {
            version: "1.0.0".to_string(),
            target: addr.format(),
            created_at: Utc::now(),
            hashin: hashin.to_string(),
            artifacts: manifest_artifacts,
        };

        let manifest_writer = self.local_cache.writer(addr, &key, MANIFEST_V1_JSON)?;
        serde_json::to_writer(manifest_writer, &manifest)?;

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
        tokio::task::spawn_blocking(enclose!((self.local_cache => local_cache) move || {
            let manifest_artifact = match local_cache.reader(&addr, &hashin, MANIFEST_V1_JSON) {
                Err(e) if e.is::<NotFoundError>() => return Ok(None),
                Err(e) => return Err(e),
                Ok(artifact) => artifact,
            };

            let manifest: Manifest = serde_json::from_reader(manifest_artifact)?;

            let mut results: Vec<CacheArtifact> = vec![];
            let mut result_meta: Vec<ArtifactMeta> = vec![];

            for artifact in manifest.artifacts {
                if artifact.r#type != ManifestArtifactType::Output {
                    continue;
                }

                result_meta.push(ArtifactMeta {
                    hashout: artifact.hashout.clone(),
                });

                if !outputs.contains(&artifact.group) {
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
        }))
        .await
        .map_err(|e| anyhow::anyhow!("artifacts_from_local_cache task panicked: {e}"))?
    }
}
