use crate::hartifactcontent::{Content as HContent, Type, WalkEntry};
use std::fs::File;
use std::io;
use crate::engine::driver::outputartifact::{Content, OutputArtifact};
use crate::engine::Engine;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use std::sync::Arc;
use crate::engine::driver::outputartifact;
use crate::hartifactcontent::tar::TarPacker;

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
    pub content_type: Type,
}

impl HContent for CacheArtifact {
    fn reader(&self) -> anyhow::Result<Box<dyn io::Read>> {
        self.cache.reader(&self.addr, &self.hashin, &self.name)
    }

    fn content_type(&self) -> anyhow::Result<Type> {
        Ok(self.content_type.clone())
    }

    fn walk(&self) -> anyhow::Result<Box<dyn Iterator<Item=anyhow::Result<WalkEntry>> + '_>> {
        Ok(match &self.content_type()? {
            Type::Tar => Box::new(crate::hartifactcontent::tar::TarWalker::new(self.reader()?)?),
            Type::Cpio => unimplemented!("cpio is not implemented"),
        })
    }
}

impl Engine {
    pub async fn cache_artifact_locally(&self, _ctoken: &dyn Cancellable, addr: &Addr, hashin: &str, artifact: &OutputArtifact) -> anyhow::Result<CacheArtifact> {
        let name = format!("{}_{}", match artifact.r#type{
            outputartifact::Type::Output => "out",
            outputartifact::Type::Log => "log",
            outputartifact::Type::SupportFile => "support",
            _ => panic!("unsupported artifact {:?}", artifact.r#type),
        }, artifact.name);
        let mut writer = self.local_cache.writer(addr, hashin, &name)?;

        let mut src: Box<dyn io::Read> = match &artifact.content {
            Content::Raw(raw) => {
                let mut p = TarPacker::new();

                p.create_raw(raw.data.clone(), raw.path.clone(), raw.x);

                let mut buf = Vec::new();
                p.pack(&mut buf)?;

                Box::new(io::Cursor::new(buf))
            },
            Content::File(file) => {
                let mut p = TarPacker::new();

                p.create_file(file.source_path.clone(), file.out_path.clone());

                let mut buf = Vec::new();
                p.pack(&mut buf)?;

                Box::new(io::Cursor::new(buf))
            },
            Content::TarPath(path) => Box::new(File::open(path)?),
            Content::CpioPath(path) => Box::new(File::open(path)?),
        };
        let content_type = match &artifact.content {
            Content::Raw(_) => Type::Tar,
            Content::File(_) => Type::Tar,
            Content::TarPath(_) => Type::Tar,
            Content::CpioPath(_) => Type::Cpio,
        };

        io::copy(&mut src, &mut writer)?;

        Ok(CacheArtifact{
            addr: addr.clone(),
            hashin: hashin.to_string(),
            name,
            cache: self.local_cache.clone(),
            content_type,
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
