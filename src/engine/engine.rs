use crate::engine::config_file::{FuseConfig, Options, PluginEntry};
use crate::engine::driver::Driver as SDKDriver;
use crate::engine::driver_managed::ManagedDriver as SDKManagedDriver;
use crate::engine::local_cache::LocalCache;
use crate::engine::local_cache_mem::LocalCacheMem;
use crate::engine::local_cache_sqlite::LocalCacheSQLite;
use crate::engine::provider::Provider as SDKProvider;
use crate::engine::request_state::RequestState;
use crate::engine::result_lock::{LockBackend, ResultLock};
use crate::engine::{driver, provider};
use crate::sandboxfuse;
use anyhow::Context;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Semaphore;
use tracing::warn;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Config {
    pub root: PathBuf,
    /// Workspace state/cache directory. If empty, defaults to `root/.heph3`.
    pub home_dir: PathBuf,
    /// Repo-root-relative directories from the config file's `fs.skip`, pruned by
    /// every plugin that walks the tree. See [`Engine::skip_dirs`].
    pub fs_skip: Vec<String>,
    pub parallelism: Option<usize>,
    pub mem_cache: MemCacheOptions,
    pub fuse: FuseConfig,
    /// Backend serializing the execute phase per addr. Defaults to `Fs`.
    pub lock_backend: LockBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemCacheOptions {
    pub per_entry_bytes: usize,
    /// Total byte budget. `0` disables the in-memory layer entirely.
    pub capacity_bytes: u64,
}

impl Default for MemCacheOptions {
    fn default() -> Self {
        Self {
            per_entry_bytes: 16 * 1024,
            capacity_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Context the engine injects when constructing any plugin (provider, driver, or
/// managed driver — whether registered directly or through a factory): the
/// workspace root plus the filesystem-skip config every tree walk must honor.
/// `fs.skip` entries are split by the engine into literal directories ([`skip_dirs`])
/// and glob patterns ([`skip_globs`]).
///
/// [`skip_dirs`]: PluginInit::skip_dirs
/// [`skip_globs`]: PluginInit::skip_globs
pub struct PluginInit<'a> {
    pub root: &'a Path,
    /// Absolute directories to prune by exact path: the heph home plus the
    /// literal (non-glob) `fs.skip` entries, resolved relative to the repo root.
    pub skip_dirs: &'a [PathBuf],
    /// Workspace-relative `fs.skip` glob patterns (e.g. `**/node_modules/**`),
    /// matched against entry paths.
    pub skip_globs: &'a [String],
}

/// Always-on exclude glob: a `.git` directory at any depth is never a build
/// input (and submodules nest it). Handed to every tree-walking plugin via
/// [`Engine::skip_globs`], so the fs plugin and the buildfile/go providers all
/// prune it without each hardcoding the rule.
const GIT_SKIP_GLOB: &str = "**/.git/**";

/// True if `entry` contains wax glob metacharacters — used to split `fs.skip`
/// into literal directories vs glob patterns.
pub(crate) fn is_skip_glob(entry: &str) -> bool {
    entry.contains(['*', '?', '[', ']', '{', '}'])
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

    pub(crate) providers: Vec<Arc<Provider>>,
    pub(crate) providers_by_name: HashMap<String, Arc<Provider>>,
    pub(crate) drivers: Vec<Arc<Driver>>,
    pub(crate) drivers_by_name: HashMap<String, Arc<Driver>>,

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
                crate::process_supervisor::register_fuse_root(root.clone());
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
        let local_cache: Arc<dyn LocalCache> = if cfg.mem_cache.capacity_bytes == 0 {
            sqlite
        } else {
            Arc::new(LocalCacheMem::new(
                sqlite,
                cfg.mem_cache.per_entry_bytes,
                cfg.mem_cache.capacity_bytes,
            ))
        };

        // Best-effort sweep of stale `sandboxfuse<pid>` dirs from crashed runs.
        sweep_stale_sandboxfuse_dirs(&home);

        let fuse = Arc::new(EngineFuse::new(cfg.fuse, &home)?);

        let lock_dir = home.join("lock");
        std::fs::create_dir_all(&lock_dir)
            .with_context(|| format!("create lock dir {lock_dir:?}"))?;
        let result_lock = ResultLock::new(cfg.lock_backend, lock_dir);

        let max_workers = 2 * parallelism;

        let mut engine = Engine {
            cfg: cfg.clone(),
            home: home.clone(),
            local_cache,
            providers: vec![],
            providers_by_name: HashMap::new(),
            drivers: vec![],
            drivers_by_name: HashMap::new(),
            requests: Mutex::new(HashMap::new()),
            result_semaphore: Arc::new(Semaphore::new(max_workers)),
            max_workers,
            provider_factories: HashMap::new(),
            driver_factories: HashMap::new(),
            managed_driver_factories: HashMap::new(),
            fuse,
            result_lock,
            provider_functions_wired: std::sync::Once::new(),
        };
        engine.register_driver(|_| Box::new(crate::plugingroup::Driver))?;
        engine.register_provider(|_| Box::new(crate::pluginquery::Provider))?;

        // The `fs` provider + driver are always-on built-ins. The engine owns
        // their ignore set (home + `fs.skip` dirs/globs) and builds it once from
        // the same data it hands every plugin via `PluginInit`, so every fs glob
        // walk prunes the same paths. Provider + driver share one `Ignore` so
        // BUILD-time `glob()` and the run-time walk agree.
        let fs_skip = Arc::new(crate::htwalk::Ignore::new(
            &engine.skip_dirs(),
            &engine.skip_globs(),
        )?);
        engine.register_provider({
            let fs_skip = fs_skip.clone();
            move |_| Box::new(crate::pluginfs::Provider::new(fs_skip))
        })?;
        engine.register_driver(move |_| Box::new(crate::pluginfs::Driver::new(fs_skip)))?;

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

    /// Every `(provider name, function name)` pair exposed across all registered
    /// providers, sorted. Surfaced via `heph inspect functions`.
    pub fn provider_functions(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .providers
            .iter()
            .flat_map(|p| {
                p.provider
                    .functions()
                    .into_iter()
                    .map(move |def| (p.name.clone(), def.name))
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

    /// Builds the owned data backing a [`PluginInit`] (`root`, skip dirs, skip
    /// globs). Borrow the parts into a `PluginInit` at the call site.
    fn plugin_init_parts(&self) -> (PathBuf, Vec<PathBuf>, Vec<String>) {
        (self.cfg.root.clone(), self.skip_dirs(), self.skip_globs())
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
        let (root, skip_dirs, skip_globs) = self.plugin_init_parts();
        let managed = factory(&PluginInit {
            root: &root,
            skip_dirs: &skip_dirs,
            skip_globs: &skip_globs,
        });
        let driver = self.new_managed_driver(managed);
        self.insert_driver(Box::new(driver))
    }

    pub fn register_driver(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKDriver>,
    ) -> anyhow::Result<()> {
        let (root, skip_dirs, skip_globs) = self.plugin_init_parts();
        let driver = factory(&PluginInit {
            root: &root,
            skip_dirs: &skip_dirs,
            skip_globs: &skip_globs,
        });
        self.insert_driver(driver)
    }

    pub fn register_provider(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKProvider>,
    ) -> anyhow::Result<()> {
        let (root, skip_dirs, skip_globs) = self.plugin_init_parts();
        let provider = factory(&PluginInit {
            root: &root,
            skip_dirs: &skip_dirs,
            skip_globs: &skip_globs,
        });

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
    pub fn skip_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.home.clone()];
        dirs.extend(
            self.cfg
                .fs_skip
                .iter()
                .filter(|e| !is_skip_glob(e))
                .map(|rel| self.cfg.root.join(rel)),
        );
        dirs
    }

    /// Exclude globs every tree-walking plugin honors: the always-on `.git`
    /// exclusion plus the glob `fs.skip` entries (e.g. `**/node_modules/**`),
    /// matched against workspace-relative paths.
    pub fn skip_globs(&self) -> Vec<String> {
        std::iter::once(GIT_SKIP_GLOB.to_string())
            .chain(self.cfg.fs_skip.iter().filter(|e| is_skip_glob(e)).cloned())
            .collect()
    }

    /// Instantiates every provider/driver listed in the entries by looking up the
    /// matching factory by name. Errors if any name has no registered factory.
    /// Factories are consumed — calling twice on the same name will error.
    pub fn apply_config(
        &mut self,
        providers: &[PluginEntry],
        drivers: &[PluginEntry],
    ) -> anyhow::Result<()> {
        let (root, skip_dirs, skip_globs) = self.plugin_init_parts();
        let init = PluginInit {
            root: &root,
            skip_dirs: &skip_dirs,
            skip_globs: &skip_globs,
        };
        for entry in providers {
            let factory = self
                .provider_factories
                .remove(&entry.name)
                .ok_or_else(|| anyhow::anyhow!("unknown provider '{}'", entry.name))?;
            let provider = factory(&init, &entry.options)?;
            let resolved_name = provider.config(provider::ConfigRequest {})?.name;
            if resolved_name != entry.name {
                return Err(anyhow::anyhow!(
                    "provider '{}' reported name '{}'; config/factory name mismatch",
                    entry.name,
                    resolved_name
                ));
            }
            self.register_provider(|_| provider)?;
        }
        for entry in drivers {
            if let Some(factory) = self.driver_factories.remove(&entry.name) {
                let driver = factory(&init, &entry.options)?;
                let resolved_name = driver.config(driver::ConfigRequest {})?.name;
                if resolved_name != entry.name {
                    return Err(anyhow::anyhow!(
                        "driver '{}' reported name '{}'; config/factory name mismatch",
                        entry.name,
                        resolved_name
                    ));
                }
                self.insert_driver(driver)?;
            } else if let Some(factory) = self.managed_driver_factories.remove(&entry.name) {
                let managed = factory(&init, &entry.options)?;
                let driver = self.new_managed_driver(managed);
                self.insert_driver(Box::new(driver))?;
            } else {
                return Err(anyhow::anyhow!("unknown driver '{}'", entry.name));
            }
        }
        Ok(())
    }
}
