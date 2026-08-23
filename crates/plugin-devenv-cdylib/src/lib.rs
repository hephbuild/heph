//! The `devenv` driver as a loadable cdylib behind the stable ABI.
//!
//! Two components, both named `devenv`: a **driver** that builds the environment
//! artifact, and an **exec runner** that serves sessions from it.
//!
//! They are separate components rather than one driver answering extra methods,
//! because a runner is not a driver — it has no schema, parses nothing, and
//! builds nothing. Being the runner is what makes this plugin the party that
//! starts the process: `prepare_spec` is per target, so one `devenv shell` is
//! opened once *inside this plugin* and every target's exec is routed into it.

use std::path::PathBuf;
use std::sync::Arc;

use hdriver_support::driver_managed::ManagedDriver;
use plugin_sdk::stabby::abi::{
    DynLogSink, DynSupervisor, NamedDriver, NamedExecRunner, PluginComponents,
};
use plugin_sdk::stabby::{
    create_config_from_bytes, install_log_sink, install_supervisor, make_dyn_exec_runner,
    make_dyn_managed_driver,
};

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility. `cfg` is prost-encoded
/// `pb::CreateConfig` bytes.
#[stabby::export]
pub extern "C" fn heph_plugin_create(cfg: stabby::vec::Vec<u8>) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("heph-devenv-plugin: plugin construction failed: {e:#}");
            std::process::abort();
        }
    }
}

/// Stable ABI log-sink entry: the host calls this right after `create` to hand the
/// plugin a sink for its `tracing` events. Without it, this cdylib's
/// statically-linked `tracing` has no subscriber and the driver's logs vanish —
/// including the "dropped non-/nix/store PATH entries" line, which is the one a
/// user needs when a tool goes missing.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// Stable ABI supervisor entry: the host hands the plugin its process-supervisor
/// client, so the `devenv print-dev-env` child this driver spawns is tracked by
/// the host's sidecar rather than by this cdylib's own (uninitialised) copy of
/// the `proc` tracker.
#[stabby::export]
pub extern "C" fn heph_plugin_set_supervisor(sup: DynSupervisor) {
    install_supervisor(sup);
}

fn build(cfg: &[u8]) -> anyhow::Result<PluginComponents> {
    let cfg = create_config_from_bytes(cfg)?;
    // The workspace root, from the engine — never discovered. `devenv` must run
    // against the real tree because `devenv.nix` lives there, and a plugin is
    // handed its locations rather than guessing them.
    let root = PathBuf::from(cfg.root);

    // What `mode = "session"` needs, assembled here rather than discovered:
    // `current_exe` in a cdylib is the *host* binary, which is exactly the agent
    // and per-target client this needs; `home` comes from the engine.
    let home = PathBuf::from(cfg.home);
    let session =
        std::env::current_exe()
            .ok()
            .map(|heph_bin| hplugin_devenv::plugindevenv::SessionSupport {
                heph_bin,
                socket_dir: home.join("exec-agents"),
                tree_root: root.clone(),
            });
    let driver: Arc<dyn ManagedDriver> = Arc::new(hplugin_devenv::plugindevenv::Driver::new(root));
    let runner: Arc<dyn plugin_sdk::runner::ExecRunner> =
        Arc::new(hplugin_devenv::plugindevenv::Runner::new(session));
    let mut drivers = stabby::vec::Vec::new();
    drivers.push(NamedDriver {
        name: hplugin_devenv::plugindevenv::NAME.into(),
        driver: make_dyn_managed_driver(driver),
    });

    let mut runners = stabby::vec::Vec::new();
    runners.push(NamedExecRunner {
        name: hplugin_devenv::plugindevenv::NAME.into(),
        runner: make_dyn_exec_runner(runner),
    });

    Ok(PluginComponents {
        // No provider and no hooks — a driver and a runner.
        provider_name: String::new().into(),
        provider: stabby::option::Option::None(),
        drivers,
        hooks: stabby::vec::Vec::new(),
        runners,
        meta: stabby::vec::Vec::new(),
    })
}
