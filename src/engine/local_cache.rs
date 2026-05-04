use crate::hartifactcontent;
use std::fs::File;
use std::{io, time};
use std::os::unix::fs::MetadataExt;
use crate::engine::Engine;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use std::sync::Arc;
use chrono::Utc;
use crate::engine::driver::outputartifact;
use serde::{Deserialize, Serialize};
use crate::engine::link::LinkedTargetDef;
use crate::engine::result::ArtifactMeta;

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

    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item=anyhow::Result<hartifactcontent::WalkEntry>> + '_>> {
        Ok(match &self.content_type {
            hartifactcontent::Type::Tar => Box::new(crate::hartifactcontent::tar::TarWalker::new(self.reader()?)?),
            hartifactcontent::Type::Cpio => unimplemented!("cpio is not implemented"),
        })
    }

    fn hashout(&self) -> anyhow::Result<String> {
        Ok(self.hashout.clone())
    }
}

impl Engine {
    pub async fn cache_artifact_locally(&self, _ctoken: &dyn Cancellable, addr: &Addr, hashin: &str, artifact: &outputartifact::OutputArtifact) -> anyhow::Result<(CacheArtifact, ManifestArtifact)> {
        let (mut src, size, content_type, name_suffix): (Box<dyn io::Read>, u64, hartifactcontent::Type, &str) = match &artifact.content {
            outputartifact::Content::Raw(raw) => {
                let mut p = hartifactcontent::tar::TarPacker::new();

                p.create_raw(raw.data.clone(), raw.path.clone(), raw.x);

                let mut buf = Vec::new();
                p.pack(&mut buf)?;
                let len = buf.len();

                (Box::new(io::Cursor::new(buf)), len as u64, hartifactcontent::Type::Tar, ".tar")
            },
            outputartifact::Content::File(file) => {
                let mut p = hartifactcontent::tar::TarPacker::new();

                p.create_file(file.source_path.clone(), file.out_path.clone());

                let mut buf = Vec::new();
                p.pack(&mut buf)?;
                let len = buf.len();

                (Box::new(io::Cursor::new(buf)), len as u64, hartifactcontent::Type::Tar, ".tar")
            },
            outputartifact::Content::TarPath(path) => (Box::new(File::open(path)?), File::open(path)?.metadata()?.size(), hartifactcontent::Type::Tar, ""),
            outputartifact::Content::CpioPath(path) => (Box::new(File::open(path)?), File::open(path)?.metadata()?.size(), hartifactcontent::Type::Cpio, ""),
        };

        let name = format!(
            "{}_{}{}",
            match artifact.r#type{
                outputartifact::Type::Output => "out",
                outputartifact::Type::Log => "log",
                outputartifact::Type::SupportFile => "support",
            },
            artifact.name,
            name_suffix
        );

        let mut writer = self.local_cache.writer(addr, hashin, &name)?;
        io::copy(&mut src, &mut writer)?;

        Ok((CacheArtifact{
            addr: addr.clone(),
            hashin: hashin.to_string(),
            name: name.clone(),
            cache: self.local_cache.clone(),
            hashout: artifact.hashout.clone(),
            content_type,
            group: artifact.group.clone(),
            r#type: match artifact.r#type {
                outputartifact::Type::Output => ManifestArtifactType::Output,
                outputartifact::Type::Log => ManifestArtifactType::Log,
                outputartifact::Type::SupportFile => ManifestArtifactType::SupportFile,
            }
        }, ManifestArtifact{
            hashout: artifact.hashout.clone(),
            group: artifact.group.clone(),
            name: name.clone(),
            size,
            r#type: match artifact.r#type {
                outputartifact::Type::Output => ManifestArtifactType::Output,
                outputartifact::Type::Log => ManifestArtifactType::Log,
                outputartifact::Type::SupportFile => ManifestArtifactType::SupportFile,
            },
            content_type: match content_type {
                hartifactcontent::Type::Tar => ManifestArtifactContentType::Tar,
                hartifactcontent::Type::Cpio => ManifestArtifactContentType::Cpio,
            },
            encoding: ManifestArtifactEncoding::None,
        }))
    }

    pub async fn cache_locally(&self, ctoken: &dyn Cancellable, addr: &Addr, hashin: &str, artifacts: Vec<outputartifact::OutputArtifact>, tmp: bool) -> anyhow::Result<Vec<CacheArtifact>> {
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
            let (cached_artifact, manifest_artifact) = self.cache_artifact_locally(ctoken, addr, &key, &artifact).await?;
            res_artifacts.push(cached_artifact);
            manifest_artifacts.push(manifest_artifact);
        }

        let manifest = Manifest{
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

    pub async fn artifacts_from_local_cache(&self, _ctoken: &dyn Cancellable, def: &LinkedTargetDef, hashin: &str, outputs: Vec<String>) -> anyhow::Result<Option<(Vec<CacheArtifact>, Vec<ArtifactMeta>)>> {
        let manifest_artifact = match self.local_cache.reader(&def.target.addr, hashin, MANIFEST_V1_JSON) {
            Err(e) if e.is::<NotFoundError>() => return Ok(None),
            Err(e) => return Err(e),
            Ok(artifact) => artifact,
        };

        let manifest: Manifest = serde_json::from_reader(manifest_artifact)?;

        let mut results: Vec<CacheArtifact> = vec![];
        let mut result_meta: Vec<ArtifactMeta> = vec![];

        for artifact in manifest.artifacts {
            if artifact.r#type != ManifestArtifactType::Output {
                continue
            }

            result_meta.push(ArtifactMeta{ hashout: artifact.hashout.clone() });

            if !outputs.contains(&artifact.group) { continue }

            if !self.local_cache.exists(&def.target.addr, hashin, artifact.name.as_ref())? {
                return Ok(None)
            }

            results.push(CacheArtifact{
                addr: def.target.addr.clone(),
                hashin: hashin.to_string(),
                name: artifact.name.clone(),
                cache: self.local_cache.clone(),
                content_type: match artifact.content_type {
                    ManifestArtifactContentType::Tar => hartifactcontent::Type::Tar,
                    ManifestArtifactContentType::Cpio => hartifactcontent::Type::Cpio,
                },
                r#type: artifact.r#type,
                hashout: artifact.hashout,
                group: artifact.group,
            });
        }

        Ok(Some((results, result_meta)))
    }
}
