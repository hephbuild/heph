//! The GitHub Actions hook as a loadable cdylib behind the stable ABI.
//!
//! Exports a single stabby `create` entry that constructs the [`GhaHook`] and
//! hands it back as an ABI-stable handle. A hook-only plugin: it carries no
//! provider (a no-op placeholder the host drops) and no drivers.

use std::sync::Arc;

use hplugin::hook::Hook;
use hplugin_gha::GhaHook;
use plugin_sdk::stabby::abi::{
    DynLogSink, DynRunnerHost, DynSupervisor, NamedHook, PluginComponents,
};
use plugin_sdk::stabby::{
    create_config_from_bytes, install_log_sink, install_runner_host, install_supervisor,
    make_dyn_hook, options_from_pb_map,
};

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility. `cfg` is prost-encoded
/// `pb::CreateConfig` bytes.
#[stabby::export]
pub extern "C" fn heph_plugin_create(cfg: stabby::vec::Vec<u8>) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("heph-gha-plugin: plugin construction failed: {e:#}");
            std::process::abort();
        }
    }
}

/// Stable ABI log-sink entry: the host calls this right after `create` to hand the
/// plugin a sink for its `tracing` events. Without it, this cdylib's
/// statically-linked `tracing` has no subscriber and the hook's logs vanish.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// Stable ABI supervisor entry: the host hands the plugin its process-supervisor
/// client, so any child this plugin spawns is tracked by the host's sidecar rather
/// than by this cdylib's own (uninitialised) copy of the `proc` tracker.
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
    let options = options_from_pb_map(cfg.options);
    let hook: Arc<dyn Hook> = Arc::new(GhaHook::from_options(&options)?);

    let mut hooks = stabby::vec::Vec::new();
    hooks.push(NamedHook {
        name: "gha".into(),
        hook: make_dyn_hook(hook),
    });

    Ok(PluginComponents {
        // Hook-only: no provider, no drivers.
        provider_name: String::new().into(),
        provider: stabby::option::Option::None(),
        drivers: stabby::vec::Vec::new(),
        hooks,
        runners: stabby::vec::Vec::new(),
        meta: stabby::vec::Vec::new(),
    })
}
