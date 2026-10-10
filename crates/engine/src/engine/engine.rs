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
    pub root: PathBuf,
    /// This checkout's own heph home — the in-process counterpart of the cdylib
    /// `CreateConfig.home`. The checkout's, not the shared one: its one consumer
    /// (the OCI runner) mounts it so a container sees this checkout's
    /// sandboxes, which live there.
    pub home: crate::engine::CheckoutHome,
    /// The home every checkout shares — the in-process counterpart of
    /// `CreateConfig.shared_home`. The same directory as `home` outside a
    /// linked git worktree. The OCI runner mounts it too: a sandbox's scratch
    /// symlinks point into it.
    pub shared_home: crate::engine::HomeDir,
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
    /// This checkout's own durable store, holding the entries of targets that
    /// never go to a remote cache ([`CacheScope::Checkout`]). `None` when the
    /// checkout home is the shared home: then there is one store,
    /// `local_cache`. Read through [`Engine::local_cache_for`].
    ///
    /// [`CacheScope::Checkout`]: crate::engine::local_cache::CacheScope::Checkout
    pub(crate) checkout_cache: Option<CheckoutCacheStore>,
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

    /// Directories of [revision pins](hdriver_support::revision_pin) — files a
    /// driver keeps alive for one cached revision, such as nix gcroots.
    /// `heph tool gc` removes each pin whose revision it no longer caches. See
    /// [`Engine::register_revision_pins`].
    pub(crate) revision_pin_dirs: Vec<std::path::PathBuf>,

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
    /// The workspace's shared heph home ([`Homes::shared`]), as resolved into
    /// the [`Config`]: in a linked git worktree, the main checkout's. The cache,
    /// gateway/revision locks, credentials, scratch, diag.
    ///
    /// Named for which home it is, next to `checkout_home`, so every use
    /// chooses one.
    ///
    /// [`Homes::shared`]: crate::engine::Homes::shared
    pub shared_home: crate::engine::HomeDir,
    /// This checkout's own home ([`Homes::checkout`]): sandboxes, their FUSE
    /// mount and execute lock, staged inputs, the fswalk cache, approvals. A
    /// sandbox only ever links to things in this home. The same directory
    /// as `shared_home` outside a linked worktree.
    ///
    /// [`Homes::checkout`]: crate::engine::Homes::checkout
    pub checkout_home: crate::engine::CheckoutHome,
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

    /// Aggregates every provider's exposed functions and injects the registry
    /// into consumers (the buildfile provider) exactly once, lazily on the first
    /// provider dispatch — by which point all registration has completed.
    pub(crate) provider_functions_wired: std::sync::Once,

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

/// Create the home if needed and make it ignore itself: a `<home>/.gitignore`
/// of `*`, written only when absent (a user's own file is left alone). The
/// default home's name changed (`.heph3` → `.heph`), and a repo's `.gitignore`
/// that named the old one would otherwise show the new one as untracked — this
/// holds for any name and any `homeDir`.
///
/// Sandboxes live under this `.gitignore`, so a tool inside a sandbox that
/// honours VCS ignores (git, ripgrep, many linters walking "the repo") sees the
/// sandbox as ignored. Deliberate, and the same as before the rename: the old
/// `.heph3` was ignored by the repos that used it too.
fn ensure_home(home: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(home)
        .with_context(|| format!("creating heph home {}", home.display()))?;
    let ignore = home.join(".gitignore");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&ignore)
    {
        Ok(mut f) => {
            use std::io::Write as _;
            f.write_all(b"*\n")
                .with_context(|| format!("writing {}", ignore.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating {}", ignore.display())),
    }
}

/// What opening a durable local cache store needs from the [`Config`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct StoreOptions {
    mem: crate::engine::config::MemCacheOptions,
    spill_threshold_bytes: u64,
    parallelism: usize,
}

impl StoreOptions {
    /// The durable store under `dir` (`<home>/cache`): manifests and small or
    /// medium blobs in sqlite, large blobs spilled to plain files, fronted by
    /// the mem tier unless it is disabled.
    fn open(self, dir: &Path) -> anyhow::Result<Arc<dyn LocalCache>> {
        let sqlite: Arc<dyn LocalCache> = Arc::new(LocalCacheSQLite::with_pipe_limit(
            dir.join("cache.db"),
            self.mem.per_entry_bytes,
            2 * self.parallelism,
        )?);
        let blobs = Arc::new(crate::engine::local_cache_fs::LocalCacheFS::new(
            dir.join("blobs"),
        )?);
        let durable: Arc<dyn LocalCache> =
            Arc::new(crate::engine::local_cache_spill::LocalCacheSpill::new(
                sqlite,
                blobs,
                usize::try_from(self.spill_threshold_bytes).unwrap_or(usize::MAX),
            ));
        Ok(if self.mem.capacity_bytes == 0 {
            durable
        } else {
            Arc::new(LocalCacheMem::new(
                durable,
                self.mem.per_entry_bytes,
                self.mem.capacity_bytes,
            ))
        })
    }
}

/// The checkout's own durable store ([`CacheScope::Checkout`]): opened at
/// construction when its database exists (the steady state), created on first
/// use otherwise.
///
/// [`CacheScope::Checkout`]: crate::engine::local_cache::CacheScope::Checkout
pub(crate) struct CheckoutCacheStore {
    dir: PathBuf,
    opts: StoreOptions,
    store: std::sync::OnceLock<Arc<dyn LocalCache>>,
    /// Serializes the first-use creation, so two first users cannot both open
    /// the sqlite db.
    opening: Mutex<()>,
}

impl CheckoutCacheStore {
    /// Opens the store now when its database is already on disk, so the
    /// steady state never opens sqlite lazily — on a runtime worker, under
    /// `opening`, parking every other worker that needs the store meanwhile.
    /// Only the very first local-only build of a checkout pays that, once.
    fn new(dir: PathBuf, opts: StoreOptions) -> anyhow::Result<Self> {
        let store = if dir.join("cache.db").exists() {
            std::sync::OnceLock::from(
                opts.open(&dir)
                    .with_context(|| format!("opening the checkout's cache {}", dir.display()))?,
            )
        } else {
            std::sync::OnceLock::new()
        };
        Ok(Self {
            dir,
            opts,
            store,
            opening: Mutex::new(()),
        })
    }

    /// Whether the store is open (tests).
    #[cfg(test)]
    fn is_open(&self) -> bool {
        self.store.get().is_some()
    }

    fn get(&self) -> anyhow::Result<Arc<dyn LocalCache>> {
        if let Some(s) = self.store.get() {
            return Ok(Arc::clone(s));
        }
        let _opening = self.opening.lock();
        if let Some(s) = self.store.get() {
            return Ok(Arc::clone(s));
        }
        let s = self
            .opts
            .open(&self.dir)
            .with_context(|| format!("opening the checkout's cache {}", self.dir.display()))?;
        Ok(Arc::clone(self.store.get_or_init(|| s)))
    }

    /// The store if it holds anything: open already, or a database on disk.
    /// What gc and clean sweep — they must not create a store to find it
    /// empty.
    fn existing(&self) -> anyhow::Result<Option<Arc<dyn LocalCache>>> {
        if self.store.get().is_some() || self.dir.join("cache.db").exists() {
            self.get().map(Some)
        } else {
            Ok(None)
        }
    }
}

impl Engine {
    /// The local cache store for `scope`. One store when the checkout home is
    /// the shared home; otherwise the checkout's own is opened on first use.
    pub(crate) fn local_cache_for(
        &self,
        scope: crate::engine::local_cache::CacheScope,
    ) -> anyhow::Result<Arc<dyn LocalCache>> {
        use crate::engine::local_cache::CacheScope;
        match (scope, &self.checkout_cache) {
            (CacheScope::Checkout, Some(store)) => store.get(),
            (CacheScope::Shared, _) | (CacheScope::Checkout, None) => {
                Ok(Arc::clone(&self.local_cache))
            }
        }
    }

    /// Every distinct store that holds entries, with its scope: the shared one,
    /// and the checkout's when it is a separate store that exists. What gc and
    /// clean sweep.
    pub(crate) fn local_caches(
        &self,
    ) -> anyhow::Result<Vec<(crate::engine::local_cache::CacheScope, Arc<dyn LocalCache>)>> {
        use crate::engine::local_cache::CacheScope;
        let mut out = vec![(CacheScope::Shared, Arc::clone(&self.local_cache))];
        if let Some(store) = &self.checkout_cache
            && let Some(c) = store.existing()?
        {
            out.push((CacheScope::Checkout, c));
        }
        Ok(out)
    }

    pub fn new(cfg: Config) -> anyhow::Result<Engine> {
        let home = cfg.homes.shared().clone();
        let checkout_home = cfg.homes.checkout().clone();
        tracing::debug!(
            root = %cfg.root.display(),
            home = %home.display(),
            checkout_home = %checkout_home.display(),
            "engine home: {}",
            cfg.homes.sharing()
        );
        // Both homes ignore themselves: in a linked worktree the checkout's is
        // inside the worktree's tree, the shared one inside the main checkout's.
        ensure_home(&home)?;
        if checkout_home.as_path() != home.as_path() {
            ensure_home(&checkout_home)?;
        }

        let parallelism = cfg.parallelism.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        });

        let store_opts = StoreOptions {
            mem: cfg.mem_cache,
            spill_threshold_bytes: cfg.spill_threshold_bytes,
            parallelism,
        };
        let local_cache = store_opts.open(&home.join("cache"))?;
        // The checkout's own store, for the entries that must not be shared
        // (see `CacheScope`). Opened here when it already exists, like the
        // shared one; created on first use otherwise: most worktrees build no
        // local-only target and never need it.
        let checkout_cache = (checkout_home.as_path() != home.as_path())
            .then(|| CheckoutCacheStore::new(checkout_home.join("cache"), store_opts))
            .transpose()?;

        // Mem-only tier for tmp/uncacheable revisions; spills oversized or
        // over-budget entries to the durable cache so a reader never misses.
        let local_cache_tmp: Arc<dyn LocalCache> =
            Arc::new(crate::engine::local_cache_tmp::LocalCacheTmp::new(
                local_cache.clone(),
                cfg.tmp_cache.per_entry_bytes,
                cfg.tmp_cache.capacity_bytes,
            ));

        // Best-effort sweep of stale `sandboxfuse<pid>` dirs from crashed runs.
        // The FUSE mount is where sandboxes live when it is on, so it is the
        // checkout's, like the sandboxes themselves.
        sweep_stale_sandboxfuse_dirs(&checkout_home);

        let fuse = Arc::new(EngineFuse::new(cfg.fuse, &checkout_home)?);

        let lock_dir = home.join("lock");
        std::fs::create_dir_all(&lock_dir)
            .with_context(|| format!("create lock dir {lock_dir:?}"))?;
        // The execute lock guards this checkout's sandbox path, so it lives
        // with the sandboxes: two checkouts running one addr each own a
        // sandbox and must not wait on each other. The gateway and revision
        // locks guard the shared cache entry and stay in the shared home.
        let execute_lock_dir = checkout_home.join("lock");
        std::fs::create_dir_all(&execute_lock_dir)
            .with_context(|| format!("create lock dir {execute_lock_dir:?}"))?;
        let result_lock = ResultLock::new(cfg.lock_backend, lock_dir.clone(), execute_lock_dir);
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
        let remote_caches =
            crate::engine::RemoteCacheSet::new(&cfg.remote_caches, home.to_path_buf())
                .context("configure remote caches")?;

        // Shared cross-run filesystem-walk cache, handed to tree-walking plugins
        // via `PluginInit`. Its own sqlite db so it can be pruned independently.
        // Per checkout: it caches *this* working tree's directory listings.
        let walker = Arc::new(hwalk::CachedWalker::open(
            &checkout_home.join("cache").join("fswalk.db"),
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
            shared_home: home.clone(),
            checkout_home,
            runtime: runtime.clone(),
            local_cache,
            local_cache_tmp,
            checkout_cache,
            walker,
            providers: vec![],
            providers_by_name: HashMap::new(),
            drivers: vec![],
            drivers_by_name: HashMap::new(),
            hooks: vec![],
            revision_pin_dirs: vec![],
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
            provider_factories: HashMap::new(),
            driver_factories: HashMap::new(),
            managed_driver_factories: HashMap::new(),
            fuse,
            result_lock,
            scratch_lock,
            credential_cache: Default::default(),
            credential_lock,
            deferred_values: hcore::hmemoizer::Memoizer::with_tag_task("deferred_value", runtime),
            remote_caches,
            provider_functions_wired: std::sync::Once::new(),
            remote_tmp_ready: tokio::sync::OnceCell::new(),
            scratch_audit_ready: tokio::sync::OnceCell::new(),
        };
        engine.register_driver(|_| Box::new(hbuiltins::plugingroup::Driver))?;
        engine.register_driver(|_| Box::new(hbuiltins::pluginscratch::Driver))?;
        engine.register_driver(|_| Box::new(hbuiltins::plugincredential::Driver))?;
        // Serves no targets; it exists to carry `heph.auth.*` into BUILD files.
        engine.register_provider(|_| Box::new(hbuiltins::plugincredential::functions::Provider))?;
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
            home: self.checkout_home.clone(),
            shared_home: self.shared_home.clone(),
            skip_dirs: self.skip_dirs(),
            skip_globs: self.skip_globs(),
            walker: self.walker.clone(),
            runtime: self.runtime.clone(),
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

    /// Have `heph tool gc` sweep `dir`, a directory of
    /// [revision pins](hdriver_support::revision_pin): after the cache trim,
    /// each pin whose revision is in none of this checkout's stores is removed,
    /// unless the revision is being built or read.
    ///
    /// A pin is judged against this checkout's stores only, so `dir` must hold
    /// the pins of this checkout's revisions — for a target whose entries are
    /// per checkout ([`CacheScope::Checkout`]), a directory in the checkout's
    /// home.
    ///
    /// [`CacheScope::Checkout`]: crate::engine::local_cache::CacheScope::Checkout
    pub fn register_revision_pins(&mut self, dir: std::path::PathBuf) {
        self.revision_pin_dirs.push(dir);
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
        // Both homes: in a linked worktree the checkout's is inside this tree,
        // and the shared one may be too (a worktree nested in the main checkout).
        let mut dirs = vec![self.shared_home.to_path_buf()];
        if self.checkout_home.as_path() != self.shared_home.as_path() {
            dirs.push(self.checkout_home.to_path_buf());
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_ignores_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let engine = Engine::new(Config::for_tests(dir.path())).expect("engine");
        let ignore = engine.shared_home.join(".gitignore");
        assert_eq!(std::fs::read_to_string(&ignore).expect("read"), "*\n");

        // A file the user wrote is left alone.
        std::fs::write(&ignore, "custom\n").expect("write");
        drop(engine);
        let engine = Engine::new(Config::for_tests(dir.path())).expect("engine");
        assert_eq!(
            std::fs::read_to_string(engine.shared_home.join(".gitignore")).expect("read"),
            "custom\n"
        );
    }

    /// In a linked worktree both homes ignore themselves: the checkout's sits
    /// inside the worktree's tree, the shared one inside the main checkout's.
    #[test]
    fn both_homes_ignore_themselves_in_a_worktree() {
        use hconfig::git_checkout::test_layout::{linked_worktree, main_checkout};
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let main = base.join("main");
        let wt = base.join("wt");
        main_checkout(&main, "master");
        linked_worktree(&main, &wt, "wt", "feat");

        let _rt = crate::engine::test_rt_enter();
        let engine = Engine::new(Config::for_tests_detected(&wt)).expect("engine");
        assert_ne!(engine.shared_home.as_path(), engine.checkout_home.as_path());
        for home in [engine.shared_home.as_path(), engine.checkout_home.as_path()] {
            assert_eq!(
                std::fs::read_to_string(home.join(".gitignore")).expect("read"),
                "*\n",
                "{}",
                home.display()
            );
        }
        assert_eq!(
            engine.plugin_init_payload().home.as_path(),
            wt.join(".heph"),
            "plugins get the checkout's home"
        );
        assert_eq!(
            engine.plugin_init_payload().shared_home.as_path(),
            main.join(".heph"),
            "and the shared home beside it"
        );
    }

    #[test]
    fn cache_scope_follows_remote_eligibility() {
        use crate::engine::driver::targetdef::CacheConfig;
        use crate::engine::local_cache::CacheScope;
        assert_eq!(CacheScope::of(&CacheConfig::on(true)), CacheScope::Shared);
        assert_eq!(
            CacheScope::of(&CacheConfig::on(false)),
            CacheScope::Checkout
        );
        assert_eq!(CacheScope::of(&CacheConfig::off()), CacheScope::Checkout);
    }

    /// One home, one store: both scopes are the same `LocalCache`, so nothing
    /// changes outside a linked worktree.
    #[test]
    fn one_home_is_one_store() {
        use crate::engine::local_cache::CacheScope;
        let dir = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let engine = Engine::new(Config::for_tests(dir.path())).expect("engine");
        let shared = engine.local_cache_for(CacheScope::Shared).expect("shared");
        let checkout = engine
            .local_cache_for(CacheScope::Checkout)
            .expect("checkout");
        assert!(Arc::ptr_eq(&shared, &checkout));
        assert_eq!(engine.local_caches().expect("stores").len(), 1);
    }

    /// In a linked worktree the checkout scope is its own store, under the
    /// worktree's home, opened on first use: what one scope holds the other
    /// does not see, and gc/clean only list it once it exists.
    #[test]
    fn a_worktree_has_a_checkout_store_of_its_own() {
        use crate::engine::local_cache::{CacheScope, MANIFEST_V1};
        use hconfig::git_checkout::test_layout::{linked_worktree, main_checkout};
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let (main, wt) = (base.join("main"), base.join("wt"));
        main_checkout(&main, "master");
        linked_worktree(&main, &wt, "wt", "feat");
        let _rt = crate::engine::test_rt_enter();
        let engine = Engine::new(Config::for_tests_detected(&wt)).expect("engine");

        assert_eq!(
            engine.local_caches().expect("stores").len(),
            1,
            "no checkout store until something needs one"
        );
        let checkout = engine
            .local_cache_for(CacheScope::Checkout)
            .expect("checkout");
        let shared = engine.local_cache_for(CacheScope::Shared).expect("shared");
        assert!(!Arc::ptr_eq(&shared, &checkout));
        assert!(wt.join(".heph").join("cache").join("cache.db").exists());

        let a = hmodel::htaddr::parse_addr("//p:a").expect("addr");
        let mut w = checkout.writer(&a, "h", MANIFEST_V1).expect("writer");
        std::io::Write::write_all(&mut w, b"x").expect("write");
        w.commit().expect("commit");
        assert!(checkout.exists(&a, "h", MANIFEST_V1).expect("exists"));
        assert!(
            !shared.exists(&a, "h", MANIFEST_V1).expect("exists"),
            "the shared store does not see the checkout's entry"
        );
        let scopes: Vec<_> = engine
            .local_caches()
            .expect("stores")
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(scopes, [CacheScope::Shared, CacheScope::Checkout]);
    }

    /// Once a checkout store exists on disk, the next engine opens it at
    /// construction, so no request opens sqlite lazily on a runtime worker.
    #[test]
    fn an_existing_checkout_store_is_opened_eagerly() {
        use crate::engine::local_cache::CacheScope;
        use hconfig::git_checkout::test_layout::{linked_worktree, main_checkout};
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonicalize");
        let (main, wt) = (base.join("main"), base.join("wt"));
        main_checkout(&main, "master");
        linked_worktree(&main, &wt, "wt", "feat");
        let _rt = crate::engine::test_rt_enter();

        let first = Engine::new(Config::for_tests_detected(&wt)).expect("engine");
        let store = first.checkout_cache.as_ref().expect("a worktree");
        assert!(!store.is_open(), "no database yet: created on first use");
        first
            .local_cache_for(CacheScope::Checkout)
            .expect("create it");
        drop(first);

        let second = Engine::new(Config::for_tests_detected(&wt)).expect("engine");
        assert!(
            second
                .checkout_cache
                .as_ref()
                .expect("a worktree")
                .is_open(),
            "the database exists, so it is open after Engine::new"
        );
    }

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
}
