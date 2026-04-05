use std::collections::HashMap;
use futures::future::BoxFuture;
use crate::engine::driver::sandbox::Sandbox;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;

pub struct ConfigRequest {}
pub struct ConfigResponse {
    pub name: String,
}

pub struct ListRequest {
    pub request_id: String,
    pub package: String,

}
pub struct ListResponse {
    pub addr: Addr
}

pub struct State {
    pub package: String,
    pub provider: String,
    pub state: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub enum TargetSpecValue {
    String(String),
    Bool(bool),
    Float(f64),
    Int(i64),
    Uint(u64),
    Null(),
    Map(HashMap<String, TargetSpecValue>),
    List(Vec<TargetSpecValue>),
}

#[derive(Default, Clone)]
pub struct TargetSpec {
    pub addr: Addr,
    pub driver: String,
    pub config: HashMap<String, TargetSpecValue>,
    pub labels: Vec<String>,
    pub transitive: Sandbox,
}

pub struct GetRequest {
    pub request_id: String,
    pub addr: Addr,
    pub states: Vec<State>,
}
pub struct GetResponse {
    pub target_spec: TargetSpec,
}

pub struct ProbeRequest {
    pub request_id: String,
    pub package: String,
}
pub struct ProbeResponse {
    pub states: Vec<State>,
}

pub enum GetError {
    NotFound,
    Other(anyhow::Error),
}

pub trait Provider: Send + Sync {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse>;
    fn list<'a>(&'a self, req: ListRequest, ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = ListResponse>>>>;
    fn get<'a>(&'a self, req: GetRequest, ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, Result<GetResponse, GetError>>;
    fn probe<'a>(&'a self, req: ProbeRequest, ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<ProbeResponse>>;
}

pub struct StaticProvider {
    pub targets: Vec<TargetSpec>,
}

impl Provider for StaticProvider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse{
            name: "static".to_string(),
        })
    }

    fn list<'a>(&'a self, _req: ListRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item=ListResponse>>>> {
        Box::pin(async move {
            let items: Vec<ListResponse> = self.targets.iter().map(|t| ListResponse{addr: t.addr.clone()}).collect();

            Ok(Box::new(items.into_iter()) as Box<dyn Iterator<Item = ListResponse>>)
        })
    }

    fn get<'a>(&'a self, req: GetRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            for t in &self.targets {
                if t.addr == req.addr {
                    return Ok(GetResponse{target_spec: t.clone()});
                }
            }

            Err(GetError::NotFound)
        })
    }

    fn probe<'a>(&'a self, _req: ProbeRequest, _ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move {
            Ok(ProbeResponse{states: vec![]})
        })
    }
}
