use std::sync::Arc;
use std::collections::HashMap;
use std::path::PathBuf;
use crate::engine::driver::Driver as SDKDriver;
use crate::engine::local_cache::LocalCache;
use crate::engine::local_cache_fs::LocalCacheFS;
use crate::engine::provider::Provider as SDKProvider;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub root: PathBuf
}

pub struct Engine {
    pub(crate) cfg: Config,
    pub(crate) local_cache: Box<dyn LocalCache>,

    pub(crate) providers: Vec<Provider>,
    pub(crate) providers_by_name: HashMap<String, Arc<Provider>>,
    pub(crate) drivers: Vec<Driver>,
    pub(crate) drivers_by_name: HashMap<String, Arc<Driver>>,
}

pub struct Provider {
    pub name: String,
    pub provider: Box<dyn SDKProvider>,
}

pub struct Driver {
    pub name: String,
    pub driver: Box<dyn SDKDriver>,
}

impl Engine {
    pub fn new(cfg: Config) -> anyhow::Result<Arc<Engine>> {
        let root = cfg.root.clone();

        let providers: Vec<Provider> = vec![];
        let providers_by_name: HashMap<String, Arc<Provider>> = HashMap::new();
        let drivers: Vec<Driver> = vec![];
        let drivers_by_name: HashMap<String, Arc<Driver>> = HashMap::new();

        Ok(Arc::new(Engine {
            cfg: cfg.clone(),
            local_cache: Box::new(LocalCacheFS::new(root.join(".heph3").join("cache"))?),
            providers,
            providers_by_name,
            drivers,
            drivers_by_name,
        }))
    }
}