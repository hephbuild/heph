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

use hplugin_devenv::plugindevenv;
use plugin_sdk::stabby::abi::{DynFunctionRegistry, PluginComponents};
use plugin_sdk::stabby::{create_config_from_bytes, named_managed_driver};

/// Stable ABI create entry. `cfg` is the encoded create config; the `bin:`
/// option is read out of it so a workspace can pin which `devenv` it
/// resolves against rather than taking whatever is on `PATH`. The plugin has no
/// functions, so the engine's function registry goes unused.
#[stabby::export]
pub extern "C" fn heph_plugin_create(
    cfg: stabby::vec::Vec<u8>,
    _functions: DynFunctionRegistry,
) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            // The create entry cannot fail across the ABI, and a panic here is a
            // non-unwinding abort. Report and hand back an empty bundle: the
            // workspace then fails on the first `devenv_runner` target with
            // "unknown driver", which names the problem at a place the user can
            // act on.
            tracing::error!(error = %format!("{e:#}"), "devenv plugin: bad configuration");
            PluginComponents::empty()
        }
    }
}

// The supervisor entry matters here: the `devenv shell` this plugin spawns to
// capture an environment would otherwise be orphaned on a hard kill of the
// host, and a stray nix evaluation is exactly the child that must not survive.
plugin_sdk::export_host_entries!();

fn build(cfg: &[u8]) -> anyhow::Result<PluginComponents> {
    let cfg = create_config_from_bytes(cfg)?;
    let mut drivers = stabby::vec::Vec::new();
    // `home` is where the driver roots the store paths a runner names.
    drivers.push(named_managed_driver(plugindevenv::Driver::from_options(
        &cfg.options,
        &cfg.home,
    )?)?);

    Ok(PluginComponents {
        drivers,
        ..PluginComponents::empty()
    })
}
