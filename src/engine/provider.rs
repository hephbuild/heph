use std::any::Any;
use std::collections::HashMap;
use crate::engine::driver::sandbox::Sandbox;
use crate::hasync;
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

pub struct TargetSpec {
    pub addr: Addr,
    pub driver: String,
    pub config: HashMap<String, Box<dyn Any>>,
    pub labels: Vec<String>,
    pub transitive: Sandbox,
}

pub struct GetRequest<'a> {
    pub request_id: &'a String,
    pub addr: Addr,
    pub states: Vec<State>,
}
pub struct GetResponse {
    pub target_spec: TargetSpec,
}

pub struct ProbeRequest<'a> {
    pub request_id: &'a String,
    pub package: String,
}
pub struct ProbeResponse {
    pub states: Vec<State>,
}

pub enum GetError {
    NotFound,
    Other(anyhow::Error),
}

pub trait Provider {
    fn config(&self, req: ConfigRequest, ctoken: &dyn hasync::Cancellable) -> anyhow::Result<ConfigResponse>;
    fn list(&self, req: ListRequest, ctoken: &dyn hasync::Cancellable) -> anyhow::Result<Box<dyn Iterator<Item = ListResponse>>>;
    fn get(&self, req: GetRequest, ctoken: &dyn hasync::Cancellable) -> Result<GetResponse, GetError>;
    fn probe(&self, req: ProbeRequest, ctoken: &dyn hasync::Cancellable) -> anyhow::Result<ProbeResponse>;
}
