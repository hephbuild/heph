//! The `gh` (GitHub) plugin as a loadable cdylib behind the stable ABI.
//!
//! Exports a single stabby `create` entry that wires the parts of
//! `plugin-gh` into ABI-stable handles: the GitHub Actions reporting
//! [`GhaHook`] (when the options say `hook: {enable: true}`), the
//! `release_asset` function and the `asset_url` driver. No provider. The
//! extract `release_asset` declares is the host's builtin `archive.extract`,
//! named rather than called: `plugin-gh` does not request `archive`'s codec
//! feature (cargo may unify it on in a build alongside `heph`), and the linker
//! drops the codecs from this cdylib since nothing in it calls them.
//!
//! A bad `options:` entry does not abort the host: the log sink is not
//! installed at creation, so an abort would be a bare `SIGABRT` with no
//! message. An option that does not decode is logged by a [`ConfigErrorHook`]
//! once the sink is up. `release_asset` reads no option, so it is exported
//! either way; a bad `server_url` or `api_url` argument fails that call.

use std::sync::Arc;

use hplugin::hook::Hook;
use hplugin_gh::{ConfigErrorHook, GhOptions, GhaHook, asset_url, release};
use plugin_sdk::stabby::abi::{DynFunctionRegistry, NamedHook, PluginComponents};
use plugin_sdk::stabby::{
    create_config_from_bytes, make_dyn_hook, make_plugin_functions, named_managed_driver,
};

plugin_sdk::export_host_entries!();

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility. `cfg` is the encoded
/// create config. The plugin's function calls no other plugin's, so the
/// engine's function registry goes unused.
#[stabby::export]
pub extern "C" fn heph_plugin_create(
    cfg: stabby::vec::Vec<u8>,
    _functions: DynFunctionRegistry,
) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            // Only an undecodable create config or an ABI wrapping failure gets
            // here — a host/plugin mismatch, not a user's option.
            tracing::error!("heph-gh-plugin: plugin construction failed: {e:#}");
            std::process::abort();
        }
    }
}

fn build(cfg: &[u8]) -> anyhow::Result<PluginComponents> {
    let cfg = create_config_from_bytes(cfg)?;
    let hook: Option<Arc<dyn Hook>> = match GhOptions::decode(&cfg.options) {
        Ok(opts) => opts
            .hook
            .enable
            .then(|| Arc::new(GhaHook::new(opts.hook)) as Arc<dyn Hook>),
        Err(e) => Some(Arc::new(ConfigErrorHook::new(&e))),
    };

    let mut hooks = stabby::vec::Vec::new();
    if let Some(hook) = hook {
        hooks.push(NamedHook {
            name: "gha".into(),
            hook: make_dyn_hook(hook),
        });
    }

    // `heph.<name>.release_asset`, declaring drivers under this plugin's
    // manifest name.
    let defs = release::functions(&cfg.name);
    let (functions, function_handle) = make_plugin_functions(&cfg.name, defs)?;

    let mut drivers = stabby::vec::Vec::new();
    drivers.push(named_managed_driver(&cfg.name, asset_url::Driver)?);

    Ok(PluginComponents {
        hooks,
        drivers,
        functions,
        function_handle: function_handle.into(),
        ..PluginComponents::empty()
    })
}
