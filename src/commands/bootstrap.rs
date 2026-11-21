use std::sync::{Arc, Weak};
use std::thread::available_parallelism;

use anyhow::Context;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

use crate::engine::config::ConfigYamlExt;
use crate::engine::config_yaml;
use crate::tui::session::SessionSlot;
use crate::{
    engine, pluginbuildfile, pluginexec, pluginhostbin, pluginhttp, pluginnix, plugintemplate,
    plugintextfile,
};

/// Builds the multi-thread runtime used by every command entry point.
///
/// `max_blocking_threads` is sized generously above the engine's `2 × parallelism`
/// execute permits, since every `block_in_place` site parks a runtime worker and
/// spawns a replacement from the blocking pool. Default `512` is enough today but
/// silently degrades to inline execution if saturated; the explicit `8 × N + 64`
/// gives clear headroom without ever hitting the cliff.
///
/// The heavy synchronous work — cache writes, zstd, starlark evaluation, the Go
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
/// `fut` to completion, drains the hooks of every engine the command built
/// ([`HookDrain`]), and lets the runtime drop normally at end of scope.
///
/// Critical teardown — FUSE unmount, SQLite flush, sandbox rmdir (all in
/// `Engine::drop`), plus the sandbox-cleanup queue the renderer waits on — has
/// already run *inside* `fut`: the last `Arc<Engine>` lives in the app task, so
/// its drop completes before `fut` resolves. The runtime drop only joins idle
/// worker/blocking threads after that.
pub fn block_on<F: Future>(fut: F) -> anyhow::Result<F::Output> {
    block_on_draining(fut, &HOOKS)
}

/// [`block_on`] over a given [`HookDrain`], so a test can use its own.
fn block_on_draining<F: Future>(fut: F, hooks: &HookDrain) -> anyhow::Result<F::Output> {
    let runtime = build_runtime().context("build tokio runtime")?;
    let out = runtime.block_on(fut);
    // On this runtime, not after it: a cdylib hook's delivery task was
    // spawned here, and dropping the runtime would cancel it.
    runtime.block_on(hooks.drain(HOOK_DRAIN_BUDGET));
    Ok(out)
}

/// The hooks of every engine [`new_engine`] built in this process.
static HOOKS: HookDrain = HookDrain::new();

/// How long a command's exit waits on its hooks' final writes.
///
/// The gha hook bounds its own close-time sync to 10s; this is the ceiling for
/// a hook that bounds nothing, so a hung plugin delays the exit rather than
/// holding it forever.
const HOOK_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Hooks to drain when the command ends.
///
/// A request's end only *signals* its hooks (`on_close`); an out-of-process
/// hook's final write — the gha plugin's last comment edit — lands after that,
/// and only [`Hook::drain`](engine::hook::Hook::drain) waits for it. Every
/// command used to exit without that wait except `heph run`, so the comment of
/// a `heph query` or `heph inspect` kept whatever it said before its last edit
/// was cut off.
///
/// Held here rather than reached through the engine: the engine has dropped by
/// the time the command's future resolves. Holding a hook past its engine is
/// safe because a plugin's library is never unloaded (`load_stable` leaks it).
pub(crate) struct HookDrain(std::sync::Mutex<Vec<Arc<dyn engine::hook::Hook>>>);

impl HookDrain {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(Vec::new()))
    }

    fn add(&self, hooks: impl IntoIterator<Item = Arc<dyn engine::hook::Hook>>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(hooks);
    }

    /// Await every added hook's drain, within `budget` overall. Each hook is
    /// drained once: they are taken out as they are drained.
    async fn drain(&self, budget: std::time::Duration) {
        let hooks = std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if hooks.is_empty() {
            return;
        }
        let all = futures::future::join_all(hooks.iter().map(|h| h.drain()));
        if tokio::time::timeout(budget, all).await.is_err() {
            tracing::warn!(
                budget_s = budget.as_secs(),
                hooks = ?hooks.iter().map(|h| h.name()).collect::<Vec<_>>(),
                "a hook did not finish its final write in time; exiting without it"
            );
        }
    }
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

/// How the exec/bash drivers get the builtin-utility shim directory.
///
/// The heph home is the host's knowledge, and materializing the shims is the
/// `coreutils` crate's — so the driver is handed a closure and needs neither.
/// Deferred rather than eager so a command that never runs a target (`query`,
/// `inspect`) never touches the filesystem for it.
fn coreutils_shims(home_dir: &std::path::Path) -> pluginexec::CoreutilsShims {
    let home = home_dir.to_path_buf();
    Arc::new(move || {
        let exe = std::env::current_exe()
            .context("locate the heph binary the builtin-utility shims point at")?;
        hcoreutils::shim_dir(&home, &exe)
    })
}

/// Build the engine for this invocation.
///
/// Takes nothing from the CLI. Everything a *run* can switch — `--force`,
/// `--shell`, `--frozen`, `--no-scratch` — rides on `ResultOptions` rather than
/// on the engine, so one engine serves an ordinary request and an audit request
/// without being rebuilt, and each switch has exactly one source of truth. Only
/// workspace-level policy reaches `Config`.
/// The always-on builtins the binary adds beside the engine's own (`fs`,
/// `group`, `scratch`, `auth`, `query`): each one plugin, named after itself.
/// None takes options. The `go`, `oci`, `devenv` and `gha` plugins are not
/// compiled in: they ship as cdylibs loaded from a `path:`/`url:` manifest
/// entry, named by that manifest.
pub fn register_builtin_plugins(
    e: &mut engine::Engine,
    home_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use engine::PluginParts;
    e.register_plugin(pluginhostbin::PLUGIN_NAME, |_| {
        Ok(PluginParts::default()
            .with_provider(Box::new(pluginhostbin::Provider))
            .with_driver(Box::new(pluginhostbin::Driver)))
    })?;
    e.register_plugin(plugintextfile::DRIVER_NAME, |_| {
        Ok(PluginParts::default().with_driver(Box::new(plugintextfile::Driver)))
    })?;
    e.register_plugin(plugintemplate::DRIVER_NAME, |_| {
        Ok(PluginParts::default().with_driver(Box::new(plugintemplate::Driver)))
    })?;
    // `http_fetch`: downloads a URL (templated over the target's addr args) into a
    // cacheable file output — how tool binaries are provisioned off the internet.
    e.register_plugin(pluginhttp::DRIVER_NAME, |_| {
        Ok(PluginParts::default().with_managed_driver(Box::new(pluginhttp::Driver)))
    })?;
    let nix_dir = home_dir.join("nix-driver");
    e.register_plugin(pluginnix::DRIVER_NAME, |_| {
        Ok(PluginParts::default().with_managed_driver(Box::new(pluginnix::Driver::new(nix_dir))))
    })?;
    Ok(())
}

/// Opt-in built-in factories — instantiated only when a `plugins: - { builtin:
/// <name> }` entry selects them.
///
/// Public so a config generated elsewhere (`heph-bench`'s Tier B corpus) can be
/// checked against the set the binary actually accepts.
pub fn register_builtin_factories(
    e: &mut engine::Engine,
    home_dir: &std::path::Path,
) -> anyhow::Result<()> {
    register_builtin_factories_with(e, home_dir, pluginbuildfile::ShadowedChecks::default())
}

/// [`register_builtin_factories`], with the buildfile provider recording its
/// shadowed check targets into `shadowed`, which `heph validate` reads.
/// Public so `crates/e2e` builds the buildfile plugin through this factory
/// and reads the same record.
pub fn register_builtin_factories_with(
    e: &mut engine::Engine,
    home_dir: &std::path::Path,
    shadowed: pluginbuildfile::ShadowedChecks,
) -> anyhow::Result<()> {
    use engine::PluginParts;
    e.register_plugin_factory("buildfile", move |init, opts| {
        // The provider (with the check targets) and the drivers that run
        // them, from one decode of the options.
        let parts = pluginbuildfile::plugin_parts(
            pluginbuildfile::PartsInit {
                root: init.root.to_path_buf(),
                skip_dirs: &init.skip_dirs,
                skip_globs: &init.skip_globs,
                runtime: init.runtime.clone(),
                walker: init.walker.clone(),
                // `heph.<plugin>.<fn>`: read from this engine's registry once
                // sealed, before the first BUILD evaluation.
                functions: Arc::clone(&init.functions),
                // `heph.types.<driver>.Kwargs`: from this engine's driver
                // schemas, sealed with the function registry.
                drivers: Arc::clone(&init.drivers),
                shadowed: shadowed.clone(),
            },
            opts,
        )?;
        Ok(parts.drivers.into_iter().fold(
            PluginParts::default().with_provider(Box::new(parts.provider)),
            PluginParts::with_managed_driver,
        ))
    })?;
    // The exec and bash drivers take a closure rather than the path itself, so
    // `plugin-exec` — which most of the workspace links — does not have to
    // depend on the forty utility crates to know it has a directory to put on
    // PATH. Resolved lazily, so a `heph query` never touches the filesystem.
    let (coreutils_exec, coreutils_bash) = (coreutils_shims(home_dir), coreutils_shims(home_dir));
    e.register_plugin_factory("exec", move |_init, opts| {
        Ok(PluginParts::default().with_managed_driver(Box::new(
            pluginexec::Driver::from_options_exec(opts)?
                .with_coreutils(hcoreutils::COREUTILS_VERSION, coreutils_exec.clone()),
        )))
    })?;
    e.register_plugin_factory("bash", move |_init, opts| {
        Ok(PluginParts::default().with_managed_driver(Box::new(
            pluginexec::Driver::from_options_bash(opts)?
                .with_coreutils(hcoreutils::COREUTILS_VERSION, coreutils_bash.clone()),
        )))
    })?;
    Ok(())
}

/// Whether `HEPH_NO_LISTED_FACTS` turns listed facts off: off when unset, empty,
/// `0`, or (any case) `false`/`no`/`off`; on for anything else, so a value
/// that is not plainly "off" errs toward resolving every candidate.
fn listed_facts_kill_switch(v: Option<&std::ffi::OsStr>) -> bool {
    let Some(v) = v else { return false };
    let Some(s) = v.to_str() else { return true };
    let s = s.trim();
    !(s.is_empty()
        || s == "0"
        || ["false", "no", "off"]
            .iter()
            .any(|off| s.eq_ignore_ascii_case(off)))
}

/// Build the engine for one command. `global` is the command line's global
/// options; the engine reads its pool overrides (`-j`, `--resource`) from it.
/// The workspace it resolves is recorded in `session`, the process's session
/// threaded from `main`, for the `meta.json` of the session a later `run_app`
/// opens. Only workspace fields are stored: a command that never reaches
/// `run_app` opens no session. The engine announces each run's live log to
/// the same slot, recorded once a session is open.
pub fn new_engine(
    global: &super::GlobalOptions,
    session: &Arc<SessionSlot>,
) -> anyhow::Result<(Arc<engine::Engine>, ShutdownTrigger)> {
    new_engine_with(
        global,
        BadEnv::Refuse,
        Some(session),
        pluginbuildfile::ShadowedChecks::default(),
    )
}

/// [`new_engine`] for `heph validate`: also returns the record the buildfile
/// provider keeps of every check target a package's own target shadows (I2),
/// filled as the provider lists each package.
pub fn new_validate_engine(
    global: &super::GlobalOptions,
    session: &Arc<SessionSlot>,
) -> anyhow::Result<(
    Arc<engine::Engine>,
    ShutdownTrigger,
    pluginbuildfile::ShadowedChecks,
)> {
    new_validate_engine_at(&workspace_root()?, global, Some(session))
}

/// [`new_validate_engine`] over the workspace at `root` instead of the one
/// the cwd resolves to (`get_root` is memoized from the cwd), so a test can
/// build validate's engine through the same path.
pub fn new_validate_engine_at(
    root: &std::path::Path,
    global: &super::GlobalOptions,
    session: Option<&Arc<SessionSlot>>,
) -> anyhow::Result<(
    Arc<engine::Engine>,
    ShutdownTrigger,
    pluginbuildfile::ShadowedChecks,
)> {
    let shadowed = pluginbuildfile::ShadowedChecks::default();
    let (engine, trigger) = new_engine_at(root, global, BadEnv::Refuse, session, shadowed.clone())?;
    Ok((engine, trigger, shadowed))
}

/// The workspace root resolved from the cwd.
fn workspace_root() -> anyhow::Result<std::path::PathBuf> {
    match engine::get_root() {
        Ok(r) => Ok(r),
        Err(inner) => anyhow::bail!("Error: {}", inner),
    }
}

/// The engine for a caller with no command line of its own: shell
/// completion and the credential helpers, which a shell or another tool runs
/// and which execute no target. A malformed `HEPH_RESOURCE_<NAME>` is ignored
/// with a warning there rather than breaking `<TAB>` or `git push`; the next
/// real command still refuses it. They record no session.
pub fn new_helper_engine() -> anyhow::Result<(Arc<engine::Engine>, ShutdownTrigger)> {
    new_engine_with(
        &super::GlobalOptions::default(),
        BadEnv::Ignore,
        None,
        pluginbuildfile::ShadowedChecks::default(),
    )
}

/// An engine over `root`'s home that only reads what other hephs wrote
/// there, for `heph session attach`'s log and output readers: the config
/// resolved from `root` (not the cwd), no plugin loaded, no hook, no SIGINT
/// handler (attach's own Ctrl-C detaches, and must not cancel or hard-exit
/// through an engine's), no session. Returns the home it resolved, which
/// the caller checks against the one the session recorded.
pub fn new_reader_engine(
    global: &super::GlobalOptions,
    root: &std::path::Path,
) -> anyhow::Result<(Arc<engine::Engine>, std::path::PathBuf)> {
    let (_file, config) = engine_config(root, global, BadEnv::Ignore, std::env::vars_os())
        .context("resolving the recorded workspace's config")?;
    let home = config.home_dir.to_path_buf();
    let engine = engine::Engine::new(config).context("building a reader engine")?;
    Ok((Arc::new(engine), home))
}

/// What a malformed `HEPH_RESOURCE_<NAME>` does to engine construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BadEnv {
    Refuse,
    Ignore,
}

/// Lay `vars`' `HEPH_RESOURCE_<NAME>` and `global`'s flags over `pools`. With
/// [`BadEnv::Ignore`], an environment layer that does not parse is dropped
/// whole (with a warning) and the flags alone are applied.
fn apply_run_overrides(
    pools: &mut std::collections::BTreeMap<hplugin::provider::PoolName, hcore::capacity::PoolSpec>,
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    global: &super::GlobalOptions,
    bad_env: BadEnv,
) -> anyhow::Result<()> {
    let flags = global.resource_overrides()?;
    let layered = super::global::env_resource_overrides(vars)
        .context("reading HEPH_RESOURCE_<NAME>")
        .and_then(|env| {
            let mut with_env = pools.clone();
            super::global::apply_resource_overrides(&mut with_env, env, &flags)
                .context("applying the run's resource overrides")?;
            Ok(with_env)
        });
    match (layered, bad_env) {
        (Ok(with_env), _) => *pools = with_env,
        (Err(e), BadEnv::Refuse) => return Err(e),
        (Err(e), BadEnv::Ignore) => {
            tracing::warn!(error = %format!("{e:#}"), "ignoring HEPH_RESOURCE_<NAME>");
            super::global::apply_resource_overrides(pools, Default::default(), &flags)
                .context("applying the run's resource overrides")?;
        }
    }
    Ok(())
}

/// The engine's runtime config for one command: the workspace's config file
/// under `root`, with the run's own choices (the command line, `vars`' and the
/// process's environment) laid over it. What [`new_engine_with`] hands
/// `Engine::new`, and the config file it read, for the plugins.
fn engine_config(
    root: &std::path::Path,
    global: &super::GlobalOptions,
    bad_env: BadEnv,
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> anyhow::Result<(config_yaml::ConfigYaml, engine::Config)> {
    // The config file is the all-optional, profile-layered YAML; `resolve`
    // applies every default in one place and yields the engine's runtime config.
    let file = config_yaml::load_from_root(root)?;
    let mut config = file.resolve(root)?;

    // The kill switch for listed facts: every candidate a fact would have
    // decided is resolved instead. Read here, never by the engine. It is
    // outside every cache key on purpose: a query target's dep list is already
    // in its def hash, so a key moves only when trust changed the dep set —
    // which only a lying listed fact can do — and a trust marker there would
    // only split keys that are equal.
    config.listed_facts_trust = engine::listed::ListedFactsTrust::from_kill_switch(
        listed_facts_kill_switch(std::env::var_os("HEPH_NO_LISTED_FACTS").as_deref()),
    );

    // A run's choice, like `--fail-fast`: never in the config file.
    config.keep_failed_sandboxes = global.keep_failed_sandboxes;

    // Pool capacities: flag over `HEPH_RESOURCE_<NAME>` over the config file;
    // the engine supplies the default for a pool none of them names. Read here,
    // like the kill switch above, so the engine never looks at the environment.
    // Parsed against the config's units, which is why they wait until now.
    apply_run_overrides(&mut config.resources, vars, global, bad_env)?;
    Ok((file, config))
}

fn new_engine_with(
    global: &super::GlobalOptions,
    bad_env: BadEnv,
    session: Option<&Arc<SessionSlot>>,
    shadowed: pluginbuildfile::ShadowedChecks,
) -> anyhow::Result<(Arc<engine::Engine>, ShutdownTrigger)> {
    new_engine_at(&workspace_root()?, global, bad_env, session, shadowed)
}

fn new_engine_at(
    root: &std::path::Path,
    global: &super::GlobalOptions,
    bad_env: BadEnv,
    session: Option<&Arc<SessionSlot>>,
    shadowed: pluginbuildfile::ShadowedChecks,
) -> anyhow::Result<(Arc<engine::Engine>, ShutdownTrigger)> {
    let (file, config) = engine_config(root, global, bad_env, std::env::vars_os())?;

    // Point `SIGQUIT` dumps at the resolved home as soon as it is known — before
    // the engine is built and plugins are downloaded and loaded, which is where
    // a startup hang is likeliest — so they land beside the stall log and the
    // in-flight report rather than in the temp-dir fallback. Every command routes
    // through here, so every command gets it.
    crate::diag::set_dump_dir(&config.home_dir);
    // The session a later `run_app` opens records which workspace it ran in,
    // and the cwd heph resolved it from (`HEPH_CWD` when set).
    let cwd = engine::get_cwd().context("resolving the working directory")?;
    if let Some(session) = session {
        session.set_workspace(&cwd, root, &config.home_dir);
    }

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
    // Each run's live log becomes a `RunLog` record of the session, so
    // another terminal can follow it. The slot carries no session id to the
    // engine (I1).
    if let Some(session) = session {
        e.set_run_log_announcer(Arc::clone(session) as _);
    }

    // Always on, never gated. The diagnostic that needs a flag is the one you did
    // not pass on the run that hung — the whole reason this exists. Its `on_event`
    // is relaxed atomics only, so a build that never stalls pays a handful of
    // instructions per event and nothing else.
    e.register_hook(Arc::new(engine::diag::DiagHook::new(Arc::clone(
        engine::diag::global(),
    ))))?;

    // `fs` (provider + driver) is registered by `Engine::new` itself, with the
    // engine's own skip dirs. The remaining built-ins have no config.
    register_builtin_plugins(&mut e, &home_dir)?;
    register_builtin_factories_with(&mut e, &home_dir, shadowed)?;

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
    engine.install_exec_runner_host();

    // Telemetry: record the enabled provider + driver type names (built-ins plus
    // whatever the config turned on) and the remote-cache count for the exit
    // reporter. Set-once.
    crate::telemetry::record_plugins(
        engine.providers_by_name.keys().cloned().collect(),
        engine.drivers_by_name.keys().cloned().collect(),
        remote_cache_backends,
    );

    // Drained when the command ends, whichever command it is.
    HOOKS.add(engine.hooks());

    let (trigger, rx) = ShutdownTrigger::new();
    spawn_sigint_producer(trigger.clone());
    spawn_shutdown_handler(Arc::downgrade(&engine), rx);
    Ok((engine, trigger))
}

/// Bridge OS SIGINT into the shutdown trigger. The terminal may swallow
/// Ctrl-C while the TUI is in raw mode (ISIG cleared), in which case this
/// task never fires and the TUI's key handler drives the trigger instead.
/// Both producers are gated by the same `SuppressionHandle`.
///
/// The listener is armed here, before the task is spawned: `ctrl_c()` arms
/// nothing until it is first polled, so a spawned task that had not run yet
/// left SIGINT's default action in place, and a ctrl-c pressed once the
/// command was already printing (`heph log -f`'s "waiting for a run to
/// start") killed heph by the signal instead of cancelling it.
fn spawn_sigint_producer(trigger: ShutdownTrigger) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(sigint) => sigint,
        Err(e) => {
            tracing::warn!(error = %e, "listening for ctrl-c; it will kill heph instead of cancelling");
            return;
        }
    };
    tokio::spawn(async move {
        while sigint.recv().await.is_some() {
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
        let mut presses = 0usize;
        while rx.recv().await.is_some() {
            // Gated at trigger time, not here: a press accepted before an
            // entrypoint was handed the terminal happened before its tool
            // existed, so it is heph's — a cancel, which the spawn's own
            // check then honours — even when this task reads it just after
            // the handover. Dropping it here would lose that Ctrl-C.
            //
            // After an entrypoint's tool exited (R27), a press cancels what heph
            // is still finishing — uploads, cleanup — and nothing else: no
            // message, no hard abort, and the exit status stays the tool's.
            if hcore::shutdown::is_after_tool() {
                if let Some(e) = engine.upgrade() {
                    e.cancel_all_requests();
                }
                continue;
            }
            presses += 1;
            if presses == 1 {
                tracing::warn!(
                    "ctrl-c received, cancelling in-flight work (press ctrl-c again to abort)"
                );
                if let Some(e) = engine.upgrade() {
                    e.cancel_all_requests();
                }
            } else {
                tracing::error!("second ctrl-c, aborting");
                hard_abort(|code| std::process::exit(code));
            }
        }
    });
}

/// The relay's listener (E17, R9): every signal in
/// [`RELAYED_SIGNALS`](hexecrunner::foreground::RELAYED_SIGNALS) that reaches
/// heph is forwarded, as is and unfiltered, into the receiver
/// [`signal_relay`] returns, which `Engine::invoke` hands to the driver that
/// spawns an entrypoint. The driver decides what the terminal already
/// delivered (R2) — only it knows the tool's pid.
///
/// Two halves with two lifetimes. The channel exists from the start of the
/// invocation, so the request can carry its receiver; the listeners are
/// installed only by [`SignalRelay::install`], which the run's handover calls
/// right before the tool is spawned (N4a: the relay runs from the spawn).
/// Before that heph is an ordinary run: TERM and HUP end it, Ctrl-C cancels,
/// SIGQUIT dumps diagnostics and SIGUSR2 a profile — and nothing is queued
/// for a tool that does not exist yet, so a QUIT sent to inspect a slow dep
/// build cannot kill the tool later.
///
/// Once installed, heph no longer dies of TERM, HUP, USR1 or USR2 for the
/// rest of the process (a tokio handler is never uninstalled), so the
/// listeners stay and change what they do when the tool exits (R27): from
/// [`hcore::shutdown::tool_exited`] on — once no other invocation's tool still
/// runs — they forward nothing, and a TERM or HUP calls `after_tool`, which
/// cancels what heph is still finishing (uploads, cleanup) so it exits
/// promptly, with the tool's status. Ctrl-C reaches the same cancel through
/// the shutdown trigger.
pub struct SignalRelay {
    tx: engine::driver::SignalSender,
    after_tool: Arc<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for SignalRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalRelay").finish_non_exhaustive()
    }
}

/// The relay's channel, its listeners not yet installed. `after_tool` is
/// what a TERM or HUP does once the tool has exited.
pub fn signal_relay(
    after_tool: impl Fn() + Send + Sync + 'static,
) -> (SignalRelay, engine::driver::SignalReceiver) {
    let (tx, rx) = engine::driver::signal_channel();
    (
        SignalRelay {
            tx,
            after_tool: Arc::new(after_tool),
        },
        rx,
    )
}

impl SignalRelay {
    /// Install the listeners. Must be called on a thread inside the runtime
    /// (the handover fires on one).
    ///
    /// tokio arms the OS handler inside `signal()` before the listener it
    /// returns is subscribed, so a signal landing in that instant — a few
    /// microseconds per signal, once per invocation — is lost rather than
    /// relayed. Accepted: arming every handler up front would swallow TERM
    /// and HUP before the handover, which is worse.
    pub fn install(self) -> anyhow::Result<()> {
        use tokio::signal::unix::{SignalKind, signal};

        // `signal` panics outside a runtime; make that an error instead.
        let handle = tokio::runtime::Handle::try_current()
            .context("installing the signal relay outside the runtime")?;
        let _enter = handle.enter();
        let listeners = hexecrunner::foreground::RELAYED_SIGNALS
            .iter()
            .map(|&signo| {
                signal(SignalKind::from_raw(signo))
                    .map(|l| (signo, l))
                    .with_context(|| format!("listening for signal {signo} to relay it"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        for (signo, mut listener) in listeners {
            let tx = self.tx.clone();
            let after_tool = Arc::clone(&self.after_tool);
            handle.spawn(async move {
                while listener.recv().await.is_some() {
                    if hcore::shutdown::is_after_tool() && !hcore::shutdown::is_transparent() {
                        if signo == libc::SIGTERM || signo == libc::SIGHUP {
                            after_tool();
                        }
                        continue;
                    }
                    // A closed receiver: this invocation's tool is gone, and
                    // there is nothing to forward to.
                    let _forwarded = tx.send(engine::driver::Signal::from_raw(signo));
                }
            });
        }
        Ok(())
    }
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

    /// A hook whose final write is a task on the command's runtime, as a cdylib
    /// hook's delivery task is: `drain` waits for it.
    #[derive(Default)]
    struct SlowFlush {
        task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
        written: std::sync::atomic::AtomicBool,
        drains: std::sync::atomic::AtomicUsize,
    }

    impl engine::hook::Hook for SlowFlush {
        fn name(&self) -> String {
            "slow".to_string()
        }
        fn on_event(&self, _: &hcore::events::BuildEvent) {}
        fn on_close(&self) {}
        fn drain(&self) -> futures::future::BoxFuture<'_, ()> {
            Box::pin(async move {
                self.drains
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let task = self.task.lock().expect("lock").take();
                if let Some(t) = task {
                    t.await.expect("flush task");
                }
            })
        }
    }

    #[test]
    fn a_command_exits_only_after_its_hooks_final_write() {
        // Every command but `heph run` used to return without draining, so the
        // process could exit with a hook's last write still in flight: the gha
        // comment of a `heph query` kept its stale state.
        let hooks = HookDrain::new();
        let hook = Arc::new(SlowFlush::default());
        hooks.add([Arc::clone(&hook) as Arc<dyn engine::hook::Hook>]);
        let h = Arc::clone(&hook);
        block_on_draining(
            async move {
                // The request ends, its hook starts the final write, and the
                // command returns without waiting for it.
                let h2 = Arc::clone(&h);
                let task = tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    h2.written.store(true, std::sync::atomic::Ordering::SeqCst);
                });
                *h.task.lock().expect("lock") = Some(task);
            },
            &hooks,
        )
        .expect("runtime");
        assert!(hook.written.load(std::sync::atomic::Ordering::SeqCst));

        // Drained once: a second command in the same process does not wait on
        // the first one's hooks again.
        block_on_draining(async {}, &hooks).expect("runtime");
        assert_eq!(hook.drains.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_hook_that_never_finishes_does_not_hold_the_exit() {
        struct Hung;
        impl engine::hook::Hook for Hung {
            fn name(&self) -> String {
                "hung".to_string()
            }
            fn on_event(&self, _: &hcore::events::BuildEvent) {}
            fn on_close(&self) {}
            fn drain(&self) -> futures::future::BoxFuture<'_, ()> {
                Box::pin(futures::future::pending())
            }
        }
        let hooks = HookDrain::new();
        hooks.add([Arc::new(Hung) as Arc<dyn engine::hook::Hook>]);
        let runtime = build_runtime().expect("runtime");
        let started = std::time::Instant::now();
        runtime.block_on(hooks.drain(std::time::Duration::from_millis(100)));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// `--keep-failed-sandboxes` reaches the engine's config through the one
    /// function every command builds it with; without it, a failed target's
    /// sandbox is removed.
    #[test]
    fn keep_failed_sandboxes_reaches_the_engine_config() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(".hephconfig"), "").expect("write .hephconfig");
        let config = |global: &super::super::GlobalOptions| {
            engine_config(root.path(), global, BadEnv::Refuse, Vec::new())
                .expect("engine config")
                .1
        };
        assert!(!config(&super::super::GlobalOptions::default()).keep_failed_sandboxes);
        let keep = super::super::GlobalOptions {
            keep_failed_sandboxes: true,
            ..Default::default()
        };
        assert!(config(&keep).keep_failed_sandboxes);
    }

    #[test]
    fn listed_facts_kill_switch_values() {
        use std::ffi::OsStr;
        assert!(!listed_facts_kill_switch(None), "unset");
        for off in ["", "0", "false", "FALSE", "no", "Off"] {
            assert!(!listed_facts_kill_switch(Some(OsStr::new(off))), "{off:?}");
        }
        for on in ["1", "true", "yes"] {
            assert!(listed_facts_kill_switch(Some(OsStr::new(on))), "{on:?}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            assert!(
                listed_facts_kill_switch(Some(OsStr::from_bytes(b"\xff"))),
                "non-UTF-8"
            );
        }
    }

    /// A malformed `HEPH_RESOURCE_<NAME>` fails a command but is ignored,
    /// whole, by the helper engines (completion, credential helpers); a good
    /// one applies either way.
    #[test]
    fn bad_resource_env_is_ignored_only_by_helpers() {
        use hcore::capacity::Capacity;
        let vars = |pairs: &[(&str, &str)]| -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        let global = super::super::GlobalOptions::default();
        let gpu = || hplugin::provider::PoolName::try_from("gpu".to_string()).expect("pool");
        for bad in [
            &[("HEPH_RESOURCE_GPU", "abc")][..],
            &[("HEPH_RESOURCE_1GPU", "1")],
            &[("HEPH_RESOURCE_GPU", "2"), ("HEPH_RESOURCE_CPU", "lots")],
        ] {
            let mut pools = Default::default();
            assert!(
                apply_run_overrides(&mut pools, vars(bad), &global, BadEnv::Refuse).is_err(),
                "{bad:?}"
            );
            let mut pools = Default::default();
            apply_run_overrides(&mut pools, vars(bad), &global, BadEnv::Ignore)
                .unwrap_or_else(|e| panic!("{bad:?}: {e:#}"));
            assert!(pools.is_empty(), "{bad:?}: the whole layer is dropped");
        }
        for bad_env in [BadEnv::Refuse, BadEnv::Ignore] {
            let mut pools = Default::default();
            apply_run_overrides(
                &mut pools,
                vars(&[("HEPH_RESOURCE_GPU", "2")]),
                &global,
                bad_env,
            )
            .expect("good env");
            assert_eq!(
                pools.get(&gpu()).and_then(|s| s.capacity),
                Some(Capacity::Amount(2))
            );
        }
    }

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
        let config = file.resolve(dir.path())?;
        let home_dir = config.home_dir.clone();
        let mut e = engine::Engine::new(config)?;

        // `fs` is auto-registered by `Engine::new`.
        register_builtin_plugins(&mut e, &home_dir)?;
        register_builtin_factories(&mut e, &home_dir)?;

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

    /// The builtin rows of the spec's name map: every provider and driver a
    /// fully configured binary registers (before any cdylib), by the name a
    /// BUILD file or `provider_state` uses. A rename that is not in the map
    /// fails here; the cdylib rows are pinned in `bin-e2e`'s `plugin_dylib`.
    #[test]
    fn shipped_names_match_the_name_map() {
        let yaml = r#"
plugins:
  - builtin: buildfile
  - builtin: exec
  - builtin: bash
"#;
        let (_dir, e) = build_engine_from_yaml(yaml).expect("engine");
        fn sorted<'a>(names: impl Iterator<Item = &'a String>) -> Vec<&'a str> {
            let mut names: Vec<&str> = names.map(String::as_str).collect();
            names.sort_unstable();
            names
        }
        assert_eq!(
            sorted(e.providers_by_name.keys()),
            // No `auth`: its `heph.auth.*` functions are the plugin's, so it
            // needs no provider to carry them.
            ["buildfile", "fs", "hostbin", "query"]
        );
        assert_eq!(
            sorted(e.drivers_by_name.keys()),
            [
                "auth.credential",
                "bash",
                // The BUILD-file check targets' drivers.
                "buildfile.format",
                "buildfile.format_check",
                "buildfile.lint_check",
                "exec",
                "fs",
                "group",
                "hostbin",
                "http_fetch",
                "nix",
                "scratch",
                "template",
                "textfile",
            ]
        );
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
                parallelism: None,
                ..engine::Config::for_tests(root)
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
