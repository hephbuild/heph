use std::sync::Arc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Weak;
use crate::engine::driver::Driver as SDKDriver;
use crate::engine::driver::sandbox::Sandbox;
use crate::engine::local_cache::LocalCache;
use crate::engine::local_cache_fs::LocalCacheFS;
use crate::engine::provider::{Provider as SDKProvider, StaticProvider, TargetSpec};
use crate::htaddr::Addr;
use crate::pluginexec;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub root: PathBuf
}

pub struct Engine {
    pub(crate) cfg: Config,
    pub(crate) local_cache: Box<dyn LocalCache>,

    pub(crate) providers: Vec<Arc<Provider>>,
    pub(crate) providers_by_name: HashMap<String, Arc<Provider>>,
    pub(crate) drivers: Vec<Arc<Driver>>,
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

        let mut providers: Vec<Arc<Provider>> = vec![];
        providers.push(Arc::new(Provider{
            name: "static".to_string(),
            provider: Box::new(StaticProvider {
                targets: vec![
                    TargetSpec {
                        addr: Addr{
                            package: "some".to_string(),
                            name: "t".to_string(),
                            args: Default::default(),
                        },
                        driver: "exec".to_string(),
                        config: Default::default(),
                        labels: vec![],
                        transitive: Sandbox::default(),
                    },
                ],
            }),
        }));

        let providers_by_name = HashMap::from_iter(providers
            .iter()
            .map(|p| (p.name.clone(), p.clone())));

        let mut drivers: Vec<Arc<Driver>> = vec![];
        drivers.push(Arc::new(Driver {
            name: "exec".to_string(),
            driver: Box::new(pluginexec::Driver::new()),
        }));

        let drivers_by_name = HashMap::from_iter(drivers
            .iter()
            .map(|p| (p.name.clone(), p.clone())));

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
