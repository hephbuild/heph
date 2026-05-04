use std::collections::HashMap;
use futures::future::BoxFuture;
use crate::engine::driver::sandbox::Sandbox;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::TargetSpecValue;

pub struct ConfigRequest {}
pub struct ConfigResponse {
    pub name: String,
}

pub struct ListRequest {
    pub request_id: String,
    pub package: PkgBuf,
}
pub struct ListResponse {
    pub addr: Addr
}

pub struct ListPackagesRequest {
    pub prefix: PkgBuf,
}
#[derive(Clone)]
pub struct ListPackageResponse {
    pub pkg: PkgBuf,
}

pub struct State {
    pub package: PkgBuf,
    pub provider: String,
    pub state: HashMap<String, String>,
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
    pub package: PkgBuf,
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
    fn list<'a>(&'a self, req: ListRequest, ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>>;
    fn list_packages<'a>(&'a self, req: ListPackagesRequest, ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>>;
    fn get<'a>(&'a self, req: GetRequest, ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, Result<GetResponse, GetError>>;
    fn probe<'a>(&'a self, req: ProbeRequest, ctoken: &'a (dyn Cancellable + Send + Sync)) -> BoxFuture<'a, anyhow::Result<ProbeResponse>>;
}

