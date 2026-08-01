use crate::pluginbuildfile::run_file::RunResult;
use anyhow::Context;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hcore::hmemoizer::Memoizer;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::GetError::NotFound;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    Provider as EProvider, ProviderFunctionRegistry, State, TargetSpec,
};
use hwalk::{CachedWalker, Ignore};
use once_cell::sync::OnceCell;
use starlark::environment::Globals;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The BUILD-file name patterns used when a workspace's buildfile-provider
/// config does not list its own `patterns`. Literal `BUILD` plus the `*.BUILD`
/// glob (e.g. `foo.BUILD`).
pub fn default_build_file_patterns() -> Vec<glob::Pattern> {
    ["BUILD", "*.BUILD"]
        .into_iter()
        .map(|p| glob::Pattern::new(p).expect("valid default build pattern"))
        .collect()
}

/// Compile the `patterns` option from a buildfile-provider config into globs,
/// falling back to [`default_build_file_patterns`] when absent, empty, or
/// uncompilable. Shared by the engine provider, the LSP, and the formatter so
/// all agree on which files are BUILD files.
pub fn build_file_patterns_from_options(
    opts: &hplugin::config::Options,
) -> anyhow::Result<Vec<glob::Pattern>> {
    let names: Option<Vec<String>> =
        hplugin::config::decode_opt(opts, "buildfile provider", "patterns")?;
    let Some(names) = names.filter(|n| !n.is_empty()) else {
        return Ok(default_build_file_patterns());
    };
    names
        .into_iter()
        .map(|p| glob::Pattern::new(&p).with_context(|| format!("invalid buildfile pattern `{p}`")))
        .collect()
}

/// The buildfile-provider settings the formatter needs, resolved from a
/// workspace `.hephconfig2`. Hands callers (the `build-fmt` command, the LSP)
/// the patterns + indent with all config loading, decoding, and defaults
/// handled here — they only supply a root.
pub struct FormatSettings {
    pub patterns: Vec<glob::Pattern>,
    pub indent: usize,
}

impl FormatSettings {
    /// Resolve from an optional workspace root. `None` — or a root with no
    /// buildfile config — yields the defaults.
    pub fn resolve(root: Option<&Path>) -> Self {
        let opts = root.map(buildfile_options).unwrap_or_default();
        FormatSettings {
            patterns: build_file_patterns_from_options(&opts)
                .unwrap_or_else(|_| default_build_file_patterns()),
            indent: build_file_indent_from_options(&opts).unwrap_or(DEFAULT_INDENT),
        }
    }
}

/// The buildfile builtin-provider's `options:` map from the workspace config at
/// `root`, or empty defaults when there is no config / no buildfile entry.
/// Mirrors how the engine resolves a built-in provider's options.
fn buildfile_options(root: &Path) -> hplugin::config::Options {
    hconfig::load_from_root(root)
        .ok()
        .and_then(|cfg| {
            cfg.plugins
                .into_iter()
                .find(|p| {
                    matches!(&p.identifier, hconfig::PluginIdentifier::Builtin(b) if b == "buildfile")
                })
                .map(|p| p.options)
        })
        .unwrap_or_default()
}

/// The indentation width (spaces per level) the formatter should use, from the
/// buildfile-provider config's `indent` option. Defaults to `DEFAULT_INDENT`.
pub fn build_file_indent_from_options(opts: &hplugin::config::Options) -> anyhow::Result<usize> {
    Ok(
        hplugin::config::decode_opt(opts, "buildfile provider", "indent")?
            .unwrap_or(DEFAULT_INDENT),
    )
}

/// Default indentation width when the config does not set `indent`.
pub const DEFAULT_INDENT: usize = 4;

/// Every file directly inside `dir` whose name matches one of `patterns`
/// (handles literal names like `BUILD` and globs like `*.BUILD`), sorted for
/// deterministic order. A package may have more than one BUILD file.
///
/// **Diverges from the build.** This reads `std::fs` directly and pattern-matches
/// a *lossy* name, where the engine's `find_build_files` goes through
/// [`CachedWalker`], which rejects a non-UTF-8 name outright. So a
/// `caf\xe9.BUILD` matches `*.BUILD` here (as `caf\u{FFFD}.BUILD`) and the
/// formatter and LSP would open a file the build hard-refuses to see. Its
/// callers are editor-side only, so this is a diagnosability trap rather than a
/// hash route — but it should be routed through the walker.
pub fn build_files_in_dir(dir: &Path, patterns: &[glob::Pattern]) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
            is_file
                && patterns
                    .iter()
                    .any(|p| p.matches(&e.file_name().to_string_lossy()))
        })
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths
}

pub struct RequestState {}

pub struct Provider {
    pub root: std::path::PathBuf,
    pub build_file_patterns: Vec<glob::Pattern>,
    /// Directories pruned during the BUILD-file walk: engine skip dirs/globs plus
    /// this provider's own `skip` option. See [`hwalk::Ignore`].
    pub skip: Arc<Ignore>,
    /// Driver applied to targets that omit `driver` in their `target(...)` call.
    /// Set via the `defaultDriver` provider option. `None` means a target with no
    /// driver is an error.
    pub default_driver: Option<String>,
    pub requests: Mutex<HashMap<String, RequestState>>,
    /// Cache: pkg name → parsed BUILD file result. Avoids re-parsing the Starlark
    /// AST on every `list`/`get`/`probe` call for the same package (3+ calls per
    /// pkg in a typical run), and dedupes concurrent in-flight parses on the same
    /// pkg. Caches errors too — a failed parse stays failed for the lifetime of
    /// the provider (BUILD file contents don't change mid-session).
    pub(crate) pkg_cache: Memoizer<String, Result<Arc<RunResult>, Arc<anyhow::Error>>>,
    /// The sorted workspace package list, walked once. Shared with every
    /// [`BuildFileLoader`] so `heph.core.packages()` reads the same list from any
    /// package's evaluation instead of re-walking the tree per call, and reused
    /// by [`Self::list_packages`] so there is exactly one walk and one order for
    /// the whole session.
    ///
    /// Built lazily by [`Self::packages`] rather than stored directly: the
    /// `Provider` is assembled with struct-update syntax (`..Self::default()`)
    /// and further mutated by [`Self::with_walker`], so a field initialized at
    /// construction time would capture the *defaults* of `root`/`patterns`/
    /// `skip`/`walker` rather than the configured values.
    ///
    /// [`BuildFileLoader`]: crate::pluginbuildfile::run_file::BuildFileLoader
    pub(crate) packages: OnceLock<Arc<PackageList>>,
    /// Sync cache: resolved BUILD-file path → parsed result. Populated during Starlark
    /// evaluation (both top-level `run_pkg` and transitive `load(...)` resolution share
    /// the same cache, so a file is parsed at most once per provider lifetime).
    pub(crate) file_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    /// Sync cache: package directory → merged result across every matching BUILD file
    /// in that dir. Loaded once and reused by both `run_pkg` and `load("//pkg", ...)`.
    pub(crate) dir_cache: Arc<Mutex<HashMap<PathBuf, Arc<RunResult>>>>,
    /// Single-flights file evaluation so concurrently-evaluating packages that
    /// `load()` the same shared build-file helper evaluate it once between them.
    pub(crate) loads: Arc<crate::pluginbuildfile::run_file::LoadRegistry>,
    /// Aggregated provider functions, injected once by the engine. Drives the
    /// `heph.<provider>.<fn>` Starlark namespace. Empty until injected (some unit
    /// tests run the provider without an engine).
    pub(crate) function_registry: OnceLock<Arc<ProviderFunctionRegistry>>,
    /// Lazily-built Starlark globals (built from `function_registry` on first eval),
    /// shared with every `BuildFileLoader` so the namespace is built at most once.
    pub(crate) globals: Arc<OnceLock<Globals>>,
    /// Shared cross-run filesystem-walk cache. The package-discovery walk reads
    /// directories through it, so an unchanged tree skips `readdir` entirely (a
    /// BUILD file's *contents* don't change the package set — that's handled by
    /// `pkg_cache`). Disabled until [`with_walker`] is called.
    ///
    /// [`with_walker`]: Provider::with_walker
    pub(crate) walker: Arc<CachedWalker>,
}

impl Provider {
    /// Field defaults shared by the real constructors (which inject the
    /// memoizer's runtime) and the test-only `Default`.
    pub(crate) fn base(
        pkg_cache: Memoizer<String, Result<Arc<RunResult>, Arc<anyhow::Error>>>,
    ) -> Self {
        Self {
            root: std::path::PathBuf::from("/"),
            build_file_patterns: default_build_file_patterns(),
            skip: Arc::new(Ignore::default()),
            default_driver: None,
            requests: Mutex::new(HashMap::new()),
            pkg_cache,
            packages: OnceLock::new(),
            file_cache: Arc::new(Mutex::new(HashMap::new())),
            dir_cache: Arc::new(Mutex::new(HashMap::new())),
            loads: Arc::default(),
            function_registry: OnceLock::new(),
            globals: Arc::new(OnceLock::new()),
            walker: Arc::new(CachedWalker::disabled()),
        }
    }
}

/// Test-only: the memoizer needs a runtime handle, and struct-update tests
/// (`..Provider::default()`) have no natural place to inject one — a shared
/// static test runtime serves them. Production constructors take the handle
/// explicitly.
#[cfg(test)]
impl Default for Provider {
    fn default() -> Self {
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let handle = RT
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("test runtime")
            })
            .handle()
            .clone();
        Self::base(Memoizer::with_tag_task("buildfile_pkg", handle))
    }
}

impl Provider {
    pub fn new(root: std::path::PathBuf, runtime: tokio::runtime::Handle) -> Self {
        Self {
            root,
            ..Self::base(Memoizer::with_tag_task("buildfile_pkg", runtime))
        }
    }

    /// Use `walker` (the shared cross-run fs-walk cache) for package discovery, so
    /// an unchanged tree skips `readdir`. Without it the provider walks the tree
    /// live every run (the in-process package list only dedupes within a run).
    pub fn with_walker(mut self, walker: Arc<CachedWalker>) -> Self {
        self.walker = walker;
        // The package list binds to the walker it was built from, so a builder
        // call after one exists must discard it rather than leave a cell whose
        // contents no longer match the provider's configuration. Free: nothing
        // has been walked yet at any real call site.
        self.packages = OnceLock::new();
        self
    }

    /// The shared, sorted package list, bound on first use to this provider's
    /// configured `root`/`patterns`/`skip`/`walker`.
    pub(crate) fn packages(&self) -> Arc<PackageList> {
        Arc::clone(self.packages.get_or_init(|| {
            Arc::new(PackageList::new(
                self.root.clone(),
                self.build_file_patterns.clone(),
                Arc::clone(&self.skip),
                Arc::clone(&self.walker),
            ))
        }))
    }

    pub fn from_options(
        root: std::path::PathBuf,
        skip_dirs: &[std::path::PathBuf],
        skip_globs: &[String],
        opts: &hplugin::config::Options,
        runtime: tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        hplugin::config::deny_unknown(
            "buildfile provider",
            opts,
            &["patterns", "skip", "defaultDriver", "indent"],
        )?;
        let compiled = build_file_patterns_from_options(opts)?;
        // Engine-wide `fs.skip` globs are merged ahead of this provider's own
        // `skip` option so both prune the same workspace-relative paths.
        let mut globs = skip_globs.to_vec();
        let user_skip: Vec<String> =
            hplugin::config::decode_opt(opts, "buildfile provider", "skip")?.unwrap_or_default();
        globs.extend(user_skip);
        let skip = Ignore::new(skip_dirs, &globs)?;
        let default_driver: Option<String> =
            hplugin::config::decode_opt(opts, "buildfile provider", "defaultDriver")?;
        Ok(Self {
            root,
            build_file_patterns: compiled,
            skip: Arc::new(skip),
            default_driver,
            ..Self::base(Memoizer::with_tag_task("buildfile_pkg", runtime))
        })
    }
}

/// The workspace package list: one BUILD-file walk, **sorted**, computed at most
/// once and shared by every consumer.
///
/// # Why sorted
///
/// [`find_packages_sync`] accumulates into a `HashSet`, whose iteration order is
/// a function of a per-instance `RandomState` seed — it differs between two sets
/// in the same process, let alone between runs. That order was reaching the def
/// hash by two routes: `heph.core.packages()` hands its result straight to a
/// `target(...)` config value, and `Provider::list_packages` order carries
/// through `Engine::packages` → `Engine::query` → `pluginquery`'s `deps` →
/// `plugingroup`, which folds `deps` into its def hash in order. Sorting here is
/// therefore not a nicety, it is what makes the value safe to cache and share at
/// all — which is also why the raw `HashSet` never leaves this type. Ordering is
/// `String`'s byte-lexicographic compare and must stay that way: a
/// collation-aware compare would make `LC_COLLATE` an undeclared hash input.
///
/// # Why bound to its inputs
///
/// The cell is constructed from the `(root, patterns, skip, walker)` it will be
/// filled from, rather than exposing a `get_or_init(closure)`, so two callers
/// with different skip configuration can never race to fill one cell and have
/// the loser silently served the winner's list. The LSP in particular builds its
/// loaders with `Ignore::default()` (it prunes nothing) — a divergence that
/// predates this type; binding keeps it from becoming a *shared* wrong answer.
///
/// # Hash-input caveat
///
/// [`CachedWalker::read_dir`] revalidates a listing by directory **mtime only**,
/// so a tree mutated without bumping a directory's mtime yields a stale listing.
/// That was a performance property of the walker; because this list reaches the
/// def hash, here it is a correctness one — a stale list is a stale *definition*.
/// `HEPH_DEBUG_CACHED_WALKER=0` bypasses the cache and can likewise compute a
/// different def hash from the same tree.
///
/// # Unicode: names are taken byte-for-byte, deliberately
///
/// A package name is the directory name exactly as `readdir` returns it. heph
/// applies **no Unicode normalization**, and none is intended.
///
/// That is uniform on the supported targets: for one commit checked out by git
/// onto a local ext4/btrfs/APFS volume, `readdir` returns identical bytes on all
/// three, so the sorted list and its def hash are identical. APFS is
/// normalization-*preserving* (an NFD directory name reads back as NFD); it is
/// only *lookup* that is normalization- and case-insensitive, which is a
/// property of address resolution, not of this list. Legacy HFS+ *does*
/// normalize to NFD on store, and so do some network and container-VM mounts —
/// those produce a **different** def hash, which is a cache miss and never a
/// wrong artifact, because every file input is content-hashed independently.
///
/// Normalizing here would be worse, not better: on a byte-preserving filesystem
/// a tree can legitimately hold both the NFC and the NFD spelling as two
/// distinct package directories, and folding them to one name would silently
/// merge two packages — trading a cache miss on an exotic mount for a name
/// collision on a supported one. It would also re-key every target of every
/// workspace with a non-ASCII package name.
pub(crate) struct PackageList {
    root: PathBuf,
    patterns: Vec<glob::Pattern>,
    skip: Arc<Ignore>,
    walker: Arc<CachedWalker>,
    cell: OnceCell<Arc<Vec<String>>>,
}

impl PackageList {
    pub(crate) fn new(
        root: PathBuf,
        patterns: Vec<glob::Pattern>,
        skip: Arc<Ignore>,
        walker: Arc<CachedWalker>,
    ) -> Self {
        Self {
            root,
            patterns,
            skip,
            walker,
            cell: OnceCell::new(),
        }
    }

    /// The already-walked list, if there is one. Lets an async caller skip the
    /// blocking-pool round-trip in the common case without duplicating the cache.
    pub(crate) fn cached(&self) -> Option<Arc<Vec<String>>> {
        self.cell.get().cloned()
    }

    /// The sorted package list, walking the tree on the first call and serving
    /// the same `Arc` afterwards.
    ///
    /// The layout is treated as fixed for this cell's lifetime — the same
    /// assumption `pkg_cache` makes about BUILD file contents. That *removes* a
    /// non-determinism (without it, two packages evaluated in one run can observe
    /// different package sets — a codegen target writing into the tree between
    /// them — so a def hash could depend on evaluation order) at the cost of a
    /// visible semantic change: a package that appears mid-run stays invisible
    /// for the rest of it. The bound that makes this safe is that a provider
    /// lives for one CLI invocation; a long-lived engine (a daemon, a watch mode)
    /// would have to revisit it.
    ///
    /// `get_or_try_init` is a blocking single-flight: concurrent callers park,
    /// exactly one walk runs, and the steady state is a lock-free read. A failed
    /// walk is **not** cached — the cell stays empty and the next caller retries.
    /// That is deliberately unlike `pkg_cache`, which caches errors stickily; a
    /// `readdir` failure is a transient environment fault, not a fact about the
    /// workspace.
    pub(crate) fn get(&self) -> anyhow::Result<Arc<Vec<String>>> {
        self.cell
            .get_or_try_init(|| {
                let mut set = std::collections::HashSet::new();
                find_packages_sync(
                    &self.walker,
                    &self.root,
                    &self.root,
                    &self.patterns,
                    &self.skip,
                    &mut set,
                )
                .with_context(|| format!("walking {} for the package list", self.root.display()))?;
                let mut pkgs: Vec<String> = set.into_iter().collect();
                // Non-negotiable: see the type doc. Dropping this returns
                // `HashSet` iteration order into the def hash.
                pkgs.sort_unstable();
                anyhow::Ok(Arc::new(pkgs))
            })
            .cloned()
    }
}

/// Recursively discover packages under `path`, reading each directory through
/// the shared [`CachedWalker`] (so an unchanged tree skips `readdir`). Filtering
/// (build-file pattern, skip-dir pruning) is applied here.
///
/// Private on purpose — go through [`PackageList`], which owns the sort the def
/// hash depends on.
fn find_packages_sync(
    walker: &CachedWalker,
    path: &std::path::Path,
    root: &std::path::Path,
    patterns: &[glob::Pattern],
    skip: &Ignore,
    packages: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let listing = walker.read_dir(path)?;
    let mut has_build_file = false;
    for entry in &listing.entries {
        match entry.kind {
            // A symlinked BUILD file is not evidence of a package: `find_build_files`
            // (run_file.rs) deliberately excludes symlinks when it later reads this
            // package's build files, so counting one here would list a package that
            // resolves zero targets when actually loaded.
            hwalk::EntryKind::File => {
                if patterns.iter().any(|p| p.matches(&entry.name)) {
                    has_build_file = true;
                }
            }
            hwalk::EntryKind::Symlink => {}
            hwalk::EntryKind::Dir => {
                let entry_path = path.join(&entry.name);
                let rel = entry_path.strip_prefix(root).unwrap_or(&entry_path);
                if skip.prune_dir(&entry_path, rel) {
                    continue;
                }
                find_packages_sync(walker, &entry_path, root, patterns, skip, packages)?;
            }
            hwalk::EntryKind::Other => {}
        }
    }

    if has_build_file {
        let mut current = path;
        while let Ok(rel) = current.strip_prefix(root) {
            // `to_str`, never `to_string_lossy`: this string is a package name,
            // and the sorted package list is a def-hash input. A lossy render
            // would fold `x\xffy` and `x\xfey` into one `x\u{FFFD}y` package and
            // hash two different trees the same. `CachedWalker::read_dir` already
            // refuses a non-UTF-8 entry name, and every component below `root`
            // comes from it, so this holds today — the check keeps it from
            // becoming a silent assumption if that ever changes.
            let pkg_name = rel
                .to_str()
                .with_context(|| {
                    format!(
                        "package directory name is not valid UTF-8: '{}'",
                        current.display()
                    )
                })?
                .to_string();
            packages.insert(pkg_name);

            if let Some(parent) = current.parent() {
                current = parent;
            } else {
                break;
            }
        }
    }

    Ok(())
}

impl Provider {}

impl EProvider for Provider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "buildfile".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            // A package inside a skipped subtree lists nothing — matching what the
            // package walk would have surfaced.
            if self
                .skip
                .prunes_package(&self.root, std::path::Path::new(req.package.as_str()))
            {
                return Ok(Box::new(std::iter::empty())
                    as Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>);
            }
            let res = self.run_pkg(req.package.as_str()).await?;

            let items: Vec<anyhow::Result<ListResponse>> = res
                .targets
                .iter()
                .map(|p| {
                    Ok(ListResponse {
                        addr: Addr::new(req.package.clone(), p.name.clone(), Default::default()),
                    })
                })
                .collect();

            Ok(Box::new(items.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        _req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            // The same sorted cell `heph.core.packages()` reads: one walk, one
            // order, one error policy for the whole session. `PackageList` is its
            // own single-flight, so no memoizer is needed in front of it.
            let list = self.packages();
            let packages = match list.cached() {
                // Already walked: a lock-free read, not worth a pool round-trip.
                Some(packages) => packages,
                // The walk is a synchronous recursive `readdir` of the workspace
                // — never on a runtime worker. It reads dirs through the shared
                // walker, so an unchanged tree comes from the cross-run fswalk
                // cache.
                None => hcore::blocking::run(move || list.get()).await?,
            };

            let items: Vec<anyhow::Result<ListPackageResponse>> = packages
                .iter()
                .map(|p| {
                    Ok(ListPackageResponse {
                        pkg: PkgBuf::from(p.as_str()),
                    })
                })
                .collect();

            Ok(Box::new(items.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send,
                >)
        })
    }

    fn get<'a>(
        &'a self,
        req: GetRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, Result<GetResponse, GetError>> {
        Box::pin(async move {
            // A target inside a skipped subtree does not resolve.
            if self
                .skip
                .prunes_package(&self.root, std::path::Path::new(req.addr.package.as_str()))
            {
                return Err(NotFound);
            }
            let res = self
                .run_pkg(req.addr.package.as_str())
                .await
                .map_err(|e: anyhow::Error| GetError::Other(e))?;

            for p in res.targets.iter() {
                if p.name == req.addr.name {
                    let driver = if p.driver.is_empty() {
                        self.default_driver.clone().ok_or_else(|| {
                            GetError::Other(anyhow::anyhow!(
                                "target {} has no driver and no defaultDriver is configured for the buildfile provider",
                                req.addr.format()
                            ))
                        })?
                    } else {
                        p.driver.clone()
                    };
                    return Ok(GetResponse {
                        target_spec: TargetSpec {
                            addr: req.addr.clone(),
                            driver,
                            config: p.config.clone(),
                            labels: p.labels.clone(),
                            transitive: p.transitive.clone(),
                            approval: p.approval.clone(),
                        },
                    });
                }
            }

            Err(NotFound)
        })
    }

    fn probe<'a>(
        &'a self,
        req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        Box::pin(async move {
            let res = self.run_pkg(req.package.as_str()).await?;

            Ok(ProbeResponse {
                states: res
                    .states
                    .iter()
                    .map(|p| State {
                        package: req.package.clone(),
                        provider: p.provider.clone(),
                        state: p.args.clone(),
                    })
                    .collect(),
            })
        })
    }

    fn set_function_registry(&self, reg: Arc<ProviderFunctionRegistry>) {
        // First injection wins; the engine wires exactly once, so a later set
        // (already-injected) is a harmless no-op.
        if self.function_registry.set(reg).is_err() {
            // Registry was already injected; keep the first one.
        }
    }
}

#[cfg(test)]
mod tests {
    /// Handle for sync tests constructing the provider: one shared runtime,
    /// built on first use.
    fn test_runtime() -> tokio::runtime::Handle {
        static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("test runtime")
        })
        .handle()
        .clone()
    }

    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hmodel::htaddr::parse_addr;
    use hplugin::config::Options;
    use hplugin::provider::GetRequest;
    use std::fs;
    use tempfile::tempdir;

    /// `Provider::list_packages` and the `heph.core.packages()` builtin are one
    /// list in one order.
    ///
    /// Both orders reach a def hash: the builtin's through the calling target's
    /// config, and `list_packages`' through `Engine::packages` → `Engine::query`
    /// → `pluginquery`'s `deps` → `plugingroup`, which folds `deps` into its def
    /// hash in order. Before they shared [`PackageList`], `list_packages` handed
    /// back raw `HashSet` iteration order, so a cold-cache run computed a
    /// different group def hash — and a different `LIST_*` file line order inside
    /// the sandbox — every time, from a byte-identical tree.
    ///
    /// Deliberately does **not** sort the result before comparing: coming out
    /// sorted is the property under test.
    #[tokio::test]
    async fn test_list_packages_is_sorted_and_agrees_with_the_starlark_builtin() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        // Names whose creation order is not their sorted order, and enough of
        // them that a `HashSet` matching sorted order by chance is not a concern.
        let names = ["zeta", "alpha", "mid/x", "beta", "mid/a", "gamma", "mid"];
        for p in names {
            let d = root.join(p);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("BUILD"), "").unwrap();
        }
        fs::write(
            root.join("BUILD"),
            r#"target(name = "t", driver = "d", pkgs = heph.core.packages("//..."))"#,
        )
        .unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let listed: Vec<String> = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap()
            .map(|r| r.unwrap().pkg.to_string())
            .collect();

        let mut expected: Vec<String> = names.iter().map(|p| p.to_string()).collect();
        expected.push(String::new()); // the root package, from the ancestor walk
        expected.sort();
        assert_eq!(listed, expected);

        let result = provider.run_pkg("").await.expect("eval root package");
        let from_builtin: Vec<String> = match result.targets[0].config.get("pkgs").unwrap() {
            hcore::htvalue::Value::List(v) => v
                .iter()
                .map(|e| match e {
                    hcore::htvalue::Value::String(s) => s.clone(),
                    other => panic!("expected string pkg, got {other:?}"),
                })
                .collect(),
            other => panic!("expected pkgs list, got {other:?}"),
        };
        assert_eq!(from_builtin, listed);
    }

    /// Package discovery is cached across runs through the shared walker: a fresh
    /// provider sharing the fswalk db reuses the discovered set for an unchanged
    /// tree, and a newly-added package (which bumps a recorded dir's mtime) is
    /// re-discovered.
    #[tokio::test]
    async fn test_list_packages_cross_run_cache() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        // fswalk db outside the walked tree (in production it's under pruned
        // `.heph3`), so its writes don't bump the discovered dirs' mtimes.
        let dbdir = tempdir().unwrap();
        let db = dbdir.path().join("fswalk.db");
        fs::write(root.join("BUILD"), "").unwrap();
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("BUILD"), "").unwrap();

        let list = |p: Provider| async move {
            let ctoken = StdCancellationToken::new();
            let res = p
                .list_packages(
                    ListPackagesRequest {
                        prefix: PkgBuf::from(""),
                    },
                    &ctoken,
                )
                .await
                .unwrap();
            // Drop the always-present synthetic LSP package; this test covers the
            // filesystem walk.
            let mut v: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();
            v.sort();
            v
        };
        let provider = || {
            Provider {
                root: root.to_path_buf(),
                ..Provider::default()
            }
            .with_walker(Arc::new(CachedWalker::open(&db)))
        };

        assert_eq!(
            list(provider()).await,
            vec!["".to_string(), "a".to_string()]
        );

        // Fresh provider sharing the walker db (new run) → same set, served from
        // the cross-run readdir cache for the unchanged tree.
        assert_eq!(
            list(provider()).await,
            vec!["".to_string(), "a".to_string()]
        );

        // Add a new package; bump root mtime so the recorded dir invalidates.
        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("BUILD"), "").unwrap();
        std::fs::File::open(root)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(7200))
            .unwrap();

        assert_eq!(
            list(provider()).await,
            vec!["".to_string(), "a".to_string(), "b".to_string()],
            "a newly-added package is re-discovered"
        );
    }

    /// A symlinked BUILD file is not a build file to `find_build_files` (run_file.rs
    /// deliberately mirrors the prior `file_type().is_file()`), so `list_packages`
    /// must not count it as package evidence either — otherwise a symlinked-BUILD
    /// package appears in `heph query` but resolves zero targets when loaded.
    #[tokio::test]
    async fn test_list_packages_excludes_symlink_only_build_file() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("real.BUILD"), "").unwrap();

        let pkg_dir = root.join("linked");
        fs::create_dir_all(&pkg_dir).unwrap();
        std::os::unix::fs::symlink(root.join("real.BUILD"), pkg_dir.join("BUILD")).unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            ..Provider::default()
        };
        let ctoken = StdCancellationToken::new();
        let listed: Vec<String> = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap()
            .map(|r| r.unwrap().pkg.to_string())
            .collect();
        assert!(
            !listed.contains(&"linked".to_string()),
            "symlinked-BUILD package must not be listed: {listed:?}"
        );

        // Consistent with the listing: actually loading the package (as
        // `find_build_files` would for evaluation) finds no build files either.
        let result = provider.run_pkg("linked").await.expect("eval package");
        assert!(result.targets.is_empty(), "{:?}", result.targets);
    }

    #[test]
    fn from_options_defaults_to_build() {
        let dir = tempdir().expect("tempdir");
        let p = Provider::from_options(
            dir.path().to_path_buf(),
            &[],
            &[],
            &Options::new(),
            test_runtime(),
        )
        .expect("from_options");
        let names: Vec<&str> = p.build_file_patterns.iter().map(|p| p.as_str()).collect();
        assert_eq!(names, vec!["BUILD", "*.BUILD"]);
    }

    #[test]
    fn from_options_reads_patterns() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert(
            "patterns".to_string(),
            serde_yaml::from_str("[BUILD2, \"*.BUILD2\"]").expect("yaml"),
        );
        let p = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts, test_runtime())
            .expect("from_options");
        let names: Vec<&str> = p.build_file_patterns.iter().map(|p| p.as_str()).collect();
        assert_eq!(names, vec!["BUILD2", "*.BUILD2"]);
    }

    #[test]
    fn from_options_rejects_invalid_glob() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert(
            "patterns".to_string(),
            serde_yaml::from_str("[\"[bad\"]").expect("yaml"),
        );
        let err = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts, test_runtime())
            .err()
            .expect("must error");
        assert!(err.to_string().contains("[bad"), "{err}");
    }

    #[test]
    fn from_options_rejects_unknown_key() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert("bogus".to_string(), serde_yaml::Value::Bool(true));
        let err = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts, test_runtime())
            .err()
            .expect("must error");
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn default_patterns_include_dot_build() {
        let names: Vec<String> = default_build_file_patterns()
            .iter()
            .map(|p| p.as_str().to_string())
            .collect();
        assert_eq!(names, vec!["BUILD", "*.BUILD"]);
    }

    #[test]
    fn build_files_in_dir_matches_literal_and_glob_sorted() {
        let dir = tempdir().expect("tempdir");
        for name in ["BUILD", "lib.BUILD", "app.BUILD", "notes.txt", "BUILD.bak"] {
            std::fs::write(dir.path().join(name), "").expect("write");
        }
        let patterns = default_build_file_patterns();
        let found: Vec<String> = build_files_in_dir(dir.path(), &patterns)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // `*.BUILD` matches `lib.BUILD`/`app.BUILD` but not `BUILD.bak` or `.txt`;
        // results are sorted.
        assert_eq!(found, vec!["BUILD", "app.BUILD", "lib.BUILD"]);
    }

    #[test]
    fn format_settings_resolve_reads_config() {
        let dir = tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(".hephconfig2"),
            "plugins:\n  - builtin: buildfile\n    options:\n      patterns: [BUILD, \"*.star\"]\n      indent: 4\n",
        )
        .expect("write config");
        let settings = FormatSettings::resolve(Some(dir.path()));
        assert_eq!(settings.indent, 4);
        let names: Vec<&str> = settings.patterns.iter().map(|p| p.as_str()).collect();
        assert_eq!(names, vec!["BUILD", "*.star"]);
    }

    #[test]
    fn format_settings_resolve_none_uses_defaults() {
        let settings = FormatSettings::resolve(None);
        assert_eq!(settings.indent, DEFAULT_INDENT);
        let names: Vec<&str> = settings.patterns.iter().map(|p| p.as_str()).collect();
        assert_eq!(names, vec!["BUILD", "*.BUILD"]);
    }

    #[test]
    fn indent_option_defaults_then_reads() {
        let empty = Options::new();
        assert_eq!(
            build_file_indent_from_options(&empty).expect("indent"),
            DEFAULT_INDENT
        );

        let mut opts = Options::new();
        opts.insert(
            "indent".to_string(),
            serde_yaml::from_str("4").expect("yaml"),
        );
        assert_eq!(build_file_indent_from_options(&opts).expect("indent"), 4);
    }

    #[test]
    fn from_options_rejects_wrong_type() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert(
            "patterns".to_string(),
            serde_yaml::Value::String("not a list".to_string()),
        );
        let err = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts, test_runtime())
            .err()
            .expect("must error");
        assert!(err.to_string().contains("patterns"), "{err}");
    }

    struct NoopExecutor;
    impl hplugin::provider::ProviderExecutor for NoopExecutor {
        fn result<'a>(
            &'a self,
            _addr: &'a Addr,
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Arc<hplugin::eresult::EResult>>>
        {
            Box::pin(async { anyhow::bail!("noop") })
        }

        fn query<'a>(
            &'a self,
            _m: &'a hmodel::htmatcher::Matcher,
            _extra_skip: &'a [String],
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Vec<Addr>>> {
            Box::pin(async { anyhow::bail!("noop") })
        }
    }

    fn get_req(pkg: &str, name: &str) -> GetRequest {
        GetRequest {
            request_id: "test".to_string(),
            addr: Addr::new(PkgBuf::from(pkg), name.to_string(), Default::default()),
            states: vec![],
            executor: Arc::new(NoopExecutor),
        }
    }

    #[test]
    fn from_options_reads_default_driver() {
        let dir = tempdir().expect("tempdir");
        let mut opts = Options::new();
        opts.insert(
            "defaultDriver".to_string(),
            serde_yaml::Value::String("exec".to_string()),
        );
        let p = Provider::from_options(dir.path().to_path_buf(), &[], &[], &opts, test_runtime())
            .expect("from_options");
        assert_eq!(p.default_driver.as_deref(), Some("exec"));
    }

    #[test]
    fn from_options_default_driver_absent_is_none() {
        let dir = tempdir().expect("tempdir");
        let p = Provider::from_options(
            dir.path().to_path_buf(),
            &[],
            &[],
            &Options::new(),
            test_runtime(),
        )
        .expect("from_options");
        assert!(p.default_driver.is_none());
    }

    #[tokio::test]
    async fn get_applies_default_driver_when_omitted() {
        let tmp_dir = tempdir().unwrap();
        let pkg_path = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), r#"target(name = "t")"#).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            default_driver: Some("exec".to_string()),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let res = provider.get(get_req("p", "t"), &ctoken).await.expect("get");
        assert_eq!(res.target_spec.driver, "exec");
    }

    #[tokio::test]
    async fn get_explicit_driver_overrides_default() {
        let tmp_dir = tempdir().unwrap();
        let pkg_path = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(
            pkg_path.join("BUILD"),
            r#"target(name = "t", driver = "bash")"#,
        )
        .unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            default_driver: Some("exec".to_string()),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let res = provider.get(get_req("p", "t"), &ctoken).await.expect("get");
        assert_eq!(res.target_spec.driver, "bash");
    }

    #[tokio::test]
    async fn get_errors_when_no_driver_and_no_default() {
        let tmp_dir = tempdir().unwrap();
        let pkg_path = tmp_dir.path().join("p");
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), r#"target(name = "t")"#).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let err = provider
            .get(get_req("p", "t"), &ctoken)
            .await
            .err()
            .expect("must error");
        let msg = format!("{err:?}");
        assert!(msg.contains("no driver"), "{msg}");
    }

    #[tokio::test]
    async fn list_packages_skips_core_dirs_and_globs() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // root/BUILD, root/.heph3/BUILD (core skip), root/vendor/BUILD (glob skip),
        // root/src/BUILD (kept).
        fs::write(root.join("BUILD"), "").unwrap();
        let heph = root.join(".heph3");
        fs::create_dir_all(&heph).unwrap();
        fs::write(heph.join("BUILD"), "").unwrap();
        let vendor = root.join("vendor");
        fs::create_dir_all(&vendor).unwrap();
        fs::write(vendor.join("BUILD"), "").unwrap();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("BUILD"), "").unwrap();

        let mut opts = Options::new();
        opts.insert(
            "skip".to_string(),
            serde_yaml::from_str("[vendor]").expect("yaml"),
        );
        let provider = Provider::from_options(
            root.to_path_buf(),
            &[heph.clone()],
            &[],
            &opts,
            test_runtime(),
        )
        .expect("provider");

        let ctoken = StdCancellationToken::new();
        let res = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap();
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"src".to_string()));
        assert!(
            !packages.contains(&".heph3".to_string()),
            "core dir not pruned"
        );
        assert!(!packages.contains(&"vendor".to_string()), "glob not pruned");
    }

    #[tokio::test]
    async fn list_packages_skips_engine_skip_dirs() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        fs::write(root.join("BUILD"), "").unwrap();
        let vendor = root.join("vendor");
        fs::create_dir_all(&vendor).unwrap();
        fs::write(vendor.join("BUILD"), "").unwrap();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("BUILD"), "").unwrap();

        // `vendor` comes in as an engine skip dir (the resolved `fs.skip`), not
        // the provider's own `skip` option — proving the engine threads it in.
        let provider = Provider::from_options(
            root.to_path_buf(),
            &[vendor.clone()],
            &[],
            &Options::new(),
            test_runtime(),
        )
        .expect("provider");

        let ctoken = StdCancellationToken::new();
        let res = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap();
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

        assert!(packages.contains(&"src".to_string()));
        assert!(
            !packages.contains(&"vendor".to_string()),
            "engine skip dir not pruned: {packages:?}"
        );
    }

    #[tokio::test]
    async fn get_and_list_skip_pruned_packages() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        let vendor_dep = root.join("vendor/dep");
        fs::create_dir_all(&vendor_dep).unwrap();
        fs::write(
            vendor_dep.join("BUILD"),
            r#"target(name = "t", driver = "d")"#,
        )
        .unwrap();

        // `vendor` is an engine skip dir; a target directly addressed under it
        // must not resolve, and listing it yields nothing.
        let provider = Provider::from_options(
            root.to_path_buf(),
            &[root.join("vendor")],
            &[],
            &Options::new(),
            test_runtime(),
        )
        .expect("provider");

        let ctoken = StdCancellationToken::new();
        let got = provider.get(get_req("vendor/dep", "t"), &ctoken).await;
        assert!(
            matches!(got, Err(NotFound)),
            "expected NotFound for skipped pkg"
        );

        let listed = provider
            .list(
                ListRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from("vendor/dep"),
                    states: vec![],
                    executor: std::sync::Arc::new(hplugin::provider::NoopExecutor),
                },
                &ctoken,
            )
            .await
            .unwrap();
        assert_eq!(listed.count(), 0);
    }

    #[tokio::test]
    async fn test_list_packages() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   BUILD (root package "")
        //   a/
        //     b/
        //       BUILD
        //   c/
        //     BUILD

        fs::write(root.join("BUILD"), "").unwrap();
        let ab = root.join("a").join("b");
        fs::create_dir_all(&ab).unwrap();
        fs::write(ab.join("BUILD"), "").unwrap();

        let c = root.join("c");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("BUILD"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            ..Provider::default()
        };

        let req = ListPackagesRequest {
            prefix: PkgBuf::from(""),
        };
        let ctoken = StdCancellationToken::new();
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

        assert_eq!(packages.len(), 4);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a/b".to_string()));
        assert!(packages.contains(&"a".to_string())); // parent of a/b
        assert!(packages.contains(&"c".to_string()));
    }

    /// Create a directory whose name is the raw byte sequence `raw`, or `None`
    /// when the filesystem refuses it. APFS rejects names that are not valid
    /// UTF-8 with `EILSEQ`, so this fixture cannot be built on
    /// `aarch64-apple-darwin`; callers skip loudly rather than assert on a
    /// package directory that was never created.
    fn try_create_non_utf8_package(root: &Path, raw: &[u8]) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let dir = root.join(std::ffi::OsStr::from_bytes(raw));
        match fs::create_dir(&dir) {
            Ok(()) => {
                fs::write(dir.join("BUILD"), "").unwrap();
                Some(dir)
            }
            Err(e) => {
                // Expected only on macOS. On Linux — the one target that
                // exercises the fix — a refusal means `$TMPDIR` cannot host the
                // fixture, and skipping would make the only real coverage a
                // silent green.
                assert!(
                    cfg!(target_os = "macos"),
                    "the non-UTF-8 package fixture must be constructible on this target, \
                     but {root:?} refused it: {e}"
                );
                eprintln!(
                    "SKIP: this filesystem refuses non-UTF-8 directory names ({e}); \
                     the non-UTF-8 package fixture cannot be built here"
                );
                None
            }
        }
    }

    /// A package directory heph cannot name must fail the walk. Dropping it
    /// silently removed the package — and every ancestor it would have inserted
    /// — from the sorted list that feeds the def hash, so adding or editing a
    /// BUILD file under it changed nothing anywhere.
    #[tokio::test]
    async fn list_packages_rejects_a_non_utf8_package_dir() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        fs::write(root.join("BUILD"), "").unwrap();
        let Some(_bad) = try_create_non_utf8_package(root, b"caf\xe9") else {
            return;
        };

        let provider = Provider {
            root: root.to_path_buf(),
            ..Provider::default()
        };
        let ctoken = StdCancellationToken::new();
        let err = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .err()
            .expect("a package dir heph cannot name must fail, not disappear from the list");
        assert!(
            format!("{err:#}").contains("not valid UTF-8"),
            "error should say why, got: {err:#}"
        );
    }

    /// Two package directories whose names `to_string_lossy` would both render
    /// as `caf\u{FFFD}` — one package name for two distinct trees, hence one
    /// def hash for two distinct definitions. Neither fusing them nor dropping
    /// them is acceptable.
    #[tokio::test]
    async fn list_packages_rejects_package_dirs_that_would_fuse_under_lossy_rendering() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        fs::write(root.join("BUILD"), "").unwrap();
        let Some(_a) = try_create_non_utf8_package(root, b"caf\xe9") else {
            return;
        };
        let Some(_b) = try_create_non_utf8_package(root, b"caf\xe8") else {
            return;
        };

        let provider = Provider {
            root: root.to_path_buf(),
            ..Provider::default()
        };
        let ctoken = StdCancellationToken::new();
        let err = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .err()
            .expect("two directories that render to one lossy name must not become one package");
        // The report has to distinguish them, or it names a directory the user
        // cannot tell from its neighbour — the very fusion being rejected.
        let msg = format!("{err:#}");
        assert!(
            msg.contains(r"\xE9") || msg.contains(r"\xE8"),
            "error must name the offending bytes escaped, got: {msg}"
        );
    }

    /// Package names are the directory bytes verbatim — heph applies no Unicode
    /// normalization, on any supported target. Pins the exemption documented on
    /// [`PackageList`]: a normalization pass here would fold the NFC and NFD
    /// spellings of a name into one package, and would re-key every target in a
    /// workspace with non-ASCII package names. Both filesystems in the supported
    /// set store the bytes they were given (APFS is normalization-preserving; it
    /// is only *lookup* that is insensitive), so this holds identically on
    /// Linux and macOS.
    #[tokio::test]
    async fn package_names_are_the_directory_bytes_not_a_normalized_form() {
        // "café" decomposed: `e` + U+0301 COMBINING ACUTE ACCENT.
        let nfd = "cafe\u{301}";
        let nfc = "caf\u{e9}";
        assert_ne!(nfd, nfc);

        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();
        fs::write(root.join("BUILD"), "").unwrap();
        let pkg = root.join(nfd);
        fs::create_dir(&pkg).unwrap();
        fs::write(pkg.join("BUILD"), "").unwrap();

        // `$TMPDIR` may sit on one of the mounts the exemption names as *not*
        // byte-preserving (HFS+, SMB, a container-VM share). Assert what heph
        // does with the bytes the filesystem kept, not what the filesystem did.
        let stored = fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n != "BUILD");
        if stored.as_deref() != Some(nfd) {
            eprintln!(
                "SKIP: this filesystem did not preserve the decomposed name \
                 (stored {stored:?}); nothing to pin here"
            );
            return;
        }

        let provider = Provider {
            root: root.to_path_buf(),
            ..Provider::default()
        };
        let ctoken = StdCancellationToken::new();
        let res = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ctoken,
            )
            .await
            .unwrap();
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

        assert!(
            packages.iter().any(|p| p == nfd),
            "expected the decomposed name verbatim, got {packages:?}"
        );
        assert!(
            !packages.iter().any(|p| p == nfc),
            "package names must not be normalized, got {packages:?}"
        );
    }

    #[tokio::test]
    async fn test_list_packages_custom_pattern() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   BUILD.heph
        //   a/
        //     BUILD.heph

        fs::write(root.join("BUILD.heph"), "").unwrap();
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("BUILD.heph"), "").unwrap();

        // Should NOT be found
        fs::write(root.join("BUILD"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("BUILD.heph").unwrap()],
            ..Provider::default()
        };

        let req = ListPackagesRequest {
            prefix: PkgBuf::from(""),
        };
        let ctoken = StdCancellationToken::new();
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

        assert_eq!(packages.len(), 2);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a".to_string()));
    }

    #[tokio::test]
    async fn test_list_packages_multiple_patterns() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   BUILD
        //   a/
        //     BUILD.heph
        //   b/
        //     BUILD.other

        fs::write(root.join("BUILD"), "").unwrap();
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("BUILD.heph"), "").unwrap();
        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("BUILD.other"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![
                glob::Pattern::new("BUILD").unwrap(),
                glob::Pattern::new("BUILD.heph").unwrap(),
                glob::Pattern::new("BUILD.other").unwrap(),
            ],
            ..Provider::default()
        };

        let req = ListPackagesRequest {
            prefix: PkgBuf::from(""),
        };
        let ctoken = StdCancellationToken::new();
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

        assert_eq!(packages.len(), 3);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a".to_string()));
        assert!(packages.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn test_list_packages_glob_pattern() {
        let tmp_dir = tempdir().unwrap();
        let root = tmp_dir.path();

        // Structure:
        // root/
        //   foo.BUILD
        //   a/
        //     bar.BUILD
        //   b/
        //     notabuild.txt   (must NOT match)

        fs::write(root.join("foo.BUILD"), "").unwrap();
        let a = root.join("a");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("bar.BUILD"), "").unwrap();
        let b = root.join("b");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("notabuild.txt"), "").unwrap();

        let provider = Provider {
            root: root.to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("*.BUILD").unwrap()],
            ..Provider::default()
        };

        let req = ListPackagesRequest {
            prefix: PkgBuf::from(""),
        };
        let ctoken = StdCancellationToken::new();
        let res = provider.list_packages(req, &ctoken).await.unwrap();
        let packages: Vec<String> = res.map(|r| r.unwrap().pkg.to_string()).collect();

        assert_eq!(packages.len(), 2);
        assert!(packages.contains(&"".to_string()));
        assert!(packages.contains(&"a".to_string()));
        assert!(!packages.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn test_run_pkg_glob_pattern() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "mypkg".to_string();
        let pkg_path = tmp_dir.path().join(&pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
target(
    name = "globtarget",
    driver = "mydriver",
)
"#;
        fs::write(pkg_path.join("my.BUILD"), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            build_file_patterns: vec![glob::Pattern::new("*.BUILD").unwrap()],
            ..Provider::default()
        };

        let result = provider.run_pkg(&pkg_name).await.unwrap();
        assert_eq!(result.targets.len(), 1);
        assert_eq!(result.targets[0].name, "globtarget");
    }

    #[tokio::test]
    async fn probe_returns_provider_states_from_build_file() {
        use hcore::htvalue::Value;

        let tmp_dir = tempdir().unwrap();
        let pkg_name = "p";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();

        let build_content = r#"
provider_state(provider = "go", root = "src", strict = True)
"#;
        fs::write(pkg_path.join("BUILD"), build_content).unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };

        let ctoken = StdCancellationToken::new();
        let res = provider
            .probe(
                ProbeRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from(pkg_name),
                },
                &ctoken,
            )
            .await
            .unwrap();

        assert_eq!(res.states.len(), 1);
        let s = &res.states[0];
        assert_eq!(s.package, PkgBuf::from(pkg_name));
        assert_eq!(s.provider, "go");
        assert_eq!(s.state.get("root"), Some(&Value::String("src".to_string())));
        assert_eq!(s.state.get("strict"), Some(&Value::Bool(true)));
        assert!(!s.state.contains_key("provider"));
    }

    #[tokio::test]
    async fn probe_missing_provider_kwarg_errors() {
        let tmp_dir = tempdir().unwrap();
        let pkg_name = "p";
        let pkg_path = tmp_dir.path().join(pkg_name);
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("BUILD"), "provider_state(root=\"x\")").unwrap();

        let provider = Provider {
            root: tmp_dir.path().to_path_buf(),
            ..Provider::default()
        };
        let ctoken = StdCancellationToken::new();
        let err = match provider
            .probe(
                ProbeRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from(pkg_name),
                },
                &ctoken,
            )
            .await
        {
            Ok(_) => panic!("missing provider must error"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("missing provider"), "{msg}");
    }
}
