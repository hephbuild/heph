//! wasm transport (in-process wasmtime component) — milestone M4.
//!
//! Runs a plugin compiled to a wasm component in-process via wasmtime. The
//! guest exports the provider/driver surface and imports the host callback
//! surface (the AbiHost: result/note_dep/query); both directions carry
//! protobuf-encoded `heph.plugin.v1` messages — the same wire schema the proto
//! and shm transports use (WIT in `crates/plugin-abi/wit/heph-plugin.wit`).
//!
//! [`WasmPlugin`] loads a component and exposes [`WasmPlugin::provider`] /
//! [`WasmPlugin::driver`] which implement the in-process `hplugin` traits, so
//! the engine registers them through its normal factory hooks and stays unaware
//! the plugin is wasm. Each call instantiates a fresh store/instance so the
//! per-request executor is isolated and concurrent calls don't share state.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::BoxFuture;
use hcore::hartifactcontent::Content;
use hcore::hasync::Cancellable;
use hplugin::provider::ProviderExecutor;
use plugin_abi::{convert, pb};
use prost::Message;
use std::sync::Arc;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

// ===========================================================================
// echo de-risk slice — kept; exercised by the `echo_*` wasm_e2e test. Proves
// the bare cargo-component guest ↔ wasmtime host contract (both directions).
// ===========================================================================
mod echo_bindings {
    wasmtime::component::bindgen!({
        // Must stay in sync with crates/wasm-guests/echo/wit/world.wit.
        inline: "package component:echo;\nworld echo {\n  import host-lookup: func(key: string) -> string;\n  export greet: func(name: string) -> string;\n}",
        world: "echo",
    });
}

/// Per-instance host state for the echo slice.
struct EchoState {
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for EchoState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl echo_bindings::EchoImports for EchoState {
    fn host_lookup(&mut self, key: String) -> String {
        format!("host:{key}")
    }
}

/// De-risk slice: instantiate `wasm` (a component exporting `greet`) and call
/// `greet(name)`. `greet` calls back into the host's `host-lookup` import.
pub fn instantiate_and_greet(wasm: &[u8], name: &str) -> Result<String> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)
        .map_err(anyhow::Error::from)
        .context("building wasmtime engine")?;
    let component = Component::from_binary(&engine, wasm)
        .map_err(anyhow::Error::from)
        .context("loading wasm component")?;

    let mut linker = Linker::<EchoState>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
        .map_err(anyhow::Error::from)
        .context("linking WASI imports")?;
    echo_bindings::Echo::add_to_linker::<_, HasSelf<EchoState>>(&mut linker, |state| state)
        .map_err(anyhow::Error::from)
        .context("linking host imports")?;

    let state = EchoState {
        table: ResourceTable::new(),
        wasi: WasiCtxBuilder::new().build(),
    };
    let mut store = Store::new(&engine, state);

    let echo = echo_bindings::Echo::instantiate(&mut store, &component, &linker)
        .map_err(anyhow::Error::from)
        .context("instantiating echo component")?;
    echo.call_greet(&mut store, name)
        .map_err(anyhow::Error::from)
        .context("calling greet export")
}

// ===========================================================================
// plugin transport — provider + driver over wasm, with host callbacks.
// ===========================================================================
mod plugin_bindings {
    wasmtime::component::bindgen!({
        path: "../plugin-abi/wit/heph-plugin.wit",
        world: "plugin",
        imports: { default: async },
        exports: { default: async },
    });
}

use plugin_bindings::heph::plugin::types::{ErrorKind as WErrorKind, PluginError as WPluginError};

/// Per-call host state: the current request's executor plus the lease table
/// holding result-artifact read-guards alive for the duration of the call.
struct PluginStore {
    wasi: WasiCtx,
    table: ResourceTable,
    executor: Option<Arc<dyn ProviderExecutor>>,
    leases: crate::lease::LeaseTable,
}

impl WasiView for PluginStore {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// True if `e`'s chain contains a dependency-cycle error.
fn is_cycle(e: &anyhow::Error) -> bool {
    hcore::hmemoizer::downcast_chain_ref::<hplugin::error::CycleError>(e).is_some()
}

fn werr(kind: WErrorKind, message: String) -> WPluginError {
    WPluginError { kind, message }
}

/// Classify an engine error into a typed wire error kind (downcast, never
/// message-matched) so the guest can reconstruct a typed error.
fn werr_from(e: &anyhow::Error) -> WPluginError {
    let kind = if is_cycle(e) {
        WErrorKind::Cycle
    } else if hplugin::error::is_cancelled(e) {
        WErrorKind::Cancelled
    } else {
        WErrorKind::Other
    };
    werr(kind, e.to_string())
}

// Host callback surface (the AbiHost): the guest calls these back while serving
// get()/parse(). Implemented against the per-call executor in the store.
impl plugin_bindings::heph::plugin::host::Host for PluginStore {
    async fn resolve(&mut self, req: Vec<u8>) -> Result<Vec<u8>, WPluginError> {
        let Some(executor) = self.executor.clone() else {
            return Err(werr(WErrorKind::Other, "no executor bound".into()));
        };
        let req = pb::ResultRequest::decode(&req[..])
            .map_err(|e| werr(WErrorKind::Other, format!("decode ResultRequest: {e}")))?;
        let addr = convert::addr_from_pb(req.addr.unwrap_or_default());
        match executor.result(&addr).await {
            Ok(eres) => {
                let artifacts: Vec<Arc<dyn Content>> = eres.artifacts.clone();
                let mut handles = Vec::with_capacity(artifacts.len());
                for (idx, art) in artifacts.iter().enumerate() {
                    let hashout = eres
                        .artifacts_meta
                        .get(idx)
                        .map(|m| m.hashout.clone())
                        .or_else(|| art.hashout().ok())
                        .unwrap_or_default();
                    handles.push(pb::ArtifactHandle {
                        handle_id: idx.to_string(),
                        group: String::new(),
                        name: String::new(),
                        hashout,
                        byte_size: art.byte_size().unwrap_or(0),
                        support: false,
                    });
                }
                let lease_id = self.leases.insert(artifacts);
                Ok(pb::ResultResponse {
                    lease_id,
                    artifacts: handles,
                }
                .encode_to_vec())
            }
            Err(e) => Err(werr_from(&e)),
        }
    }

    async fn note_dep(&mut self, req: Vec<u8>) -> Result<Vec<u8>, WPluginError> {
        let Some(executor) = self.executor.clone() else {
            return Err(werr(WErrorKind::Other, "no executor bound".into()));
        };
        let req = pb::NoteDepRequest::decode(&req[..])
            .map_err(|e| werr(WErrorKind::Other, format!("decode NoteDepRequest: {e}")))?;
        let addr = convert::addr_from_pb(req.addr.unwrap_or_default());
        let resp = match executor.note_dep(&addr).await {
            Ok(()) => pb::NoteDepResponse {
                ok: true,
                cycle: false,
                message: String::new(),
            },
            Err(e) => pb::NoteDepResponse {
                ok: false,
                cycle: is_cycle(&e),
                message: e.to_string(),
            },
        };
        Ok(resp.encode_to_vec())
    }

    async fn query(&mut self, req: Vec<u8>) -> Result<Vec<u8>, WPluginError> {
        let Some(executor) = self.executor.clone() else {
            return Err(werr(WErrorKind::Other, "no executor bound".into()));
        };
        let req = pb::QueryRequest::decode(&req[..])
            .map_err(|e| werr(WErrorKind::Other, format!("decode QueryRequest: {e}")))?;
        let matcher = convert::matcher_from_pb(req.matcher.unwrap_or_default());
        match executor.query(&matcher, &req.extra_skip).await {
            Ok(addrs) => Ok(pb::QueryResponse {
                addrs: addrs.iter().map(convert::addr_to_pb).collect(),
            }
            .encode_to_vec()),
            Err(e) => Err(werr_from(&e)),
        }
    }
}

/// A loaded wasm plugin component. Holds the engine + linked component; each
/// provider/driver call instantiates a fresh store/instance.
pub struct WasmPlugin {
    engine: Engine,
    component: Component,
    linker: Linker<PluginStore>,
    provider_name: String,
    driver_name: String,
}

impl WasmPlugin {
    /// Load a plugin component from raw bytes. `provider_name`/`driver_name` are
    /// the registry names the engine will route to this plugin (the guest also
    /// reports them via `*_config`, used as the source of truth where queried).
    pub fn load(
        wasm: &[u8],
        provider_name: impl Into<String>,
        driver_name: impl Into<String>,
    ) -> Result<Arc<Self>> {
        // wasmtime 45 always supports async; component-model must be enabled.
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)
            .map_err(anyhow::Error::from)
            .context("building wasmtime engine")?;
        let component = Component::from_binary(&engine, wasm)
            .map_err(anyhow::Error::from)
            .context("loading plugin component")?;

        let mut linker = Linker::<PluginStore>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)
            .map_err(anyhow::Error::from)
            .context("linking WASI imports")?;
        plugin_bindings::heph::plugin::host::add_to_linker::<_, HasSelf<PluginStore>>(
            &mut linker,
            |s| s,
        )
        .map_err(anyhow::Error::from)
        .context("linking host callback imports")?;

        Ok(Arc::new(Self {
            engine,
            component,
            linker,
            provider_name: provider_name.into(),
            driver_name: driver_name.into(),
        }))
    }

    fn new_store(&self, executor: Option<Arc<dyn ProviderExecutor>>) -> Store<PluginStore> {
        Store::new(
            &self.engine,
            PluginStore {
                wasi: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
                executor,
                leases: crate::lease::LeaseTable::default(),
            },
        )
    }

    async fn instantiate(&self, store: &mut Store<PluginStore>) -> Result<plugin_bindings::Plugin> {
        plugin_bindings::Plugin::instantiate_async(store, &self.component, &self.linker)
            .await
            .map_err(anyhow::Error::from)
            .context("instantiating plugin component")
    }

    /// Provider handle implementing `hplugin::Provider`.
    pub fn provider(self: &Arc<Self>) -> WasmProvider {
        WasmProvider {
            plugin: Arc::clone(self),
        }
    }

    /// Driver handle implementing `hplugin::Driver`.
    pub fn driver(self: &Arc<Self>) -> WasmDriver {
        WasmDriver {
            plugin: Arc::clone(self),
        }
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }
    pub fn driver_name(&self) -> &str {
        &self.driver_name
    }
}

/// A typed WIT error surfaced from a guest export.
fn wit_to_anyhow(e: &WPluginError) -> anyhow::Error {
    anyhow::anyhow!("plugin error ({:?}): {}", e.kind, e.message)
}

pub struct WasmProvider {
    plugin: Arc<WasmPlugin>,
}

impl hplugin::provider::Provider for WasmProvider {
    fn config(
        &self,
        _req: hplugin::provider::ConfigRequest,
    ) -> Result<hplugin::provider::ConfigResponse> {
        Ok(hplugin::provider::ConfigResponse {
            name: self.plugin.provider_name.clone(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: hplugin::provider::ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        Result<Box<dyn Iterator<Item = Result<hplugin::provider::ListResponse>> + Send>>,
    > {
        // list() is not part of the hello-world data path; an empty listing is
        // correct for a provider addressed only via get().
        Box::pin(async move {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = Result<hplugin::provider::ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: hplugin::provider::ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        Result<Box<dyn Iterator<Item = Result<hplugin::provider::ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            Ok(Box::new(std::iter::empty())
                as Box<
                    dyn Iterator<Item = Result<hplugin::provider::ListPackageResponse>> + Send,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: hplugin::provider::GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        std::result::Result<hplugin::provider::GetResponse, hplugin::provider::GetError>,
    > {
        Box::pin(async move {
            use hplugin::provider::GetError;
            let pb_req = pb::GetRequest {
                request_id: req.request_id,
                addr: Some(convert::addr_to_pb(&req.addr)),
                states: req.states.iter().map(convert::state_to_pb).collect(),
            };
            let mut store = self.plugin.new_store(Some(Arc::clone(&req.executor)));
            let inst = self
                .plugin
                .instantiate(&mut store)
                .await
                .map_err(GetError::Other)?;
            let res = inst
                .heph_plugin_api()
                .call_provider_get(&mut store, &pb_req.encode_to_vec())
                .await
                .map_err(anyhow::Error::from)
                .map_err(GetError::Other)?;
            match res {
                Ok(bytes) => {
                    let resp = pb::GetResponse::decode(&bytes[..])
                        .map_err(|e| GetError::Other(anyhow::anyhow!("decode GetResponse: {e}")))?;
                    Ok(hplugin::provider::GetResponse {
                        target_spec: convert::target_spec_from_pb(
                            resp.target_spec.unwrap_or_default(),
                        ),
                    })
                }
                Err(e) => match e.kind {
                    WErrorKind::NotFound => Err(GetError::NotFound),
                    WErrorKind::Cycle => Err(GetError::Other(anyhow::Error::new(
                        hplugin::error::CycleError {
                            from: req.addr.clone(),
                            to: req.addr.clone(),
                        },
                    ))),
                    WErrorKind::Cancelled => Err(GetError::Other(anyhow::Error::new(
                        hplugin::error::CancelledError,
                    ))),
                    _ => Err(GetError::Other(wit_to_anyhow(&e))),
                },
            }
        })
    }

    fn probe<'a>(
        &'a self,
        _req: hplugin::provider::ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<hplugin::provider::ProbeResponse>> {
        Box::pin(async move { Ok(hplugin::provider::ProbeResponse { states: vec![] }) })
    }
}

pub struct WasmDriver {
    plugin: Arc<WasmPlugin>,
}

#[async_trait]
impl hplugin::driver::Driver for WasmDriver {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: self.plugin.driver_name.clone(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        hplugin::driver::DriverSchema::default()
    }

    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<hplugin::driver::ParseResponse> {
        let pb_req = pb::ParseRequest {
            request_id: req.request_id,
            target_spec: Some(convert::target_spec_to_pb(req.target_spec.as_ref())),
            driver: self.plugin.driver_name.clone(),
        };
        let mut store = self.plugin.new_store(None);
        let inst = self.plugin.instantiate(&mut store).await?;
        let res = inst
            .heph_plugin_api()
            .call_driver_parse(&mut store, &pb_req.encode_to_vec())
            .await
            .map_err(anyhow::Error::from)?;
        let bytes = res.map_err(|e| wit_to_anyhow(&e))?;
        let resp = pb::ParseResponse::decode(&bytes[..]).context("decode ParseResponse")?;
        Ok(hplugin::driver::ParseResponse {
            target_def: convert::target_def_from_pb(resp.target_def.unwrap_or_default())?,
        })
    }

    async fn apply_transitive(
        &self,
        _req: hplugin::driver::ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<hplugin::driver::ApplyTransitiveResponse> {
        anyhow::bail!("wasm driver apply_transitive() not implemented")
    }

    async fn run<'a, 'io>(
        &self,
        req: hplugin::driver::RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<hplugin::driver::RunResponse> {
        self.run_inner(req, false).await
    }

    async fn run_shell<'a, 'io>(
        &self,
        req: hplugin::driver::RunRequest<'a, 'io>,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> Result<hplugin::driver::RunResponse> {
        self.run_inner(req, true).await
    }
}

impl WasmDriver {
    async fn run_inner<'a, 'io>(
        &self,
        req: hplugin::driver::RunRequest<'a, 'io>,
        shell: bool,
    ) -> Result<hplugin::driver::RunResponse> {
        // stdio crosses as inherited fds elsewhere; inputs for the hello-world
        // driver are not materialized over wasm (no abstract-artifact pull yet).
        let pb_req = pb::RunRequest {
            request_id: req.request_id.clone(),
            target: Some(convert::target_def_to_pb(req.target)?),
            tree_root_path: req.tree_root_path.to_string_lossy().into_owned(),
            inputs: vec![],
            hashin: req.hashin.to_string(),
            sandbox_dir: req.sandbox_dir.to_string_lossy().into_owned(),
            has_stdin: req.stdin.is_some(),
            has_stdout: req.stdout.is_some(),
            has_stderr: req.stderr.is_some(),
            shell,
        };
        let mut store = self.plugin.new_store(None);
        let inst = self.plugin.instantiate(&mut store).await?;
        let res = inst
            .heph_plugin_api()
            .call_driver_run(&mut store, &pb_req.encode_to_vec())
            .await
            .map_err(anyhow::Error::from)?;
        let bytes = res.map_err(|e| wit_to_anyhow(&e))?;
        let resp = pb::RunResponse::decode(&bytes[..]).context("decode RunResponse")?;
        Ok(hplugin::driver::RunResponse {
            artifacts: resp
                .artifacts
                .into_iter()
                .map(convert::output_artifact_from_pb)
                .collect(),
            ..Default::default()
        })
    }
}
