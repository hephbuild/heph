use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use futures::future::BoxFuture;
use crate::engine::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse,
    ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse,
    ProbeRequest, ProbeResponse, Provider as EProvider, TargetSpec, TargetSpecValue,
};
use crate::hasync::Cancellable;
use crate::htaddr::parse_addr;
use crate::htpkg::PkgBuf;

pub struct Target {
    pub addr: String,
    pub driver: String,
    pub run: Option<String>,
    pub out: Option<String>,
    pub labels: Vec<String>,
}

pub struct Provider {
    targets: Vec<TargetSpec>,
    packages: OnceLock<Vec<PkgBuf>>,
}

impl Provider {
    pub fn new(targets: Vec<Target>) -> anyhow::Result<Self> {
        let specs = targets
            .into_iter()
            .map(|t| -> anyhow::Result<TargetSpec> {
                let mut config = HashMap::new();
                if let Some(run) = t.run {
                    config.insert("run".to_string(), TargetSpecValue::String(run));
                }
                if let Some(out) = t.out {
                    config.insert("out".to_string(), TargetSpecValue::String(out));
                }
                Ok(TargetSpec {
                    addr: parse_addr(&t.addr)?,
                    driver: t.driver,
                    config,
                    labels: t.labels,
                    transitive: Default::default(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(Self {
            targets: specs,
            packages: OnceLock::new(),
        })
    }
}

impl EProvider for Provider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "pluginstatictarget".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>> {
        Box::pin(async move {
            let items: Vec<anyhow::Result<ListResponse>> = self
                .targets
                .iter()
                .filter(|t| t.addr.package == req.package)
                .map(|t| Ok(ListResponse { addr: t.addr.clone() }))
                .collect();
            Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>> {
        let pkgs = self.packages.get_or_init(|| {
            let mut seen = HashSet::new();
            self.targets
                .iter()
                .filter(|t| seen.insert(t.addr.package.clone()))
                .map(|t| t.addr.package.clone())
                .collect()
        });

        Box::pin(async move {
            let items: Vec<anyhow::Result<ListPackageResponse>> = pkgs
                .iter()
                .cloned()
                .map(|pkg| Ok(ListPackageResponse { pkg }))
                .collect();
            Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            for t in &self.targets {
                if t.addr == req.addr {
                    return Ok(GetResponse { target_spec: t.clone() });
                }
            }
            Err(GetError::NotFound)
        })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
    }
}
