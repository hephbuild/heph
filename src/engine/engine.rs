use std::path::PathBuf;
use crate::engine::local_cache::LocalCache;
use crate::engine::local_cache_fs::LocalCacheFS;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub root: PathBuf
}

pub struct Engine {
    // cfg: Config,
    pub local_cache: Box<dyn LocalCache>,
}

impl Engine {
    pub fn new(cfg: Config) -> anyhow::Result<Engine> {
        let root = cfg.root.clone();

        Ok(Engine {
            // cfg,
            local_cache: Box::new(LocalCacheFS::new(root.join(".heph3").join("cache"))?)
        })
    }
}