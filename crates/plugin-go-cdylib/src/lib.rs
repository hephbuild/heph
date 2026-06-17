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
use plugin_sdk::abi::{CreateConfig, NamedDriver, PluginComponents};
use plugin_sdk::serve::{make_dyn_managed_driver, make_dyn_provider};
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
    let go_bin =
        std::env::var("HEPH_PLUGIN_GO_BIN").unwrap_or_else(|_| "//@heph/bin:go".to_string());
    let walk_db = std::env::var_os("HEPH_PLUGIN_GO_WALK_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".heph-plugin-go-fswalk.db"));

    let walker = Arc::new(hwalk::CachedWalker::open(&walk_db));
    let opts = hplugin::config::Options::new();
    let provider: Arc<dyn hplugin::provider::Provider> =
        Arc::new(Provider::from_options(root, &[], &[], &opts, walker)?);

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
