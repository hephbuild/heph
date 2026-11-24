//! The GitHub Actions hook as a loadable cdylib behind the stable ABI.
//!
//! Exports a single stabby `create` entry that constructs the [`GhaHook`] and
//! hands it back as an ABI-stable handle. A hook-only plugin: it carries no
//! provider, no drivers and no functions.

use std::sync::Arc;

use hplugin::hook::Hook;
use hplugin_gha::GhaHook;
use plugin_sdk::stabby::abi::{DynFunctionRegistry, NamedHook, PluginComponents};
use plugin_sdk::stabby::{create_config_from_bytes, make_dyn_hook};

plugin_sdk::export_host_entries!();

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility. `cfg` is the encoded
/// create config. A hook has no functions, so the engine's function
/// registry goes unused.
#[stabby::export]
pub extern "C" fn heph_plugin_create(
    cfg: stabby::vec::Vec<u8>,
    _functions: DynFunctionRegistry,
) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("heph-gha-plugin: plugin construction failed: {e:#}");
            std::process::abort();
        }
    }
}

fn build(cfg: &[u8]) -> anyhow::Result<PluginComponents> {
    let options = create_config_from_bytes(cfg)?.options;
    let hook: Arc<dyn Hook> = Arc::new(GhaHook::from_options(&options)?);

    let mut hooks = stabby::vec::Vec::new();
    hooks.push(NamedHook {
        name: "gha".into(),
        hook: make_dyn_hook(hook),
    });

    // Hook-only: no provider, no drivers, no functions.
    Ok(PluginComponents {
        hooks,
        ..PluginComponents::empty()
    })
}
