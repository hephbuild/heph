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
use hplugin_go::plugingo::{GoEmbedDriver, GoGolistDriver, GoTestmainDriver, Provider};
use plugin_sdk::stabby::abi::{CreateConfig, NamedDriver, PluginComponents};
use plugin_sdk::stabby::{make_dyn_managed_driver, make_dyn_provider};
use std::path::PathBuf;
use std::sync::Arc;

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols the
/// host's `get_stabbied` checks for ABI compatibility.
#[stabby::export]
pub extern "C" fn heph_plugin_create(cfg: CreateConfig) -> PluginComponents {
    match build(cfg) {
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

fn build(cfg: CreateConfig) -> anyhow::Result<PluginComponents> {
    let root = PathBuf::from(cfg.root.to_string());
    let home = PathBuf::from(cfg.home.to_string());

    // Tunables come from the plugin's `options:` map (config yaml), decoded the
    // same way an in-process plugin reads its options.
    let mut options = plugin_sdk::stabby::options_from_pb_bytes(&cfg.options[..])?;
    // `go_bin`/`walk_db` are this cdylib's own (driver/walker) options — consume
    // them here and keep them out of the provider's map, whose `from_options`
    // rejects unknown keys (it knows only its provider options, e.g. `gotool`).
    let go_bin = hplugin::config::decode_opt::<String>(&options, "go", "go_bin")?
        .unwrap_or_else(|| "//@heph/bin:go".to_string());
    // The walker db lives in the engine's home dir (e.g. `.heph3`), not the repo
    // root — `home` comes from the engine, never hardcoded.
    let walk_db = hplugin::config::decode_opt::<PathBuf>(&options, "go", "walk_db")?
        .unwrap_or_else(|| home.join("heph-plugin-go-fswalk.db"));
    options.remove("go_bin");
    options.remove("walk_db");

    let walker = Arc::new(hwalk::CachedWalker::open(&walk_db));
    let provider: Arc<dyn hplugin::provider::Provider> =
        Arc::new(Provider::from_options(root, &[], &[], &options, walker)?);

    let mut drivers = stabby::vec::Vec::new();
    let golist: Arc<dyn ManagedDriver> = Arc::new(GoGolistDriver::new(go_bin));
    drivers.push(NamedDriver {
        name: "go_golist".into(),
        driver: make_dyn_managed_driver(golist),
    });
    let embed: Arc<dyn ManagedDriver> = Arc::new(GoEmbedDriver);
    drivers.push(NamedDriver {
        name: "go_embed".into(),
        driver: make_dyn_managed_driver(embed),
    });
    let testmain: Arc<dyn ManagedDriver> = Arc::new(GoTestmainDriver);
    drivers.push(NamedDriver {
        name: "go_testmain".into(),
        driver: make_dyn_managed_driver(testmain),
    });

    Ok(PluginComponents {
        provider_name: "go".into(),
        provider: make_dyn_provider(provider),
        drivers,
    })
}
