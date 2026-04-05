use std::collections::HashMap;
use std::sync::Mutex;
use futures::future::BoxFuture;
use crate::engine::provider::{ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider as EProvider, State, TargetSpec};
use crate::engine::provider::GetError::NotFound;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;

pub struct RequestState {

}

pub struct Provider {
    pub root: std::path::PathBuf,
    pub requests: Mutex<HashMap<String, RequestState>>,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            root: std::path::PathBuf::from("/"),
            requests: Mutex::new(HashMap::new()),
        }
    }
}

impl EProvider for Provider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse{
            name: "buildfile".to_string(),
        })
    }

    fn list<'a>(&'a self, req: ListRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item=ListResponse>>>> {
        Box::pin(async move {
            let res = self.run_pkg(&req.package)?;

            let items: Vec<ListResponse> = res.targets.into_iter().map(|p| {
                ListResponse{
                    addr: Addr{
                        package: req.package.clone(),
                        name: p.name,
                        args: Default::default(),
                    },
                }
            }).collect();

            Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = ListResponse>>)
        })
    }

    fn get<'a>(&'a self, req: GetRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            let res = self.run_pkg(&req.addr.package).map_err(|e: anyhow::Error| GetError::Other(e))?;

            for p in res.targets {
                if p.name == req.addr.name {
                    return Ok(GetResponse{
                        target_spec: TargetSpec{
                            addr: req.addr.clone(),
                            driver: p.driver,
                            config: p.config,
                            labels: p.labels,
                            transitive: Default::default(),
                        },
                    })
                }
            }

            Err(NotFound)
        })
    }

    fn probe<'a>(&'a self, req: ProbeRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move {
            let res = self.run_pkg(&req.package)?;

            Ok(ProbeResponse{
                states: res.states.into_iter().map(|_p| {
                    State{
                        package: req.package.clone(),
                        provider: "buildfile".to_string(), // TODO: move into engine
                        state: Default::default(),
                    }
                }).collect(),
            })
        })
    }
}

