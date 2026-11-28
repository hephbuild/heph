//! The go plugin as a loadable cdylib behind the stable ABI.
//!
//! Exports a single stabby `create` entry that constructs the go provider + its
//! managed drivers and hands them back as ABI-stable handles. The host loads this
//! with `hplugin_stabby::load_stable::load`, which verifies ABI compatibility via
//! stabby's type reports before use. Calls then run in-process at native speed —
//! no serialization on the hot path, no IPC (see ai-docs/PERFORMANCE.md).
//!
//! Plugin-specific settings are read from the environment; only the workspace
//! root crosses in [`plugin_sdk::stabby::CreateConfig`].

use hplugin_go::plugingo::{self, Provider};
use plugin_sdk::stabby::abi::{DynFunctionRegistry, PluginComponents};
use plugin_sdk::stabby::{
    create_config_from_bytes, make_dyn_plugin_provider, make_plugin_functions,
    named_boxed_managed_driver,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility. `cfg` is the encoded
/// create config (protobuf), so config fields are additive across versions.
///
/// `_functions` is the loading engine's function registry: the go functions
/// call no other plugin's, so it goes unused.
#[stabby::export]
pub extern "C" fn heph_plugin_create(
    cfg: stabby::vec::Vec<u8>,
    _functions: DynFunctionRegistry,
) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            // No safe way to surface an error through the stable bundle (it must
            // carry a valid provider handle), and unwinding across the FFI
            // boundary is UB — so fail loudly and abort.
            tracing::error!("heph-plugin-go: plugin construction failed: {e:#}");
            std::process::abort();
        }
    }
}

// The supervisor entry tracks every `go` compile this plugin spawns, which would
// otherwise be orphaned on a hard kill of the host.
plugin_sdk::export_host_entries!();

fn build(cfg: &[u8]) -> anyhow::Result<PluginComponents> {
    let cfg = create_config_from_bytes(cfg)?;
    let root = cfg.root;
    let home = cfg.home;
    // Tunables come from the plugin's `options:` map (config yaml) — read the
    // same way an in-process plugin does.
    let mut options = cfg.options;
    // The walker db lives in the engine's home dir (e.g. `.heph`), not the repo
    // root — `home` comes from the engine, never hardcoded. It's this cdylib's own
    // option — consume it so it's kept out of the provider's map, whose
    // `from_options` rejects unknown keys.
    let walk_db = hplugin::config::decode_opt::<PathBuf>(&options, "go", "walk_db")?
        .unwrap_or_else(|| home.join("heph-plugin-go-fswalk.db"));
    options.remove("walk_db");
    // The environment every Go *tool* runs in — `go list`, `go tool compile`,
    // `heph-govet`, `gofmt`. A target address producing a `runner.json`, or the
    // literal "local" (the default) to spawn on the host. Like `walk_db` this is
    // consumed here rather than declared to the provider, whose `from_options`
    // rejects unknown keys — it configures the drivers, not package discovery.
    //
    // Deliberately NOT the runner for generated *test* targets: a test is an
    // exec/bash target, and it takes its runner from `provider_state(provider =
    // "go", test = {"runner": ...})`, falling back to the exec/bash driver's own
    // `runner:` option. Building the toolchain somewhere and running the tests
    // there are separate decisions — a build wants the compiler's environment, a
    // test often wants the runtime's.
    let go_runner = hplugin_go::plugingo::runner::read_runner_option(&options)?;

    let walker = Arc::new(hwalk::CachedWalker::open(&walk_db));
    let provider = Provider::from_options(
        root,
        &[],
        &[],
        &options,
        walker,
        plugin_sdk::stabby::cdylib_runtime_handle(),
    )?;
    // `heph.go.*`: built from the same options as the provider, exported
    // beside it — functions are the plugin's, not the provider's.
    let (functions, function_handle) = make_plugin_functions(&cfg.name, provider.functions())?;
    let provider: Arc<dyn hplugin::provider::Provider> = Arc::new(provider);

    let mut drivers = stabby::vec::Vec::new();
    // The shared golist GOCACHE is no longer this plugin's to place: it is a
    // `scratch` target the provider declares, and the host resolves, locks and
    // mounts it. So there is no writable root to hand in here — which is the
    // point, since the plugin also stops owning the eviction, the locking and
    // the visibility it never had.
    for driver in plugingo::managed_drivers(go_runner) {
        drivers.push(named_boxed_managed_driver(&cfg.name, driver)?);
    }

    Ok(PluginComponents {
        provider: stabby::option::Option::Some(make_dyn_plugin_provider(&cfg.name, provider)),
        drivers,
        functions,
        function_handle: function_handle.into(),
        // No hooks, no runners, no return-side metadata.
        ..PluginComponents::empty()
    })
}
