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
    GoCompileDriver, GoFormatCheckDriver, GoFormatDriver, GoGolistDriver, GoLintDriver,
    GoLintFixDriver, GoLintGateDriver, GoTestmainDriver, GoToolchainDriver, Provider,
};
use plugin_sdk::stabby::abi::{DynLogSink, DynSupervisor, NamedDriver, PluginComponents};
use plugin_sdk::stabby::{
    create_config_from_bytes, install_log_sink, install_supervisor, make_dyn_managed_driver,
    make_dyn_provider, options_from_pb_map,
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
            tracing::error!("heph-plugin-go: plugin construction failed: {e:#}");
            std::process::abort();
        }
    }
}

/// Stable ABI log-sink entry: the host calls this right after `create` to hand the
/// plugin a sink for its `tracing` events. Without it, this cdylib's
/// statically-linked `tracing` has no subscriber and the plugin's logs vanish.
#[stabby::export]
pub extern "C" fn heph_plugin_set_log_sink(sink: DynLogSink) {
    install_log_sink(sink);
}

/// Stable ABI supervisor entry: the host hands the plugin its process-supervisor
/// client. This cdylib links its own `proc`, whose tracker the host's startup
/// `init` never reached — without this, every `go` compile this plugin spawns goes
/// unregistered with the sidecar (orphaned on a hard kill of the host).
#[stabby::export]
pub extern "C" fn heph_plugin_set_supervisor(sup: DynSupervisor) {
    install_supervisor(sup);
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
    let go_runner = hplugin_go::plugingo::runner::take_runner_option(&mut options)?;

    let walker = Arc::new(hwalk::CachedWalker::open(&walk_db));
    let provider: Arc<dyn hplugin::provider::Provider> = Arc::new(Provider::from_options(
        root,
        &[],
        &[],
        &options,
        walker,
        plugin_sdk::stabby::cdylib_runtime_handle(),
    )?);

    let mut drivers = stabby::vec::Vec::new();
    // The shared golist GOCACHE lives in the engine's home dir, next to the
    // walker db and for the same reason: it is heph-owned scratch, not repo
    // content, and a plugin is handed its writable locations rather than
    // discovering them.
    let golist: Arc<dyn ManagedDriver> = Arc::new(
        GoGolistDriver::with_gocache_root(home.join("go-golist-gocache"))
            .with_default_runner(go_runner.clone()),
    );
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
    let compile: Arc<dyn ManagedDriver> =
        Arc::new(GoCompileDriver::new().with_default_runner(go_runner.clone()));
    drivers.push(NamedDriver {
        name: "go_compile".into(),
        driver: make_dyn_managed_driver(compile),
    });
    let testmain: Arc<dyn ManagedDriver> = Arc::new(GoTestmainDriver);
    drivers.push(NamedDriver {
        name: "go_testmain".into(),
        driver: make_dyn_managed_driver(testmain),
    });
    // Per-package go/analysis (vet) with serialized facts, nogo-style.
    let lint: Arc<dyn ManagedDriver> =
        Arc::new(GoLintDriver::new().with_default_runner(go_runner.clone()));
    drivers.push(NamedDriver {
        name: "go_lint".into(),
        driver: make_dyn_managed_driver(lint),
    });
    // Gate: fails the build when a package's lint report has findings.
    let lint_gate: Arc<dyn ManagedDriver> = Arc::new(GoLintGateDriver::new());
    drivers.push(NamedDriver {
        name: "go_lint_gate".into(),
        driver: make_dyn_managed_driver(lint_gate),
    });
    // Fix: applies the report's suggested fixes back into source (codegen).
    let lint_fix: Arc<dyn ManagedDriver> = Arc::new(GoLintFixDriver::new());
    drivers.push(NamedDriver {
        name: "go_lint_fix".into(),
        driver: make_dyn_managed_driver(lint_fix),
    });
    // Formatters (gofmt/gofumpt/goimports) via heph-govet's -format mode.
    let format: Arc<dyn ManagedDriver> =
        Arc::new(GoFormatDriver::new().with_default_runner(go_runner.clone()));
    drivers.push(NamedDriver {
        name: "go_format".into(),
        driver: make_dyn_managed_driver(format),
    });
    let format_check: Arc<dyn ManagedDriver> =
        Arc::new(GoFormatCheckDriver::new().with_default_runner(go_runner));
    drivers.push(NamedDriver {
        name: "go_format_check".into(),
        driver: make_dyn_managed_driver(format_check),
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
