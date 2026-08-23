//! Host side of the native (mux-free) stable transport: wrap a loaded plugin's
//! [`DynProvider`] / [`DynManagedDriver`] as `hplugin::Provider` /
//! `hdriver_support::ManagedDriver`, so the engine drives them through its normal
//! traits. Each cold method is a direct stabby call; `get` passes the engine's
//! executor across natively (`HostExecutor`) so callbacks are direct.

use crate::abi::{
    CREATE_SYMBOL, CreateFn, DynExecRunner, DynExecSession, DynExecutor, DynHook, DynItemStream,
    DynManagedDriver, DynProvider, SET_LOG_SINK_SYMBOL, SET_SUPERVISOR_SYMBOL, SetLogSinkFn,
    SetSupervisorFn, StableCancelDyn, StableExecRunnerDyn, StableExecSessionDyn, StableHookDyn,
    StableItemStream, StableItemStreamDyn, StableManagedDriverDyn, StableMetaDyn,
    StableProviderDyn,
};
use crate::host::HostExecutor;
use crate::vtable::dynify;
use async_trait::async_trait;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunInput, ManagedRunRequest, ManagedRunResponse,
};
use hmodel::htpkg::PkgBuf;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest as DriverConfigRequest,
    ConfigResponse as DriverConfigResponse, DriverSchema, ParseRequest, ParseResponse,
    inputartifact,
};
use hplugin::hook::Hook;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, FnArgs, FnCallContext, GetError, GetRequest, GetResponse,
    ListPackageResponse, ListPackagesRequest, ListRequest, ListResponse, ProbeRequest,
    ProbeResponse, Provider, ProviderFn, ProviderFunctionDef, ProviderFunctionRegistry,
    StateSchema,
};
use plugin_abi::pb::frame::Body;
use plugin_abi::{convert, pb};
use prost::Message;
use stabby::vec::Vec as SVec;
use std::sync::Arc;

fn sv(bytes: &[u8]) -> SVec<u8> {
    SVec::from(bytes)
}

/// Wrap the engine executor for the seam, choosing the spawn mode explicitly.
///
/// Production always reaches this on the engine runtime (these methods are
/// engine-driven futures), so callback bodies spawn there. The `Err` arm is
/// the in-process test harnesses (`futures::executor`-driven, no tokio
/// runtime): there the *caller* is the driver of every future involved, so
/// inline execution on the polling thread is exactly right — and there is no
/// host reactor a callback body could touch. The fork is explicit here, at
/// the one layer that serves both, rather than a silent probe inside `wrap`.
fn wrap_executor(exec: &Arc<dyn hplugin::provider::ProviderExecutor>) -> DynExecutor {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => HostExecutor::wrap(Arc::clone(exec), handle),
        Err(_) => HostExecutor::wrap_inline(Arc::clone(exec)),
    }
}

/// A loaded plugin's host-side handles: an optional provider + named drivers +
/// named hooks.
pub type LoadedComponents = (
    Option<StableRemoteProvider>,
    Vec<(String, StableRemoteManagedDriver)>,
    Vec<(String, StableRemoteHook)>,
    Vec<(String, StableRemoteExecRunner)>,
);

/// Load a plugin cdylib and construct the host-side handles. The library's ABI is
/// verified against ours via stabby's type reports (`get_stabbied`); a mismatch
/// (different stabby version, or drifted boundary types) is a hard error. The
/// `Library` is intentionally leaked: the returned trait objects' vtables live in
/// the dylib's code, which must stay mapped for the process lifetime.
pub fn load(
    path: &std::path::Path,
    root: &str,
    home: &str,
    options: std::collections::HashMap<String, pb::Value>,
) -> anyhow::Result<LoadedComponents> {
    use crate::abi::PluginComponents;
    use anyhow::Context;
    use stabby::libloading::StabbyLibrary;

    // The create config crosses as prost bytes so its fields are additive; build it
    // once here from the structured options (no nested encode).
    let cfg = pb::CreateConfig {
        root: root.to_string(),
        home: home.to_string(),
        options,
    }
    .encode_to_vec();

    // SAFETY: loading a plugin dylib runs its initializers; the path is operator-
    // controlled config. The ABI of what we call is checked below via get_stabbied.
    let lib = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("dlopen plugin {}", path.display()))?;

    // Scope the symbol borrow so the library can be leaked after the call.
    let comps: PluginComponents = {
        // Install the host log sink and supervisor *before* `create` runs: `create`
        // is exactly where plugin construction can fail, and a `tracing::error!`
        // logged during that failure needs a subscriber already installed or it is
        // silently dropped — right before the ABI seam turns any panic into a
        // non-unwinding abort with no diagnostic at all. Optional: a plugin built
        // against an older SDK simply won't export these symbols; that is not an
        // error.
        // SAFETY: get_stabbied checks the symbol's stabby type report against
        // `SetLogSinkFn` before returning it.
        if let Ok(set_sink) = unsafe { lib.get_stabbied::<SetLogSinkFn>(SET_LOG_SINK_SYMBOL) } {
            set_sink(crate::host::HostLogSink::wrap());
        }
        // Hand the plugin the host's supervisor client. The plugin's own copy of
        // the `proc` crate has an uninitialised tracker (statics are not shared
        // across the dylib boundary), so without this every child it spawns goes
        // unregistered — no reaping on a hard kill of the host, and a warning per
        // spawn. Same older-SDK tolerance as the log sink.
        // SAFETY: get_stabbied checks the symbol's stabby type report against
        // `SetSupervisorFn` before returning it.
        let set_supervisor = unsafe { lib.get_stabbied::<SetSupervisorFn>(SET_SUPERVISOR_SYMBOL) };
        if let Ok(set_supervisor) = set_supervisor {
            set_supervisor(crate::host::HostSupervisor::wrap());
        }
        // SAFETY: get_stabbied verifies the symbol's stabby type report matches
        // `CreateFn` before returning it; calling it is then ABI-sound.
        let create = unsafe { lib.get_stabbied::<CreateFn>(CREATE_SYMBOL) }
            .map_err(|e| anyhow::anyhow!("stabby ABI check failed for {}: {e}", path.display()))?;
        create(sv(&cfg))
    };
    // Keep the dylib mapped for the process lifetime (the returned trait objects'
    // vtables point into its code); leaking the handle is intentional.
    let _: &'static mut libloading::Library = Box::leak(Box::new(lib));

    let PluginComponents {
        runners,
        provider_name,
        provider,
        drivers,
        hooks,
        // Reserved return-side metadata; nothing consumes it yet.
        meta: _,
    } = comps;
    // `provider` is optional: hook-only / driver-only plugins export `None`.
    let provider: std::option::Option<DynProvider> = provider.into();
    let host_provider = provider.map(|p| StableRemoteProvider::new(p, provider_name.to_string()));

    let mut host_drivers = Vec::new();
    for nd in drivers {
        let name = nd.name.to_string();
        host_drivers.push((
            name.clone(),
            StableRemoteManagedDriver::new(nd.driver, name),
        ));
    }

    let mut host_hooks = Vec::new();
    for nh in hooks {
        let name = nh.name.to_string();
        host_hooks.push((name.clone(), StableRemoteHook::new(nh.hook, name)));
    }

    let mut host_runners = Vec::new();
    for nr in runners {
        let name = nr.name.to_string();
        host_runners.push((name.clone(), StableRemoteExecRunner::new(nr.runner, name)));
    }
    Ok((host_provider, host_drivers, host_hooks, host_runners))
}

/// Host handle to a loaded plugin's exec runner. `Clone` shares the loaded
/// component, like a driver's.
#[derive(Clone)]
pub struct StableRemoteExecRunner {
    inner: Arc<DynExecRunner>,
    name: String,
}

impl StableRemoteExecRunner {
    pub fn new(inner: DynExecRunner, name: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(inner),
            name: name.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[async_trait]
impl hexec_runner::ExecRunner for StableRemoteExecRunner {
    async fn open(
        &self,
        req: hexec_runner::OpenRequest,
        ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<Arc<dyn hexec_runner::ExecSession>> {
        let request_id = format!("open-session:{}", req.key);
        let pb_req = pb::OpenSessionRequest {
            request_id: request_id.clone(),
            key: req.key.clone(),
            runner_addr: req.runner_addr.clone(),
            artifacts: req
                .artifacts
                .iter()
                .map(|a| pb::RunnerArtifact {
                    path: a.path.clone(),
                    content: a.bytes.clone().into(),
                })
                .collect(),
        }
        .encode_to_vec();
        let fut = self.inner.open(sv(&pb_req));
        let outcome = await_with_cancel(ct, runner_cancel(&self.inner, &request_id), fut).await;
        if !outcome.ok {
            anyhow::bail!("{}", outcome.message);
        }
        let session: std::option::Option<DynExecSession> = outcome.session.into();
        let session = session.ok_or_else(|| {
            anyhow::anyhow!("open: runner reported success but returned no session")
        })?;
        let info = match decode_unary(&outcome.info)? {
            Body::OpenedSessionInfo(i) => i,
            other => anyhow::bail!("open: unexpected reply {other:?}"),
        };
        let (caps, description, base_env) =
            convert::session_info_from_pb(info, &req.runner_addr, &req.key);
        Ok(Arc::new(StableRemoteExecSession {
            inner: session,
            caps,
            description,
            base_env,
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }
}

/// A live session living inside the plugin.
///
/// Everything this struct holds beyond `inner` is what the *host* needs — the
/// session's own state (a shell, a socket, a pid, a mux over them) never
/// crosses and the host has no name for it.
struct StableRemoteExecSession {
    inner: DynExecSession,
    caps: hexec_runner::SessionCaps,
    description: hexec_runner::SessionDescription,
    base_env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
    /// So `close` cannot tell the plugin twice: the orderly path and the pool's
    /// drop path can both reach it.
    closed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl hexec_runner::ExecSession for StableRemoteExecSession {
    async fn prepare(
        &self,
        spec: hproc::proc_exec::Spec,
    ) -> Result<hproc::proc_exec::Spec, hexec_runner::SpawnError> {
        let pb_req = pb::PrepareRequest {
            request_id: String::new(),
            spec: Some(convert::exec_spec_to_pb(&spec)),
            stdin: convert::stdio_kind_to_pb(&spec.stdin),
            stdout: convert::stdio_kind_to_pb(&spec.stdout),
            stderr: convert::stdio_kind_to_pb(&spec.stderr),
        }
        .encode_to_vec();
        let bytes = self
            .inner
            .invoke(pb::ExecSessionMethod::Prepare as u32, sv(&pb_req))
            .await;
        let died = |reason: String| hexec_runner::SpawnError::SessionDied {
            key: self.description.key.clone(),
            reason,
        };
        match decode_unary(&bytes).map_err(|e| died(format!("{e:#}")))? {
            Body::PrepareResp(r) => {
                // `spec` still owns the real stdio — possibly a PTY slave
                // `OwnedFd`. Only the fields a runner may change come back
                // across; nothing from the wire replaces a descriptor.
                let mut spec = spec;
                convert::exec_spec_apply(&mut spec, r.spec.unwrap_or_default());
                Ok(spec)
            }
            other => Err(died(format!("unexpected reply {other:?}"))),
        }
    }

    fn base_env(&self) -> Option<&[(std::ffi::OsString, std::ffi::OsString)]> {
        self.base_env.as_deref()
    }

    fn caps(&self) -> &hexec_runner::SessionCaps {
        &self.caps
    }

    fn describe(&self) -> &hexec_runner::SessionDescription {
        &self.description
    }

    async fn close(&self) -> anyhow::Result<()> {
        if self.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let pb_req = pb::CloseRequest {
            request_id: String::new(),
        }
        .encode_to_vec();
        let bytes = self
            .inner
            .invoke(pb::ExecSessionMethod::Close as u32, sv(&pb_req))
            .await;
        match decode_unary(&bytes)? {
            Body::CloseResp(_) => Ok(()),
            other => anyhow::bail!("close: unexpected reply {other:?}"),
        }
    }

    fn teardown(&self) -> Option<hexec_runner::TeardownJob> {
        // Nothing synchronous to hand back: closing this session means calling
        // into the plugin, which is async. The orderly path calls `close`.
        //
        // On hard abort nothing async runs, and that is covered elsewhere: a
        // process the plugin spawned was registered with the HOST's supervisor
        // (`heph_plugin_set_supervisor`), which kills the group on exit. So the
        // shell or container does not outlive heph even when `close` never runs
        // — it is reaped rather than asked to leave.
        None
    }
}

fn decode_unary(bytes: &[u8]) -> anyhow::Result<Body> {
    pb::Frame::decode(bytes)?
        .body
        .ok_or_else(|| anyhow::anyhow!("empty stable response frame"))
}

/// Host-side lazy adapter over a plugin response stream: each `next` pulls one
/// frame across the seam (`StableItemStream::next`) and decodes it, so items flow
/// incrementally and the full set is never buffered. `Send` (the stream handle is
/// `Send + Sync`, `decode` is a fn pointer) so it satisfies the engine's `Provider`
/// iterator bound.
struct ItemStreamIter<T> {
    stream: DynItemStream,
    decode: fn(&[u8]) -> anyhow::Result<T>,
    done: bool,
}

impl<T> Iterator for ItemStreamIter<T> {
    type Item = anyhow::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let bytes = self.stream.next();
        // Empty == the stream is exhausted cleanly.
        if bytes.is_empty() {
            self.done = true;
            return None;
        }
        let frame = match pb::Frame::decode(&bytes[..]) {
            Ok(f) => f,
            Err(e) => {
                self.done = true;
                return Some(Err(anyhow::anyhow!("stream frame decode: {e}")));
            }
        };
        match frame.body {
            Some(Body::StreamItem(si)) => Some((self.decode)(&si.item)),
            Some(Body::StreamEnd(se)) => {
                self.done = true;
                se.error.map(|e| Err(anyhow::anyhow!("{}", e.message)))
            }
            Some(Body::Error(e)) => {
                self.done = true;
                Some(Err(anyhow::anyhow!("{}", e.message)))
            }
            _ => {
                self.done = true;
                None
            }
        }
    }
}

fn decode_list_item(b: &[u8]) -> anyhow::Result<ListResponse> {
    let lr = pb::ListResponse::decode(b)?;
    Ok(ListResponse {
        addr: convert::addr_from_pb(lr.addr.unwrap_or_default()),
    })
}

fn decode_list_package_item(b: &[u8]) -> anyhow::Result<ListPackageResponse> {
    let lpr = pb::ListPackageResponse::decode(b)?;
    Ok(ListPackageResponse {
        pkg: PkgBuf::from(lpr.pkg),
    })
}

/// Host-side request stream: yields a fixed sequence of pre-encoded items (today a
/// single `RunInFrame{start}`; live stdin would push more), empty == end.
struct HostItemStream {
    items: std::sync::Mutex<std::vec::IntoIter<Vec<u8>>>,
}

impl StableItemStream for HostItemStream {
    extern "C" fn next(&self) -> stabby::vec::Vec<u8> {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        match items.next() {
            Some(b) => stabby::vec::Vec::from(b.as_slice()),
            None => stabby::vec::Vec::new(),
        }
    }
}

fn host_item_stream(items: Vec<Vec<u8>>) -> DynItemStream {
    dynify(stabby::boxed::Box::new(HostItemStream {
        items: std::sync::Mutex::new(items.into_iter()),
    }))
}

/// Which of the target's two streams a forwarded chunk belongs to.
enum Chunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// Drain a bidi `run` response stream to its terminal result, forwarding the
/// driver's stdout/stderr chunks to `chunks` as they arrive. Blocking (called on
/// a dedicated thread) because the stream's `next` blocks on subprocess output.
///
/// The chunks go out on a channel rather than being written here: the caller
/// owns the target's stdio sinks, which are async and borrowed from the run
/// request. Forwarding them as they arrive rather than at the end is the whole
/// point — a `docker buildx build` or a `go build` should print while it runs,
/// not in one burst after it finishes.
/// Check a guest's runner **ack**: its echo of the environment key it was told
/// to run under.
///
/// Positive confirmation, not absence of complaint. A guest compiled before
/// `runner_env` existed cannot object to what it ignored — prost drops unknown
/// fields before guest code runs — so silence has to read as failure. Otherwise
/// the guest builds in the host environment while the target's `hashin` already
/// asserts the runner's, and that artifact goes to the shared remote cache for
/// every other machine to pick up.
fn verify_runner_ack(expected: &str, got: &str, driver: &str) -> anyhow::Result<()> {
    if expected.is_empty() || got == expected {
        return Ok(());
    }
    anyhow::bail!(
        "driver `{driver}` did not confirm it ran under the requested exec environment \
         (expected key {expected:?}, got {got:?}). The target's cache key already asserts that \
         environment, so the run is refused rather than producing an artifact whose key does not \
         describe how it was built. Rebuild the plugin against a heph that understands exec \
         runners, or set `runner = None` on this target."
    )
}

#[cfg(test)]
mod runner_ack_tests {
    use super::verify_runner_ack;

    /// No runner requested: every guest, old or new, echoes nothing and that is
    /// correct. This is the path every existing plugin takes.
    #[test]
    fn no_runner_requested_needs_no_ack() {
        verify_runner_ack("", "", "go").expect("no runner requested must pass");
    }

    #[test]
    fn matching_ack_passes() {
        verify_runner_ack("k1", "k1", "go").expect("a matching ack must pass");
    }

    /// The case this exists for: an older cdylib silently ignores the
    /// environment and returns a result as though nothing happened.
    #[test]
    fn silence_from_an_older_guest_is_a_failure_not_a_pass() {
        let err = verify_runner_ack("k1", "", "go").expect_err("silence must not pass");
        let msg = format!("{err:#}");
        assert!(msg.contains("did not confirm"), "{msg}");
        assert!(msg.contains("go"), "{msg}");
        assert!(msg.contains("runner = None"), "must name a way out: {msg}");
    }

    /// A guest that applied a *different* environment than the one the key
    /// asserts is the same class of wrong, and is caught by the same check.
    #[test]
    fn a_mismatched_ack_is_a_failure() {
        assert!(verify_runner_ack("k1", "k2", "go").is_err());
    }
}

/// Drain a run's response stream, returning the result and the guest's
/// **runner ack** — its echo of the environment key it was asked to use.
///
/// The ack rides out of here rather than on `ManagedRunResponse` so the Rust
/// type every in-tree managed driver constructs stays as it was; only the
/// seam that can actually be lied to needs to carry it.
fn drain_run(
    stream: DynItemStream,
    chunks: &tokio::sync::mpsc::UnboundedSender<Chunk>,
) -> anyhow::Result<(ManagedRunResponse, String)> {
    loop {
        let bytes = stream.next();
        if bytes.is_empty() {
            anyhow::bail!("run stream ended without a result");
        }
        match pb::RunOutFrame::decode(&bytes[..])?.msg {
            Some(pb::run_out_frame::Msg::Response(r)) => {
                let ack = r.runner_key.clone();
                return Ok((
                    ManagedRunResponse {
                        artifacts: r
                            .artifacts
                            .into_iter()
                            .map(convert::output_artifact_from_pb)
                            .collect(),
                    },
                    ack,
                ));
            }
            Some(pb::run_out_frame::Msg::Error(e)) => anyhow::bail!("{e}"),
            // A dropped receiver means the caller stopped reading; keep draining
            // so the run still reaches its result.
            Some(pb::run_out_frame::Msg::StdoutChunk(b)) => {
                drop(chunks.send(Chunk::Stdout(b.to_vec())));
            }
            Some(pb::run_out_frame::Msg::StderrChunk(b)) => {
                drop(chunks.send(Chunk::Stderr(b.to_vec())));
            }
            None => {}
        }
    }
}

/// Await `fut`, but if `ct` fires first, run `on_cancel` (signal the plugin to
/// cancel this request) and keep awaiting — the call then returns its cancelled
/// result. The plugin trips the token it gave the provider/driver, so a long `get`
/// or a running subprocess stops, exactly as for an in-process target.
async fn await_with_cancel<T>(
    ct: &(dyn Cancellable + Send + Sync),
    on_cancel: impl FnOnce() + Send,
    fut: impl std::future::Future<Output = T> + Send,
) -> T {
    use futures::future::Either;
    futures::pin_mut!(fut);
    match futures::future::select(fut, ct.cancelled()).await {
        Either::Left((out, _)) => out,
        Either::Right(((), fut)) => {
            on_cancel();
            fut.await
        }
    }
}

/// The cancel signal for a provider call: trip the plugin's in-flight `request_id`.
fn provider_cancel(inner: &Arc<DynProvider>, request_id: &str) -> impl FnOnce() + Send {
    let inner = Arc::clone(inner);
    let id = request_id.to_string();
    move || inner.cancel(id.into())
}

/// The cancel signal for a driver call.
/// A cold `open_session` is tens of seconds for a devenv evaluation, so a
/// Ctrl-C during it has to reach the plugin rather than wait it out.
fn runner_cancel(inner: &Arc<DynExecRunner>, request_id: &str) -> impl FnOnce() + Send {
    let inner = Arc::clone(inner);
    let id = request_id.to_string();
    move || inner.cancel(id.into())
}

fn driver_cancel(inner: &Arc<DynManagedDriver>, request_id: &str) -> impl FnOnce() + Send {
    let inner = Arc::clone(inner);
    let id = request_id.to_string();
    move || inner.cancel(id.into())
}

/// Host handle to a loaded plugin's provider. `Clone` (cheap — shares the loaded
/// component) so the engine's provider factory can mint handles.
#[derive(Clone)]
pub struct StableRemoteProvider {
    inner: Arc<DynProvider>,
    name: String,
}

impl StableRemoteProvider {
    pub fn new(inner: DynProvider, name: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(inner),
            name: name.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Provider for StableRemoteProvider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: self.name.clone(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        _ct: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            let pb_req = pb::ListRequest {
                request_id: req.request_id,
                package: req.package.as_str().to_string(),
                states: req.states.iter().map(convert::state_to_pb).collect(),
            }
            .encode_to_vec();
            // Server-streaming **with** the executor, so the plugin's `list` can
            // call back (e.g. the go module variant universe via `states_under`).
            // Items still stream lazily across the seam.
            let exec: DynExecutor = wrap_executor(&req.executor);
            let stream = self
                .inner
                .invoke_exec_server_stream(pb::ProviderMethod::List as u32, sv(&pb_req), exec)
                .await;
            Ok(Box::new(ItemStreamIter {
                stream,
                decode: decode_list_item,
                done: false,
            })
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        _ct: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            let pb_req = pb::ListPackagesRequest {
                prefix: req.prefix.as_str().to_string(),
            }
            .encode_to_vec();
            let stream = self
                .inner
                .invoke_server_stream(pb::ProviderMethod::ListPackages as u32, sv(&pb_req))
                .await;
            Ok(Box::new(ItemStreamIter {
                stream,
                decode: decode_list_package_item,
                done: false,
            })
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        ct: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, std::result::Result<GetResponse, GetError>> {
        Box::pin(async move {
            let pb_req = pb::GetRequest {
                request_id: req.request_id.clone(),
                addr: Some(convert::addr_to_pb(&req.addr)),
                states: req.states.iter().map(convert::state_to_pb).collect(),
            }
            .encode_to_vec();
            let exec: DynExecutor = wrap_executor(&req.executor);
            let fut = self
                .inner
                .invoke_exec(pb::ProviderMethod::Get as u32, sv(&pb_req), exec);
            let bytes =
                await_with_cancel(ct, provider_cancel(&self.inner, &req.request_id), fut).await;
            let body = decode_unary(&bytes).map_err(GetError::Other)?;
            match body {
                Body::GetResp(gr) => Ok(GetResponse {
                    target_spec: convert::target_spec_from_pb(gr.target_spec.unwrap_or_default()),
                }),
                Body::GetErr(ge) => match pb::get_error::Kind::try_from(ge.kind)
                    .unwrap_or(pb::get_error::Kind::Other)
                {
                    pb::get_error::Kind::NotFound => Err(GetError::NotFound),
                    pb::get_error::Kind::Cycle => Err(GetError::Other(anyhow::Error::new(
                        hplugin::error::CycleError {
                            from: req.addr.clone(),
                            to: req.addr.clone(),
                        },
                    ))),
                    pb::get_error::Kind::Cancelled => Err(GetError::Other(anyhow::Error::new(
                        hplugin::error::CancelledError,
                    ))),
                    _ => Err(GetError::Other(anyhow::anyhow!("{}", ge.message))),
                },
                other => Err(GetError::Other(anyhow::anyhow!(
                    "unexpected get response: {other:?}"
                ))),
            }
        })
    }

    fn probe<'a>(
        &'a self,
        req: ProbeRequest,
        ct: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move {
            let request_id = req.request_id.clone();
            let pb_req = pb::ProbeRequest {
                request_id: req.request_id,
                package: req.package.as_str().to_string(),
            }
            .encode_to_vec();
            let fut = self
                .inner
                .invoke(pb::ProviderMethod::Probe as u32, sv(&pb_req));
            let bytes = await_with_cancel(ct, provider_cancel(&self.inner, &request_id), fut).await;
            match decode_unary(&bytes)? {
                Body::ProbeResp(pr) => Ok(ProbeResponse {
                    states: pr.states.into_iter().map(convert::state_from_pb).collect(),
                }),
                Body::Error(e) => anyhow::bail!("{}", e.message),
                other => anyhow::bail!("unexpected probe response: {other:?}"),
            }
        })
    }

    fn functions(&self) -> Vec<ProviderFunctionDef> {
        // Sync metadata call across the seam; decode the plugin's function defs
        // and wrap each handler in a proxy that dispatches back over the ABI.
        let bytes = self.inner.meta(pb::ProviderMethod::Functions as u32);
        let resp = match pb::FunctionsResponse::decode(&bytes[..]) {
            Ok(r) => r,
            // A decode failure here would be an ABI bug; surface no functions
            // rather than poison registry wiring (which has no error channel).
            Err(_) => return Vec::new(),
        };
        resp.functions
            .into_iter()
            .filter_map(|d| {
                Some(ProviderFunctionDef {
                    signature: convert::fn_signature_from_pb(d.signature?),
                    doc: d.doc,
                    func: Arc::new(StableRemoteFn {
                        inner: Arc::clone(&self.inner),
                        name: d.name.clone(),
                    }),
                    name: d.name,
                })
            })
            .collect()
    }

    fn state_schema(&self) -> Option<StateSchema> {
        // An empty SVec encodes `None`; any encoded `Schema` (even fields-empty)
        // encodes `Some`.
        let bytes = self.inner.meta(pb::ProviderMethod::StateSchema as u32);
        if bytes.is_empty() {
            return None;
        }
        pb::Schema::decode(&bytes[..])
            .ok()
            .map(convert::state_schema_from_pb)
    }

    fn set_function_registry(&self, reg: Arc<ProviderFunctionRegistry>) {
        // Cross the metadata once, and hand the plugin a callback to invoke any
        // function in the aggregate registry (handlers are not transmissible).
        let functions = reg
            .iter()
            .map(|(provider, name, rf)| pb::RegisteredFunction {
                provider: provider.to_string(),
                name: name.to_string(),
                signature: Some(convert::fn_signature_to_pb(&rf.signature)),
                doc: rf.doc.clone(),
            })
            .collect();
        let metadata = pb::FunctionRegistry { functions }.encode_to_vec();
        // Same explicit fork as `wrap_executor`: production wiring happens on
        // the engine runtime; only runtime-less in-process harnesses take the
        // inline arm, and they drive the callback futures themselves.
        let cb = match tokio::runtime::Handle::try_current() {
            Ok(handle) => crate::host::HostFunctionRegistry::wrap(reg, handle),
            Err(_) => crate::host::HostFunctionRegistry::wrap_inline(reg),
        };
        self.inner.invoke_registry(
            pb::ProviderMethod::SetFunctionRegistry as u32,
            sv(&metadata),
            cb,
        );
    }
}

/// Proxy handler for a dylib provider function: each call encodes its args and
/// the `FnCallContext`, dispatches `call_function` over the stable ABI, and
/// decodes the returned [`Value`].
struct StableRemoteFn {
    inner: Arc<DynProvider>,
    name: String,
}

#[async_trait]
impl ProviderFn for StableRemoteFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<Value> {
        let pb_req = pb::CallFunctionRequest {
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
            .inner
            .invoke(pb::ProviderMethod::CallFunction as u32, sv(&pb_req))
            .await;
        match decode_unary(&bytes)? {
            Body::CallFunctionResp(r) => Ok(convert::value_from_pb(r.value.unwrap_or_default())),
            Body::Error(e) => anyhow::bail!("{}", e.message),
            other => anyhow::bail!("unexpected call_function response: {other:?}"),
        }
    }
}

/// Host handle to a loaded plugin's managed driver. `Clone` (shares the loaded
/// component) for the engine's driver factory — and for the exec-runner lane,
/// where a session-serving driver is registered twice: once as the engine's
/// driver, once behind a `DriverExecRunner`. Both handles address the same
/// plugin object.
#[derive(Clone)]
pub struct StableRemoteManagedDriver {
    inner: Arc<DynManagedDriver>,
    name: String,
}

impl StableRemoteManagedDriver {
    pub fn new(inner: DynManagedDriver, name: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(inner),
            name: name.into(),
        }
    }
}

#[async_trait]
impl ManagedDriver for StableRemoteManagedDriver {
    fn config(&self, _req: DriverConfigRequest) -> anyhow::Result<DriverConfigResponse> {
        Ok(DriverConfigResponse {
            name: self.name.clone(),
        })
    }

    fn schema(&self) -> DriverSchema {
        let bytes = self.inner.meta(pb::DriverMethod::Schema as u32);
        pb::Schema::decode(&bytes[..])
            .map(convert::driver_schema_from_pb)
            .unwrap_or_default()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let request_id = req.request_id.clone();
        let pb_req = pb::ParseRequest {
            request_id: req.request_id,
            target_spec: Some(convert::target_spec_to_pb(req.target_spec.as_ref())),
            driver: self.name.clone(),
        }
        .encode_to_vec();
        let fut = self
            .inner
            .invoke(pb::DriverMethod::Parse as u32, sv(&pb_req));
        let bytes = await_with_cancel(ct, driver_cancel(&self.inner, &request_id), fut).await;
        match decode_unary(&bytes)? {
            Body::ParseResp(pr) => Ok(ParseResponse {
                target_def: convert::target_def_from_pb(pr.target_def.unwrap_or_default())?,
            }),
            Body::Error(e) => anyhow::bail!("{}", e.message),
            other => anyhow::bail!("unexpected parse response: {other:?}"),
        }
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        let request_id = req.request_id.clone();
        let pb_req = pb::ApplyTransitiveRequest {
            request_id: req.request_id,
            target_def: Some(convert::target_def_to_pb(&req.target_def)?),
            sandbox: Some(convert::sandbox_to_pb(&req.sandbox)),
            driver: self.name.clone(),
        }
        .encode_to_vec();
        let fut = self
            .inner
            .invoke(pb::DriverMethod::ApplyTransitive as u32, sv(&pb_req));
        let bytes = await_with_cancel(ct, driver_cancel(&self.inner, &request_id), fut).await;
        match decode_unary(&bytes)? {
            Body::ApplyTransitiveResp(r) => Ok(ApplyTransitiveResponse {
                target_def: convert::target_def_from_pb(r.target_def.unwrap_or_default())?,
            }),
            Body::Error(e) => anyhow::bail!("{}", e.message),
            other => anyhow::bail!("unexpected apply_transitive response: {other:?}"),
        }
    }

    fn supports_shell(&self) -> bool {
        // Report no native shell so the host's ManagedDriverBridge dispatches
        // `--shell` to its pluginexec fallback (an interactive bash in the
        // already-materialized sandbox) instead of forwarding `run_shell` across
        // the ABI. The stable ABI exposes no `supports_shell`, and no external
        // plugin implements an interactive `run_shell` — forwarding only reaches
        // the driver's default `run_shell`, which bails ("the bridge must
        // dispatch to the shell fallback"). Returning false makes `--shell` work
        // uniformly for every external managed driver (e.g. go_compile).
        false
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.dispatch_run(req, false, ct).await
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.dispatch_run(req, true, ct).await
    }
}

impl StableRemoteManagedDriver {
    async fn dispatch_run(
        &self,
        mut req: ManagedRunRequest<'_, '_>,
        shell: bool,
        ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let request_id = req.request.request_id.clone();
        let pmrr = pb::ManagedRunRequest {
            request_id: req.request.request_id.clone(),
            target: Some(convert::target_def_to_pb(req.request.target)?),
            tree_root_path: req.request.tree_root_path.to_string_lossy().into_owned(),
            hashin: req.request.hashin.to_string(),
            sandbox_dir: req.sandbox_dir.to_string_lossy().into_owned(),
            sandbox_ws_dir: req.sandbox_ws_dir.to_string_lossy().into_owned(),
            sandbox_pkg_dir: req.sandbox_pkg_dir.to_string_lossy().into_owned(),
            inputs: req.inputs.iter().map(managed_input_to_pb).collect(),
            shell,
            driver: self.name.clone(),
            // The environment the guest must create its processes in. Sent as
            // bytes because env values are bytes; see `pb::EnvVar`.
            runner_env: req
                .runner
                .base_env()
                .unwrap_or_default()
                .iter()
                .map(|(k, v)| pb::EnvVar {
                    key: k.as_encoded_bytes().to_vec().into(),
                    value: v.as_encoded_bytes().to_vec().into(),
                })
                .collect(),
            runner_key: req.runner.describe().key.clone(),
            runner_addr: req.runner.describe().runner.clone(),
        };
        // Held for the ack check below: a guest that ignored the environment
        // must not be able to pass by staying quiet.
        let expect_runner_key = req.runner.describe().key.clone();
        // `run` is bidi: the request stream carries the run request (RunInFrame),
        // the response stream carries the result (RunOutFrame). `shell` rides pmrr.
        let in_frame = pb::RunInFrame {
            msg: Some(pb::run_in_frame::Msg::Start(pmrr)),
        }
        .encode_to_vec();
        let resp_stream = self
            .inner
            .invoke_bidi(
                pb::DriverMethod::Run as u32,
                host_item_stream(vec![in_frame]),
            )
            .await;
        // The response stream's `next` blocks (on subprocess output), so drain it on
        // a dedicated thread and bridge the result back — while watching `ct` so a
        // cancel trips the plugin's run token (which stops the subprocess).
        let (tx, rx) = futures::channel::oneshot::channel();
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<Chunk>();
        std::thread::spawn(move || {
            // Receiver dropped only if the caller gave up; ignore send failure.
            drop(tx.send(drain_run(resp_stream, &chunk_tx)));
        });

        // Relay the driver's output to the target's own stdio while the run is
        // still going. The sinks belong to the request, so this has to happen
        // here rather than on the drain thread.
        let mut stdout = req.request.stdout.take();
        let mut stderr = req.request.stderr.take();
        let pump = async {
            use tokio::io::AsyncWriteExt as _;
            while let Some(chunk) = chunk_rx.recv().await {
                let (sink, bytes) = match chunk {
                    Chunk::Stdout(b) => (stdout.as_deref_mut(), b),
                    Chunk::Stderr(b) => (stderr.as_deref_mut(), b),
                };
                if let Some(w) = sink {
                    drop(w.write_all(&bytes).await);
                    drop(w.flush().await);
                }
            }
            // The drain thread dropped its sender, so no more output is coming.
            // Park rather than resolve: the run's terminal result is what ends
            // the select below. `Infallible` says that in the type, so the
            // select's other arm needs no unreachable branch.
            std::future::pending::<std::convert::Infallible>().await
        };

        let result = {
            futures::pin_mut!(pump);
            let run = await_with_cancel(ct, driver_cancel(&self.inner, &request_id), rx);
            futures::pin_mut!(run);
            // `pump` never completes; the run's terminal result ends the select.
            match futures::future::select(run, pump).await {
                futures::future::Either::Left((out, _)) => out,
                // `pump` resolves to `Infallible`, so this arm is uninhabited:
                // only the run's terminal result can win the select.
                futures::future::Either::Right((never, _)) => match never {},
            }
        };
        let (response, ack) = match result {
            Ok(result) => result?,
            Err(_canceled) => anyhow::bail!("run drain thread dropped"),
        };

        // Positive confirmation, not absence of complaint. A guest compiled
        // before `runner_env` existed cannot object to what it ignored — prost
        // drops unknown fields before guest code runs — so silence has to read
        // as failure. Otherwise the guest builds in the host environment while
        // the target's `hashin` already asserts the runner's, and that artifact
        // goes to the shared remote cache for every other machine to pick up.
        verify_runner_ack(&expect_runner_key, &ack, &self.name)?;

        Ok(response)
    }
}

/// Host-side request stream backed by a channel: each `next` blocks until the
/// host pushes the next event frame (or all senders drop == clean end). The guest
/// pulls it on a blocking thread, so the blocking `recv` is sound (mirrors how the
/// guest drains run output on a blocking task).
struct HostChannelItemStream {
    rx: std::sync::Mutex<std::sync::mpsc::Receiver<Vec<u8>>>,
}

impl StableItemStream for HostChannelItemStream {
    extern "C" fn next(&self) -> SVec<u8> {
        let rx = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        // `Err` == every sender dropped (stream closed cleanly) => empty SVec.
        match rx.recv() {
            Ok(b) => SVec::from(b.as_slice()),
            Err(_) => SVec::new(),
        }
    }
}

fn host_channel_item_stream(rx: std::sync::mpsc::Receiver<Vec<u8>>) -> DynItemStream {
    dynify(stabby::boxed::Box::new(HostChannelItemStream {
        rx: std::sync::Mutex::new(rx),
    }))
}

/// One build event, framed as the envelope `StreamItem` carrying its serde-JSON
/// bytes (the wire form a hook plugin pulls and deserializes).
fn event_frame(ev: &hcore::events::BuildEvent) -> Vec<u8> {
    let item = serde_json::to_vec(ev).unwrap_or_default();
    pb::Frame {
        id: 0,
        body: Some(Body::StreamItem(pb::StreamItem { item: item.into() })),
    }
    .encode_to_vec()
}

/// Lazily-opened state for a remote hook's event stream.
#[derive(Default)]
struct HookStreamState {
    started: bool,
    /// Pushes event frames to the in-flight client-stream; `None` once closed.
    tx: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    /// The task driving the plugin's `invoke_client_stream` to its ack.
    join: Option<tokio::task::JoinHandle<()>>,
}

/// Host handle to a loaded plugin's hook. Forwards each engine `BuildEvent` across
/// the seam by client-streaming it into the plugin (`HOOK_METHOD_ON_EVENTS`). The
/// stream opens lazily on the first event; [`on_close`](Hook::on_close) ends it;
/// [`drain`](Hook::drain) awaits the plugin's ack so its final flush completes
/// before the host exits.
pub struct StableRemoteHook {
    inner: Arc<DynHook>,
    name: String,
    state: std::sync::Mutex<HookStreamState>,
    /// Set once the stream has been ended by `on_close` / `drain`.
    ///
    /// Read before encoding so a closed hook costs an atomic load per event
    /// instead of a serialization. The hook is engine-level while `on_close`
    /// fires per *request*, so in a long-lived host (the LSP) every event after
    /// the first request ends would otherwise be encoded only to be dropped.
    closed: std::sync::atomic::AtomicBool,
}

impl StableRemoteHook {
    pub fn new(inner: DynHook, name: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(inner),
            name: name.into(),
            state: std::sync::Mutex::new(HookStreamState::default()),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Hook for StableRemoteHook {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn on_event(&self, ev: &hcore::events::BuildEvent) {
        // The stream is one-shot: once ended, nothing reopens it, so there is
        // nothing to encode for.
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        // Encode BEFORE taking the lock. This is the engine's emit chokepoint —
        // every event of every target passes through it — and `event_frame` is a
        // serde-JSON render plus a prost encode. Holding the state mutex across
        // that serializes all emitters behind the slowest one, which is exactly
        // what `hplugin::hook`'s contract says a hook must not do. The lock only
        // guards the lazily-opened stream handle; it does not order frames (the
        // channel does), so encoding outside it changes nothing observable.
        let frame = event_frame(ev);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.started {
            st.started = true;
            // Open the client-stream once: the host pushes frames into `tx`, the
            // plugin pulls them and acks when the stream closes. Spawned because
            // the call runs for the whole request, concurrent with event flow.
            let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
            let stream = host_channel_item_stream(rx);
            let inner = Arc::clone(&self.inner);
            let join = tokio::spawn(async move {
                drop(
                    inner
                        .invoke_client_stream(pb::HookMethod::OnEvents as u32, stream)
                        .await,
                );
            });
            st.tx = Some(tx);
            st.join = Some(join);
        }
        if let Some(tx) = &st.tx {
            // Plugin gone / stream closed => receiver dropped; best-effort.
            drop(tx.send(frame));
        }
    }

    fn on_close(&self) {
        // Drop the sender so the plugin's pull sees end-of-stream and flushes.
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.tx = None;
    }

    fn drain(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // Ensure the stream is closed, then await the plugin's ack so its final
            // write lands before the host exits.
            self.closed
                .store(true, std::sync::atomic::Ordering::Release);
            let join = {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.tx = None;
                st.join.take()
            };
            if let Some(j) = join {
                drop(j.await);
            }
        })
    }
}

fn managed_input_to_pb(mi: &ManagedRunInput) -> pb::ManagedRunInput {
    let ty = match mi.input.artifact.r#type {
        inputartifact::Type::Dep => pb::InputArtifactType::Dep,
        inputartifact::Type::Support => pb::InputArtifactType::Support,
    };
    pb::ManagedRunInput {
        r#type: ty as i32,
        origin_id: mi.input.origin_id.clone(),
        source_addr: Some(convert::addr_to_pb(&mi.input.source_addr)),
        filters: mi.input.filters.clone(),
        annotations: mi
            .input
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        unpack_root: mi.unpack_root.to_string_lossy().into_owned(),
        list_path: mi
            .list_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    }
}
