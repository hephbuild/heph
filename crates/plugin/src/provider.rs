use crate::driver::sandbox::Sandbox;
use crate::eresult::EResult;
use async_trait::async_trait;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
use hcore::htvalue::signature::FnSignature;
use hmodel::htaddr::Addr;
use hmodel::htmatcher::Matcher;
use hmodel::htpkg::PkgBuf;
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
    /// Engine callback surface, so `list` can gather config beyond the package's
    /// ancestry `states` — e.g. the go plugin fetching a module's variant universe
    /// via [`ProviderExecutor::states_under`] to enumerate library variants.
    /// Providers that don't need it ignore it; non-engine call sites (LSP, tests)
    /// pass [`NoopExecutor`].
    pub executor: Arc<dyn ProviderExecutor>,
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

/// One frame of a target's source provenance: a call site on the Starlark call
/// stack at the moment `target(...)` ran. The innermost frame is the `target()`
/// call itself; outer frames are the user macros / loops that led to it. Lets
/// tooling (the BUILD-file LSP) map a source position back to every target that
/// originated from the symbol at that position. Lines/columns are 1-based.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceFrame {
    /// Name of the function whose body this call site is in (`<module>` at top level).
    pub fn_name: String,
    /// Absolute path of the BUILD file containing the call site.
    pub file: String,
    pub line_start: u32,
    pub line_end: u32,
    pub col_start: u32,
    pub col_end: u32,
}

/// Host-side approval gate for a target. When `required`, the engine pauses the
/// target's execution for an explicit user decision (interactive Y/N in the TUI,
/// a stdin prompt in non-TUI mode, or `--auto-approve`). `notice` lists input
/// group names (`origin_id`s) whose rendered contents are shown to the user
/// before they decide.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub required: bool,
    pub notice: Vec<String>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct TargetSpec {
    pub addr: Addr,
    pub driver: String,
    pub config: HashMap<String, Value>,
    pub labels: Vec<String>,
    pub transitive: Sandbox,
    pub approval: Approval,
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

    /// Register a `parent → addr` dependency edge without resolving `addr`'s
    /// result. The cache-hit fast path: a provider that already has `addr`'s
    /// derived data still must register the edge (the synchronous cycle check),
    /// but needn't pay for a full `result`. Returns a [`crate::error::CycleError`]
    /// (in the error chain) when the edge closes a cycle.
    ///
    /// Default falls back to `result` (correct, just not cheap); the engine
    /// overrides it with a direct edge insert.
    fn note_dep<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { self.result(addr).await.map(|_| ()) })
    }

    /// Fetch the provider states declared for every package **at or under**
    /// `prefix` (each package's own `provider_state`s, all providers) — the
    /// downward subtree, not the upward ancestry that `GetRequest.states`
    /// carries.
    ///
    /// Unlike `result`/`query`, this registers **no** `DepDag` edge: states are
    /// build *configuration*, not a build dependency, so reading them must not
    /// couple the caller into a cycle. Used by a provider that needs config
    /// declared across a whole subtree — e.g. the go plugin gathering a Go
    /// module's variant *universe* (every `variants` declaration under the
    /// `go.mod` root, siblings included) to resolve/enumerate library variants,
    /// which `GetRequest.states` (ancestry-only) cannot supply.
    ///
    /// Default returns empty — real executors (engine, cdylib guest) override it;
    /// test mocks that never gather a subtree can keep the default.
    fn states_under<'a>(
        &'a self,
        _prefix: &'a PkgBuf,
    ) -> BoxFuture<'a, anyhow::Result<Vec<State>>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

/// A [`ProviderExecutor`] that resolves nothing — for call sites that build a
/// [`ListRequest`] outside the engine (LSP, tests) where no real callback surface
/// exists. `result`/`query` error if reached; `note_dep`/`states_under` are no-ops.
#[derive(Debug, Default)]
pub struct NoopExecutor;

impl ProviderExecutor for NoopExecutor {
    fn result<'a>(&'a self, addr: &'a Addr) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>> {
        Box::pin(async move { anyhow::bail!("NoopExecutor: cannot resolve {}", addr.format()) })
    }
    fn query<'a>(
        &'a self,
        _m: &'a Matcher,
        _extra_skip: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
        Box::pin(async move { anyhow::bail!("NoopExecutor: cannot query") })
    }
}

/// The [`ProviderExecutor`] callback surface, addressed by request scope id
/// instead of carrying the executor inline. Used to route a plugin's callbacks
/// back to the right per-request executor over a transport boundary where the
/// executor itself cannot cross (out-of-process, or a dylib): the host registers
/// each `get`'s executor under a scope id, the plugin's callbacks carry that id,
/// and this trait dispatches them. In-process the implementation is a direct
/// call into the engine executor (no serialization); across a cdylib it is the
/// stable-ABI mirror.
pub trait ScopedExecutor: Send + Sync {
    fn result<'a>(
        &'a self,
        request_id: &'a str,
        addr: &'a Addr,
    ) -> BoxFuture<'a, anyhow::Result<Arc<EResult>>>;
    fn note_dep<'a>(
        &'a self,
        request_id: &'a str,
        addr: &'a Addr,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
    fn query<'a>(
        &'a self,
        request_id: &'a str,
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
/// declarative signature, a one-line doc string, and its handler. The engine
/// enforces `signature` against every call (see
/// [`hcore::htvalue::signature::FnSignature`]); `doc` is surfaced by the
/// BUILD-file LSP on hover over `heph.<provider>.<name>`.
pub struct ProviderFunctionDef {
    pub name: String,
    pub signature: FnSignature,
    /// Human-readable description shown in LSP hover. Empty for undocumented
    /// functions (the hover then shows just the rendered signature).
    pub doc: String,
    pub func: Arc<dyn ProviderFn>,
}

/// A function as held in the [`ProviderFunctionRegistry`]: its signature
/// (shared, so the Starlark bridge can both enforce it and derive a native
/// param spec from it), its hover doc, plus the handler.
#[derive(Clone)]
pub struct RegisteredFn {
    pub signature: Arc<FnSignature>,
    pub doc: String,
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
                    doc: def.doc,
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

/// One keyword argument a provider accepts in a `provider_state(provider="X", …)`
/// call, with its type and docs. Consumed by the BUILD-file LSP for completion and
/// hover of provider-state args.
#[derive(Clone, Debug)]
pub struct StateField {
    pub name: String,
    pub ty: hcore::htvalue::signature::ParamType,
    pub doc: String,
    pub required: bool,
}

/// Declarative description of the state a provider accepts. Returned by
/// [`Provider::state_schema`]; `None` means the provider declares no state schema.
#[derive(Clone, Debug, Default)]
pub struct StateSchema {
    pub fields: Vec<StateField>,
}

pub trait Provider: Send + Sync {
    fn config(&self, req: ConfigRequest) -> anyhow::Result<ConfigResponse>;
    /// The addrs this provider defines in `req.package`, in a **deterministic
    /// order**.
    ///
    /// Unlike [`Provider::list_packages`], this order is *not* canonicalized by
    /// the engine: `Engine::query` yields these addrs as it receives them, they
    /// become `pluginquery`'s `deps`, and they reach the sandbox as the line
    /// order of the staged `input_<origin>.list` / `dep_<group>.list` files.
    /// Returning `HashSet` iteration order — the natural shape when the addrs
    /// come off a filesystem walk — therefore gives the same tree a different
    /// build definition, and a different list-file order, on every run. Sort, or
    /// preserve a stable walk order.
    fn list<'a>(
        &'a self,
        req: ListRequest,
        ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>;
    /// The packages this provider knows about.
    ///
    /// The order returned here is **not** observable: `Engine::packages` sorts
    /// each provider's block before merging it, so a `HashSet`-order listing is
    /// as good as a sorted one. That was not always true — this order reaches a
    /// def hash (`query` → `pluginquery`'s `deps` → `plugingroup` folds them in
    /// order) and the sandbox's list-file line order, so a per-process hash seed
    /// used to leak straight into a build definition. The engine now canonicalizes
    /// it centrally rather than trusting every provider, in-tree or third-party,
    /// to get it right.
    ///
    /// Notes for plugin authors:
    ///
    /// - The guarantee covers *this return value* and nothing else. A provider
    ///   that surfaces the same list through another channel that reaches a spec
    ///   or def — as the buildfile provider does via the `heph.core.packages()`
    ///   builtin, whose result lands in a target's config verbatim — still owns
    ///   the order on that channel. Do not drop a sort that is feeding one.
    /// - The guarantee is a property of the *host*, and there is no version
    ///   negotiation for it. A plugin that stops sorting because the host sorts
    ///   will behave nondeterministically again under a host older than this
    ///   change.
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

    /// Optional: the keyword args this provider accepts in a
    /// `provider_state(provider="<name>", …)` call, so the BUILD-file LSP can
    /// complete and document them. Default: none.
    fn state_schema(&self) -> Option<StateSchema> {
        None
    }

    /// Hand this provider the aggregated registry of every provider's functions.
    /// Called once by the engine before the first dispatch. Default: no-op —
    /// only consumers (the buildfile provider) override it.
    fn set_function_registry(&self, _reg: Arc<ProviderFunctionRegistry>) {}
}
