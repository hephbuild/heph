use crate::engine::config_file::{Options, PluginEntry};
use crate::engine::driver::Driver as SDKDriver;
use crate::engine::driver_managed::ManagedDriver as SDKManagedDriver;
use crate::engine::local_cache::LocalCache;
use crate::engine::local_cache_mem::LocalCacheMem;
use crate::engine::local_cache_sqlite::LocalCacheSQLite;
use crate::engine::provider::Provider as SDKProvider;
use crate::engine::request_state::RequestState;
use crate::engine::{driver, provider};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Semaphore;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub root: PathBuf,
    /// Workspace state/cache directory. If empty, defaults to `root/.heph3`.
    pub home_dir: PathBuf,
    pub parallelism: Option<usize>,
    pub mem_cache: MemCacheOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemCacheOptions {
    pub per_entry_bytes: usize,
    /// Total byte budget. `0` disables the in-memory layer entirely.
    pub capacity_bytes: u64,
}

impl Default for MemCacheOptions {
    fn default() -> Self {
        Self {
            per_entry_bytes: 16 * 1024,
            capacity_bytes: 64 * 1024 * 1024,
        }
    }
}

pub type ProviderFactory =
    Box<dyn FnOnce(&Path, &Options) -> anyhow::Result<Box<dyn SDKProvider>> + Send + Sync>;
pub type DriverFactory =
    Box<dyn FnOnce(&Options) -> anyhow::Result<Box<dyn SDKDriver>> + Send + Sync>;
pub type ManagedDriverFactory =
    Box<dyn FnOnce(&Options) -> anyhow::Result<Box<dyn SDKManagedDriver>> + Send + Sync>;

pub struct Engine {
    pub(crate) cfg: Config,
    pub(crate) local_cache: Arc<dyn LocalCache>,

    pub(crate) providers: Vec<Arc<Provider>>,
    pub(crate) providers_by_name: HashMap<String, Arc<Provider>>,
    pub(crate) drivers: Vec<Arc<Driver>>,
    pub(crate) drivers_by_name: HashMap<String, Arc<Driver>>,

    pub requests: Mutex<HashMap<String, Weak<RequestState>>>,
    pub home: PathBuf,
    pub(crate) result_semaphore: Arc<Semaphore>,

    pub(crate) provider_factories: HashMap<String, ProviderFactory>,
    pub(crate) driver_factories: HashMap<String, DriverFactory>,
    pub(crate) managed_driver_factories: HashMap<String, ManagedDriverFactory>,
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
        let home = if cfg.home_dir.as_os_str().is_empty() {
            root.join(".heph3")
        } else {
            cfg.home_dir.clone()
        };

        let parallelism = cfg.parallelism.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });

        let sqlite: Arc<dyn LocalCache> = Arc::new(LocalCacheSQLite::new(
            home.join("cache").join("cache.db"),
            cfg.mem_cache.per_entry_bytes,
        )?);
        let local_cache: Arc<dyn LocalCache> = if cfg.mem_cache.capacity_bytes == 0 {
            sqlite
        } else {
            Arc::new(LocalCacheMem::new(
                sqlite,
                cfg.mem_cache.per_entry_bytes,
                cfg.mem_cache.capacity_bytes,
            ))
        };

        let mut engine = Engine {
            cfg: cfg.clone(),
            home: home.clone(),
            local_cache,
            providers: vec![],
            providers_by_name: HashMap::new(),
            drivers: vec![],
            drivers_by_name: HashMap::new(),
            requests: Mutex::new(HashMap::new()),
            result_semaphore: Arc::new(Semaphore::new(2 * parallelism)),
            provider_factories: HashMap::new(),
            driver_factories: HashMap::new(),
            managed_driver_factories: HashMap::new(),
        };
        engine.register_driver(Box::new(crate::plugingroup::Driver))?;
        engine.register_provider(|_| Box::new(crate::pluginquery::Provider))?;
        Ok(engine)
    }

    /// Cancel every in-flight request's cancellation token. Used to broadcast
    /// graceful shutdown (e.g. on SIGINT). Idempotent.
    pub fn cancel_all_requests(&self) {
        let Ok(requests) = self.requests.lock() else {
            return;
        };
        for weak in requests.values() {
            if let Some(rs) = weak.upgrade() {
                rs.ctoken().cancel();
            }
        }
    }

    pub fn register_managed_driver(
        &mut self,
        driver: Box<dyn SDKManagedDriver>,
    ) -> anyhow::Result<()> {
        let driver = self.new_managed_driver(driver);
        self.register_driver(Box::new(driver))
    }

    pub fn register_driver(&mut self, driver: Box<dyn SDKDriver>) -> anyhow::Result<()> {
        let driver = Arc::new(Driver {
            name: driver.config(driver::ConfigRequest {})?.name,
            driver,
        });

        if self.drivers_by_name.contains_key(&driver.name) {
            return Err(anyhow::anyhow!(
                "driver with name '{}' already registered",
                driver.name
            ));
        }
        self.drivers.push(driver.clone());
        self.drivers_by_name.insert(driver.name.clone(), driver);
        Ok(())
    }

    pub fn register_provider(
        &mut self,
        provider_factory: impl FnOnce(&Path) -> Box<dyn SDKProvider>,
    ) -> anyhow::Result<()> {
        let provider = provider_factory(self.cfg.root.as_path());

        let provider = Arc::new(Provider {
            name: provider.config(provider::ConfigRequest {})?.name,
            provider,
        });

        if self.providers_by_name.contains_key(&provider.name) {
            return Err(anyhow::anyhow!(
                "provider with name '{}' already registered",
                provider.name
            ));
        }
        self.providers.push(provider.clone());
        self.providers_by_name
            .insert(provider.name.clone(), provider);
        Ok(())
    }

    pub fn register_provider_factory(
        &mut self,
        name: &str,
        factory: impl FnOnce(&Path, &Options) -> anyhow::Result<Box<dyn SDKProvider>>
        + Send
        + Sync
        + 'static,
    ) -> anyhow::Result<()> {
        if self.provider_factories.contains_key(name) {
            return Err(anyhow::anyhow!(
                "provider factory '{name}' already registered"
            ));
        }
        self.provider_factories
            .insert(name.to_string(), Box::new(factory));
        Ok(())
    }

    pub fn register_driver_factory(
        &mut self,
        name: &str,
        factory: impl FnOnce(&Options) -> anyhow::Result<Box<dyn SDKDriver>> + Send + Sync + 'static,
    ) -> anyhow::Result<()> {
        if self.driver_factories.contains_key(name)
            || self.managed_driver_factories.contains_key(name)
        {
            return Err(anyhow::anyhow!(
                "driver factory '{name}' already registered"
            ));
        }
        self.driver_factories
            .insert(name.to_string(), Box::new(factory));
        Ok(())
    }

    pub fn register_managed_driver_factory(
        &mut self,
        name: &str,
        factory: impl FnOnce(&Options) -> anyhow::Result<Box<dyn SDKManagedDriver>>
        + Send
        + Sync
        + 'static,
    ) -> anyhow::Result<()> {
        if self.driver_factories.contains_key(name)
            || self.managed_driver_factories.contains_key(name)
        {
            return Err(anyhow::anyhow!(
                "driver factory '{name}' already registered"
            ));
        }
        self.managed_driver_factories
            .insert(name.to_string(), Box::new(factory));
        Ok(())
    }

    /// Instantiates every provider/driver listed in the entries by looking up the
    /// matching factory by name. Errors if any name has no registered factory.
    /// Factories are consumed — calling twice on the same name will error.
    pub fn apply_config(
        &mut self,
        providers: &[PluginEntry],
        drivers: &[PluginEntry],
    ) -> anyhow::Result<()> {
        let root = self.cfg.root.clone();
        for entry in providers {
            let factory = self
                .provider_factories
                .remove(&entry.name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider '{}'", entry.name))?;
            let provider = factory(&root, &entry.options)?;
            let resolved_name = provider.config(provider::ConfigRequest {})?.name;
            if resolved_name != entry.name {
                return Err(anyhow::anyhow!(
                    "provider '{}' reported name '{}'; config/factory name mismatch",
                    entry.name,
                    resolved_name
                ));
            }
            self.register_provider(|_| provider)?;
        }
        for entry in drivers {
            if let Some(factory) = self.driver_factories.remove(&entry.name) {
                let driver = factory(&entry.options)?;
                let resolved_name = driver.config(driver::ConfigRequest {})?.name;
                if resolved_name != entry.name {
                    return Err(anyhow::anyhow!(
                        "driver '{}' reported name '{}'; config/factory name mismatch",
                        entry.name,
                        resolved_name
                    ));
                }
                self.register_driver(driver)?;
            } else if let Some(factory) = self.managed_driver_factories.remove(&entry.name) {
                let driver = factory(&entry.options)?;
                self.register_managed_driver(driver)?;
            } else {
                return Err(anyhow::anyhow!("unknown driver '{}'", entry.name));
            }
        }
        Ok(())
    }
}
