//! Guest side of the native (mux-free) stable transport: wrap an author's
//! `Provider` / `ManagedDriver` as [`StableProvider`] / [`StableManagedDriver`].
//!
//! Cold requests/responses cross as prost `pb::Frame` bytes (the response `Body`
//! the mux serve loop would have sent, returned directly instead). `get` receives
//! the host executor natively ([`DynExecutor`]) so the plugin's hot callbacks are
//! direct calls — no mux, no channels, no task spawn (see ai-docs/PERFORMANCE.md).

use crate::guest::GuestExecutor;
use anyhow::{Context, Result};
use hcore::hartifactcontent::tar::TarPacker;
use hcore::hartifactcontent::{Content, WalkEntry, WalkEntryKind};
use hcore::hasync::StdCancellationToken;
use hcore::htvalue::Value;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunInput, ManagedRunRequest};
use hmodel::htpkg::PkgBuf;
use hplugin::driver::{
    ApplyTransitiveRequest, ConfigRequest as DriverConfigRequest, ParseRequest, RunInput,
    RunRequest, inputartifact,
};
use hplugin::provider::{
    ConfigRequest, FnArgs, FnCallContext, GetError, GetRequest, ListPackagesRequest, ListRequest,
    ProbeRequest, Provider, ProviderExecutor, ProviderFn, ProviderFunctionDef,
    ProviderFunctionRegistry,
};
use hplugin_stabby::abi::{
    DynExecutor, DynFunctionRegistry, DynItemStream, StableCancel, StableFunctionRegistryDyn,
    StableItemStream, StableItemStreamDyn, StableManagedDriver, StableMeta, StableProvider,
};
use plugin_abi::convert;
use plugin_abi::pb;
use plugin_abi::pb::frame::Body;
use prost::Message;
use stabby::future::DynFutureUnsync as DynFuture;
use stabby::vec::Vec as SVec;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

/// The cdylib's own tokio runtime. A loaded cdylib's statically-linked tokio is a
/// separate instance from the host's, so async work that touches the reactor (a
/// driver `run` shelling out via `proc_exec`) must run here, not on the host
/// worker that polls our returned future. Sized like the engine's runtime
/// (plugin-go parks workers via `block_in_place` per subprocess chunk).
fn cdylib_runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        let n = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(8);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(n)
            .max_blocking_threads(8 * n + 64)
            .enable_all()
            .build()
            .expect("build cdylib plugin runtime")
    })
}

fn unary(body: Body) -> SVec<u8> {
    let f = pb::Frame {
        id: 0,
        body: Some(body),
    };
    SVec::from(f.encode_to_vec().as_slice())
}

/// Encode one `pb::Frame` carrying `body` (one frame per stream `next`).
fn frame_bytes(body: Body) -> Vec<u8> {
    pb::Frame {
        id: 0,
        body: Some(body),
    }
    .encode_to_vec()
}

/// Guest-side response stream: pulls items from a provider iterator lazily and
/// frames each on demand — nothing is buffered at the seam. `Mutex` (not `RefCell`
/// like [`HostRead`]) so the handle is `Send + Sync`, since list results flow into
/// the host engine, which requires `Send`.
struct GuestItemStream {
    frames: std::sync::Mutex<Box<dyn Iterator<Item = Vec<u8>> + Send>>,
}

impl StableItemStream for GuestItemStream {
    extern "C" fn next(&self) -> SVec<u8> {
        let mut frames = self.frames.lock().unwrap_or_else(|e| e.into_inner());
        // Empty == stream exhausted; otherwise one encoded `pb::Frame`.
        match frames.next() {
            Some(bytes) => SVec::from(bytes.as_slice()),
            None => SVec::new(),
        }
    }
}

fn make_item_stream(frames: Box<dyn Iterator<Item = Vec<u8>> + Send>) -> DynItemStream {
    stabby::boxed::Box::new(GuestItemStream {
        frames: std::sync::Mutex::new(frames),
    })
    .into()
}

/// Lazily map a provider's fallible item iterator into encoded `StreamItem` frames,
/// terminating with a `StreamEnd{error}` frame if an item errors. A clean end emits
/// no terminal frame (the empty `next` signals it).
fn frame_iter<T: 'static>(
    mut iter: Box<dyn Iterator<Item = Result<T>> + Send>,
    encode_item: fn(T) -> Vec<u8>,
) -> Box<dyn Iterator<Item = Vec<u8>> + Send> {
    let mut done = false;
    Box::new(std::iter::from_fn(move || {
        if done {
            return None;
        }
        match iter.next() {
            Some(Ok(t)) => Some(frame_bytes(Body::StreamItem(pb::StreamItem {
                item: encode_item(t).into(),
            }))),
            Some(Err(e)) => {
                done = true;
                Some(frame_bytes(stream_err(e.to_string())))
            }
            None => {
                done = true;
                None
            }
        }
    }))
}

/// A response stream that fails immediately with `msg` (one `StreamEnd{error}`).
fn error_item_stream(msg: String) -> DynItemStream {
    make_item_stream(Box::new(std::iter::once(frame_bytes(stream_err(msg)))))
}

/// A response stream for an unimplemented streaming method — the stream-shaped
/// counterpart of [`unimplemented`], carrying `Error{Unimplemented}` so a newer
/// host falls back instead of failing hard.
fn unimplemented_item_stream(method: u32) -> DynItemStream {
    let body = Body::StreamEnd(pb::StreamEnd {
        error: Some(pb::Error {
            kind: pb::error::Kind::Unimplemented as i32,
            message: format!("dispatch method {method} not implemented"),
        }),
    });
    make_item_stream(Box::new(std::iter::once(frame_bytes(body))))
}

fn err_body(message: String) -> Body {
    Body::Error(pb::Error {
        kind: pb::error::Kind::Other as i32,
        message,
    })
}

fn is_cycle(e: &anyhow::Error) -> bool {
    hcore::hmemoizer::downcast_chain_ref::<hplugin::error::CycleError>(e).is_some()
}

fn get_error_kind(e: &anyhow::Error) -> pb::get_error::Kind {
    if is_cycle(e) {
        pb::get_error::Kind::Cycle
    } else if hplugin::error::is_cancelled(e) {
        pb::get_error::Kind::Cancelled
    } else {
        pb::get_error::Kind::Other
    }
}

/// Wrap a real provider as an ABI-stable [`hplugin_stabby::abi::DynProvider`] handle
/// (in-process; the cdylib entry produces the same handle across the boundary).
pub fn make_dyn_provider(provider: Arc<dyn Provider>) -> hplugin_stabby::abi::DynProvider {
    stabby::boxed::Box::new(StableProviderImpl {
        provider,
        cancels: Arc::new(CancelRegistry::default()),
    })
    .into()
}

/// Wrap a real managed driver as an ABI-stable [`hplugin_stabby::abi::DynManagedDriver`].
pub fn make_dyn_managed_driver(
    driver: Arc<dyn ManagedDriver>,
) -> hplugin_stabby::abi::DynManagedDriver {
    stabby::boxed::Box::new(StableManagedDriverImpl {
        driver,
        cancels: Arc::new(CancelRegistry::default()),
    })
    .into()
}

/// In-flight calls keyed by `request_id`, so [`StableCancel::cancel`] can trip the
/// token a running call handed the provider/driver. A cancel that races ahead of
/// its call (arrives before the call registers) is parked in `precancelled` and
/// applied when the call enters — so it is never lost.
#[derive(Default)]
struct CancelRegistry {
    inflight: Mutex<HashMap<String, StdCancellationToken>>,
    precancelled: Mutex<HashSet<String>>,
}

impl CancelRegistry {
    /// Register a fresh token for `id` and return a guard that deregisters on drop.
    /// An empty id (no cancellation wired for this call) is a no-op passthrough.
    fn enter(self: &Arc<Self>, id: &str) -> CancelGuard {
        let token = StdCancellationToken::new();
        if !id.is_empty() {
            if self
                .precancelled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id)
            {
                token.cancel();
            }
            self.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.to_string(), token.clone());
        }
        CancelGuard {
            reg: Arc::clone(self),
            id: id.to_string(),
            token,
        }
    }

    fn cancel(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        let map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = map.get(id) {
            t.cancel();
        } else {
            drop(map);
            self.precancelled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.to_string());
        }
    }
}

/// Holds a call's cancellation token registered; deregisters on drop.
struct CancelGuard {
    reg: Arc<CancelRegistry>,
    id: String,
    token: StdCancellationToken,
}

impl CancelGuard {
    fn token(&self) -> &StdCancellationToken {
        &self.token
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.id.is_empty() {
            self.reg
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.id);
        }
    }
}

/// Wraps an author `Provider` as a [`StableProvider`].
pub struct StableProviderImpl {
    pub provider: Arc<dyn Provider>,
    cancels: Arc<CancelRegistry>,
}

impl StableCancel for StableProviderImpl {
    extern "C" fn cancel(&self, request_id: stabby::string::String) {
        self.cancels.cancel(&request_id);
    }
}

/// Unary reply for a dispatch method the plugin does not implement. A newer host
/// calling a method this (older) plugin predates gets this back and falls back —
/// the mechanism that keeps added dispatch methods additive (see ABI_VERSIONING.md).
fn unimplemented(method: u32) -> SVec<u8> {
    unary(Body::Error(pb::Error {
        kind: pb::error::Kind::Unimplemented as i32,
        message: format!("dispatch method {method} not implemented"),
    }))
}

// ---- Provider RPC bodies (moved verbatim out of the old per-method vtable slots
// into helpers; the dispatch impls below route method ids to them) ----

// Server-streaming: the provider's iterator is pulled lazily across the seam (one
// item per `StableItemStream::next`), never materialized into one blob.

async fn provider_list_stream(provider: Arc<dyn Provider>, req: SVec<u8>) -> DynItemStream {
    let req = pb::ListRequest::decode(&req[..]).unwrap_or_default();
    let tok = StdCancellationToken::new();
    let lreq = ListRequest {
        request_id: req.request_id,
        package: PkgBuf::from(req.package),
        states: req.states.into_iter().map(convert::state_from_pb).collect(),
    };
    match provider.list(lreq, &tok).await {
        Ok(iter) => make_item_stream(frame_iter(iter, |lr| {
            pb::ListResponse {
                addr: Some(convert::addr_to_pb(&lr.addr)),
            }
            .encode_to_vec()
        })),
        Err(e) => error_item_stream(e.to_string()),
    }
}

async fn provider_list_packages_stream(
    provider: Arc<dyn Provider>,
    req: SVec<u8>,
) -> DynItemStream {
    let req = pb::ListPackagesRequest::decode(&req[..]).unwrap_or_default();
    let tok = StdCancellationToken::new();
    let lreq = ListPackagesRequest {
        prefix: PkgBuf::from(req.prefix),
    };
    match provider.list_packages(lreq, &tok).await {
        Ok(iter) => make_item_stream(frame_iter(iter, |lpr| {
            pb::ListPackageResponse {
                pkg: lpr.pkg.as_str().to_string(),
            }
            .encode_to_vec()
        })),
        Err(e) => error_item_stream(e.to_string()),
    }
}

async fn provider_get(
    provider: Arc<dyn Provider>,
    req: SVec<u8>,
    exec: DynExecutor,
    cancels: Arc<CancelRegistry>,
) -> SVec<u8> {
    let req = pb::GetRequest::decode(&req[..]).unwrap_or_default();
    let executor: Arc<dyn ProviderExecutor> = Arc::new(GuestExecutor::new(exec));
    let guard = cancels.enter(&req.request_id);
    let greq = GetRequest {
        request_id: req.request_id,
        addr: convert::addr_from_pb(req.addr.unwrap_or_default()),
        states: req.states.into_iter().map(convert::state_from_pb).collect(),
        executor,
    };
    let body = match provider.get(greq, guard.token()).await {
        Ok(gr) => Body::GetResp(pb::GetResponse {
            target_spec: Some(convert::target_spec_to_pb(&gr.target_spec)),
        }),
        Err(GetError::NotFound) => Body::GetErr(pb::GetError {
            kind: pb::get_error::Kind::NotFound as i32,
            message: String::new(),
        }),
        Err(GetError::Other(e)) => Body::GetErr(pb::GetError {
            kind: get_error_kind(&e) as i32,
            message: e.to_string(),
        }),
    };
    unary(body)
}

async fn provider_probe(
    provider: Arc<dyn Provider>,
    req: SVec<u8>,
    cancels: Arc<CancelRegistry>,
) -> SVec<u8> {
    let req = pb::ProbeRequest::decode(&req[..]).unwrap_or_default();
    let guard = cancels.enter(&req.request_id);
    let preq = ProbeRequest {
        request_id: req.request_id,
        package: PkgBuf::from(req.package),
    };
    let body = match provider.probe(preq, guard.token()).await {
        Ok(pr) => Body::ProbeResp(pb::ProbeResponse {
            states: pr.states.iter().map(convert::state_to_pb).collect(),
        }),
        Err(e) => err_body(e.to_string()),
    };
    unary(body)
}

async fn provider_call_function(provider: Arc<dyn Provider>, req: SVec<u8>) -> SVec<u8> {
    let req = pb::CallFunctionRequest::decode(&req[..]).unwrap_or_default();
    // Re-derive the def each call (cheap; provider functions are static
    // metadata). The handler is not transmissible, so it must be invoked
    // here on the guest side.
    let def = provider
        .functions()
        .into_iter()
        .find(|d| d.name == req.name);
    let Some(def) = def else {
        return unary(err_body(format!(
            "unknown provider function `{}`",
            req.name
        )));
    };
    let ctx = FnCallContext {
        pkg: &req.pkg,
        root: std::path::Path::new(&req.root),
    };
    let args = FnArgs {
        positional: req
            .positional
            .into_iter()
            .map(convert::value_from_pb)
            .collect(),
        named: req
            .named
            .into_iter()
            .map(|(k, v)| (k, convert::value_from_pb(v)))
            .collect(),
    };
    let body = match def.func.call(&ctx, args).await {
        Ok(v) => Body::CallFunctionResp(pb::CallFunctionResponse {
            value: Some(convert::value_to_pb(&v)),
        }),
        Err(e) => err_body(format!("{e:#}")),
    };
    unary(body)
}

// Sync metadata helpers. Each returns the metadatum's RAW prost bytes (NOT a
// `Frame` — matching the prior `functions()`/`state_schema()`/`config()` wire).

fn provider_config(provider: &Arc<dyn Provider>) -> SVec<u8> {
    let name = provider
        .config(ConfigRequest {})
        .map(|r| r.name)
        .unwrap_or_default();
    SVec::from(pb::ConfigResponse { name }.encode_to_vec().as_slice())
}

fn provider_functions(provider: &Arc<dyn Provider>) -> SVec<u8> {
    let functions = provider
        .functions()
        .into_iter()
        .map(|d| pb::ProviderFunctionDef {
            name: d.name,
            signature: Some(convert::fn_signature_to_pb(&d.signature)),
            doc: d.doc,
        })
        .collect();
    SVec::from(
        pb::FunctionsResponse { functions }
            .encode_to_vec()
            .as_slice(),
    )
}

fn provider_state_schema(provider: &Arc<dyn Provider>) -> SVec<u8> {
    // Empty SVec == `None`; an encoded Schema == `Some` (see the ABI doc).
    match provider.state_schema() {
        Some(s) => SVec::from(convert::state_schema_to_pb(&s).encode_to_vec().as_slice()),
        None => SVec::new(),
    }
}

fn provider_set_registry(
    provider: &Arc<dyn Provider>,
    metadata: SVec<u8>,
    reg: DynFunctionRegistry,
) {
    let meta = pb::FunctionRegistry::decode(&metadata[..]).unwrap_or_default();
    // Shared across every proxy handler — each dispatches back over the host
    // callback to invoke the actual function.
    let reg = Arc::new(reg);
    let mut by_provider: std::collections::HashMap<String, Vec<ProviderFunctionDef>> =
        std::collections::HashMap::new();
    for f in meta.functions {
        let Some(signature) = f.signature.map(convert::fn_signature_from_pb) else {
            continue;
        };
        by_provider
            .entry(f.provider.clone())
            .or_default()
            .push(ProviderFunctionDef {
                name: f.name.clone(),
                signature,
                doc: f.doc,
                func: Arc::new(GuestRegisteredFn {
                    reg: Arc::clone(&reg),
                    provider: f.provider,
                    name: f.name,
                }),
            });
    }
    let mut registry = ProviderFunctionRegistry::default();
    for (provider_name, defs) in by_provider {
        registry.insert_provider(&provider_name, defs);
    }
    provider.set_function_registry(Arc::new(registry));
}

impl StableMeta for StableProviderImpl {
    extern "C" fn meta(&self, kind: u32) -> SVec<u8> {
        match pb::ProviderMethod::try_from(kind as i32) {
            Ok(pb::ProviderMethod::Config) => provider_config(&self.provider),
            Ok(pb::ProviderMethod::Functions) => provider_functions(&self.provider),
            Ok(pb::ProviderMethod::StateSchema) => provider_state_schema(&self.provider),
            // Unknown sync metadatum: empty == "none", never a hard failure.
            _ => SVec::new(),
        }
    }
}

impl StableProvider for StableProviderImpl {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let provider = Arc::clone(&self.provider);
        let cancels = Arc::clone(&self.cancels);
        stabby::boxed::Box::new(async move {
            match pb::ProviderMethod::try_from(method as i32) {
                Ok(pb::ProviderMethod::Probe) => provider_probe(provider, req, cancels).await,
                Ok(pb::ProviderMethod::CallFunction) => provider_call_function(provider, req).await,
                _ => unimplemented(method),
            }
        })
        .into()
    }

    extern "C" fn invoke_server_stream<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
    ) -> DynFuture<'a, DynItemStream> {
        let provider = Arc::clone(&self.provider);
        stabby::boxed::Box::new(async move {
            match pb::ProviderMethod::try_from(method as i32) {
                Ok(pb::ProviderMethod::List) => provider_list_stream(provider, req).await,
                Ok(pb::ProviderMethod::ListPackages) => {
                    provider_list_packages_stream(provider, req).await
                }
                _ => unimplemented_item_stream(method),
            }
        })
        .into()
    }

    extern "C" fn invoke_client_stream<'a>(
        &'a self,
        method: u32,
        // No client-streaming provider RPC yet; the request stream is dropped.
        _req: DynItemStream,
    ) -> DynFuture<'a, SVec<u8>> {
        stabby::boxed::Box::new(async move { unimplemented(method) }).into()
    }

    extern "C" fn invoke_bidi<'a>(
        &'a self,
        method: u32,
        _req: DynItemStream,
    ) -> DynFuture<'a, DynItemStream> {
        stabby::boxed::Box::new(async move { unimplemented_item_stream(method) }).into()
    }

    extern "C" fn invoke_exec<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
        exec: DynExecutor,
    ) -> DynFuture<'a, SVec<u8>> {
        let provider = Arc::clone(&self.provider);
        let cancels = Arc::clone(&self.cancels);
        stabby::boxed::Box::new(async move {
            match pb::ProviderMethod::try_from(method as i32) {
                Ok(pb::ProviderMethod::Get) => provider_get(provider, req, exec, cancels).await,
                _ => unimplemented(method),
            }
        })
        .into()
    }

    extern "C" fn invoke_registry(&self, method: u32, req: SVec<u8>, reg: DynFunctionRegistry) {
        // Only SetFunctionRegistry rides this slot today; an unknown id has no
        // return channel, so the handle is simply dropped.
        if let Ok(pb::ProviderMethod::SetFunctionRegistry) =
            pb::ProviderMethod::try_from(method as i32)
        {
            provider_set_registry(&self.provider, req, reg);
        }
    }
}

/// Guest-side proxy for a function in the host's aggregate registry: dispatches
/// `call_registered` back over the host callback, decoding the returned value.
struct GuestRegisteredFn {
    reg: Arc<DynFunctionRegistry>,
    provider: String,
    name: String,
}

#[async_trait::async_trait]
impl ProviderFn for GuestRegisteredFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> Result<Value> {
        let pb_req = pb::CallRegisteredRequest {
            provider: self.provider.clone(),
            name: self.name.clone(),
            pkg: ctx.pkg.to_string(),
            root: ctx.root.to_string_lossy().into_owned(),
            positional: args.positional.iter().map(convert::value_to_pb).collect(),
            named: args
                .named
                .iter()
                .map(|(k, v)| (k.clone(), convert::value_to_pb(v)))
                .collect(),
        }
        .encode_to_vec();
        let bytes = self
            .reg
            .call_registered(SVec::from(pb_req.as_slice()))
            .await;
        match pb::Frame::decode(&bytes[..])?.body {
            Some(Body::CallFunctionResp(r)) => {
                Ok(convert::value_from_pb(r.value.unwrap_or_default()))
            }
            Some(Body::Error(e)) => anyhow::bail!("{}", e.message),
            other => anyhow::bail!("unexpected call_registered response: {other:?}"),
        }
    }
}

fn stream_err(message: String) -> Body {
    Body::StreamEnd(pb::StreamEnd {
        error: Some(pb::Error {
            kind: pb::error::Kind::Other as i32,
            message,
        }),
    })
}

/// Wraps an author `ManagedDriver` as a [`StableManagedDriver`].
pub struct StableManagedDriverImpl {
    pub driver: Arc<dyn ManagedDriver>,
    cancels: Arc<CancelRegistry>,
}

impl StableCancel for StableManagedDriverImpl {
    extern "C" fn cancel(&self, request_id: stabby::string::String) {
        self.cancels.cancel(&request_id);
    }
}

fn driver_config(driver: &Arc<dyn ManagedDriver>) -> SVec<u8> {
    let name = driver
        .config(DriverConfigRequest {})
        .map(|r| r.name)
        .unwrap_or_default();
    SVec::from(pb::ConfigResponse { name }.encode_to_vec().as_slice())
}

fn driver_schema(driver: &Arc<dyn ManagedDriver>) -> SVec<u8> {
    SVec::from(
        convert::driver_schema_to_pb(&driver.schema())
            .encode_to_vec()
            .as_slice(),
    )
}

async fn driver_parse(
    driver: Arc<dyn ManagedDriver>,
    req: SVec<u8>,
    cancels: Arc<CancelRegistry>,
) -> SVec<u8> {
    let req = pb::ParseRequest::decode(&req[..]).unwrap_or_default();
    let guard = cancels.enter(&req.request_id);
    let preq = ParseRequest {
        request_id: req.request_id,
        target_spec: Arc::new(convert::target_spec_from_pb(
            req.target_spec.unwrap_or_default(),
        )),
    };
    let body = match driver.parse(preq, guard.token()).await {
        Ok(resp) => match convert::target_def_to_pb(&resp.target_def) {
            Ok(td) => Body::ParseResp(pb::ParseResponse {
                target_def: Some(td),
            }),
            Err(e) => err_body(e.to_string()),
        },
        Err(e) => err_body(e.to_string()),
    };
    unary(body)
}

async fn driver_apply_transitive(
    driver: Arc<dyn ManagedDriver>,
    req: SVec<u8>,
    cancels: Arc<CancelRegistry>,
) -> SVec<u8> {
    let req = pb::ApplyTransitiveRequest::decode(&req[..]).unwrap_or_default();
    let guard = cancels.enter(&req.request_id);
    let target_def = match convert::target_def_from_pb(req.target_def.unwrap_or_default()) {
        Ok(td) => td,
        Err(e) => return unary(err_body(e.to_string())),
    };
    let areq = ApplyTransitiveRequest {
        request_id: req.request_id,
        target_def,
        sandbox: convert::sandbox_from_pb(req.sandbox.unwrap_or_default()),
    };
    let body = match driver.apply_transitive(areq, guard.token()).await {
        Ok(resp) => match convert::target_def_to_pb(&resp.target_def) {
            Ok(td) => Body::ApplyTransitiveResp(pb::ApplyTransitiveResponse {
                target_def: Some(td),
            }),
            Err(e) => err_body(e.to_string()),
        },
        Err(e) => err_body(e.to_string()),
    };
    unary(body)
}

impl StableMeta for StableManagedDriverImpl {
    extern "C" fn meta(&self, kind: u32) -> SVec<u8> {
        match pb::DriverMethod::try_from(kind as i32) {
            Ok(pb::DriverMethod::Config) => driver_config(&self.driver),
            Ok(pb::DriverMethod::Schema) => driver_schema(&self.driver),
            _ => SVec::new(),
        }
    }
}

impl StableManagedDriver for StableManagedDriverImpl {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let driver = Arc::clone(&self.driver);
        let cancels = Arc::clone(&self.cancels);
        stabby::boxed::Box::new(async move {
            match pb::DriverMethod::try_from(method as i32) {
                Ok(pb::DriverMethod::Parse) => driver_parse(driver, req, cancels).await,
                Ok(pb::DriverMethod::ApplyTransitive) => {
                    driver_apply_transitive(driver, req, cancels).await
                }
                _ => unimplemented(method),
            }
        })
        .into()
    }

    // No unary->stream or stream->unary driver RPC yet; provisioned, Unimplemented.
    extern "C" fn invoke_server_stream<'a>(
        &'a self,
        method: u32,
        _req: SVec<u8>,
    ) -> DynFuture<'a, DynItemStream> {
        stabby::boxed::Box::new(async move { unimplemented_item_stream(method) }).into()
    }

    extern "C" fn invoke_client_stream<'a>(
        &'a self,
        method: u32,
        _req: DynItemStream,
    ) -> DynFuture<'a, SVec<u8>> {
        stabby::boxed::Box::new(async move { unimplemented(method) }).into()
    }

    // `run` is the one bidi RPC: request stream = RunInFrame (run request, then live
    // stdin), response stream = RunOutFrame (live stdout/stderr, then the result).
    extern "C" fn invoke_bidi<'a>(
        &'a self,
        method: u32,
        req: DynItemStream,
    ) -> DynFuture<'a, DynItemStream> {
        let driver = Arc::clone(&self.driver);
        let cancels = Arc::clone(&self.cancels);
        stabby::boxed::Box::new(async move {
            match pb::DriverMethod::try_from(method as i32) {
                Ok(pb::DriverMethod::Run) => run_bidi(driver, req, cancels).await,
                _ => unimplemented_item_stream(method),
            }
        })
        .into()
    }
}

/// Guest-side response stream backed by an mpsc the run task feeds. `blocking_recv`
/// is sound because the host drains run output on a blocking task (it cannot ride
/// the host's async workers), matching how `run` already parks threads per chunk.
struct ChannelItemStream {
    rx: Mutex<tokio::sync::mpsc::Receiver<Vec<u8>>>,
}

impl StableItemStream for ChannelItemStream {
    extern "C" fn next(&self) -> SVec<u8> {
        let mut rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        match rx.blocking_recv() {
            Some(bytes) => SVec::from(bytes.as_slice()),
            None => SVec::new(),
        }
    }
}

fn make_channel_item_stream(rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> DynItemStream {
    stabby::boxed::Box::new(ChannelItemStream { rx: Mutex::new(rx) }).into()
}

fn run_out_err(msg: String) -> pb::RunOutFrame {
    pb::RunOutFrame {
        msg: Some(pb::run_out_frame::Msg::Error(msg)),
    }
}

/// A run response stream that fails immediately (one `RunOutFrame{error}`).
fn run_error_stream(msg: String) -> DynItemStream {
    make_item_stream(Box::new(std::iter::once(run_out_err(msg).encode_to_vec())))
}

/// Pull the first request-stream item: the run request (`RunInFrame{start}`).
fn pull_run_start(req: &DynItemStream) -> Option<pb::ManagedRunRequest> {
    let bytes = req.next();
    if bytes.is_empty() {
        return None;
    }
    match pb::RunInFrame::decode(&bytes[..]).ok()?.msg {
        Some(pb::run_in_frame::Msg::Start(s)) => Some(s),
        _ => None,
    }
}

async fn run_bidi(
    driver: Arc<dyn ManagedDriver>,
    req: DynItemStream,
    cancels: Arc<CancelRegistry>,
) -> DynItemStream {
    let Some(start) = pull_run_start(&req) else {
        return run_error_stream("run: missing start frame".into());
    };
    // The run shells out via the reactor — execute on the cdylib's own runtime,
    // feeding RunOutFrames into a channel the host drains. (Live stdin/stdout will
    // ride `req` / the channel later; today only the terminal result is sent.)
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    cdylib_runtime().spawn(async move {
        let guard = cancels.enter(&start.request_id);
        let out = run_once(driver, start, guard.token()).await;
        // Host gone => receiver dropped; ignore send failure.
        drop(tx.send(out.encode_to_vec()).await);
    });
    make_channel_item_stream(rx)
}

async fn run_once(
    driver: Arc<dyn ManagedDriver>,
    req: pb::ManagedRunRequest,
    ct: &StdCancellationToken,
) -> pb::RunOutFrame {
    // `shell` selects run_shell over run; it rides the request.
    let shell = req.shell;
    let target = match convert::target_def_from_pb(req.target.unwrap_or_default()) {
        Ok(t) => t,
        Err(e) => return run_out_err(e.to_string()),
    };
    let request_id = req.request_id;
    let hashin = req.hashin;
    let sandbox_dir = PathBuf::from(req.sandbox_dir);
    let run_inputs: Vec<RunInput> = req.inputs.iter().map(run_input_from_pb).collect();
    let managed_inputs: Vec<ManagedRunInput> =
        req.inputs.into_iter().map(managed_input_from_pb).collect();
    let rr = RunRequest {
        request_id: &request_id,
        target: &target,
        tree_root_path: PathBuf::from(req.tree_root_path),
        inputs: run_inputs,
        hashin: hashin.as_str(),
        stdin: None,
        stdout: None,
        stderr: None,
        sandbox_dir: sandbox_dir.clone(),
    };
    let mrr = ManagedRunRequest {
        request: rr,
        sandbox_dir,
        sandbox_ws_dir: PathBuf::from(req.sandbox_ws_dir),
        sandbox_pkg_dir: PathBuf::from(req.sandbox_pkg_dir),
        inputs: managed_inputs,
    };
    let result = if shell {
        driver.run_shell(mrr, ct).await
    } else {
        driver.run(mrr, ct).await
    };
    match result {
        Ok(resp) => pb::RunOutFrame {
            msg: Some(pb::run_out_frame::Msg::Response(pb::ManagedRunResponse {
                artifacts: resp
                    .artifacts
                    .iter()
                    .map(convert::output_artifact_to_pb)
                    .collect(),
            })),
        },
        Err(e) => run_out_err(e.to_string()),
    }
}

fn run_input_from_pb(mi: &pb::ManagedRunInput) -> RunInput {
    let ty = match pb::InputArtifactType::try_from(mi.r#type).unwrap_or(pb::InputArtifactType::Dep)
    {
        pb::InputArtifactType::Support => inputartifact::Type::Support,
        _ => inputartifact::Type::Dep,
    };
    RunInput {
        artifact: inputartifact::InputArtifact {
            r#type: ty,
            origin_id: mi.origin_id.clone(),
            // The host already materialized this input onto the shared filesystem
            // before invoking the driver. Back the content by those on-disk files
            // so a driver may read it (`walk`/`reader`) just like in-process —
            // no bytes are re-shipped over the boundary.
            content: Arc::new(DiskInputContent {
                unpack_root: PathBuf::from(&mi.unpack_root),
                list_path: mi.list_path.clone().map(PathBuf::from),
            }),
        },
        origin_id: mi.origin_id.clone(),
        source_addr: convert::addr_from_pb(mi.source_addr.clone().unwrap_or_default()),
        filters: mi.filters.clone(),
        annotations: mi
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

fn managed_input_from_pb(mi: pb::ManagedRunInput) -> ManagedRunInput {
    let input = run_input_from_pb(&mi);
    ManagedRunInput {
        input,
        list_path: mi.list_path.map(PathBuf::from),
        unpack_root: PathBuf::from(mi.unpack_root),
    }
}

/// [`Content`] for a managed run input, backed by the files the host already
/// materialized onto the shared filesystem under `unpack_root`. `list_path` (Dep
/// inputs) names the exact absolute paths of this input's files; without it
/// (Support inputs) the whole `unpack_root` tree is walked. `walk` reads those
/// files from disk; `reader` re-tars them (artifacts are tar by convention).
struct DiskInputContent {
    unpack_root: PathBuf,
    list_path: Option<PathBuf>,
}

impl DiskInputContent {
    /// Absolute paths of this input's materialized files.
    fn files(&self) -> Result<Vec<PathBuf>> {
        if let Some(lp) = &self.list_path {
            let data = std::fs::read_to_string(lp)
                .with_context(|| format!("read input list file {}", lp.display()))?;
            return Ok(data
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect());
        }
        // Support inputs carry no list file; walk the materialized tree.
        let mut out = Vec::new();
        let mut stack = vec![self.unpack_root.clone()];
        while let Some(dir) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("read dir {}", dir.display()));
                }
            };
            for entry in rd {
                let entry = entry?;
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    stack.push(entry.path());
                } else {
                    out.push(entry.path());
                }
            }
        }
        Ok(out)
    }
}

impl Content for DiskInputContent {
    fn reader(&self) -> Result<Box<dyn Read>> {
        let mut packer = TarPacker::new();
        for abs in self.files()? {
            let rel = abs.strip_prefix(&self.unpack_root).unwrap_or(&abs);
            packer.create_file(
                abs.to_string_lossy().into_owned(),
                rel.to_string_lossy().into_owned(),
            );
        }
        let mut buf = Vec::new();
        packer
            .pack(&mut buf)
            .context("pack managed input content")?;
        Ok(Box::new(std::io::Cursor::new(buf)))
    }

    fn walk(&self) -> Result<Box<dyn Iterator<Item = Result<WalkEntry>> + '_>> {
        let root = self.unpack_root.clone();
        let iter = self.files()?.into_iter().map(move |abs| {
            let rel = abs.strip_prefix(&root).unwrap_or(&abs).to_path_buf();
            let meta = std::fs::symlink_metadata(&abs)
                .with_context(|| format!("stat input file {}", abs.display()))?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&abs)
                    .with_context(|| format!("readlink {}", abs.display()))?;
                return Ok(WalkEntry {
                    path: rel,
                    kind: WalkEntryKind::Symlink { target },
                });
            }
            let x = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                {
                    false
                }
            };
            let f = std::fs::File::open(&abs).with_context(|| format!("open {}", abs.display()))?;
            Ok(WalkEntry {
                path: rel,
                kind: WalkEntryKind::File {
                    data: Box::new(f),
                    x,
                },
            })
        });
        Ok(Box::new(iter))
    }

    fn hashout(&self) -> Result<String> {
        // The hashout isn't carried on the run wire; inputs are addressed by path
        // here, not by content hash.
        Ok(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write as _;

    // A managed input's content is readable from the files the host materialized
    // under unpack_root, scoped to this input's files via the list file.
    #[test]
    fn disk_input_content_walks_and_tars_listed_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("a.txt"), b"alpha").expect("write a");
        std::fs::write(root.join("sub/b.txt"), b"beta").expect("write b");
        // A sibling file NOT in the list must be excluded (proves per-input scoping).
        std::fs::write(root.join("other.txt"), b"nope").expect("write other");

        let list = root.join("input.list");
        {
            let mut f = std::fs::File::create(&list).expect("list");
            writeln!(f, "{}", root.join("a.txt").display()).expect("w");
            writeln!(f, "{}", root.join("sub/b.txt").display()).expect("w");
        }

        let content = DiskInputContent {
            unpack_root: root.clone(),
            list_path: Some(list),
        };

        // walk(): exactly the listed files, with their bytes, relative to root.
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        for e in content.walk().expect("walk") {
            let mut e = e.expect("entry");
            if let WalkEntryKind::File { data, .. } = &mut e.kind {
                let mut s = String::new();
                data.read_to_string(&mut s).expect("read");
                seen.insert(e.path.to_string_lossy().into_owned(), s);
            }
        }
        assert_eq!(seen.get("a.txt").map(String::as_str), Some("alpha"));
        assert_eq!(seen.get("sub/b.txt").map(String::as_str), Some("beta"));
        assert!(!seen.contains_key("other.txt"), "must scope to the list");

        // reader(): a tar of the same files.
        let mut buf = Vec::new();
        content
            .reader()
            .expect("reader")
            .read_to_end(&mut buf)
            .expect("read tar");
        let entries: BTreeMap<String, String> =
            hcore::hartifactcontent::tar::TarWalker::new(std::io::Cursor::new(buf))
                .expect("tar walker")
                .map(|e| {
                    let mut e = e.expect("tar entry");
                    let mut s = String::new();
                    if let WalkEntryKind::File { data, .. } = &mut e.kind {
                        data.read_to_string(&mut s).expect("read tar file");
                    }
                    (e.path.to_string_lossy().into_owned(), s)
                })
                .collect();
        assert_eq!(entries.get("a.txt").map(String::as_str), Some("alpha"));
        assert_eq!(entries.get("sub/b.txt").map(String::as_str), Some("beta"));
        assert!(!entries.contains_key("other.txt"));
    }

    use hcore::hasync::Cancellable;
    use hcore::htvalue::Value;
    use hcore::htvalue::signature::{FnSignature, Param, ParamType};
    use hplugin::provider::{
        ConfigResponse, GetResponse, ListPackageResponse, ListResponse, ProbeResponse, ProviderFn,
        ProviderFunctionDef,
    };
    use std::path::Path;

    // A provider exposing one function `echo(msg, times=1)` whose handler reads
    // the call context's `pkg` — so the test proves both the call arguments and
    // the FnCallContext cross the seam, not just the metadata.
    struct FnProvider;

    struct EchoFn;
    #[async_trait::async_trait]
    impl ProviderFn for EchoFn {
        async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> Result<Value> {
            let msg = match args.positional.first() {
                Some(Value::String(s)) => s.clone(),
                _ => anyhow::bail!("echo: `msg` must be a string"),
            };
            let times = match args.named.get("times") {
                Some(Value::Int(n)) => *n,
                _ => 1,
            };
            Ok(Value::String(format!(
                "{}:{}",
                ctx.pkg,
                msg.repeat(usize::try_from(times).unwrap_or(0))
            )))
        }
    }

    impl Provider for FnProvider {
        fn config(&self, _req: ConfigRequest) -> Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "mock".into(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ct: &'a (dyn Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<Box<dyn Iterator<Item = Result<ListResponse>> + Send>>,
        > {
            Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ct: &'a (dyn Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<Box<dyn Iterator<Item = Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
        }
        fn get<'a>(
            &'a self,
            _req: GetRequest,
            _ct: &'a (dyn Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, std::result::Result<GetResponse, GetError>> {
            Box::pin(async { Err(GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ct: &'a (dyn Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
        fn functions(&self) -> Vec<ProviderFunctionDef> {
            vec![ProviderFunctionDef {
                name: "echo".into(),
                signature: FnSignature {
                    positional: vec![Param::required("msg", ParamType::String)],
                    named: vec![Param::optional("times", ParamType::Int, Value::Int(1))],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: "Echo `msg` `times` times, prefixed by the calling package.".into(),
                func: Arc::new(EchoFn),
            }]
        }
        fn state_schema(&self) -> Option<hplugin::provider::StateSchema> {
            use hplugin::provider::{StateField, StateSchema};
            Some(StateSchema {
                fields: vec![StateField {
                    name: "verbose".into(),
                    ty: ParamType::Bool,
                    doc: "Enable verbose output for this package.".into(),
                    required: false,
                }],
            })
        }
    }

    // The additive-compat contract: a dispatch method id this plugin does not
    // know (a newer host calling a method this build predates) returns
    // Error{Unimplemented} rather than crashing. This is what lets the host add
    // RPC methods without an ABI break — the frozen vtable still loads, and the
    // old guest answers unknown ids gracefully.
    #[test]
    fn unknown_dispatch_method_is_unimplemented() {
        use hplugin_stabby::abi::StableProviderDyn;

        let dynp = make_dyn_provider(Arc::new(FnProvider) as Arc<dyn Provider>);
        // 9999 is not a ProviderMethod value (none assigned).
        let bytes = futures::executor::block_on(dynp.invoke(9999, SVec::new()));
        match pb::Frame::decode(&bytes[..]).expect("frame").body {
            Some(Body::Error(e)) => assert_eq!(
                e.kind,
                pb::error::Kind::Unimplemented as i32,
                "unknown method must report Unimplemented"
            ),
            other => panic!("expected Unimplemented error, got {other:?}"),
        }
    }

    // Cancellation propagates host -> plugin: a `probe` that blocks until its token
    // trips returns only once the host's request token is cancelled. The host wires
    // `ct` -> StableCancel::cancel(request_id) -> the guest token the provider holds.
    // (If the wiring were missing the provider would block forever and this hangs.)
    #[test]
    fn cancellation_propagates_to_plugin() {
        use hcore::hasync::StdCancellationToken;
        use hplugin_stabby::load_stable::StableRemoteProvider;

        struct BlockingProbe;
        impl Provider for BlockingProbe {
            fn config(&self, _r: ConfigRequest) -> Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "blocker".into(),
                })
            }
            fn list<'a>(
                &'a self,
                _r: ListRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<
                'a,
                Result<Box<dyn Iterator<Item = Result<ListResponse>> + Send>>,
            > {
                Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
            }
            fn list_packages<'a>(
                &'a self,
                _r: ListPackagesRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<
                'a,
                Result<Box<dyn Iterator<Item = Result<ListPackageResponse>> + Send>>,
            > {
                Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
            }
            fn get<'a>(
                &'a self,
                _r: GetRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, std::result::Result<GetResponse, GetError>>
            {
                Box::pin(async { Err(GetError::NotFound) })
            }
            // Blocks until the (guest) token trips, then returns — proving the host
            // cancel reached the token this provider was handed.
            fn probe<'a>(
                &'a self,
                _r: ProbeRequest,
                ct: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, Result<ProbeResponse>> {
                Box::pin(async move {
                    ct.cancelled().await;
                    Ok(ProbeResponse { states: vec![] })
                })
            }
        }

        let host = StableRemoteProvider::new(make_dyn_provider(Arc::new(BlockingProbe)), "blocker");
        let ct = StdCancellationToken::new();
        let preq = ProbeRequest {
            request_id: "rq-1".into(),
            package: PkgBuf::from("p"),
        };
        // Drive the probe and the cancel concurrently; completing at all proves the
        // cancel unblocked the provider.
        let out = futures::executor::block_on(async {
            let probe = host.probe(preq, &ct);
            let cancel = async { ct.cancel() };
            let (r, ()) = futures::future::join(probe, cancel).await;
            r
        });
        out.expect("probe returns after cancellation");
    }

    // A provider whose `list` yields a scripted sequence of items (Ok) and an
    // optional terminal error — used to prove server-streaming delivers every item
    // and surfaces a mid-stream error across the seam.
    struct ListProvider {
        items: Vec<std::result::Result<&'static str, &'static str>>,
    }
    impl Provider for ListProvider {
        fn config(&self, _req: ConfigRequest) -> Result<ConfigResponse> {
            Ok(ConfigResponse {
                name: "lister".into(),
            })
        }
        fn list<'a>(
            &'a self,
            _req: ListRequest,
            _ct: &'a (dyn Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<Box<dyn Iterator<Item = Result<ListResponse>> + Send>>,
        > {
            let items: Vec<Result<ListResponse>> = self
                .items
                .iter()
                .map(|it| match it {
                    Ok(name) => Ok(ListResponse {
                        addr: convert::addr_from_pb(pb::Addr {
                            package: "p".into(),
                            name: (*name).into(),
                            args: Default::default(),
                        }),
                    }),
                    Err(msg) => Err(anyhow::anyhow!("{msg}")),
                })
                .collect();
            Box::pin(async move { Ok(Box::new(items.into_iter()) as Box<_>) })
        }
        fn list_packages<'a>(
            &'a self,
            _req: ListPackagesRequest,
            _ct: &'a (dyn Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<
            'a,
            Result<Box<dyn Iterator<Item = Result<ListPackageResponse>> + Send>>,
        > {
            Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
        }
        fn get<'a>(
            &'a self,
            _req: GetRequest,
            _ct: &'a (dyn Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, std::result::Result<GetResponse, GetError>> {
            Box::pin(async { Err(GetError::NotFound) })
        }
        fn probe<'a>(
            &'a self,
            _req: ProbeRequest,
            _ct: &'a (dyn Cancellable + Send + Sync),
        ) -> futures::future::BoxFuture<'a, Result<ProbeResponse>> {
            Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
        }
    }

    fn list_all(
        items: Vec<std::result::Result<&'static str, &'static str>>,
    ) -> Vec<Result<String>> {
        use hcore::hasync::StdCancellationToken;
        use hplugin_stabby::load_stable::StableRemoteProvider;

        let dynp = make_dyn_provider(Arc::new(ListProvider { items }) as Arc<dyn Provider>);
        let host = StableRemoteProvider::new(dynp, "lister");
        let tok = StdCancellationToken::new();
        let iter = futures::executor::block_on(host.list(
            ListRequest {
                request_id: String::new(),
                package: PkgBuf::from("p"),
                states: vec![],
            },
            &tok,
        ))
        .expect("list ok");
        iter.map(|r| r.map(|lr| lr.addr.to_string())).collect()
    }

    // Server-streaming list delivers every item across the seam, in order.
    #[test]
    fn list_streams_all_items() {
        let got = list_all(vec![Ok("x"), Ok("y"), Ok("z")]);
        let names: Vec<String> = got.into_iter().map(|r| r.expect("item")).collect();
        assert_eq!(names.len(), 3, "all three items must arrive");
        assert!(names[0].ends_with("x"));
        assert!(names[2].ends_with("z"));
    }

    // A mid-stream provider error surfaces as a failed item, and the stream ends.
    #[test]
    fn list_stream_propagates_midstream_error() {
        let got = list_all(vec![Ok("x"), Err("boom")]);
        assert_eq!(got.len(), 2, "the ok item then the error");
        assert!(got[0].is_ok());
        let err = got[1].as_ref().expect_err("second item is the error");
        assert!(err.to_string().contains("boom"));
    }

    // `run` over invoke_bidi: the request stream (RunInFrame) is consumed and the
    // response stream yields exactly one terminal RunOutFrame. Proves the bidi
    // plumbing — request pulled, run spawned, result delivered over the channel.
    #[test]
    fn run_bidi_yields_one_terminal_frame() {
        use hcore::hasync::Cancellable;
        use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};
        use hplugin_stabby::abi::{StableItemStreamDyn, StableManagedDriverDyn};

        struct NoopDriver;
        #[async_trait::async_trait]
        impl ManagedDriver for NoopDriver {
            fn config(
                &self,
                _r: hplugin::driver::ConfigRequest,
            ) -> Result<hplugin::driver::ConfigResponse> {
                Ok(hplugin::driver::ConfigResponse {
                    name: "noop".into(),
                })
            }
            fn schema(&self) -> hplugin::driver::DriverSchema {
                hplugin::driver::DriverSchema::default()
            }
            async fn parse(
                &self,
                _r: hplugin::driver::ParseRequest,
                _c: &(dyn Cancellable + Send + Sync),
            ) -> Result<hplugin::driver::ParseResponse> {
                anyhow::bail!("unused")
            }
            async fn apply_transitive(
                &self,
                _r: hplugin::driver::ApplyTransitiveRequest,
                _c: &(dyn Cancellable + Send + Sync),
            ) -> Result<hplugin::driver::ApplyTransitiveResponse> {
                anyhow::bail!("unused")
            }
            async fn run<'a, 'io>(
                &self,
                _r: ManagedRunRequest<'a, 'io>,
                _c: &(dyn Cancellable + Send + Sync),
            ) -> Result<ManagedRunResponse> {
                Ok(ManagedRunResponse { artifacts: vec![] })
            }
        }

        let dynd = make_dyn_managed_driver(Arc::new(NoopDriver) as Arc<dyn ManagedDriver>);
        let start = pb::RunInFrame {
            msg: Some(pb::run_in_frame::Msg::Start(pb::ManagedRunRequest {
                request_id: "r".into(),
                ..Default::default()
            })),
        }
        .encode_to_vec();
        let req_stream = make_item_stream(Box::new(std::iter::once(start)));

        let resp =
            futures::executor::block_on(dynd.invoke_bidi(pb::DriverMethod::Run as u32, req_stream));
        // `next` blocks on the run task; drain on a thread.
        let frames = std::thread::spawn(move || {
            let mut out: Vec<Vec<u8>> = Vec::new();
            loop {
                let b = resp.next();
                if b.is_empty() {
                    break;
                }
                out.push(b.to_vec());
            }
            out
        })
        .join()
        .expect("drain thread");

        assert_eq!(frames.len(), 1, "exactly one terminal RunOutFrame then end");
        let frame = pb::RunOutFrame::decode(&frames[0][..]).expect("decode RunOutFrame");
        // A terminal frame (Response or Error) — proves the request was consumed and
        // the run task's output crossed the response stream. (The default target
        // fails conversion, so this run terminates as Error; the plumbing is the point.)
        assert!(
            matches!(
                frame.msg,
                Some(pb::run_out_frame::Msg::Response(_)) | Some(pb::run_out_frame::Msg::Error(_))
            ),
            "expected a terminal RunOutFrame, got {:?}",
            frame.msg
        );
    }

    // Provider functions survive the guest→host stable-ABI round trip: the host
    // sees the same name/signature/doc, and invoking the proxied handler carries
    // both the arguments and the FnCallContext across the seam.
    #[test]
    fn provider_functions_roundtrip() {
        use hplugin_stabby::load_stable::StableRemoteProvider;

        let dynp = make_dyn_provider(Arc::new(FnProvider) as Arc<dyn Provider>);
        let host = StableRemoteProvider::new(dynp, "mock");

        // Metadata crosses: exactly one function, rendered as declared.
        let defs = host.functions();
        assert_eq!(defs.len(), 1);
        let def = &defs[0];
        assert_eq!(def.name, "echo");
        assert_eq!(
            def.signature.render("echo"),
            "echo(msg: string, times?: int) -> string"
        );
        assert!(def.doc.contains("Echo `msg`"));

        let root = std::path::PathBuf::from("/ws");
        let ctx = FnCallContext {
            pkg: "mypkg",
            root: Path::new(&root),
        };
        // Default `times` (omitted) → 1; the handler reads ctx.pkg.
        let out = futures::executor::block_on(def.func.call(
            &ctx,
            FnArgs {
                positional: vec![Value::String("hi".into())],
                named: Default::default(),
            },
        ))
        .expect("call echo");
        assert_eq!(out, Value::String("mypkg:hi".into()));

        // Named arg crosses and is honored.
        let mut named = std::collections::HashMap::new();
        named.insert("times".to_string(), Value::Int(3));
        let out = futures::executor::block_on(def.func.call(
            &ctx,
            FnArgs {
                positional: vec![Value::String("ab".into())],
                named,
            },
        ))
        .expect("call echo times=3");
        assert_eq!(out, Value::String("mypkg:ababab".into()));

        // The provider's state schema crosses too (Some, with its one field).
        let schema = host.state_schema().expect("state schema crosses as Some");
        assert_eq!(schema.fields.len(), 1);
        assert_eq!(schema.fields[0].name, "verbose");
        assert_eq!(schema.fields[0].ty, ParamType::Bool);
        assert!(schema.fields[0].doc.contains("verbose output"));
        assert!(!schema.fields[0].required);
    }

    // The host's aggregate function registry is injected into a dylib provider:
    // the provider receives proxy handlers that dispatch back over the host
    // callback, so invoking one reaches the real (host-side) function — args and
    // FnCallContext included.
    #[test]
    fn function_registry_injection_roundtrip() {
        use hplugin_stabby::load_stable::StableRemoteProvider;
        use std::sync::Mutex;

        // A provider that records the registry it is handed.
        struct Recorder {
            stored: Arc<Mutex<Option<Arc<ProviderFunctionRegistry>>>>,
        }
        impl Provider for Recorder {
            fn config(&self, _req: ConfigRequest) -> Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: "recorder".into(),
                })
            }
            fn list<'a>(
                &'a self,
                _req: ListRequest,
                _ct: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<
                'a,
                Result<Box<dyn Iterator<Item = Result<ListResponse>> + Send>>,
            > {
                Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
            }
            fn list_packages<'a>(
                &'a self,
                _req: ListPackagesRequest,
                _ct: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<
                'a,
                Result<Box<dyn Iterator<Item = Result<ListPackageResponse>> + Send>>,
            > {
                Box::pin(async { Ok(Box::new(std::iter::empty()) as Box<_>) })
            }
            fn get<'a>(
                &'a self,
                _req: GetRequest,
                _ct: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, std::result::Result<GetResponse, GetError>>
            {
                Box::pin(async { Err(GetError::NotFound) })
            }
            fn probe<'a>(
                &'a self,
                _req: ProbeRequest,
                _ct: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, Result<ProbeResponse>> {
                Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
            }
            fn set_function_registry(&self, reg: Arc<ProviderFunctionRegistry>) {
                *self.stored.lock().unwrap() = Some(reg);
            }
        }

        let stored = Arc::new(Mutex::new(None));
        let recorder = Arc::new(Recorder {
            stored: Arc::clone(&stored),
        });
        let dynp = make_dyn_provider(recorder as Arc<dyn Provider>);
        let host = StableRemoteProvider::new(dynp, "recorder");

        // A host-side aggregate registry holding one function under "greeter".
        let mut reg = ProviderFunctionRegistry::default();
        reg.insert_provider(
            "greeter",
            vec![ProviderFunctionDef {
                name: "echo".into(),
                signature: FnSignature {
                    positional: vec![Param::required("msg", ParamType::String)],
                    named: vec![],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: "echo".into(),
                func: Arc::new(EchoFn),
            }],
        );
        host.set_function_registry(Arc::new(reg));

        // The recorder received a registry; its proxy resolves back to the host
        // EchoFn, carrying both the arg and ctx.pkg across the (reverse) seam.
        let received = stored.lock().unwrap().clone().expect("registry injected");
        let rf = received.get("greeter", "echo").expect("echo registered");
        let root = std::path::PathBuf::from("/ws");
        let ctx = FnCallContext {
            pkg: "callerpkg",
            root: Path::new(&root),
        };
        let out = futures::executor::block_on(rf.func.call(
            &ctx,
            FnArgs {
                positional: vec![Value::String("yo".into())],
                named: Default::default(),
            },
        ))
        .expect("call proxied echo");
        assert_eq!(out, Value::String("callerpkg:yo".into()));
    }

    // A managed driver's config schema survives the round trip (LSP kwargs).
    #[test]
    fn driver_schema_roundtrip() {
        use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};
        use hplugin::driver::{DriverField, DriverSchema};
        use hplugin_stabby::load_stable::StableRemoteManagedDriver;

        struct SchemaDriver;
        #[async_trait::async_trait]
        impl ManagedDriver for SchemaDriver {
            fn config(
                &self,
                _req: hplugin::driver::ConfigRequest,
            ) -> Result<hplugin::driver::ConfigResponse> {
                Ok(hplugin::driver::ConfigResponse {
                    name: "mockdrv".into(),
                })
            }
            fn schema(&self) -> DriverSchema {
                DriverSchema {
                    fields: vec![DriverField {
                        name: "args".into(),
                        ty: ParamType::list(ParamType::String),
                        doc: "Command arguments.".into(),
                        required: true,
                    }],
                }
            }
            async fn parse(
                &self,
                _req: hplugin::driver::ParseRequest,
                _ct: &(dyn Cancellable + Send + Sync),
            ) -> Result<hplugin::driver::ParseResponse> {
                anyhow::bail!("unused")
            }
            async fn apply_transitive(
                &self,
                _req: hplugin::driver::ApplyTransitiveRequest,
                _ct: &(dyn Cancellable + Send + Sync),
            ) -> Result<hplugin::driver::ApplyTransitiveResponse> {
                anyhow::bail!("unused")
            }
            async fn run<'a, 'io>(
                &self,
                _req: ManagedRunRequest<'a, 'io>,
                _ct: &(dyn Cancellable + Send + Sync),
            ) -> Result<ManagedRunResponse> {
                anyhow::bail!("unused")
            }
        }

        let dynd = make_dyn_managed_driver(Arc::new(SchemaDriver) as Arc<dyn ManagedDriver>);
        let host = StableRemoteManagedDriver::new(dynd, "mockdrv");
        let schema = host.schema();
        assert_eq!(schema.fields.len(), 1);
        assert_eq!(schema.fields[0].name, "args");
        assert_eq!(schema.fields[0].ty, ParamType::list(ParamType::String));
        assert!(schema.fields[0].required);
        assert!(schema.fields[0].doc.contains("Command arguments"));
    }
}
