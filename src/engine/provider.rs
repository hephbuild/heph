use crate::engine::driver::sandbox::Sandbox;
use crate::engine::result::EResult;
use crate::hasync::Cancellable;
use crate::htaddr::Addr;
use crate::htmatcher::Matcher;
use crate::htpkg::PkgBuf;
use crate::htvalue::Value;
use crate::htvalue::signature::FnSignature;
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub struct ConfigRequest {}
pub struct ConfigResponse {
    pub name: String,
}

pub struct ListRequest {
    pub request_id: String,
    pub package: PkgBuf,
    pub states: Vec<State>,
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

#[derive(Clone, Debug)]
pub struct State {
    pub package: PkgBuf,
    pub provider: String,
    pub state: HashMap<String, Value>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct TargetSpec {
    pub addr: Addr,
    pub driver: String,
    pub config: HashMap<String, Value>,
    pub labels: Vec<String>,
    pub transitive: Sandbox,
}

pub trait ProviderExecutor: Send + Sync {
    /// Resolve a target's result.
    ///
    /// Each call routes through `Engine::result_addr`, which adds a
    /// `parent → addr` edge to the request's `DepDag` before any await. That
    /// edge is the engine's sole synchronous cycle check — `DepDag::add_dep`
    /// returns `CycleError` the moment a closing edge is inserted.
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

/// A function a provider exposes to BUILD files, surfaced as the Starlark symbol
/// `heph.<provider name>.<function name>`. Args and the return value are the loose
/// dynamic [`Value`] type so calls can cross provider boundaries (in-process now,
/// out-of-process plugins later).
#[async_trait]
pub trait ProviderFn: Send + Sync {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value>;
}

/// One exposed function: its bare name (no `heph.<provider>.` prefix), its
/// declarative signature, and its handler. The engine enforces `signature`
/// against every call (see [`crate::htvalue::signature::FnSignature`]).
pub struct ProviderFunctionDef {
    pub name: String,
    pub signature: FnSignature,
    pub func: Arc<dyn ProviderFn>,
}

/// A function as held in the [`ProviderFunctionRegistry`]: its signature
/// (shared, so the Starlark bridge can both enforce it and derive a native
/// param spec from it) plus the handler.
#[derive(Clone)]
pub struct RegisteredFn {
    pub signature: Arc<FnSignature>,
    pub func: Arc<dyn ProviderFn>,
}

/// Context handed to a [`ProviderFn`] at call time.
///
/// Intentionally minimal — `pkg` + `root` is what filesystem helpers like `glob`
/// need. A `ProviderExecutor`/cancellation token is deliberately absent: a function
/// that resolves targets through the engine would also need the buildfile provider's
/// cross-request `pkg_cache` reworked (it caches BUILD eval per provider lifetime,
/// not per request), so engine-calling functions are out of scope for now.
pub struct FnCallContext<'a> {
    /// Package the calling BUILD file lives in (e.g. `"foo/bar"`, empty at root).
    pub pkg: &'a str,
    /// Workspace root.
    pub root: &'a Path,
}

/// Positional + named arguments passed from the Starlark call site.
#[derive(Default)]
pub struct FnArgs {
    pub positional: Vec<Value>,
    pub named: HashMap<String, Value>,
}

/// Aggregate of every provider's exposed functions: provider name → function name →
/// handler. Built once by the engine and injected into providers that consume it
/// (the buildfile provider) via [`Provider::set_function_registry`].
#[derive(Default)]
pub struct ProviderFunctionRegistry {
    map: HashMap<String, HashMap<String, RegisteredFn>>,
}

impl ProviderFunctionRegistry {
    /// Insert all of `provider`'s exposed functions under its name.
    pub fn insert_provider(&mut self, provider: &str, defs: Vec<ProviderFunctionDef>) {
        if defs.is_empty() {
            return;
        }
        let entry = self.map.entry(provider.to_string()).or_default();
        for def in defs {
            entry.insert(
                def.name,
                RegisteredFn {
                    signature: Arc::new(def.signature),
                    func: def.func,
                },
            );
        }
    }

    /// Look up a single function by provider + function name.
    pub fn get(&self, provider: &str, func: &str) -> Option<&RegisteredFn> {
        self.map.get(provider).and_then(|m| m.get(func))
    }

    /// Iterate `(provider, function name, function)` over every registered function.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &RegisteredFn)> {
        self.map.iter().flat_map(|(p, fns)| {
            fns.iter()
                .map(move |(name, rf)| (p.as_str(), name.as_str(), rf))
        })
    }

    /// Iterate `(provider name, its functions)` — one entry per provider.
    pub fn providers(&self) -> impl Iterator<Item = (&str, &HashMap<String, RegisteredFn>)> {
        self.map.iter().map(|(p, fns)| (p.as_str(), fns))
    }
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
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>;
    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    >;
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

    /// Functions this provider exposes to BUILD files as `heph.<name>.<fn>`.
    /// Default: none.
    fn functions(&self) -> Vec<ProviderFunctionDef> {
        vec![]
    }

    /// Hand this provider the aggregated registry of every provider's functions.
    /// Called once by the engine before the first dispatch. Default: no-op —
    /// only consumers (the buildfile provider) override it.
    fn set_function_registry(&self, _reg: Arc<ProviderFunctionRegistry>) {}
}
