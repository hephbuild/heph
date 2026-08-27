//! The devenv plugin as a loadable cdylib behind the stable ABI.
//!
//! One driver, `devenv_runner`, and nothing else: no provider (devenv targets
//! are declared in BUILD files like anything else) and no exec runner.
//!
//! The absent exec runner is the point. Agent mode — holding one `devenv shell`
//! open for a whole build and running targets inside it — is the host's builtin
//! `session` runner, which this plugin selects from the `runner.json` it emits.
//! A plugin only implements a runner when it has a lifecycle of its own to
//! manage, and devenv does not: a held shell is exactly what `session` already
//! is.

use hdriver_support::driver_managed::ManagedDriver;
use hplugin_devenv::plugindevenv;
use plugin_sdk::stabby::abi::{
    DynLogSink, DynRunnerHost, DynSupervisor, NamedDriver, PluginComponents,
};
use plugin_sdk::stabby::{
    create_config_from_bytes, install_log_sink, install_runner_host, install_supervisor,
    make_dyn_managed_driver, options_from_pb_map,
};
use std::sync::Arc;

/// Stable ABI create entry. `cfg` is prost-encoded `pb::CreateConfig`; the
/// `bin:` option is read out of it so a workspace can pin which `devenv` it
/// resolves against rather than taking whatever is on `PATH`.
#[stabby::export]
pub extern "C" fn heph_plugin_create(cfg: stabby::vec::Vec<u8>) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            // The create entry cannot fail across the ABI, and a panic here is a
            // non-unwinding abort. Report and hand back an empty bundle: the
            // workspace then fails on the first `devenv_runner` target with
            // "unknown driver", which names the problem at a place the user can
            // act on.
            tracing::error!(error = %format!("{e:#}"), "devenv plugin: bad configuration");
            PluginComponents {
                provider_name: "".into(),
                provider: stabby::option::Option::None(),
                drivers: stabby::vec::Vec::new(),
                hooks: stabby::vec::Vec::new(),
                meta: stabby::vec::Vec::new(),
            }
        }
    }
}

/// Stable ABI log-sink entry. Without it this cdylib's statically-linked
/// `tracing` has no subscriber and every diagnostic it emits vanishes.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// Stable ABI supervisor entry. This cdylib links its own `proc`, so without it
/// the `devenv shell` it spawns to capture an environment is unregistered with
/// the sidecar and orphaned on a hard kill of the host — and a stray nix
/// evaluation is exactly the child that must not survive.
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
    let opts = options_from_pb_map(cfg.options);
    let driver: Arc<dyn ManagedDriver> = Arc::new(plugindevenv::Driver::from_options(&opts)?);

    let mut drivers = stabby::vec::Vec::new();
    drivers.push(NamedDriver {
        name: plugindevenv::DRIVER_NAME.into(),
        driver: make_dyn_managed_driver(driver),
    });

    Ok(PluginComponents {
        provider_name: "".into(),
        provider: stabby::option::Option::None(),
        drivers,
        hooks: stabby::vec::Vec::new(),
        meta: stabby::vec::Vec::new(),
    })
}
