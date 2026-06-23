use crate::engine::config::Config;
use crate::engine::config_yaml::{FuseConfig, Options};
use crate::engine::driver::Driver as SDKDriver;
use crate::engine::driver_managed::ManagedDriver as SDKManagedDriver;
use crate::engine::hook::Hook as SDKHook;
use crate::engine::local_cache::LocalCache;
use crate::engine::local_cache_mem::LocalCacheMem;
use crate::engine::local_cache_sqlite::LocalCacheSQLite;
use crate::engine::provider::Provider as SDKProvider;
use crate::engine::request_state::RequestState;
use crate::engine::result_lock::ResultLock;
use crate::engine::{driver, provider};
use anyhow::Context;
use hsandboxfuse as sandboxfuse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Semaphore;
use tracing::warn;

/// Context the engine injects when constructing any plugin (provider, driver, or
/// managed driver — whether registered directly or through a factory): the
/// workspace root plus the filesystem-skip config every tree walk must honor.
/// `fs.skip` entries are split by the engine into literal directories ([`skip_dirs`])
/// and glob patterns ([`skip_globs`]).
///
/// [`skip_dirs`]: PluginInit::skip_dirs
/// [`skip_globs`]: PluginInit::skip_globs
pub struct PluginInit {
    pub root: PathBuf,
    /// Absolute directories to prune by exact path: the heph home plus the
    /// literal (non-glob) `fs.skip` entries, resolved relative to the repo root.
    pub skip_dirs: Vec<PathBuf>,
    /// Workspace-relative `fs.skip` glob patterns (e.g. `**/node_modules/**`),
    /// matched against entry paths.
    pub skip_globs: Vec<String>,
    /// Shared cross-run filesystem-walk cache for tree-walking plugins.
    pub walker: Arc<hwalk::CachedWalker>,
}

/// True if `entry` contains wax glob metacharacters — used to split `fs.skip`
/// into literal directories vs glob patterns.
pub(crate) fn is_skip_glob(entry: &str) -> bool {
    entry.contains(['*', '?', '[', ']', '{', '}'])
}

/// Normalizes a `fs.skip` entry to a root-relative path: a leading `./` (the
/// "current dir" form, e.g. `./node_modules`) is dropped so it resolves the same
/// as the bare form.
pub(crate) fn normalize_skip(entry: &str) -> &str {
    entry.strip_prefix("./").unwrap_or(entry)
}

/// Factory args: the [`PluginInit`] context and the plugin's YAML options.
pub type ProviderFactory =
    Box<dyn FnOnce(&PluginInit, &Options) -> anyhow::Result<Box<dyn SDKProvider>> + Send + Sync>;
pub type DriverFactory =
    Box<dyn FnOnce(&PluginInit, &Options) -> anyhow::Result<Box<dyn SDKDriver>> + Send + Sync>;
pub type ManagedDriverFactory = Box<
    dyn FnOnce(&PluginInit, &Options) -> anyhow::Result<Box<dyn SDKManagedDriver>> + Send + Sync,
>;

pub struct Engine {
    pub(crate) cfg: Config,
    pub(crate) local_cache: Arc<dyn LocalCache>,
    /// Mem-only store for `tmp`/uncacheable revisions. Small entries live in
    /// memory and never touch the SQLite WAL; entries over the per-entry cap
    /// spill to `local_cache`. See [`LocalCacheTmp`].
    pub(crate) local_cache_tmp: Arc<dyn LocalCache>,
    /// Shared cross-run filesystem-walk cache (separate `fswalk.db`), handed to
    /// tree-walking plugins via [`PluginInit`].
    pub(crate) walker: Arc<hwalk::CachedWalker>,

    pub(crate) providers: Vec<Arc<Provider>>,
    pub providers_by_name: HashMap<String, Arc<Provider>>,
    pub(crate) drivers: Vec<Arc<Driver>>,
    pub drivers_by_name: HashMap<String, Arc<Driver>>,

    /// Registered build-event hooks. Fed every emitted `BuildEvent` (see
    /// `RequestState::emit`); unlike providers/drivers they are never queried,
    /// only observed. Usually empty (a cheap no-op on the emit hot path).
    pub(crate) hooks: Vec<Arc<dyn SDKHook>>,

    pub requests: Mutex<HashMap<String, Weak<RequestState>>>,
    pub home: PathBuf,
    pub(crate) result_semaphore: Arc<Semaphore>,
    /// Maximum concurrent executes (the `result_semaphore` permit count). Cached
    /// here so it can be announced to clients via a `MaxWorkers` build event
    /// without reaching into the semaphore (whose live `available_permits` only
    /// reflects the *free* count, not the configured max).
    pub(crate) max_workers: usize,

    pub(crate) provider_factories: HashMap<String, ProviderFactory>,
    pub(crate) driver_factories: HashMap<String, DriverFactory>,
    pub(crate) managed_driver_factories: HashMap<String, ManagedDriverFactory>,

    /// Process-wide FUSE sandbox state. Mount is eager — attempted in
    /// `Engine::new` so the bridge gets a ready `LayeredFs` at
    /// construction (or `None` when unsupported in Auto mode).
    pub(crate) fuse: Arc<EngineFuse>,

    /// Guards the execute phase so at most one execute runs per target addr at
    /// a time (cross-process with the filesystem backend, in-process with mem).
    pub(crate) result_lock: ResultLock,

    /// Ordered set of remote (shared) caches fronting the local cache. Empty
    /// (a cheap no-op on every path) unless `caches:` is configured.
    pub(crate) remote_caches: Arc<crate::engine::RemoteCacheSet>,

    /// Aggregates every provider's exposed functions and injects the registry
    /// into consumers (the buildfile provider) exactly once, lazily on the first
    /// provider dispatch — by which point all registration has completed.
    pub(crate) provider_functions_wired: std::sync::Once,
}

/// Per-process FUSE sandbox state. Owns the `<home>/sandboxfuse<pid>/`
/// hierarchy and eagerly mounts a single FUSE filesystem at `lower/` if
/// support is present and config doesn't disable it. `upper/` is a real
/// disk dir backing passthrough writes + copy-up bytes.
pub struct EngineFuse {
    pub(crate) root: PathBuf,
    pub(crate) lower: PathBuf,
    pub(crate) upper: PathBuf,
    pub(crate) mount: Option<EngineMount>,
}

pub struct EngineMount {
    pub(crate) _mount: sandboxfuse::Mount,
    pub(crate) fs: Arc<sandboxfuse::LayeredFs>,
}

impl EngineFuse {
    /// Construct + (when not disabled) mount eagerly. Errors only when
    /// `cfg.enabled == Some(true)` and the mount cannot be brought up.
    pub fn new(cfg: FuseConfig, home: &Path) -> anyhow::Result<Self> {
        let pid = std::process::id();
        let root = home.join(format!("sandboxfuse{pid}"));
        let lower = root.join("lower");
        let upper = root.join("upper");

        if cfg.is_off() {
            return Ok(Self {
                root,
                lower,
                upper,
                mount: None,
            });
        }

        let support = sandboxfuse::support_check();
        if !support.is_available() {
            if cfg.is_on() {
                anyhow::bail!("FUSE forced on but unsupported: {support:?}");
            }
            return Ok(Self {
                root,
                lower,
                upper,
                mount: None,
            });
        }

        std::fs::create_dir_all(&lower)
            .with_context(|| format!("create FUSE lower dir {:?}", lower))?;
        std::fs::create_dir_all(&upper)
            .with_context(|| format!("create FUSE upper dir {:?}", upper))?;
        let fs = Arc::new(sandboxfuse::LayeredFs::new_empty(upper.clone()));
        match sandboxfuse::Mount::mount(&lower, fs.clone()) {
            Ok(m) => {
                // Tell the supervisor about this mount so a crashed
                // parent doesn't leak the FUSE mount into the next run.
                // The supervisor will `umount -f <root>/lower` on EOF.
                hproc::process_supervisor::register_fuse_root(root.clone());
                Ok(Self {
                    root,
                    lower,
                    upper,
                    mount: Some(EngineMount { _mount: m, fs }),
                })
            }
            Err(e) => {
                if cfg.is_on() {
                    return Err(e).context("FUSE forced on but mount failed");
                }
                warn!(error = ?e, "FUSE mount failed; falling back to unpack-copy");
                Ok(Self {
                    root,
                    lower,
                    upper,
                    mount: None,
                })
            }
        }
    }

    /// Returns the shared `LayeredFs` when FUSE was successfully mounted.
    pub fn layered_fs(&self) -> Option<Arc<sandboxfuse::LayeredFs>> {
        self.mount.as_ref().map(|m| m.fs.clone())
    }
}

impl Drop for EngineFuse {
    fn drop(&mut self) {
        // Drop the mount on a worker thread with a bounded wait. The
        // FUSE unmount path can hang in the kernel (macFUSE umount
        // blocks if any FD into the device is still open, and bugs in
        // userspace teardown have wedged the process in `?E+` for
        // minutes). The watchdog upgrades to a forced umount + leaks
        // the worker thread so the process can still exit.
        if let Some(mount) = self.mount.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("heph-fuse-unmount".into())
                .spawn(move || {
                    drop(mount);
                    _ = tx.send(());
                })
                .expect("spawn fuse unmount thread");
            if rx.recv_timeout(std::time::Duration::from_secs(5)).is_err() {
                warn!(
                    lower = ?self.lower,
                    "FUSE unmount exceeded 5s; issuing force umount and leaking session"
                );
                force_umount(&self.lower);
            }
        }
        if self.root.exists() {
            drop(crate::engine::sandbox_cleaner::remove_dir_all(&self.root));
        }
    }
}

/// Force-unmount a FUSE mountpoint. Bounded, idempotent — safe to call
/// against an already-unmounted path. Used by the EngineFuse drop
/// watchdog and by stale-dir sweep on next startup.
pub(crate) fn force_umount(path: &Path) {
    #[cfg(target_os = "linux")]
    {
        drop(
            std::process::Command::new("fusermount3")
                .arg("-uz")
                .arg(path)
                .output(),
        );
    }
    #[cfg(target_os = "macos")]
    {
        drop(
            std::process::Command::new("umount")
                .arg("-f")
                .arg(path)
                .output(),
        );
    }
}

/// Walk `<home>/` for `sandboxfuse<pid>` directories whose pid is no
/// longer alive (via `kill(pid, 0)` returning ESRCH). For each: best-
/// effort umount of `<dir>/lower` (idempotent — works whether stale or
/// not), then `remove_dir_all(<dir>)`. Silent on any error.
fn sweep_stale_sandboxfuse_dirs(home: &Path) {
    let Ok(entries) = std::fs::read_dir(home) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid_str) = name.strip_prefix("sandboxfuse") else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<libc::pid_t>() else {
            continue;
        };
        // SAFETY: kill(pid, 0) is a probe-only syscall; sig=0 doesn't deliver.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if alive {
            continue;
        }
        let dir = entry.path();
        let lower = dir.join("lower");
        // Best-effort force umount. The previous process crashed; any
        // mount it left is unowned and may have dangling kernel state.
        // Plain `umount` blocks indefinitely on macFUSE if the kext
        // still has refs (which is exactly why we're sweeping), so
        // force is required to make sweep idempotent + bounded.
        force_umount(&lower);
        drop(crate::engine::sandbox_cleaner::remove_dir_all(&dir));
    }
}

pub struct Provider {
    pub name: String,
    pub provider: Box<dyn SDKProvider>,
}

pub struct Driver {
    pub name: String,
    pub driver: Box<dyn SDKDriver>,
}

impl Engine {
    pub fn new(cfg: Config) -> anyhow::Result<Engine> {
        let root = cfg.root.clone();
        let home = if cfg.home_dir.as_os_str().is_empty() {
            root.join(".heph3")
        } else {
            cfg.home_dir.clone()
        };

        let parallelism = cfg.parallelism.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });

        let sqlite: Arc<dyn LocalCache> = Arc::new(LocalCacheSQLite::with_pipe_limit(
            home.join("cache").join("cache.db"),
            cfg.mem_cache.per_entry_bytes,
            2 * parallelism,
        )?);
        // Durable tier: manifests + small/medium blobs in sqlite, large blobs
        // spilled to plain files. The mem tier (below) fronts both.
        let blobs = Arc::new(crate::engine::local_cache_fs::LocalCacheFS::new(
            home.join("cache").join("blobs"),
        )?);
        let durable: Arc<dyn LocalCache> =
            Arc::new(crate::engine::local_cache_spill::LocalCacheSpill::new(
                sqlite,
                blobs,
                usize::try_from(cfg.spill_threshold_bytes).unwrap_or(usize::MAX),
            ));
        let local_cache: Arc<dyn LocalCache> = if cfg.mem_cache.capacity_bytes == 0 {
            durable
        } else {
            Arc::new(LocalCacheMem::new(
                durable,
                cfg.mem_cache.per_entry_bytes,
                cfg.mem_cache.capacity_bytes,
            ))
        };

        // Mem-only tier for tmp/uncacheable revisions; spills oversized or
        // over-budget entries to the durable cache so a reader never misses.
        let local_cache_tmp: Arc<dyn LocalCache> =
            Arc::new(crate::engine::local_cache_tmp::LocalCacheTmp::new(
                local_cache.clone(),
                cfg.tmp_cache.per_entry_bytes,
                cfg.tmp_cache.capacity_bytes,
            ));

        // Best-effort sweep of stale `sandboxfuse<pid>` dirs from crashed runs.
        sweep_stale_sandboxfuse_dirs(&home);

        let fuse = Arc::new(EngineFuse::new(cfg.fuse, &home)?);

        let lock_dir = home.join("lock");
        std::fs::create_dir_all(&lock_dir)
            .with_context(|| format!("create lock dir {lock_dir:?}"))?;
        let result_lock = ResultLock::new(cfg.lock_backend, lock_dir);

        // Remote caches: backends are constructed synchronously here (no
        // network); latency ordering is measured lazily on first use.
        let remote_caches = crate::engine::RemoteCacheSet::new(&cfg.remote_caches, home.clone())
            .context("configure remote caches")?;

        // Shared cross-run filesystem-walk cache, handed to tree-walking plugins
        // via `PluginInit`. Its own sqlite db so it can be pruned independently.
        let walker = Arc::new(hwalk::CachedWalker::open(
            &home.join("cache").join("fswalk.db"),
        ));

        let max_workers = 2 * parallelism;

        let mut engine = Engine {
            cfg: cfg.clone(),
            home: home.clone(),
            local_cache,
            local_cache_tmp,
            walker,
            providers: vec![],
            providers_by_name: HashMap::new(),
            drivers: vec![],
            drivers_by_name: HashMap::new(),
            hooks: vec![],
            requests: Mutex::new(HashMap::new()),
            result_semaphore: Arc::new(Semaphore::new(max_workers)),
            max_workers,
            provider_factories: HashMap::new(),
            driver_factories: HashMap::new(),
            managed_driver_factories: HashMap::new(),
            fuse,
            result_lock,
            remote_caches,
            provider_functions_wired: std::sync::Once::new(),
        };
        engine.register_driver(|_| Box::new(hbuiltins::plugingroup::Driver))?;
        engine.register_provider(|_| Box::new(hplugin_query::pluginquery::Provider))?;

        // The `fs` provider + driver are always-on built-ins. Each builds its
        // `Ignore` from the same `PluginInit` (home + `fs.skip` dirs/globs) the
        // engine hands every plugin, so every fs glob walk prunes the same paths.
        // The fallible variant lets a bad `fs.skip` glob surface as an error here.
        engine.try_register_provider(|init| {
            let ignore = Arc::new(hwalk::Ignore::new(&init.skip_dirs, &init.skip_globs)?);
            Ok(Box::new(hbuiltins::pluginfs::Provider::new(
                ignore,
                init.walker.clone(),
            )))
        })?;
        engine.try_register_driver(|init| {
            let ignore = Arc::new(hwalk::Ignore::new(&init.skip_dirs, &init.skip_globs)?);
            Ok(Box::new(hbuiltins::pluginfs::Driver::new(
                ignore,
                init.walker.clone(),
            )))
        })?;

        Ok(engine)
    }

    /// Cancel every in-flight request's cancellation token. Used to broadcast
    /// graceful shutdown (e.g. on SIGINT). Idempotent.
    pub fn cancel_all_requests(&self) {
        let Ok(requests) = self.requests.lock() else {
            return;
        };
        for weak in requests.values() {
            if let Some(rs) = weak.upgrade() {
                rs.ctoken().cancel();
            }
        }
    }

    /// The per-addr execute-phase lock.
    pub fn result_lock(&self) -> &ResultLock {
        &self.result_lock
    }

    /// Workspace root.
    pub fn root(&self) -> &std::path::Path {
        &self.cfg.root
    }

    /// The aggregate provider-function registry (every provider's `heph.<p>.<fn>`
    /// functions), built fresh. Used by the BUILD-file LSP to assemble the same
    /// Starlark globals BUILD evaluation sees, for symbol completion/hover.
    pub fn provider_function_registry(&self) -> Arc<provider::ProviderFunctionRegistry> {
        let mut registry = provider::ProviderFunctionRegistry::default();
        for provider in &self.providers {
            registry.insert_provider(&provider.name, provider.provider.functions());
        }
        Arc::new(registry)
    }

    /// The config schema a registered driver exposes. Used by the BUILD-file LSP
    /// to complete and document a target's driver-specific config fields. Returns
    /// `None` only for an unknown driver name; a known config-less driver returns
    /// `Some(DriverSchema::default())` (no fields).
    pub fn driver_schema(&self, name: &str) -> Option<crate::engine::driver::DriverSchema> {
        self.drivers_by_name.get(name).map(|d| d.driver.schema())
    }

    /// Names of all registered drivers, sorted. Used by the BUILD-file LSP to
    /// complete the `driver =` argument of a `target(...)` call.
    pub fn driver_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.drivers_by_name.keys().cloned().collect();
        names.sort();
        names
    }

    /// The state schema a registered provider exposes, if any. Used by the
    /// BUILD-file LSP to complete and document `provider_state(provider="<name>", …)`
    /// args. Returns `None` for unknown providers or providers without a schema.
    pub fn provider_state_schema(&self, name: &str) -> Option<provider::StateSchema> {
        self.providers_by_name
            .get(name)
            .and_then(|p| p.provider.state_schema())
    }

    /// Every `(provider name, function name, rendered signature)` exposed across
    /// all registered providers, sorted. The rendered signature looks like
    /// `glob(pattern: string) -> list[string]`. Surfaced via `heph inspect functions`.
    pub fn provider_functions(&self) -> Vec<(String, String, String)> {
        let mut out: Vec<(String, String, String)> = self
            .providers
            .iter()
            .flat_map(|p| {
                p.provider.functions().into_iter().map(move |def| {
                    let rendered = def.signature.render(&def.name);
                    (p.name.clone(), def.name, rendered)
                })
            })
            .collect();
        out.sort();
        out
    }

    /// Build the aggregate function registry from every registered provider and
    /// inject it into each provider, exactly once. Idempotent and cheap after the
    /// first call. Invoked at the top of provider-dispatch paths so the registry
    /// is complete by the first BUILD evaluation.
    pub(crate) fn ensure_provider_functions_wired(&self) {
        self.provider_functions_wired.call_once(|| {
            let mut registry = provider::ProviderFunctionRegistry::default();
            for provider in &self.providers {
                registry.insert_provider(&provider.name, provider.provider.functions());
            }
            let registry = Arc::new(registry);
            for provider in &self.providers {
                provider
                    .provider
                    .set_function_registry(Arc::clone(&registry));
            }
        });
    }

    /// The [`PluginInit`] context handed to every plugin constructor (direct
    /// registration or factory): workspace root + the engine's skip dirs/globs.
    fn plugin_init_payload(&self) -> PluginInit {
        PluginInit {
            root: self.cfg.root.clone(),
            skip_dirs: self.skip_dirs(),
            skip_globs: self.skip_globs(),
            walker: self.walker.clone(),
        }
    }

    /// Registers an already-constructed driver. Shared by [`Self::register_driver`]
    /// and [`Self::register_managed_driver`].
    fn insert_driver(&mut self, driver: Box<dyn SDKDriver>) -> anyhow::Result<()> {
        let driver = Arc::new(Driver {
            name: driver.config(driver::ConfigRequest {})?.name,
            driver,
        });

        if self.drivers_by_name.contains_key(&driver.name) {
            return Err(anyhow::anyhow!(
                "driver with name '{}' already registered",
                driver.name
            ));
        }
        self.drivers.push(driver.clone());
        self.drivers_by_name.insert(driver.name.clone(), driver);
        Ok(())
    }

    pub fn register_managed_driver(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKManagedDriver>,
    ) -> anyhow::Result<()> {
        let managed = factory(&self.plugin_init_payload());
        let driver = self.new_managed_driver(managed);
        self.insert_driver(Box::new(driver))
    }

    pub fn register_driver(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKDriver>,
    ) -> anyhow::Result<()> {
        self.try_register_driver(|init| Ok(factory(init)))
    }

    /// Like [`Self::register_driver`], but the factory may fail (e.g. compiling a
    /// glob set). The error propagates out of registration.
    pub fn try_register_driver(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> anyhow::Result<Box<dyn SDKDriver>>,
    ) -> anyhow::Result<()> {
        let driver = factory(&self.plugin_init_payload())?;
        self.insert_driver(driver)
    }

    pub fn register_provider(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKProvider>,
    ) -> anyhow::Result<()> {
        self.try_register_provider(|init| Ok(factory(init)))
    }

    /// Like [`Self::register_provider`], but the factory may fail (e.g. compiling
    /// a glob set). The error propagates out of registration.
    pub fn try_register_provider(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> anyhow::Result<Box<dyn SDKProvider>>,
    ) -> anyhow::Result<()> {
        let provider = factory(&self.plugin_init_payload())?;

        let provider = Arc::new(Provider {
            name: provider.config(provider::ConfigRequest {})?.name,
            provider,
        });

        if self.providers_by_name.contains_key(&provider.name) {
            return Err(anyhow::anyhow!(
                "provider with name '{}' already registered",
                provider.name
            ));
        }
        self.providers.push(provider.clone());
        self.providers_by_name
            .insert(provider.name.clone(), provider);
        Ok(())
    }

    /// Register a build-event hook. Hooks are observers — fed every emitted
    /// `BuildEvent` and never queried — so they need no name-uniqueness guard
    /// (two CI summary hooks would just both run). Loaded from out-of-process
    /// plugins (see `plugin_load`).
    pub fn register_hook(&mut self, hook: Arc<dyn SDKHook>) -> anyhow::Result<()> {
        self.hooks.push(hook);
        Ok(())
    }

    /// Snapshot of the registered hooks, cloned into a request's state so every
    /// emitted event fans out to them. Empty unless a hook plugin is configured.
    pub fn hooks(&self) -> Vec<Arc<dyn SDKHook>> {
        self.hooks.clone()
    }

    /// Await every hook's in-flight delivery/flush. Called at command teardown,
    /// after the request state (and thus its `on_close`) has dropped, so an
    /// out-of-process hook's final write completes before the process exits.
    pub async fn await_hooks(&self) {
        for h in &self.hooks {
            h.drain().await;
        }
    }

    pub fn register_provider_factory(
        &mut self,
        name: &str,
        factory: impl FnOnce(&PluginInit, &Options) -> anyhow::Result<Box<dyn SDKProvider>>
        + Send
        + Sync
        + 'static,
    ) -> anyhow::Result<()> {
        if self.provider_factories.contains_key(name) {
            return Err(anyhow::anyhow!(
                "provider factory '{name}' already registered"
            ));
        }
        self.provider_factories
            .insert(name.to_string(), Box::new(factory));
        Ok(())
    }

    pub fn register_driver_factory(
        &mut self,
        name: &str,
        factory: impl FnOnce(&PluginInit, &Options) -> anyhow::Result<Box<dyn SDKDriver>>
        + Send
        + Sync
        + 'static,
    ) -> anyhow::Result<()> {
        if self.driver_factories.contains_key(name)
            || self.managed_driver_factories.contains_key(name)
        {
            return Err(anyhow::anyhow!(
                "driver factory '{name}' already registered"
            ));
        }
        self.driver_factories
            .insert(name.to_string(), Box::new(factory));
        Ok(())
    }

    pub fn register_managed_driver_factory(
        &mut self,
        name: &str,
        factory: impl FnOnce(&PluginInit, &Options) -> anyhow::Result<Box<dyn SDKManagedDriver>>
        + Send
        + Sync
        + 'static,
    ) -> anyhow::Result<()> {
        if self.driver_factories.contains_key(name)
            || self.managed_driver_factories.contains_key(name)
        {
            return Err(anyhow::anyhow!(
                "driver factory '{name}' already registered"
            ));
        }
        self.managed_driver_factories
            .insert(name.to_string(), Box::new(factory));
        Ok(())
    }

    /// Absolute directories every plugin that walks the tree must prune by exact
    /// path: the engine-owned home (cache/sandboxes/locks — never packages) plus
    /// the literal (non-glob) `fs.skip` entries, resolved relative to the root.
    /// Whether anonymous usage telemetry is enabled for this engine (config
    /// `telemetry.enabled`, default `true`).
    pub fn telemetry_enabled(&self) -> bool {
        self.cfg.telemetry_enabled
    }

    pub fn skip_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.home.clone()];
        dirs.extend(
            self.cfg
                .fs_skip
                .iter()
                .filter(|e| !is_skip_glob(e))
                .map(|rel| self.cfg.root.join(normalize_skip(rel))),
        );
        dirs
    }

    /// Exclude globs every tree-walking plugin honors: the always-on `.git`
    /// exclusion (a `.git` dir is never a build input, and submodules nest it)
    /// plus the glob `fs.skip` entries (e.g. `**/node_modules/**`). All are
    /// root-relative.
    pub fn skip_globs(&self) -> Vec<String> {
        std::iter::once("**/.git/**".to_string())
            .chain(
                self.cfg
                    .fs_skip
                    .iter()
                    .filter(|e| is_skip_glob(e))
                    .map(|e| normalize_skip(e).to_string()),
            )
            .collect()
    }

    /// Instantiate one built-in plugin by name (a `plugins: - { builtin: <name> }`
    /// entry), looking up its registered factory (provider, then driver, then
    /// managed driver) and registering it with `options`. Errors if `name` has no
    /// registered factory. Factories are consumed — applying the same name twice
    /// errors.
    pub fn apply_builtin(&mut self, name: &str, options: &Options) -> anyhow::Result<()> {
        let init = self.plugin_init_payload();
        if let Some(factory) = self.provider_factories.remove(name) {
            let provider = factory(&init, options)?;
            let resolved_name = provider.config(provider::ConfigRequest {})?.name;
            if resolved_name != name {
                return Err(anyhow::anyhow!(
                    "provider '{name}' reported name '{resolved_name}'; config/factory name mismatch"
                ));
            }
            self.register_provider(|_| provider)?;
        } else if let Some(factory) = self.driver_factories.remove(name) {
            let driver = factory(&init, options)?;
            let resolved_name = driver.config(driver::ConfigRequest {})?.name;
            if resolved_name != name {
                return Err(anyhow::anyhow!(
                    "driver '{name}' reported name '{resolved_name}'; config/factory name mismatch"
                ));
            }
            self.insert_driver(driver)?;
        } else if let Some(factory) = self.managed_driver_factories.remove(name) {
            let managed = factory(&init, options)?;
            let driver = self.new_managed_driver(managed);
            self.insert_driver(Box::new(driver))?;
        } else {
            return Err(anyhow::anyhow!("unknown builtin plugin '{name}'"));
        }
        Ok(())
    }
}

impl hplugin::lsp::LspEngine for Engine {
    fn root(&self) -> &std::path::Path {
        &self.cfg.root
    }

    fn provider_function_registry(&self) -> Arc<provider::ProviderFunctionRegistry> {
        Engine::provider_function_registry(self)
    }

    fn driver_schema(&self, name: &str) -> Option<crate::engine::driver::DriverSchema> {
        Engine::driver_schema(self, name)
    }

    fn driver_names(&self) -> Vec<String> {
        Engine::driver_names(self)
    }

    fn provider_state_schema(&self, name: &str) -> Option<provider::StateSchema> {
        Engine::provider_state_schema(self, name)
    }

    fn provider_options(&self, name: &str) -> hplugin::config::Options {
        // Built-in providers carry their options on the matching `builtin:` plugin
        // entry. (cdylib-plugin providers self-describe and aren't keyed by name
        // in config, so they fall through to defaults here.)
        use crate::engine::config_yaml::PluginIdentifier;
        crate::engine::config_yaml::load_from_root(self.root())
            .ok()
            .and_then(|cfg| {
                cfg.plugins
                    .into_iter()
                    .find(|p| matches!(&p.identifier, PluginIdentifier::Builtin(b) if b == name))
                    .map(|p| p.options)
            })
            .unwrap_or_default()
    }
}
