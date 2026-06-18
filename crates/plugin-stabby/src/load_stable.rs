//! Host side of the native (mux-free) stable transport: wrap a loaded plugin's
//! [`DynProvider`] / [`DynManagedDriver`] as `hplugin::Provider` /
//! `hdriver_support::ManagedDriver`, so the engine drives them through its normal
//! traits. Each cold method is a direct stabby call; `get` passes the engine's
//! executor across natively (`HostExecutor`) so callbacks are direct.

use crate::abi::{
    CREATE_SYMBOL, CreateConfig, CreateFn, DynExecutor, DynManagedDriver, DynProvider, RunIo,
    StableManagedDriverDyn, StableMetaDyn, StableProviderDyn,
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

fn decode_stream(bytes: &[u8]) -> anyhow::Result<Vec<Body>> {
    use prost::bytes::Buf;
    let mut cur = bytes;
    let mut out = Vec::new();
    while cur.has_remaining() {
        let f = pb::Frame::decode_length_delimited(&mut cur)?;
        if let Some(b) = f.body {
            out.push(b);
        }
    }
    Ok(out)
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
            let bytes = self
                .inner
                .invoke(pb::ProviderMethod::List as u32, sv(&pb_req))
                .await;
            let mut out: Vec<anyhow::Result<ListResponse>> = Vec::new();
            for b in decode_stream(&bytes)? {
                match b {
                    Body::StreamItem(si) => {
                        let lr = pb::ListResponse::decode(&si.item[..])?;
                        out.push(Ok(ListResponse {
                            addr: convert::addr_from_pb(lr.addr.unwrap_or_default()),
                        }));
                    }
                    Body::StreamEnd(se) => {
                        if let Some(e) = se.error {
                            anyhow::bail!("{}", e.message);
                        }
                        break;
                    }
                    Body::Error(e) => anyhow::bail!("{}", e.message),
                    _ => {}
                }
            }
            Ok(Box::new(out.into_iter())
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
            let bytes = self
                .inner
                .invoke(pb::ProviderMethod::ListPackages as u32, sv(&pb_req))
                .await;
            let mut out: Vec<anyhow::Result<ListPackageResponse>> = Vec::new();
            for b in decode_stream(&bytes)? {
                match b {
                    Body::StreamItem(si) => {
                        let lpr = pb::ListPackageResponse::decode(&si.item[..])?;
                        out.push(Ok(ListPackageResponse {
                            pkg: PkgBuf::from(lpr.pkg),
                        }));
                    }
                    Body::StreamEnd(se) => {
                        if let Some(e) = se.error {
                            anyhow::bail!("{}", e.message);
                        }
                        break;
                    }
                    Body::Error(e) => anyhow::bail!("{}", e.message),
                    _ => {}
                }
            }
            Ok(Box::new(out.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ct: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, std::result::Result<GetResponse, GetError>> {
        Box::pin(async move {
            let pb_req = pb::GetRequest {
                request_id: req.request_id.clone(),
                addr: Some(convert::addr_to_pb(&req.addr)),
                states: req.states.iter().map(convert::state_to_pb).collect(),
            }
            .encode_to_vec();
            let exec: DynExecutor = HostExecutor::wrap(Arc::clone(&req.executor));
            let bytes = self
                .inner
                .invoke_exec(pb::ProviderMethod::Get as u32, sv(&pb_req), exec)
                .await;
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
        _ct: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move {
            let pb_req = pb::ProbeRequest {
                request_id: req.request_id,
                package: req.package.as_str().to_string(),
            }
            .encode_to_vec();
            let bytes = self
                .inner
                .invoke(pb::ProviderMethod::Probe as u32, sv(&pb_req))
                .await;
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
        _ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let pb_req = pb::ParseRequest {
            request_id: req.request_id,
            target_spec: Some(convert::target_spec_to_pb(req.target_spec.as_ref())),
            driver: self.name.clone(),
        }
        .encode_to_vec();
        let bytes = self
            .inner
            .invoke(pb::DriverMethod::Parse as u32, sv(&pb_req))
            .await;
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
        _ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        let pb_req = pb::ApplyTransitiveRequest {
            request_id: req.request_id,
            target_def: Some(convert::target_def_to_pb(&req.target_def)?),
            sandbox: Some(convert::sandbox_to_pb(&req.sandbox)),
            driver: self.name.clone(),
        }
        .encode_to_vec();
        let bytes = self
            .inner
            .invoke(pb::DriverMethod::ApplyTransitive as u32, sv(&pb_req))
            .await;
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
        _ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.dispatch_run(req, false).await
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        _ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.dispatch_run(req, true).await
    }
}

impl StableRemoteManagedDriver {
    async fn dispatch_run(
        &self,
        req: ManagedRunRequest<'_, '_>,
        shell: bool,
    ) -> anyhow::Result<ManagedRunResponse> {
        let pb_req = pb::ManagedRunRequest {
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
        }
        .encode_to_vec();
        // stdio rails are frozen but unused today (the subprocess inherits stdio at
        // spawn); pass all-None. `shell` already rides pb_req (set above).
        let io = RunIo {
            stdin: Default::default(),
            stdout: Default::default(),
            stderr: Default::default(),
        };
        let bytes = self
            .inner
            .invoke_io(pb::DriverMethod::Run as u32, sv(&pb_req), io)
            .await;
        match decode_unary(&bytes)? {
            Body::ManagedRunResp(r) => Ok(ManagedRunResponse {
                artifacts: r
                    .artifacts
                    .into_iter()
                    .map(convert::output_artifact_from_pb)
                    .collect(),
            }),
            Body::Error(e) => anyhow::bail!("{}", e.message),
            other => anyhow::bail!("unexpected managed run response: {other:?}"),
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
