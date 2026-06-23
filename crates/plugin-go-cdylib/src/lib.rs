//! The go plugin as a loadable cdylib behind the stable ABI.
//!
//! Exports a single stabby `create` entry that constructs the go provider + its
//! managed drivers and hands them back as ABI-stable handles. The host loads this
//! with `hplugin_stabby::load_stable::load`, which verifies ABI compatibility via
//! stabby's type reports before use. Calls then run in-process at native speed —
//! no serialization on the hot path, no IPC (see ai-docs/PERFORMANCE.md).
//!
//! Plugin-specific settings are read from the environment; only the workspace
//! root crosses in [`CreateConfig`].

use hdriver_support::driver_managed::ManagedDriver;
use hplugin_go::plugingo::{
    GoCompileDriver, GoGolistDriver, GoTestmainDriver, GoToolchainDriver, Provider,
};
use plugin_sdk::stabby::abi::{NamedDriver, PluginComponents};
use plugin_sdk::stabby::{
    create_config_from_bytes, make_dyn_managed_driver, make_dyn_provider, options_from_pb_map,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility. `cfg` is prost-encoded
/// `pb::CreateConfig` bytes, so config fields are additive across versions.
#[stabby::export]
pub extern "C" fn heph_plugin_create(cfg: stabby::vec::Vec<u8>) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            // No safe way to surface an error through the stable bundle (it must
            // carry a valid provider handle), and unwinding across the FFI
            // boundary is UB — so fail loudly and abort.
            eprintln!("heph-plugin-go: plugin construction failed: {e:#}");
            std::process::abort();
        }
    }
}

fn build(cfg: &[u8]) -> anyhow::Result<PluginComponents> {
    let cfg = create_config_from_bytes(cfg)?;
    let root = PathBuf::from(cfg.root);
    let home = PathBuf::from(cfg.home);

    // Tunables come from the plugin's `options:` map (config yaml), carried as
    // structured CreateConfig data — read the same way an in-process plugin does.
    let mut options = options_from_pb_map(cfg.options);
    // The walker db lives in the engine's home dir (e.g. `.heph3`), not the repo
    // root — `home` comes from the engine, never hardcoded. It's this cdylib's own
    // option — consume it so it's kept out of the provider's map, whose
    // `from_options` rejects unknown keys.
    let walk_db = hplugin::config::decode_opt::<PathBuf>(&options, "go", "walk_db")?
        .unwrap_or_else(|| home.join("heph-plugin-go-fswalk.db"));
    options.remove("walk_db");

    let walker = Arc::new(hwalk::CachedWalker::open(&walk_db));
    let provider: Arc<dyn hplugin::provider::Provider> =
        Arc::new(Provider::from_options(root, &[], &[], &options, walker)?);

    let mut drivers = stabby::vec::Vec::new();
    let golist: Arc<dyn ManagedDriver> = Arc::new(GoGolistDriver::new());
    drivers.push(NamedDriver {
        name: "go_golist".into(),
        driver: make_dyn_managed_driver(golist),
    });
    // Hermetic Go toolchain: downloads + extracts the pinned SDK that backs
    // every Go build/list/test target.
    let toolchain: Arc<dyn ManagedDriver> = Arc::new(GoToolchainDriver);
    drivers.push(NamedDriver {
        name: "go_toolchain".into(),
        driver: make_dyn_managed_driver(toolchain),
    });
    let compile: Arc<dyn ManagedDriver> = Arc::new(GoCompileDriver::new());
    drivers.push(NamedDriver {
        name: "go_compile".into(),
        driver: make_dyn_managed_driver(compile),
    });
    let testmain: Arc<dyn ManagedDriver> = Arc::new(GoTestmainDriver);
    drivers.push(NamedDriver {
        name: "go_testmain".into(),
        driver: make_dyn_managed_driver(testmain),
    });

    Ok(PluginComponents {
        provider_name: "go".into(),
        provider: stabby::option::Option::Some(make_dyn_provider(provider)),
        drivers,
        // The go plugin exports no hooks.
        hooks: stabby::vec::Vec::new(),
        // No return-side metadata to report yet.
        meta: stabby::vec::Vec::new(),
    })
}
