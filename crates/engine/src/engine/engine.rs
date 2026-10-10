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
use hlock::hlock::{FLock, FWriteGuard, Lock};
use hplugin::function::{FunctionRegistry, FunctionSlot, PluginFnDef};
use hsandboxfuse as sandboxfuse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tracing::{error, warn};

/// Context the engine injects when constructing any plugin (provider, driver, or
/// managed driver — whether registered directly or through a factory): the
/// workspace root plus the filesystem-skip config every tree walk must honor.
/// `fs.skip` entries are split by the engine into literal directories ([`skip_dirs`])
/// and glob patterns ([`skip_globs`]).
///
/// [`skip_dirs`]: PluginInit::skip_dirs
/// [`skip_globs`]: PluginInit::skip_globs
pub struct PluginInit {
    /// The plugin's name: its registration key (a builtin) or its manifest's
    /// `name` (a cdylib). Its `heph.<name>` namespace and the prefix of every
    /// component's name — handed over for a plugin that wants it in messages.
    pub name: String,
    pub root: PathBuf,
    /// Absolute directories to prune by exact path: the heph home plus the
    /// literal (non-glob) `fs.skip` entries, resolved relative to the repo root.
    pub skip_dirs: Vec<PathBuf>,
    /// Workspace-relative `fs.skip` glob patterns (e.g. `**/node_modules/**`),
    /// matched against entry paths.
    pub skip_globs: Vec<String>,
    /// Shared cross-run filesystem-walk cache for tree-walking plugins.
    pub walker: Arc<hwalk::CachedWalker>,
    /// The engine's runtime — what an in-process plugin's memoizers spawn
    /// their computations on. Handed to the plugin, never discovered by it
    /// (a cdylib plugin uses its own runtime instead; this field serves the
    /// in-process construction path).
    pub runtime: tokio::runtime::Handle,
    /// This engine's function registry, for a plugin whose functions call
    /// another plugin's (`heph.<plugin>.<fn>` by `(plugin, fn)`). Unsealed while
    /// plugins register — reading it then is an error — and sealed before the
    /// first BUILD evaluation. Per engine: never share it across engines.
    pub functions: Arc<FunctionSlot>,
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

/// One driver a plugin ships: a plain [`SDKDriver`], or a [`SDKManagedDriver`]
/// the engine wraps in its sandbox bridge.
pub enum PluginDriver {
    Plain(Box<dyn SDKDriver>),
    Managed(Box<dyn SDKManagedDriver>),
}

/// Everything one plugin contributes. It carries no name: the engine supplies
/// it from the registration key (a builtin) or the manifest (a cdylib), and
/// names each component after it — `<plugin>` for a component that reports no
/// local name (or one equal to the plugin's), `<plugin>.<local>` otherwise.
#[derive(Default)]
pub struct PluginParts {
    pub provider: Option<Box<dyn SDKProvider>>,
    pub drivers: Vec<PluginDriver>,
    /// BUILD-file functions, `heph.<plugin>.<fn>`. A plugin may have functions
    /// and no provider at all.
    pub functions: Vec<PluginFnDef>,
    pub hooks: Vec<Arc<dyn SDKHook>>,
    pub runners: Vec<Arc<dyn hexecrunner::registry::ExecRunner>>,
}

impl PluginParts {
    pub fn with_functions(mut self, functions: Vec<PluginFnDef>) -> Self {
        self.functions.extend(functions);
        self
    }

    pub fn with_provider(mut self, provider: Box<dyn SDKProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn with_driver(mut self, driver: Box<dyn SDKDriver>) -> Self {
        self.drivers.push(PluginDriver::Plain(driver));
        self
    }

    pub fn with_managed_driver(mut self, driver: Box<dyn SDKManagedDriver>) -> Self {
        self.drivers.push(PluginDriver::Managed(driver));
        self
    }
}

/// A YAML-selected builtin: built from the [`PluginInit`] context and the
/// plugin's `options:` when a `plugins: - { builtin: <name> }` entry names it.
pub type PluginFactory =
    Box<dyn FnOnce(&PluginInit, &Options) -> anyhow::Result<PluginParts> + Send + Sync>;

/// One registered plugin and the full name of each component it contributed —
/// what `heph tool plugins` lists.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginInfo {
    pub name: String,
    /// Where it came from: `builtin plugin "fs"`, or a cdylib's path and manifest.
    pub source: String,
    pub provider: Option<String>,
    pub drivers: Vec<String>,
    /// As called from a BUILD file: `heph.<plugin>.<fn>`.
    pub functions: Vec<String>,
    pub runners: Vec<String>,
    pub hooks: usize,
}

/// The name every provider/driver is reachable under: `<plugin>` when the
/// component reports no local name (or its plugin's own), `<plugin>.<local>`
/// otherwise.
pub fn component_name(plugin: &str, local: &str) -> String {
    if local.is_empty() || local == plugin {
        plugin.to_string()
    } else {
        format!("{plugin}.{local}")
    }
}

/// The rule a plugin name follows: it is the `heph.<name>` BUILD namespace, so
/// it must be a Starlark identifier segment.
pub const PLUGIN_NAME_RULE: &str = "[a-z_][a-z0-9_]*";

/// `PluginInit.name` for the test-only bare registrations, whose plugin is
/// named after the component they construct — so not known until it exists.
const BARE_PLUGIN_NAME: &str = "";

/// Names no plugin may take: `core` is the static `heph.core` namespace.
const RESERVED_PLUGIN_NAMES: &[&str] = &["core"];

fn is_valid_name_segment(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The full name of a component `kind` ("provider"/"driver") reporting the
/// local name `local`, inside plugin `plugin`. A local name follows the same
/// character rule as a plugin name, so `<plugin>.<local>` stays unambiguous.
fn resolve_component_name(plugin: &str, local: &str, kind: &str) -> anyhow::Result<String> {
    if !local.is_empty() && !is_valid_name_segment(local) {
        anyhow::bail!(
            "plugin {plugin:?} has a {kind} with local name {local:?}; a local name must match \
             `{PLUGIN_NAME_RULE}` (or be empty, to be reachable as {plugin:?})"
        );
    }
    Ok(component_name(plugin, local))
}

/// Checks a plugin name against the naming rule and the reserved set.
pub fn validate_plugin_name(name: &str) -> anyhow::Result<()> {
    if RESERVED_PLUGIN_NAMES.contains(&name) {
        anyhow::bail!(
            "plugin name {name:?} is reserved (it is the built-in `heph.{name}` namespace); \
             choose another name"
        );
    }
    if !is_valid_name_segment(name) {
        anyhow::bail!(
            "invalid plugin name {name:?}: a plugin name must match `{PLUGIN_NAME_RULE}`, \
             because it is the `heph.<name>` namespace in BUILD files"
        );
    }
    Ok(())
}

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
    /// Every exec runner this host knows, by name: `local`, `wrap`, `session`.
    ///
    /// Builtins only. A plugin that wants agent mode does not implement a
    /// runner — it emits a `runner.json` naming `session` with the argv that
    /// enters its environment, and the descriptor passing, cancellation and
    /// pooling are shared. Both in-tree plugins that run targets somewhere else
    /// (devenv, oci) work that way, which is why there is no ABI surface for a
    /// plugin-exported runner.
    ///
    /// Held behind an `Arc` so `install_exec_runner_host` can hand the resolver
    /// a clone.
    pub(crate) exec_runners: Arc<hexecrunner::registry::RunnerRegistry>,
    pub home: PathBuf,
    /// The runtime every request's memoizers spawn their computations on.
    /// Captured once at construction — the engine is handed its runtime, the
    /// memoizers never discover one at spawn time.
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) result_permits: Arc<crate::engine::worker_pool::WorkerPool>,
    /// Maximum concurrent executes (the `result_permits` capacity). Cached
    /// here so it can be announced to clients via a `RequestConfig` build event
    /// without reaching into the semaphore (whose live `available_permits` only
    /// reflects the *free* count, not the configured max).
    pub(crate) max_workers: usize,

    /// YAML-selected builtins, by plugin name; consumed when applied.
    pub(crate) plugin_factories: HashMap<String, PluginFactory>,
    /// Every registered plugin, by name: where it came from (for the
    /// duplicate-name error, which names both sources) and its components.
    pub(crate) plugins: HashMap<String, PluginInfo>,

    /// Process-wide FUSE sandbox state. Mount is eager — attempted in
    /// `Engine::new` so the bridge gets a ready `LayeredFs` at
    /// construction (or `None` when unsupported in Auto mode).
    pub(crate) fuse: Arc<EngineFuse>,

    /// Guards the execute phase so at most one execute runs per target addr at
    /// a time (cross-process with the filesystem backend, in-process with mem).
    pub(crate) result_lock: ResultLock,
    pub(crate) scratch_lock: crate::engine::scratch::ScratchLock,

    /// Acquired credential material, per process. Not a `Memoizer`: material
    /// expires, and a memoized cell is computed once and kept forever. See
    /// [`CredentialCache`](crate::engine::credential::CredentialCache).
    pub(crate) credential_cache: crate::engine::credential::CredentialCache,
    /// Serializes acquisition across processes, per resolution key. Two
    /// concurrent `heph` invocations must not both drive an interactive vendor
    /// CLI for the same credential.
    pub(crate) credential_lock: crate::engine::credential::CredentialLock,

    /// Deferred option values, keyed on the *producer's* `hashin`.
    ///
    /// Caches the derived value, never the resolution — the rule
    /// `execrunner_host` already follows for a runner's config. The shape this
    /// feature invites is fan-out (one `//infra:registry`, every image target),
    /// and `result_addr` memoizes the producer's *build* but not the artifact
    /// walk, so without this each consumer re-walks and re-reads the tar.
    ///
    /// Per engine rather than process-wide, and through the memoizer rather than
    /// a bare map, so two concurrent misses single-flight instead of both doing
    /// the read.
    pub(crate) deferred_values:
        hcore::hmemoizer::Memoizer<String, Result<String, Arc<anyhow::Error>>>,

    /// Ordered set of remote (shared) caches fronting the local cache. Empty
    /// (a cheap no-op on every path) unless `caches:` is configured.
    pub(crate) remote_caches: Arc<crate::engine::RemoteCacheSet>,

    /// Every plugin's BUILD-file functions, keyed `(plugin, fn)`. Built
    /// incrementally while plugins register (`Arc::get_mut`, so only while the
    /// engine holds the only reference), then sealed into [`Self::function_slot`].
    /// The engine holds the only strong reference: the slot and every plugin's
    /// registry handle hold it weakly.
    pub(crate) functions: Arc<FunctionRegistry>,
    /// The handle every plugin is given (in `PluginInit`, or behind a cdylib's
    /// registry handle). An error to read before [`Self::seal_functions`].
    pub(crate) function_slot: Arc<FunctionSlot>,

    /// The remote cache's temp directory, created and swept of abandoned temps
    /// on first use (`Engine::remote_tmp_dir`). Lazy, so a build with no
    /// `caches:` configured never touches the directory at all.
    pub(crate) remote_tmp_ready: tokio::sync::OnceCell<PathBuf>,

    /// This process's `--no-scratch` audit directory, created (and older,
    /// dead-process ones swept) on first use. Lazy, so an ordinary build never
    /// creates it.
    pub(crate) scratch_audit_ready: tokio::sync::OnceCell<PathBuf>,
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
    /// Held for the process's entire lifetime once `root` exists, so a later
    /// process's `sweep_stale_sandboxfuse_dirs` can tell this directory is
    /// still owned. `None` when `root` was never created (FUSE off or
    /// unsupported).
    _lock: Option<FWriteGuard>,
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
                _lock: None,
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
                _lock: None,
            });
        }

        std::fs::create_dir_all(&lower)
            .with_context(|| format!("create FUSE lower dir {:?}", lower))?;
        std::fs::create_dir_all(&upper)
            .with_context(|| format!("create FUSE upper dir {:?}", upper))?;
        // Ownership marker for `sweep_stale_sandboxfuse_dirs` in a future
        // process: held for as long as this `EngineFuse` lives. `root` is
        // freshly named after our own pid and any stale same-named leftover
        // was already reclaimed by the sweep that ran before this call, so
        // contention here means a real invariant violation, not a race to
        // retry.
        let lock = FLock::new(root.join("lock"))
            .try_lock()
            .with_context(|| format!("locking sandboxfuse dir {:?}", root))?
            .with_context(|| format!("sandboxfuse dir {:?} already owned", root))?;
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
                    _lock: Some(lock),
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
                    _lock: Some(lock),
                })
            }
        }
    }

    /// Returns the shared `LayeredFs` when FUSE was successfully mounted.
    pub fn layered_fs(&self) -> Option<Arc<sandboxfuse::LayeredFs>> {
        self.mount.as_ref().map(|m| m.fs.clone())
    }
}

/// Release anything the exec runners hold open.
///
/// Not left to the runners' own `Drop`: the registry is also reachable from
/// `hexecrunner`'s process-global installed host, and Rust never runs
/// destructors for statics — so nothing in that chain is ever dropped at exit.
/// A session runner relying on `Drop` therefore leaves its agent running after
/// heph is gone, and an agent that inherited a descriptor will hang whatever is
/// reading heph's output to EOF.
impl Drop for Engine {
    fn drop(&mut self) {
        self.exec_runners.shutdown_all();
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
            // Can't propagate a Result from Drop. On spawn failure the
            // closure (and the `mount` it captured) is dropped in place —
            // the unmount still runs, just synchronously and without the
            // bounded-wait/force-umount watchdog below, since there's no
            // worker thread left to send on `tx`. Log and fall through:
            // `rx.recv_timeout` observes the disconnected channel and
            // takes the same force-umount path as a timeout would.
            let spawned = std::thread::Builder::new()
                .name("heph-fuse-unmount".into())
                .spawn(move || {
                    drop(mount);
                    _ = tx.send(());
                });
            if let Err(e) = &spawned {
                error!(error = ?e, lower = ?self.lower, "failed to spawn FUSE unmount thread");
            }
            if rx.recv_timeout(std::time::Duration::from_secs(5)).is_err() {
                if spawned.is_ok() {
                    warn!(
                        lower = ?self.lower,
                        "FUSE unmount exceeded 5s; issuing force umount and leaking session"
                    );
                }
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

/// Walk `<home>/` for `sandboxfuse<pid>` directories no longer owned by a
/// live process. For each: best-effort umount of `<dir>/lower` (idempotent —
/// works whether stale or not), then `remove_dir_all(<dir>)`. Silent on any
/// error.
///
/// Ownership is decided by probing `<dir>/lock` with [`FLock::is_path_held`],
/// not by `kill(pid, 0)` on the directory's pid suffix. `kill` cannot tell a
/// live process owned by another uid from a dead one: both report `EPERM` or
/// `ESRCH` indistinguishably to a caller that only checks the return code, and
/// treating `EPERM` (process exists, just not ours) as "dead" reclaims a live
/// process's sandbox out from under it. The `flock` probe answers for the
/// lock itself, so it is immune to that inversion, to pid reuse, and to a
/// zombie whose pid still answers `kill`. `EngineFuse::new` takes the lock for
/// the directory's whole lifetime, so "held" and "owned by a live process" are
/// the same fact.
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
        if pid_str.is_empty() || !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let dir = entry.path();
        match FLock::is_path_held(dir.join("lock")) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(_) => continue,
        }
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
        let result_lock = ResultLock::new(cfg.lock_backend, lock_dir.clone());
        // Separate keyed lock, separate files: a scratch slot is keyed by slot id
        // and an addr's result by addr, and colliding those namespaces would let
        // one wait on the other for no reason.
        let scratch_lock_dir = lock_dir.join("scratch");
        std::fs::create_dir_all(&scratch_lock_dir)
            .with_context(|| format!("create scratch lock dir {scratch_lock_dir:?}"))?;
        let scratch_lock =
            crate::engine::scratch::ScratchLock::new(cfg.lock_backend, scratch_lock_dir);
        // Third namespace, third directory: a credential is keyed by its
        // resolution key, which is neither an addr nor a slot id.
        let credential_lock_dir = lock_dir.join("auth");
        std::fs::create_dir_all(&credential_lock_dir)
            .with_context(|| format!("create credential lock dir {credential_lock_dir:?}"))?;
        let credential_lock =
            crate::engine::credential::CredentialLock::new(cfg.lock_backend, credential_lock_dir);

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

        // Fails loudly here rather than at first request: every request's
        // memoizers spawn on this handle, and an engine constructed off-runtime
        // could only defer that failure to a stranger place.
        let runtime = tokio::runtime::Handle::try_current().context(
            "Engine::new must run inside a tokio runtime (request memoizers spawn on it)",
        )?;

        let mut engine = Engine {
            cfg: cfg.clone(),
            home: home.clone(),
            runtime: runtime.clone(),
            local_cache,
            local_cache_tmp,
            walker,
            providers: vec![],
            providers_by_name: HashMap::new(),
            drivers: vec![],
            drivers_by_name: HashMap::new(),
            hooks: vec![],
            requests: Mutex::new(HashMap::new()),
            exec_runners: Arc::new({
                let mut registry = hexecrunner::registry::RunnerRegistry::with_builtins();
                registry
                    .register(Arc::new(hexecrunner::session::SessionRunner::new()))
                    .context("register the builtin session exec runner")?;
                registry
            }),
            result_permits: {
                let pool = crate::engine::worker_pool::WorkerPool::new(max_workers);
                // Two readers of the same pool, for two different lines: the
                // permit accounting (`N max, N free, N running`) and the
                // `workers` limiter's saturation age. Both must sample when the
                // report is rendered, not at the last acquire.
                let free = {
                    let pool = Arc::clone(&pool);
                    move || pool.available()
                };
                crate::engine::diag::global().register_worker_pool(max_workers, free.clone());
                crate::engine::diag::global()
                    .limiter("workers")
                    .attach_gauge(free);
                pool
            },
            max_workers,
            plugin_factories: HashMap::new(),
            plugins: HashMap::new(),
            fuse,
            result_lock,
            scratch_lock,
            credential_cache: Default::default(),
            credential_lock,
            deferred_values: hcore::hmemoizer::Memoizer::with_tag_task("deferred_value", runtime),
            remote_caches,
            functions: Arc::new(FunctionRegistry::default()),
            function_slot: FunctionSlot::new(),
            remote_tmp_ready: tokio::sync::OnceCell::new(),
            scratch_audit_ready: tokio::sync::OnceCell::new(),
        };
        engine.register_plugin("group", |_| {
            Ok(PluginParts::default().with_driver(Box::new(hbuiltins::plugingroup::Driver)))
        })?;
        engine.register_plugin("scratch", |_| {
            Ok(PluginParts::default().with_driver(Box::new(hbuiltins::pluginscratch::Driver)))
        })?;
        // `auth.credential` plus the `heph.auth.*` functions — no provider: the
        // functions are the plugin's, so nothing has to stand in for one.
        engine.register_plugin(hbuiltins::plugincredential::PLUGIN_NAME, |_| {
            Ok(PluginParts::default()
                .with_driver(Box::new(hbuiltins::plugincredential::Driver))
                .with_functions(hbuiltins::plugincredential::functions::definitions()))
        })?;
        engine.register_plugin("query", |_| {
            Ok(
                PluginParts::default()
                    .with_provider(Box::new(hplugin_query::pluginquery::Provider)),
            )
        })?;

        // The `fs` provider, driver and `heph.fs.*` functions are always-on
        // built-ins. Each builds its `Ignore` from the same `PluginInit` (home +
        // `fs.skip` dirs/globs) the engine hands every plugin, so every fs glob
        // walk prunes the same paths. A bad `fs.skip` glob surfaces as an error
        // here.
        engine.register_plugin("fs", |init| {
            let functions_ignore = Arc::new(hwalk::Ignore::new(&init.skip_dirs, &init.skip_globs)?);
            let driver_ignore = Arc::new(hwalk::Ignore::new(&init.skip_dirs, &init.skip_globs)?);
            Ok(PluginParts::default()
                .with_provider(Box::new(hbuiltins::pluginfs::Provider))
                .with_driver(Box::new(hbuiltins::pluginfs::Driver::new(
                    driver_ignore,
                    init.walker.clone(),
                )))
                .with_functions(hbuiltins::pluginfs::functions(
                    &functions_ignore,
                    &init.walker,
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
    /// The scratch lineage this run writes to.
    pub fn scratch_scope(&self) -> &str {
        &self.cfg.scratch.scope
    }

    /// Lineages this run may read from when its own has nothing.
    pub fn scratch_restore_scopes(&self) -> &[String] {
        &self.cfg.scratch.restore_scopes
    }

    /// Whether a cold lineage seeds itself from the first warm fallback.
    pub fn scratch_seeds_on_fork(&self) -> bool {
        self.cfg.scratch.seed_on_fork
    }

    pub fn result_lock(&self) -> &ResultLock {
        &self.result_lock
    }

    /// Workspace root.
    pub fn root(&self) -> &std::path::Path {
        &self.cfg.root
    }

    /// The sealed plugin-function registry (every plugin's `heph.<p>.<fn>`),
    /// sealing it if nothing has yet. The same registry BUILD evaluation reads
    /// — the LSP assembles its Starlark globals from it, and `heph inspect
    /// functions` lists it. Built once, during registration; never rebuilt.
    pub fn function_registry(&self) -> Arc<FunctionRegistry> {
        self.seal_functions();
        Arc::clone(&self.functions)
    }

    /// Seal the function registry: from here on every plugin's slot resolves
    /// it, and registering another plugin is an error. Idempotent and cheap —
    /// called at the top of every provider dispatch, by which point
    /// registration has finished (registering needs `&mut self`, which a
    /// shared engine no longer hands out).
    pub(crate) fn seal_functions(&self) {
        self.function_slot.seal(&self.functions);
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

    /// Every `(plugin name, function name, rendered signature)` exposed across
    /// all registered plugins, sorted by plugin, then function. The rendered
    /// signature looks like `glob(pattern: string) -> list[string]`. Surfaced
    /// via `heph inspect functions`.
    pub fn functions(&self) -> Vec<(String, String, String)> {
        self.function_registry()
            .iter()
            .map(|(plugin, name, rf)| {
                (
                    plugin.to_string(),
                    name.to_string(),
                    rf.signature.render(name),
                )
            })
            .collect()
    }

    /// The [`PluginInit`] context handed to every plugin constructor (direct
    /// registration or factory): the plugin's name, the workspace root, the
    /// engine's skip dirs/globs, and its function slot.
    fn plugin_init_payload(&self, name: &str) -> PluginInit {
        PluginInit {
            name: name.to_string(),
            root: self.cfg.root.clone(),
            skip_dirs: self.skip_dirs(),
            skip_globs: self.skip_globs(),
            walker: self.walker.clone(),
            runtime: self.runtime.clone(),
            functions: Arc::clone(&self.function_slot),
        }
    }

    /// This engine's function slot — what a cdylib's registry handle resolves
    /// through.
    pub(crate) fn function_slot(&self) -> Arc<FunctionSlot> {
        Arc::clone(&self.function_slot)
    }

    /// Refuses `name` if it breaks the naming rule, is reserved, or is already
    /// taken by a registered plugin or a pending builtin factory.
    fn check_plugin_name(&self, name: &str, source: &str) -> anyhow::Result<()> {
        validate_plugin_name(name).with_context(|| format!("registering {source}"))?;
        let existing = self
            .plugins
            .get(name)
            .map(|p| p.source.clone())
            .or_else(|| {
                self.plugin_factories
                    .contains_key(name)
                    .then(|| format!("builtin plugin {name:?} (selectable from `plugins:`)"))
            });
        if let Some(existing) = existing {
            anyhow::bail!(
                "plugin name {name:?} is taken twice: by {existing}, and by {source}. \
                 Plugin names are unique across builtins and cdylibs; rename one of them"
            );
        }
        Ok(())
    }

    /// Registers an always-on builtin plugin: `build` runs now, with the
    /// [`PluginInit`] every plugin gets, and each part it returns is named after
    /// `name` (see [`component_name`]).
    pub fn register_plugin(
        &mut self,
        name: &str,
        build: impl FnOnce(&PluginInit) -> anyhow::Result<PluginParts>,
    ) -> anyhow::Result<()> {
        let source = format!("builtin plugin {name:?}");
        self.check_plugin_name(name, &source)?;
        let parts = build(&self.plugin_init_payload(name))
            .with_context(|| format!("constructing plugin {name:?}"))?;
        self.insert_plugin(name, source, parts)
    }

    /// The one place a plugin's parts join the engine. `name` has passed
    /// [`Self::check_plugin_name`]; every component name is resolved and
    /// checked before anything is registered, so a refused plugin leaves no
    /// part behind.
    pub(crate) fn insert_plugin(
        &mut self,
        name: &str,
        source: String,
        parts: PluginParts,
    ) -> anyhow::Result<()> {
        self.check_plugin_name(name, &source)?;
        if self.function_slot.is_sealed() {
            anyhow::bail!(
                "{source} registered after the engine sealed its function registry; every \
                 plugin must be registered before the first BUILD evaluation"
            );
        }
        let PluginParts {
            provider,
            drivers,
            functions,
            hooks,
            runners,
        } = parts;

        let provider = match provider {
            Some(provider) => {
                let local = provider
                    .config(provider::ConfigRequest {})
                    .with_context(|| format!("reading the provider config of plugin {name:?}"))?
                    .name;
                Some(Arc::new(Provider {
                    name: resolve_component_name(name, &local, "provider")?,
                    provider,
                }))
            }
            None => None,
        };

        let mut resolved: Vec<Arc<Driver>> = Vec::with_capacity(drivers.len());
        for driver in drivers {
            let driver: Box<dyn SDKDriver> = match driver {
                PluginDriver::Plain(d) => d,
                PluginDriver::Managed(m) => Box::new(self.new_managed_driver(m)),
            };
            let local = driver
                .config(driver::ConfigRequest {})
                .with_context(|| format!("reading a driver config of plugin {name:?}"))?
                .name;
            let full = resolve_component_name(name, &local, "driver")?;
            if resolved.iter().any(|d| d.name == full) {
                anyhow::bail!(
                    "plugin {name:?} has two drivers named {full:?}; give each driver of one \
                     plugin its own local name (at most one may omit it)"
                );
            }
            resolved.push(Arc::new(Driver { name: full, driver }));
        }

        // Plugin names are unique and a component name always starts with its
        // plugin's, so these only fire for a test-only bare registration racing
        // a component of the same name — kept as a guard, not a rule.
        if let Some(p) = &provider
            && self.providers_by_name.contains_key(&p.name)
        {
            anyhow::bail!("provider with name '{}' already registered", p.name);
        }
        if let Some(d) = resolved
            .iter()
            .find(|d| self.drivers_by_name.contains_key(&d.name))
        {
            anyhow::bail!("driver with name '{}' already registered", d.name);
        }

        let mut info = PluginInfo {
            name: name.to_string(),
            source: source.clone(),
            provider: provider.as_ref().map(|p| p.name.clone()),
            drivers: resolved.iter().map(|d| d.name.clone()).collect(),
            functions: functions
                .iter()
                .map(|f| format!("heph.{name}.{}", f.name))
                .collect(),
            runners: runners.iter().map(|r| r.name().to_string()).collect(),
            hooks: hooks.len(),
        };
        info.drivers.sort();
        info.functions.sort();
        info.runners.sort();

        // All or nothing, and refused before any other part lands. `get_mut`
        // fails only once the registry has been handed out, which sealing does.
        Arc::get_mut(&mut self.functions)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{source} registered after the engine handed out its function registry"
                )
            })?
            .insert(name, functions)
            .with_context(|| format!("registering the functions of {source}"))?;

        for runner in runners {
            self.register_exec_runner(runner)
                .with_context(|| format!("registering an exec runner of plugin {name:?}"))?;
        }
        if let Some(provider) = provider {
            self.providers.push(Arc::clone(&provider));
            self.providers_by_name
                .insert(provider.name.clone(), provider);
        }
        for driver in resolved {
            self.drivers.push(Arc::clone(&driver));
            self.drivers_by_name.insert(driver.name.clone(), driver);
        }
        self.hooks.extend(hooks);
        self.plugins.insert(name.to_string(), info);
        Ok(())
    }

    /// Every registered plugin with the full names of its components, sorted
    /// by plugin name. Surfaced via `heph tool plugins`.
    pub fn plugins(&self) -> Vec<&PluginInfo> {
        let mut plugins: Vec<&PluginInfo> = self.plugins.values().collect();
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        plugins
    }

    /// Test convenience: registers a one-component plugin named after the
    /// provider's own reported name, which must follow the plugin naming rule.
    /// Shipped code registers through [`Self::register_plugin`].
    pub fn register_provider(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKProvider>,
    ) -> anyhow::Result<()> {
        let provider = factory(&self.plugin_init_payload(BARE_PLUGIN_NAME));
        let name = provider
            .config(provider::ConfigRequest {})
            .context("reading the provider config")?
            .name;
        let source = format!("register_provider of provider {name:?}");
        self.insert_plugin(
            &name,
            source,
            PluginParts::default().with_provider(provider),
        )
    }

    /// Test convenience: registers a one-driver plugin named after the
    /// driver's own reported name. See [`Self::register_provider`].
    pub fn register_driver(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKDriver>,
    ) -> anyhow::Result<()> {
        let driver = factory(&self.plugin_init_payload(BARE_PLUGIN_NAME));
        self.insert_bare_driver(PluginDriver::Plain(driver))
    }

    /// Test convenience: registers a one-driver plugin named after the managed
    /// driver's own reported name. See [`Self::register_provider`].
    pub fn register_managed_driver(
        &mut self,
        factory: impl FnOnce(&PluginInit) -> Box<dyn SDKManagedDriver>,
    ) -> anyhow::Result<()> {
        let managed = factory(&self.plugin_init_payload(BARE_PLUGIN_NAME));
        self.insert_bare_driver(PluginDriver::Managed(managed))
    }

    fn insert_bare_driver(&mut self, driver: PluginDriver) -> anyhow::Result<()> {
        let name = match &driver {
            PluginDriver::Plain(d) => d.config(driver::ConfigRequest {})?.name,
            PluginDriver::Managed(m) => m.config(driver::ConfigRequest {})?.name,
        };
        let source = format!("register_driver of driver {name:?}");
        let parts = PluginParts {
            drivers: vec![driver],
            ..Default::default()
        };
        self.insert_plugin(&name, source, parts)
    }

    /// Hand `hexecrunner` the resolver for this engine.
    ///
    /// Must be called once the engine is behind an `Arc`, because the resolver
    /// holds a `Weak` back to it. Both the CLI and the test harness do this
    /// immediately after `Arc::new`; without it, a target naming a runner fails
    /// with "no runner host is installed" rather than silently spawning
    /// locally.
    pub fn install_exec_runner_host(self: &Arc<Self>) {
        hexecrunner::install_host(Arc::new(
            crate::engine::execrunner_host::EngineRunnerHost::new(
                Arc::downgrade(self),
                Arc::clone(&self.exec_runners),
            ),
        ));
    }

    /// Register an exec runner a plugin implements, as a peer of the builtins.
    ///
    /// Takes `&mut self` deliberately. Plugins are loaded while the engine is
    /// still owned (`register_plugins` runs before `Arc::new`), so the registry
    /// is still uniquely referenced and `Arc::get_mut` succeeds. Once the engine
    /// is behind an `Arc` and the resolver holds a clone, the set of runners is
    /// fixed for the process — which is the property the by-name dispatch wants:
    /// no runner can appear after a target has already been told the name is
    /// unknown.
    pub fn register_exec_runner(
        &mut self,
        runner: Arc<dyn hexecrunner::registry::ExecRunner>,
    ) -> anyhow::Result<()> {
        let name = runner.name().to_string();
        Arc::get_mut(&mut self.exec_runners)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "exec runner '{name}' registered after the engine was shared. Runners must be \
                     registered during plugin load, before the resolver is handed a clone of the \
                     registry."
                )
            })?
            .register(runner)
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

    /// Registers a YAML-selected builtin: `factory` runs only when a
    /// `plugins: - { builtin: <name> }` entry selects it (see
    /// [`Self::apply_builtin`]), with that entry's `options:`.
    pub fn register_plugin_factory(
        &mut self,
        name: &str,
        factory: impl FnOnce(&PluginInit, &Options) -> anyhow::Result<PluginParts>
        + Send
        + Sync
        + 'static,
    ) -> anyhow::Result<()> {
        self.check_plugin_name(name, &format!("builtin plugin factory {name:?}"))?;
        self.plugin_factories
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
        std::iter::once(hwalk::GIT_SKIP_GLOB.to_string())
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
    /// entry), running its registered factory with `options` and registering
    /// the parts it returns under `name`. Errors if `name` has no registered
    /// factory. Factories are consumed — applying the same name twice errors.
    pub fn apply_builtin(&mut self, name: &str, options: &Options) -> anyhow::Result<()> {
        let Some(factory) = self.plugin_factories.remove(name) else {
            if self.plugins.contains_key(name) {
                anyhow::bail!("builtin plugin '{name}' is already registered");
            }
            anyhow::bail!("unknown builtin plugin '{name}'");
        };
        let parts = factory(&self.plugin_init_payload(name), options)
            .with_context(|| format!("constructing builtin plugin {name:?}"))?;
        self.insert_plugin(name, format!("builtin plugin {name:?}"), parts)
    }
}

impl hplugin::lsp::LspEngine for Engine {
    fn root(&self) -> &std::path::Path {
        &self.cfg.root
    }

    fn function_registry(&self) -> Arc<FunctionRegistry> {
        Engine::function_registry(self)
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

#[cfg(test)]
mod tests {
    use super::*;

    // Names the dir after pid 1 (init/launchd): always alive, always owned by
    // another user, so a non-root caller's `kill(1, 0)` always answers
    // `EPERM`. That is precisely the case the old `kill(pid, 0) == 0` check
    // got backwards — `EPERM` means the process exists and isn't ours, not
    // that it's dead — and it is what let the sweep reclaim a live process's
    // sandbox out from under it. The fix doesn't probe the pid at all: it
    // probes whether `<dir>/lock` is held, which this test pins by holding it
    // itself for the sweep's whole duration.
    #[test]
    fn sweep_preserves_a_dir_whose_lock_is_held() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join("sandboxfuse1");
        std::fs::create_dir_all(&dir).expect("create sandboxfuse dir");
        let guard = FLock::new(dir.join("lock"))
            .try_lock()
            .expect("try_lock")
            .expect("lock free");

        sweep_stale_sandboxfuse_dirs(home.path());

        assert!(
            dir.exists(),
            "a dir whose lock is held must survive the sweep"
        );
        drop(guard);
    }

    #[test]
    fn sweep_removes_a_dir_whose_lock_is_not_held() {
        let home = tempfile::tempdir().expect("tempdir");
        let dir = home.path().join("sandboxfuse2");
        std::fs::create_dir_all(&dir).expect("create sandboxfuse dir");
        // A lock file can exist without being held (its owner exited without
        // unlinking it, or it was never acquired): `try_lock` then drop
        // leaves exactly that on disk.
        drop(
            FLock::new(dir.join("lock"))
                .try_lock()
                .expect("try_lock")
                .expect("lock free"),
        );

        sweep_stale_sandboxfuse_dirs(home.path());

        assert!(
            !dir.exists(),
            "a dir whose lock is not held must be reclaimed"
        );
    }

    #[test]
    fn sweep_ignores_entries_that_do_not_match_the_naming_scheme() {
        let home = tempfile::tempdir().expect("tempdir");
        let unrelated = home.path().join("sandboxfuse-not-a-pid");
        std::fs::create_dir_all(&unrelated).expect("create dir");

        sweep_stale_sandboxfuse_dirs(home.path());

        assert!(
            unrelated.exists(),
            "a dir whose suffix isn't a pid must be left alone"
        );
    }

    mod plugins {
        use super::super::*;
        use crate::engine::driver::{
            ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse,
            DriverSchema, ParseRequest, ParseResponse, RunRequest, RunResponse,
        };
        use crate::engine::fault_provider::{FaultProvider, Faults};
        use hcore::hasync::Cancellable;

        /// A driver that only has a name: registration never runs it.
        struct NamedDriver(&'static str);

        #[async_trait::async_trait]
        impl SDKDriver for NamedDriver {
            fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
                Ok(ConfigResponse {
                    name: self.0.to_string(),
                })
            }
            fn schema(&self) -> DriverSchema {
                DriverSchema::default()
            }
            async fn parse(
                &self,
                _req: ParseRequest,
                _ctoken: &(dyn Cancellable + Send + Sync),
            ) -> anyhow::Result<ParseResponse> {
                anyhow::bail!("not under test")
            }
            async fn apply_transitive(
                &self,
                _req: ApplyTransitiveRequest,
                _ctoken: &(dyn Cancellable + Send + Sync),
            ) -> anyhow::Result<ApplyTransitiveResponse> {
                anyhow::bail!("not under test")
            }
            async fn run<'a, 'io>(
                &self,
                _req: RunRequest<'a, 'io>,
                _ctoken: &(dyn Cancellable + Send + Sync),
            ) -> anyhow::Result<RunResponse> {
                anyhow::bail!("not under test")
            }
            async fn run_shell<'a, 'io>(
                &self,
                _req: RunRequest<'a, 'io>,
                _ctoken: &(dyn Cancellable + Send + Sync),
            ) -> anyhow::Result<RunResponse> {
                anyhow::bail!("not under test")
            }
        }

        fn provider(name: &'static str) -> Box<dyn SDKProvider> {
            Box::new(
                FaultProvider::new(
                    vec![],
                    Faults {
                        name: Some(name),
                        ..Default::default()
                    },
                )
                .expect("fault provider"),
            )
        }

        fn engine() -> (tempfile::TempDir, Engine) {
            let root = tempfile::tempdir().expect("tempdir");
            let engine = Engine::new(Config {
                root: root.path().to_path_buf(),
                home_dir: root.path().join(".heph"),
                ..Default::default()
            })
            .expect("engine");
            (root, engine)
        }

        fn sorted(names: impl Iterator<Item = String>) -> Vec<String> {
            let mut v: Vec<String> = names.collect();
            v.sort();
            v
        }

        #[tokio::test]
        async fn provider_and_driver_names_follow_the_plugin() {
            let (_root, mut e) = engine();
            e.register_plugin("mine", |_| {
                Ok(PluginParts::default()
                    .with_provider(provider(""))
                    .with_driver(Box::new(NamedDriver("")))
                    .with_driver(Box::new(NamedDriver("extra"))))
            })
            .expect("register");
            assert!(
                e.providers_by_name.contains_key("mine"),
                "no local name → <plugin>"
            );
            assert!(
                e.drivers_by_name.contains_key("mine"),
                "no local name → <plugin>"
            );
            assert!(
                e.drivers_by_name.contains_key("mine.extra"),
                "local name → <plugin>.<name>"
            );
            assert!(!e.drivers_by_name.contains_key("extra"), "never bare");

            // A local name equal to its plugin's is the plugin's own.
            e.register_plugin("same", |_| {
                Ok(PluginParts::default()
                    .with_provider(provider("same"))
                    .with_driver(Box::new(NamedDriver("same"))))
            })
            .expect("register");
            assert!(e.providers_by_name.contains_key("same"));
            assert!(e.drivers_by_name.contains_key("same"));

            // Two drivers of one plugin that both omit the name collide, and
            // the plugin registers nothing.
            let err = e
                .register_plugin("twice", |_| {
                    Ok(PluginParts::default()
                        .with_provider(provider(""))
                        .with_driver(Box::new(NamedDriver("")))
                        .with_driver(Box::new(NamedDriver(""))))
                })
                .expect_err("two unnamed drivers");
            let msg = format!("{err:#}");
            assert!(msg.contains("plugin \"twice\""), "{msg}");
            assert!(
                !e.providers_by_name.contains_key("twice"),
                "nothing half-registered"
            );
            assert!(!e.drivers_by_name.contains_key("twice"));
            // Nor is the name taken: the plugin can be registered once fixed.
            e.register_plugin("twice", |_| {
                Ok(PluginParts::default().with_driver(Box::new(NamedDriver(""))))
            })
            .expect("name is still free");
        }

        #[tokio::test]
        async fn duplicate_plugin_name_is_refused() {
            let (_root, mut e) = engine();

            // Builtin vs builtin: `fs` is registered by `Engine::new`.
            let err = e
                .register_plugin("fs", |_| Ok(PluginParts::default()))
                .expect_err("second fs");
            let msg = format!("{err:#}");
            assert_eq!(
                msg,
                "plugin name \"fs\" is taken twice: by builtin plugin \"fs\", and by builtin \
                 plugin \"fs\". Plugin names are unique across builtins and cdylibs; rename one \
                 of them"
            );

            // A cdylib named after a builtin: both sources are named.
            let err = e
                .insert_plugin(
                    "fs",
                    "cdylib plugin /p/libfs.so (manifest /p/heph-fs-plugin.json)".to_string(),
                    PluginParts::default().with_driver(Box::new(NamedDriver(""))),
                )
                .expect_err("cdylib fs");
            let msg = format!("{err:#}");
            assert!(msg.contains("by builtin plugin \"fs\""), "{msg}");
            assert!(
                msg.contains("by cdylib plugin /p/libfs.so (manifest /p/heph-fs-plugin.json)"),
                "{msg}"
            );

            // A pending YAML builtin holds its name too.
            e.register_plugin_factory("lazy", |_, _| Ok(PluginParts::default()))
                .expect("factory");
            let err = e
                .register_plugin("lazy", |_| Ok(PluginParts::default()))
                .expect_err("taken by a factory");
            assert!(format!("{err:#}").contains("selectable from `plugins:`"));

            // A bare test registration is a plugin like any other.
            let err = e
                .register_driver(|_| Box::new(NamedDriver("group")))
                .expect_err("group is a builtin");
            assert!(format!("{err:#}").contains("plugin name \"group\" is taken twice"));
        }

        #[tokio::test]
        async fn plugin_named_core_is_refused() {
            let (_root, mut e) = engine();
            let err = e
                .register_plugin("core", |_| Ok(PluginParts::default()))
                .expect_err("core is reserved");
            assert_eq!(
                format!("{err:#}"),
                "registering builtin plugin \"core\": plugin name \"core\" is reserved (it is \
                 the built-in `heph.core` namespace); choose another name"
            );
            assert!(!e.plugins.contains_key("core"));
        }

        #[tokio::test]
        async fn invalid_plugin_name_is_refused() {
            let (_root, mut e) = engine();
            for name in ["", "a.b", "my-plugin", "Go", "1st", "a b", "é"] {
                let err = e
                    .register_plugin(name, |_| Ok(PluginParts::default()))
                    .expect_err(name);
                assert_eq!(
                    format!("{err:#}"),
                    format!(
                        "registering builtin plugin {name:?}: invalid plugin name {name:?}: a \
                         plugin name must match `[a-z_][a-z0-9_]*`, because it is the \
                         `heph.<name>` namespace in BUILD files"
                    )
                );
            }
            // The rule's edges are accepted.
            for name in ["_", "_x", "a1_b2"] {
                e.register_plugin(name, |_| Ok(PluginParts::default()))
                    .unwrap_or_else(|err| panic!("{name}: {err:#}"));
            }
            // A local name follows the same rule, so `<plugin>.<local>` parses.
            let err = e
                .register_plugin("ok", |_| {
                    Ok(PluginParts::default().with_driver(Box::new(NamedDriver("a.b"))))
                })
                .expect_err("dotted local name");
            assert!(format!("{err:#}").contains("local name \"a.b\""));
        }

        /// A function that only has a name: registration never calls it.
        struct NoopFn;

        #[async_trait::async_trait]
        impl hplugin::function::PluginFn for NoopFn {
            async fn call(
                &self,
                _: &hplugin::function::FnCallContext<'_>,
                _: hplugin::function::FnArgs,
            ) -> anyhow::Result<hplugin::function::FnOutcome> {
                anyhow::bail!("not under test")
            }
        }

        fn function(name: &str) -> PluginFnDef {
            use hcore::htvalue::signature::{FnSignature, ParamType};
            PluginFnDef {
                name: name.to_string(),
                signature: FnSignature {
                    positional: vec![],
                    named: vec![],
                    variadic: None,
                    returns: ParamType::String,
                },
                doc: String::new(),
                func: Arc::new(NoopFn),
            }
        }

        #[tokio::test]
        async fn builtin_bundle_registers_provider_drivers_and_functions() {
            let (_root, mut e) = engine();
            e.register_plugin_factory("bundle", |init, opts| {
                // The factory sees the same init every plugin gets — its own
                // name included — and its own YAML options.
                assert!(init.root.is_absolute());
                assert_eq!(init.name, "bundle");
                let local: String =
                    hplugin::config::decode_opt(opts, "bundle", "local")?.unwrap_or_default();
                let local: &'static str = Box::leak(local.into_boxed_str());
                Ok(PluginParts::default()
                    .with_provider(provider(""))
                    .with_driver(Box::new(NamedDriver("")))
                    .with_driver(Box::new(NamedDriver(local)))
                    .with_functions(vec![function(local)]))
            })
            .expect("factory");
            assert!(
                !e.providers_by_name.contains_key("bundle"),
                "a factory waits for config"
            );

            let opts: Options = serde_yaml::from_str("local: from_opts").expect("opts");
            e.apply_builtin("bundle", &opts).expect("apply");
            assert_eq!(
                sorted(
                    e.drivers_by_name
                        .keys()
                        .filter(|n| n.starts_with("bundle"))
                        .cloned()
                ),
                vec!["bundle", "bundle.from_opts"]
            );
            assert!(e.providers_by_name.contains_key("bundle"));
            assert!(
                e.function_registry().get("bundle", "from_opts").is_some(),
                "the bundle's function registers under the bundle's name"
            );

            let info = e
                .plugins()
                .into_iter()
                .find(|p| p.name == "bundle")
                .expect("bundle is listed");
            assert_eq!(info.source, "builtin plugin \"bundle\"");
            assert_eq!(info.provider.as_deref(), Some("bundle"));
            assert_eq!(info.drivers, vec!["bundle", "bundle.from_opts"]);
            assert_eq!(info.functions, vec!["heph.bundle.from_opts"]);
        }

        /// A builtin applied twice is refused, before the seal.
        #[tokio::test]
        async fn builtin_bundle_applies_once() {
            let (_root, mut e) = engine();
            e.register_plugin_factory("bundle", |_, _| Ok(PluginParts::default()))
                .expect("factory");
            let opts = Options::default();
            e.apply_builtin("bundle", &opts).expect("apply");
            let err = e.apply_builtin("bundle", &opts).expect_err("applied twice");
            assert_eq!(
                format!("{err:#}"),
                "builtin plugin 'bundle' is already registered"
            );
        }

        /// D6: the slot every plugin gets errs until the engine seals it, and
        /// resolves the engine's registry after.
        #[tokio::test]
        async fn registry_read_before_seal_is_an_error() {
            let (_root, mut e) = engine();
            let mut seen = None;
            e.register_plugin("early", |init| {
                seen = Some(Arc::clone(&init.functions));
                Ok(PluginParts::default().with_functions(vec![function("f")]))
            })
            .expect("register");
            let slot = seen.expect("init carries the slot");
            let err = slot.get().expect_err("read before the seal");
            assert!(
                format!("{err:#}").contains("before every plugin was registered"),
                "{err:#}"
            );

            e.seal_functions();
            let registry = slot.get().expect("sealed");
            assert!(registry.get("early", "f").is_some());
            assert!(registry.get("fs", "glob").is_some());
        }

        /// D6: once the registry is sealed, registering a plugin is an error
        /// naming the plugin — never a function that silently misses the
        /// namespace every BUILD file already sees.
        #[tokio::test]
        async fn plugin_registered_after_seal_is_refused() {
            let (_root, mut e) = engine();
            let _ = e.function_registry();
            let err = e
                .register_plugin("late", |_| {
                    Ok(PluginParts::default().with_functions(vec![function("f")]))
                })
                .expect_err("registered after the seal");
            let msg = format!("{err:#}");
            assert!(msg.contains("builtin plugin \"late\""), "{msg}");
            assert!(msg.contains("after the engine sealed"), "{msg}");
            assert!(e.function_registry().get("late", "f").is_none());
        }

        /// I2: a function-bearing plugin may have no provider; the two never
        /// shared a namespace by accident.
        #[tokio::test]
        async fn auth_is_functions_and_a_driver_without_a_provider() {
            let (_root, e) = engine();
            assert!(!e.providers_by_name.contains_key("auth"));
            assert!(e.drivers_by_name.contains_key("auth.credential"));
            assert!(e.function_registry().get("auth", "env").is_some());
        }
    }
}
