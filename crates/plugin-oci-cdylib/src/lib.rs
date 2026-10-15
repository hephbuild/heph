//! The OCI plugin as a loadable cdylib behind the stable ABI.
//!
//! Exports a single stabby `create` entry that constructs the plugin's managed
//! drivers and hands them back as ABI-stable handles. The host loads this with
//! `hplugin_stabby::load_stable::load`, which verifies ABI compatibility via
//! stabby's type reports before use.
//!
//! Images themselves are declared by whatever provider the workspace already
//! uses (`BUILD` files). The one thing this plugin's own provider contributes is
//! `//@heph/oci:platform` — the builder-platform probe a `docker_build` without
//! explicit `platforms` depends on, so that platform reaches the cache key as an
//! input hash rather than as a parse-time side effect. No hooks.
//!
//! Two things are read from [`CreateConfig`]: the plugin's `name` (its
//! manifest's), under which the platform probe target names its driver, and the
//! resolved heph `home`, which the `oci.runner` driver mounts into its
//! container. Otherwise `docker_build` and the platform probe take the `docker`
//! binary from `PATH`, the same host capability the in-process build used, and
//! `oci.push` / `oci.pull` need no host binary at all.

use hplugin_oci::pluginoci;
use plugin_sdk::stabby::abi::{
    DynFunctionRegistry, DynLogSink, DynRunnerHost, DynSupervisor, NamedRunner, PluginComponents,
};
use plugin_sdk::stabby::{
    create_config_from_bytes, install_log_sink, install_runner_host, install_supervisor,
    make_dyn_provider, make_dyn_runner, named_managed_driver,
};
use std::sync::Arc;

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility. `cfg` is the encoded
/// create config; this plugin reads its `name` and the resolved heph `home` out
/// of it. The oci plugin exports no functions, so the engine's function
/// registry goes unused.
///
/// Nothing here aborts: the log sink is not installed yet, so an abort would
/// kill the host with a bare `SIGABRT` and no message. A `home` that is not
/// usable fails only `oci.runner`, the one driver that needs it — at run, with
/// the reason.
#[stabby::export]
pub extern "C" fn heph_plugin_create(
    cfg: stabby::vec::Vec<u8>,
    _functions: DynFunctionRegistry,
) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            // The create entry cannot fail across the ABI, and a panic here is a
            // non-unwinding abort. Hand back an empty bundle: the first `oci.*`
            // target then fails with "unknown driver". (The log sink is not
            // installed yet, so this event only reaches a test subscriber.)
            tracing::error!(error = %format!("{e:#}"), "oci plugin: create failed");
            PluginComponents {
                provider: stabby::option::Option::None(),
                drivers: stabby::vec::Vec::new(),
                functions: stabby::vec::Vec::new(),
                function_handle: stabby::option::Option::None(),
                hooks: stabby::vec::Vec::new(),
                runners: stabby::vec::Vec::new(),
                meta: stabby::vec::Vec::new(),
            }
        }
    }
}

/// Stable ABI log-sink entry: the host calls this right after `create` to hand
/// the plugin a sink for its `tracing` events. Without it, this cdylib's
/// statically-linked `tracing` has no subscriber and every diagnostic these
/// drivers emit — an unpinned `oci_pull` tag, a log file that would not open —
/// vanishes silently.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// ABI version entry: the host reads it before anything else and refuses this
/// plugin if it was built for an ABI the host does not load. A plain C symbol,
/// so it reads the same whatever stabby layout either side has.
#[unsafe(no_mangle)]
pub extern "C" fn heph_plugin_abi_semver() -> *const std::ffi::c_char {
    plugin_sdk::stabby::abi_semver()
}

/// Stable ABI log-filter entry: the host's filter as protobuf `LogFilter` bytes,
/// so events it would discard — reqwest/hyper's chatter under every pull — are
/// dropped here instead of forwarded.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_filter(filter: stabby::vec::Vec<u8>) {
    plugin_sdk::stabby::install_log_filter(&filter);
}

/// Stable ABI supervisor entry: the host hands the plugin its process-supervisor
/// client. This cdylib links its own `proc`, whose tracker the host's startup
/// `init` never reached — without this, every `docker buildx` this
/// plugin spawns goes unregistered with the sidecar and is orphaned on a hard
/// kill of the host. A detached `buildx` is exactly the child that must not
/// survive: it keeps writing into a sandbox the cleaner is deleting.
#[stabby::export]
pub extern "C" fn heph_plugin_set_supervisor(sup: DynSupervisor) {
    install_supervisor(sup);
}

/// Stable ABI exec-runner entry: the host hands the plugin a handle to its
/// runner registry. This cdylib links its own `execrunner`, whose registry the
/// engine's `install_exec_runner_host` never reached — without this, a target
/// here that names a `runner` fails with "no runner host is installed in this
/// component" rather than running in the environment it asked for.
#[stabby::export]
pub extern "C" fn heph_plugin_set_runner_host(host: DynRunnerHost) {
    install_runner_host(host);
}

fn build(cfg: &[u8]) -> anyhow::Result<PluginComponents> {
    let cfg = create_config_from_bytes(cfg)?;
    // The plugin's name, from its manifest.
    let plugin = cfg.name;
    // The host's resolved heph home, or why it is not usable.
    let home = pluginoci::runner::home_from_host(&cfg.home.to_string_lossy());

    // Each driver is exported under the local name its `config()` reports.
    let mut drivers = stabby::vec::Vec::new();
    for driver in [
        // Assembles target outputs into an image. No daemon, no execution.
        named_managed_driver(pluginoci::image::Driver::new())?,
        named_managed_driver(pluginoci::layer::Driver::new())?,
        // Groups per-platform images into one multi-platform image.
        named_managed_driver(pluginoci::index::Driver::new())?,
        // Builds a Dockerfile + context into a cacheable image archive.
        named_managed_driver(pluginoci::docker_build::Driver::new())?,
        // Pulls a base image into a cacheable archive (or OCI layout).
        named_managed_driver(pluginoci::pull::Driver::new())?,
        // Actions — uncached, they mutate a registry or the local daemon.
        named_managed_driver(pluginoci::push::Driver::new())?,
        named_managed_driver(pluginoci::load::Driver::new())?,
        // Asks buildx which platform it would build by default. Depended on,
        // never named by a user.
        named_managed_driver(pluginoci::platform::Driver::new())?,
        // Describes a container other targets run inside. Emits a `runner.json`
        // naming the builtin `session` runner: the container is held open for
        // the build and targets run in it over the agent protocol, so this
        // plugin needs no runner implementation of its own.
        named_managed_driver(pluginoci::runner::Driver::from_host_home(home))?,
    ] {
        drivers.push(driver);
    }

    let provider: Arc<dyn hplugin::provider::Provider> =
        Arc::new(pluginoci::platform::Provider::new(&plugin));

    // The runner this plugin *implements*, as opposed to the `oci_runner`
    // driver above, which only writes the `runner.json` naming it. Registered
    // by the host beside `local`/`wrap`/`session`: a container's lifecycle is
    // one no builtin expresses, since a target's cwd is per-exec and a session
    // launch argv is fixed when the runner target is built.
    let mut runners = stabby::vec::Vec::new();
    runners.push(NamedRunner {
        name: pluginoci::exec_runner::RUNNER_NAME.into(),
        runner: make_dyn_runner(Arc::new(pluginoci::exec_runner::OciRunner::new())),
    });

    Ok(PluginComponents {
        provider: stabby::option::Option::Some(make_dyn_provider(provider)),
        drivers,
        functions: stabby::vec::Vec::new(),
        function_handle: stabby::option::Option::None(),
        hooks: stabby::vec::Vec::new(),
        runners,
        meta: stabby::vec::Vec::new(),
    })
}
