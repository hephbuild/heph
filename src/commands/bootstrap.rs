use std::sync::{Arc, Weak};
use std::thread::available_parallelism;

use anyhow::Context;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

use crate::engine::config::ConfigYamlExt;
use crate::engine::config_yaml;
use crate::{
    engine, pluginbuildfile, pluginexec, pluginhostbin, pluginhttp, pluginnix, plugintextfile,
};

/// Builds the multi-thread runtime used by every command entry point.
///
/// `max_blocking_threads` is sized generously above the engine's `2 × parallelism`
/// execute permits, since every `block_in_place` site parks a runtime worker and
/// spawns a replacement from the blocking pool. Default `512` is enough today but
/// silently degrades to inline execution if saturated; the explicit `8 × N + 64`
/// gives clear headroom without ever hitting the cliff.
///
/// The heavy synchronous work — cache writes, gzip, starlark evaluation, the Go
/// package walks — does *not* draw on this pool: it runs on `hcore::blocking`'s own
/// dedicated threads, which is what keeps these `n` workers free to poll the
/// reactor and the timer wheel.
pub fn build_runtime() -> std::io::Result<Runtime> {
    let n = available_parallelism().map(|p| p.get()).unwrap_or(8);
    Builder::new_multi_thread()
        .worker_threads(n)
        .max_blocking_threads(8 * n + 64)
        .enable_all()
        .build()
}

/// Every command entry point funnels through here so the blocking-pool sizing
/// in `build_runtime` applies uniformly (the plain `#[tokio::main]` attribute
/// can't express `max_blocking_threads`). Builds the tuned runtime, drives
/// `fut` to completion, and lets the runtime drop normally at end of scope.
///
/// Critical teardown — FUSE unmount, SQLite flush, sandbox rmdir (all in
/// `Engine::drop`), plus the sandbox-cleanup queue the renderer waits on — has
/// already run *inside* `fut`: the last `Arc<Engine>` lives in the app task, so
/// its drop completes before `fut` resolves. The runtime drop only joins idle
/// worker/blocking threads after that.
pub fn block_on<F: Future>(fut: F) -> anyhow::Result<F::Output> {
    Ok(build_runtime()
        .context("build tokio runtime")?
        .block_on(fut))
}

/// Producer-side handle for the in-process shutdown signal. Both the SIGINT
/// listener and the TUI's Ctrl+C key handler call `trigger()` on this; the
/// state machine in `spawn_shutdown_handler` is the single consumer.
///
/// While `SuppressionHandle::set(true)` is in effect — only while the TUI has
/// handed the keyboard to a `--shell` session (`tui::PauseFor::Input`) —
/// `trigger()` silently drops presses, so neither kernel SIGINT nor TUI Ctrl+C
/// cancels engine work in that window. A pause that merely borrows the terminal
/// for *output* (a diagnostic, a stdout flush, a target running with the
/// terminal attached) must not suppress: the key handler is not reading while
/// paused, so dropping the SIGINT too leaves the run with no Ctrl-C at all.
// The shutdown signal types moved to `heph-core::shutdown` so the TUI can hold a
// trigger without depending on this bin module; re-exported for existing callers.
pub use hcore::shutdown::{ShutdownTrigger, SuppressionHandle};

/// Built-in hook that feeds the build-event stream into the usage-telemetry
/// counters (the PostHog exit reporter snapshots them). Registered only when
/// telemetry is enabled — see `new_engine`. The flush, the synchronous
/// `snapshot()` read, and the non-event recorders stay as direct calls.
struct TelemetryHook;

impl engine::hook::Hook for TelemetryHook {
    fn name(&self) -> String {
        "telemetry".to_string()
    }
    fn on_event(&self, ev: &hcore::events::BuildEvent) {
        crate::telemetry::observe_event(&ev.kind);
    }
    fn on_close(&self) {}
}

/// Read the telemetry opt-out flag straight from `.hephconfig2`, independent of
/// engine construction, so the top-level CLI reporter can decide whether to send
/// without holding an `Engine`. Any failure (no repo root, unreadable/invalid
/// config) defaults to enabled — the env kill switch still applies on top.
pub fn telemetry_enabled_from_config() -> bool {
    let Ok(root) = engine::get_root() else {
        return true;
    };
    config_yaml::load_from_root(&root)
        .map(|f| f.telemetry_enabled())
        .unwrap_or(true)
}

pub fn new_engine() -> anyhow::Result<(Arc<engine::Engine>, ShutdownTrigger)> {
    let root = match engine::get_root() {
        Ok(r) => r,
        Err(inner) => anyhow::bail!("Error: {}", inner),
    };

    // The config file is the all-optional, profile-layered YAML; `resolve`
    // applies every default in one place and yields the engine's runtime config.
    let file = config_yaml::load_from_root(&root)?;
    let config = file.resolve(&root)?;

    // Captured before `config` is moved into the engine: the nix driver's state
    // dir hangs off `home_dir`, and telemetry reports the remote-cache backend
    // kinds (scheme only — never the URIs).
    let home_dir = config.home_dir.clone();
    let remote_cache_backends: Vec<String> = config
        .remote_caches
        .iter()
        .map(|d| d.backend_kind().to_string())
        .collect();

    let mut e = engine::Engine::new(config)?;

    // Always on, never gated. The diagnostic that needs a flag is the one you did
    // not pass on the run that hung — the whole reason this exists. Its `on_event`
    // is relaxed atomics only, so a build that never stalls pays a handful of
    // instructions per event and nothing else.
    e.register_hook(Arc::new(engine::diag::DiagHook::new(Arc::clone(
        engine::diag::global(),
    ))))?;

    // `fs` (provider + driver) is registered by `Engine::new` itself, with the
    // engine's own skip dirs. The remaining built-ins have no config.
    e.register_provider(|_| Box::new(pluginhostbin::Provider))?;
    e.register_driver(|_| Box::new(pluginhostbin::Driver))?;
    e.register_driver(|_| Box::new(plugintextfile::Driver))?;
    // `http_fetch`: downloads a URL (templated over the target's addr args) into a
    // cacheable file output — how tool binaries are provisioned off the internet.
    e.register_managed_driver(|_| Box::new(pluginhttp::Driver))?;
    // The `oci_*` drivers are not compiled in: like the go plugin, they ship as
    // a separate cdylib loaded from a `path:`/`url:` manifest entry
    // (`heph-oci-plugin.json`), under their own `docker_build` / `oci_pull` /
    // `oci_push` / `oci_load` names.
    e.register_managed_driver(|_| Box::new(pluginnix::Driver::new(home_dir.join("nix-driver"))))?;

    // Opt-in built-in factories — instantiated only when a `plugins: - { builtin:
    // <name> }` entry selects them. The go plugin is no longer compiled in: it
    // ships as a separate cdylib loaded from a `path:`/`url:` manifest entry,
    // under its own `go`/`go_*` names.
    e.register_provider_factory("buildfile", |init, opts| {
        Ok(Box::new(
            pluginbuildfile::Provider::from_options(
                init.root.to_path_buf(),
                &init.skip_dirs,
                &init.skip_globs,
                opts,
                init.runtime.clone(),
            )?
            .with_walker(init.walker.clone()),
        ))
    })?;
    e.register_managed_driver_factory("exec", |_init, opts| {
        Ok(Box::new(pluginexec::Driver::from_options_exec(opts)?))
    })?;
    e.register_managed_driver_factory("bash", |_init, opts| {
        Ok(Box::new(pluginexec::Driver::from_options_bash(opts)?))
    })?;
    e.register_managed_driver_factory("sh", |_init, opts| {
        Ok(Box::new(pluginexec::Driver::from_options_sh(opts)?))
    })?;

    // Apply every `plugins:` entry: a `builtin:` instantiates the matching
    // factory above; a `path:`/`url:` resolves a manifest, loads the cdylib, and
    // registers the provider + drivers it exports. The engine owns the resolve +
    // download + load machinery (see `engine::plugin_load`).
    e.register_plugins(&file.plugins)?;

    // Usage telemetry is a built-in, auto-enabled hook: it folds the build-event
    // stream into the process-global counters the exit reporter flushes to
    // PostHog. Registered only when telemetry is enabled, so an opt-out pays no
    // per-event cost. Non-event signals (execute_ms, artifacts, graph size, the
    // CLI invocation) are still recorded via direct `telemetry::record_*` calls,
    // and the synchronous `snapshot()` read the engine uses stays direct.
    if crate::telemetry::is_enabled(file.telemetry_enabled()) {
        e.register_hook(Arc::new(TelemetryHook))?;
    }

    let engine = Arc::new(e);

    // Telemetry: record the enabled provider + driver type names (built-ins plus
    // whatever the config turned on) and the remote-cache count for the exit
    // reporter. Set-once.
    crate::telemetry::record_plugins(
        engine.providers_by_name.keys().cloned().collect(),
        engine.drivers_by_name.keys().cloned().collect(),
        remote_cache_backends,
    );

    // Point `SIGQUIT` dumps at the resolved home, so they land beside the stall
    // log and the in-flight report instead of under whatever cwd the process was
    // launched from. Every command routes through here, so every command gets it.
    crate::diag::set_dump_dir(&engine.home.join("diag"));

    let (trigger, rx) = ShutdownTrigger::new();
    spawn_sigint_producer(trigger.clone());
    spawn_shutdown_handler(Arc::downgrade(&engine), rx);
    Ok((engine, trigger))
}

/// Bridge OS SIGINT into the shutdown trigger. The terminal may swallow
/// Ctrl-C while the TUI is in raw mode (ISIG cleared), in which case this
/// task never fires and the TUI's key handler drives the trigger instead.
/// Both producers are gated by the same `SuppressionHandle`.
fn spawn_sigint_producer(trigger: ShutdownTrigger) {
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            trigger.trigger();
        }
    });
}

/// Single consumer for the shutdown trigger. First press broadcasts
/// cancellation to every in-flight request, letting drivers kill+reap their
/// children and the TUI restore the terminal before unwinding naturally.
/// Second press hard-exits with code 130 — the process supervisor sidecar
/// reaps any remaining tracked process groups.
///
/// Engine is held as a `Weak` so the spawned task can't keep the engine
/// alive past the command's natural lifetime. Must be called from inside a
/// tokio runtime; `bootstrap::new_engine` is only invoked from command entry
/// points already running on a `bootstrap::block_on` runtime.
fn spawn_shutdown_handler(engine: Weak<engine::Engine>, mut rx: mpsc::UnboundedReceiver<()>) {
    tokio::spawn(async move {
        if rx.recv().await.is_none() {
            return;
        }
        tracing::warn!("ctrl-c received, cancelling in-flight work (press ctrl-c again to abort)");
        if let Some(e) = engine.upgrade() {
            e.cancel_all_requests();
        }
        if rx.recv().await.is_none() {
            return;
        }
        tracing::error!("second ctrl-c, aborting");
        hard_abort(|code| std::process::exit(code));
    });
}

/// The second-Ctrl-C abort sequence, with the actual exit call taken as a
/// parameter so tests can observe the ordering without killing the test
/// process. `std::process::exit` runs no destructors — a raw-mode terminal
/// left as-is would stay raw (and its cursor hidden) after heph exits — so
/// the restore must happen here, before `exit`, rather than relying on a
/// `Drop` impl the exit call would skip.
///
/// `restore_terminal` is a no-op unless a raw-mode session (the TUI)
/// registered a restore closure, so a non-interactive run's second Ctrl-C
/// still exits with no extra work and no stray writes to a redirected
/// stdout/stderr.
fn hard_abort(exit: impl FnOnce(i32)) {
    hcore::shutdown::restore_terminal();
    exit(130);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_engine_from_yaml(yaml: &str) -> anyhow::Result<(tempfile::TempDir, engine::Engine)> {
        // Engine::new captures the ambient runtime for its request memoizers;
        // these are plain #[test]s, so provide one. Static: handles captured
        // from it stay valid for the life of the test process.
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        let _rt = RT
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .expect("test runtime")
            })
            .enter();
        let file: config_yaml::ConfigYaml = serde_yaml::from_str(yaml)?;
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let home_dir = file
            .home_dir
            .as_ref()
            .map(|p| root.join(p))
            .unwrap_or_else(|| root.join(".heph3"));
        let mut e = engine::Engine::new(engine::Config {
            root,
            home_dir: home_dir.clone(),
            fs_skip: file.fs.clone().map(|f| f.skip).unwrap_or_default(),
            parallelism: None,
            ..Default::default()
        })?;

        // `fs` is auto-registered by `Engine::new`.
        e.register_provider(|_| Box::new(pluginhostbin::Provider))?;
        e.register_driver(|_| Box::new(pluginhostbin::Driver))?;
        e.register_driver(|_| Box::new(plugintextfile::Driver))?;
        e.register_managed_driver(|_| Box::new(pluginhttp::Driver))?;
        e.register_managed_driver(|_| {
            Box::new(pluginnix::Driver::new(home_dir.join("nix-driver")))
        })?;

        e.register_provider_factory("buildfile", |init, opts| {
            Ok(Box::new(
                pluginbuildfile::Provider::from_options(
                    init.root.to_path_buf(),
                    &init.skip_dirs,
                    &init.skip_globs,
                    opts,
                    init.runtime.clone(),
                )?
                .with_walker(init.walker.clone()),
            ))
        })?;
        e.register_managed_driver_factory("exec", |_init, opts| {
            Ok(Box::new(pluginexec::Driver::from_options_exec(opts)?))
        })?;
        e.register_managed_driver_factory("bash", |_init, opts| {
            Ok(Box::new(pluginexec::Driver::from_options_bash(opts)?))
        })?;

        // The helper exercises built-in plugins only (no cdylib loading).
        for spec in &file.plugins {
            match &spec.identifier {
                config_yaml::PluginIdentifier::Builtin(name) => {
                    e.apply_builtin(name, &spec.options)?
                }
                _ => anyhow::bail!("test helper supports only `builtin:` plugins"),
            }
        }
        Ok((dir, e))
    }

    #[test]
    fn applies_listed_builtins() {
        let yaml = r#"
plugins:
  - builtin: buildfile
    options:
      patterns: [BUILD]
  - builtin: exec
  - builtin: bash
"#;
        let (_dir, e) = build_engine_from_yaml(yaml).expect("engine");
        assert!(e.providers_by_name.contains_key("buildfile"));
        assert!(e.drivers_by_name.contains_key("exec"));
        assert!(e.drivers_by_name.contains_key("bash"));
        assert!(e.providers_by_name.contains_key("fs"));
    }

    #[test]
    fn fs_skip_splits_dirs_and_globs() {
        let yaml = r#"
fs:
  skip: [vendor, "./node_modules", "**/node_modules/**"]
"#;
        // Literal entries (incl. the `./` "current dir" form) resolve to absolute
        // root-relative skip dirs; glob entries become skip globs. Both are handed
        // to the fs plugin and every provider factory.
        let (dir, e) = build_engine_from_yaml(yaml).expect("engine");
        assert!(
            e.skip_dirs().contains(&dir.path().join("vendor")),
            "{:?}",
            e.skip_dirs()
        );
        // `./node_modules` normalizes to the bare root-relative `node_modules`.
        assert!(
            e.skip_dirs().contains(&dir.path().join("node_modules")),
            "{:?}",
            e.skip_dirs()
        );
        // `.git` is an always-on engine built-in, ahead of the config globs.
        assert_eq!(
            e.skip_globs(),
            vec!["**/.git/**".to_string(), "**/node_modules/**".to_string()]
        );
        assert!(e.providers_by_name.contains_key("fs"));
        assert!(e.drivers_by_name.contains_key("fs"));
    }

    #[test]
    fn unknown_builtin_errors() {
        let yaml = "plugins:\n  - builtin: nope\n";
        let err = build_engine_from_yaml(yaml).err().expect("must error");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn empty_config_only_loads_builtins() {
        let (_dir, e) = build_engine_from_yaml("").expect("engine");
        assert!(!e.providers_by_name.contains_key("buildfile"));
        assert!(!e.drivers_by_name.contains_key("exec"));
        assert!(e.providers_by_name.contains_key("fs"));
        assert!(e.drivers_by_name.contains_key("fs"));
    }

    /// Build an engine + a shutdown trigger wired to the consumer state
    /// machine, but skip the SIGINT producer — tests drive the trigger
    /// directly to keep the path hermetic (no real signals).
    fn build_engine_with_trigger() -> (tempfile::TempDir, Arc<engine::Engine>, ShutdownTrigger) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let engine = Arc::new(
            engine::Engine::new(engine::Config {
                root: root.clone(),
                home_dir: root.join(".heph3"),
                parallelism: None,
                ..Default::default()
            })
            .expect("engine"),
        );
        let (trigger, rx) = ShutdownTrigger::new();
        spawn_shutdown_handler(Arc::downgrade(&engine), rx);
        (dir, engine, trigger)
    }

    async fn await_cancelled(token: &crate::hasync::StdCancellationToken) -> bool {
        for _ in 0..100 {
            if token.is_cancelled() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        token.is_cancelled()
    }

    #[tokio::test]
    async fn trigger_cancels_in_flight_requests() {
        let (_dir, engine, trigger) = build_engine_with_trigger();
        let rs = engine.new_state();
        assert!(!rs.ctoken().is_cancelled());

        trigger.trigger();

        assert!(
            await_cancelled(rs.ctoken()).await,
            "trigger must propagate to in-flight request tokens"
        );
    }

    #[tokio::test]
    async fn suppression_drops_trigger_then_resumes() {
        let (_dir, engine, trigger) = build_engine_with_trigger();
        let rs = engine.new_state();
        let suppression = trigger.suppression();

        // Suppressed: trigger must be a no-op.
        suppression.set(true);
        trigger.trigger();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !rs.ctoken().is_cancelled(),
            "suppressed trigger must not cancel"
        );

        // Resumed: trigger must take effect.
        suppression.set(false);
        trigger.trigger();
        assert!(
            await_cancelled(rs.ctoken()).await,
            "trigger after resume must cancel"
        );
    }

    // Serialized: `hcore::shutdown`'s restore-closure slot is a process-global
    // static, so these must not run concurrently with each other (or with
    // `crates/core`'s own tests of the same static, which live in a separate
    // binary and can't collide with this one).
    static TERMINAL_RESTORE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Regression for the bug this module used to have: the second Ctrl-C
    /// called `std::process::exit(130)` directly, which runs no destructors —
    /// a raw-mode terminal stayed raw and its cursor stayed hidden after heph
    /// exited. `hard_abort` must restore the terminal *before* exiting rather
    /// than relying on a `Drop` impl the exit call would skip.
    ///
    /// Pre-fix (`std::process::exit(130)` called directly, no
    /// `restore_terminal()`), this test goes red: the registered closure
    /// never fires. Exit is injected so the test can observe the ordering
    /// without killing the test process.
    #[test]
    fn hard_abort_restores_terminal_before_exiting() {
        let _guard = TERMINAL_RESTORE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let restored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let restored_in_closure = std::sync::Arc::clone(&restored);
        hcore::shutdown::set_terminal_restore(move || {
            restored_in_closure.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let mut exit_code = None;
        hard_abort(|code| exit_code = Some(code));

        assert!(
            restored.load(std::sync::atomic::Ordering::SeqCst),
            "hard_abort must restore the terminal before exiting"
        );
        assert_eq!(exit_code, Some(130), "must exit with code 130");

        hcore::shutdown::clear_terminal_restore();
    }

    /// A non-interactive run never registers a restore closure — `hard_abort`
    /// must still exit cleanly and must not panic reaching for one.
    #[test]
    fn hard_abort_is_a_plain_exit_when_nothing_was_registered() {
        let _guard = TERMINAL_RESTORE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        hcore::shutdown::clear_terminal_restore();

        let mut exit_code = None;
        hard_abort(|code| exit_code = Some(code));

        assert_eq!(exit_code, Some(130));
    }
}
