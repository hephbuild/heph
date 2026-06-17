use std::sync::{Arc, Weak};
use std::thread::available_parallelism;

use anyhow::Context;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

use crate::engine::config_yaml;
use crate::{engine, pluginbuildfile, pluginexec, pluginhostbin, pluginnix, plugintextfile};

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
    // registers the provider + drivers it exports.
    register_plugins(&mut e, &root, &home_dir, &file)?;

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

/// Apply every `plugins:` entry. `builtin:` entries instantiate the matching
/// in-process factory; `path:`/`url:` entries resolve a `*-plugin.json` manifest,
/// load the cdylib it names for this host, and register the provider + drivers it
/// exports (directly — the plugin self-describes its names via `config()`).
fn register_plugins(
    e: &mut engine::Engine,
    root: &std::path::Path,
    home_dir: &std::path::Path,
    file: &config_yaml::ConfigYaml,
) -> anyhow::Result<()> {
    let mut manifests: Vec<(config_yaml::PluginSource, Vec<u8>)> = Vec::new();
    for spec in &file.plugins {
        match spec.resolve()? {
            config_yaml::PluginSource::Builtin(name) => e
                .apply_builtin(&name, &spec.options)
                .with_context(|| format!("apply builtin plugin `{name}`"))?,
            // Encode options once (pb::Value bytes) for the cdylib's create entry.
            other => manifests.push((
                other,
                hplugin_abi::convert::options_to_pb_bytes(&spec.options),
            )),
        }
    }
    load_dylib_plugins(e, root, home_dir, manifests)
}

/// One plugin distribution manifest (`*-plugin.json`): a name + the dylib to load
/// per os/arch.
#[cfg(unix)]
#[derive(serde::Deserialize)]
struct PluginManifest {
    name: String,
    #[serde(default)]
    artifacts: Vec<ManifestArtifact>,
}

#[cfg(unix)]
#[derive(serde::Deserialize)]
struct ManifestArtifact {
    os: String,
    arch: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// Resolve each manifest to a host dylib, load it (parallel — loading constructs
/// the plugin, which isn't free), and register its exported components directly.
#[cfg(unix)]
fn load_dylib_plugins(
    e: &mut engine::Engine,
    root: &std::path::Path,
    home_dir: &std::path::Path,
    manifests: Vec<(config_yaml::PluginSource, Vec<u8>)>,
) -> anyhow::Result<()> {
    use rayon::prelude::*;

    if manifests.is_empty() {
        return Ok(());
    }
    let root_str = root.to_string_lossy().into_owned();
    let loaded = manifests
        .into_par_iter()
        .map(|(src, options_pb)| -> anyhow::Result<_> {
            let dylib = resolve_manifest_dylib(&src, root, home_dir)?;
            hplugin_stabby::load_stable::load(&dylib, &root_str, &options_pb)
                .with_context(|| format!("load plugin dylib {}", dylib.display()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Engine mutation is single-threaded; register the loaded components in order.
    for (provider, drivers) in loaded {
        if let Some(p) = provider {
            e.register_provider(|_| Box::new(p))?;
        }
        for (_name, drv) in drivers {
            e.register_managed_driver(|_| Box::new(drv))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn load_dylib_plugins(
    _e: &mut engine::Engine,
    _root: &std::path::Path,
    _home_dir: &std::path::Path,
    manifests: Vec<(config_yaml::PluginSource, Vec<u8>)>,
) -> anyhow::Result<()> {
    if manifests.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("`path:`/`url:` (cdylib) plugins are only supported on unix")
    }
}

/// Resolve a plugin manifest source to the concrete dylib path for this host:
/// load the manifest (local read or download), pick the artifact matching the
/// host os/arch, then resolve that artifact (a local `path` sibling of the
/// manifest, or a `url` to download + cache).
#[cfg(unix)]
fn resolve_manifest_dylib(
    src: &config_yaml::PluginSource,
    root: &std::path::Path,
    home_dir: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let (manifest_bytes, manifest_dir) = match src {
        config_yaml::PluginSource::ManifestPath(p) => {
            let pb = std::path::PathBuf::from(p);
            let mp = if pb.is_absolute() { pb } else { root.join(pb) };
            let bytes = std::fs::read(&mp)
                .with_context(|| format!("read plugin manifest {}", mp.display()))?;
            let dir = mp
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| root.to_path_buf());
            (bytes, dir)
        }
        config_yaml::PluginSource::ManifestUrl(u) => {
            let mp = download_plugin(u, home_dir)?;
            let bytes = std::fs::read(&mp)
                .with_context(|| format!("read downloaded plugin manifest {}", mp.display()))?;
            let dir = mp
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_else(|| home_dir.to_path_buf());
            (bytes, dir)
        }
        config_yaml::PluginSource::Builtin(_) => {
            anyhow::bail!("internal: builtin plugin reached manifest resolution")
        }
    };

    let manifest: PluginManifest =
        serde_json::from_slice(&manifest_bytes).context("parse plugin manifest json")?;
    let (os, arch) = host_os_arch();
    let art = manifest
        .artifacts
        .iter()
        .find(|a| a.os == os && a.arch == arch)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "plugin `{}` has no artifact for this host ({os}/{arch})",
                manifest.name
            )
        })?;
    match (&art.path, &art.url) {
        (Some(p), None) => {
            let pb = std::path::PathBuf::from(p);
            Ok(if pb.is_absolute() {
                pb
            } else {
                manifest_dir.join(pb)
            })
        }
        (None, Some(u)) => download_plugin(u, home_dir),
        _ => anyhow::bail!(
            "plugin `{}` artifact for {os}/{arch} must set exactly one of `path`/`url`",
            manifest.name
        ),
    }
}

/// Host os/arch in the published-artifact spelling (`darwin`/`linux`,
/// `amd64`/`arm64`), matching the manifest's `artifacts[].{os,arch}`.
#[cfg(unix)]
fn host_os_arch() -> (String, String) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    (os.to_string(), arch.to_string())
}

/// An exclusive, advisory, cross-process file lock (`flock(2)`), released on drop.
/// Serializes concurrent plugin downloads of the same artifact across heph
/// processes so two runs don't fetch (or half-write) the same file at once.
#[cfg(unix)]
struct FileLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `self.file` owns a valid fd for the lifetime of this guard.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(unix)]
fn lock_exclusive(path: &std::path::Path) -> anyhow::Result<FileLock> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open lock file {}", path.display()))?;
    // Blocking exclusive acquire; advisory and tied to the open file description.
    // SAFETY: `file` owns the fd; `flock` is valid for the call's duration.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("flock LOCK_EX on {}", path.display()));
    }
    Ok(FileLock { file })
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

    // Serialize concurrent downloads of this artifact (across processes too): hold
    // an exclusive lock for the fetch, then re-check — another run may have just
    // installed it while we waited.
    let _lock = lock_exclusive(&dir.join(format!("{filename}.lock")))?;
    if dest.exists() {
        return Ok(dest);
    }

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

        // The helper exercises built-in plugins only (no cdylib loading).
        for spec in &file.plugins {
            match spec.resolve()? {
                config_yaml::PluginSource::Builtin(name) => {
                    e.apply_builtin(&name, &spec.options)?
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

    #[test]
    fn substitute_os_arch_replaces_placeholders() {
        let out = super::substitute_os_arch("https://x/heph-{os}-{arch}.bin");
        assert!(!out.contains("{os}") && !out.contains("{arch}"), "{out}");
        // arch uses the published spelling (amd64/arm64), never the rust consts.
        assert!(!out.contains("x86_64") && !out.contains("aarch64"), "{out}");
        // os uses darwin, never the rust `macos` spelling.
        assert!(!out.contains("macos"), "{out}");
    }
}
