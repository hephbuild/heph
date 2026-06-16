use std::sync::{Arc, Weak};
use std::thread::available_parallelism;

use anyhow::Context;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

use crate::engine::config_yaml;
use crate::{
    engine, pluginbuildfile, pluginexec, plugingo, pluginhostbin, pluginnix, plugintextfile,
};

/// Builds the multi-thread runtime used by every command entry point.
///
/// `max_blocking_threads` is sized generously above the engine's `2 × parallelism`
/// execute permits, since every `block_in_place` site parks a runtime worker and
/// spawns a replacement from the blocking pool. Default `512` is enough today but
/// silently degrades to inline execution if saturated; the explicit `8 × N + 64`
/// gives clear headroom for nested `block_or_inline` chains (execute → cache →
/// plugingo) without ever hitting the cliff.
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
/// While `SuppressionHandle::set(true)` is in effect (e.g. the TUI is paused
/// for an interactive prompt), `trigger()` silently drops presses — neither
/// kernel SIGINT nor TUI Ctrl+C cancels engine work in that window.
// The shutdown signal types moved to `heph-core::shutdown` so the TUI can hold a
// trigger without depending on this bin module; re-exported for existing callers.
pub use hcore::shutdown::{ShutdownTrigger, SuppressionHandle};

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

    // `fs` (provider + driver) is registered by `Engine::new` itself, with the
    // engine's own skip dirs. The remaining built-ins have no config.
    e.register_provider(|_| Box::new(pluginhostbin::Provider))?;
    e.register_driver(|_| Box::new(pluginhostbin::Driver))?;
    e.register_driver(|_| Box::new(plugintextfile::Driver))?;
    e.register_managed_driver(|_| Box::new(pluginnix::Driver::new(home_dir.join("nix-driver"))))?;

    // Names a `bin:` config entry routes to an out-of-process plugin. For those,
    // skip the in-process built-in factory below and register a remote handle
    // (spawned by `register_bin_plugins`) under the same name instead.
    // A name routes out-of-process (`bin:`) OR to an in-process loadable cdylib
    // (`dylib:`) / wasm component (`wasm:`); either way the built-in in-process
    // factory below is skipped.
    let bin_names: std::collections::HashSet<&str> = file
        .providers
        .iter()
        .chain(file.drivers.iter())
        .filter(|en| en.bin.is_some() || en.dylib.is_some() || en.wasm.is_some())
        .map(|en| en.name.as_str())
        .collect();

    // Opt-in built-in factories — instantiated by `apply_config` if listed in
    // the YAML and not overridden by a `bin:` entry.
    if !bin_names.contains("buildfile") {
        e.register_provider_factory("buildfile", |init, opts| {
            Ok(Box::new(
                pluginbuildfile::Provider::from_options(
                    init.root.to_path_buf(),
                    &init.skip_dirs,
                    &init.skip_globs,
                    opts,
                )?
                .with_walker(init.walker.clone()),
            ))
        })?;
    }
    if !bin_names.contains("go") {
        e.register_provider_factory("go", |init, opts| {
            Ok(Box::new(plugingo::Provider::from_options(
                init.root.to_path_buf(),
                &init.skip_dirs,
                &init.skip_globs,
                opts,
                init.walker.clone(),
            )?))
        })?;
    }
    if !bin_names.contains("go_golist") {
        e.register_managed_driver_factory("go_golist", |_init, opts| {
            config_yaml::deny_unknown("go_golist driver", opts, &[])?;
            Ok(Box::new(plugingo::GoGolistDriver::new("//@heph/bin:go")))
        })?;
    }
    if !bin_names.contains("go_embed") {
        e.register_managed_driver_factory("go_embed", |_init, opts| {
            config_yaml::deny_unknown("go_embed driver", opts, &[])?;
            Ok(Box::new(plugingo::GoEmbedDriver))
        })?;
    }
    if !bin_names.contains("go_testmain") {
        e.register_managed_driver_factory("go_testmain", |_init, opts| {
            config_yaml::deny_unknown("go_testmain driver", opts, &[])?;
            Ok(Box::new(plugingo::GoTestmainDriver))
        })?;
    }
    if !bin_names.contains("exec") {
        e.register_managed_driver_factory("exec", |_init, opts| {
            Ok(Box::new(pluginexec::Driver::from_options_exec(opts)?))
        })?;
    }
    if !bin_names.contains("bash") {
        e.register_managed_driver_factory("bash", |_init, opts| {
            Ok(Box::new(pluginexec::Driver::from_options_bash(opts)?))
        })?;
    }
    if !bin_names.contains("sh") {
        e.register_managed_driver_factory("sh", |_init, opts| {
            Ok(Box::new(pluginexec::Driver::from_options_sh(opts)?))
        })?;
    }
    drop(bin_names);

    // Spawn + register every out-of-process plugin declared via `bin:`. One
    // process per distinct launch command; all the names that share it (e.g. the
    // `go` provider + its `go_*` drivers) register against that one connection.
    register_bin_plugins(&mut e, &root, &home_dir, &file)?;

    // Load + register every in-process loadable plugin declared via `dylib:`. One
    // cdylib per distinct path serves a provider + its drivers (e.g. `go` plus the
    // `go_*` drivers), loaded once behind the stable ABI at native speed.
    register_dylib_plugins(&mut e, &root, &home_dir, &file)?;

    // Load + register every wasm-component plugin declared via `wasm:`.
    register_wasm_plugins(&mut e, &root, &home_dir, &file)?;

    e.apply_config(&file.providers, &file.drivers)?;

    let engine = Arc::new(e);

    // Telemetry: record the enabled provider + driver type names (built-ins plus
    // whatever the config turned on) and the remote-cache count for the exit
    // reporter. Set-once.
    crate::telemetry::record_plugins(
        engine.providers_by_name.keys().cloned().collect(),
        engine.drivers_by_name.keys().cloned().collect(),
        remote_cache_backends,
    );

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
        std::process::exit(130);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_engine_from_yaml(yaml: &str) -> anyhow::Result<(tempfile::TempDir, engine::Engine)> {
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

        e.apply_config(&file.providers, &file.drivers)?;
        Ok((dir, e))
    }

    #[test]
    fn applies_listed_providers_and_drivers() {
        let yaml = r#"
providers:
  - name: buildfile
    options:
      patterns: [BUILD]
drivers:
  - name: exec
  - name: bash
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
    fn unknown_provider_errors() {
        let yaml = r#"
providers:
  - name: nope
"#;
        let err = build_engine_from_yaml(yaml).err().expect("must error");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn unknown_driver_errors() {
        let yaml = r#"
drivers:
  - name: ghost
"#;
        let err = build_engine_from_yaml(yaml).err().expect("must error");
        assert!(err.to_string().contains("ghost"), "{err}");
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

    #[test]
    fn substitute_os_arch_replaces_placeholders() {
        let out = super::substitute_os_arch("https://x/heph-{os}-{arch}.bin");
        assert!(!out.contains("{os}") && !out.contains("{arch}"), "{out}");
        // arch uses the published spelling (amd64/arm64), never the rust consts.
        assert!(!out.contains("x86_64") && !out.contains("aarch64"), "{out}");
        // os uses darwin, never the rust `macos` spelling.
        assert!(!out.contains("macos"), "{out}");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_bin_argv_path_and_exec() {
        use crate::engine::config_yaml::BinConfig;
        let root = std::path::Path::new("/repo");
        let home = std::path::Path::new("/tmp/unused");
        let path = BinConfig {
            path: Some("/usr/local/bin/heph-plugin-go".into()),
            exec: None,
            url: None,
        };
        assert_eq!(
            super::resolve_bin_argv(&path, "go", root, home).unwrap(),
            vec!["/usr/local/bin/heph-plugin-go".to_string()]
        );
        // Relative path resolves against the workspace root, not the cwd.
        let rel = BinConfig {
            path: Some(".heph3/heph-go-plugin".into()),
            exec: None,
            url: None,
        };
        assert_eq!(
            super::resolve_bin_argv(&rel, "go", root, home).unwrap(),
            vec!["/repo/.heph3/heph-go-plugin".to_string()]
        );
        let exec = BinConfig {
            path: None,
            exec: Some(vec![
                "cargo".into(),
                "run".into(),
                "-p".into(),
                "plugin-go".into(),
            ]),
            url: None,
        };
        assert_eq!(
            super::resolve_bin_argv(&exec, "go", root, home).unwrap(),
            vec![
                "cargo".to_string(),
                "run".to_string(),
                "-p".to_string(),
                "plugin-go".to_string()
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn plan_bin_groups_collapses_shared_command() {
        // A provider + two drivers all pointing at the same binary collapse into
        // one spawn group; a driver with a different binary is its own group.
        let yaml = r#"
providers:
  - name: go
    bin:
      path: /opt/heph-plugin-go
drivers:
  - name: go_golist
    bin:
      path: /opt/heph-plugin-go
  - name: go_embed
    bin:
      path: /opt/heph-plugin-go
  - name: exec
    bin:
      path: /opt/heph-plugin-exec
"#;
        let file: config_yaml::ConfigYaml = serde_yaml::from_str(yaml).expect("parse");
        let root = std::path::Path::new("/repo");
        let home = std::path::Path::new("/tmp/unused");
        let groups = super::plan_bin_groups(&file, root, home).expect("plan");
        assert_eq!(groups.len(), 2, "two distinct binaries => two groups");

        let go = groups
            .get(&vec!["/opt/heph-plugin-go".to_string()])
            .expect("go group");
        assert_eq!(
            go,
            &vec![
                ("go".to_string(), super::BinKind::Provider),
                ("go_golist".to_string(), super::BinKind::Driver),
                ("go_embed".to_string(), super::BinKind::Driver),
            ]
        );
        let exec = groups
            .get(&vec!["/opt/heph-plugin-exec".to_string()])
            .expect("exec group");
        assert_eq!(exec, &vec![("exec".to_string(), super::BinKind::Driver)]);
    }

    #[cfg(unix)]
    #[test]
    fn plan_bin_groups_empty_without_bin() {
        let file: config_yaml::ConfigYaml =
            serde_yaml::from_str("providers:\n  - name: buildfile\n").expect("parse");
        let groups = super::plan_bin_groups(
            &file,
            std::path::Path::new("/repo"),
            std::path::Path::new("/tmp"),
        )
        .expect("plan");
        assert!(groups.is_empty());
    }
}

/// Whether a `bin` entry contributes a provider or a (managed) driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BinKind {
    Provider,
    Driver,
}

/// Spawn plan: launch argv → the named provider/driver entries served by that
/// one process.
#[cfg(unix)]
type BinGroups = std::collections::BTreeMap<Vec<String>, Vec<(String, BinKind)>>;

/// Plan which out-of-process plugins to spawn: resolve every `bin:` entry to its
/// launch argv and group entries that share the identical command, so one
/// process serves all of them (e.g. the `go` provider + its `go_*` drivers).
/// `url` sources are downloaded here. Pure of engine state so it is unit-tested.
#[cfg(unix)]
fn plan_bin_groups(
    file: &config_yaml::ConfigYaml,
    root: &std::path::Path,
    home_dir: &std::path::Path,
) -> anyhow::Result<BinGroups> {
    let mut groups: BinGroups = std::collections::BTreeMap::new();
    for (entries, kind) in [
        (&file.providers, BinKind::Provider),
        (&file.drivers, BinKind::Driver),
    ] {
        for entry in entries {
            if let Some(bin) = &entry.bin {
                let argv = resolve_bin_argv(bin, &entry.name, root, home_dir)?;
                groups
                    .entry(argv)
                    .or_default()
                    .push((entry.name.clone(), kind));
            }
        }
    }
    Ok(groups)
}

/// Spawn every out-of-process plugin declared via a `bin:` config entry and
/// register a remote handle under each entry's name. Entries that resolve to the
/// identical launch command share one spawned process + connection (so the `go`
/// provider and its `go_*` drivers, all pointing at the same binary, run in one
/// process). plugin-go/exec stay Rust binaries — `bin.exec: [cargo, run, ...]`
/// or `bin.path: /usr/local/bin/heph-plugin-go` is how they are now launched.
#[cfg(unix)]
fn register_bin_plugins(
    e: &mut engine::Engine,
    root: &std::path::Path,
    home_dir: &std::path::Path,
    file: &config_yaml::ConfigYaml,
) -> anyhow::Result<()> {
    // Group entries by resolved argv so identical launch commands collapse to one
    // process (deterministic order via BTreeMap).
    let groups = plan_bin_groups(file, root, home_dir)?;
    if groups.is_empty() {
        return Ok(());
    }

    // Spawned plugins discover the workspace root through this env var.
    let env = vec![(
        "HEPH_PLUGIN_ROOT".to_string(),
        root.to_string_lossy().into_owned(),
    )];

    for (argv, members) in groups {
        let (program, args) = argv
            .split_first()
            .context("internal: empty plugin argv after resolution")?;
        // Cold protocol over a UDS socketpair (fd 3). For native-speed in-process
        // plugins use `dylib:` (the stable ABI) instead; this `bin:` path is the
        // portable out-of-process transport.
        let ((r, w), child) =
            hplugin_remote::spawn_streams(std::path::Path::new(program), args, &env)
                .with_context(|| format!("spawn plugin `{program}`"))?;
        drop(child);
        let plugin = hplugin_remote::RemotePlugin::connect(r, w);
        for (name, kind) in members {
            match kind {
                BinKind::Provider => {
                    let p = plugin.clone();
                    let nm = name.clone();
                    e.register_provider_factory(&name, move |_init, _opts| {
                        Ok(Box::new(p.provider(nm)))
                    })?;
                }
                BinKind::Driver => {
                    let p = plugin.clone();
                    let nm = name.clone();
                    e.register_managed_driver_factory(&name, move |_init, _opts| {
                        Ok(Box::new(p.managed_driver(nm)))
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// Load + register every `dylib:` plugin. One cdylib per distinct path is loaded
/// in-process behind the stable ABI (ABI-checked via stabby type reports), and the
/// provider + drivers it exports are registered under their own names. The engine
/// then drives them through its normal traits at native speed.
#[cfg(unix)]
fn register_dylib_plugins(
    e: &mut engine::Engine,
    root: &std::path::Path,
    home_dir: &std::path::Path,
    file: &config_yaml::ConfigYaml,
) -> anyhow::Result<()> {
    use std::collections::BTreeSet;

    let mut paths: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    for en in file.providers.iter().chain(file.drivers.iter()) {
        if let Some(d) = &en.dylib {
            paths.insert(resolve_artifact_path(d.resolve(&en.name)?, root, home_dir)?);
        }
    }

    let root_str = root.to_string_lossy().into_owned();
    for path in paths {
        let (provider, drivers) = hplugin_stabby::load_stable::load(&path, &root_str)
            .with_context(|| format!("load plugin dylib {}", path.display()))?;
        if let Some(p) = provider {
            let name = p.name().to_string();
            e.register_provider_factory(&name, move |_init, _opts| Ok(Box::new(p.clone())))?;
        }
        for (name, drv) in drivers {
            e.register_managed_driver_factory(&name, move |_init, _opts| {
                Ok(Box::new(drv.clone()))
            })?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn register_dylib_plugins(
    _e: &mut engine::Engine,
    _root: &std::path::Path,
    _home_dir: &std::path::Path,
    file: &config_yaml::ConfigYaml,
) -> anyhow::Result<()> {
    if file
        .providers
        .iter()
        .chain(file.drivers.iter())
        .any(|en| en.dylib.is_some())
    {
        anyhow::bail!("`dylib:` plugins are only supported on unix");
    }
    Ok(())
}

/// Resolve a loadable-artifact source to a concrete file path: a `path` is taken
/// relative to the workspace `root`; a `url` is downloaded + cached (same fetch
/// path as `bin:` urls).
#[cfg(unix)]
fn resolve_artifact_path(
    src: config_yaml::ArtifactSource,
    root: &std::path::Path,
    home_dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    Ok(match src {
        config_yaml::ArtifactSource::Path(p) => root.join(p),
        config_yaml::ArtifactSource::Url(u) => download_plugin(&u, home_dir)?,
    })
}

/// Load + register every `wasm:` plugin. One wasm component per distinct path is
/// loaded in-process via wasmtime (capability-sandboxed); the provider and/or
/// driver it exports register under the config entry names that point at it.
#[cfg(all(unix, feature = "wasm"))]
fn register_wasm_plugins(
    e: &mut engine::Engine,
    root: &std::path::Path,
    home_dir: &std::path::Path,
    file: &config_yaml::ConfigYaml,
) -> anyhow::Result<()> {
    use std::collections::BTreeMap;

    // path -> (provider entry name, driver entry name)
    let mut by_path: BTreeMap<std::path::PathBuf, (Option<String>, Option<String>)> =
        BTreeMap::new();
    for en in &file.providers {
        if let Some(w) = &en.wasm {
            let p = resolve_artifact_path(w.resolve(&en.name)?, root, home_dir)?;
            by_path.entry(p).or_default().0 = Some(en.name.clone());
        }
    }
    for en in &file.drivers {
        if let Some(w) = &en.wasm {
            let p = resolve_artifact_path(w.resolve(&en.name)?, root, home_dir)?;
            by_path.entry(p).or_default().1 = Some(en.name.clone());
        }
    }

    for (path, (prov, drv)) in by_path {
        let bytes =
            std::fs::read(&path).with_context(|| format!("read wasm plugin {}", path.display()))?;
        let plugin = hplugin_remote::wasm::WasmPlugin::load(
            &bytes,
            prov.clone().unwrap_or_default(),
            drv.clone().unwrap_or_default(),
        )
        .with_context(|| format!("load wasm plugin {}", path.display()))?;
        if let Some(name) = prov {
            let pl = std::sync::Arc::clone(&plugin);
            e.register_provider_factory(&name, move |_init, _opts| Ok(Box::new(pl.provider())))?;
        }
        if let Some(name) = drv {
            let pl = std::sync::Arc::clone(&plugin);
            e.register_driver_factory(&name, move |_init, _opts| Ok(Box::new(pl.driver())))?;
        }
    }
    Ok(())
}

#[cfg(not(all(unix, feature = "wasm")))]
fn register_wasm_plugins(
    _e: &mut engine::Engine,
    _root: &std::path::Path,
    _home_dir: &std::path::Path,
    file: &config_yaml::ConfigYaml,
) -> anyhow::Result<()> {
    if file
        .providers
        .iter()
        .chain(file.drivers.iter())
        .any(|en| en.wasm.is_some())
    {
        anyhow::bail!("`wasm:` plugins require building heph with the `wasm` feature on unix");
    }
    Ok(())
}

#[cfg(not(unix))]
fn register_bin_plugins(
    _e: &mut engine::Engine,
    _root: &std::path::Path,
    _home_dir: &std::path::Path,
    file: &config_yaml::ConfigYaml,
) -> anyhow::Result<()> {
    if file
        .providers
        .iter()
        .chain(file.drivers.iter())
        .any(|en| en.bin.is_some())
    {
        anyhow::bail!("out-of-process `bin:` plugins are only supported on unix");
    }
    Ok(())
}

/// Resolve a [`config_yaml::BinConfig`] to a spawnable argv (`argv[0]` is the
/// program). A relative `path` is resolved against the workspace `root` (so it
/// doesn't depend on the process cwd); `exec` is used as-is (its program is
/// PATH-resolved at spawn); `url` is downloaded + cached first.
#[cfg(unix)]
fn resolve_bin_argv(
    bin: &config_yaml::BinConfig,
    ctx: &str,
    root: &std::path::Path,
    home_dir: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    Ok(match bin.resolve(ctx)? {
        config_yaml::BinSource::Path(p) => {
            let pb = std::path::PathBuf::from(&p);
            let abs = if pb.is_absolute() { pb } else { root.join(pb) };
            vec![abs.to_string_lossy().into_owned()]
        }
        config_yaml::BinSource::Exec(argv) => argv,
        config_yaml::BinSource::Url(url) => {
            vec![
                download_plugin(&url, home_dir)?
                    .to_string_lossy()
                    .into_owned(),
            ]
        }
    })
}

/// Substitute `{os}`/`{arch}` in a plugin download URL with the values used in
/// published artifact names: os `linux`/`darwin` and arch `amd64`/`arm64`
/// (mapped from the rust `std::env::consts` spellings `macos` and
/// `x86_64`/`aarch64`). Unmapped hosts fall back to the raw consts value.
fn substitute_os_arch(url: &str) -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    url.replace("{os}", os).replace("{arch}", arch)
}

/// Download a plugin binary from `url_tmpl` (after `{os}`/`{arch}` substitution),
/// cache it under `<home>/plugins/<os>-<arch>/`, make it executable, and return
/// its path. A previously-downloaded binary is reused (no re-fetch).
#[cfg(unix)]
fn download_plugin(
    url_tmpl: &str,
    home_dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let url = substitute_os_arch(url_tmpl);
    let filename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("plugin");
    let dir = home_dir.join("plugins").join(format!(
        "{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    let dest = dir.join(filename);
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    // reqwest::blocking spins up its own runtime; run it on a dedicated std
    // thread so it is safe to call from within the async runtime new_engine runs
    // on (a nested block_on would otherwise panic).
    let url_for_thread = url.clone();
    let bytes = std::thread::spawn(move || -> anyhow::Result<Vec<u8>> {
        let resp = reqwest::blocking::get(&url_for_thread)
            .with_context(|| format!("GET {url_for_thread}"))?
            .error_for_status()
            .with_context(|| format!("GET {url_for_thread}"))?;
        Ok(resp.bytes()?.to_vec())
    })
    .join()
    .map_err(|_e| anyhow::anyhow!("plugin download thread panicked"))??;

    // Write to a temp path then rename so a partial download is never seen as a
    // usable binary by a concurrent run.
    let tmp = dir.join(format!(".{filename}.download"));
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(&bytes)?;
        f.flush()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("install plugin to {}", dest.display()))?;
    Ok(dest)
}
