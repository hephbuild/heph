//! Host side of the native (mux-free) stable transport: wrap a loaded plugin's
//! [`DynProvider`] / [`DynManagedDriver`] as `hplugin::Provider` /
//! `hdriver_support::ManagedDriver`, so the engine drives them through its normal
//! traits. Each cold method is a direct stabby call; `get` passes the engine's
//! executor across natively (`HostExecutor`) so callbacks are direct.

use crate::abi::{
    CREATE_SYMBOL, CreateFn, DynExecutor, DynHook, DynItemStream, DynManagedDriver,
    DynPluginFunctions, DynProvider, NamedDriver, NamedFunction, SET_LOG_FILTER_SYMBOL,
    SET_LOG_SINK_SYMBOL, SET_RUNNER_HOST_SYMBOL, SET_SUPERVISOR_SYMBOL, SetLogFilterFn,
    SetLogSinkFn, SetRunnerHostFn, SetSupervisorFn, StableCancelDyn, StableHookDyn,
    StableItemStream, StableItemStreamDyn, StableManagedDriverDyn, StableMetaDyn,
    StablePluginFunctionsDyn, StableProviderDyn,
};
use crate::host::HostExecutor;
use crate::vtable::dynify;
use async_trait::async_trait;
use buffa::Message;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunInput, ManagedRunRequest, ManagedRunResponse,
};
use hmodel::htpkg::PkgBuf;
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest as DriverConfigRequest,
    ConfigResponse as DriverConfigResponse, DriverSchema, ParseRequest, ParseResponse,
    inputartifact,
};
use hplugin::function::{FnArgs, FnCallContext, FnOutcome, FunctionSlot, PluginFn, PluginFnDef};
use hplugin::hook::Hook;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse, Provider,
    StateSchema,
};
use plugin_abi::convert::{required, required_enum, required_msg};
use plugin_abi::pb::frame::Body;
use plugin_abi::{convert, pb};
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

/// A loaded plugin's host-side handles.
pub struct LoadedComponents {
    pub provider: Option<StableRemoteProvider>,
    pub drivers: Vec<(String, StableRemoteManagedDriver)>,
    /// The plugin's BUILD-file functions, metadata decoded, each calling through
    /// the plugin's one functions handle.
    pub functions: Vec<PluginFnDef>,
    pub hooks: Vec<(String, StableRemoteHook)>,
    /// Exec runners the plugin implements, ready to register beside the
    /// builtins. Empty for a plugin that only names one in its `runner.json`.
    pub runners: Vec<std::sync::Arc<dyn hexecrunner::registry::ExecRunner>>,
}

/// The text a create entry that fails stabby's structural check gets: the
/// plugin was built against a different `PluginComponents` / `CreateFn` layout,
/// which is what an ABI break changes. The version check ([`check_plugin_abi`])
/// runs first; this catches a plugin whose reported version is accepted but
/// whose layout still differs.
pub fn abi_mismatch_hint(path: &std::path::Path) -> String {
    abi_refusal(path, format_args!("host {}", plugin_abi::ABI_SEMVER))
}

/// The one shape every ABI refusal takes, so a user learns the same fix
/// whichever check caught the plugin.
fn abi_refusal(path: &std::path::Path, detail: std::fmt::Arguments<'_>) -> String {
    format!(
        "plugin {} was built against a different heph ABI ({detail}); rebuild it against \
         this heph, or reinstall plugins from the same release as this heph",
        path.display(),
    )
}

/// The ABI version this plugin exported, or `None` when it exports none.
///
/// Called before anything else in the plugin: the symbol is a plain C function
/// precisely so that reading it trusts no stabby layout.
fn plugin_abi_version(lib: &libloading::Library) -> Option<String> {
    use crate::abi::{ABI_SEMVER_SYMBOL, AbiSemverFn};
    // SAFETY: `ABI_SEMVER_SYMBOL`'s type is fixed by contract (never renamed or
    // retyped, see `abi.rs`): `unsafe extern "C" fn() -> *const c_char`.
    let entry = unsafe { lib.get::<AbiSemverFn>(ABI_SEMVER_SYMBOL) }.ok()?;
    // SAFETY: the entry takes no arguments and returns a pointer to a
    // `'static` string, by the same contract.
    let ptr = unsafe { entry() };
    if ptr.is_null() {
        return Some(String::new());
    }
    // SAFETY: non-null, and NUL-terminated and `'static` by contract; copied out
    // before anything else runs.
    let reported = unsafe { std::ffi::CStr::from_ptr(ptr) };
    Some(reported.to_string_lossy().into_owned())
}

/// The plugin ABI versions this host loads: `^MIN_PLUGIN_ABI`. Pre-1.0 that is
/// one minor's lane (`^0.17.0` = `>=0.17.0, <0.18.0`): a patch is additive, so
/// a plugin a few patches newer or older than the host loads; a minor is a
/// break, which raises `MIN_PLUGIN_ABI` to the new `x.y.0`.
pub fn accepted_plugin_abi() -> anyhow::Result<semver::VersionReq> {
    use anyhow::Context as _;
    semver::VersionReq::parse(&format!("^{}", plugin_abi::MIN_PLUGIN_ABI))
        .context("parsing MIN_PLUGIN_ABI")
}

/// Refuse a plugin whose reported ABI version (`None`: it reports none, so it
/// predates 0.17.0, when the version started being read) does not match
/// [`accepted_plugin_abi`]. Versions compare as semver, never as strings.
///
/// A plugin from a later minor is refused like an older one: this host cannot
/// know what that break changed.
pub fn check_plugin_abi(path: &std::path::Path, reported: Option<&str>) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use semver::Version;
    let host = Version::parse(plugin_abi::ABI_SEMVER).context("parsing the host's ABI_SEMVER")?;
    let req = accepted_plugin_abi()?;
    let accepts = format!("host {host}, which loads {req}");
    let Some(reported) = reported else {
        anyhow::bail!(abi_refusal(
            path,
            format_args!("it reports no ABI version, so it predates 0.17.0; {accepts}")
        ));
    };
    let plugin = match Version::parse(reported) {
        Ok(v) => v,
        Err(_) => anyhow::bail!(abi_refusal(
            path,
            format_args!("it reports ABI version {reported:?}, which is not a version; {accepts}")
        )),
    };
    if req.matches(&plugin) {
        return Ok(());
    }
    if plugin > host {
        anyhow::bail!(abi_refusal(
            path,
            format_args!("plugin {plugin}, newer than this heph, which is unsupported; {accepts}")
        ));
    }
    anyhow::bail!(abi_refusal(
        path,
        format_args!("plugin {plugin}; {accepts}")
    ))
}

/// An optional entry point. `None`, silently, when the plugin does not export
/// `symbol` (an SDK that predates it); `None` with a `warn!` when it exports one
/// whose stabby type report does not match ours — a skewed plugin, whose
/// capability would otherwise just vanish without a word.
fn optional_entry<'l, T: stabby::IStable>(
    lib: &'l libloading::Library,
    symbol: &[u8],
    path: &std::path::Path,
) -> Option<stabby::libloading::Symbol<'l, T>> {
    use stabby::libloading::StabbyLibrary;
    // SAFETY: only probes that the symbol exists; nothing is called through
    // this pointer type.
    if unsafe { lib.get::<*const ()>(symbol) }.is_err() {
        return None;
    }
    // SAFETY: get_stabbied checks the symbol's stabby type report against `T`
    // before returning it.
    match unsafe { lib.get_stabbied::<T>(symbol) } {
        Ok(entry) => Some(entry),
        Err(e) => {
            tracing::warn!(
                plugin = %path.display(),
                symbol = %String::from_utf8_lossy(symbol),
                error = %e,
                "plugin exports this entry point with a type that does not match this heph; \
                 skipping it. Rebuild the plugin against the same heph release"
            );
            None
        }
    }
}

/// A plugin's exported drivers as host-side drivers, each under the local name
/// it was exported with — the name the engine registers it by (prefixed with
/// the plugin's). The plugin-side driver's own `config()` is never asked. Fails
/// on a driver whose schema does not decode.
pub fn decode_drivers(
    drivers: impl IntoIterator<Item = NamedDriver>,
) -> anyhow::Result<Vec<(String, StableRemoteManagedDriver)>> {
    drivers
        .into_iter()
        .map(|nd| {
            let name = nd.name.to_string();
            let driver = StableRemoteManagedDriver::new(nd.driver, name.clone())?;
            Ok((name, driver))
        })
        .collect()
}

/// Decode a plugin's exported function metadata into host-side defs, each
/// calling through `handle`. Fails — naming the plugin and the function — on
/// undecodable metadata, a missing signature, a name that disagrees with its
/// metadata, or functions without a handle: a dropped function would surface
/// later as a confusing "no function" in some BUILD file.
pub fn decode_functions(
    plugin: &str,
    functions: Vec<NamedFunction>,
    handle: Option<DynPluginFunctions>,
) -> anyhow::Result<Vec<PluginFnDef>> {
    use anyhow::Context;
    if functions.is_empty() {
        return Ok(Vec::new());
    }
    let handle = Arc::new(handle.ok_or_else(|| {
        anyhow::anyhow!(
            "plugin {plugin:?} exports {} function(s) but no function handle to call them through",
            functions.len()
        )
    })?);
    functions
        .into_iter()
        .map(|nf| {
            let name = nf.name.to_string();
            let def = plugin_abi::decode::<pb::FunctionDef>(&nf.meta[..])
                .with_context(|| format!("decoding the metadata of function {plugin}.{name}"))?;
            let def_name = required(def.name, "FunctionDef.name")
                .with_context(|| format!("function {plugin}.{name}"))?;
            if def_name != name {
                anyhow::bail!(
                    "plugin {plugin:?} exports function {name:?} whose metadata names {def_name:?}"
                );
            }
            let signature = required_msg(def.signature, "FunctionDef.signature")
                .and_then(convert::fn_signature_from_pb)
                .with_context(|| format!("the signature of function {plugin}.{name}"))?;
            Ok(PluginFnDef {
                signature,
                doc: def.doc.unwrap_or_default(),
                func: Arc::new(StableRemoteFn {
                    handle: Arc::clone(&handle),
                    name: name.clone(),
                }),
                name,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()
        .with_context(|| format!("loading the functions of plugin {plugin:?}"))
}

/// Load a plugin cdylib and construct the host-side handles. The library's ABI is
/// verified against ours via stabby's type reports (`get_stabbied`); a mismatch
/// (different stabby version, or drifted boundary types) is a hard error, reported
/// with [`abi_mismatch_hint`]. The `Library` is intentionally leaked: the returned
/// trait objects' vtables live in the dylib's code, which must stay mapped for
/// the process lifetime.
///
/// `cfg` is what the plugin's create entry receives. Its `name` is the plugin's
/// name, from its manifest: its provider and functions are named after it; the
/// binary reports no name.
///
/// `runtime` is the engine's: host-side bodies the plugin calls into (the
/// runner host's `prepare`, the function registry's `call_registered`) are
/// spawned there. It is passed rather than probed because `load` runs on rayon
/// workers, where `Handle::try_current()` always fails — a probe would silently
/// hand every production plugin the inline, runtime-less handle meant for test
/// harnesses.
///
/// `functions` is the loading engine's function slot. The plugin's create entry
/// gets a registry handle resolving through it — this instance's own, so two
/// engines in one process never see each other's functions.
pub fn load(
    path: &std::path::Path,
    cfg: &convert::CreateConfig,
    runtime: tokio::runtime::Handle,
    functions: Arc<FunctionSlot>,
) -> anyhow::Result<LoadedComponents> {
    use crate::abi::PluginComponents;
    use anyhow::Context;
    use stabby::libloading::StabbyLibrary;

    let name = cfg.name.as_str();
    // The create config crosses as protobuf bytes so its fields are additive;
    // the options ride it as structured values (no nested encode).
    let cfg = convert::create_config_to_pb(cfg).encode_to_vec();

    // SAFETY: loading a plugin dylib runs its initializers; the path is operator-
    // controlled config. The ABI of what we call is checked below via get_stabbied.
    let lib = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("dlopen plugin {}", path.display()))?;

    // The plugin's ABI version, before anything else in it is called: a plugin
    // from another ABI is refused here, once, rather than failing each call
    // with whatever the skew happens to break (a wire-only break passes
    // stabby's structural check below).
    check_plugin_abi(path, plugin_abi_version(&lib).as_deref())?;

    // Scope the symbol borrow so the library can be leaked after the call.
    let comps: PluginComponents = {
        // Install the host log sink and supervisor *before* `create` runs: `create`
        // is exactly where plugin construction can fail, and a `tracing::error!`
        // logged during that failure needs a subscriber already installed or it is
        // silently dropped — right before the ABI seam turns any panic into a
        // non-unwinding abort with no diagnostic at all. Optional: a plugin built
        // against an older SDK simply won't export these symbols; that is not an
        // error — but one exported with a mismatched type is warned about.
        if let Some(set_sink) = optional_entry::<SetLogSinkFn>(&lib, SET_LOG_SINK_SYMBOL, path) {
            set_sink(crate::host::HostLogSink::wrap());
        }
        // Then the host's log filter, so the plugin drops what the host would
        // discard instead of forwarding every event. Same older-SDK tolerance; a
        // plugin without it, or a host that set no filter, forwards everything.
        let set_filter = optional_entry::<SetLogFilterFn>(&lib, SET_LOG_FILTER_SYMBOL, path);
        if let (Some(set_filter), Some(bytes)) = (set_filter, crate::hostlog::plugin_log_filter()) {
            set_filter(stabby::vec::Vec::from(bytes.as_slice()));
        }
        // Hand the plugin the host's supervisor client. The plugin's own copy of
        // the `proc` crate has an uninitialised tracker (statics are not shared
        // across the dylib boundary), so without this every child it spawns goes
        // unregistered — no reaping on a hard kill of the host, and a warning per
        // spawn. Same older-SDK tolerance as the log sink.
        if let Some(set_supervisor) =
            optional_entry::<SetSupervisorFn>(&lib, SET_SUPERVISOR_SYMBOL, path)
        {
            set_supervisor(crate::host::HostSupervisor::wrap());
        }
        // Hand the plugin a handle to the host's exec-runner registry. Same
        // reason as the supervisor: this cdylib links its own `execrunner`,
        // whose registry `Engine::install_exec_runner_host` never reached, so
        // without this a driver in here whose target names a runner fails with
        // "no runner host is installed in this component". It does not degrade
        // to a local spawn — that would run the target outside the environment
        // its cache key claims. Same older-SDK tolerance as the two above.
        if let Some(set_runner) =
            optional_entry::<SetRunnerHostFn>(&lib, SET_RUNNER_HOST_SYMBOL, path)
        {
            set_runner(crate::host::HostRunnerHost::wrap(runtime.clone()));
        }
        // SAFETY: get_stabbied verifies the symbol's stabby type report matches
        // `CreateFn` before returning it; calling it is then ABI-sound.
        let create = unsafe { lib.get_stabbied::<CreateFn>(CREATE_SYMBOL) }
            .map_err(|e| anyhow::anyhow!("stabby ABI check failed: {e}"))
            .with_context(|| abi_mismatch_hint(path))?;
        // This instance's registry handle: resolved through this engine's slot,
        // on this engine's runtime.
        create(
            sv(&cfg),
            crate::host::HostFunctionRegistry::wrap(functions, runtime),
        )
    };
    // Keep the dylib mapped for the process lifetime (the returned trait objects'
    // vtables point into its code); leaking the handle is intentional.
    let _: &'static mut libloading::Library = Box::leak(Box::new(lib));

    let PluginComponents {
        provider,
        drivers,
        functions,
        function_handle,
        hooks,
        runners,
        // Reserved return-side metadata; nothing consumes it yet.
        meta: _,
    } = comps;
    let functions = decode_functions(
        name,
        functions.into_iter().collect(),
        function_handle.into(),
    )?;
    // `provider` is optional: hook-only / driver-only plugins export `None`.
    let provider: std::option::Option<DynProvider> = provider.into();
    let host_provider = provider
        .map(|p| StableRemoteProvider::new(p, name))
        .transpose()
        .with_context(|| format!("loading plugin {name:?}"))?;

    let host_drivers =
        decode_drivers(drivers).with_context(|| format!("loading plugin {name:?}"))?;

    let mut host_hooks = Vec::new();
    for nh in hooks {
        let name = nh.name.to_string();
        host_hooks.push((name.clone(), StableRemoteHook::new(nh.hook, name)));
    }
    let mut host_runners: Vec<std::sync::Arc<dyn hexecrunner::registry::ExecRunner>> = Vec::new();
    for nr in runners {
        let name = nr.name.to_string();
        host_runners.push(std::sync::Arc::new(crate::runner::PluginRunner::new(
            name, nr.runner,
        )));
    }
    Ok(LoadedComponents {
        provider: host_provider,
        drivers: host_drivers,
        functions,
        hooks: host_hooks,
        runners: host_runners,
    })
}

fn decode_unary(bytes: &[u8]) -> anyhow::Result<Body> {
    use anyhow::Context as _;
    let frame =
        plugin_abi::decode::<pb::Frame>(bytes).context("decoding a stable response frame")?;
    required(frame.body, "Frame.body")
}

/// The text of a wire `Error`. A missing message is an error with no text — an
/// honest default, unlike a missing kind.
fn error_text(e: pb::Error) -> String {
    e.message.unwrap_or_default()
}

/// Host-side lazy adapter over a plugin response stream: each `next` pulls one
/// frame across the seam (`StableItemStream::next`) and decodes it, so items flow
/// incrementally and the full set is never buffered. `Send` (the stream handle is
/// `Send + Sync`, `decode` is a fn pointer) so it satisfies the engine's `Provider`
/// iterator bound.
struct ItemStreamIter<D> {
    stream: DynItemStream,
    decode: D,
    done: bool,
}

impl<T, D: FnMut(&[u8]) -> anyhow::Result<T>> Iterator for ItemStreamIter<D> {
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
        let frame = match plugin_abi::decode::<pb::Frame>(&bytes[..]) {
            Ok(f) => f,
            Err(e) => {
                self.done = true;
                return Some(Err(anyhow::anyhow!("stream frame decode: {e}")));
            }
        };
        match frame.body {
            Some(Body::StreamItem(si)) => match si.item {
                Some(item) => Some((self.decode)(&item)),
                None => {
                    self.done = true;
                    Some(Err(anyhow::anyhow!("missing field StreamItem.item")))
                }
            },
            Some(Body::StreamEnd(se)) => {
                self.done = true;
                se.error
                    .into_option()
                    .map(|e| Err(anyhow::anyhow!("{}", error_text(e))))
            }
            Some(Body::Error(e)) => {
                self.done = true;
                Some(Err(anyhow::anyhow!("{}", error_text(*e))))
            }
            _ => {
                self.done = true;
                None
            }
        }
    }
}

/// One `list` call's decoder. Interns driver names for the call, so a package
/// of a thousand entries holds each driver once rather than once per entry.
fn list_item_decoder() -> impl FnMut(&[u8]) -> anyhow::Result<ListResponse> + Send {
    let mut drivers: std::collections::HashMap<String, Arc<str>> = Default::default();
    move |b| {
        decode_list_item(b, &mut |d: String| {
            Arc::clone(
                drivers
                    .entry(d)
                    .or_insert_with_key(|d| Arc::from(d.as_str())),
            )
        })
    }
}

/// A malformed `ListedFacts` fails the whole item, and so the provider's
/// `list` — nested messages decode eagerly. An absent one (a plugin older than
/// ABI 0.13, or `addr_only`) is every fact unknown, never a prune. A missing
/// `addr` fails the item too: there is no target to list.
fn decode_list_item(
    b: &[u8],
    intern: &mut impl FnMut(String) -> Arc<str>,
) -> anyhow::Result<ListResponse> {
    use anyhow::Context as _;
    let lr = plugin_abi::decode::<pb::ListResponse>(b).context("decoding a listed target")?;
    let addr = convert::addr_field(lr.addr, "ListResponse.addr")?;
    let facts = convert::listed_facts_from_pb(lr.facts.into_option(), intern)
        .with_context(|| format!("the listed facts of {}", addr.format()))?;
    Ok(ListResponse::with_facts(addr, facts))
}

fn decode_list_package_item(b: &[u8]) -> anyhow::Result<ListPackageResponse> {
    use anyhow::Context as _;
    let lpr =
        plugin_abi::decode::<pb::ListPackageResponse>(b).context("decoding a listed package")?;
    Ok(ListPackageResponse {
        pkg: PkgBuf::from(required(lpr.pkg, "ListPackageResponse.pkg")?),
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
fn drain_run(
    stream: DynItemStream,
    chunks: &tokio::sync::mpsc::UnboundedSender<Chunk>,
) -> anyhow::Result<ManagedRunResponse> {
    loop {
        let bytes = stream.next();
        if bytes.is_empty() {
            anyhow::bail!("run stream ended without a result");
        }
        let frame = {
            use anyhow::Context as _;
            plugin_abi::decode::<pb::RunOutFrame>(&bytes[..]).context("decoding a run frame")?
        };
        match frame.msg {
            Some(pb::run_out_frame::Msg::Response(r)) => {
                return Ok(ManagedRunResponse {
                    artifacts: r
                        .artifacts
                        .into_iter()
                        .map(convert::output_artifact_from_pb)
                        .collect::<anyhow::Result<_>>()?,
                });
            }
            Some(pb::run_out_frame::Msg::Error(e)) => anyhow::bail!("{e}"),
            // A dropped receiver means the caller stopped reading; keep draining
            // so the run still reaches its result.
            Some(pb::run_out_frame::Msg::StdoutChunk(b)) => {
                drop(chunks.send(Chunk::Stdout(b)));
            }
            Some(pb::run_out_frame::Msg::StderrChunk(b)) => {
                drop(chunks.send(Chunk::Stderr(b)));
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
    /// Decoded once, at load: `state_schema` has no error channel, and the
    /// plugin's answer never changes.
    state_schema: Option<Arc<StateSchema>>,
}

impl StableRemoteProvider {
    /// Wrap a loaded provider, reading its state schema. A schema that does not
    /// decode fails here — at load, naming the provider — like undecodable
    /// function metadata, rather than reading as "no schema" on every call.
    pub fn new(inner: DynProvider, name: impl Into<String>) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let name = name.into();
        // An empty SVec encodes `None`; any encoded `Schema` (even fields-empty)
        // encodes `Some`.
        let state_schema =
            decode_state_schema(&inner.meta(pb::ProviderMethod::StateSchema as u32)[..])
                .with_context(|| format!("decoding the state schema of provider {name:?}"))?
                .map(Arc::new);
        Ok(Self {
            inner: Arc::new(inner),
            name,
            state_schema,
        })
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
                request_id: Some(req.request_id),
                package: Some(req.package.as_str().to_string()),
                states: req.states.iter().map(convert::state_to_pb).collect(),
                ..Default::default()
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
                decode: list_item_decoder(),
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
                prefix: Some(req.prefix.as_str().to_string()),
                ..Default::default()
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
                request_id: Some(req.request_id.clone()),
                addr: convert::addr_to_pb(&req.addr).into(),
                states: req.states.iter().map(convert::state_to_pb).collect(),
                ..Default::default()
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
                    target_spec: required_msg(gr.target_spec, "GetResponse.target_spec")
                        .and_then(convert::target_spec_from_pb)
                        .map_err(GetError::Other)?,
                }),
                // A missing or unknown kind is an error with the plugin's message:
                // never `NotFound`, which would read as "no such target".
                Body::GetErr(ge) => match required_enum(ge.kind, "GetError.kind") {
                    Ok(pb::get_error::Kind::NotFound) => Err(GetError::NotFound),
                    Ok(pb::get_error::Kind::Cycle) => Err(GetError::Other(anyhow::Error::new(
                        hplugin::error::CycleError {
                            from: req.addr.clone(),
                            to: req.addr.clone(),
                        },
                    ))),
                    Ok(pb::get_error::Kind::Cancelled) => Err(GetError::Other(anyhow::Error::new(
                        hplugin::error::CancelledError,
                    ))),
                    Ok(_) => Err(GetError::Other(anyhow::anyhow!(
                        "{}",
                        ge.message.unwrap_or_default()
                    ))),
                    Err(kind) => Err(GetError::Other(
                        kind.context(ge.message.unwrap_or_default()),
                    )),
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
                request_id: Some(req.request_id),
                package: Some(req.package.as_str().to_string()),
                ..Default::default()
            }
            .encode_to_vec();
            let fut = self
                .inner
                .invoke(pb::ProviderMethod::Probe as u32, sv(&pb_req));
            let bytes = await_with_cancel(ct, provider_cancel(&self.inner, &request_id), fut).await;
            match decode_unary(&bytes)? {
                Body::ProbeResp(pr) => Ok(ProbeResponse {
                    states: convert::states_from_pb(pr.states)?,
                }),
                Body::Error(e) => anyhow::bail!("{}", error_text(*e)),
                other => anyhow::bail!("unexpected probe response: {other:?}"),
            }
        })
    }

    fn state_schema(&self) -> Option<StateSchema> {
        self.state_schema.as_deref().cloned()
    }
}

/// Proxy handler for a cdylib plugin function: each call encodes its args and
/// the `FnCallContext`, dispatches `FUNCTION_METHOD_CALL` through the plugin's
/// one functions handle, and decodes the reply into an [`FnOutcome`] — value
/// plus whatever the plugin declared. The host merges the declarations at the
/// call site, so a cdylib function stands up targets exactly as an in-process
/// one does.
struct StableRemoteFn {
    handle: Arc<DynPluginFunctions>,
    name: String,
}

#[async_trait]
impl PluginFn for StableRemoteFn {
    async fn call(&self, ctx: &FnCallContext<'_>, args: FnArgs) -> anyhow::Result<FnOutcome> {
        let (positional, named) = convert::fn_args_to_pb(&args);
        let pb_req = pb::CallFunctionRequest {
            name: Some(self.name.clone()),
            pkg: Some(ctx.pkg.to_string()),
            root: Some(ctx.root.to_string_lossy().into_owned()),
            positional,
            named,
            // The host merges declarations into the package being evaluated.
            accepts_declarations: Some(true),
            ..Default::default()
        }
        .encode_to_vec();
        let bytes = self
            .handle
            .invoke(
                pb::FunctionMethod::Call as u32,
                self.name.as_str().into(),
                sv(&pb_req),
            )
            .await;
        match decode_unary(&bytes)? {
            Body::CallFunctionResp(r) => convert::fn_outcome_from_pb(*r),
            Body::Error(e) => anyhow::bail!("{}", error_text(*e)),
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
    /// Decoded once, at load: `schema` has no error channel, and the plugin's
    /// answer never changes.
    schema: Arc<DriverSchema>,
}

impl StableRemoteManagedDriver {
    /// Wrap a loaded driver, reading its config schema. A schema that does not
    /// decode fails here — at load, naming the driver — rather than reading as
    /// the empty schema on every call, which would flag each of the driver's
    /// keys as unknown.
    pub fn new(inner: DynManagedDriver, name: impl Into<String>) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        let name = name.into();
        let schema = decode_driver_schema(&inner.meta(pb::DriverMethod::Schema as u32)[..])
            .with_context(|| format!("decoding the schema of driver {name:?}"))?;
        Ok(Self {
            inner: Arc::new(inner),
            name,
            schema: Arc::new(schema),
        })
    }
}

/// A provider's `StateSchema` meta answer. Empty bytes are "no schema"; any
/// encoded `Schema` (even fields-empty) is one.
fn decode_state_schema(bytes: &[u8]) -> anyhow::Result<Option<StateSchema>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    convert::state_schema_from_pb(plugin_abi::decode::<pb::Schema>(bytes)?).map(Some)
}

/// A driver's `Schema` meta answer.
fn decode_driver_schema(bytes: &[u8]) -> anyhow::Result<DriverSchema> {
    convert::driver_schema_from_pb(plugin_abi::decode::<pb::Schema>(bytes)?)
}

#[async_trait]
impl ManagedDriver for StableRemoteManagedDriver {
    fn config(&self, _req: DriverConfigRequest) -> anyhow::Result<DriverConfigResponse> {
        Ok(DriverConfigResponse {
            name: self.name.clone(),
        })
    }

    fn schema(&self) -> DriverSchema {
        DriverSchema::clone(&self.schema)
    }

    async fn parse(
        &self,
        req: ParseRequest,
        ct: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let request_id = req.request_id.clone();
        let pb_req = pb::ParseRequest {
            request_id: Some(req.request_id),
            target_spec: convert::target_spec_to_pb(req.target_spec.as_ref()).into(),
            driver: Some(self.name.clone()),
            ..Default::default()
        }
        .encode_to_vec();
        let fut = self
            .inner
            .invoke(pb::DriverMethod::Parse as u32, sv(&pb_req));
        let bytes = await_with_cancel(ct, driver_cancel(&self.inner, &request_id), fut).await;
        match decode_unary(&bytes)? {
            Body::ParseResp(pr) => Ok(ParseResponse {
                target_def: convert::target_def_from_pb(required_msg(
                    pr.target_def,
                    "ParseResponse.target_def",
                )?)?,
            }),
            Body::Error(e) => anyhow::bail!("{}", error_text(*e)),
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
            request_id: Some(req.request_id),
            target_def: convert::target_def_to_pb(&req.target_def)?.into(),
            sandbox: convert::sandbox_to_pb(&req.sandbox).into(),
            driver: Some(self.name.clone()),
            ..Default::default()
        }
        .encode_to_vec();
        let fut = self
            .inner
            .invoke(pb::DriverMethod::ApplyTransitive as u32, sv(&pb_req));
        let bytes = await_with_cancel(ct, driver_cancel(&self.inner, &request_id), fut).await;
        match decode_unary(&bytes)? {
            Body::ApplyTransitiveResp(r) => Ok(ApplyTransitiveResponse {
                target_def: convert::target_def_from_pb(required_msg(
                    r.target_def,
                    "ApplyTransitiveResponse.target_def",
                )?)?,
            }),
            Body::Error(e) => anyhow::bail!("{}", error_text(*e)),
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
        let deferred = convert::deferred_to_pb(&req.request.deferred)?;
        let pmrr = pb::ManagedRunRequest {
            request_id: Some(req.request.request_id.clone()),
            target: convert::target_def_to_pb(req.request.target)?.into(),
            tree_root_path: Some(req.request.tree_root_path.to_string_lossy().into_owned()),
            hashin: Some(req.request.hashin.to_string()),
            sandbox_dir: Some(req.sandbox_dir.to_string_lossy().into_owned()),
            sandbox_ws_dir: Some(req.sandbox_ws_dir.to_string_lossy().into_owned()),
            sandbox_pkg_dir: Some(req.sandbox_pkg_dir.to_string_lossy().into_owned()),
            inputs: req.inputs.iter().map(managed_input_to_pb).collect(),
            shell: Some(shell),
            driver: Some(self.name.clone()),
            scratch: req
                .request
                .scratch
                .iter()
                .map(convert::scratch_mount_to_pb)
                .collect(),
            credentials: req
                .request
                .credentials
                .iter()
                .map(convert::credential_mount_to_pb)
                .collect(),
            deferred,
            ..Default::default()
        };
        // `run` is bidi: the request stream carries the run request (RunInFrame),
        // the response stream carries the result (RunOutFrame). `shell` rides pmrr.
        let in_frame = pb::RunInFrame {
            msg: Some(pb::run_in_frame::Msg::from(pmrr)),
            ..Default::default()
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
        match result {
            Ok(result) => result,
            Err(_canceled) => anyhow::bail!("run drain thread dropped"),
        }
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
        id: Some(0),
        body: Some(Body::from(pb::StreamItem {
            item: Some(item),
            ..Default::default()
        })),
        ..Default::default()
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
        // serde-JSON render plus a protobuf encode. Holding the state mutex across
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
        r#type: Some(ty.into()),
        origin_id: Some(mi.input.origin_id.clone()),
        source_addr: convert::addr_to_pb(&mi.input.source_addr).into(),
        filters: mi.input.filters.clone(),
        annotations: mi
            .input
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        unpack_root: Some(mi.unpack_root.to_string_lossy().into_owned()),
        list_path: mi
            .list_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        // Absent when the host could not read one (or the artifact has none),
        // rather than failing the run: most drivers never read it, and one that
        // does must refuse a missing hashout anyway.
        hashout: mi
            .input
            .artifact
            .content
            .hashout()
            .ok()
            .and_then(|h| convert::absent_if_empty(&h)),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(reported: Option<&str>) -> String {
        let path = std::path::Path::new("/p/libdrift.so");
        format!(
            "{:#}",
            check_plugin_abi(path, reported).expect_err("must be refused")
        )
    }

    /// The host's own version is in its lane. A break that bumps the minor
    /// without raising `MIN_PLUGIN_ABI` (or raises MIN past the host) fails
    /// here, rather than shipping a host that refuses its own plugins.
    #[test]
    fn the_host_abi_is_in_its_own_lane() {
        let host = semver::Version::parse(plugin_abi::ABI_SEMVER).expect("ABI_SEMVER");
        let req = accepted_plugin_abi().expect("MIN_PLUGIN_ABI");
        assert!(req.matches(&host), "ABI_SEMVER {host} does not match {req}");
        let min = semver::Version::parse(plugin_abi::MIN_PLUGIN_ABI).expect("MIN_PLUGIN_ABI");
        assert_eq!(min.patch, 0, "MIN_PLUGIN_ABI is the break's x.y.0");
    }

    /// Patches are additive: any patch in the host's minor loads, older or
    /// newer than the host.
    #[test]
    fn a_plugin_in_the_accepted_lane_loads() {
        let path = std::path::Path::new("/p/libok.so");
        let host = semver::Version::parse(plugin_abi::ABI_SEMVER).expect("ABI_SEMVER");
        let newer_patch = format!("{}.{}.{}", host.major, host.minor, host.patch + 7);
        for v in [
            plugin_abi::ABI_SEMVER,
            plugin_abi::MIN_PLUGIN_ABI,
            &newer_patch,
        ] {
            check_plugin_abi(path, Some(v)).unwrap_or_else(|e| panic!("{v} in lane: {e:#}"));
        }
    }

    /// A later minor is a break: refused, as newer than this heph.
    #[test]
    fn a_plugin_from_the_next_minor_is_refused() {
        let host = semver::Version::parse(plugin_abi::ABI_SEMVER).expect("ABI_SEMVER");
        let next = format!("{}.{}.0", host.major, host.minor + 1);
        let msg = refusal(Some(&next));
        assert!(
            msg.contains(&format!(
                "plugin {next}, newer than this heph, which is unsupported"
            )),
            "{msg}"
        );
        assert!(
            msg.contains(&format!("which loads ^{}", plugin_abi::MIN_PLUGIN_ABI)),
            "{msg}"
        );
    }

    #[test]
    fn a_plugin_without_a_version_is_refused_as_predating_the_check() {
        let msg = refusal(None);
        for want in [
            "plugin /p/libdrift.so was built against a different heph ABI",
            "it reports no ABI version, so it predates 0.17.0",
            &format!("host {}", plugin_abi::ABI_SEMVER),
            "rebuild it against this heph",
        ] {
            assert!(msg.contains(want), "missing {want:?}: {msg}");
        }
    }

    #[test]
    fn an_older_or_newer_plugin_is_refused_naming_both_versions() {
        let msg = refusal(Some("0.16.0"));
        for want in [
            "plugin /p/libdrift.so",
            "(plugin 0.16.0; host ",
            plugin_abi::ABI_SEMVER,
            "rebuild it against this heph",
        ] {
            assert!(msg.contains(want), "missing {want:?}: {msg}");
        }
        let msg = refusal(Some("1.0.0"));
        assert!(
            msg.contains("plugin 1.0.0, newer than this heph, which is unsupported"),
            "{msg}"
        );
    }

    /// Semver, not strings: "0.9.0" sorts after "0.17.0" as text, and a
    /// pre-release of the minimum is below it.
    #[test]
    fn versions_compare_as_semver() {
        assert!(refusal(Some("0.9.0")).contains("(plugin 0.9.0; host "));
        let pre = format!("{}-rc.1", plugin_abi::MIN_PLUGIN_ABI);
        assert!(refusal(Some(&pre)).contains(&format!("(plugin {pre}; host ")));
    }

    /// A schema is decoded once, at load, and one that does not decode fails
    /// the load instead of reading as "no schema" (every driver key unknown).
    #[test]
    fn an_undecodable_schema_is_an_error_not_a_default() {
        let string =
            || convert::param_type_to_pb(&hcore::htvalue::signature::ParamType::String).into();
        let unnamed_field = pb::Schema {
            fields: vec![pb::SchemaField {
                ty: string(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .encode_to_vec();
        for bytes in [&[0xff][..], &unnamed_field[..]] {
            decode_driver_schema(bytes).expect_err("driver schema");
            decode_state_schema(bytes).expect_err("state schema");
        }
        assert!(decode_state_schema(&[]).expect("empty").is_none());
        let named = pb::Schema {
            fields: vec![pb::SchemaField {
                name: Some("args".into()),
                ty: string(),
                ..Default::default()
            }],
            ..Default::default()
        }
        .encode_to_vec();
        assert_eq!(
            decode_driver_schema(&named).expect("decodes").fields.len(),
            1
        );
        assert!(decode_state_schema(&named).expect("decodes").is_some());
    }

    #[test]
    fn an_unparsable_version_is_refused() {
        for bad in ["", "0.17", "v0.17.0", "zero"] {
            let msg = refusal(Some(bad));
            assert!(
                msg.contains(&format!(
                    "reports ABI version {bad:?}, which is not a version"
                )),
                "{msg}"
            );
        }
    }

    /// A functions handle no test calls: D11 fails before any call.
    struct NeverCalled;

    impl crate::abi::StablePluginFunctions for NeverCalled {
        extern "C" fn invoke<'a>(
            &'a self,
            method: u32,
            _name: stabby::string::String,
            _req: SVec<u8>,
        ) -> stabby::future::DynFutureUnsync<'a, SVec<u8>> {
            dynify(stabby::boxed::Box::new(async move {
                panic!("method {method} called on a plugin whose load must have failed")
            }))
        }
    }

    fn handle() -> Option<DynPluginFunctions> {
        Some(dynify(stabby::boxed::Box::new(NeverCalled)))
    }

    fn named(name: &str, meta: &[u8]) -> NamedFunction {
        NamedFunction {
            name: name.into(),
            meta: SVec::from(meta),
        }
    }

    fn signature() -> pb::FnSignature {
        convert::fn_signature_to_pb(&hcore::htvalue::signature::FnSignature {
            positional: vec![],
            named: vec![],
            variadic: None,
            returns: hcore::htvalue::signature::ParamType::String,
        })
    }

    /// D11: metadata the host cannot decode fails the plugin's load, naming the
    /// plugin and the function — it used to be dropped, and the function then
    /// surfaced as "no function" in some BUILD file.
    #[test]
    fn undecodable_function_metadata_fails_load() {
        let err = decode_functions("gen", vec![named("rule", &[0xff, 0xff, 0xff])], handle())
            .err()
            .expect("undecodable metadata must fail the load");
        let msg = format!("{err:#}");
        assert!(msg.contains("plugin \"gen\""), "{msg}");
        assert!(msg.contains("gen.rule"), "{msg}");
    }

    /// D11: a function whose metadata carries no signature fails the load too;
    /// the host cannot validate a call against nothing.
    #[test]
    fn function_without_signature_fails_load() {
        let meta = pb::FunctionDef {
            name: Some("rule".into()),
            ..Default::default()
        }
        .encode_to_vec();
        let err = decode_functions("gen", vec![named("rule", &meta)], handle())
            .err()
            .expect("a missing signature must fail the load");
        let msg = format!("{err:#}");
        assert!(msg.contains("gen.rule"), "{msg}");
        assert!(msg.contains("missing field FunctionDef.signature"), "{msg}");

        // Functions without a handle to call them through fail as well.
        let meta = pb::FunctionDef {
            name: Some("rule".into()),
            signature: signature().into(),
            ..Default::default()
        }
        .encode_to_vec();
        let err = decode_functions("gen", vec![named("rule", &meta)], None)
            .err()
            .expect("functions without a handle must fail the load");
        assert!(format!("{err:#}").contains("no function handle"), "{err:#}");

        // The well-formed case decodes, signature and all.
        let defs = decode_functions("gen", vec![named("rule", &meta)], handle()).expect("decodes");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].signature.render("rule"), "rule() -> string");
    }

    /// A run input's hashout crosses to the guest. Without it a guest driver
    /// reading `content.hashout()` sees `""`, which in-process tests never do.
    #[test]
    fn a_managed_input_sends_its_hashout() {
        struct Hashed;
        impl hcore::hartifactcontent::Content for Hashed {
            fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
                anyhow::bail!("unused")
            }
            fn walk(
                &self,
            ) -> anyhow::Result<
                Box<dyn Iterator<Item = anyhow::Result<hcore::hartifactcontent::WalkEntry>> + '_>,
            > {
                anyhow::bail!("unused")
            }
            fn hashout(&self) -> anyhow::Result<String> {
                Ok("abc123".to_string())
            }
        }
        let mi = ManagedRunInput {
            input: hplugin::driver::RunInput {
                artifact: inputartifact::InputArtifact {
                    r#type: inputartifact::Type::Dep,
                    origin_id: "image".to_string(),
                    content: Arc::new(Hashed),
                },
                origin_id: "image".to_string(),
                source_addr: hmodel::htaddr::parse_addr("//app:img").expect("addr"),
                filters: vec![],
                annotations: Default::default(),
            },
            list_path: None,
            unpack_root: std::path::PathBuf::from("/unused"),
        };
        let wire = managed_input_to_pb(&mi);
        assert_eq!(wire.hashout.as_deref(), Some("abc123"));
        // No file list (a Support input's shape) crosses as absent, not `""`,
        // which the guest refuses as malformed.
        assert_eq!(wire.list_path, None);
    }

    fn addr() -> pb::Addr {
        pb::Addr {
            package: Some("p".into()),
            name: Some("t".into()),
            ..Default::default()
        }
    }

    /// A plugin built before ABI 0.13 sends no `facts`: every fact must decode
    /// as unknown, never as "no labels", or every label selection would
    /// silently skip its targets. One driver name decodes to one shared `Arc`
    /// across a `list` call.
    #[test]
    fn old_plugin_listing_decodes_unknown_and_drivers_are_interned() {
        let mut decode = list_item_decoder();
        let old = pb::ListResponse {
            addr: addr().into(),
            ..Default::default()
        };
        let item = decode(&old.encode_to_vec()).expect("decoding a ListResponse");
        assert!(item.facts.is_unknown());

        let with = |driver: &str| pb::ListResponse {
            addr: addr().into(),
            facts: pb::ListedFacts {
                labels: pb::StringSet::default().into(),
                driver: Some(driver.to_string()),
                has_codegen: Some(false),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let a = decode(&with("bash").encode_to_vec()).expect("decode");
        let b = decode(&with("bash").encode_to_vec()).expect("decode");
        assert_eq!(a.facts.labels(), Some(&[][..]));
        assert_eq!(a.facts.has_codegen(), Some(false));
        let (Some(da), Some(db)) = (a.facts.driver(), b.facts.driver()) else {
            panic!("driver should be known");
        };
        // Both borrow from the same allocation: one `Arc` per driver name.
        assert!(
            std::ptr::eq(da.as_ptr(), db.as_ptr()),
            "one Arc per driver name per list call"
        );
    }

    /// C33: a malformed nested `ListedFacts` fails the item, so the `list`.
    #[test]
    fn malformed_listed_facts_fails_the_item() {
        // Field 1 (`addr`), then field 4 (`facts`) two bytes long:
        // `has_codegen`'s tag, then a varint running past the message's end.
        use buffa::encoding::{Tag, WireType};
        use buffa::types::put_len_delimited_header;
        let addr = addr().encode_to_vec();
        let mut wire = Vec::new();
        put_len_delimited_header(1, addr.len() as u64, &mut wire);
        wire.extend_from_slice(&addr);
        let mut facts = Vec::new();
        Tag::new(3, WireType::Varint).encode(&mut facts);
        facts.push(0xFF); // a continuation bit with nothing after it
        put_len_delimited_header(4, facts.len() as u64, &mut wire);
        wire.extend_from_slice(&facts);
        assert!(list_item_decoder()(&wire).is_err());
    }

    /// A listing with no `addr` lists no target: an error, not the root
    /// package's empty-named target.
    #[test]
    fn a_listing_without_an_addr_fails_the_item() {
        let err = list_item_decoder()(&pb::ListResponse::default().encode_to_vec())
            .err()
            .expect("no addr");
        assert!(
            format!("{err:#}").contains("missing field ListResponse.addr"),
            "{err:#}"
        );
    }
}
