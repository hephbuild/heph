use crate::engine::driver::sandbox::Sandbox;
use crate::engine::result::EResult;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::loosespecparser::TargetSpecValue;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

pub struct ConfigRequest {}
pub struct ConfigResponse {
    pub name: String,
}

pub struct ListRequest {
    pub request_id: String,
    pub package: PkgBuf,
}
pub struct ListResponse {
    pub addr: Addr,
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

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct TargetSpec {
    pub addr: Addr,
    pub driver: String,
    pub config: HashMap<String, TargetSpecValue>,
    pub labels: Vec<String>,
    pub transitive: Sandbox,
}

pub trait ProviderExecutor: Send + Sync {
    /// Resolve a target's result.
    ///
    /// Each call routes through `Engine::result_addr`, which adds a
    /// `parent → addr` edge to the request's `DepDag` before any await. That
    /// edge is the engine's sole synchronous cycle check — `daggy` returns
    /// `WouldCycle`, surfaced as `CycleError`, the moment a closing edge is
    /// inserted.
    ///
    /// **Do not wrap this call in a provider-internal memoizer keyed on the
    /// `addr`.** A waiter that hits the cache would skip this call, never
    /// register its dep edge, and let a real target-dep cycle silently turn
    /// into a memoizer deadlock instead of a typed `CycleError`. Cache the
    /// parsed/derived output if needed; never the executor call itself.
    fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>>;
    /// Resolve all targets matching `m`. `extra_skip` is unioned with the
    /// request's `skip_providers` for this iteration only — used to keep a
    /// provider-emitted target from re-entering its own provider while
    /// resolving query inputs (see `rewrite_query_inputs`).
    ///
    /// Same caveat as `result`: each call resolves matched addrs through
    /// `Engine::get_spec`, which registers `parent → addr` in the `DepDag`.
    /// Do not memoize this on the matcher in a provider — waiters would bypass
    /// the dep registration and a target-dep cycle would hide as a deadlock.
    fn query<'a>(
        &'a self,
        m: &'a Matcher,
        extra_skip: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>>;
}

pub struct GetRequest {
    pub request_id: String,
    pub addr: Addr,
    pub states: Vec<State>,
    pub executor: Arc<dyn ProviderExecutor>,
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

impl std::fmt::Debug for GetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GetError::NotFound => write!(f, "GetError::NotFound"),
            GetError::Other(e) => write!(f, "GetError::Other({:#})", e),
        }
    }
}

pub trait Provider: Send + Sync {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse>;
    fn list<'a>(
        &'a self,
        req: ListRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>>>>>;
    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>>>>>;
    fn get<'a>(
        &'a self,
        req: GetRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>>;
    fn probe<'a>(
        &'a self,
        req: ProbeRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>>;
}
