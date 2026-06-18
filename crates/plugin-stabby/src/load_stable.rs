//! Host side of the native (mux-free) stable transport: wrap a loaded plugin's
//! [`DynProvider`] / [`DynManagedDriver`] as `hplugin::Provider` /
//! `hdriver_support::ManagedDriver`, so the engine drives them through its normal
//! traits. Each cold method is a direct stabby call; `get` passes the engine's
//! executor across natively (`HostExecutor`) so callbacks are direct.

use crate::abi::{
    CREATE_SYMBOL, CreateConfig, CreateFn, DynExecutor, DynItemStream, DynManagedDriver,
    DynProvider, StableCancelDyn, StableItemStream, StableItemStreamDyn, StableManagedDriverDyn,
    StableMetaDyn, StableProviderDyn,
};
use crate::host::HostExecutor;
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

/// A loaded plugin's host-side handles: an optional provider + named drivers.
pub type LoadedComponents = (
    Option<StableRemoteProvider>,
    Vec<(String, StableRemoteManagedDriver)>,
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
    options: &[u8],
) -> anyhow::Result<LoadedComponents> {
    use crate::abi::PluginComponents;
    use anyhow::Context;
    use stabby::libloading::StabbyLibrary;

    // SAFETY: loading a plugin dylib runs its initializers; the path is operator-
    // controlled config. The ABI of what we call is checked below via get_stabbied.
    let lib = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("dlopen plugin {}", path.display()))?;

    // Scope the symbol borrow so the library can be leaked after the call.
    let comps: PluginComponents = {
        // SAFETY: get_stabbied verifies the symbol's stabby type report matches
        // `CreateFn` before returning it; calling it is then ABI-sound.
        let create = unsafe { lib.get_stabbied::<CreateFn>(CREATE_SYMBOL) }
            .map_err(|e| anyhow::anyhow!("stabby ABI check failed for {}: {e}", path.display()))?;
        create(CreateConfig {
            root: root.into(),
            home: home.into(),
            options: stabby::vec::Vec::from(options),
        })
    };
    // Keep the dylib mapped for the process lifetime (the returned trait objects'
    // vtables point into its code); leaking the handle is intentional.
    let _: &'static mut libloading::Library = Box::leak(Box::new(lib));

    let PluginComponents {
        provider_name,
        provider,
        drivers,
    } = comps;
    let pname = provider_name.to_string();
    let host_provider = if pname.is_empty() {
        None
    } else {
        Some(StableRemoteProvider::new(provider, pname))
    };

    let mut host_drivers = Vec::new();
    for nd in drivers {
        let name = nd.name.to_string();
        host_drivers.push((
            name.clone(),
            StableRemoteManagedDriver::new(nd.driver, name),
        ));
    }
    Ok((host_provider, host_drivers))
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
    stabby::boxed::Box::new(HostItemStream {
        items: std::sync::Mutex::new(items.into_iter()),
    })
    .into()
}

/// Drain a bidi `run` response stream to its terminal result. Blocking (called on a
/// blocking task) because the stream's `next` blocks on subprocess output. stdout/
/// stderr chunks are reserved — routed to host stdio once live streaming is wired.
fn drain_run(stream: DynItemStream) -> anyhow::Result<ManagedRunResponse> {
    loop {
        let bytes = stream.next();
        if bytes.is_empty() {
            anyhow::bail!("run stream ended without a result");
        }
        match pb::RunOutFrame::decode(&bytes[..])?.msg {
            Some(pb::run_out_frame::Msg::Response(r)) => {
                return Ok(ManagedRunResponse {
                    artifacts: r
                        .artifacts
                        .into_iter()
                        .map(convert::output_artifact_from_pb)
                        .collect(),
                });
            }
            Some(pb::run_out_frame::Msg::Error(e)) => anyhow::bail!("{e}"),
            // stdout/stderr chunks: not yet surfaced; ignore and keep draining.
            _ => {}
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
            // Server-streaming: pull items lazily across the seam — the whole list
            // is never materialized on either side.
            let stream = self
                .inner
                .invoke_server_stream(pb::ProviderMethod::List as u32, sv(&pb_req))
                .await;
            Ok(Box::new(ItemStreamIter {
                stream,
                decode: decode_list_item,
                done: false,
            }) as Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>)
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
                as Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>)
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
            let exec: DynExecutor = HostExecutor::wrap(Arc::clone(&req.executor));
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
        let cb = crate::host::HostFunctionRegistry::wrap(reg);
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
/// component) for the engine's driver factory.
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
        true
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
        req: ManagedRunRequest<'_, '_>,
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
        };
        // `run` is bidi: the request stream carries the run request (RunInFrame),
        // the response stream carries the result (RunOutFrame). `shell` rides pmrr.
        let in_frame = pb::RunInFrame {
            msg: Some(pb::run_in_frame::Msg::Start(pmrr)),
        }
        .encode_to_vec();
        let resp_stream = self
            .inner
            .invoke_bidi(pb::DriverMethod::Run as u32, host_item_stream(vec![in_frame]))
            .await;
        // The response stream's `next` blocks (on subprocess output), so drain it on
        // a dedicated thread and bridge the result back — while watching `ct` so a
        // cancel trips the plugin's run token (which stops the subprocess).
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            // Receiver dropped only if the caller gave up; ignore send failure.
            drop(tx.send(drain_run(resp_stream)));
        });
        match await_with_cancel(ct, driver_cancel(&self.inner, &request_id), rx).await {
            Ok(result) => result,
            Err(_canceled) => anyhow::bail!("run drain thread dropped"),
        }
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
