use std::sync::{Arc, Mutex, Weak};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use crate::engine::driver::Driver as SDKDriver;
use crate::engine::local_cache::LocalCache;
use crate::engine::local_cache_fs::LocalCacheFS;
use crate::engine::{driver, provider};
use crate::engine::provider::{Provider as SDKProvider};
use crate::engine::request_state::RequestState;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub root: PathBuf
}

pub struct Engine {
    pub(crate) cfg: Config,
    pub(crate) local_cache: Arc<dyn LocalCache>,

    pub(crate) providers: Vec<Arc<Provider>>,
    pub(crate) providers_by_name: HashMap<String, Arc<Provider>>,
    pub(crate) drivers: Vec<Arc<Driver>>,
    pub(crate) drivers_by_name: HashMap<String, Arc<Driver>>,

    pub requests: Mutex<HashMap<String, Weak<RequestState>>>,
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
    pub fn new(cfg: Config) -> anyhow::Result<Engine> {
        let root = cfg.root.clone();

        Ok(Engine {
            cfg: cfg.clone(),
            local_cache: Arc::new(LocalCacheFS::new(root.join(".heph3").join("cache"))?),
            providers: vec![],
            providers_by_name: HashMap::new(),
            drivers: vec![],
            drivers_by_name: HashMap::new(),
            requests: Mutex::new(HashMap::new()),
        })
    }

    pub fn register_driver(&mut self, driver: Box<dyn SDKDriver>) -> anyhow::Result<()> {
        let driver = Arc::new(Driver {
            name: driver.config(driver::ConfigRequest{})?.name,
            driver,
        });

        if self.drivers_by_name.contains_key(&driver.name) {
            return Err(anyhow::anyhow!("driver with name '{}' already registered", driver.name));
        }
        self.drivers.push(driver.clone());
        self.drivers_by_name.insert(driver.name.clone(), driver);
        Ok(())
    }

    pub fn register_provider(&mut self, provider_factory: fn(root: &Path) -> Box<dyn SDKProvider>) -> anyhow::Result<()> {
        let provider = provider_factory(self.cfg.root.as_path());

        let provider = Arc::new(Provider {
            name: provider.config(provider::ConfigRequest{})?.name,
            provider,
        });

        if self.providers_by_name.contains_key(&provider.name) {
            return Err(anyhow::anyhow!("provider with name '{}' already registered", provider.name));
        }
        self.providers.push(provider.clone());
        self.providers_by_name.insert(provider.name.clone(), provider);
        Ok(())
    }
}
