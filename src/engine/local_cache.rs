use crate::engine::result::Artifact;
use std::fs::File;
use std::io;
use crate::engine::driver::outputartifact::OutputArtifact;
use crate::hartifactcontent::Content;
use crate::engine::Engine;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use std::sync::Arc;

pub trait LocalCache: Send + Sync {
    fn reader(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Box<dyn io::Read>>;
    fn writer(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<Box<dyn io::Write>>;
    fn exists(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<bool>;
    fn delete(&self, addr: &Addr, hashin: &str, name: &str) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct CacheArtifact {
    pub addr: Addr,
    pub hashin: String,
    pub name: String,
    pub cache: Arc<dyn LocalCache>,
}

impl Artifact for CacheArtifact {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>> {
        self.cache.reader(&self.addr, &self.hashin, &self.name)
    }
}

impl Engine {
    pub async fn cache_artifact_locally(&self, _ctoken: &dyn Cancellable, addr: &Addr, hashin: &str, artifact: &OutputArtifact) -> anyhow::Result<CacheArtifact> {
        let mut writer = self.local_cache.writer(addr, hashin, &artifact.name)?;

        let mut src: Box<dyn io::Read> = match &artifact.content {
            Content::Raw(raw) => Box::new(io::Cursor::new(raw.data.clone())),
            Content::File(file) => Box::new(File::open(&file.source_path)?),
            Content::TarPath(path) => Box::new(File::open(path)?),
            Content::CpioPath(path) => Box::new(File::open(path)?),
        };

        io::copy(&mut src, &mut writer)?;

        Ok(CacheArtifact{
            addr: addr.clone(),
            hashin: hashin.to_string(),
            name: artifact.name.clone(),
            cache: self.local_cache.clone(),
        })
    }

    pub async fn cache_locally(&self, ctoken: &dyn Cancellable, addr: &Addr, hashin: &str, artifacts: Vec<OutputArtifact>) -> anyhow::Result<Vec<CacheArtifact>> {
        let mut results = vec![];
        for artifact in artifacts {
            results.push(self.cache_artifact_locally(ctoken, addr, hashin, &artifact).await?);
        }

        Ok(results)
    }
}
