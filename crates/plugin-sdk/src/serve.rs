//! Guest side of the native (mux-free) stable transport: wrap an author's
//! `Provider` / `ManagedDriver` as [`StableProvider`] / [`StableManagedDriver`].
//!
//! Cold requests/responses cross as protobuf `pb::Frame` bytes (the response `Body`
//! the mux serve loop would have sent, returned directly instead). `get` receives
//! the host executor natively ([`DynExecutor`]) so the plugin's hot callbacks are
//! direct calls — no mux, no channels, no task spawn (see ai-docs/PERFORMANCE.md).

use crate::guest::GuestExecutor;
use anyhow::{Context, Result};
use buffa::Message;
use hcore::hartifactcontent::tar::TarPacker;
use hcore::hartifactcontent::{Content, WalkEntry, WalkEntryKind};
use hcore::hasync::StdCancellationToken;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunInput, ManagedRunRequest};
use hmodel::htpkg::PkgBuf;
use hplugin::driver::{
    ApplyTransitiveRequest, ConfigRequest as DriverConfigRequest, ParseRequest, RunInput,
    RunRequest, inputartifact,
};
use hplugin::function::{FnArgs, FnCallContext, FnOutcome, FunctionCaller, PluginFn, PluginFnDef};
use hplugin::hook::Hook;
use hplugin::provider::{
    ConfigRequest, GetError, GetRequest, ListPackagesRequest, ListRequest, ProbeRequest, Provider,
    ProviderExecutor,
};
use hplugin_stabby::abi::{
    DynExecutor, DynFunctionRegistry, DynItemStream, DynPluginFunctions, NamedFunction,
    StableCancel, StableFunctionRegistryDyn, StableHook, StableItemStream, StableItemStreamDyn,
    StableManagedDriver, StableMeta, StablePluginFunctions, StableProvider,
};
use hplugin_stabby::seam::panic_text;
use hplugin_stabby::vtable::dynify;
use plugin_abi::convert;
use plugin_abi::convert::{absent_not_empty, required, required_msg};
use plugin_abi::pb;
use plugin_abi::pb::frame::Body;
use stabby::future::DynFutureUnsync as DynFuture;
use stabby::vec::Vec as SVec;
use std::collections::{HashMap, HashSet};
use std::future::Future;
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
            .thread_name("heph-plugin-worker")
            .enable_all()
            .build()
            .expect("build cdylib plugin runtime")
    })
}

/// The cdylib runtime's handle, for the plugin's own construction code: what
/// a plugin's memoizers spawn on (`Memoizer::with_tag_task`). Handed to
/// constructors at `create` time — the plugin never discovers a runtime, and
/// `heph_plugin_create` runs on a host thread where `Handle::current()` would
/// be wrong or absent.
pub fn cdylib_runtime_handle() -> tokio::runtime::Handle {
    cdylib_runtime().handle().clone()
}

/// Awaits a seam task's `JoinHandle` from the wrapper future the host polls.
///
/// **Abort-on-drop**: the host dropping the wrapper (rather than cancelling
/// cooperatively) is the only stop signal for entry points with no
/// `CancelRegistry` wiring (`list`, `list_packages`, `call_function`), so the
/// spawned body is aborted when the wrapper goes away. The cooperative path
/// (`await_with_cancel` host-side) cancels then keeps polling — it never
/// drops, so it is unaffected.
///
/// The completion wake crosses from a plugin-runtime worker to the host's
/// waker through the stabby seam — plain waker forwarding, which is trusted:
/// the dropped-wake hazard this wrapper used to insure with
/// `hcore::blocking::Backstop` failed to reproduce across ~40M wakes
/// (docs/CONCURRENCY_MEASUREMENTS.md §2), and that registry no longer exists.
///
/// `JoinHandle::poll` is runtime-free, so polling this from a host worker (or
/// a plain `futures::executor::block_on`) is sound — `hook_on_events` is the
/// in-tree precedent of a guest `JoinHandle` awaited across the seam.
struct SeamTask<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> std::future::Future for SeamTask<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.get_mut().handle).poll(cx)
    }
}

impl<T> Drop for SeamTask<T> {
    fn drop(&mut self) {
        // No-op on a finished task; stops an abandoned body otherwise.
        self.handle.abort();
    }
}

/// Spawn an ABI entry point's body onto the plugin runtime and return the
/// wrapper future handed back across the seam.
///
/// **Seam invariant — eager start**: the body starts running the moment the
/// `extern "C"` entry point is called, *before* the host first polls the
/// returned future. Request decode, `CancelRegistry::enter`, `note_dep`
/// inserts — any prefix of the body — may already have happened by first poll.
/// `enter` stays INSIDE the spawned body so a cancel racing ahead of the spawn
/// is parked in `precancelled` and applied when the body enters, never lost.
///
/// A panicking body surfaces as `JoinError::is_panic` (tokio's task harness is
/// the `catch_unwind`) and is mapped to an error payload via `mk_err` — never
/// `resume_unwind`, since this wrapper's poll runs inside the host's
/// `extern "C"` shim where an unwind would abort the process. A
/// runtime-shutdown `JoinError` maps to an error the same way.
fn spawn_seam<T, F>(
    plugin: &Arc<str>,
    method: &'static str,
    key: String,
    fut: F,
    mk_err: fn(String) -> T,
) -> impl Future<Output = T> + Send + 'static
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let task = SeamTask {
        handle: hcore::hmemoizer::spawn_on_with_cycle_ctx(cdylib_runtime().handle(), fut),
    };
    let plugin = Arc::clone(plugin);
    async move {
        match task.await {
            Ok(v) => v,
            Err(e) if e.is_panic() => {
                let payload = e.into_panic();
                mk_err(format!(
                    "plugin {plugin}: {method}({key}) panicked: {}",
                    panic_text(payload.as_ref())
                ))
            }
            Err(_) => mk_err(format!(
                "plugin {plugin}: {method}({key}) aborted: plugin runtime shut down"
            )),
        }
    }
}

/// Component name for seam diagnostics: the configured name, or `<unnamed>`
/// when `config()` failed or returned an empty name.
fn seam_name(configured: Result<String>) -> Arc<str> {
    match configured {
        Ok(n) if !n.is_empty() => n.into(),
        _ => "<unnamed>".into(),
    }
}

/// Render a pb addr for seam diagnostics (no full `Addr` reconstruction).
fn pb_addr_key(a: Option<&pb::Addr>) -> String {
    match a {
        Some(a) => format!(
            "//{}:{}",
            a.package.as_deref().unwrap_or_default(),
            a.name.as_deref().unwrap_or_default()
        ),
        None => String::new(),
    }
}

/// A dispatch method id as the generated enum. `None` for an id this build
/// does not know, which the dispatch answers with `Error{Unimplemented}`.
fn method_id<E: buffa::Enumeration>(method: u32) -> Option<E> {
    i32::try_from(method).ok().and_then(E::from_i32)
}

/// Decode a request, naming it on failure. A request that does not decode is
/// an error frame — never a defaulted request run as if the host had sent it.
fn decode_req<M: buffa::Message>(bytes: &[u8], what: &str) -> Result<M> {
    plugin_abi::decode::<M>(bytes).with_context(|| format!("decoding {what}"))
}

fn unary(body: Body) -> SVec<u8> {
    SVec::from(frame_bytes(body).as_slice())
}

/// Encode one `pb::Frame` carrying `body` (one frame per stream `next`).
fn frame_bytes(body: Body) -> Vec<u8> {
    pb::Frame {
        id: Some(0),
        body: Some(body),
        ..Default::default()
    }
    .encode_to_vec()
}

fn wire_error(kind: pb::error::Kind, message: String) -> pb::Error {
    pb::Error {
        kind: Some(kind.into()),
        message: Some(message),
        ..Default::default()
    }
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
    dynify(stabby::boxed::Box::new(GuestItemStream {
        frames: std::sync::Mutex::new(frames),
    }))
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
            Some(Ok(t)) => Some(frame_bytes(Body::from(pb::StreamItem {
                item: Some(encode_item(t)),
                ..Default::default()
            }))),
            Some(Err(e)) => {
                done = true;
                Some(frame_bytes(stream_err(err_message(&e))))
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
    let body = Body::from(pb::StreamEnd {
        error: wire_error(
            pb::error::Kind::Unimplemented,
            format!("dispatch method {method} not implemented"),
        )
        .into(),
        ..Default::default()
    });
    make_item_stream(Box::new(std::iter::once(frame_bytes(body))))
}

fn err_body(message: String) -> Body {
    Body::from(wire_error(pb::error::Kind::Other, message))
}

/// Serialize an error for transmission across the ABI boundary. Uses anyhow's
/// alternate (`{:#}`) form so the FULL cause chain crosses, not just the
/// outermost context — the host reconstructs a single-message error from this
/// string, so a bare `to_string()` would silently drop every underlying cause
/// (e.g. `compute embedcfg` without the `//go:embed pattern(s) matched no
/// files: …` that explains it).
fn err_message(e: &anyhow::Error) -> String {
    format!("{e:#}")
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
    // Captured once for seam diagnostics (panic messages) — `config` is static
    // metadata; calling it lazily on the error path would run author code
    // inside the extern shim's poll. `<unnamed>` rather than an empty string,
    // so a failing config still yields a readable "plugin <unnamed>: …" error.
    let name: Arc<str> = seam_name(provider.config(ConfigRequest {}).map(|r| r.name));
    dynify(stabby::boxed::Box::new(StableProviderImpl {
        provider,
        name,
        cancels: Arc::new(CancelRegistry::default()),
    }))
}

/// Wrap a real managed driver as an ABI-stable [`hplugin_stabby::abi::DynManagedDriver`].
pub fn make_dyn_managed_driver(
    driver: Arc<dyn ManagedDriver>,
) -> hplugin_stabby::abi::DynManagedDriver {
    let name: Arc<str> = seam_name(driver.config(DriverConfigRequest {}).map(|r| r.name));
    dynify(stabby::boxed::Box::new(StableManagedDriverImpl {
        driver,
        name,
        cancels: Arc::new(CancelRegistry::default()),
    }))
}

/// Export a managed driver for a cdylib's `PluginComponents.drivers`, under the
/// local name its own `config()` reports — the name the host registers it by
/// (prefixed with the plugin's). The driver states its name once; a cdylib
/// never repeats it as a literal beside the driver.
///
/// Fails, naming the driver's type, when its `config()` does.
pub fn named_managed_driver<D: ManagedDriver + 'static>(
    driver: D,
) -> Result<hplugin_stabby::abi::NamedDriver> {
    let name = driver
        .config(DriverConfigRequest {})
        .with_context(|| format!("config of driver {}", std::any::type_name::<D>()))?
        .name;
    Ok(hplugin_stabby::abi::NamedDriver {
        name: name.as_str().into(),
        driver: make_dyn_managed_driver(Arc::new(driver)),
    })
}

/// In-flight calls keyed by `request_id`, so [`StableCancel::cancel`] can trip the
/// token a running call handed the provider/driver. A cancel that races ahead of
/// its call (arrives before the call registers) is parked in `precancelled` and
/// applied when the call enters — so it is never lost.
///
/// A request id names an engine *request* (`req-{n}`), not a single call — the
/// whole request subtree shares it — so multiple calls can be in flight under
/// one id concurrently. Each keeps its own token in the `Vec`; `cancel` trips
/// them all (a request cancel means the whole subtree stops).
#[derive(Default)]
struct CancelRegistry {
    inflight: Mutex<HashMap<String, Vec<StdCancellationToken>>>,
    precancelled: Mutex<HashSet<String>>,
}

impl CancelRegistry {
    /// Register a fresh token for `id` and return a guard that deregisters on drop.
    /// An empty id (no cancellation wired for this call) is a no-op passthrough.
    ///
    /// Ordering pairs with [`cancel`](Self::cancel): each side *publishes* (the
    /// token into `inflight` / the id into `precancelled`) before it *checks*
    /// the other's map, so whichever side loses the race, at least one of the
    /// two checks observes the other's write — a cancel is applied by one of
    /// them (possibly both; `cancel()` on a token is idempotent), never
    /// dropped. Checking before publishing (the old order) had a window where
    /// `enter` saw no parked cancel and `cancel` saw no in-flight token, losing
    /// the cancel — the eager seam spawn makes that window hittable.
    fn enter(self: &Arc<Self>, id: &str) -> CancelGuard {
        let token = StdCancellationToken::new();
        if !id.is_empty() {
            self.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(id.to_string())
                .or_default()
                .push(token.clone());
            if self
                .precancelled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id)
            {
                token.cancel();
            }
        }
        CancelGuard {
            reg: Arc::clone(self),
            id: id.to_string(),
            token,
        }
    }

    /// See [`enter`](Self::enter) for the publish-then-check pairing. A parked
    /// id not consumed here is consumed (and applied) by the next `enter` under
    /// the same id — ids name an engine request, and a cancelled request stays
    /// cancelled, so applying it to that later call is correct.
    fn cancel(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        self.precancelled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string());
        let map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tokens) = map.get(id) {
            // Every call in flight under this request id, not just the latest.
            for t in tokens {
                t.cancel();
            }
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
            {
                let mut map = self.reg.inflight.lock().unwrap_or_else(|e| e.into_inner());
                // Identity-checked: a request id names an engine request, not a
                // single call, so sibling calls' tokens share this entry — a
                // guard removes only its OWN token, never a sibling's.
                if let Some(tokens) = map.get_mut(&self.id) {
                    if let Some(i) = tokens.iter().position(|t| t.ptr_eq(&self.token)) {
                        tokens.swap_remove(i);
                    }
                    if tokens.is_empty() {
                        map.remove(&self.id);
                    }
                }
            }
            // A cancelled call consumes its request's parked pre-cancel marker,
            // so a cancelled request with no later `enter` doesn't park its id
            // forever (unbounded in a long-lived LSP/watch process). Safe to
            // consume: every token in flight under this id was already tripped
            // by `cancel`, and any *later* call under this (cancelled) request
            // gets re-cancelled by its own host-side `await_with_cancel`, which
            // observes the already-tripped request token on first poll and
            // fires `provider_cancel` again.
            if self.token.is_cancelled() {
                self.reg
                    .precancelled
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.id);
            }
        }
    }
}

/// Wraps an author `Provider` as a [`StableProvider`].
pub struct StableProviderImpl {
    pub provider: Arc<dyn Provider>,
    /// Provider name, for seam diagnostics (panic/abort error messages).
    name: Arc<str>,
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
    unary(Body::from(wire_error(
        pb::error::Kind::Unimplemented,
        format!("dispatch method {method} not implemented"),
    )))
}

// ---- Provider RPC bodies (moved verbatim out of the old per-method vtable slots
// into helpers; the dispatch impls below route method ids to them). Each takes
// its already-decoded request: the dispatch decodes eagerly (to derive the seam
// diagnostic key) before spawning the body onto the plugin runtime. ----

// Server-streaming: the provider's iterator is pulled lazily across the seam (one
// item per `StableItemStream::next`), never materialized into one blob.

async fn provider_list_stream(
    provider: Arc<dyn Provider>,
    req: pb::ListRequest,
    exec: DynExecutor,
) -> DynItemStream {
    let tok = StdCancellationToken::new();
    let executor: Arc<dyn ProviderExecutor> = Arc::new(GuestExecutor::new(exec));
    let lreq = (|| -> Result<ListRequest> {
        Ok(ListRequest {
            request_id: required(req.request_id, "ListRequest.request_id")?,
            package: PkgBuf::from(required(req.package, "ListRequest.package")?),
            states: convert::states_from_pb(req.states)?,
            executor,
        })
    })();
    let lreq = match lreq {
        Ok(r) => r,
        Err(e) => return error_item_stream(err_message(&e)),
    };
    match provider.list(lreq, &tok).await {
        Ok(iter) => make_item_stream(frame_iter(iter, |lr| {
            pb::ListResponse {
                addr: convert::addr_to_pb(&lr.addr).into(),
                facts: convert::listed_facts_to_pb(&lr.facts).into(),
                ..Default::default()
            }
            .encode_to_vec()
        })),
        Err(e) => error_item_stream(err_message(&e)),
    }
}

async fn provider_list_packages_stream(
    provider: Arc<dyn Provider>,
    req: pb::ListPackagesRequest,
) -> DynItemStream {
    let tok = StdCancellationToken::new();
    let prefix = match required(req.prefix, "ListPackagesRequest.prefix") {
        Ok(p) => p,
        Err(e) => return error_item_stream(err_message(&e)),
    };
    let lreq = ListPackagesRequest {
        prefix: PkgBuf::from(prefix),
    };
    match provider.list_packages(lreq, &tok).await {
        Ok(iter) => make_item_stream(frame_iter(iter, |lpr| {
            pb::ListPackageResponse {
                pkg: Some(lpr.pkg.as_str().to_string()),
                ..Default::default()
            }
            .encode_to_vec()
        })),
        Err(e) => error_item_stream(err_message(&e)),
    }
}

async fn provider_get(
    provider: Arc<dyn Provider>,
    req: pb::GetRequest,
    exec: DynExecutor,
    cancels: Arc<CancelRegistry>,
) -> SVec<u8> {
    let executor: Arc<dyn ProviderExecutor> = Arc::new(GuestExecutor::new(exec));
    let greq = (|| -> Result<GetRequest> {
        Ok(GetRequest {
            request_id: required(req.request_id, "GetRequest.request_id")?,
            addr: convert::addr_field(req.addr, "GetRequest.addr")?,
            states: convert::states_from_pb(req.states)?,
            executor,
        })
    })();
    let greq = match greq {
        Ok(r) => r,
        Err(e) => return unary(get_err(pb::get_error::Kind::Other, err_message(&e))),
    };
    let guard = cancels.enter(&greq.request_id);
    let body = match provider.get(greq, guard.token()).await {
        Ok(gr) => Body::from(pb::GetResponse {
            target_spec: convert::target_spec_to_pb(&gr.target_spec).into(),
            ..Default::default()
        }),
        Err(GetError::NotFound) => get_err(pb::get_error::Kind::NotFound, String::new()),
        Err(GetError::Other(e)) => get_err(get_error_kind(&e), err_message(&e)),
    };
    unary(body)
}

fn get_err(kind: pb::get_error::Kind, message: String) -> Body {
    Body::from(pb::GetError {
        kind: Some(kind.into()),
        message: Some(message),
        ..Default::default()
    })
}

async fn provider_probe(
    provider: Arc<dyn Provider>,
    req: pb::ProbeRequest,
    cancels: Arc<CancelRegistry>,
) -> SVec<u8> {
    let preq = (|| -> Result<ProbeRequest> {
        Ok(ProbeRequest {
            request_id: required(req.request_id, "ProbeRequest.request_id")?,
            package: PkgBuf::from(required(req.package, "ProbeRequest.package")?),
        })
    })();
    let preq = match preq {
        Ok(r) => r,
        Err(e) => return unary(err_body(err_message(&e))),
    };
    let guard = cancels.enter(&preq.request_id);
    let body = match provider.probe(preq, guard.token()).await {
        Ok(pr) => Body::from(pb::ProbeResponse {
            states: pr.states.iter().map(convert::state_to_pb).collect(),
            ..Default::default()
        }),
        Err(e) => err_body(err_message(&e)),
    };
    unary(body)
}

/// Serve one `FUNCTION_METHOD_CALL`: run the named handler guest-side (it is not
/// transmissible) and answer its outcome as a `CallFunctionResp` frame.
async fn plugin_call_function(
    func: Arc<dyn PluginFn>,
    name: String,
    req: pb::CallFunctionRequest,
) -> SVec<u8> {
    let call = match FunctionCall::from_pb(req) {
        Ok(c) => c,
        Err(e) => {
            return unary(err_body(err_message(
                &e.context(format!("the call of plugin function {name:?}")),
            )));
        }
    };
    let ctx = FnCallContext {
        pkg: &call.pkg,
        root: &call.root,
    };
    // Declarations ride back with the value when the host advertised that it
    // carries them; against an older host they are an error, never a silent
    // drop (see `CallFunctionRequest.accepts_declarations`).
    let res = match func.call(&ctx, call.args).await {
        Ok(o) => {
            let n = o.targets().len() + o.states().len();
            tracing::debug!(
                function = %name,
                pkg = %call.pkg,
                declarations = n,
                "plugin function answered"
            );
            // Only the refusal path errors here, so this context is about the
            // peer: name the side that is old and what fixes it.
            convert::call_function_response(o, call.accepts_declarations).with_context(|| {
                format!(
                    "plugin function {name:?}: the calling host does not carry declarations \
                     (heph older than plugin ABI 0.15.0) — upgrade heph"
                )
            })
        }
        Err(e) => Err(e).with_context(|| format!("plugin function {name:?}")),
    };
    let body = match res {
        Ok(resp) => Body::from(resp),
        Err(e) => err_body(err_message(&e)),
    };
    unary(body)
}

/// A `CallFunctionRequest`, decoded. `pkg` and `root` are required — a
/// defaulted `""` root would resolve the function's paths against the plugin's
/// cwd. An absent `accepts_declarations` is an older host, which cannot carry
/// declarations: `false`.
struct FunctionCall {
    pkg: String,
    root: std::path::PathBuf,
    args: FnArgs,
    accepts_declarations: bool,
}

impl FunctionCall {
    fn from_pb(req: pb::CallFunctionRequest) -> Result<Self> {
        Ok(Self {
            pkg: required(req.pkg, "CallFunctionRequest.pkg")?,
            root: required(req.root, "CallFunctionRequest.root")?.into(),
            args: convert::fn_args_from_pb(req.positional, req.named)?,
            accepts_declarations: req.accepts_declarations.unwrap_or(false),
        })
    }
}

// Sync metadata helpers. Each returns the metadatum's RAW protobuf bytes (NOT a
// `Frame` — matching the prior `state_schema()`/`config()` wire).

/// A `ConfigResponse` whose `name` is absent when `config()` failed, rather
/// than an empty name that reads as one.
fn config_response(name: Result<String>) -> SVec<u8> {
    SVec::from(
        pb::ConfigResponse {
            name: name.ok(),
            ..Default::default()
        }
        .encode_to_vec()
        .as_slice(),
    )
}

fn provider_config(provider: &Arc<dyn Provider>) -> SVec<u8> {
    config_response(provider.config(ConfigRequest {}).map(|r| r.name))
}

fn provider_state_schema(provider: &Arc<dyn Provider>) -> SVec<u8> {
    // Empty SVec == `None`; an encoded Schema == `Some` (see the ABI doc).
    match provider.state_schema() {
        Some(s) => SVec::from(convert::state_schema_to_pb(&s).encode_to_vec().as_slice()),
        None => SVec::new(),
    }
}

/// A plugin's functions as the ABI carries them: one [`NamedFunction`] per
/// function (its metadata as protobuf `pb::FunctionDef` bytes) and the one
/// [`DynPluginFunctions`] handle they are all called through — `None` when the
/// plugin has no functions. Goes into `PluginComponents.functions` /
/// `.function_handle`. `plugin` names the plugin in seam diagnostics.
///
/// Refuses a function name used twice: the host would refuse the plugin's
/// load anyway, and here the author sees it at build.
pub fn make_plugin_functions(
    plugin: &str,
    defs: Vec<PluginFnDef>,
) -> Result<(SVec<NamedFunction>, Option<DynPluginFunctions>)> {
    if defs.is_empty() {
        return Ok((SVec::new(), None));
    }
    let mut named = SVec::new();
    let mut fns: HashMap<String, Arc<dyn PluginFn>> = HashMap::with_capacity(defs.len());
    for def in defs {
        let meta = pb::FunctionDef {
            name: Some(def.name.clone()),
            signature: convert::fn_signature_to_pb(&def.signature).into(),
            doc: Some(def.doc),
            ..Default::default()
        }
        .encode_to_vec();
        named.push(NamedFunction {
            name: def.name.as_str().into(),
            meta: SVec::from(meta.as_slice()),
        });
        if fns.insert(def.name.clone(), def.func).is_some() {
            anyhow::bail!(
                "plugin {plugin:?} exports two functions named {:?}",
                def.name
            );
        }
    }
    let handle: DynPluginFunctions = dynify(stabby::boxed::Box::new(StablePluginFunctionsImpl {
        fns: Arc::new(fns),
        name: seam_name(Ok(plugin.to_string())),
    }));
    Ok((named, Some(handle)))
}

/// Serves a plugin's functions behind its one handle.
struct StablePluginFunctionsImpl {
    fns: Arc<HashMap<String, Arc<dyn PluginFn>>>,
    /// Plugin name, for seam diagnostics.
    name: Arc<str>,
}

// Each call runs through [`spawn_seam`], like `StableProviderImpl::invoke`: the
// host calls BUILD functions from a `block_on` inside Starlark evaluation, where
// no plugin reactor is current — a function touching tokio there would abort
// at the seam. On the plugin runtime it simply works.
impl StablePluginFunctions for StablePluginFunctionsImpl {
    extern "C" fn invoke<'a>(
        &'a self,
        method: u32,
        name: stabby::string::String,
        req: SVec<u8>,
    ) -> DynFuture<'a, SVec<u8>> {
        match method_id::<pb::FunctionMethod>(method) {
            Some(pb::FunctionMethod::Call) => {
                let name = name.to_string();
                let Some(func) = self.fns.get(&name).cloned() else {
                    let body = unary(err_body(format!(
                        "plugin {} has no function {name:?}",
                        self.name
                    )));
                    return dynify(stabby::boxed::Box::new(async move { body }));
                };
                let req =
                    match decode_req::<pb::CallFunctionRequest>(&req[..], "CallFunctionRequest") {
                        Ok(r) => r,
                        Err(e) => {
                            let body = unary(err_body(format!(
                                "decoding the call of function {name:?}: {e:#}"
                            )));
                            return dynify(stabby::boxed::Box::new(async move { body }));
                        }
                    };
                dynify(stabby::boxed::Box::new(spawn_seam(
                    &self.name,
                    "call_function",
                    name.clone(),
                    plugin_call_function(func, name, req),
                    |m| unary(err_body(m)),
                )))
            }
            _ => dynify(stabby::boxed::Box::new(
                async move { unimplemented(method) },
            )),
        }
    }
}

/// The registry handle a cdylib's create entry receives, as the
/// [`FunctionCaller`] the plugin's own functions use to call another plugin's
/// function by `(plugin, fn)`. Per instance: hand it to this instance's
/// functions, never to a process-global, or a second engine in the process
/// would call into the first one's functions.
pub fn guest_functions(reg: DynFunctionRegistry) -> Arc<dyn FunctionCaller> {
    Arc::new(GuestFunctions { reg })
}

impl StableMeta for StableProviderImpl {
    extern "C" fn meta(&self, kind: u32) -> SVec<u8> {
        match method_id::<pb::ProviderMethod>(kind) {
            Some(pb::ProviderMethod::Config) => provider_config(&self.provider),
            Some(pb::ProviderMethod::StateSchema) => provider_state_schema(&self.provider),
            // Unknown sync metadatum: empty == "none", never a hard failure.
            _ => SVec::new(),
        }
    }
}

// Entry-point dispatch. Every implemented method body is spawned onto the
// plugin's own runtime via [`spawn_seam`] — the returned future only awaits the
// `JoinHandle`, so the host worker polling it never executes plugin code (whose
// reactor/timer use would panic outside the plugin runtime), and a panicking
// body surfaces as an error instead of unwinding through the extern shim.
// Seam invariant (eager start): the body starts at `invoke*()` call time, before
// the host's first poll — see [`spawn_seam`].
impl StableProvider for StableProviderImpl {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let provider = Arc::clone(&self.provider);
        match method_id::<pb::ProviderMethod>(method) {
            Some(pb::ProviderMethod::Probe) => {
                let req = match decode_req::<pb::ProbeRequest>(&req[..], "ProbeRequest") {
                    Ok(r) => r,
                    Err(e) => {
                        let body = unary(err_body(err_message(&e)));
                        return dynify(stabby::boxed::Box::new(async move { body }));
                    }
                };
                let key = req.package.clone().unwrap_or_default();
                dynify(stabby::boxed::Box::new(spawn_seam(
                    &self.name,
                    "probe",
                    key,
                    provider_probe(provider, req, Arc::clone(&self.cancels)),
                    |m| unary(err_body(m)),
                )))
            }
            _ => dynify(stabby::boxed::Box::new(
                async move { unimplemented(method) },
            )),
        }
    }

    extern "C" fn invoke_server_stream<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
    ) -> DynFuture<'a, DynItemStream> {
        let provider = Arc::clone(&self.provider);
        match method_id::<pb::ProviderMethod>(method) {
            // `List` rides `invoke_exec_server_stream` (it needs an executor).
            Some(pb::ProviderMethod::ListPackages) => {
                let req =
                    match decode_req::<pb::ListPackagesRequest>(&req[..], "ListPackagesRequest") {
                        Ok(r) => r,
                        Err(e) => {
                            let stream = error_item_stream(err_message(&e));
                            return dynify(stabby::boxed::Box::new(async move { stream }));
                        }
                    };
                let key = req.prefix.clone().unwrap_or_default();
                dynify(stabby::boxed::Box::new(spawn_seam(
                    &self.name,
                    "list_packages",
                    key,
                    provider_list_packages_stream(provider, req),
                    error_item_stream,
                )))
            }
            _ => dynify(stabby::boxed::Box::new(async move {
                unimplemented_item_stream(method)
            })),
        }
    }

    extern "C" fn invoke_exec_server_stream<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
        exec: DynExecutor,
    ) -> DynFuture<'a, DynItemStream> {
        let provider = Arc::clone(&self.provider);
        match method_id::<pb::ProviderMethod>(method) {
            Some(pb::ProviderMethod::List) => {
                let req = match decode_req::<pb::ListRequest>(&req[..], "ListRequest") {
                    Ok(r) => r,
                    Err(e) => {
                        let stream = error_item_stream(err_message(&e));
                        return dynify(stabby::boxed::Box::new(async move { stream }));
                    }
                };
                let key = req.package.clone().unwrap_or_default();
                dynify(stabby::boxed::Box::new(spawn_seam(
                    &self.name,
                    "list",
                    key,
                    provider_list_stream(provider, req, exec),
                    error_item_stream,
                )))
            }
            _ => dynify(stabby::boxed::Box::new(async move {
                unimplemented_item_stream(method)
            })),
        }
    }

    extern "C" fn invoke_client_stream<'a>(
        &'a self,
        method: u32,
        // No client-streaming provider RPC yet; the request stream is dropped.
        _req: DynItemStream,
    ) -> DynFuture<'a, SVec<u8>> {
        dynify(stabby::boxed::Box::new(
            async move { unimplemented(method) },
        ))
    }

    extern "C" fn invoke_bidi<'a>(
        &'a self,
        method: u32,
        _req: DynItemStream,
    ) -> DynFuture<'a, DynItemStream> {
        dynify(stabby::boxed::Box::new(async move {
            unimplemented_item_stream(method)
        }))
    }

    extern "C" fn invoke_exec<'a>(
        &'a self,
        method: u32,
        req: SVec<u8>,
        exec: DynExecutor,
    ) -> DynFuture<'a, SVec<u8>> {
        let provider = Arc::clone(&self.provider);
        match method_id::<pb::ProviderMethod>(method) {
            Some(pb::ProviderMethod::Get) => {
                let req = match decode_req::<pb::GetRequest>(&req[..], "GetRequest") {
                    Ok(r) => r,
                    Err(e) => {
                        let body = unary(get_err(pb::get_error::Kind::Other, err_message(&e)));
                        return dynify(stabby::boxed::Box::new(async move { body }));
                    }
                };
                let key = pb_addr_key(req.addr.as_option());
                dynify(stabby::boxed::Box::new(spawn_seam(
                    &self.name,
                    "get",
                    key,
                    provider_get(provider, req, exec, Arc::clone(&self.cancels)),
                    // A panic/abort is a GetErr (not a bare Error) so the host's
                    // `get` decode surfaces the message as-is.
                    |m| unary(get_err(pb::get_error::Kind::Other, m)),
                )))
            }
            _ => dynify(stabby::boxed::Box::new(
                async move { unimplemented(method) },
            )),
        }
    }
}

/// Guest side of the engine's function registry: dispatches `call_registered`
/// back over the host callback, decoding the returned outcome.
struct GuestFunctions {
    reg: DynFunctionRegistry,
}

#[async_trait::async_trait]
impl FunctionCaller for GuestFunctions {
    async fn call(
        &self,
        plugin: &str,
        name: &str,
        ctx: &FnCallContext<'_>,
        args: FnArgs,
    ) -> Result<FnOutcome> {
        let (positional, named) = convert::fn_args_to_pb(&args);
        let pb_req = pb::CallRegisteredRequest {
            plugin: Some(plugin.to_string()),
            name: Some(name.to_string()),
            pkg: Some(ctx.pkg.to_string()),
            root: Some(ctx.root.to_string_lossy().into_owned()),
            positional,
            named,
            // This proxy hands the whole outcome back to its caller, so it can
            // carry declarations. Whether they reach the package is then up to
            // the calling function: it must `FnOutcome::absorb` them into its
            // own outcome, not read `.value` and drop the rest.
            accepts_declarations: Some(true),
            ..Default::default()
        }
        .encode_to_vec();
        let bytes = self
            .reg
            .call_registered(SVec::from(pb_req.as_slice()))
            .await;
        match decode_req::<pb::Frame>(&bytes[..], "the call_registered reply")?.body {
            Some(Body::CallFunctionResp(r)) => convert::fn_outcome_from_pb(*r),
            Some(Body::Error(e)) => anyhow::bail!("{}", e.message.unwrap_or_default()),
            other => anyhow::bail!("unexpected call_registered response: {other:?}"),
        }
    }
}

fn stream_err(message: String) -> Body {
    Body::from(pb::StreamEnd {
        error: wire_error(pb::error::Kind::Other, message).into(),
        ..Default::default()
    })
}

/// Wraps an author `ManagedDriver` as a [`StableManagedDriver`].
pub struct StableManagedDriverImpl {
    pub driver: Arc<dyn ManagedDriver>,
    /// Driver name, for seam diagnostics (panic/abort error messages).
    name: Arc<str>,
    cancels: Arc<CancelRegistry>,
}

impl StableCancel for StableManagedDriverImpl {
    extern "C" fn cancel(&self, request_id: stabby::string::String) {
        self.cancels.cancel(&request_id);
    }
}

fn driver_config(driver: &Arc<dyn ManagedDriver>) -> SVec<u8> {
    config_response(driver.config(DriverConfigRequest {}).map(|r| r.name))
}

fn driver_schema(driver: &Arc<dyn ManagedDriver>) -> SVec<u8> {
    SVec::from(
        convert::driver_schema_to_pb(&driver.schema())
            .encode_to_vec()
            .as_slice(),
    )
}

/// Run a driver call on the cdylib's own runtime and await its answer.
///
/// The future this returns is polled by a *host* worker thread, whose tokio
/// thread-locals belong to the host's separately-linked tokio instance — this
/// cdylib's copy sees no runtime there at all. Any reactor touch (a `proc_exec`
/// spawn, a timer) then panics, and a panic across the ABI seam is a
/// non-unwinding abort, not an error: the whole `heph` process dies.
///
/// `run` has always hopped for this reason. `parse` and `apply_transitive` must
/// too: a driver that probes its toolchain to build the cache key shells out
/// from `parse` (`docker_build` asks buildx for its default platform), which is the
/// same reactor touch one call earlier.
///
/// The `CancelGuard` moves into the task so the registry entry outlives the
/// call, and the borrowed token it hands the driver stays valid for it.
async fn on_plugin_runtime<F, Fut>(guard: CancelGuard, f: F) -> SVec<u8>
where
    F: FnOnce(CancelGuard) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Body> + Send,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    // The guard moves into the task: it must outlive the call it cancels, and
    // the token the driver borrows comes from it there.
    cdylib_runtime().spawn(async move {
        drop(tx.send(f(guard).await));
    });
    match rx.await {
        Ok(body) => unary(body),
        Err(_) => unary(err_body("plugin task dropped before completing".into())),
    }
}

/// A def a guest driver returned, encoded for the host — refused if it declares
/// an entrypoint the host cannot honour (R3), so an old host never builds and
/// caches one.
fn def_to_wire(def: &hplugin::driver::targetdef::TargetDef) -> Result<pb::TargetDef> {
    crate::capabilities::check_def(crate::capabilities::host_capabilities(), def)?;
    convert::target_def_to_pb(def)
}

async fn driver_parse(
    driver: Arc<dyn ManagedDriver>,
    req: pb::ParseRequest,
    cancels: Arc<CancelRegistry>,
) -> SVec<u8> {
    // `driver` is informational: each driver has its own handle.
    let preq = (|| -> Result<ParseRequest> {
        Ok(ParseRequest {
            request_id: required(req.request_id, "ParseRequest.request_id")?,
            target_spec: Arc::new(convert::target_spec_from_pb(required_msg(
                req.target_spec,
                "ParseRequest.target_spec",
            )?)?),
        })
    })();
    let preq = match preq {
        Ok(r) => r,
        Err(e) => return unary(err_body(err_message(&e))),
    };
    let guard = cancels.enter(&preq.request_id);
    on_plugin_runtime(guard, move |guard| async move {
        match driver.parse(preq, guard.token()).await {
            Ok(resp) => match def_to_wire(&resp.target_def) {
                Ok(td) => Body::from(pb::ParseResponse {
                    target_def: td.into(),
                    ..Default::default()
                }),
                Err(e) => err_body(err_message(&e)),
            },
            Err(e) => err_body(err_message(&e)),
        }
    })
    .await
}

async fn driver_apply_transitive(
    driver: Arc<dyn ManagedDriver>,
    req: pb::ApplyTransitiveRequest,
    cancels: Arc<CancelRegistry>,
) -> SVec<u8> {
    // `sandbox` is required too: an absent one would apply nothing and read as
    // success.
    let areq = (|| -> Result<ApplyTransitiveRequest> {
        Ok(ApplyTransitiveRequest {
            request_id: required(req.request_id, "ApplyTransitiveRequest.request_id")?,
            target_def: convert::target_def_from_pb(required_msg(
                req.target_def,
                "ApplyTransitiveRequest.target_def",
            )?)?,
            sandbox: convert::sandbox_from_pb(required_msg(
                req.sandbox,
                "ApplyTransitiveRequest.sandbox",
            )?)?,
        })
    })();
    let areq = match areq {
        Ok(r) => r,
        Err(e) => return unary(err_body(err_message(&e))),
    };
    let guard = cancels.enter(&areq.request_id);
    on_plugin_runtime(guard, move |guard| async move {
        match driver.apply_transitive(areq, guard.token()).await {
            Ok(resp) => match def_to_wire(&resp.target_def) {
                Ok(td) => Body::from(pb::ApplyTransitiveResponse {
                    target_def: td.into(),
                    ..Default::default()
                }),
                Err(e) => err_body(err_message(&e)),
            },
            Err(e) => err_body(err_message(&e)),
        }
    })
    .await
}

impl StableMeta for StableManagedDriverImpl {
    extern "C" fn meta(&self, kind: u32) -> SVec<u8> {
        match method_id::<pb::DriverMethod>(kind) {
            Some(pb::DriverMethod::Config) => driver_config(&self.driver),
            Some(pb::DriverMethod::Schema) => driver_schema(&self.driver),
            _ => SVec::new(),
        }
    }
}

// Same seam shape as [`StableProvider`]: bodies spawn onto the plugin runtime,
// the host polls only a `JoinHandle` await, panics become errors (never an
// unwind through the extern shim), and bodies start eagerly at call time.
impl StableManagedDriver for StableManagedDriverImpl {
    extern "C" fn invoke<'a>(&'a self, method: u32, req: SVec<u8>) -> DynFuture<'a, SVec<u8>> {
        let driver = Arc::clone(&self.driver);
        match method_id::<pb::DriverMethod>(method) {
            Some(pb::DriverMethod::Parse) => {
                let req = match decode_req::<pb::ParseRequest>(&req[..], "ParseRequest") {
                    Ok(r) => r,
                    Err(e) => {
                        let body = unary(err_body(err_message(&e)));
                        return dynify(stabby::boxed::Box::new(async move { body }));
                    }
                };
                let key = pb_addr_key(req.target_spec.as_option().and_then(|t| t.addr.as_option()));
                dynify(stabby::boxed::Box::new(spawn_seam(
                    &self.name,
                    "parse",
                    key,
                    driver_parse(driver, req, Arc::clone(&self.cancels)),
                    |m| unary(err_body(m)),
                )))
            }
            Some(pb::DriverMethod::ApplyTransitive) => {
                let req = match decode_req::<pb::ApplyTransitiveRequest>(
                    &req[..],
                    "ApplyTransitiveRequest",
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        let body = unary(err_body(err_message(&e)));
                        return dynify(stabby::boxed::Box::new(async move { body }));
                    }
                };
                let key = pb_addr_key(req.target_def.as_option().and_then(|t| t.addr.as_option()));
                dynify(stabby::boxed::Box::new(spawn_seam(
                    &self.name,
                    "apply_transitive",
                    key,
                    driver_apply_transitive(driver, req, Arc::clone(&self.cancels)),
                    |m| unary(err_body(m)),
                )))
            }
            _ => dynify(stabby::boxed::Box::new(
                async move { unimplemented(method) },
            )),
        }
    }

    // No unary->stream or stream->unary driver RPC yet; provisioned, Unimplemented.
    extern "C" fn invoke_server_stream<'a>(
        &'a self,
        method: u32,
        _req: SVec<u8>,
    ) -> DynFuture<'a, DynItemStream> {
        dynify(stabby::boxed::Box::new(async move {
            unimplemented_item_stream(method)
        }))
    }

    extern "C" fn invoke_client_stream<'a>(
        &'a self,
        method: u32,
        _req: DynItemStream,
    ) -> DynFuture<'a, SVec<u8>> {
        dynify(stabby::boxed::Box::new(
            async move { unimplemented(method) },
        ))
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
        dynify(stabby::boxed::Box::new(async move {
            match method_id::<pb::DriverMethod>(method) {
                Some(pb::DriverMethod::Run) => run_bidi(driver, req, cancels).await,
                _ => unimplemented_item_stream(method),
            }
        }))
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
    dynify(stabby::boxed::Box::new(ChannelItemStream {
        rx: Mutex::new(rx),
    }))
}

fn run_out_err(msg: String) -> pb::RunOutFrame {
    pb::RunOutFrame {
        msg: Some(pb::run_out_frame::Msg::Error(msg)),
        ..Default::default()
    }
}

/// A run response stream that fails immediately (one `RunOutFrame{error}`).
fn run_error_stream(msg: String) -> DynItemStream {
    make_item_stream(Box::new(std::iter::once(run_out_err(msg).encode_to_vec())))
}

/// Pull the first request-stream item: the run request (`RunInFrame{start}`),
/// with its `request_id` — required, since it is what a cancel names.
fn pull_run_start(req: &DynItemStream) -> Result<(String, pb::ManagedRunRequest)> {
    let bytes = req.next();
    if bytes.is_empty() {
        anyhow::bail!("run: missing start frame");
    }
    match decode_req::<pb::RunInFrame>(&bytes[..], "the run start frame")?.msg {
        Some(pb::run_in_frame::Msg::Start(mut s)) => {
            let id = required(s.request_id.take(), "ManagedRunRequest.request_id")?;
            Ok((id, *s))
        }
        // Named, not Debug-printed: a stdin chunk is the target's input bytes,
        // which have no place in an error message.
        Some(pb::run_in_frame::Msg::StdinChunk(c)) => anyhow::bail!(
            "run: the first frame is a stdin chunk ({} bytes), not a start frame",
            c.len()
        ),
        Some(pb::run_in_frame::Msg::StdinClose(_)) => {
            anyhow::bail!("run: the first frame is a stdin close, not a start frame")
        }
        Some(pb::run_in_frame::Msg::Signal(signo)) => {
            anyhow::bail!("run: the first frame is a signal ({signo}), not a start frame")
        }
        Some(pb::run_in_frame::Msg::Cont(_)) => {
            anyhow::bail!("run: the first frame is a continue, not a start frame")
        }
        None => anyhow::bail!("run: the first frame is empty, not a start frame"),
    }
}

async fn run_bidi(
    driver: Arc<dyn ManagedDriver>,
    req: DynItemStream,
    cancels: Arc<CancelRegistry>,
) -> DynItemStream {
    let (request_id, start) = match pull_run_start(&req) {
        Ok(s) => s,
        Err(e) => return run_error_stream(err_message(&e)),
    };
    let mut start = match RunStart::from_pb(start) {
        Ok(s) => s,
        Err(e) => return run_error_stream(err_message(&e)),
    };
    // A run in the caller's terminal keeps reading the request stream for the
    // signals the host relays. Only that run: a captured one has nothing more to
    // read, and drops the stream here.
    if let Some(signals) = start.channels.signals.take() {
        spawn_signal_reader(req, signals, start.channels.continues.take());
    }
    let stops = start.channels.stops.take();
    // The run shells out via the reactor — execute on the cdylib's own runtime,
    // feeding RunOutFrames into a channel the host drains. (Live stdin/stdout will
    // ride `req` / the channel later; today only the terminal result is sent.)
    // `spawn_on_with_cycle_ctx` (not a bare `spawn`) so the run task inherits the
    // caller's memoizer frame chain for cycle detection.
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    hcore::hmemoizer::spawn_on_with_cycle_ctx(cdylib_runtime().handle(), async move {
        let guard = cancels.enter(&request_id);
        // The tool's stops ride the response stream too, on a run with a PTY.
        // A task of its own: `run_once` holds the run's whole lifetime.
        let stop_forward = stops.map(|mut stops| {
            let tx = tx.clone();
            tokio::spawn(async move {
                while stops.recv().await.is_some() {
                    let frame = pb::RunOutFrame {
                        msg: Some(pb::run_out_frame::Msg::Stopped(true)),
                        ..Default::default()
                    };
                    drop(tx.send(frame.encode_to_vec()).await);
                }
            })
        });
        // `tx` carries both the live output frames and the terminal one, in that
        // order, so the host sees the log before the result that ends the stream.
        let out = run_once(driver, request_id, start, &tx, guard.token()).await;
        if let Some(stop_forward) = stop_forward {
            stop_forward.abort();
        }
        // Host gone => receiver dropped; ignore send failure.
        drop(tx.send(out.encode_to_vec()).await);
    });
    make_channel_item_stream(rx)
}

/// Feed every `RunInFrame.signal` on the rest of the request stream into the
/// run's [`Foreground::signals`](hplugin::driver::Foreground::signals), until
/// the stream ends.
///
/// The stream's `next` blocks, so this is its own thread, not a task: a task
/// would park a runtime worker for the whole run. It ends when the host ends
/// the stream, which a host does only after it read the run's terminal frame
/// (N11) — and an older host does right after `start`. Either way the end drops
/// the sender, which the driver reads as "no more signals", never a cancel. The
/// run's terminal frame never waits on this thread. On a PTY run it carries each
/// `RunInFrame.cont` into [`PtyStdio::continues`](hplugin::driver::PtyStdio::continues).
fn spawn_signal_reader(
    req: DynItemStream,
    signals: hplugin::driver::SignalSender,
    continues: Option<hplugin::driver::ContinueSender>,
) {
    let spawned = std::thread::Builder::new()
        .name("heph-run-signals".to_string())
        .spawn(move || read_signal_frames(&req, &signals, continues.as_ref()));
    if let Err(e) = spawned {
        // The run still goes ahead; it only loses the relay.
        tracing::warn!(error = %e, "could not start the run's signal reader; signals sent to heph will not reach the tool");
    }
}

fn read_signal_frames(
    req: &DynItemStream,
    signals: &hplugin::driver::SignalSender,
    continues: Option<&hplugin::driver::ContinueSender>,
) {
    loop {
        let bytes = req.next();
        if bytes.is_empty() {
            return;
        }
        let frame = match decode_req::<pb::RunInFrame>(&bytes[..], "a run request frame") {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "dropping an undecodable run request frame");
                continue;
            }
        };
        match frame.msg {
            Some(pb::run_in_frame::Msg::Signal(signo)) => {
                // The driver dropped its receiver: its child is gone, and
                // nothing left on the stream is for it.
                if signals
                    .send(hplugin::driver::Signal::from_raw(signo))
                    .is_err()
                {
                    return;
                }
            }
            Some(pb::run_in_frame::Msg::Cont(_)) => {
                if let Some(continues) = continues {
                    let _forwarded = continues.send(());
                }
            }
            // Live stdin is not wired; an inherited run reads the terminal itself.
            Some(pb::run_in_frame::Msg::StdinChunk(_) | pb::run_in_frame::Msg::StdinClose(_)) => {}
            Some(pb::run_in_frame::Msg::Start(_)) => {
                tracing::warn!("ignoring a second start frame on a run's request stream");
            }
            None => {}
        }
    }
}

/// A run's terminal failure frame: the message, plus the process's exit status
/// when the failure is one ([`hplugin::error::ProcessFailed::exit`]). `error`
/// stays the terminal frame, so an older host still reports the message.
fn run_out_failure(e: &anyhow::Error) -> pb::RunOutFrame {
    let mut frame = run_out_err(err_message(e));
    if let Some(exit) = hplugin::error::process_exit(e) {
        frame.exit = convert::exit_to_pb(exit).into();
    }
    frame
}

/// An `AsyncWrite` that turns everything written to it into `RunOutFrame`s on the
/// run's response channel — one frame per write, tagged stdout or stderr.
///
/// This is what makes a driver's subprocess output visible when the driver is a
/// cdylib. In-process, a driver writes straight to the sinks the engine handed
/// it; across the ABI seam there is nothing to hand it, so the bytes have to
/// travel as stream frames and be re-attached to the target's stdio on the host
/// side. Without it a `docker buildx build` prints its whole progress log into a
/// channel nobody reads, and the user watches an idle spinner for minutes.
struct FrameSink {
    tx: tokio_util::sync::PollSender<Vec<u8>>,
    stderr: bool,
}

impl FrameSink {
    fn new(tx: &tokio::sync::mpsc::Sender<Vec<u8>>, stderr: bool) -> Self {
        FrameSink {
            tx: tokio_util::sync::PollSender::new(tx.clone()),
            stderr,
        }
    }
}

impl tokio::io::AsyncWrite for FrameSink {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // Reserving (rather than `try_send`) applies the channel's backpressure
        // to the child: a host that stops reading slows the writer instead of
        // dropping its output on the floor.
        match self.tx.poll_reserve(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            // Host gone. Report the write as done rather than erroring: losing
            // the log tail must not fail a build that otherwise succeeded.
            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Ok(buf.len())),
            std::task::Poll::Ready(Ok(())) => {
                let msg = if self.stderr {
                    pb::run_out_frame::Msg::StderrChunk(buf.to_vec())
                } else {
                    pb::run_out_frame::Msg::StdoutChunk(buf.to_vec())
                };
                let frame = pb::RunOutFrame {
                    msg: Some(msg),
                    ..Default::default()
                }
                .encode_to_vec();
                drop(self.tx.send_item(frame));
                std::task::Poll::Ready(Ok(buf.len()))
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// The decoded parts of a `ManagedRunRequest`. The target, `hashin` and every
/// path are required: a defaulted `""` sandbox dir would run the target in the
/// plugin's cwd, and a defaulted `hashin` would key it as nothing. `shell` is
/// off unless said. `driver` is informational (each driver has its own handle).
struct RunStart {
    shell: bool,
    target: hplugin::driver::targetdef::TargetDef,
    hashin: String,
    tree_root_path: PathBuf,
    sandbox_dir: PathBuf,
    sandbox_ws_dir: PathBuf,
    sandbox_pkg_dir: PathBuf,
    inputs: Vec<ManagedRunInput>,
    credentials: Vec<hplugin::driver::CredentialMount>,
    scratch: Vec<hplugin::driver::ScratchMount>,
    deferred: std::collections::BTreeMap<String, hplugin::driver::DeferredValue>,
    /// The run's caller-side options; default for every build.
    options: hplugin::driver::RunOptions,
    /// What the run's streams feed, for a run in the caller's terminal.
    channels: convert::RunChannels,
}

impl RunStart {
    fn from_pb(req: pb::ManagedRunRequest) -> Result<Self> {
        let path = |v: Option<String>, field| required(v, field).map(PathBuf::from);
        // A cdylib shares the host's process, so a PTY slave's number is good.
        let (options, channels) = convert::run_options_from_pb(
            req.options.into_option(),
            convert::FdScope::SharedProcess,
        )
        .context("decoding the run's options")?;
        Ok(Self {
            options,
            channels,
            shell: req.shell.unwrap_or(false),
            target: convert::target_def_from_pb(required_msg(
                req.target,
                "ManagedRunRequest.target",
            )?)?,
            hashin: required(req.hashin, "ManagedRunRequest.hashin")?,
            tree_root_path: path(req.tree_root_path, "ManagedRunRequest.tree_root_path")?,
            sandbox_dir: path(req.sandbox_dir, "ManagedRunRequest.sandbox_dir")?,
            sandbox_ws_dir: path(req.sandbox_ws_dir, "ManagedRunRequest.sandbox_ws_dir")?,
            sandbox_pkg_dir: path(req.sandbox_pkg_dir, "ManagedRunRequest.sandbox_pkg_dir")?,
            inputs: req
                .inputs
                .into_iter()
                .map(managed_input_from_pb)
                .collect::<Result<_>>()
                .context("decoding the run's inputs")?,
            // Fallible and never lossy: a credential dropped here would leave the
            // target with no identity, which fails much later somewhere else — or
            // succeeds against whatever ambient identity the host happened to have.
            credentials: req
                .credentials
                .into_iter()
                .map(convert::credential_mount_from_pb)
                .collect::<Result<_>>()?,
            scratch: req
                .scratch
                .into_iter()
                .filter_map(convert::scratch_mount_from_pb)
                .collect(),
            // The wire carries finished values and nothing else: a `NeedsSandbox`
            // is completed host-side before serialization, and `load_stable`
            // refuses to serialize one that is not. So every entry arriving here
            // is `Ready` by construction rather than by assumption.
            deferred: req
                .deferred
                .into_iter()
                .map(|(k, v)| (k, hplugin::driver::DeferredValue::Ready(v)))
                .collect(),
        })
    }
}

async fn run_once(
    driver: Arc<dyn ManagedDriver>,
    request_id: String,
    start: RunStart,
    out: &tokio::sync::mpsc::Sender<Vec<u8>>,
    ct: &StdCancellationToken,
) -> pb::RunOutFrame {
    // `shell` selects run_shell over run; it rides the request.
    let shell = start.shell;
    // The run request and the managed layer each own a handle on the same
    // on-disk content.
    let run_inputs: Vec<RunInput> = start.inputs.iter().map(|mi| mi.input.clone()).collect();
    // The driver's subprocess output rides the response stream back to the host,
    // which re-attaches it to the target's stdio. Handing it `None` here is what
    // made a cdylib driver's build log disappear.
    let mut stdout_sink = FrameSink::new(out, false);
    let mut stderr_sink = FrameSink::new(out, true);
    let mut rr = RunRequest::new(&request_id, &start.target, start.sandbox_dir.clone());
    rr.tree_root_path = start.tree_root_path;
    rr.inputs = run_inputs;
    rr.hashin = start.hashin.as_str();
    // A run in the caller's terminal has no output to carry: the process
    // writes to the fds it inherited.
    if matches!(start.options.stdio, hplugin::driver::RunStdio::Captured) {
        rr.stdout = Some(&mut stdout_sink);
        rr.stderr = Some(&mut stderr_sink);
    }
    rr.scratch = start.scratch;
    rr.credentials = start.credentials;
    rr.deferred = start.deferred;
    rr.options = start.options;
    let mrr = ManagedRunRequest {
        request: rr,
        sandbox_dir: start.sandbox_dir,
        sandbox_ws_dir: start.sandbox_ws_dir,
        sandbox_pkg_dir: start.sandbox_pkg_dir,
        inputs: start.inputs,
    };
    let result = if shell {
        driver.run_shell(mrr, ct).await
    } else {
        driver.run(mrr, ct).await
    };
    match result {
        Ok(resp) => match resp
            .artifacts
            .iter()
            .map(convert::output_artifact_to_pb)
            .collect::<anyhow::Result<Vec<_>>>()
        {
            Ok(artifacts) => pb::RunOutFrame {
                msg: Some(pb::run_out_frame::Msg::from(pb::ManagedRunResponse {
                    artifacts,
                    ..Default::default()
                })),
                ..Default::default()
            },
            Err(e) => run_out_err(err_message(&e)),
        },
        Err(e) => run_out_failure(&e),
    }
}

/// A managed run input, decoded. `type`, `source_addr` and `unpack_root` are
/// required; an absent `list_path` is a Support input (which has none) and an
/// absent `hashout` is one the host could not read — the in-process content
/// reports that as `""`, as it always has.
fn managed_input_from_pb(mi: pb::ManagedRunInput) -> Result<ManagedRunInput> {
    let ty = match convert::required_enum(mi.r#type, "ManagedRunInput.type")? {
        pb::InputArtifactType::Dep => inputartifact::Type::Dep,
        pb::InputArtifactType::Support => inputartifact::Type::Support,
        pb::InputArtifactType::Unspecified => {
            anyhow::bail!("field ManagedRunInput.type is INPUT_ARTIFACT_TYPE_UNSPECIFIED")
        }
    };
    let unpack_root = PathBuf::from(required(mi.unpack_root, "ManagedRunInput.unpack_root")?);
    let list_path = absent_not_empty(mi.list_path, "ManagedRunInput.list_path")?.map(PathBuf::from);
    let origin_id = mi.origin_id.unwrap_or_default();
    let input = RunInput {
        artifact: inputartifact::InputArtifact {
            r#type: ty,
            origin_id: origin_id.clone(),
            // The host already materialized this input onto the shared filesystem
            // before invoking the driver. Back the content by those on-disk files
            // so a driver may read it (`walk`/`reader`) just like in-process —
            // no bytes are re-shipped over the boundary.
            content: Arc::new(DiskInputContent {
                unpack_root: unpack_root.clone(),
                list_path: list_path.clone(),
                hashout: absent_not_empty(mi.hashout, "ManagedRunInput.hashout")?
                    .unwrap_or_default(),
            }),
        },
        origin_id,
        source_addr: convert::addr_field(mi.source_addr, "ManagedRunInput.source_addr")?,
        filters: mi.filters,
        annotations: mi.annotations.into_iter().collect(),
    };
    Ok(ManagedRunInput {
        input,
        list_path,
        unpack_root,
    })
}

/// [`Content`] for a managed run input, backed by the files the host already
/// materialized onto the shared filesystem under `unpack_root`. `list_path` (Dep
/// inputs) names the exact absolute paths of this input's files; without it
/// (Support inputs) the whole `unpack_root` tree is walked. `walk` reads those
/// files from disk; `reader` re-tars them (artifacts are tar by convention).
struct DiskInputContent {
    unpack_root: PathBuf,
    list_path: Option<PathBuf>,
    /// As the host sent it. Not recomputed from the files here: that would hash
    /// the unpacked bytes, not the artifact the consumer's hashin folded in.
    hashout: String,
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
            let size = f
                .metadata()
                .with_context(|| format!("stat {}", abs.display()))?
                .len();
            Ok(WalkEntry {
                path: rel,
                kind: WalkEntryKind::File {
                    data: Box::new(f),
                    x,
                    size,
                },
            })
        });
        Ok(Box::new(iter))
    }

    fn hashout(&self) -> Result<String> {
        // Empty when the host could not read it, or predates the field.
        Ok(self.hashout.clone())
    }
}

// ---- Hook (build-event consumer) ----

/// Wrap an author `Hook` as an ABI-stable [`hplugin_stabby::abi::DynHook`].
pub fn make_dyn_hook(hook: Arc<dyn Hook>) -> hplugin_stabby::abi::DynHook {
    dynify(stabby::boxed::Box::new(StableHookImpl { hook }))
}

/// Wraps an author `Hook` as a [`StableHook`].
pub struct StableHookImpl {
    pub hook: Arc<dyn Hook>,
}

impl StableMeta for StableHookImpl {
    extern "C" fn meta(&self, kind: u32) -> SVec<u8> {
        match method_id::<pb::HookMethod>(kind) {
            Some(pb::HookMethod::Config) => config_response(Ok(self.hook.name())),
            // Unknown sync metadatum: empty == "none", never a hard failure.
            _ => SVec::new(),
        }
    }
}

impl StableHook for StableHookImpl {
    extern "C" fn invoke_client_stream<'a>(
        &'a self,
        method: u32,
        req: DynItemStream,
    ) -> DynFuture<'a, SVec<u8>> {
        let hook = Arc::clone(&self.hook);
        dynify(stabby::boxed::Box::new(async move {
            match method_id::<pb::HookMethod>(method) {
                Some(pb::HookMethod::OnEvents) => hook_on_events(hook, req).await,
                _ => unimplemented(method),
            }
        }))
    }
}

/// Decode one host->plugin event frame into a `BuildEvent`. `None` ends the pull
/// loop (a `StreamEnd`, an error, or a non-item frame). Events ride as serde-JSON
/// inside `StreamItem.item` — the `BuildEvent` type is already serde, so there is
/// no parallel proto mirror to keep in lockstep.
fn decode_event_frame(bytes: &[u8]) -> Option<hcore::events::BuildEvent> {
    match plugin_abi::decode::<pb::Frame>(bytes).ok()?.body? {
        Body::StreamItem(si) => serde_json::from_slice(si.item.as_deref()?).ok(),
        _ => None,
    }
}

/// Consume the client-streamed events: pull each frame on a blocking thread (the
/// host's stream `next` blocks until the next event arrives), hand it to the
/// author hook, then signal end-of-stream. The reply is an empty ack — the host
/// only needs this future to resolve, which means the plugin drained the full
/// stream and ran its final flush.
async fn hook_on_events(hook: Arc<dyn Hook>, req: DynItemStream) -> SVec<u8> {
    let handle = cdylib_runtime().spawn_blocking(move || {
        loop {
            let bytes = req.next();
            // Empty == the host closed the stream (request finished).
            if bytes.is_empty() {
                break;
            }
            match decode_event_frame(&bytes) {
                Some(ev) => hook.on_event(&ev),
                None => break,
            }
        }
        hook.on_close();
    });
    // Wait for the blocking pull loop to finish (stream fully drained + the hook's
    // final flush done) before acking the host.
    drop(handle.await);
    SVec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write as _;

    /// `list_path` is absent for a Support input (which has none) and names
    /// the file list for a Dep one; a present-but-empty one is malformed, not a
    /// second spelling of "none".
    #[test]
    fn a_managed_input_list_path_is_absent_or_real_never_empty() {
        let base = |ty: pb::InputArtifactType| pb::ManagedRunInput {
            r#type: Some(ty.into()),
            source_addr: convert::addr_to_pb(
                &hmodel::htaddr::parse_addr("//app:img").expect("addr"),
            )
            .into(),
            unpack_root: Some("/u".into()),
            ..Default::default()
        };
        let support = managed_input_from_pb(base(pb::InputArtifactType::Support)).expect("support");
        assert_eq!(support.list_path, None);

        let mut dep = base(pb::InputArtifactType::Dep);
        dep.list_path = Some("/u/list".into());
        let dep = managed_input_from_pb(dep).expect("dep");
        assert_eq!(dep.list_path, Some(PathBuf::from("/u/list")));

        let mut empty = base(pb::InputArtifactType::Support);
        empty.list_path = Some(String::new());
        let err = managed_input_from_pb(empty)
            .err()
            .expect("present-empty list_path");
        assert!(
            format!("{err:#}").contains("ManagedRunInput.list_path is present but empty"),
            "{err:#}"
        );
    }

    /// The first run frame must be the start frame; a stdin chunk there is
    /// refused without its bytes reaching the error.
    #[test]
    fn a_stdin_chunk_before_the_start_frame_is_refused_without_its_bytes() {
        let frame = pb::RunInFrame {
            msg: Some(pb::run_in_frame::Msg::StdinChunk(b"s3cr3t".to_vec())),
            ..Default::default()
        };
        let stream = make_item_stream(Box::new(std::iter::once(frame.encode_to_vec())));
        let err = pull_run_start(&stream).expect_err("not a start frame");
        let msg = format!("{err:#}");
        assert!(msg.contains("stdin chunk (6 bytes)"), "{msg}");
        assert!(!msg.contains("s3cr3t") && !msg.contains("115"), "{msg}");
    }

    /// A target def with every required field, for tests that must get past
    /// decoding (an empty raw_def blob fails JSON parse, so a trivial `{}`).
    fn wire_target() -> pb::TargetDef {
        pb::TargetDef {
            addr: convert::addr_to_pb(&hmodel::htaddr::Addr::new(
                PkgBuf::from("p"),
                "t".into(),
                Default::default(),
            ))
            .into(),
            raw_def: pb::RawDefBlob {
                format: Some(pb::raw_def_blob::Format::Json.into()),
                data: Some(b"{}".to_vec()),
                ..Default::default()
            }
            .into(),
            cache: pb::CacheConfig {
                enabled: Some(true),
                remote_enabled: Some(false),
                history: Some(1),
                ..Default::default()
            }
            .into(),
            hash: Some(vec![1]),
            ..Default::default()
        }
    }

    /// A run request with every required field.
    fn wire_run(target: pb::TargetDef) -> pb::ManagedRunRequest {
        pb::ManagedRunRequest {
            request_id: Some("r".into()),
            target: target.into(),
            tree_root_path: Some("/ws".into()),
            hashin: Some("h".into()),
            sandbox_dir: Some("/sb".into()),
            sandbox_ws_dir: Some("/sb/ws".into()),
            sandbox_pkg_dir: Some("/sb/ws/p".into()),
            ..Default::default()
        }
    }

    /// `run_once` from the wire request, decoding it as `run_bidi` does.
    async fn run_once_wire(
        driver: Arc<dyn ManagedDriver>,
        request_id: String,
        req: pb::ManagedRunRequest,
        out: &tokio::sync::mpsc::Sender<Vec<u8>>,
        ct: &StdCancellationToken,
    ) -> pb::RunOutFrame {
        match RunStart::from_pb(req) {
            Ok(start) => run_once(driver, request_id, start, out, ct).await,
            Err(e) => run_out_err(err_message(&e)),
        }
    }

    fn start_frame(req: pb::ManagedRunRequest) -> Vec<u8> {
        pb::RunInFrame {
            msg: Some(pb::run_in_frame::Msg::from(req)),
            ..Default::default()
        }
        .encode_to_vec()
    }

    /// A run request missing a required field fails the run naming it — it
    /// used to default, and a `""` sandbox dir ran the target in the plugin's
    /// cwd.
    #[test]
    fn a_run_request_missing_a_path_is_refused() {
        use hcore::hasync::Cancellable;
        use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};

        let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        struct Unreached;
        #[async_trait::async_trait]
        impl ManagedDriver for Unreached {
            fn config(
                &self,
                _r: hplugin::driver::ConfigRequest,
            ) -> Result<hplugin::driver::ConfigResponse> {
                anyhow::bail!("unused")
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
                panic!("a malformed request must not reach the driver")
            }
        }
        let mut req = wire_run(wire_target());
        req.sandbox_dir = None;
        let frame = futures::executor::block_on(run_once_wire(
            Arc::new(Unreached) as Arc<dyn ManagedDriver>,
            "r".into(),
            req,
            &out_tx,
            &StdCancellationToken::new(),
        ));
        match frame.msg {
            Some(pb::run_out_frame::Msg::Error(msg)) => assert!(
                msg.contains("missing field ManagedRunRequest.sandbox_dir"),
                "{msg}"
            ),
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    // A managed input's content is readable from the files the host materialized
    // under unpack_root, scoped to this input's files via the list file.
    /// A guest driver reads the hashout the host sent, not an empty string:
    /// `oci_push`'s `${image_hashout}` runs on the far side of this seam.
    #[test]
    fn a_run_input_carries_the_hosts_hashout() {
        let wire = |hashout: Option<&str>| pb::ManagedRunInput {
            r#type: Some(pb::InputArtifactType::Dep.into()),
            origin_id: Some("image".to_string()),
            source_addr: convert::addr_to_pb(
                &hmodel::htaddr::parse_addr("//app:img").expect("addr"),
            )
            .into(),
            unpack_root: Some("/unused".to_string()),
            hashout: hashout.map(str::to_string),
            ..Default::default()
        };
        let input = managed_input_from_pb(wire(Some("abc123"))).expect("decode");
        assert_eq!(
            input.input.artifact.content.hashout().expect("hashout"),
            "abc123"
        );
        // Absent is "the host could not read one", which the content reports
        // as `""`; a present empty one is no longer a way to say that.
        let none = managed_input_from_pb(wire(None)).expect("decode");
        assert_eq!(none.input.artifact.content.hashout().expect("hashout"), "");
        assert!(managed_input_from_pb(wire(Some(""))).is_err());
        // A Dep input with no `source_addr` is malformed, not `//:`.
        let mut no_addr = wire(None);
        no_addr.source_addr = buffa::MessageField::none();
        let err = managed_input_from_pb(no_addr)
            .err()
            .expect("no source_addr");
        assert!(
            format!("{err:#}").contains("missing field ManagedRunInput.source_addr"),
            "{err:#}"
        );
    }

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
            hashout: String::new(),
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
    use hplugin::function::{DeclaredState, DeclaredTarget, FunctionRegistry, FunctionSlot};
    use hplugin::provider::{
        ConfigResponse, GetResponse, ListPackageResponse, ListResponse, ProbeResponse,
    };
    use hplugin_stabby::load_stable::decode_functions;
    use std::path::Path;

    // A provider with a state schema and no functions: functions are the
    // plugin's, not the provider's.
    struct FnProvider;

    /// `echo(msg, times=1)`, whose handler reads the call context's `pkg` — so
    /// the tests prove both the call arguments and the FnCallContext cross the
    /// seam, not just the metadata.
    fn echo_def() -> PluginFnDef {
        PluginFnDef {
            name: "echo".into(),
            signature: FnSignature {
                positional: vec![Param::required("msg", ParamType::String)],
                named: vec![Param::optional("times", ParamType::Int, Value::Int(1))],
                variadic: None,
                returns: ParamType::String,
            },
            doc: "Echo `msg` `times` times, prefixed by the calling package.".into(),
            func: Arc::new(EchoFn),
        }
    }

    /// A plugin's functions through the seam and back, as the loader sees
    /// them: metadata crossed as `NamedFunction` bytes, decoded once, each
    /// calling through the plugin's one handle.
    fn through_the_seam(plugin: &str, defs: Vec<PluginFnDef>) -> Vec<PluginFnDef> {
        let (named, handle) = make_plugin_functions(plugin, defs).expect("export");
        decode_functions(plugin, named.into_iter().collect(), handle).expect("load")
    }

    fn call(def: &PluginFnDef, pkg: &str, positional: Vec<Value>) -> Result<FnOutcome> {
        let root = std::path::PathBuf::from("/ws");
        let ctx = FnCallContext {
            pkg,
            root: Path::new(&root),
        };
        futures::executor::block_on(def.func.call(
            &ctx,
            FnArgs {
                positional,
                named: Default::default(),
            },
        ))
    }

    struct EchoFn;
    #[async_trait::async_trait]
    impl PluginFn for EchoFn {
        async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> Result<FnOutcome> {
            let msg = match args.positional.first() {
                Some(Value::String(s)) => s.clone(),
                _ => anyhow::bail!("echo: `msg` must be a string"),
            };
            // A declaring call: every field of a declaration must survive the
            // seam, and against a caller that cannot carry them it must fail.
            if msg == "declare" {
                let mut out = FnOutcome::from(Value::String("//mypkg:t".into()));
                out.declare_target(DeclaredTarget {
                    name: "t".into(),
                    driver: "exec".into(),
                    labels: vec!["gen".into()],
                    transitive: Value::Map(
                        [(
                            "deps".to_string(),
                            Value::Map(
                                [(
                                    "g".to_string(),
                                    Value::List(vec![Value::String(":sib".into())]),
                                )]
                                .into(),
                            ),
                        )]
                        .into(),
                    ),
                    approval: hplugin::provider::Approval {
                        required: true,
                        notice: vec!["sec".into()],
                    },
                    config: [("run".to_string(), Value::String("gen".into()))].into(),
                })
                .declare_state(DeclaredState {
                    provider: "codegen".into(),
                    args: [("toolchain".to_string(), Value::String("v1".into()))].into(),
                });
                return Ok(out);
            }
            let times = match args.named.get("times") {
                Some(Value::Int(n)) => *n,
                _ => 1,
            };
            Ok(Value::String(format!(
                "{}:{}",
                ctx.pkg,
                msg.repeat(usize::try_from(times).unwrap_or(0))
            ))
            .into())
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
        match pb::Frame::decode_from_slice(&bytes[..])
            .expect("frame")
            .body
        {
            Some(Body::Error(e)) => assert_eq!(
                e.kind,
                Some(pb::error::Kind::Unimplemented.into()),
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

        let host = StableRemoteProvider::new(make_dyn_provider(Arc::new(BlockingProbe)), "blocker")
            .expect("load");
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
                    Ok(name) => Ok(ListResponse::addr_only(hmodel::htaddr::Addr::new(
                        PkgBuf::from("p"),
                        (*name).to_string(),
                        Default::default(),
                    ))),
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
        let host = StableRemoteProvider::new(dynp, "lister").expect("load");
        let tok = StdCancellationToken::new();
        let iter = futures::executor::block_on(host.list(
            ListRequest {
                request_id: String::new(),
                package: PkgBuf::from("p"),
                states: vec![],
                executor: Arc::new(hplugin::provider::NoopExecutor),
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
        got[0].as_ref().expect("first item is ok");
        let err = got[1].as_ref().expect_err("second item is the error");
        assert!(err.to_string().contains("boom"));
    }

    // A driver error crossing the proto seam must carry its FULL anyhow cause
    // chain, not just the top context. Serializing with `e.to_string()` dropped
    // the deepest cause (e.g. a driver wrapping `compute embedcfg` over a real
    // `//go:embed matched no files` bail surfaced as a useless one-liner). The
    // message must be the `{:#}` chain so the host re-renders the root cause.
    #[test]
    fn run_error_message_carries_full_cause_chain() {
        use hcore::hasync::Cancellable;
        use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};

        struct FailDriver;
        #[async_trait::async_trait]
        impl ManagedDriver for FailDriver {
            fn config(
                &self,
                _r: hplugin::driver::ConfigRequest,
            ) -> Result<hplugin::driver::ConfigResponse> {
                Ok(hplugin::driver::ConfigResponse {
                    name: "fail".into(),
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
                Err(anyhow::anyhow!("//go:embed matched no files").context("compute embedcfg"))
            }
        }

        // A valid request so run_once gets past decoding and actually invokes
        // the driver. The output channel goes nowhere here: this test is about
        // the terminal error frame, not about live output.
        let (out_tx, _out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let frame = futures::executor::block_on(run_once_wire(
            Arc::new(FailDriver) as Arc<dyn ManagedDriver>,
            "r".into(),
            wire_run(wire_target()),
            &out_tx,
            &StdCancellationToken::new(),
        ));
        match frame.msg {
            Some(pb::run_out_frame::Msg::Error(msg)) => {
                assert!(
                    msg.contains("compute embedcfg"),
                    "top context present: {msg}"
                );
                assert!(
                    msg.contains("//go:embed matched no files"),
                    "deepest cause must survive the seam: {msg}"
                );
            }
            other => panic!("expected error frame, got {other:?}"),
        }
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
        let start = start_frame(wire_run(wire_target()));
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
        let frame = pb::RunOutFrame::decode_from_slice(&frames[0][..]).expect("decode RunOutFrame");
        // A terminal frame (Response or Error) — proves the request was consumed and
        // the run task's output crossed the response stream; the plumbing is the point.
        assert!(
            matches!(
                frame.msg,
                Some(pb::run_out_frame::Msg::Response(_)) | Some(pb::run_out_frame::Msg::Error(_))
            ),
            "expected a terminal RunOutFrame, got {:?}",
            frame.msg
        );
    }

    // Regression: a driver error's FULL anyhow cause chain must cross the seam,
    // not just the outermost context. The host reconstructs a single-message
    // error from the transmitted string, so serializing with a bare
    // `to_string()` would drop every underlying cause — e.g. surfacing
    // `compute embedcfg` with no `//go:embed pattern(s) matched no files: …` to
    // explain it. We serialize with `{:#}` so the explanation rides along.
    #[test]
    fn run_error_carries_full_cause_chain() {
        use anyhow::Context as _;
        use hcore::hasync::Cancellable;
        use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};
        use hplugin_stabby::abi::{StableItemStreamDyn, StableManagedDriverDyn};

        struct ChainErrDriver;
        #[async_trait::async_trait]
        impl ManagedDriver for ChainErrDriver {
            fn config(
                &self,
                _r: hplugin::driver::ConfigRequest,
            ) -> Result<hplugin::driver::ConfigResponse> {
                Ok(hplugin::driver::ConfigResponse {
                    name: "chainerr".into(),
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
                Err(anyhow::anyhow!(
                    "//go:embed pattern(s) matched no files: ui_dist/*"
                ))
                .context("compute embedcfg")
            }
        }

        let dynd = make_dyn_managed_driver(Arc::new(ChainErrDriver) as Arc<dyn ManagedDriver>);
        // A valid request so decoding succeeds and `driver.run` is reached.
        let start = start_frame(wire_run(wire_target()));
        let req_stream = make_item_stream(Box::new(std::iter::once(start)));

        let resp =
            futures::executor::block_on(dynd.invoke_bidi(pb::DriverMethod::Run as u32, req_stream));
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

        let frame = pb::RunOutFrame::decode_from_slice(&frames[0][..]).expect("decode RunOutFrame");
        let msg = match frame.msg {
            Some(pb::run_out_frame::Msg::Error(e)) => e,
            other => panic!("expected an Error frame, got {other:?}"),
        };
        // Both the outermost context AND the underlying cause must be present.
        assert!(
            msg.contains("compute embedcfg"),
            "outermost context missing: {msg}"
        );
        assert!(
            msg.contains("//go:embed pattern(s) matched no files: ui_dist/*"),
            "underlying cause dropped — only the top frame crossed: {msg}"
        );
    }

    // A hook receives every client-streamed event in order across the seam, then
    // `on_close` fires when the host ends the stream. Exercises the full
    // host→guest client-stream path: StableRemoteHook (host) → make_dyn_hook
    // (guest) → author Hook.
    #[test]
    fn hook_client_stream_roundtrip() {
        use hcore::events::{BuildEvent, BuildEventKind};
        use hplugin::hook::Hook;
        use hplugin_stabby::load_stable::StableRemoteHook;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Default)]
        struct Recorder {
            seen: Mutex<Vec<u64>>,
            closed: AtomicBool,
        }
        impl Hook for Recorder {
            fn name(&self) -> String {
                "rec".into()
            }
            fn on_event(&self, ev: &BuildEvent) {
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ev.at_unix_ms);
            }
            fn on_close(&self) {
                self.closed.store(true, Ordering::Release);
            }
        }

        let rec = Arc::new(Recorder::default());
        let remote = StableRemoteHook::new(make_dyn_hook(Arc::clone(&rec) as Arc<dyn Hook>), "rec");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let mk = |at| BuildEvent {
                at_unix_ms: at,
                kind: BuildEventKind::ResultStart {
                    addr: "//a:b".into(),
                },
            };
            remote.on_event(&mk(1));
            remote.on_event(&mk(2));
            remote.on_event(&mk(3));
            // Closes the stream and awaits the plugin's ack (its full drain).
            remote.drain().await;
        });

        let seen = rec.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            seen,
            vec![1, 2, 3],
            "all events arrive in order across the seam"
        );
        assert!(
            rec.closed.load(Ordering::Acquire),
            "on_close fires when the host ends the stream"
        );
    }

    // The event stream is one-shot. `StableRemoteHook` is registered on the
    // *engine* but `on_close` fires per *request*, so in a long-lived host every
    // event after the first request ends reaches a closed stream. Those events
    // must be dropped — never resurrect the stream, never reach the plugin — and
    // must not pay for a frame encode on the way out.
    #[test]
    fn hook_events_after_close_are_dropped_and_never_reopen_the_stream() {
        use hcore::events::{BuildEvent, BuildEventKind};
        use hplugin::hook::Hook;
        use hplugin_stabby::load_stable::StableRemoteHook;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct Recorder {
            seen: Mutex<Vec<u64>>,
            closes: AtomicUsize,
        }
        impl Hook for Recorder {
            fn name(&self) -> String {
                "rec".into()
            }
            fn on_event(&self, ev: &BuildEvent) {
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ev.at_unix_ms);
            }
            fn on_close(&self) {
                self.closes.fetch_add(1, Ordering::Release);
            }
        }

        let rec = Arc::new(Recorder::default());
        let remote = StableRemoteHook::new(make_dyn_hook(Arc::clone(&rec) as Arc<dyn Hook>), "rec");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let mk = |at| BuildEvent {
                at_unix_ms: at,
                kind: BuildEventKind::ResultStart {
                    addr: "//a:b".into(),
                },
            };
            remote.on_event(&mk(1));
            remote.drain().await;
            // Second request's worth of events, arriving after the close.
            remote.on_event(&mk(2));
            remote.on_event(&mk(3));
            // A second drain must be a no-op, not a second stream.
            remote.drain().await;
        });

        assert_eq!(
            rec.seen.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec![1],
            "events emitted after the stream closed must not reach the plugin"
        );
        assert_eq!(
            rec.closes.load(Ordering::Acquire),
            1,
            "the stream is one-shot: no second stream is opened, so on_close fires once"
        );
    }

    // Concurrent emitters all deliver. Guards the encode-outside-the-lock change
    // in `StableRemoteHook::on_event`: the state mutex guards only the lazily
    // opened stream handle, so exactly one stream must still be opened and every
    // frame must arrive, regardless of how many threads race the first event.
    #[test]
    fn hook_concurrent_emitters_open_one_stream_and_lose_nothing() {
        use hcore::events::{BuildEvent, BuildEventKind};
        use hplugin::hook::Hook;
        use hplugin_stabby::load_stable::StableRemoteHook;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Default)]
        struct Recorder {
            seen: Mutex<Vec<u64>>,
            closes: AtomicUsize,
        }
        impl Hook for Recorder {
            fn name(&self) -> String {
                "rec".into()
            }
            fn on_event(&self, ev: &BuildEvent) {
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ev.at_unix_ms);
            }
            fn on_close(&self) {
                self.closes.fetch_add(1, Ordering::Release);
            }
        }

        const EMITTERS: u64 = 8;
        const PER_EMITTER: u64 = 32;

        let rec = Arc::new(Recorder::default());
        let remote = Arc::new(StableRemoteHook::new(
            make_dyn_hook(Arc::clone(&rec) as Arc<dyn Hook>),
            "rec",
        ));

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            // `on_event` lazily `tokio::spawn`s the stream driver, so every
            // emitter needs runtime context — the engine always emits from one.
            let handle = tokio::runtime::Handle::current();
            std::thread::scope(|scope| {
                for t in 0..EMITTERS {
                    let remote = Arc::clone(&remote);
                    let handle = handle.clone();
                    scope.spawn(move || {
                        let _guard = handle.enter();
                        for i in 0..PER_EMITTER {
                            remote.on_event(&BuildEvent {
                                at_unix_ms: t * PER_EMITTER + i,
                                kind: BuildEventKind::ResultStart {
                                    addr: "//a:b".into(),
                                },
                            });
                        }
                    });
                }
            });
            remote.drain().await;
        });

        let mut seen = rec.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..EMITTERS * PER_EMITTER).collect::<Vec<_>>(),
            "every concurrently emitted event must cross the seam exactly once"
        );
        assert_eq!(
            rec.closes.load(Ordering::Acquire),
            1,
            "racing emitters must open exactly one stream"
        );
    }

    // Plugin functions survive the guest→host stable-ABI round trip as
    // `NamedFunction` metadata decoded once at load: the host sees the same
    // name/signature/doc, and invoking the proxied handler carries both the
    // arguments and the FnCallContext through the plugin's one handle (D4).
    #[test]
    fn plugin_functions_roundtrip() {
        use hplugin_stabby::load_stable::StableRemoteProvider;

        let dynp = make_dyn_provider(Arc::new(FnProvider) as Arc<dyn Provider>);
        let host = StableRemoteProvider::new(dynp, "mock").expect("load");

        // Metadata crosses: exactly one function, rendered as declared.
        let defs = through_the_seam("mock", vec![echo_def()]);
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
        assert_eq!(out.value(), &Value::String("mypkg:hi".into()));

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
        assert_eq!(out.value(), &Value::String("mypkg:ababab".into()));

        // A declaring plugin function: the declaration crosses the seam whole,
        // so the host can merge it into the package being evaluated.
        let out = futures::executor::block_on(def.func.call(
            &ctx,
            FnArgs {
                positional: vec![Value::String("declare".into())],
                named: Default::default(),
            },
        ))
        .expect("call declaring echo");
        assert_eq!(out.value(), &Value::String("//mypkg:t".into()));
        assert_eq!(out.targets().len(), 1);
        let t = &out.targets()[0];
        assert_eq!((t.name.as_str(), t.driver.as_str()), ("t", "exec"));
        assert_eq!(t.labels, ["gen"]);
        assert_eq!(t.config.get("run"), Some(&Value::String("gen".into())));
        assert!(t.approval.required);
        assert_eq!(t.approval.notice, ["sec"]);
        // `transitive` crosses unparsed — the host parses it as it parses the
        // `target()` argument, so a declaration can't carry a sandbox a BUILD
        // file could not.
        assert_eq!(
            t.transitive,
            Value::Map(
                [(
                    "deps".to_string(),
                    Value::Map(
                        [(
                            "g".to_string(),
                            Value::List(vec![Value::String(":sib".into())]),
                        )]
                        .into(),
                    ),
                )]
                .into(),
            )
        );
        assert_eq!(out.states().len(), 1);
        assert_eq!(out.states()[0].provider, "codegen");
        assert_eq!(
            out.states()[0].args.get("toolchain"),
            Some(&Value::String("v1".into()))
        );

        // The provider's state schema crosses too (Some, with its one field).
        let schema = host.state_schema().expect("state schema crosses as Some");
        assert_eq!(schema.fields.len(), 1);
        assert_eq!(schema.fields[0].name, "verbose");
        assert_eq!(schema.fields[0].ty, ParamType::Bool);
        assert!(schema.fields[0].doc.contains("verbose output"));
        assert!(!schema.fields[0].required);
    }

    // A caller that predates the capability (ABI < 0.15.0) sends no
    // `accepts_declarations`, which reads as false. A declaring function must
    // then fail the call: the old caller would skip the declaration fields, and
    // build a target-less package from a BUILD file that asked for one. The
    // plugin's functions handle is driven directly, since the host-side proxy
    // always advertises support.
    #[test]
    fn declarations_to_a_caller_that_cannot_carry_them_fail() {
        use hplugin_stabby::abi::StablePluginFunctionsDyn;

        let (_, handle) = make_plugin_functions("mock", vec![echo_def()]).expect("export");
        let handle = handle.expect("a plugin with functions has a handle");
        let invoke = |req: pb::CallFunctionRequest| {
            futures::executor::block_on(handle.invoke(
                pb::FunctionMethod::Call as u32,
                "echo".into(),
                SVec::from(req.encode_to_vec().as_slice()),
            ))
        };
        // An older caller: no `accepts_declarations` on the wire at all.
        let call = |arg: &str| pb::CallFunctionRequest {
            name: Some("echo".into()),
            pkg: Some("mypkg".into()),
            root: Some("/ws".into()),
            positional: vec![convert::value_to_pb(&Value::String(arg.into()))],
            ..Default::default()
        };
        let bytes = invoke(call("declare"));
        match pb::Frame::decode_from_slice(&bytes[..])
            .expect("decode frame")
            .body
        {
            Some(Body::Error(e)) => {
                let msg = e.message.unwrap_or_default();
                // Names what was about to be lost, which side is old, and the
                // remedy — the plugin author cannot fix the host's version.
                assert!(msg.contains("declared 1 target"), "{msg}");
                assert!(
                    msg.contains("the calling host does not carry declarations"),
                    "{msg}"
                );
                assert!(msg.contains("upgrade heph"), "{msg}");
            }
            other => panic!("expected an error, got {other:?}"),
        }

        // The same function, same caller, not declaring: unaffected.
        let bytes = invoke(call("hi"));
        match pb::Frame::decode_from_slice(&bytes[..])
            .expect("decode frame")
            .body
        {
            Some(Body::CallFunctionResp(r)) => assert_eq!(
                convert::value_from_pb(r.value.into_option().expect("a value")).expect("decode"),
                Value::String("mypkg:hi".into())
            ),
            other => panic!("expected a value, got {other:?}"),
        }

        // A call that does not say its root is refused, naming the field.
        let mut no_root = call("hi");
        no_root.root = None;
        match pb::Frame::decode_from_slice(&invoke(no_root)[..])
            .expect("decode frame")
            .body
        {
            Some(Body::Error(e)) => {
                let msg = e.message.unwrap_or_default();
                assert!(
                    msg.contains("missing field CallFunctionRequest.root"),
                    "{msg}"
                );
            }
            other => panic!("expected an error, got {other:?}"),
        }

        // An unknown method id on the handle is Unimplemented, not a crash —
        // the same additive contract as the provider's dispatch.
        let bytes = futures::executor::block_on(handle.invoke(9999, "echo".into(), SVec::new()));
        match pb::Frame::decode_from_slice(&bytes[..])
            .expect("frame")
            .body
        {
            Some(Body::Error(e)) => {
                assert_eq!(e.kind, Some(pb::error::Kind::Unimplemented.into()));
            }
            other => panic!("expected Unimplemented, got {other:?}"),
        }
    }

    // D4 / D13: a guest function that touches tokio (here a timer) works when
    // the host calls it from a plain `block_on` — exactly how Starlark
    // evaluation calls `heph.<plugin>.<fn>`, with no plugin reactor current on
    // the calling thread. The handle runs each call through `spawn_seam`, on
    // the plugin's own runtime; without that, the sleep panics ("there is no
    // reactor running") inside the extern shim and the process aborts.
    #[test]
    fn a_plugin_fn_touching_tokio_runs_on_the_seam() {
        struct SleepFn;
        #[async_trait::async_trait]
        impl PluginFn for SleepFn {
            async fn call(&self, _: &FnCallContext<'_>, _: FnArgs) -> Result<FnOutcome> {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                Ok(Value::String("slept".into()).into())
            }
        }
        let defs = through_the_seam(
            "sleepy",
            vec![PluginFnDef {
                name: "nap".into(),
                signature: FnSignature {
                    positional: vec![],
                    named: vec![],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: String::new(),
                func: Arc::new(SleepFn),
            }],
        );
        let out = call(&defs[0], "p", vec![]).expect("a tokio-touching function answers");
        assert_eq!(out.value(), &Value::String("slept".into()));
    }

    // A plugin with functions and no provider at all: its function still
    // round-trips across the seam (the codegen-wrapper-around-`exec` shape).
    #[test]
    fn function_only_plugin_is_callable_from_build() {
        let (named, handle) = make_plugin_functions("gen", vec![echo_def()]).expect("export");
        // What a function-only cdylib's create entry returns: no provider.
        let comps = hplugin_stabby::abi::PluginComponents {
            provider: stabby::option::Option::None(),
            drivers: SVec::new(),
            functions: named,
            function_handle: handle.into(),
            hooks: SVec::new(),
            runners: SVec::new(),
            meta: SVec::new(),
        };
        let provider: Option<hplugin_stabby::abi::DynProvider> = comps.provider.into();
        assert!(provider.is_none());
        let defs = decode_functions(
            "gen",
            comps.functions.into_iter().collect(),
            comps.function_handle.into(),
        )
        .expect("load");
        assert_eq!(defs.len(), 1);
        let out = call(&defs[0], "pkg", vec![Value::String("x".into())]).expect("call");
        assert_eq!(out.value(), &Value::String("pkg:x".into()));
    }

    // I5: two plugin instances created in one process, each handed its own
    // engine's registry, resolve their own `fs` — never the other engine's.
    // A process-global registry export would make the second instance see the
    // first engine's functions.
    #[test]
    fn two_engines_never_share_a_registry() {
        struct ConstFn(&'static str);
        #[async_trait::async_trait]
        impl PluginFn for ConstFn {
            async fn call(&self, _: &FnCallContext<'_>, _: FnArgs) -> Result<FnOutcome> {
                Ok(Value::String(self.0.into()).into())
            }
        }
        let engine = |answer: &'static str| {
            let mut reg = FunctionRegistry::default();
            reg.insert(
                "fs",
                vec![PluginFnDef {
                    name: "glob".into(),
                    signature: FnSignature {
                        positional: vec![],
                        named: vec![],
                        variadic: None,
                        returns: ParamType::String,
                    },
                    doc: String::new(),
                    func: Arc::new(ConstFn(answer)),
                }],
            )
            .expect("insert");
            Arc::new(reg)
        };
        let (reg_a, reg_b) = (engine("a"), engine("b"));
        let plugin_in = |reg: &Arc<FunctionRegistry>| {
            guest_functions(hplugin_stabby::host::HostFunctionRegistry::wrap_inline(
                FunctionSlot::sealed(reg),
            ))
        };
        let (a, b) = (plugin_in(&reg_a), plugin_in(&reg_b));
        let root = std::path::PathBuf::from("/ws");
        let ctx = FnCallContext {
            pkg: "p",
            root: Path::new(&root),
        };
        let glob = |f: &Arc<dyn FunctionCaller>| {
            futures::executor::block_on(f.call("fs", "glob", &ctx, FnArgs::default()))
                .expect("call fs.glob")
                .into_value_only()
                .expect("value")
        };
        assert_eq!(glob(&a), Value::String("a".into()));
        assert_eq!(glob(&b), Value::String("b".into()));

        // The handle holds its engine's registry weakly: once that engine is
        // gone, the call says so instead of reaching anything else.
        drop(reg_a);
        let err = futures::executor::block_on(a.call("fs", "glob", &ctx, FnArgs::default()))
            .expect_err("engine gone");
        assert!(format!("{err:#}").contains("is gone"), "{err:#}");
    }

    /// Plugin A's `relay` function: calls `greeter.echo("declare")` through
    /// the registry handle its create entry received, and passes the
    /// declarations on with `absorb`.
    struct RelayFn {
        functions: Arc<dyn FunctionCaller>,
    }

    #[async_trait::async_trait]
    impl PluginFn for RelayFn {
        async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> Result<FnOutcome> {
            let mut out = FnOutcome::from(Value::Null());
            let value = out.absorb(self.functions.call("greeter", "echo", ctx, args).await?);
            out.set_value(value);
            Ok(out)
        }
    }

    /// What a function-only cdylib's create entry does with the registry it is
    /// passed: hand it to its own functions, then export them.
    fn create_relay_plugin(
        reg: DynFunctionRegistry,
    ) -> (SVec<NamedFunction>, Option<DynPluginFunctions>) {
        make_plugin_functions(
            "a",
            vec![PluginFnDef {
                name: "relay".into(),
                signature: FnSignature {
                    positional: vec![Param::required("msg", ParamType::String)],
                    named: vec![],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: "Relay to greeter.echo.".into(),
                func: Arc::new(RelayFn {
                    functions: guest_functions(reg),
                }),
            }],
        )
        .expect("export")
    }

    /// The host side of the hop: an engine registry with `greeter.echo`.
    fn greeter_registry() -> Arc<FunctionRegistry> {
        let mut reg = FunctionRegistry::default();
        reg.insert("greeter", vec![echo_def()]).expect("insert");
        Arc::new(reg)
    }

    // The registry passed to `create` reaches a function-only plugin, and its
    // function calls another plugin's function by `(plugin, fn)` back over the
    // seam — args, FnCallContext and declarations included.
    #[test]
    fn function_registry_injection_roundtrip() {
        let reg = greeter_registry();
        let slot = FunctionSlot::sealed(&reg);
        let (named, handle) = create_relay_plugin(
            hplugin_stabby::host::HostFunctionRegistry::wrap_inline(slot),
        );
        let defs = decode_functions("a", named.into_iter().collect(), handle).expect("load");

        let out = call(&defs[0], "callerpkg", vec![Value::String("yo".into())])
            .expect("call relay → greeter.echo");
        assert_eq!(out.value(), &Value::String("callerpkg:yo".into()));
    }

    /// The chain the capability exists for: the host calls plugin A's function,
    /// which calls plugin B's function (through the registry handle, i.e. back
    /// over the seam), and B's declared target reaches the host.
    ///
    /// A's obligation is the one thing no guard can enforce for it — it must
    /// `absorb` the inner outcome instead of reading `.value`, or the
    /// declarations stop at that hop. Here it absorbs, and the target arrives.
    #[test]
    fn declarations_cross_a_plugin_to_plugin_chain() {
        let reg = greeter_registry();
        let (named, handle) = create_relay_plugin(
            hplugin_stabby::host::HostFunctionRegistry::wrap_inline(FunctionSlot::sealed(&reg)),
        );
        let defs = decode_functions("a", named.into_iter().collect(), handle).expect("load");

        let out =
            call(&defs[0], "mypkg", vec![Value::String("declare".into())]).expect("call relay");

        // Two seam crossings later, B's target is here for the host to merge.
        assert_eq!(out.value(), &Value::String("//mypkg:t".into()));
        assert_eq!(out.targets().len(), 1, "B's target survived A relaying it");
        assert_eq!(out.targets()[0].name, "t");
        assert_eq!(out.targets()[0].driver, "exec");
        assert_eq!(out.states().len(), 1, "B's provider_state survived too");
    }

    // A managed driver's config schema survives the round trip (LSP kwargs).
    /// E19 through the real seam (host proxy → stable ABI → SDK guest): an
    /// entrypoint run's options reach a cdylib driver, a signal the host queued
    /// before the run reaches its receiver over the live request stream (R5),
    /// and the driver's structured exit status comes back beside its unchanged
    /// message. A def without an entrypoint is never sent options (N9).
    #[test]
    fn entrypoint_run_crosses_the_seam() {
        use hdriver_support::driver_managed::{ManagedRunRequest, ManagedRunResponse};
        use hplugin::driver::targetdef::{Entrypoint, Workdir};
        use hplugin::driver::{Foreground, RunOptions, RunStdio, Signal};
        use hplugin::error::{Exit, ProcessFailed, process_exit};
        use hplugin_stabby::load_stable::StableRemoteManagedDriver;

        struct SignalDriver;
        #[async_trait::async_trait]
        impl ManagedDriver for SignalDriver {
            fn config(
                &self,
                _req: hplugin::driver::ConfigRequest,
            ) -> Result<hplugin::driver::ConfigResponse> {
                Ok(hplugin::driver::ConfigResponse {
                    name: "sigdrv".into(),
                })
            }
            fn schema(&self) -> hplugin::driver::DriverSchema {
                hplugin::driver::DriverSchema::default()
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
                req: ManagedRunRequest<'a, 'io>,
                _ct: &(dyn Cancellable + Send + Sync),
            ) -> Result<ManagedRunResponse> {
                let r = req.request;
                anyhow::ensure!(
                    r.stdout.is_none() && r.stderr.is_none(),
                    "inherit has no sinks"
                );
                anyhow::ensure!(
                    r.options.cwd.as_deref() == Some(std::path::Path::new("/caller")),
                    "cwd: {:?}",
                    r.options.cwd
                );
                anyhow::ensure!(
                    r.options.args == Some(vec!["a b".into()]),
                    "args: {:?}",
                    r.options.args
                );
                let RunStdio::Inherit(mut fg) = r.options.stdio else {
                    anyhow::bail!("stdio: {:?}", r.options.stdio);
                };
                let sig = fg.signals.recv().await.context("the relay closed empty")?;
                Err(ProcessFailed {
                    status: format!("signal: {}", sig.as_raw()),
                    log: Arc::new(hcore::hartifactcontent::FileContent::new("/dev/null")),
                    exit: Some(Exit::Signal(sig.as_raw())),
                }
                .into())
            }
        }

        let dynd = make_dyn_managed_driver(Arc::new(SignalDriver) as Arc<dyn ManagedDriver>);
        let host = StableRemoteManagedDriver::new(dynd, "sigdrv").expect("load");
        let ct = StdCancellationToken::new();
        let id = "r".to_string();
        let mreq = |request| ManagedRunRequest {
            request,
            sandbox_dir: "/sb".into(),
            sandbox_ws_dir: "/sb/ws".into(),
            sandbox_pkg_dir: "/sb/ws/p".into(),
            inputs: vec![],
        };

        let mut target = convert::target_def_from_pb(wire_target()).expect("def");
        target.entrypoint = Some(Entrypoint::new(true, Workdir::Caller));
        let (tx, rx) = hplugin::driver::signal_channel();
        // Queued before the run exists: it waits in the stream for the driver.
        tx.send(Signal::from_raw(15)).expect("queue");
        let mut rr = RunRequest::new(&id, &target, "/sb");
        rr.hashin = "h";
        rr.options = RunOptions::default()
            .with_cwd("/caller")
            .with_args(vec!["a b".into()])
            .with_stdio(RunStdio::Inherit(Foreground::new(rx, vec![])));
        let err = match futures::executor::block_on(host.run(mreq(rr), &ct)) {
            Ok(_) => panic!("the driver fails with the signal it received"),
            Err(e) => e,
        };
        assert_eq!(process_exit(&err), Some(Exit::Signal(15)), "{err:#}");
        assert!(
            format!("{err:#}").contains("process exited with status: signal: 15"),
            "{err:#}"
        );
        drop(tx);

        // N9: the same options for a def that declared no entrypoint are
        // refused at the seam, before the plugin sees anything.
        let plain = convert::target_def_from_pb(wire_target()).expect("def");
        let mut rr = RunRequest::new(&id, &plain, "/sb");
        rr.hashin = "h";
        rr.options = RunOptions::default().with_cwd("/caller");
        let err = match futures::executor::block_on(host.run(mreq(rr), &ct)) {
            Ok(_) => panic!("options for a plain def are refused"),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("declared no entrypoint"),
            "{err:#}"
        );
    }

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
                    accepts_deferred: false,
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
        let host = StableRemoteManagedDriver::new(dynd, "mockdrv").expect("load");
        let schema = host.schema();
        assert_eq!(schema.fields.len(), 1);
        assert_eq!(schema.fields[0].name, "args");
        assert_eq!(schema.fields[0].ty, ParamType::list(ParamType::String));
        assert!(schema.fields[0].required);
        assert!(schema.fields[0].doc.contains("Command arguments"));
    }

    // The remote managed-driver proxy must report no native shell, so the host's
    // ManagedDriverBridge dispatches `--shell` to its pluginexec fallback rather
    // than forwarding run_shell across the ABI (which would hit the driver's
    // default run_shell and bail). Regression: this used to hardcode `true`, so
    // `--shell` on any external managed driver (e.g. go_compile) failed.
    #[test]
    fn remote_managed_driver_reports_no_native_shell() {
        use hplugin_stabby::load_stable::StableRemoteManagedDriver;

        let dynd =
            make_dyn_managed_driver(Arc::new(BareDriver(Some("bare"))) as Arc<dyn ManagedDriver>);
        let host = StableRemoteManagedDriver::new(dynd, "bare").expect("load");
        assert!(
            !host.supports_shell(),
            "remote proxy must defer --shell to the host pluginexec fallback"
        );
    }

    /// A managed driver whose `config()` reports `.0` as its name, or fails
    /// when it is `None`. Every other method is unused.
    struct BareDriver(Option<&'static str>);

    #[async_trait::async_trait]
    impl ManagedDriver for BareDriver {
        fn config(
            &self,
            _req: hplugin::driver::ConfigRequest,
        ) -> Result<hplugin::driver::ConfigResponse> {
            let name = self.0.context("config deliberately fails")?;
            Ok(hplugin::driver::ConfigResponse { name: name.into() })
        }
        fn schema(&self) -> hplugin::driver::DriverSchema {
            hplugin::driver::DriverSchema::default()
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
            _req: hdriver_support::driver_managed::ManagedRunRequest<'a, 'io>,
            _ct: &(dyn Cancellable + Send + Sync),
        ) -> Result<hdriver_support::driver_managed::ManagedRunResponse> {
            anyhow::bail!("unused")
        }
    }

    /// A cdylib states a driver's local name once, in its `config()`: what the
    /// host registers is that name, carried by the exported `NamedDriver`.
    #[test]
    fn an_exported_driver_is_registered_under_its_config_name() {
        let nd = named_managed_driver(BareDriver(Some("layer"))).expect("export");
        let mut host = hplugin_stabby::load_stable::decode_drivers([nd]).expect("load");
        assert_eq!(host.len(), 1);
        let (local, drv) = host.remove(0);
        assert_eq!(local, "layer");
        let name = drv
            .config(hplugin::driver::ConfigRequest {})
            .expect("host config")
            .name;
        assert_eq!(
            name, "layer",
            "the host-side driver reports the exported name"
        );
    }

    /// A driver whose `config()` fails is not exported under some placeholder:
    /// the export fails, naming the driver's type so the cdylib's log says which.
    #[test]
    fn exporting_a_driver_whose_config_fails_names_the_driver() {
        let Err(err) = named_managed_driver(BareDriver(None)) else {
            panic!("a failing config must fail the export");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("BareDriver"), "{msg}");
        assert!(msg.contains("config deliberately fails"), "{msg}");
    }

    // ---- CancelRegistry: deterministic coverage of the enter/cancel/drop
    // contract. The race-loop test below exercises the concurrent interleaving;
    // these pin each ordering on its own. ----

    // (a) A cancel that arrives before its call registers is parked and applied
    // at enter — the token starts cancelled.
    #[test]
    fn registry_cancel_before_enter_starts_cancelled() {
        let reg = Arc::new(CancelRegistry::default());
        reg.cancel("x");
        let g = reg.enter("x");
        assert!(
            g.token().is_cancelled(),
            "parked pre-cancel must apply at enter"
        );
    }

    // (b) The plain order: enter, then cancel trips the registered token.
    #[test]
    fn registry_cancel_after_enter_trips_the_token() {
        let reg = Arc::new(CancelRegistry::default());
        let g = reg.enter("x");
        reg.cancel("x");
        assert!(g.token().is_cancelled(), "cancel must trip the live token");
    }

    // (c) Request ids are shared by the whole request subtree, so a stale
    // guard's late drop must not deregister a successor call's token. With the
    // old unconditional `remove(&self.id)` this goes red: g1's drop removed the
    // entry g2 re-registered, and the cancel found nothing to trip.
    #[test]
    fn registry_stale_guard_drop_does_not_deregister_successor() {
        let reg = Arc::new(CancelRegistry::default());
        let g1 = reg.enter("x");
        let g2 = reg.enter("x");
        drop(g1);
        reg.cancel("x");
        assert!(
            g2.token().is_cancelled(),
            "g1's late drop must not have deregistered g2's token"
        );
    }

    // Concurrent calls under ONE request id each keep their own token, and a
    // request cancel trips them ALL. With the old single-slot map this goes
    // red: the second enter replaced the first's token, so the cancel tripped
    // only the last registrant while the first call ran to completion.
    #[test]
    fn registry_cancel_trips_every_concurrent_call_under_one_id() {
        let reg = Arc::new(CancelRegistry::default());
        let g1 = reg.enter("x");
        let g2 = reg.enter("x");
        reg.cancel("x");
        assert!(g1.token().is_cancelled(), "first call's token must trip");
        assert!(g2.token().is_cancelled(), "second call's token must trip");
    }

    // A cancelled guard's drop consumes the parked pre-cancel marker and its
    // own inflight slot, so a long-lived process (LSP/watch) doesn't accumulate
    // one parked id per cancelled request.
    #[test]
    fn registry_cancelled_guard_drop_leaves_no_state_behind() {
        let reg = Arc::new(CancelRegistry::default());
        let g = reg.enter("x");
        reg.cancel("x");
        drop(g);
        assert!(
            reg.precancelled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "the parked marker must be consumed by the cancelled guard's drop"
        );
        assert!(
            reg.inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "the guard's token (and the emptied entry) must be gone"
        );
    }

    /// Base provider for the seam tests below: every method is a benign stub;
    /// individual tests override the one they exercise.
    macro_rules! stub_provider_boilerplate {
        ($name:literal) => {
            fn config(&self, _r: ConfigRequest) -> Result<ConfigResponse> {
                Ok(ConfigResponse { name: $name.into() })
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
        };
    }

    // A panicking provider body must surface as an actionable seam error —
    // plugin name, method, key, payload — never unwind through the extern shim
    // (which would abort the process). Tokio's task harness is the catch_unwind;
    // the wrapper maps JoinError::is_panic to the error body.
    #[test]
    fn panicking_provider_body_is_contained_as_error() {
        use hplugin_stabby::load_stable::StableRemoteProvider;

        struct Boomer;
        impl Provider for Boomer {
            stub_provider_boilerplate!("boomer");
            fn get<'a>(
                &'a self,
                _r: GetRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, std::result::Result<GetResponse, GetError>>
            {
                Box::pin(async { Err(GetError::NotFound) })
            }
            fn probe<'a>(
                &'a self,
                _r: ProbeRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, Result<ProbeResponse>> {
                Box::pin(async { panic!("probe exploded") })
            }
        }

        let host =
            StableRemoteProvider::new(make_dyn_provider(Arc::new(Boomer)), "boomer").expect("load");
        let ct = StdCancellationToken::new();
        let msg = match futures::executor::block_on(host.probe(
            ProbeRequest {
                request_id: String::new(),
                package: PkgBuf::from("p"),
            },
            &ct,
        )) {
            Ok(_) => panic!("panic must surface as an error, not success"),
            Err(e) => format!("{e:#}"),
        };
        assert!(msg.contains("plugin boomer"), "names the plugin: {msg}");
        assert!(msg.contains("probe(p)"), "names method + key: {msg}");
        assert!(msg.contains("panicked"), "states it panicked: {msg}");
        assert!(msg.contains("probe exploded"), "carries the payload: {msg}");
    }

    // Abort-on-drop: dropping the seam wrapper without a cooperative cancel is
    // the only stop signal for entry points with no CancelRegistry wiring — the
    // spawned guest body must provably stop, not leak on the plugin runtime.
    #[test]
    fn dropped_seam_future_aborts_the_spawned_body() {
        use hplugin_stabby::abi::StableProviderDyn;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        struct SetOnDrop(Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        struct Hanger {
            started: Arc<AtomicBool>,
            stopped: Arc<AtomicBool>,
        }
        impl Provider for Hanger {
            stub_provider_boilerplate!("hanger");
            fn get<'a>(
                &'a self,
                _r: GetRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, std::result::Result<GetResponse, GetError>>
            {
                Box::pin(async { Err(GetError::NotFound) })
            }
            fn probe<'a>(
                &'a self,
                _r: ProbeRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, Result<ProbeResponse>> {
                let started = Arc::clone(&self.started);
                let stopped = Arc::clone(&self.stopped);
                Box::pin(async move {
                    // Dropped only when this future is dropped — i.e. when the
                    // spawned task is aborted (it never completes on its own).
                    let _guard = SetOnDrop(stopped);
                    started.store(true, Ordering::SeqCst);
                    futures::future::pending::<()>().await;
                    Ok(ProbeResponse { states: vec![] })
                })
            }
        }

        let started = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let dynp = make_dyn_provider(Arc::new(Hanger {
            started: Arc::clone(&started),
            stopped: Arc::clone(&stopped),
        }) as Arc<dyn Provider>);

        let req = pb::ProbeRequest {
            request_id: Some(String::new()),
            package: Some("p".into()),
            ..Default::default()
        }
        .encode_to_vec();
        let fut = dynp.invoke(pb::ProviderMethod::Probe as u32, SVec::from(req.as_slice()));

        // Eager start: the body begins without the wrapper ever being polled.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !started.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "spawned body never started");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Host abandons the call: drop without polling to completion.
        drop(fut);

        while !stopped.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "spawned body still running after the wrapper was dropped"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // Pre-cancel race loop: a cancel issued through the extern cancel path may
    // arrive before the (eagerly spawned) body reaches CancelRegistry::enter.
    // The registry parks it in `precancelled` and applies it at enter — the
    // cancel must never be lost, whichever side wins the race.
    #[test]
    fn precancel_racing_the_spawn_is_never_lost() {
        use hplugin_stabby::abi::{StableCancelDyn, StableProviderDyn};
        use std::time::Duration;

        struct CancelWait;
        impl Provider for CancelWait {
            stub_provider_boilerplate!("cancelwait");
            fn get<'a>(
                &'a self,
                _r: GetRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, std::result::Result<GetResponse, GetError>>
            {
                Box::pin(async { Err(GetError::NotFound) })
            }
            // Returns only once the token this call was handed trips.
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

        let dynp = make_dyn_provider(Arc::new(CancelWait) as Arc<dyn Provider>);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");

        for i in 0..100 {
            let id = format!("rq-{i}");
            let req = pb::ProbeRequest {
                request_id: Some(id.clone()),
                package: Some("p".into()),
                ..Default::default()
            }
            .encode_to_vec();
            // Spawn (eager) ...
            let fut = dynp.invoke(pb::ProviderMethod::Probe as u32, SVec::from(req.as_slice()));
            // ... and race the cancel against the body reaching `enter`.
            dynp.cancel(id.into());
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), fut)
                    .await
                    .unwrap_or_else(|_| panic!("cancel lost on iteration {i}: probe never woke"));
            });
        }
    }

    // Reentrancy through the seam: a provider `get` whose body calls the host
    // executor's `result`, whose resolution issues a SECOND provider `get` —
    // a second spawn into the same plugin runtime while the first body is
    // parked on the callback. Must complete, not deadlock. (Also exercises the
    // host-side mirror: the executor body is spawned onto the host runtime
    // captured at wrap() time, not run inline on a guest worker.)
    #[test]
    fn nested_seam_get_completes_without_deadlock() {
        use hmodel::htaddr::Addr;
        use hplugin::provider::{NoopExecutor, ProviderExecutor};
        use hplugin_stabby::load_stable::StableRemoteProvider;
        use std::sync::OnceLock;
        use std::time::Duration;

        struct Nester;
        impl Provider for Nester {
            stub_provider_boilerplate!("nester");
            fn probe<'a>(
                &'a self,
                _r: ProbeRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, Result<ProbeResponse>> {
                Box::pin(async { Ok(ProbeResponse { states: vec![] }) })
            }
            fn get<'a>(
                &'a self,
                req: GetRequest,
                _c: &'a (dyn Cancellable + Send + Sync),
            ) -> futures::future::BoxFuture<'a, std::result::Result<GetResponse, GetError>>
            {
                Box::pin(async move {
                    if req.addr.name != "top" {
                        return Err(GetError::NotFound);
                    }
                    let dep = Addr::new(PkgBuf::from("p"), "dep".into(), Default::default());
                    match req.executor.result(&dep).await {
                        // The nested chain completed; surface a recognizable
                        // marker instead of fabricating a TargetSpec.
                        Ok(_) => Err(GetError::Other(anyhow::anyhow!("nested-complete"))),
                        Err(e) => Err(GetError::Other(e.context("nested result failed"))),
                    }
                })
            }
        }

        /// Host-side executor whose `result` resolves by issuing a second `get`
        /// through the real seam.
        struct NestedExec {
            host: OnceLock<StableRemoteProvider>,
        }
        impl ProviderExecutor for NestedExec {
            fn result<'a>(
                &'a self,
                addr: &'a Addr,
            ) -> futures::future::BoxFuture<'a, Result<Arc<hplugin::eresult::EResult>>>
            {
                Box::pin(async move {
                    let host = self.host.get().expect("host handle wired");
                    let ct = StdCancellationToken::new();
                    let req = GetRequest {
                        request_id: String::new(),
                        addr: addr.clone(),
                        states: vec![],
                        executor: Arc::new(NoopExecutor),
                    };
                    match host.get(req, &ct).await {
                        Err(GetError::NotFound) => Ok(Arc::new(hplugin::eresult::EResult {
                            artifacts: vec![],
                            support_artifacts: vec![],
                            artifacts_meta: vec![],
                        })),
                        Ok(_) => anyhow::bail!("dep get unexpectedly found a spec"),
                        Err(GetError::Other(e)) => Err(e.context("nested dep get")),
                    }
                })
            }
            fn query<'a>(
                &'a self,
                _m: &'a hmodel::htmatcher::Matcher,
                _s: &'a [String],
            ) -> futures::future::BoxFuture<'a, Result<Vec<Addr>>> {
                Box::pin(async { anyhow::bail!("unused") })
            }
        }

        let host = StableRemoteProvider::new(
            make_dyn_provider(Arc::new(Nester) as Arc<dyn Provider>),
            "nester",
        )
        .expect("load");
        let exec = Arc::new(NestedExec {
            host: OnceLock::new(),
        });
        exec.host
            .set(host.clone())
            .unwrap_or_else(|_| panic!("set host"));

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        let out = rt
            .block_on(async {
                let ct = StdCancellationToken::new();
                tokio::time::timeout(
                    Duration::from_secs(30),
                    host.get(
                        GetRequest {
                            request_id: "rq-top".into(),
                            addr: Addr::new(PkgBuf::from("p"), "top".into(), Default::default()),
                            states: vec![],
                            executor: exec.clone() as Arc<dyn ProviderExecutor>,
                        },
                        &ct,
                    ),
                )
                .await
            })
            .expect("deadlock: nested seam get did not complete");
        match out {
            Err(GetError::Other(e)) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("nested-complete"),
                    "nested chain must have completed: {msg}"
                );
            }
            Err(GetError::NotFound) => panic!("top get must run the nested body"),
            Ok(_) => panic!("top get returns the marker error"),
        }
    }
}
