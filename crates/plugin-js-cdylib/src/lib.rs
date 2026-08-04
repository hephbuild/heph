//! The JS/TS plugin as a loadable cdylib behind the stable ABI.
//!
//! **M0-M5 scope**: exports the `js` provider (package discovery + pnpm/npm
//! workspace-member resolution + lockfile-driven dependency wiring + oxc-based
//! import-graph resolution), its `js_package_info` dependency-wiring driver,
//! the hermetic `js_install` third-party fetch driver, the `js_typecheck`
//! driver (M3 — `tsc --noEmit` per package, via a disclosed non-hermetic
//! `tstool = "host"` toolchain; see `hplugin_js::pluginjs::driver_typecheck`
//! module docs), the `js_test` driver (M4 — one target per test file, via
//! the configured `testrunner` (`vitest`/`jest`), same disclosed non-hermetic
//! host-toolchain shape; see `hplugin_js::pluginjs::driver_test` module
//! docs), and the `js_lint` driver (M5 — one target per package, via the
//! configured `linter` (`oxlint`/`eslint`), same disclosed non-hermetic
//! host-toolchain shape; see `hplugin_js::pluginjs::driver_lint` module
//! docs). The host loads this with `hplugin_stabby::load_stable::load`,
//! which verifies ABI compatibility via stabby's type reports before use.
//! Calls then run in-process at native speed — no serialization on the hot
//! path, no IPC.
//!
//! Formatting and bundling drivers are later milestones (see
//! `ai-docs/js-plugin-plan.md`) and are not wired up here yet.
//!
//! Plugin-specific settings are read from the environment; only the
//! workspace root crosses in [`CreateConfig`].

use hdriver_support::driver_managed::ManagedDriver;
use hplugin_js::pluginjs::{
    JsInstallDriver, JsLintDriver, JsPackageInfoDriver, JsTestDriver, JsTypecheckDriver, Provider,
};
use plugin_sdk::stabby::abi::{DynLogSink, DynSupervisor, NamedDriver, PluginComponents};
use plugin_sdk::stabby::{
    create_config_from_bytes, install_log_sink, install_supervisor, make_dyn_managed_driver,
    make_dyn_provider, options_from_pb_map,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Stable ABI create entry. `#[stabby::export]` emits the type-report symbols
/// the host's `get_stabbied` checks for ABI compatibility. `cfg` is
/// prost-encoded `pb::CreateConfig` bytes, so config fields are additive
/// across versions.
#[stabby::export]
pub extern "C" fn heph_plugin_create(cfg: stabby::vec::Vec<u8>) -> PluginComponents {
    match build(&cfg) {
        Ok(c) => c,
        Err(e) => {
            // No safe way to surface an error through the stable bundle (it
            // must carry a valid provider handle), and unwinding across the
            // FFI boundary is UB — so fail loudly and abort.
            tracing::error!("heph-plugin-js: plugin construction failed: {e:#}");
            std::process::abort();
        }
    }
}

/// Stable ABI log-sink entry: the host calls this right after `create` to
/// hand the plugin a sink for its `tracing` events. Without it, this
/// cdylib's statically-linked `tracing` has no subscriber and the plugin's
/// logs vanish.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// Stable ABI supervisor entry: the host hands the plugin its
/// process-supervisor client. M0 spawns no subprocesses (no install/exec
/// driver yet), but every cdylib plugin wires this the same way so a later
/// milestone's driver doesn't need this entry added retroactively.
#[stabby::export]
pub extern "C" fn heph_plugin_set_supervisor(sup: DynSupervisor) {
    install_supervisor(sup);
}

fn build(cfg: &[u8]) -> anyhow::Result<PluginComponents> {
    let cfg = create_config_from_bytes(cfg)?;
    let root = PathBuf::from(cfg.root);
    let home = PathBuf::from(cfg.home);

    // Tunables come from the plugin's `options:` map (config yaml), carried
    // as structured CreateConfig data — read the same way an in-process
    // plugin does.
    let mut options = options_from_pb_map(cfg.options);
    // The walker db lives in the engine's home dir (e.g. `.heph3`), not the
    // repo root — `home` comes from the engine, never hardcoded. It's this
    // cdylib's own option — consume it so it's kept out of the provider's
    // map, whose `from_options` rejects unknown keys.
    let walk_db = hplugin::config::decode_opt::<PathBuf>(&options, "js", "walk_db")?
        .unwrap_or_else(|| home.join("heph-plugin-js-fswalk.db"));
    options.remove("walk_db");

    let walker = Arc::new(hwalk::CachedWalker::open(&walk_db));
    let provider: Arc<dyn hplugin::provider::Provider> =
        Arc::new(Provider::from_options(root, &[], &[], &options, walker)?);

    let mut drivers = stabby::vec::Vec::new();
    let package_info: Arc<dyn ManagedDriver> = Arc::new(JsPackageInfoDriver::new());
    drivers.push(NamedDriver {
        name: "js_package_info".into(),
        driver: make_dyn_managed_driver(package_info),
    });
    // Hermetic per-`(name, version, integrity)` third-party dependency
    // fetch — see `ai-docs/js-plugin-plan.md`'s Hermeticity section.
    let install: Arc<dyn ManagedDriver> = Arc::new(JsInstallDriver::new());
    drivers.push(NamedDriver {
        name: "js_install".into(),
        driver: make_dyn_managed_driver(install),
    });
    // `js_typecheck` (M3): runs `tsc --noEmit` per package — see
    // `hplugin_js::pluginjs::driver_typecheck` module docs, including its
    // disclosed non-hermetic `tstool = "host"` toolchain gap.
    let typecheck: Arc<dyn ManagedDriver> = Arc::new(JsTypecheckDriver::new());
    drivers.push(NamedDriver {
        name: "js_typecheck".into(),
        driver: make_dyn_managed_driver(typecheck),
    });
    // `js_test` (M4): runs the configured test runner (`vitest` default,
    // `jest` alt) against one test file at a time — see
    // `hplugin_js::pluginjs::driver_test` module docs, including its
    // disclosed non-hermetic host-toolchain gap.
    let test: Arc<dyn ManagedDriver> = Arc::new(JsTestDriver::new());
    drivers.push(NamedDriver {
        name: "js_test".into(),
        driver: make_dyn_managed_driver(test),
    });
    // `js_lint` (M5): runs the configured linter (`oxlint` default, `eslint`
    // alt) against one package at a time — see
    // `hplugin_js::pluginjs::driver_lint` module docs, including its
    // disclosed non-hermetic host-toolchain gap.
    let lint: Arc<dyn ManagedDriver> = Arc::new(JsLintDriver::new());
    drivers.push(NamedDriver {
        name: "js_lint".into(),
        driver: make_dyn_managed_driver(lint),
    });

    Ok(PluginComponents {
        provider_name: "js".into(),
        provider: stabby::option::Option::Some(make_dyn_provider(provider)),
        drivers,
        // The js plugin exports no hooks yet.
        hooks: stabby::vec::Vec::new(),
        // No return-side metadata to report yet.
        meta: stabby::vec::Vec::new(),
    })
}
