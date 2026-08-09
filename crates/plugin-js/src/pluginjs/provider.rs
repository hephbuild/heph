use crate::pluginjs::lockfile::{self, Lockfile, ResolvedGraph};
use crate::pluginjs::workspace::{self, PkgManager, WorkspaceMember};
use crate::pluginjs::{
    BUNDLE_TARGET, LINT_TARGET, NODE_MODULES_SYNC_TARGET, PACKAGE_INFO_TARGET, PACKAGE_JSON,
    TEST_TARGET, TYPECHECK_TARGET, deps, importgraph, is_skipped_dir_name, package_json, platform,
    resolvers, thirdparty, toolchain,
};
use anyhow::Context;
use enclose::enclose;
use futures::future::BoxFuture;
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::provider::{
    ConfigRequest, ConfigResponse, GetError, GetRequest, GetResponse, ListPackageResponse,
    ListPackagesRequest, ListRequest, ListResponse, ProbeRequest, ProbeResponse,
    Provider as ProviderTrait, TargetSpec,
};
use hwalk::{CachedWalker, EntryKind, Ignore};
use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::OnceCell;

/// Default `js_test` test-file globs, applied when the provider's
/// `test_glob` option is absent — mirrors vitest's own default
/// (`**/*.{test,spec}.?(c|m)[jt]s?(x)`) and jest's `**/?(*.)+(spec|test).[tj]s?(x)`
/// convention (jest's separate `**/__tests__/**/*` convention is not matched
/// — a disclosed scope trim, not silent; see `driver_test.rs` module docs).
/// Matches this task's own example glob.
const DEFAULT_TEST_GLOBS: &[&str] = &["**/*.test.{ts,tsx,js,jsx}", "**/*.spec.{ts,tsx,js,jsx}"];

/// Provider construction config. See [`Provider::from_options`] for how each
/// field is populated from the provider's `options:` map.
pub struct Config {
    /// Which package manager's workspace-member convention applies (mirrors
    /// the Go plugin's required `gotool` option — provider-level, never a
    /// per-target variant).
    pub pkgmanager: PkgManager,
    /// Directories pruned during package discovery: engine skip dirs/globs
    /// plus this provider's own `skip` option. See [`hwalk::Ignore`].
    pub skip: Arc<Ignore>,
    /// Shared cross-run filesystem-walk cache. Disabled by default — unit
    /// tests build a bare provider and walk live; the cdylib wrapper injects
    /// the real shared walker via `from_options`.
    pub walker: Arc<CachedWalker>,
    /// Package names (bare `"name"`, or pinned `"name@version"`) allowed to
    /// run lifecycle scripts during `js_install`. Off for everything else —
    /// see `ai-docs/js-plugin-plan.md`'s Hermeticity section: heph owns this
    /// allowlist itself, uniformly across both managers.
    pub allow_scripts: Vec<String>,
    /// Which TypeScript toolchain `js_typecheck` uses — mirrors the Go
    /// plugin's `gotool` axis (provider-level, never a variant), but with
    /// exactly one supported value today: `"host"`
    /// (`toolchain::HOST`/`toolchain::is_host`) — see `toolchain.rs` module
    /// docs for why. Defaults to `"host"` since it is currently the *only*
    /// working value — unlike `pkgmanager`, there is no genuine ambiguity to
    /// force an explicit choice over.
    pub tstool: String,
    /// Which test runner `js_test` invokes — `"vitest"` (default) or
    /// `"jest"`, per `ai-docs/js-plugin-plan.md`'s `js_test` row. Same
    /// provider-level, host-toolchain shape as `tstool` — see
    /// `toolchain::resolve_host_test_runner`.
    pub testrunner: String,
    /// Glob patterns (wax syntax) a package's own first-party source files
    /// are matched against to discover `js_test` targets. Defaults to
    /// [`DEFAULT_TEST_GLOBS`] when unset — see that constant's doc for why.
    pub test_glob: Vec<String>,
    /// Which linter `js_lint` invokes — `"oxlint"` (default) or `"eslint"`,
    /// per `ai-docs/js-plugin-plan.md`'s `js_lint` row. Same provider-level,
    /// host-toolchain shape as `tstool`/`testrunner` — see
    /// `toolchain::resolve_host_linter`.
    pub linter: String,
    /// Which bundler `js_bundle` invokes — `"esbuild"` (default, the only
    /// value implemented this milestone), per `ai-docs/js-plugin-plan.md`'s
    /// `js_bundle` row. Same provider-level, host-toolchain shape as
    /// `tstool`/`testrunner`/`linter` — see `toolchain::resolve_host_bundler`.
    pub bundler: String,
}

impl Config {
    fn new(pkgmanager: PkgManager) -> Self {
        Self {
            pkgmanager,
            skip: Arc::new(Ignore::default()),
            walker: Arc::new(CachedWalker::disabled()),
            allow_scripts: Vec::new(),
            tstool: toolchain::HOST.to_string(),
            testrunner: toolchain::VITEST.to_string(),
            test_glob: DEFAULT_TEST_GLOBS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            linter: toolchain::OXLINT.to_string(),
            bundler: toolchain::ESBUILD.to_string(),
        }
    }
}

pub struct Provider {
    workspace_root: PathBuf,
    pkgmanager: PkgManager,
    skip: Arc<Ignore>,
    walker: Arc<CachedWalker>,
    allow_scripts: Vec<String>,
    tstool: String,
    testrunner: String,
    test_glob: Vec<String>,
    /// Lazily parsed lockfile and its derived [`ResolvedGraph`], keyed by
    /// discovered lockfile root (see [`Provider::find_lockfile_root`]) —
    /// each `Provider::get` for a third-party `js_install` addr or a
    /// package's declared deps would otherwise re-read and re-parse the
    /// whole lockfile from scratch. Keyed rather than a single cell since a
    /// workspace may contain more than one independent npm/pnpm project.
    lockfile_cache: LockfileCache,
    resolved_graph_cache: ResolvedGraphCache,
    /// The full set of lockfile roots under `workspace_root`, discovered by
    /// [`collect_lockfile_roots`] and cached once for the `Provider`'s
    /// lifetime — same rationale as `tsc_cache`: a `Provider` lifetime maps
    /// to one build invocation over a presumed-static tree, and
    /// [`Provider::find_resolved_graph_for`] now walks the whole workspace
    /// on every third-party `js_install` resolution (see its doc), so this
    /// is the difference between one real filesystem walk per `Provider`
    /// and one per distinct `(name, version)` resolved in the run.
    lockfile_roots_cache: OnceCell<Arc<Vec<PathBuf>>>,
    /// Lazily resolved host `tsc` binary path + queried `tsc --version`,
    /// cached once for the `Provider`'s lifetime — same rationale as
    /// `lockfile_cache`/`resolved_graph_cache`: `typecheck_config` runs once
    /// per `js_typecheck` target per `Provider::get`, and the host toolchain
    /// resolution/version query is identical across every package in the
    /// workspace, so re-resolving and re-spawning `tsc --version` per package
    /// would scale a real subprocess cost linearly with package count on
    /// every single invocation, including a 100%-cache-hit run (M3 review
    /// finding).
    tsc_cache: OnceCell<Arc<(PathBuf, String)>>,
    /// Lazily resolved host test-runner binary path + queried `--version`,
    /// cached once for the `Provider`'s lifetime — same rationale as
    /// `tsc_cache`: `test_config` runs once per `js_test` target (one per
    /// test *file*, potentially many per package) per `Provider::get`, so
    /// re-resolving and re-spawning `<runner> --version` per target would
    /// scale a real subprocess cost with the number of test files, not just
    /// packages.
    testrunner_cache: OnceCell<Arc<(PathBuf, String)>>,
    /// Which linter `js_lint` invokes — see [`Config::linter`]'s doc.
    linter: String,
    /// Lazily resolved host linter binary path + queried `--version`, cached
    /// once for the `Provider`'s lifetime — same rationale as
    /// `tsc_cache`/`testrunner_cache`: `lint_config` runs once per `js_lint`
    /// target (one per package) per `Provider::get`, so re-resolving and
    /// re-spawning `<linter> --version` per package would scale a real
    /// subprocess cost with package count, including a 100%-cache-hit run
    /// (the same M3/M4-review-flagged mistake this milestone's task
    /// explicitly calls out not to repeat).
    linter_cache: OnceCell<Arc<(PathBuf, String)>>,
    /// Which bundler `js_bundle` invokes — see [`Config::bundler`]'s doc.
    bundler: String,
    /// Lazily resolved host bundler binary path + queried `--version`,
    /// cached once for the `Provider`'s lifetime — same rationale as
    /// `tsc_cache`/`testrunner_cache`/`linter_cache`: `bundle_config` runs
    /// once per `js_bundle` target per `Provider::get`, so re-resolving and
    /// re-spawning `<bundler> --version` per target would scale a real
    /// subprocess cost with target count, including a 100%-cache-hit run
    /// (the same M3/M4-review-flagged mistake this milestone's task
    /// explicitly calls out not to repeat).
    bundler_cache: OnceCell<Arc<(PathBuf, String)>>,
    /// Workspace-member `{name -> addr}` map, resolved once and cached for
    /// the `Provider`'s lifetime — same rationale as `tsc_cache`/
    /// `testrunner_cache`/`linter_cache`: `deps_config`/`typecheck_config`/
    /// `test_config`/`lint_config` each independently called
    /// `member_addrs_by_name_blocking` (a full recursive workspace walk +
    /// `package.json` parse of every package, plus a glob-match) from
    /// scratch on every single call — for `test_config` that meant once
    /// *per test file*. This is the identical "recompute-on-every-call"
    /// shape `graph_cache` fixes for the import graph above, applied to
    /// workspace-member discovery (feature-quality/code-quality M5 review
    /// finding: fixing it for one O(P) walk while leaving an identically-
    /// shaped one right next to it unfixed left O(P²) work on the table at
    /// scale). Workspace membership can't change mid-`Provider`-lifetime any
    /// more than the lockfile can, so a single cached value is correct for
    /// every caller.
    member_addrs_cache: OnceCell<Arc<BTreeMap<String, String>>>,
    /// Per-package [`importgraph::ImportGraph`], parsed+resolved once per
    /// package and cached for the `Provider`'s lifetime — see
    /// [`Provider::import_graph`]'s doc for the M2/M4-review-flagged perf
    /// issue this fixes: each of `deps_config`/`typecheck_config`/
    /// `test_config` independently called
    /// `importgraph::build_package_import_graph` (a full oxc_parser parse +
    /// oxc_resolver resolve of every first-party file in the package) from
    /// scratch on every single `Provider::get` call — for `js_test` this
    /// meant once *per test file*, so a package with N source files and T
    /// test files paid `2+T` full-package graph builds where 1 would do.
    ///
    /// Keyed per-package with an `Arc<OnceCell<_>>` per key rather than one
    /// `Mutex` held across the build itself, so unrelated packages' builds
    /// never serialize behind one lock — only concurrent `Provider::get`
    /// calls for the *same* package coalesce onto one build, which is the
    /// point.
    graph_cache: tokio::sync::Mutex<HashMap<PkgBuf, Arc<OnceCell<Arc<importgraph::ImportGraph>>>>>,
    /// Test-only: counts real `importgraph::build_package_import_graph`
    /// invocations (cache misses), so a test can prove `graph_cache` actually
    /// memoizes across independent callers rather than merely being
    /// structurally present — see
    /// `import_graph_is_shared_across_independent_callers` in this module's
    /// tests.
    #[cfg(test)]
    graph_build_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Per-`(entry_pkg, entry_file_rel)` [`BundleClosureResult`], built once
    /// and cached for the `Provider`'s lifetime — same `Arc<OnceCell<_>>`-
    /// per-key shape as `graph_cache`, applied to `Provider::bundle_closure`'s
    /// whole-graph BFS (feature-quality/hermeticity M6 review finding): the
    /// closure is provably invariant across `js_bundle`'s `format`/`target`
    /// variant axis for the same entry point, but was recomputed from
    /// scratch — manifest re-reads, phantom-dependency cross-checks, edge
    /// walk and all — on every single `Provider::get`, unlike every other
    /// expensive per-target-kind computation in this file. Requesting the
    /// common esm+cjs (or four-way esm/cjs × node/browser) pair for one
    /// package now pays the BFS once, not once per variant.
    bundle_closure_cache: BundleClosureCache,
}

/// Key: `(entry_pkg, entry_file_rel)`. See [`Provider::bundle_closure_cache`]'s
/// doc — factored into its own alias since the nested `Arc<OnceCell<_>>>`
/// shape trips clippy's `type_complexity` inline.
type BundleClosureCache =
    tokio::sync::Mutex<HashMap<(String, String), Arc<OnceCell<Arc<BundleClosureResult>>>>>;

/// Key: a discovered lockfile root (absolute path). See
/// [`Provider::lockfile_cache`]'s doc.
type LockfileCache = tokio::sync::Mutex<HashMap<PathBuf, Arc<OnceCell<Option<Arc<Lockfile>>>>>>;
/// Key: a discovered lockfile root (absolute path). See
/// [`Provider::resolved_graph_cache`]'s doc.
type ResolvedGraphCache = tokio::sync::Mutex<HashMap<PathBuf, Arc<OnceCell<Arc<ResolvedGraph>>>>>;

/// Key: a `js_bundle` closure walk's `cur_pkg`. See
/// [`Provider::bundle_closure_uncached`]'s per-package lockfile lookup.
type PkgLockfileInfo = HashMap<String, (String, Option<Arc<Lockfile>>, Option<Arc<ResolvedGraph>>)>;

impl Provider {
    pub fn new(workspace_root: PathBuf, pkgmanager: PkgManager) -> Self {
        Self::with_config(workspace_root, Config::new(pkgmanager))
    }

    pub fn from_options(
        workspace_root: PathBuf,
        skip_dirs: &[PathBuf],
        skip_globs: &[String],
        opts: &hplugin::config::Options,
        walker: Arc<CachedWalker>,
    ) -> anyhow::Result<Self> {
        // `pkgmanager` selects the workspace-member convention and is
        // REQUIRED — there is no implicit default (a repo with both a
        // `pnpm-workspace.yaml` and a `package.json` "workspaces" array would
        // otherwise have an ambiguous, silently-picked answer).
        hplugin::config::deny_unknown(
            "js provider",
            opts,
            &[
                "pkgmanager",
                "skip",
                "allow_scripts",
                "tstool",
                "testrunner",
                "test_glob",
                "linter",
                "bundler",
            ],
        )?;
        let pkgmanager_str: String =
            hplugin::config::decode_opt(opts, "js provider", "pkgmanager")?.ok_or_else(|| {
                anyhow::anyhow!(
                    "js provider: `pkgmanager` is required — set it to \"npm\" or \"pnpm\""
                )
            })?;
        let pkgmanager = PkgManager::parse(&pkgmanager_str)?;

        // Engine-wide `fs.skip` globs are merged ahead of this provider's own
        // `skip` option so both prune the same workspace-relative paths.
        let mut globs = skip_globs.to_vec();
        let user_skip: Vec<String> =
            hplugin::config::decode_opt(opts, "js provider", "skip")?.unwrap_or_default();
        globs.extend(user_skip);
        let skip = Arc::new(Ignore::new(skip_dirs, &globs)?);

        // Off by default — see `Config::allow_scripts`.
        let allow_scripts: Vec<String> =
            hplugin::config::decode_opt(opts, "js provider", "allow_scripts")?.unwrap_or_default();

        // See `Config::tstool`'s doc: defaults to the only supported value.
        let tstool: String = hplugin::config::decode_opt(opts, "js provider", "tstool")?
            .unwrap_or_else(|| toolchain::HOST.to_string());

        // See `Config::testrunner`'s doc: defaults to the design doc's
        // recommended default.
        let testrunner: String = hplugin::config::decode_opt(opts, "js provider", "testrunner")?
            .unwrap_or_else(|| toolchain::VITEST.to_string());
        anyhow::ensure!(
            toolchain::is_supported_testrunner(&testrunner),
            "js provider: unsupported `testrunner` {testrunner:?} — expected \"vitest\" or \"jest\""
        );

        // See `DEFAULT_TEST_GLOBS`'s doc: defaults to vitest/jest's own
        // conventions when unset.
        let test_glob: Vec<String> = hplugin::config::decode_opt(opts, "js provider", "test_glob")?
            .unwrap_or_else(|| {
                DEFAULT_TEST_GLOBS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            });

        // See `Config::linter`'s doc: defaults to the design doc's
        // recommended default (`oxlint`, the fast oxc-family syntactic
        // linter; `eslint` is the alt for type-aware rules).
        let linter: String = hplugin::config::decode_opt(opts, "js provider", "linter")?
            .unwrap_or_else(|| toolchain::OXLINT.to_string());
        anyhow::ensure!(
            toolchain::is_supported_linter(&linter),
            "js provider: unsupported `linter` {linter:?} — expected \"oxlint\" or \"eslint\""
        );

        // See `Config::bundler`'s doc: defaults to the design doc's
        // recommended default (`esbuild`, the fast oxc-family-adjacent
        // bundler; `rollup`/`webpack`/`vite` are a stated M6+ follow-up).
        let bundler: String = hplugin::config::decode_opt(opts, "js provider", "bundler")?
            .unwrap_or_else(|| toolchain::ESBUILD.to_string());
        anyhow::ensure!(
            toolchain::is_supported_bundler(&bundler),
            "js provider: unsupported `bundler` {bundler:?} — expected \"esbuild\" \
             (rollup/webpack/vite are a stated M6+ follow-up, not yet supported)"
        );

        Ok(Self::with_config(
            workspace_root,
            Config {
                pkgmanager,
                skip,
                walker,
                allow_scripts,
                tstool,
                testrunner,
                test_glob,
                linter,
                bundler,
            },
        ))
    }

    pub fn with_config(workspace_root: PathBuf, config: Config) -> Self {
        Self {
            workspace_root,
            pkgmanager: config.pkgmanager,
            skip: config.skip,
            walker: config.walker,
            allow_scripts: config.allow_scripts,
            tstool: config.tstool,
            testrunner: config.testrunner,
            test_glob: config.test_glob,
            lockfile_cache: tokio::sync::Mutex::new(HashMap::new()),
            resolved_graph_cache: tokio::sync::Mutex::new(HashMap::new()),
            lockfile_roots_cache: OnceCell::new(),
            tsc_cache: OnceCell::new(),
            testrunner_cache: OnceCell::new(),
            linter: config.linter,
            linter_cache: OnceCell::new(),
            bundler: config.bundler,
            bundler_cache: OnceCell::new(),
            member_addrs_cache: OnceCell::new(),
            graph_cache: tokio::sync::Mutex::new(HashMap::new()),
            #[cfg(test)]
            graph_build_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            bundle_closure_cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Every `package.json`-anchored package under `prefix` (workspace-root
    /// relative), regardless of workspace membership. This is what
    /// `Provider::list_packages` serves — see [`collect_js_packages`].
    pub fn packages_under(&self, prefix: &str) -> anyhow::Result<Vec<PkgBuf>> {
        let search_dir = if prefix.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(prefix)
        };
        let mut result = Vec::new();
        collect_js_packages(
            &self.walker,
            &search_dir,
            &self.workspace_root,
            &self.skip,
            &mut result,
        );
        result.into_iter().collect()
    }

    /// Resolve the workspace-member list for the configured package manager:
    /// pnpm-workspace.yaml's `packages` globs, or npm's root `package.json`
    /// `"workspaces"` globs, matched against the discovered `package.json`
    /// set and unified into one manager-agnostic list.
    pub fn workspace_members(&self) -> anyhow::Result<Vec<WorkspaceMember>> {
        let patterns = match self.pkgmanager {
            PkgManager::Npm => workspace::read_npm_workspace_globs(&self.workspace_root)?,
            PkgManager::Pnpm => workspace::read_pnpm_workspace_globs(&self.workspace_root)?,
        };
        if patterns.is_empty() {
            return Ok(Vec::new());
        }
        let packages = self.packages_under("")?;
        workspace::resolve_members(&self.workspace_root, &packages, &patterns)
    }

    /// The workspace's lockfile, parsed once and cached for the Provider's
    /// lifetime, per discovered root. `None` when no ancestor of `pkg_dir`,
    /// up to and including `self.workspace_root`, has a lockfile file — not
    /// every workspace needs one (a package with zero third-party
    /// dependencies), so its absence is only an error at the point a
    /// resolution is actually attempted against it (see `deps::resolve_package_deps`).
    ///
    /// `pkg_dir`-relative, not a single fixed `self.workspace_root`-relative
    /// path: this repo may contain more than one independent npm/pnpm
    /// project (each with its own lockfile) nested at different points in
    /// one heph workspace — auto-discovered by lockfile presence via an
    /// ancestor walk, the exact same pattern `importgraph`'s own
    /// tsconfig/eslint/bundler-config discovery already uses (see
    /// `importgraph::find_nearest_file`), rather than requiring every js
    /// package in the whole workspace to share one lockfile at a single
    /// configured root. Returns the discovered root alongside the parsed
    /// lockfile since callers need it to translate a workspace-relative
    /// `pkg` into a path relative to *this* lockfile's own root before
    /// calling `Lockfile::resolve_dependency`/`lockfile::resolve_transitive`
    /// — see `Provider::lockfile_relative_pkg`.
    async fn lockfile(&self, pkg_dir: &Path) -> anyhow::Result<Option<(PathBuf, Arc<Lockfile>)>> {
        let Some(root) = self.find_lockfile_root(pkg_dir) else {
            return Ok(None);
        };
        let lf = self.lockfile_at_root(&root).await?;
        Ok(lf.map(|lf| (root, lf)))
    }

    /// Ancestor-search from `pkg_dir` (inclusive) up to `self.workspace_root`
    /// for the nearest directory containing a lockfile file. Cheap (a bounded
    /// number of `is_file` stats, no directory listing) — not itself cached,
    /// unlike the lockfile's own parsed content. Delegates to
    /// `importgraph::find_nearest_file`, the same walk-up-by-presence logic
    /// tsconfig/eslint/bundler-config discovery already uses.
    fn find_lockfile_root(&self, pkg_dir: &Path) -> Option<PathBuf> {
        let filename = Lockfile::filename(self.pkgmanager);
        let found = importgraph::find_nearest_file(&self.workspace_root, pkg_dir, &[filename])?;
        // `find_nearest_file` returns `dir.join(filename)` for a directory it
        // walked to, so `.parent()` is always `Some`.
        found.parent().map(Path::to_path_buf)
    }

    /// Load+parse the lockfile at exactly `root` (already discovered by
    /// [`Provider::find_lockfile_root`]), cached per root so multiple
    /// packages sharing one npm/pnpm project don't each re-read and
    /// re-parse the same (potentially large) file.
    async fn lockfile_at_root(&self, root: &Path) -> anyhow::Result<Option<Arc<Lockfile>>> {
        let pkgmanager = self.pkgmanager;
        let root = root.to_path_buf();
        let cell = {
            let mut cache = self.lockfile_cache.lock().await;
            Arc::clone(
                cache
                    .entry(root.clone())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        let result = cell
            .get_or_try_init(|| async move {
                let path = root.join(Lockfile::filename(pkgmanager));
                let read = hcore::blocking::run(enclose!((path) move || {
                    if !path.is_file() {
                        return Ok(None);
                    }
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))
                        .map(Some)
                }))
                .await?;
                let Some(contents) = read else {
                    return anyhow::Ok(None);
                };
                let lf = Lockfile::parse(pkgmanager, &contents)
                    .with_context(|| format!("parsing {}", path.display()))?;
                anyhow::Ok(Some(Arc::new(lf)))
            })
            .await?;
        Ok(result.clone())
    }

    /// [`Provider::lockfile`], plus the flattened [`ResolvedGraph`] — parsed
    /// once and cached alongside the lockfile itself, per discovered root.
    ///
    /// Callers that already hold the result of a prior [`Provider::lockfile`]
    /// call for the same `pkg_dir` should use
    /// [`Provider::resolved_graph_for_lockfile`] instead — this fn re-runs
    /// `lockfile`'s own ancestor walk internally, which is wasted work when
    /// the caller already paid for it.
    async fn resolved_graph(
        &self,
        pkg_dir: &Path,
    ) -> anyhow::Result<Option<(PathBuf, Arc<ResolvedGraph>)>> {
        let lockfile = self.lockfile(pkg_dir).await?;
        let root = lockfile.as_ref().map(|(root, _)| root.clone());
        let graph = self.resolved_graph_for_lockfile(&lockfile).await?;
        Ok(root.zip(graph))
    }

    /// [`Provider::resolved_graph`], but for a `(root, lockfile)` pair the
    /// caller already resolved via [`Provider::lockfile`] — skips repeating
    /// its ancestor walk. `None` in, `Ok(None)` out. `Err` when the
    /// lockfile itself is internally inconsistent — see
    /// `lockfile::entries_agree_where_comparable`'s doc.
    async fn resolved_graph_for_lockfile(
        &self,
        lockfile: &Option<(PathBuf, Arc<Lockfile>)>,
    ) -> anyhow::Result<Option<Arc<ResolvedGraph>>> {
        let Some((root, lockfile)) = lockfile.as_ref() else {
            return Ok(None);
        };
        let cell = {
            let mut cache = self.resolved_graph_cache.lock().await;
            Arc::clone(
                cache
                    .entry(root.clone())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        let lockfile = Arc::clone(lockfile);
        let graph = cell
            .get_or_try_init(|| async move { lockfile.resolved_graph().map(Arc::new) })
            .await?;
        Ok(Some(Arc::clone(graph)))
    }

    /// Translate a `self.workspace_root`-relative `pkg` (heph's own package
    /// addressing) into a path relative to `lockfile_root` (a lockfile's own
    /// root, from [`Provider::lockfile`]/[`Provider::resolved_graph`]) — the
    /// basis every `Lockfile::resolve_dependency`/`lockfile::resolve_transitive`
    /// call needs, since a lockfile's own `importers`/`node_modules`-ancestor
    /// lookups are relative to *its* root, not necessarily
    /// `self.workspace_root`. Identical to `pkg` whenever the lockfile's own
    /// root *is* `self.workspace_root` (the common single-project case).
    fn lockfile_relative_pkg(&self, lockfile_root: &Path, pkg: &str) -> String {
        let pkg_dir = if pkg.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(pkg)
        };
        // `lockfile_root` is always an ancestor of `pkg_dir` by construction
        // — every caller gets it from `Provider::find_lockfile_root`, which
        // only ever walks *upward* from `pkg_dir` and returns an ancestor it
        // found on that walk. `expect`, not a silent same-path fallback: if
        // that invariant is ever broken by a future refactor, this must fail
        // loudly rather than hand an absolute, host-specific path into a
        // `Lockfile`'s importer-relative lookup.
        pkg_dir
            .strip_prefix(lockfile_root)
            .expect(
                "find_lockfile_root should only ever return an ancestor of pkg_dir, so \
                 pkg_dir must be under lockfile_root",
            )
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Search every lockfile root this workspace has, for a [`ResolvedGraph`]
    /// entry naming `name`/`version`. Used by
    /// [`Provider::thirdparty_install_spec`], which resolves a bare
    /// `(name, version)` addr with no originating package to ancestor-search
    /// from at all.
    ///
    /// Always walks the full workspace for lockfile roots — see
    /// [`collect_lockfile_roots`] — in addition to checking whatever is
    /// already cached (see [`Provider::lockfile`]). A cache hit alone is not
    /// sufficient: it proves *one* root resolves this package, but says
    /// nothing about whether some *other*, not-yet-resolved root resolves it
    /// differently. Skipping the walk whenever the cache already has a match
    /// would make the ambiguity check below order-dependent — only firing
    /// when the conflicting root happened to be resolved first — which
    /// silently defeats it in the far more common case where roots are
    /// resolved one at a time as the engine walks the dependency graph.
    ///
    /// The walk also covers the fully-cold case: a `js_install` target can be
    /// requested directly (a user running
    /// `heph run @heph/js/thirdparty/lodash@4.17.21` with nothing having
    /// resolved it as a dependency first, or a test exercising the driver in
    /// isolation) where nothing is cached yet at all.
    ///
    /// **Does not stop at the first match.** `thirdparty_addr`'s scheme
    /// (bare `name`/`version`/`os`/`arch`, no project scoping) is only a
    /// valid cache key when a published package's metadata is genuinely the
    /// same regardless of which project's lockfile recorded it — true for
    /// one lockfile, but this `Provider` can now discover *several*
    /// independent ones (see this module's multi-project support), and
    /// nothing stops two unrelated projects from validly pinning the same
    /// `(name, version)` against different registries/mirrors with
    /// genuinely different content. Picking whichever root happened to
    /// answer first — process-randomized `HashMap` iteration order for the
    /// cached case — would silently build one project against another's
    /// package, differently across runs of the identical command. So every
    /// discovered root is checked, and a genuine disagreement in the
    /// resolved record (`integrity`, `resolved`, or any other field — full
    /// [`ResolvedPackage`] equality, not just `integrity`: `resolved` alone
    /// feeds `JsInstallDef`'s hash too, see `driver_install.rs`) fails
    /// loudly naming both roots, rather than picking one.
    async fn find_resolved_graph_for(
        &self,
        name: &str,
        version: &str,
    ) -> anyhow::Result<Option<Arc<ResolvedGraph>>> {
        let mut matches: Vec<(PathBuf, Arc<ResolvedGraph>)> = Vec::new();
        let mut seen_roots: HashSet<PathBuf> = HashSet::new();
        {
            let cache = self.resolved_graph_cache.lock().await;
            for (root, cell) in cache.iter() {
                if let Some(graph) = cell.get()
                    && graph.get(name, version).is_some()
                {
                    matches.push((root.clone(), Arc::clone(graph)));
                    seen_roots.insert(root.clone());
                }
            }
        }

        // Always walk, even when the cache scan above already found a match —
        // a match served from the cache only proves *one* root resolves this
        // package; it says nothing about a second, conflicting root that
        // simply hasn't been resolved (and therefore cached) yet. Skipping
        // the walk whenever the cache is non-empty would make the ambiguity
        // check above order-dependent: it would only fire when the
        // conflicting root happened to be resolved first. See this fn's
        // doc — the whole point is to check every discovered root.
        let walker = Arc::clone(&self.walker);
        let skip = Arc::clone(&self.skip);
        let workspace_root = self.workspace_root.clone();
        let filename = Lockfile::filename(self.pkgmanager);
        let roots = self
            .lockfile_roots_cache
            .get_or_init(|| async move {
                let roots = hcore::blocking::run(move || {
                    let mut roots = Vec::new();
                    collect_lockfile_roots(
                        &walker,
                        &workspace_root,
                        &workspace_root,
                        filename,
                        &skip,
                        &mut roots,
                    );
                    roots
                })
                .await;
                Arc::new(roots)
            })
            .await;
        for root in roots.iter() {
            if seen_roots.contains(root) {
                continue;
            }
            if let Some((root, graph)) = self.resolved_graph(root).await?
                && graph.get(name, version).is_some()
            {
                seen_roots.insert(root.clone());
                matches.push((root, graph));
            }
        }

        // Sorted by root path for deterministic error attribution and a
        // deterministic pick in the (degenerate) all-empty-integrity
        // fallback below — `matches` was assembled from two `HashMap`
        // iterations (the cache scan, and `seen_roots`'s membership order
        // has no bearing on `roots.iter()`'s own order either), so without
        // this, which root's data appears first in an ambiguity error — or
        // gets silently picked when literally nothing has real integrity —
        // would vary process-to-process for the identical command.
        matches.sort_by(|(a, _), (b, _)| a.cmp(b));

        // Prefer an entry with real (non-empty) `integrity` as the "winner"
        // returned below — it's the one `js_install`'s own integrity
        // verification can actually validate against (see
        // `driver_install.rs`'s `verify_integrity`); an empty-integrity
        // entry never had this package's content genuinely pinned by that
        // root's own lockfile in the first place (a peer-suffixed/deduped
        // graph key folded onto one node picking up a variant with no
        // resolution data of its own is a real, observed shape, not
        // hypothetical — confirmed live in a real workspace).
        //
        // This is a preference for *which entry to return*, not an excuse to
        // skip comparing the rest: every discovered root's entry still
        // participates in the ambiguity check below, via
        // `entries_agree_where_comparable`, which only exempts the
        // `integrity` *field* itself when one side is empty — a real
        // divergence on `resolved`/`os`/`cpu`/`dependencies`/
        // `has_install_script` still fails loudly regardless of which side's
        // `integrity` happens to be blank. An earlier version of this fix
        // dropped the *whole entry* on empty integrity, which would have
        // silently let a genuinely different `resolved` URL on that root
        // through unchecked — caught in review before merge.
        let Some((first_root, first_graph)) = matches
            .iter()
            .find(|(_, graph)| {
                !graph
                    .get(name, version)
                    .expect("just matched above")
                    .integrity
                    .is_empty()
            })
            .or(matches.first())
        else {
            return Ok(None);
        };
        let first_entry = first_graph.get(name, version).expect("just matched above");
        for (root, graph) in matches.iter() {
            if std::ptr::eq(root, first_root) {
                continue;
            }
            let entry = graph.get(name, version).expect("just matched above");
            anyhow::ensure!(
                lockfile::entries_agree_where_comparable(first_entry, entry),
                "js provider: {name}@{version} resolves to different content in two \
                 independent lockfiles this workspace discovered — {first_root:?} records \
                 integrity {:?} resolved from {:?}, {root:?} records integrity {:?} resolved \
                 from {:?}. Both projects declare this dependency, but it is not the same \
                 package (a different registry/mirror, or a real supply-chain concern) — heph \
                 cannot pick one silently; rename or otherwise disambiguate so each project's \
                 own install resolves it unambiguously",
                first_entry.integrity,
                first_entry.resolved,
                entry.integrity,
                entry.resolved,
            );
        }
        Ok(Some(Arc::clone(first_graph)))
    }

    /// The host `tsc` binary path and its queried `--version`, resolved once
    /// and cached for the `Provider`'s lifetime — see [`Provider::tsc_cache`]'s
    /// doc for why this matters (a real subprocess spawn, otherwise repeated
    /// once per `js_typecheck` target per `Provider::get`).
    async fn resolved_host_tsc(&self) -> anyhow::Result<Arc<(PathBuf, String)>> {
        let workspace_root = self.workspace_root.clone();
        let tstool = self.tstool.clone();
        let result = self
            .tsc_cache
            .get_or_try_init(|| async move {
                anyhow::ensure!(
                    toolchain::is_host(&tstool),
                    "js provider: unsupported tstool {tstool:?} — only \"host\" is supported in \
                     this milestone (no hermetic TypeScript toolchain exists yet); see \
                     pluginjs::toolchain module docs"
                );
                hcore::blocking::run(move || -> anyhow::Result<Arc<(PathBuf, String)>> {
                    let tsc_bin = toolchain::resolve_host_tsc(&workspace_root)
                        .context("resolving the js_typecheck tsc toolchain")?;
                    let tsc_version = toolchain::query_tsc_version(&tsc_bin)
                        .with_context(|| format!("querying {tsc_bin:?} --version"))?;
                    Ok(Arc::new((tsc_bin, tsc_version)))
                })
                .await
            })
            .await?;
        Ok(Arc::clone(result))
    }

    /// The host test-runner binary path and its queried `--version`, resolved
    /// once and cached for the `Provider`'s lifetime — see
    /// [`Provider::testrunner_cache`]'s doc for why this matters.
    async fn resolved_host_test_runner(&self) -> anyhow::Result<Arc<(PathBuf, String)>> {
        let workspace_root = self.workspace_root.clone();
        let testrunner = self.testrunner.clone();
        let result = self
            .testrunner_cache
            .get_or_try_init(|| async move {
                anyhow::ensure!(
                    toolchain::is_supported_testrunner(&testrunner),
                    "js provider: unsupported testrunner {testrunner:?} — only \"vitest\" or \
                     \"jest\" is supported in this milestone; see pluginjs::toolchain module docs"
                );
                hcore::blocking::run(move || -> anyhow::Result<Arc<(PathBuf, String)>> {
                    let runner_bin =
                        toolchain::resolve_host_test_runner(&workspace_root, &testrunner)
                            .context("resolving the js_test runner toolchain")?;
                    let runner_version = toolchain::query_test_runner_version(&runner_bin)
                        .with_context(|| format!("querying {runner_bin:?} --version"))?;
                    Ok(Arc::new((runner_bin, runner_version)))
                })
                .await
            })
            .await?;
        Ok(Arc::clone(result))
    }

    /// The host linter binary path and its queried `--version`, resolved once
    /// and cached for the `Provider`'s lifetime — see
    /// [`Provider::linter_cache`]'s doc for why this matters.
    async fn resolved_host_linter(&self) -> anyhow::Result<Arc<(PathBuf, String)>> {
        let workspace_root = self.workspace_root.clone();
        let linter = self.linter.clone();
        let result = self
            .linter_cache
            .get_or_try_init(|| async move {
                anyhow::ensure!(
                    toolchain::is_supported_linter(&linter),
                    "js provider: unsupported linter {linter:?} — only \"oxlint\" or \"eslint\" \
                     is supported in this milestone; see pluginjs::toolchain module docs"
                );
                hcore::blocking::run(move || -> anyhow::Result<Arc<(PathBuf, String)>> {
                    let linter_bin = toolchain::resolve_host_linter(&workspace_root, &linter)
                        .context("resolving the js_lint linter toolchain")?;
                    let linter_version = toolchain::query_linter_version(&linter_bin)
                        .with_context(|| format!("querying {linter_bin:?} --version"))?;
                    Ok(Arc::new((linter_bin, linter_version)))
                })
                .await
            })
            .await?;
        Ok(Arc::clone(result))
    }

    /// The host bundler binary path and its queried `--version`, resolved
    /// once and cached for the `Provider`'s lifetime — see
    /// [`Provider::bundler_cache`]'s doc for why this matters.
    async fn resolved_host_bundler(&self) -> anyhow::Result<Arc<(PathBuf, String)>> {
        let workspace_root = self.workspace_root.clone();
        let bundler = self.bundler.clone();
        let result = self
            .bundler_cache
            .get_or_try_init(|| async move {
                anyhow::ensure!(
                    toolchain::is_supported_bundler(&bundler),
                    "js provider: unsupported bundler {bundler:?} — only \"esbuild\" is \
                     supported in this milestone; see pluginjs::toolchain module docs"
                );
                hcore::blocking::run(move || -> anyhow::Result<Arc<(PathBuf, String)>> {
                    let bundler_bin = toolchain::resolve_host_bundler(&workspace_root, &bundler)
                        .context("resolving the js_bundle bundler toolchain")?;
                    let bundler_version = toolchain::query_bundler_version(&bundler_bin)
                        .with_context(|| format!("querying {bundler_bin:?} --version"))?;
                    Ok(Arc::new((bundler_bin, bundler_version)))
                })
                .await
            })
            .await?;
        Ok(Arc::clone(result))
    }

    /// Workspace-member `{name -> addr}` map, resolved once and cached for
    /// the `Provider`'s lifetime — see [`Provider::member_addrs_cache`]'s doc
    /// for why this matters. Every one of `deps_config`/`typecheck_config`/
    /// `test_config`/`lint_config` calls this instead of redoing the
    /// discovery walk inside its own blocking closure.
    async fn member_addrs_by_name(&self) -> anyhow::Result<Arc<BTreeMap<String, String>>> {
        let workspace_root = self.workspace_root.clone();
        let walker = Arc::clone(&self.walker);
        let skip = Arc::clone(&self.skip);
        let pkgmanager = self.pkgmanager;
        let result = self
            .member_addrs_cache
            .get_or_try_init(|| async move {
                hcore::blocking::run(move || -> anyhow::Result<Arc<BTreeMap<String, String>>> {
                    member_addrs_by_name_blocking(&walker, &workspace_root, &skip, pkgmanager)
                        .map(Arc::new)
                })
                .await
            })
            .await?;
        Ok(Arc::clone(result))
    }

    /// Build the config for one `js_lint` target (one package): the host
    /// linter toolchain resolution/version-query this milestone's disclosed
    /// `linter`-is-host-resolved escape hatch requires (same shape as
    /// `resolved_host_tsc`/`resolved_host_test_runner`) plus
    /// `lint_deps_config`'s pure, linter-binary-free Input-scoping.
    ///
    /// Deliberately does **not** go through `Provider::import_graph`: unlike
    /// `deps_config`/`typecheck_config`/`test_config`, a `js_lint` target's
    /// Inputs are just the package's own first-party source files plus its
    /// resolved linter config (and, for eslint type-aware rules, the
    /// tsconfig/extends chain) — no cross-package import-graph edges are
    /// needed to scope it (see `driver_lint.rs` module docs' "Inputs / cache
    /// key" section). Skipping it here keeps `js_lint` from becoming a fifth
    /// caller of the expensive parse+resolve path for no reason, on top of
    /// the fix already applied to the other three.
    async fn lint_config(&self, pkg: &PkgBuf) -> anyhow::Result<HashMap<String, Value>> {
        let resolved_linter = self.resolved_host_linter().await?;
        let linter_bin = resolved_linter.0.to_string_lossy().into_owned();
        let linter_version = resolved_linter.1.clone();
        let pkg_str = pkg.as_str().to_string();
        let pkg_dir = if pkg_str.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(&pkg_str)
        };
        let lockfile = self.lockfile(&pkg_dir).await?;
        let resolved_graph = self.resolved_graph_for_lockfile(&lockfile).await?;
        let lockfile_pkg = lockfile
            .as_ref()
            .map(|(root, _)| self.lockfile_relative_pkg(root, &pkg_str))
            .unwrap_or_else(|| pkg_str.clone());
        let lockfile = lockfile.map(|(_, lf)| lf);
        let member_addrs_by_name = self.member_addrs_by_name().await?;
        let workspace_root = self.workspace_root.clone();
        let walker = Arc::clone(&self.walker);
        let linter = self.linter.clone();
        let os = platform::current_os();
        let arch = platform::current_arch();

        hcore::blocking::run(move || -> anyhow::Result<HashMap<String, Value>> {
            let lint_deps = lint_deps_config(
                &walker,
                &workspace_root,
                &pkg_str,
                &lockfile_pkg,
                &linter,
                lockfile.as_deref(),
                resolved_graph.as_deref(),
                &member_addrs_by_name,
                &os,
                &arch,
            )
            .with_context(|| format!("building js_lint inputs for {pkg_str:?}"))?;

            let mut config: HashMap<String, Value> = HashMap::new();
            config.insert("linter".to_string(), Value::String(linter));
            config.insert("linter_bin".to_string(), Value::String(linter_bin));
            config.insert("linter_version".to_string(), Value::String(linter_version));
            config.insert(
                "config_path".to_string(),
                Value::String(lint_deps.config_path),
            );
            config.insert(
                "config_content".to_string(),
                Value::String(lint_deps.config_content),
            );
            config.insert(
                "tsconfig_path".to_string(),
                Value::String(lint_deps.tsconfig_path),
            );
            config.insert(
                "tsconfig_content".to_string(),
                Value::String(lint_deps.tsconfig_content),
            );
            config.insert("deps".to_string(), Value::Map(lint_deps.deps));
            Ok(config)
        })
        .await
    }

    /// The package's [`importgraph::ImportGraph`], built once and cached for
    /// the `Provider`'s lifetime — see [`Provider::graph_cache`]'s doc for why
    /// this exists. `deps_config`, `typecheck_config` (via
    /// `typecheck_deps_config`), and `test_config` (via `test_deps_config`,
    /// once per test *file*) all go through this single entry point so a
    /// package with N source files and T test files pays one full-package
    /// graph build, not `2+T`.
    ///
    /// Locking: the outer `graph_cache` mutex is only held long enough to
    /// get-or-insert this package's own `OnceCell` — the actual parse+resolve
    /// work runs after it's released, inside that per-package cell's
    /// `get_or_try_init`. Concurrent `Provider::get` calls for *different*
    /// packages therefore never serialize behind one lock; concurrent calls
    /// for the *same* package correctly coalesce onto one build via the cell,
    /// the same shape `tsc_cache`/`testrunner_cache` already use for a single
    /// (not per-key) value.
    async fn import_graph(&self, pkg: &PkgBuf) -> anyhow::Result<Arc<importgraph::ImportGraph>> {
        let cell = {
            let mut cache = self.graph_cache.lock().await;
            Arc::clone(
                cache
                    .entry(pkg.clone())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };

        let workspace_root = self.workspace_root.clone();
        let walker = Arc::clone(&self.walker);
        let pkg_str = pkg.as_str().to_string();
        #[cfg(test)]
        let build_count = Arc::clone(&self.graph_build_count);

        let graph = cell
            .get_or_try_init(|| async move {
                hcore::blocking::run(move || -> anyhow::Result<Arc<importgraph::ImportGraph>> {
                    #[cfg(test)]
                    build_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                    let pkg_dir = if pkg_str.is_empty() {
                        workspace_root.clone()
                    } else {
                        workspace_root.join(&pkg_str)
                    };
                    let tsconfig = importgraph::find_nearest_tsconfig(&workspace_root, &pkg_dir);
                    let import_resolvers = resolvers::Resolvers::new(tsconfig.as_deref());
                    let resolve_cache = importgraph::ResolveCache::new();
                    let graph = importgraph::build_package_import_graph(
                        &walker,
                        &workspace_root,
                        &pkg_str,
                        &import_resolvers,
                        &resolve_cache,
                        tsconfig.as_deref(),
                    )
                    .with_context(|| format!("building import graph for {pkg_str:?}"))?;
                    Ok(Arc::new(graph))
                })
                .await
            })
            .await?;
        Ok(Arc::clone(graph))
    }

    /// Whether the provider's `allow_scripts` option permits `name@version`
    /// (or a bare `name` allowlist entry) to run lifecycle scripts.
    fn scripts_allowed_for(&self, name: &str, version: &str) -> bool {
        self.allow_scripts
            .iter()
            .any(|entry| entry == name || entry == &lockfile::graph_key(name, version))
    }

    /// Resolve a package's own `package.json` dependencies/devDependencies
    /// into target-dep addrs (see `deps::resolve_package_deps`), returned as
    /// a `{group: [addr, …]}` config value ready to attach to a
    /// `js_package_info` `TargetSpec`'s `deps` field.
    ///
    /// **M2**: before returning, this also builds the package's real
    /// import graph (`importgraph::build_package_import_graph`, oxc-based —
    /// see that module and `resolvers.rs`) and cross-checks every resolved
    /// edge against the package's declared-dependency closure
    /// (`importgraph::transitive_declared_closure`, which folds in the
    /// direct `dependencies`/`devDependencies`/`peerDependencies`
    /// `declared_closure` returns *plus* anything transitively reachable
    /// through them per the lockfile — see that function's doc). M1's
    /// `package.json`-declaration path above is still what maps a specifier
    /// to an addr; this is the correctness check on top of it — a
    /// resolved-but-undeclared-and-unreachable import is a hermeticity
    /// violation and fails `Provider::get` loudly, per
    /// `ai-docs/js-plugin-plan.md`'s Hermeticity section. See
    /// `importgraph.rs` module docs for why an *unresolvable* specifier is
    /// deliberately not treated the same way.
    async fn deps_config(&self, pkg: &PkgBuf) -> anyhow::Result<Value> {
        let pkg_str = pkg.as_str().to_string();
        let pkg_dir = if pkg_str.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(&pkg_str)
        };
        let lockfile = self.lockfile(&pkg_dir).await?;
        let resolved_graph = self.resolved_graph_for_lockfile(&lockfile).await?;
        // See `Provider::lockfile_relative_pkg`'s doc: `lockfile`/`resolved_graph`
        // are workspace-relative-`pkg`-agnostic — they're keyed by whichever
        // ancestor directory actually has a lockfile file, which may not be
        // `self.workspace_root` at all in a repo with more than one
        // independent npm/pnpm project. Every `Lockfile`-touching call below
        // needs `pkg` translated to be relative to *that* root instead.
        let lockfile_pkg = lockfile
            .as_ref()
            .map(|(root, _)| self.lockfile_relative_pkg(root, &pkg_str))
            .unwrap_or_else(|| pkg_str.clone());
        let lockfile = lockfile.map(|(_, lf)| lf);
        let graph = self.import_graph(pkg).await?;
        let member_addrs_by_name = self.member_addrs_by_name().await?;
        let workspace_root = self.workspace_root.clone();
        let os = platform::current_os();
        let arch = platform::current_arch();

        hcore::blocking::run(move || -> anyhow::Result<Value> {
            let package_json_path = workspace_root.join(&pkg_str).join(PACKAGE_JSON);
            let manifest = package_json::read_package_manifest(&package_json_path)
                .with_context(|| format!("reading dependencies of {pkg_str:?}"))?;

            let resolved = deps::resolve_package_deps(
                &pkg_str,
                &lockfile_pkg,
                &manifest,
                lockfile.as_deref(),
                resolved_graph.as_deref(),
                &member_addrs_by_name,
                &os,
                &arch,
            )?;

            // M2: cross-validate the declared-dependency wiring above against
            // the package's real import graph — see this method's doc
            // comment and `importgraph.rs` module docs. `graph` was already
            // built (and cached) by `Provider::import_graph` before this
            // closure was spawned.
            let declared_closure = importgraph::transitive_declared_closure(
                &manifest,
                &lockfile_pkg,
                lockfile.as_deref(),
                resolved_graph.as_deref(),
                &os,
                &arch,
            )?;
            importgraph::check_phantom_dependencies(
                &workspace_root,
                &pkg_str,
                &graph,
                &declared_closure,
            )
            .with_context(|| {
                format!(
                    "cross-checking {pkg_str:?}'s import graph against its declared dependencies"
                )
            })?;

            let mut groups: HashMap<String, Vec<Value>> = HashMap::new();
            for dep in resolved {
                groups
                    .entry(dep.group.to_string())
                    .or_default()
                    .push(Value::String(dep.addr));
            }
            Ok(Value::Map(
                groups
                    .into_iter()
                    .map(|(k, v)| (k, Value::List(v)))
                    .collect(),
            ))
        })
        .await
    }

    /// Build the config for a `js_typecheck` target: `typecheck_deps_config`'s
    /// pure, tsc-free Input-scoping (see that function's doc) plus the host
    /// toolchain resolution/version-query this milestone's disclosed
    /// `tstool = "host"` escape hatch requires — see `toolchain.rs` module
    /// docs for why the version is queried here (spec-resolution time) and
    /// not deferred to the driver's `run()`.
    async fn typecheck_config(&self, pkg: &PkgBuf) -> anyhow::Result<HashMap<String, Value>> {
        // Read straight out of the cached `Arc`'s fields rather than cloning
        // the whole tuple: `tsc_bin` only ever ends up re-stringified via
        // `to_string_lossy().into_owned()` below, so cloning the `PathBuf`
        // first (via `.as_ref().clone()`) would allocate a copy that's
        // immediately discarded.
        let resolved_tsc = self.resolved_host_tsc().await?;
        let tsc_bin = resolved_tsc.0.to_string_lossy().into_owned();
        let tsc_version = resolved_tsc.1.clone();
        let pkg_str = pkg.as_str().to_string();
        let pkg_dir = if pkg_str.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(&pkg_str)
        };
        let lockfile = self.lockfile(&pkg_dir).await?;
        let resolved_graph = self.resolved_graph_for_lockfile(&lockfile).await?;
        let lockfile_pkg = lockfile
            .as_ref()
            .map(|(root, _)| self.lockfile_relative_pkg(root, &pkg_str))
            .unwrap_or_else(|| pkg_str.clone());
        let lockfile = lockfile.map(|(_, lf)| lf);
        let graph = self.import_graph(pkg).await?;
        let member_addrs_by_name = self.member_addrs_by_name().await?;
        let workspace_root = self.workspace_root.clone();
        let walker = Arc::clone(&self.walker);
        let os = platform::current_os();
        let arch = platform::current_arch();

        hcore::blocking::run(move || -> anyhow::Result<HashMap<String, Value>> {
            let (deps, tsconfig_path, tsconfig_content) = typecheck_deps_config(
                &walker,
                &workspace_root,
                &pkg_str,
                &lockfile_pkg,
                &graph,
                lockfile.as_deref(),
                resolved_graph.as_deref(),
                &member_addrs_by_name,
                &os,
                &arch,
            )
            .with_context(|| format!("building js_typecheck inputs for {pkg_str:?}"))?;

            let mut config: HashMap<String, Value> = HashMap::new();
            config.insert("tsc_bin".to_string(), Value::String(tsc_bin));
            config.insert("tsc_version".to_string(), Value::String(tsc_version));
            config.insert("tsconfig_path".to_string(), Value::String(tsconfig_path));
            config.insert(
                "tsconfig_content".to_string(),
                Value::String(tsconfig_content),
            );
            config.insert("deps".to_string(), Value::Map(deps));
            Ok(config)
        })
        .await
    }

    /// Discover this package's own `js_test` test files (see
    /// `importgraph::discover_test_files`), matched against the provider's
    /// configured `test_glob`. Workspace-root-relative paths, sorted.
    async fn discover_test_files(&self, pkg: &PkgBuf) -> anyhow::Result<Vec<String>> {
        let workspace_root = self.workspace_root.clone();
        let walker = Arc::clone(&self.walker);
        let pkg_str = pkg.as_str().to_string();
        let test_glob = self.test_glob.clone();
        hcore::blocking::run(move || -> anyhow::Result<Vec<String>> {
            importgraph::discover_test_files(&walker, &workspace_root, &pkg_str, &test_glob)
        })
        .await
        .with_context(|| format!("discovering js_test targets for {}", pkg.as_str()))
    }

    /// Build the config for one `js_test` target (one test file): the host
    /// runner toolchain resolution/version-query this milestone's disclosed
    /// `testrunner`-is-host-resolved escape hatch requires (see
    /// `toolchain.rs` module docs for why the version is queried here, at
    /// spec-resolution time, rather than deferred to the driver's `run()`)
    /// plus `test_deps_config`'s pure, runner-free Input-scoping.
    async fn test_config(
        &self,
        pkg: &PkgBuf,
        test_file_rel: &str,
    ) -> anyhow::Result<HashMap<String, Value>> {
        // See `typecheck_config`'s identical fix: read the cached `Arc`'s
        // fields directly instead of cloning the whole tuple, since
        // `runner_bin` only ever ends up re-stringified below.
        let resolved_test_runner = self.resolved_host_test_runner().await?;
        let runner_bin = resolved_test_runner.0.to_string_lossy().into_owned();
        let runner_version = resolved_test_runner.1.clone();
        let pkg_str = pkg.as_str().to_string();
        let pkg_dir = if pkg_str.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(&pkg_str)
        };
        let lockfile = self.lockfile(&pkg_dir).await?;
        let resolved_graph = self.resolved_graph_for_lockfile(&lockfile).await?;
        let lockfile_pkg = lockfile
            .as_ref()
            .map(|(root, _)| self.lockfile_relative_pkg(root, &pkg_str))
            .unwrap_or_else(|| pkg_str.clone());
        let lockfile = lockfile.map(|(_, lf)| lf);
        let graph = self.import_graph(pkg).await?;
        let member_addrs_by_name = self.member_addrs_by_name().await?;
        let workspace_root = self.workspace_root.clone();
        let testrunner = self.testrunner.clone();
        let test_file_rel = test_file_rel.to_string();
        let os = platform::current_os();
        let arch = platform::current_arch();

        hcore::blocking::run(move || -> anyhow::Result<HashMap<String, Value>> {
            let (deps, runner_config_path, runner_config_content) = test_deps_config(
                &workspace_root,
                &pkg_str,
                &lockfile_pkg,
                &test_file_rel,
                &graph,
                lockfile.as_deref(),
                resolved_graph.as_deref(),
                &member_addrs_by_name,
                &os,
                &arch,
                &testrunner,
                runner_config_candidates(&testrunner)?,
            )
            .with_context(|| {
                format!("building js_test inputs for {pkg_str:?} test file {test_file_rel:?}")
            })?;

            let mut config: HashMap<String, Value> = HashMap::new();
            config.insert("testrunner".to_string(), Value::String(testrunner));
            config.insert("runner_bin".to_string(), Value::String(runner_bin));
            config.insert("runner_version".to_string(), Value::String(runner_version));
            config.insert("test_file".to_string(), Value::String(test_file_rel));
            config.insert(
                "runner_config_path".to_string(),
                Value::String(runner_config_path),
            );
            config.insert(
                "runner_config_content".to_string(),
                Value::String(runner_config_content),
            );
            config.insert("deps".to_string(), Value::Map(deps));
            Ok(config)
        })
        .await
    }

    /// Build the `js_install` `TargetSpec` for a third-party
    /// `@heph/js/thirdparty/<name>@<version>` addr, resolving `(name,
    /// version)` against the lockfile's [`ResolvedGraph`] for its
    /// integrity/tarball-URL/platform-restriction/install-script metadata.
    async fn thirdparty_install_spec(&self, addr: &Addr) -> anyhow::Result<TargetSpec> {
        let (name, version) =
            thirdparty::parse_thirdparty_pkg(addr.package.as_str()).ok_or_else(|| {
                anyhow::anyhow!("not a js thirdparty addr: {}", addr.package.as_str())
            })?;
        let os = addr
            .args
            .get("os")
            .cloned()
            .unwrap_or_else(platform::current_os);
        let arch = addr
            .args
            .get("arch")
            .cloned()
            .unwrap_or_else(platform::current_arch);

        let graph = self
            .find_resolved_graph_for(name, version)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "js provider: {name}@{version} not found in any {} discovered so far in this \
                 workspace — cannot resolve this third-party package. This addr has no \
                 originating package to search from directly; it relies on some other \
                 target's own dependency resolution having already discovered the relevant \
                 lockfile (see `Provider::find_resolved_graph_for`'s doc) — if this js_install \
                 target was requested on its own, without anything actually depending on it, \
                 that discovery never happened",
                    Lockfile::filename(self.pkgmanager)
                )
            })?;
        let resolved = graph.get(name, version).ok_or_else(|| {
            anyhow::anyhow!(
                "js provider: {name}@{version} not found in the lockfile — is it stale?"
            )
        })?;

        anyhow::ensure!(
            platform::matches_platform(&resolved.os, &resolved.cpu, &os, &arch),
            "js provider: {name}@{version} is restricted to os={:?} cpu={:?}, which does not \
             include the requested platform {os}/{arch}",
            resolved.os,
            resolved.cpu
        );

        // Empty `integrity` this deep (after `find_resolved_graph_for`
        // already searched every lockfile discovered in the workspace for a
        // populated entry) means the *lockfile itself* never recorded one
        // for this package anywhere — a real, observed npm shape, not a
        // heph bug: `npm install` (unlike `npm ci`) can satisfy a package
        // from its local cache and strip `resolved`/`integrity` from an
        // existing lockfile entry instead of re-populating them (a
        // long-standing npm CLI bug — npm/cli#4263, #4460, #6301). Rather
        // than block the install on a lockfile heph didn't write and can't
        // fix, `js_install` proceeds unverified in that case — see
        // `driver_install::fetch_and_extract`'s doc for the tracing
        // breadcrumb this leaves.
        let resolved_url = resolved
            .resolved
            .clone()
            .unwrap_or_else(|| default_registry_url(name, version));
        let scripts_allowed = self.scripts_allowed_for(name, version);

        // Only resolved when a lifecycle script will actually run — the
        // overwhelming majority of `js_install` targets have no script at
        // all, and this must cost them nothing. A package's own postinstall
        // routinely needs its sibling `dependencies`/`optionalDependencies`
        // materialized as real `node_modules/<name>` entries next to it
        // (the flagship shape: a small loader package `require.resolve()`s
        // a platform-specific `optionalDependencies` sibling for its native
        // binary — esbuild, sharp, @swc/core, and many more all follow this
        // exact pattern) — confirmed live, esbuild's own postinstall fails
        // outright without this. See `thirdparty::node_modules_addr`'s doc:
        // the *same* relocation mechanism already used for a consumer's own
        // `node_modules`, reused here with an empty `consuming_pkg` so the
        // relocated entries land at bare `node_modules/<name>` — the
        // driver's own sandbox workspace root is already an ancestor of its
        // package dir, so no separate unpack-root redirection or `cwd`
        // change is needed for Node's own ancestor `node_modules` walk to
        // find them (`driver_install.rs`'s `run()` only adds a self-
        // reference symlink on top of this, for a package that
        // `require()`s its own name — see that file's doc).
        let mut deps: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        if resolved.has_install_script && scripts_allowed {
            // Required: a *platform-mismatched* entry is a real, actionable
            // problem — never silently drop a sibling this package's own
            // postinstall may need — mirrors `deps::resolve_one_dependency`'s
            // established asymmetry between `dependencies` (hard error) and
            // `optionalDependencies` (silent skip) exactly. An entirely
            // *unresolvable* required dependency (no lockfile entry
            // anywhere) can't be distinguished here at all: `resolve_npm_edges`
            // (which built `resolved.dependencies` from this exact `graph`)
            // already silently drops any edge it can't resolve — deliberately,
            // for every consumer of this field, not only this one.
            //
            // A graph_key present in a package's `dependencies` but absent
            // from this same `graph` is *not* provably impossible, unlike an
            // earlier draft of this comment claimed: an `npm:`-aliased
            // dependency (`"foo": "npm:bar@1.2.3"`, `deps.rs`'s own doc
            // explains why `local`/`resolved` names diverge for these)
            // computes its edge keyed by the *alias* (`resolve_npm_edges`
            // keys by the declared name), while `resolved_graph()`'s own
            // top-level loop keys the same package by its *real* name —
            // the two can disagree. A hard, diagnosable error here (not a
            // panic) matches every other lockfile-inconsistency this file
            // reports (see `entries_agree_where_comparable`'s own "the
            // lockfile itself is inconsistent" framing) — this specific
            // shape (an npm-aliased required dependency on a script-bearing
            // package) just isn't resolvable by this milestone's
            // edge-tracking yet, which the message says plainly rather
            // than crashing the whole process over it.
            //
            // Not just the script package's *direct* dependencies: a
            // lifecycle script routinely `require()`s a package two or more
            // hops away in the graph (confirmed live: `@sentry/cli`'s
            // postinstall requires `which`, and `which` itself requires
            // `isexe` — a dependency of a dependency, never a direct edge of
            // `@sentry/cli` at all). Real npm/pnpm hoist the whole
            // transitive closure into one flat `node_modules` so any
            // ancestor-walk `require()` finds it; this walks the resolved
            // graph the same way, breadth over the queue below, so every
            // reachable `dependencies`/`optionalDependencies` edge — at any
            // depth — gets its own sibling `node_modules/<name>` entry.
            // `seen_names` both dedupes (a diamond dependency is not fetched
            // or declared as an `Input` twice) and bounds the walk (a
            // circular edge back to an already-visited name is simply not
            // re-queued, so a dependency cycle in the graph can't loop this
            // forever).
            let mut seen_names: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            let mut frontier: Vec<&lockfile::ResolvedPackage> = vec![resolved];
            while let Some(pkg) = frontier.pop() {
                for (dep_name, dep_key) in &pkg.dependencies {
                    if seen_names.contains(dep_name) {
                        continue;
                    }
                    let dep_pkg = graph.packages.get(dep_key).ok_or_else(|| {
                        anyhow::anyhow!(
                            "js provider: {}@{} declares a required dependency on `{dep_name}` \
                             ({dep_key}), but that exact package/version has no resolved entry \
                             in the lockfile — if `{dep_name}` is an npm alias \
                             (`\"{dep_name}\": \"npm:...\"`), this milestone's sibling-dependency \
                             resolution for a lifecycle script doesn't yet follow aliases; \
                             otherwise the lockfile is likely stale — re-run the package \
                             manager's install to regenerate it",
                            pkg.name,
                            pkg.version
                        )
                    })?;
                    // A circular edge back to the script-owning package
                    // itself: already reachable via `driver_install.rs`'s
                    // own self-reference symlink, and must never become a
                    // second `Input` — that would be this exact `js_install`
                    // target depending on itself.
                    if dep_pkg.name == name && dep_pkg.version == version {
                        continue;
                    }
                    anyhow::ensure!(
                        platform::matches_platform(&dep_pkg.os, &dep_pkg.cpu, &os, &arch),
                        "js provider: {}@{}'s required dependency `{dep_name}` ({dep_key}) is \
                         restricted to os={:?} cpu={:?}, which does not include the requested \
                         platform {os}/{arch} — {name}@{version}'s lifecycle script cannot run \
                         without it",
                        pkg.name,
                        pkg.version,
                        dep_pkg.os,
                        dep_pkg.cpu
                    );
                    seen_names.insert(dep_name.clone());
                    deps.push(
                        thirdparty::node_modules_addr(
                            "",
                            dep_name,
                            &dep_pkg.name,
                            &dep_pkg.version,
                            &os,
                            &arch,
                        )
                        .format(),
                    );
                    frontier.push(dep_pkg);
                }

                // Optional: most entries here are for *other* platforms and
                // never apply — an unresolvable or platform-mismatched one
                // is expected, silently skipped, and never wired as a
                // dependency edge (so it can never itself be the cause of a
                // graph cycle or a hard resolution failure, and its own
                // subtree is never walked) — but the reason is recorded so
                // a lifecycle script that *does* fail for lack of it names
                // what was missing instead of an opaque `Cannot find
                // module`.
                for (dep_name, dep_key) in &pkg.optional_dependencies {
                    if seen_names.contains(dep_name) {
                        continue;
                    }
                    let Some(dep_pkg) = graph.packages.get(dep_key) else {
                        skipped.push(format!("{dep_name} ({dep_key}: not in the lockfile)"));
                        continue;
                    };
                    if dep_pkg.name == name && dep_pkg.version == version {
                        continue;
                    }
                    if !platform::matches_platform(&dep_pkg.os, &dep_pkg.cpu, &os, &arch) {
                        skipped.push(format!(
                            "{dep_name} ({dep_key}: restricted to os={:?} cpu={:?}, current \
                             platform is {os}/{arch})",
                            dep_pkg.os, dep_pkg.cpu
                        ));
                        continue;
                    }
                    seen_names.insert(dep_name.clone());
                    deps.push(
                        thirdparty::node_modules_addr(
                            "",
                            dep_name,
                            &dep_pkg.name,
                            &dep_pkg.version,
                            &os,
                            &arch,
                        )
                        .format(),
                    );
                    frontier.push(dep_pkg);
                }
            }
        }

        let mut config: HashMap<String, Value> = HashMap::new();
        config.insert("name".to_string(), Value::String(name.to_string()));
        config.insert("version".to_string(), Value::String(version.to_string()));
        config.insert(
            "integrity".to_string(),
            Value::String(resolved.integrity.clone()),
        );
        config.insert("resolved".to_string(), Value::String(resolved_url));
        config.insert("os".to_string(), Value::String(os));
        config.insert("arch".to_string(), Value::String(arch));
        config.insert(
            "has_install_script".to_string(),
            Value::Bool(resolved.has_install_script),
        );
        config.insert("scripts_allowed".to_string(), Value::Bool(scripts_allowed));
        config.insert(
            "deps".to_string(),
            Value::List(deps.into_iter().map(Value::String).collect()),
        );
        config.insert(
            "skipped_deps".to_string(),
            Value::List(skipped.into_iter().map(Value::String).collect()),
        );

        Ok(TargetSpec {
            addr: addr.clone(),
            driver: "js_install".to_string(),
            config,
            labels: vec![],
            transitive: Default::default(),
            approval: Default::default(),
        })
    }

    /// The `TargetSpec` for a [`thirdparty::node_modules_addr`] — a builtin
    /// `group` target that relocates the underlying `js_install` download
    /// (`Content::DirPath(thirdparty_pkg(name, version))`, see
    /// `driver_install.rs`) to `<consuming_pkg>/node_modules/<local_name>`,
    /// the only path Node's own module resolution ever looks at. Reuses the
    /// `group` driver (`crates/builtins/src/plugingroup`) rather than
    /// inventing a relocation mechanism — see that driver's module doc for
    /// why a path-transform view is zero-copy and already folds into a
    /// consumer's cache key via its own def hash.
    ///
    /// Synchronous and infallible-by-construction: unlike
    /// `thirdparty_install_spec`, this never touches a lockfile — it only
    /// re-derives the *addr* of the `js_install` target it relocates, which
    /// `Provider::get` resolves independently (and lazily) when the engine
    /// actually walks this target's own `deps`.
    fn node_modules_group_spec(
        &self,
        addr: &Addr,
        r: &thirdparty::NodeModulesRelocation,
    ) -> TargetSpec {
        let install_addr =
            thirdparty::thirdparty_addr(&r.resolved_name, &r.version, &r.os, &r.arch);
        // A depth-1 diamond-dependency override (`r.nested_under`, see
        // `lockfile::TransitiveEntry`'s doc) nests one extra
        // `<parent>/node_modules/` hop inside the consuming package's own
        // `node_modules` — exactly where npm's own nested override would
        // put it, and exactly where Node's own ancestor `node_modules` walk
        // from inside the parent's own placement looks first.
        let node_modules_root = if r.consuming_pkg.is_empty() {
            "node_modules".to_string()
        } else {
            format!("{}/node_modules", r.consuming_pkg)
        };
        let node_modules_dir = match &r.nested_under {
            None => format!("{node_modules_root}/{}", r.local_name),
            Some(parent) => format!("{node_modules_root}/{parent}/node_modules/{}", r.local_name),
        };

        let mut config: HashMap<String, Value> = HashMap::new();
        config.insert(
            "deps".to_string(),
            Value::List(vec![Value::String(install_addr.format())]),
        );
        config.insert(
            "strip_prefix".to_string(),
            Value::String(
                thirdparty::thirdparty_pkg(&r.resolved_name, &r.version)
                    .as_str()
                    .to_string(),
            ),
        );
        config.insert("prefix".to_string(), Value::String(node_modules_dir));

        TargetSpec {
            addr: addr.clone(),
            driver: thirdparty::NODE_MODULES_TARGET.to_string(),
            config,
            labels: vec![],
            transitive: Default::default(),
            approval: Default::default(),
        }
    }

    /// The `TargetSpec` for [`crate::pluginjs::NODE_MODULES_SYNC_TARGET`] — an
    /// aggregating `group` over every third-party dependency `pkg` resolves
    /// (direct and transitive), each already relocated by
    /// [`Provider::node_modules_group_spec`] to its own
    /// `<pkg>/node_modules/<name>`. `include = ["**"]` is enough by itself to
    /// make this a *relocating* (non-transparent, has-its-own-artifact)
    /// group — see `plugingroup` module docs — without actually renaming a
    /// single path, since every dep already landed at its correct final
    /// destination; this target exists purely to give that already-correct
    /// content one address `codegen = "copy"` can materialize.
    ///
    /// **Why this needs its own target at all**, rather than just tagging
    /// each per-dependency `group` with `codegen`: `heph`'s codegen
    /// write-back only fires for the *top-level requested* target (a
    /// compatibility review's finding on this design) — every one of this
    /// package's individual relocated-dependency addrs is normally only
    /// ever reached as a `js_test`/`js_typecheck`/`js_bundle` *input*, never
    /// requested directly, so `codegen` on those addrs alone would never
    /// fire. `heph run //pkg:node_modules` makes this the top-level target.
    async fn node_modules_sync_spec(&self, pkg: &PkgBuf) -> anyhow::Result<TargetSpec> {
        let addr = Addr::new(
            pkg.clone(),
            NODE_MODULES_SYNC_TARGET.to_string(),
            Default::default(),
        );
        let pkg_str = pkg.as_str().to_string();
        let pkg_dir = if pkg_str.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(&pkg_str)
        };
        let lockfile = self.lockfile(&pkg_dir).await?;
        let resolved_graph = self.resolved_graph_for_lockfile(&lockfile).await?;
        let lockfile_pkg = lockfile
            .as_ref()
            .map(|(root, _)| self.lockfile_relative_pkg(root, &pkg_str))
            .unwrap_or_else(|| pkg_str.clone());
        let lockfile = lockfile.map(|(_, lf)| lf);
        let member_addrs_by_name = self.member_addrs_by_name().await?;
        let workspace_root = self.workspace_root.clone();
        let os = platform::current_os();
        let arch = platform::current_arch();

        let deps_addrs = hcore::blocking::run(move || -> anyhow::Result<Vec<String>> {
            let package_json_path = workspace_root.join(&pkg_str).join(PACKAGE_JSON);
            let manifest = package_json::read_package_manifest(&package_json_path)
                .with_context(|| format!("reading dependencies of {pkg_str:?}"))?;

            // Direct deps, filtered to third-party ones only — a workspace-
            // sibling `ResolvedDep` addresses that sibling's own
            // `package_info` target, not a relocated `js_install` download,
            // and has no place in a `node_modules` sync.
            let mut addrs: Vec<String> = deps::resolve_package_deps(
                &pkg_str,
                &lockfile_pkg,
                &manifest,
                lockfile.as_deref(),
                resolved_graph.as_deref(),
                &member_addrs_by_name,
                &os,
                &arch,
            )?
            .into_iter()
            .map(|d| d.addr)
            .filter(|addr| addr.contains(thirdparty::NODE_MODULES_PKG))
            .collect();

            if let (Some(lf), Some(rg)) = (lockfile.as_deref(), resolved_graph.as_deref()) {
                addrs.extend(deps::resolve_transitive_closure(
                    &pkg_str,
                    &lockfile_pkg,
                    &manifest,
                    lf,
                    rg,
                    &os,
                    &arch,
                )?);
            }
            addrs.sort();
            addrs.dedup();
            Ok(addrs)
        })
        .await?;

        let mut config: HashMap<String, Value> = HashMap::new();
        config.insert(
            "deps".to_string(),
            Value::List(deps_addrs.into_iter().map(Value::String).collect()),
        );
        config.insert(
            "include".to_string(),
            Value::List(vec![Value::String("**".to_string())]),
        );
        config.insert("codegen".to_string(), Value::String("copy".to_string()));

        Ok(TargetSpec {
            addr,
            driver: thirdparty::NODE_MODULES_TARGET.to_string(),
            config,
            labels: vec![],
            transitive: Default::default(),
            approval: Default::default(),
        })
    }

    /// The workspace-relative path to a package's default `js_bundle` entry
    /// point — its own `package.json` `"main"` field, resolved against the
    /// package directory and checked to actually exist. `None` when the
    /// package has no `"main"` field, `"main"` fails the same escape/
    /// containment checks a user-supplied `entry=` addr arg gets (see module
    /// docs' bug-class (c) note), or `"main"` names a file that doesn't
    /// exist — any of these simply means there is no usable default, so
    /// `Provider::list` lists no `js_bundle` target for this package
    /// (mirrors `js_test`'s "no matched files, no listed target" shape); an
    /// explicit `entry=` addr arg still works via `Provider::get` regardless.
    async fn default_entry_for_package(&self, pkg: &PkgBuf) -> anyhow::Result<Option<String>> {
        let workspace_root = self.workspace_root.clone();
        let pkg_str = pkg.as_str().to_string();
        hcore::blocking::run(move || -> anyhow::Result<Option<String>> {
            let pkg_dir = if pkg_str.is_empty() {
                workspace_root.clone()
            } else {
                workspace_root.join(&pkg_str)
            };
            let manifest = package_json::read_package_manifest(&pkg_dir.join(PACKAGE_JSON))
                .with_context(|| {
                    format!("reading {pkg_str:?}'s package.json for its default js_bundle entry")
                })?;
            let Some(main) = manifest.main else {
                return Ok(None);
            };
            let main_rel = main.trim_start_matches("./");
            let entry_rel = if pkg_str.is_empty() {
                main_rel.to_string()
            } else {
                format!("{pkg_str}/{main_rel}")
            };
            // `package.json`'s own `"main"` is first-party content this
            // package fully controls — trusted the same way this crate
            // already trusts tsconfig/runner-config content elsewhere — but
            // still run through the same escape/containment checks a
            // user-supplied `entry=` addr arg gets (bug class (c) in this
            // milestone's task): a malformed `"main"` (e.g.
            // `"../../../etc/passwd"`) must not become a default entry.
            if reject_path_escape("package.json main", &entry_rel).is_err()
                || !path_under_package(&pkg_str, &entry_rel)
            {
                return Ok(None);
            }
            let entry_abs = workspace_root.join(&entry_rel);
            if !entry_abs.is_file() {
                return Ok(None);
            }
            Ok(Some(entry_rel))
        })
        .await
    }

    /// The full transitive first-party closure reachable from
    /// `entry_pkg`'s `entry_file_rel` via `ImportGraph::runtime_edges`,
    /// recursing across workspace-package boundaries, plus every
    /// third-party package the closure's own unresolved bare specifiers (or
    /// ambiently-resolved `node_modules` edges) name — see
    /// `driver_bundle.rs` module docs' "Inputs / cache key" section for why
    /// this cannot reuse `importgraph::build_test_closure`'s one-hop-external
    /// trim. `entry_file_rel` is workspace-relative and must already be
    /// validated (`reject_path_escape` + `path_under_package`) by the
    /// caller.
    ///
    /// Memoized per `(entry_pkg, entry_file_rel)` on the `Provider` — see
    /// [`Provider::bundle_closure_cache`]'s doc — since the BFS below is
    /// provably invariant across `js_bundle`'s `format`/`target` variant
    /// axis for the same entry point; callers requesting more than one
    /// variant of the same package share one BFS.
    async fn bundle_closure(
        &self,
        entry_pkg: &str,
        entry_file_rel: &str,
    ) -> anyhow::Result<Arc<BundleClosureResult>> {
        let key = (entry_pkg.to_string(), entry_file_rel.to_string());
        let cell = {
            let mut cache = self.bundle_closure_cache.lock().await;
            Arc::clone(
                cache
                    .entry(key)
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        let result = cell
            .get_or_try_init(|| self.bundle_closure_uncached(entry_pkg, entry_file_rel))
            .await?;
        Ok(Arc::clone(result))
    }

    /// The actual BFS behind [`Provider::bundle_closure`] — split out so the
    /// memoizing wrapper stays a thin, easy-to-audit cache lookup. Returns
    /// `(first_party_files, third_party_addrs, third_party_names)` bundled
    /// as a [`BundleClosureResult`]: `third_party_names` (the bare specifier
    /// each third-party edge was actually imported by, e.g. `"lodash"`) is
    /// what `Provider::bundle_config` feeds esbuild's own `--external:<name>`
    /// flag — see that field's doc and the feature-quality M6 review BLOCKER
    /// it fixes (the discovered closure and the bundler's `--external` flags
    /// were previously two disconnected sources, so every real third-party
    /// import made `esbuild --bundle` hard-fail with "Could not resolve").
    ///
    /// **Known perf trim, disclosed rather than silent**: each dequeued file
    /// (and each newly-visited package's manifest read + phantom check)
    /// gets its own `hcore::blocking::run` round trip rather than batching a
    /// whole package's worth of files into one blocking call — correct, but
    /// more blocking-pool hops than necessary for a package with many
    /// closure-reached files. `Provider::import_graph`'s own per-package
    /// memoization means no *parse* work is repeated, so this is scheduling
    /// overhead, not redundant computation — acceptable for this milestone,
    /// a real target for batching if profiling ever names it a bottleneck.
    async fn bundle_closure_uncached(
        &self,
        entry_pkg: &str,
        entry_file_rel: &str,
    ) -> anyhow::Result<Arc<BundleClosureResult>> {
        let workspace_root = self.workspace_root.clone();
        let canonical_root = hcore::blocking::run(enclose!((workspace_root) move || {
            workspace_root
                .canonicalize()
                .with_context(|| format!("canonicalize workspace root {workspace_root:?}"))
        }))
        .await?;

        let member_addrs_by_name = self.member_addrs_by_name().await?;
        let os = platform::current_os();
        let arch = platform::current_arch();

        let mut files: BTreeSet<String> = BTreeSet::new();
        let mut external_addrs: BTreeSet<String> = BTreeSet::new();
        let mut external_names: BTreeSet<String> = BTreeSet::new();
        let mut visited_pkgs: HashSet<String> = HashSet::new();
        let mut manifests: HashMap<String, package_json::PackageManifest> = HashMap::new();
        // Keyed alongside `manifests`, computed once per package rather than
        // once per file — the ancestor walk in `Provider::lockfile` isn't
        // free even though its parse result is cached per root, and a
        // package can own many files in this closure.
        let mut lockfile_info: PkgLockfileInfo = HashMap::new();
        let mut queue: VecDeque<(String, String)> = VecDeque::new();
        files.insert(entry_file_rel.to_string());
        queue.push_back((entry_pkg.to_string(), entry_file_rel.to_string()));

        while let Some((cur_pkg, cur_file)) = queue.pop_front() {
            let graph = self.import_graph(&PkgBuf::from(cur_pkg.clone())).await?;

            if !manifests.contains_key(&cur_pkg) {
                let manifest = hcore::blocking::run(enclose!(
                    (workspace_root, cur_pkg) move || -> anyhow::Result<package_json::PackageManifest> {
                        let pkg_dir = if cur_pkg.is_empty() {
                            workspace_root.clone()
                        } else {
                            workspace_root.join(&cur_pkg)
                        };
                        package_json::read_package_manifest(&pkg_dir.join(PACKAGE_JSON))
                            .with_context(|| {
                                format!("reading {cur_pkg:?}'s package.json for js_bundle")
                            })
                    }
                ))
                .await?;
                manifests.insert(cur_pkg.clone(), manifest);

                // Per-`cur_pkg`, computed exactly once here (not per file):
                // this closure walk crosses package boundaries (a
                // `js_bundle` entry can pull in a sibling package's own
                // first-party files), and different packages in one heph
                // workspace may belong to different, independent npm/pnpm
                // projects with different lockfiles — see
                // `Provider::lockfile`'s doc.
                let cur_pkg_dir = if cur_pkg.is_empty() {
                    workspace_root.clone()
                } else {
                    workspace_root.join(&cur_pkg)
                };
                let lockfile = self.lockfile(&cur_pkg_dir).await?;
                let resolved_graph = self.resolved_graph_for_lockfile(&lockfile).await?;
                let lockfile_pkg = lockfile
                    .as_ref()
                    .map(|(root, _)| self.lockfile_relative_pkg(root, &cur_pkg))
                    .unwrap_or_else(|| cur_pkg.clone());
                let lockfile = lockfile.map(|(_, lf)| lf);
                lockfile_info.insert(cur_pkg.clone(), (lockfile_pkg, lockfile, resolved_graph));
            }
            let manifest = manifests
                .get(&cur_pkg)
                .expect("just inserted above")
                .clone();
            let (lockfile_pkg, lockfile, resolved_graph) = lockfile_info
                .get(&cur_pkg)
                .cloned()
                .expect("just inserted above");

            // Defense in depth, mirroring `deps_config`/`typecheck_deps_config`'s
            // identical phantom-dependency cross-check — each package this
            // closure reaches gets checked exactly once, even though its own
            // `js_package_info`/`js_typecheck` targets (if requested
            // separately) already check it too; a `js_bundle`-only workflow
            // that never requests those targets for a *sibling* package must
            // not silently skip this.
            if visited_pkgs.insert(cur_pkg.clone()) {
                hcore::blocking::run(enclose!(
                    (workspace_root, cur_pkg, lockfile_pkg, graph, manifest, lockfile, resolved_graph, os, arch) move || -> anyhow::Result<()> {
                        let declared = importgraph::transitive_declared_closure(
                            &manifest,
                            &lockfile_pkg,
                            lockfile.as_deref(),
                            resolved_graph.as_deref(),
                            &os,
                            &arch,
                        )?;
                        importgraph::check_phantom_dependencies(
                            &workspace_root,
                            &cur_pkg,
                            &graph,
                            &declared,
                        )
                    }
                ))
                .await
                .with_context(|| {
                    format!(
                        "cross-checking {cur_pkg:?}'s import graph against its declared \
                         dependencies for js_bundle"
                    )
                })?;
            }

            let step = hcore::blocking::run(enclose!(
                (canonical_root, cur_pkg, lockfile_pkg, cur_file, graph, manifest, lockfile, resolved_graph,
                 member_addrs_by_name, os, arch) move || {
                    bundle_closure_step(
                        &canonical_root,
                        &cur_pkg,
                        &lockfile_pkg,
                        &cur_file,
                        &graph,
                        &manifest,
                        lockfile.as_deref(),
                        resolved_graph.as_deref(),
                        &member_addrs_by_name,
                        &os,
                        &arch,
                    )
                }
            ))
            .await
            .with_context(|| {
                format!("walking js_bundle closure edges for {cur_pkg:?}'s {cur_file:?}")
            })?;

            for (name, addr) in step.new_external {
                external_names.insert(name);
                external_addrs.insert(addr);
            }
            for (owning_pkg, rel) in step.new_files {
                if files.insert(rel.clone()) {
                    queue.push_back((owning_pkg, rel));
                }
            }
        }

        Ok(Arc::new(BundleClosureResult {
            files,
            external_addrs,
            external_names,
        }))
    }

    /// Build everything about a `js_bundle` target's config *except* the
    /// bundler toolchain resolution/version-query: `bundle_closure`'s full
    /// transitive first-party/third-party Input scoping, the resolved
    /// bundler config file (if any) and anything it itself references, and
    /// the entry package's resolved tsconfig (if any) plus its `extends`
    /// chain. Deliberately split out from [`Provider::bundle_config`] so it
    /// never touches the host bundler binary — unit-testable **without** a
    /// real `esbuild` installed, mirroring `typecheck_deps_config`/
    /// `test_deps_config`/`lint_deps_config`'s identical split (this
    /// milestone's own "single most important test" precedent).
    async fn bundle_deps_config(
        &self,
        pkg: &PkgBuf,
        entry_file_rel: &str,
    ) -> anyhow::Result<BundleDepsConfig> {
        let closure = self.bundle_closure(pkg.as_str(), entry_file_rel).await?;

        let workspace_root = self.workspace_root.clone();
        let pkg_str = pkg.as_str().to_string();

        let ancillary =
            hcore::blocking::run(move || -> anyhow::Result<BundleAncillary> {
                let canonical_root = workspace_root
                    .canonicalize()
                    .with_context(|| format!("canonicalize workspace root {workspace_root:?}"))?;
                let pkg_dir = if pkg_str.is_empty() {
                    workspace_root.clone()
                } else {
                    workspace_root.join(&pkg_str)
                };

                // The bundler config file (`esbuild.config.json`), if any, plus
                // every file it itself references.
                let config_path = importgraph::find_nearest_bundler_config(
                    &workspace_root,
                    &pkg_dir,
                    &["esbuild.config.json"],
                );
                let (
                    bundler_config_path,
                    bundler_config_content,
                    bundler_config_refs,
                    config_external,
                ) = match &config_path {
                    Some(p) => {
                        let rel = p
                            .strip_prefix(&workspace_root)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .replace('\\', "/");
                        let content = std::fs::read_to_string(p)
                            .with_context(|| format!("reading bundler config {p:?}"))?;
                        let refs = importgraph::resolve_runner_config_referenced_files(p, &content)
                            .with_context(|| {
                                format!("resolving files referenced by bundler config {p:?}")
                            })?;
                        // `bare_specifiers` is deliberately unused here:
                        // `esbuild.config.json` is JSON, which
                        // `resolve_runner_config_referenced_files` (via
                        // `importparse::parse_file_imports`) never parses as
                        // a module in the first place, so this is always
                        // empty in practice — unlike `test_deps_config`'s
                        // real JS/TS runner configs, which do need it (see
                        // that call site).
                        //
                        // Hard error (never a silent same-path fallback) on
                        // workspace-root escape — the exact M5-review-fixed
                        // anti-pattern (`strip_prefix(...).unwrap_or(...)`
                        // quietly keeping a raw, possibly-absolute host path),
                        // recurring in this sibling call site (code-quality
                        // M6 review MAJOR).
                        let refs_rel: Vec<String> = refs
                            .files
                            .iter()
                            .map(|r| {
                                anyhow::ensure!(
                                    r.starts_with(&canonical_root),
                                    "js_bundle: bundler config {p:?} references {r:?}, which \
                                     resolved outside the workspace root ({canonical_root:?})"
                                );
                                Ok(r.strip_prefix(&canonical_root)
                                    .unwrap_or(r)
                                    .to_string_lossy()
                                    .replace('\\', "/"))
                            })
                            .collect::<anyhow::Result<Vec<String>>>()
                            .with_context(|| {
                                format!("scoping files referenced by bundler config {p:?}")
                            })?;
                        let external = parse_bundler_config_external(&content)
                            .with_context(|| format!("parsing bundler config {p:?}"))?;
                        (rel, content, refs_rel, external)
                    }
                    None => (String::new(), String::new(), Vec::new(), Vec::new()),
                };

                // The entry package's own resolved tsconfig (if any) plus its
                // `extends` chain — esbuild auto-discovers and applies a
                // tsconfig's `compilerOptions` (`paths`/`baseUrl`/`jsx`/`target`/
                // `experimentalDecorators`) the same way `tsc` does, so it must
                // be a declared, staged, hashed Input the same way
                // `typecheck_deps_config` already treats it (code-quality M6
                // review BLOCKER).
                let tsconfig = importgraph::find_nearest_tsconfig(&workspace_root, &pkg_dir);
                let (tsconfig_path, mut tsconfig_content) = match &tsconfig {
                    Some(p) => {
                        let rel = p
                            .strip_prefix(&workspace_root)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .replace('\\', "/");
                        let content = std::fs::read_to_string(p)
                            .with_context(|| format!("reading tsconfig {p:?}"))?;
                        (rel, content)
                    }
                    None => (String::new(), String::new()),
                };
                let mut tsconfig_refs: Vec<String> = Vec::new();
                if let Some(leaf) = &tsconfig {
                    let chain = importgraph::resolve_tsconfig_extends_chain(&workspace_root, leaf)
                        .with_context(|| {
                            format!("resolving tsconfig extends chain for {pkg_str:?}")
                        })?;
                    for ancestor in &chain {
                        anyhow::ensure!(
                            ancestor.starts_with(&canonical_root),
                            "js_bundle: tsconfig {leaf:?}'s extends chain resolved {ancestor:?} \
                         outside the workspace root ({canonical_root:?})"
                        );
                        let rel = ancestor
                            .strip_prefix(&canonical_root)
                            .unwrap_or(ancestor)
                            .to_string_lossy()
                            .replace('\\', "/");
                        tsconfig_refs.push(rel);
                        let content = std::fs::read_to_string(ancestor)
                            .with_context(|| format!("reading extended tsconfig {ancestor:?}"))?;
                        tsconfig_content.push('\n');
                        tsconfig_content.push_str(&content);
                    }
                }

                Ok(BundleAncillary {
                    bundler_config_path,
                    bundler_config_content,
                    bundler_config_refs,
                    config_external,
                    tsconfig_path,
                    tsconfig_content,
                    tsconfig_refs,
                })
            })
            .await?;

        let mut deps: HashMap<String, Value> = HashMap::new();
        deps.insert(
            String::new(),
            Value::List(
                closure
                    .files
                    .iter()
                    .map(|p| Value::String(hbuiltins::pluginfs::file_addr(p).format()))
                    .collect(),
            ),
        );
        if !closure.external_addrs.is_empty() {
            deps.insert(
                "external".to_string(),
                Value::List(
                    closure
                        .external_addrs
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        let mut bundler_config_addrs: BTreeSet<String> = BTreeSet::new();
        if !ancillary.bundler_config_path.is_empty() {
            bundler_config_addrs
                .insert(hbuiltins::pluginfs::file_addr(&ancillary.bundler_config_path).format());
        }
        for rel in &ancillary.bundler_config_refs {
            bundler_config_addrs.insert(hbuiltins::pluginfs::file_addr(rel).format());
        }
        if !bundler_config_addrs.is_empty() {
            deps.insert(
                "bundler_config".to_string(),
                Value::List(
                    bundler_config_addrs
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        let mut tsconfig_addrs: BTreeSet<String> = BTreeSet::new();
        if !ancillary.tsconfig_path.is_empty() {
            tsconfig_addrs
                .insert(hbuiltins::pluginfs::file_addr(&ancillary.tsconfig_path).format());
        }
        for rel in &ancillary.tsconfig_refs {
            tsconfig_addrs.insert(hbuiltins::pluginfs::file_addr(rel).format());
        }
        if !tsconfig_addrs.is_empty() {
            deps.insert(
                "tsconfig".to_string(),
                Value::List(tsconfig_addrs.into_iter().map(Value::String).collect()),
            );
        }

        // Union the closure's own discovered third-party bare-specifier
        // names with the bundler config's opt-in `"external"` list — see
        // `BundleClosureResult::external_names`'s doc and the feature-quality
        // M6 review BLOCKER this fixes.
        let mut external_set: BTreeSet<String> = closure.external_names.clone();
        external_set.extend(ancillary.config_external.iter().cloned());

        Ok(BundleDepsConfig {
            deps,
            bundler_config_path: ancillary.bundler_config_path,
            bundler_config_content: ancillary.bundler_config_content,
            external: external_set.into_iter().collect(),
            tsconfig_path: ancillary.tsconfig_path,
            tsconfig_content: ancillary.tsconfig_content,
        })
    }

    /// Build the config for a `js_bundle` target: the host bundler toolchain
    /// resolution/version-query this milestone's disclosed
    /// `bundler`-is-host-resolved escape hatch requires (see `toolchain.rs`
    /// module docs for why the version is queried here, at spec-resolution
    /// time, rather than deferred to the driver's `run()`), plus
    /// `bundle_deps_config`'s pure, bundler-binary-free Input-scoping.
    async fn bundle_config(
        &self,
        pkg: &PkgBuf,
        entry_file_rel: &str,
        format: &str,
        target_env: &str,
    ) -> anyhow::Result<HashMap<String, Value>> {
        let resolved_bundler = self.resolved_host_bundler().await?;
        let bundler_bin = resolved_bundler.0.to_string_lossy().into_owned();
        let bundler_version = resolved_bundler.1.clone();
        let bundler = self.bundler.clone();

        let deps_config = self.bundle_deps_config(pkg, entry_file_rel).await?;

        // `format`/`target_env` are part of the output path — otherwise
        // `js_bundle@format=esm` and `js_bundle@format=cjs` for the same
        // package both declare the identical `Content::DirPath` output,
        // colliding whenever both are built together (the common dual-
        // format-publish shape this milestone's own headline variant axis
        // exists for — feature-quality M6 review BLOCKER).
        let outdir = if pkg.as_str().is_empty() {
            format!("dist/{format}-{target_env}")
        } else {
            format!("{}/dist/{format}-{target_env}", pkg.as_str())
        };

        let mut config: HashMap<String, Value> = HashMap::new();
        config.insert("bundler".to_string(), Value::String(bundler));
        config.insert("bundler_bin".to_string(), Value::String(bundler_bin));
        config.insert(
            "bundler_version".to_string(),
            Value::String(bundler_version),
        );
        config.insert(
            "entry_file".to_string(),
            Value::String(entry_file_rel.to_string()),
        );
        config.insert("format".to_string(), Value::String(format.to_string()));
        config.insert("target".to_string(), Value::String(target_env.to_string()));
        config.insert("outdir".to_string(), Value::String(outdir));
        config.insert(
            "bundler_config_path".to_string(),
            Value::String(deps_config.bundler_config_path),
        );
        config.insert(
            "bundler_config_content".to_string(),
            Value::String(deps_config.bundler_config_content),
        );
        config.insert(
            "external".to_string(),
            Value::List(
                deps_config
                    .external
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
        config.insert(
            "tsconfig_path".to_string(),
            Value::String(deps_config.tsconfig_path),
        );
        config.insert(
            "tsconfig_content".to_string(),
            Value::String(deps_config.tsconfig_content),
        );
        config.insert("deps".to_string(), Value::Map(deps_config.deps));
        Ok(config)
    }
}

/// Result of [`Provider::bundle_closure`]: the full transitive first-party
/// closure plus the third-party packages it reaches, both by declared
/// `Input` addr (`external_addrs`) and by the bare-specifier name esbuild's
/// own `--external:<name>` flag needs (`external_names`) — see that field's
/// doc.
struct BundleClosureResult {
    files: BTreeSet<String>,
    external_addrs: BTreeSet<String>,
    /// The bare specifier name (e.g. `"lodash"`, `"@scope/pkg"`) each
    /// third-party edge in `external_addrs` was actually imported by —
    /// captured from `graph.unresolved_bare_specifiers`' own
    /// `site.package_name` (the lockfile-driven path) or
    /// `importgraph::thirdparty_pkg_name_from_path` (the ambient-
    /// `node_modules` path), never re-derived from the resolved addr, which
    /// carries only a version-pinned `js_install` target, not the specifier
    /// text a source file actually wrote. Feeds esbuild's `--external:<name>`
    /// flag directly (`Provider::bundle_deps_config`) — the piece the
    /// feature-quality M6 review found missing entirely: previously only a
    /// bundler-config-file's own opt-in `"external"` array reached `--external`,
    /// so every real third-party import (never listed there) made
    /// `esbuild --bundle` hard-fail trying to inline it.
    external_names: BTreeSet<String>,
}

/// Result of [`Provider::bundle_deps_config`]: the `deps` map (see that
/// function's doc) plus the resolved bundler config's and tsconfig's own
/// workspace-relative paths/raw content, and the merged `--external` name
/// list, for a `js_bundle` target. A plain tuple would work but — same
/// precedent as [`LintDepsConfig`] — a 6-element one is too easy to
/// mis-order at the call site; a named struct makes each field
/// self-documenting instead.
struct BundleDepsConfig {
    deps: HashMap<String, Value>,
    bundler_config_path: String,
    bundler_config_content: String,
    /// The closure's own discovered third-party bare-specifier names, unioned
    /// with the bundler config's opt-in `"external"` array — see
    /// [`BundleClosureResult::external_names`]'s doc.
    external: Vec<String>,
    tsconfig_path: String,
    tsconfig_content: String,
}

/// Filesystem-only resolution [`Provider::bundle_deps_config`] performs
/// inside a single `hcore::blocking::run` round trip: the bundler config (if
/// any) plus its own referenced files, and the entry package's tsconfig (if
/// any) plus its `extends` chain.
struct BundleAncillary {
    bundler_config_path: String,
    bundler_config_content: String,
    bundler_config_refs: Vec<String>,
    config_external: Vec<String>,
    tsconfig_path: String,
    tsconfig_content: String,
    tsconfig_refs: Vec<String>,
}

/// One BFS step's discoveries when walking `file_rel`'s own `runtime_edges`
/// in `pkg`'s [`importgraph::ImportGraph`] — see [`Provider::bundle_closure`].
struct BundleClosureStep {
    /// `(owning_pkg, workspace_rel_path)` pairs for every not-yet-seen
    /// first-party edge target — `owning_pkg` may differ from `pkg` when the
    /// edge crosses into a sibling workspace package (see
    /// `importgraph::firstparty_owning_pkg_dir`).
    new_files: Vec<(String, String)>,
    /// `(bare_specifier_name, target_addr)` pairs for every third-party
    /// package this step's edges/bare specifiers reached — see
    /// `driver_bundle.rs` module docs' "Inputs / cache key" section and
    /// [`BundleClosureResult::external_names`]'s doc for why the name is
    /// carried alongside the addr, not discarded.
    new_external: Vec<(String, String)>,
}

/// How a resolved import edge landing outside the owning package classifies.
enum EdgeClassification {
    /// Not under any `node_modules/` — a genuine first-party edge (a
    /// workspace sibling), for the caller to handle its own way.
    FirstParty,
    /// Landed inside a `node_modules/` tree — only possible when one
    /// happens to exist ambient on this host (`Provider::get` runs before
    /// `js_install` ever executes). `Some((name, addr))` when resolution
    /// found a relocated `js_install` addr to wire; `None` when the name
    /// resolves to a declared-but-platform-mismatched optional dependency
    /// (nothing to wire, not an error).
    ThirdParty(Option<(String, String)>),
}

/// Classify one resolved import edge and, if it landed inside an ambient
/// `node_modules`, resolve it the *same* hermetic, lockfile-driven way an
/// `unresolved_bare_specifiers` site is — never by declaring a raw `fs:file`
/// Input at the ambient path, which would depend on host filesystem state no
/// input hashes, and would never even be reached on a fresh checkout with no
/// `node_modules` installed yet. Shared by `bundle_closure_step`,
/// `typecheck_deps_config`, and `test_deps_config` — the same fix, previously
/// duplicated three times (a code-quality review finding: the class of bug
/// this function exists to prevent had already independently drifted across
/// those three copies once).
///
/// `site` is a human-readable description of what's being resolved (e.g. the
/// importing file's path) — folded into the error context so a resolution
/// failure names *which* file/edge caused it, not just the package.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors resolve_one_dependency's own parameter set, plus the resolved path and a \
              site description for error context"
)]
fn classify_resolved_edge(
    resolved: &Path,
    site: &str,
    caller: &str,
    consuming_pkg: &str,
    lockfile_pkg: &str,
    manifest: &package_json::PackageManifest,
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    member_addrs_by_name: &BTreeMap<String, String>,
    os: &str,
    arch: &str,
) -> anyhow::Result<EdgeClassification> {
    let Some(name) = importgraph::thirdparty_pkg_name_from_path(resolved) else {
        return Ok(EdgeClassification::FirstParty);
    };
    let addr = deps::resolve_one_dependency(
        consuming_pkg,
        lockfile_pkg,
        &name,
        manifest,
        lockfile,
        resolved_graph,
        member_addrs_by_name,
        os,
        arch,
    )
    .with_context(|| {
        format!(
            "resolving {site:?}'s ambient-node_modules-resolved import of `{name}` for {caller}"
        )
    })?;
    Ok(EdgeClassification::ThirdParty(
        addr.map(|addr| (name, addr)),
    ))
}

/// Pure (no async, only what's already loaded) per-file edge walk:
/// everything [`importgraph::build_test_closure`] does for one file,
/// generalized to (a) recurse across package boundaries instead of stopping
/// at one hop and (b) only follow `runtime_edges`, never `type_edges` (a
/// bundler erases type-only imports — see `driver_bundle.rs` module docs).
/// Split out from [`Provider::bundle_closure`] so the per-file work can run
/// on the blocking pool without an `async fn` in the way, and so it is
/// unit-testable without a real bundler binary.
///
/// Deliberately does **not** wire [`deps::resolve_transitive_closure`] the
/// way `typecheck_deps_config`/`test_deps_config` do: a bundler marks every
/// resolved third-party name `--external` (never bundled, left as a runtime
/// `require`/`import`) and therefore never descends into that package's own
/// source to discover *its* further dependencies at build time — unlike
/// `tsc` (which follows a resolved package's own `.d.ts` chain) or a real
/// `vitest`/`jest` run (which executes the resolved package's own code), a
/// bundle's build-time Input set genuinely stops at the directly-referenced
/// name.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors typecheck_deps_config's/test_deps_config's own lockfile/graph/member/\
              platform parameter set, needed here too for the same on-demand third-party-input \
              resolution"
)]
fn bundle_closure_step(
    canonical_root: &Path,
    consuming_pkg: &str,
    lockfile_pkg: &str,
    file_rel: &str,
    graph: &importgraph::ImportGraph,
    manifest: &package_json::PackageManifest,
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    member_addrs_by_name: &BTreeMap<String, String>,
    os: &str,
    arch: &str,
) -> anyhow::Result<BundleClosureStep> {
    let mut new_files = Vec::new();
    let mut new_external = Vec::new();

    for edge in graph.runtime_edges.iter().filter(|e| e.file == file_rel) {
        match classify_resolved_edge(
            &edge.resolved,
            &edge.file,
            "js_bundle",
            consuming_pkg,
            lockfile_pkg,
            manifest,
            lockfile,
            resolved_graph,
            member_addrs_by_name,
            os,
            arch,
        )? {
            EdgeClassification::ThirdParty(resolved) => {
                if let Some(pair) = resolved {
                    new_external.push(pair);
                }
                continue;
            }
            EdgeClassification::FirstParty => {}
        }

        anyhow::ensure!(
            edge.resolved.starts_with(canonical_root),
            "js_bundle: {:?} imports from {:?}, which resolved outside the workspace root \
             ({:?}) — cannot express it as a declared js_bundle input (this typically means \
             node_modules is a symlink to a global store outside the workspace)",
            edge.file,
            edge.resolved,
            canonical_root
        );
        let rel = edge
            .resolved
            .strip_prefix(canonical_root)
            .unwrap_or(&edge.resolved)
            .to_string_lossy()
            .replace('\\', "/");
        let owning_pkg = importgraph::firstparty_owning_pkg_dir(&edge.resolved, canonical_root)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "js_bundle: {:?} imports from {:?}, which resolved inside the workspace \
                     root but has no owning package.json ancestor",
                    edge.file,
                    edge.resolved
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        new_files.push((owning_pkg, rel));
    }

    for site in graph
        .unresolved_bare_specifiers
        .iter()
        .filter(|s| s.file == file_rel)
    {
        if let Some(addr) = deps::resolve_one_dependency(
            consuming_pkg,
            lockfile_pkg,
            &site.package_name,
            manifest,
            lockfile,
            resolved_graph,
            member_addrs_by_name,
            os,
            arch,
        )
        .with_context(|| {
            format!(
                "resolving {:?}'s unresolved import of `{}` for js_bundle",
                site.file, site.package_name
            )
        })? {
            new_external.push((site.package_name.clone(), addr));
        }
    }

    Ok(BundleClosureStep {
        new_files,
        new_external,
    })
}

/// Parse a `js_bundle` bundler config's `"external"` array (esbuild's own
/// `--external:<name>` flag, one per entry) — the only field this
/// milestone's minimal `esbuild.config.json` schema recognizes (the esbuild
/// CLI has no config-file convention of its own; see `driver_bundle.rs`
/// module docs and `importgraph::find_nearest_bundler_config`'s doc for why
/// this crate defines its own small schema instead). `Ok(vec![])` for a
/// config with no `"external"` key. A malformed JSON document, or a present
/// `"external"` value that isn't an array of strings, is a hard
/// `Provider::get` error naming the problem — never a silently ignored
/// config.
fn parse_bundler_config_external(content: &str) -> anyhow::Result<Vec<String>> {
    let value: serde_json::Value =
        serde_json::from_str(content).context("parsing bundler config as JSON")?;
    let Some(external) = value.get("external") else {
        return Ok(Vec::new());
    };
    let arr = external.as_array().ok_or_else(|| {
        anyhow::anyhow!("bundler config's \"external\" field must be an array of strings")
    })?;
    arr.iter()
        .map(|v| {
            v.as_str().map(str::to_string).ok_or_else(|| {
                anyhow::anyhow!("bundler config's \"external\" array must contain only strings")
            })
        })
        .collect()
}

/// Build the `deps` map (`""` = the package's own first-party source files,
/// scoped to the tsconfig's own `include`/`files`/`exclude` when it declares
/// any; `"types"` = every file `ImportGraph::type_edges`/`runtime_edges`
/// resolved to outside the package (workspace-sibling or third-party),
/// **plus** — for an import that named a third-party/sibling package but
/// never resolved on disk at all (the ambient-`node_modules`-absent steady
/// state) — that package's whole `js_install`/sibling addr, resolved the
/// same lockfile-driven way `Provider::deps_config` already does; `"tsconfig"`
/// = the resolved tsconfig itself plus every config in its `extends` chain)
/// plus the resolved tsconfig's own workspace-relative path and raw content
/// (the leaf's, with every `extends`-chain ancestor's content appended), for
/// a `js_typecheck` target.
///
/// Deliberately split out from [`Provider::typecheck_config`] so it never
/// touches the host `tsc` toolchain — the whole point being that it's
/// unit-testable **without** a real `tsc` binary. This is the piece the
/// M3 task calls "the single most important test in this milestone": a
/// scoping mistake here either silently under-caches (a `.d.ts` dependency
/// change never busts the cache) or over-caches (every package's
/// `js_typecheck` re-keys on any workspace file touch, defeating per-package
/// granularity entirely). See this module's tests.
///
/// A *shared* tsconfig (the package has none of its own, and inherits an
/// ancestor's) is only trusted when that ancestor's own `include`/`files`
/// provably confine it to this package — see
/// `importgraph::check_tsconfig_scope`'s doc. Anything else is a hard
/// `Provider::get` error naming the ambiguity, rather than silently building
/// an Input set that might not match what `tsc --project` actually reads.
///
/// `pkg` is the package's workspace-relative path (`""` for the root).
/// `graph` is `pkg`'s [`importgraph::ImportGraph`] — built once by
/// `Provider::import_graph` and shared with `deps_config`/`test_deps_config`
/// rather than rebuilt here; see that method's doc for why.
/// `lockfile`/`resolved_graph`/`member_addrs_by_name`/`os`/`arch` are
/// only consulted when an import names a package that never resolved on
/// disk — see above — and mirror the identically-named parameters
/// `deps::resolve_package_deps` already takes for `Provider::deps_config`.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors deps::resolve_package_deps's own lockfile/graph/member/platform parameter \
              set, needed here too for the same on-demand third-party-type-input resolution"
)]
fn typecheck_deps_config(
    walker: &CachedWalker,
    workspace_root: &Path,
    pkg: &str,
    lockfile_pkg: &str,
    graph: &importgraph::ImportGraph,
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    member_addrs_by_name: &BTreeMap<String, String>,
    os: &str,
    arch: &str,
) -> anyhow::Result<(HashMap<String, Value>, String, String)> {
    let pkg_dir = if pkg.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(pkg)
    };

    let manifest =
        package_json::read_package_manifest(&pkg_dir.join(PACKAGE_JSON)).with_context(|| {
            format!("reading {pkg:?}'s package.json for js_typecheck Input scoping")
        })?;

    // `edge.resolved`/`extends` targets are realpath'd (`symlinks: true` —
    // see `importgraph.rs` module docs' "Hermeticity" section), so every
    // containment check below must compare against a canonicalized
    // `workspace_root` — comparing against the non-canonicalized one
    // silently breaks containment on any host where an ancestor of the
    // workspace is itself a symlink (e.g. macOS's `/tmp` -> `/private/tmp`),
    // the same class of bug `check_phantom_dependencies` already guards
    // against.
    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {workspace_root:?}"))?;

    let tsconfig = importgraph::find_nearest_tsconfig(workspace_root, &pkg_dir);
    let tsconfig_fields = match &tsconfig {
        Some(p) => importgraph::read_tsconfig_fields(p)
            .with_context(|| format!("reading {p:?}'s include/exclude/files"))?,
        None => Default::default(),
    };
    if let Some(p) = &tsconfig {
        let tsconfig_dir = p.parent().unwrap_or(p);
        importgraph::check_tsconfig_scope(p, tsconfig_dir, &pkg_dir, &tsconfig_fields)
            .with_context(|| format!("checking tsconfig scope for {pkg:?}"))?;
    }

    let (tsconfig_path_rel, mut tsconfig_content) = match &tsconfig {
        Some(p) => {
            let rel = p
                .strip_prefix(workspace_root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            let content =
                std::fs::read_to_string(p).with_context(|| format!("reading tsconfig {p:?}"))?;
            (rel, content)
        }
        None => (String::new(), String::new()),
    };

    // `extends` chain: `tsc --project` merges every ancestor's
    // `compilerOptions` into the effective program, so each one is both a
    // declared Input (so the sandbox actually stages it — otherwise a real
    // `tsc` run can't find it at all) and folded into the hash.
    let mut tsconfig_rel: BTreeSet<String> = BTreeSet::new();
    if let Some(leaf) = &tsconfig {
        tsconfig_rel.insert(tsconfig_path_rel.clone());
        let chain = importgraph::resolve_tsconfig_extends_chain(&canonical_root, leaf)
            .with_context(|| format!("resolving tsconfig extends chain for {pkg:?}"))?;
        for ancestor in &chain {
            let rel = ancestor
                .strip_prefix(&canonical_root)
                .unwrap_or(ancestor)
                .to_string_lossy()
                .replace('\\', "/");
            tsconfig_rel.insert(rel);
            let content = std::fs::read_to_string(ancestor)
                .with_context(|| format!("reading extended tsconfig {ancestor:?}"))?;
            tsconfig_content.push('\n');
            tsconfig_content.push_str(&content);
        }
    }

    // Phantom-dependency check: `Provider::deps_config` (the `js_package_info`
    // target) already runs this, but a workspace member requesting only
    // `js_typecheck` (never `js_package_info`) must not skip it — this is
    // also what justifies treating every name reached below via
    // `member_addrs_by_name`/the lockfile as genuinely declared, rather than
    // re-deriving that from scratch. `graph` is the caller-supplied,
    // `Provider::import_graph`-cached graph — see this function's doc.
    let declared_closure = importgraph::transitive_declared_closure(
        &manifest,
        lockfile_pkg,
        lockfile,
        resolved_graph,
        os,
        arch,
    )?;
    importgraph::check_phantom_dependencies(workspace_root, pkg, graph, &declared_closure)
        .with_context(|| {
            format!("cross-checking {pkg:?}'s import graph against its declared dependencies")
        })?;

    let src_files = importgraph::package_source_files(walker, workspace_root, pkg)
        .with_context(|| format!("collecting first-party source files for {pkg:?}"))?;
    let src_files = importgraph::filter_by_tsconfig_fields(
        src_files,
        tsconfig
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or(&pkg_dir),
        &tsconfig_fields,
    )
    .with_context(|| format!("filtering {pkg:?}'s source files by tsconfig include/exclude"))?;

    // BTreeSet: deterministic order (the config/hash must not depend on
    // filesystem walk order) and cheap de-dup of any path reachable more than
    // once (e.g. two type-only imports of the same sibling file).
    let mut src_rel: BTreeSet<String> = BTreeSet::new();
    for f in &src_files {
        let rel = f
            .strip_prefix(workspace_root)
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        src_rel.insert(rel);
    }

    // Every edge — type-only *and* plain runtime — that resolved outside the
    // package is something `tsc` reads for this package and must be a
    // declared Input; a plain (non-`import type`) cross-package import needs
    // its target's types just as much as an `import type` does (code-quality
    // M3 review finding).
    let mut types_addrs: BTreeSet<String> = BTreeSet::new();
    for edge in graph.type_edges.iter().chain(graph.runtime_edges.iter()) {
        match classify_resolved_edge(
            &edge.resolved,
            &edge.file,
            "js_typecheck",
            pkg,
            lockfile_pkg,
            &manifest,
            lockfile,
            resolved_graph,
            member_addrs_by_name,
            os,
            arch,
        )? {
            EdgeClassification::ThirdParty(resolved) => {
                if let Some((_, addr)) = resolved {
                    types_addrs.insert(addr);
                }
                continue;
            }
            EdgeClassification::FirstParty => {}
        }
        let rel = edge
            .resolved
            .strip_prefix(&canonical_root)
            .with_context(|| {
                format!(
                    "js_typecheck: {:?} imports from {:?}, which resolved outside the workspace \
                     root ({:?}) — cannot express it as a declared `js_typecheck` input (this \
                     typically means node_modules is a symlink to a global store outside the \
                     workspace)",
                    edge.file, edge.resolved, workspace_root
                )
            })?
            .to_string_lossy()
            .replace('\\', "/");
        if !src_rel.contains(&rel) {
            types_addrs.insert(hbuiltins::pluginfs::file_addr(&rel).format());
        }
    }

    // An import that named a third-party/sibling package but never resolved
    // on disk at all — the realistic steady state absent an out-of-band
    // install — is otherwise invisible to the loop above (an unresolved
    // specifier never becomes an edge). Resolve each such name the same
    // lockfile-driven way `Provider::deps_config` addresses a declared
    // dependency, so the Input set doesn't silently go empty just because
    // `node_modules` doesn't happen to exist yet (feature-quality M3 review
    // finding). `check_phantom_dependencies` above already proved every one
    // of these names is declared; a `None` here means it's a declared
    // `optionalDependencies` entry that doesn't apply to this
    // platform/lockfile state — nothing to depend on, matching
    // `deps_config`'s own handling of the same case.
    for site in &graph.unresolved_bare_specifiers {
        if let Some(addr) = deps::resolve_one_dependency(
            pkg,
            lockfile_pkg,
            &site.package_name,
            &manifest,
            lockfile,
            resolved_graph,
            member_addrs_by_name,
            os,
            arch,
        )
        .with_context(|| {
            format!(
                "resolving {:?}'s unresolved import of `{}` for js_typecheck",
                site.file, site.package_name
            )
        })? {
            types_addrs.insert(addr);
        }
    }
    // A resolved third-party package's own internal dependencies need the
    // same relocation a directly-imported one gets, or `tsc` reading that
    // package's own `.d.ts` chain hits an unresolved import one edge deeper
    // — see `deps::resolve_transitive_closure`'s doc.
    if let (Some(lf), Some(rg)) = (lockfile, resolved_graph) {
        types_addrs.extend(
            deps::resolve_transitive_closure(pkg, lockfile_pkg, &manifest, lf, rg, os, arch)
                .with_context(|| {
                    format!("resolving {pkg:?}'s transitive third-party closure for js_typecheck")
                })?,
        );
    }

    let mut deps: HashMap<String, Value> = HashMap::new();
    deps.insert(
        String::new(),
        Value::List(
            src_rel
                .iter()
                .map(|p| Value::String(hbuiltins::pluginfs::file_addr(p).format()))
                .collect(),
        ),
    );
    if !types_addrs.is_empty() {
        deps.insert(
            "types".to_string(),
            Value::List(types_addrs.into_iter().map(Value::String).collect()),
        );
    }
    if !tsconfig_rel.is_empty() {
        deps.insert(
            "tsconfig".to_string(),
            Value::List(
                tsconfig_rel
                    .iter()
                    .map(|p| Value::String(hbuiltins::pluginfs::file_addr(p).format()))
                    .collect(),
            ),
        );
    }

    Ok((deps, tsconfig_path_rel, tsconfig_content))
}

/// Candidate config filenames for `testrunner`'s ancestor-chain walk (see
/// `importgraph::find_nearest_test_runner_config`) — every extension the
/// runner itself accepts for its own config file. Errors on an unsupported
/// `testrunner` rather than guessing — callers only ever reach this after
/// `toolchain::is_supported_testrunner` has already validated it (see
/// `Provider::resolved_host_test_runner`), so this should never actually
/// trigger in practice, but a fallible return keeps that an enforced
/// invariant rather than an assumed one.
fn runner_config_candidates(testrunner: &str) -> anyhow::Result<&'static [&'static str]> {
    match testrunner {
        toolchain::VITEST => Ok(&[
            "vitest.config.ts",
            "vitest.config.js",
            "vitest.config.mjs",
            "vitest.config.cjs",
            "vitest.config.mts",
            "vitest.config.cts",
            // Vitest's own documented fallback: a project that configures
            // testing entirely under `vite.config.ts`'s `test: {...}` key,
            // with no separate `vitest.config.*` file at all, is real and
            // common (hermeticity M4 review) — checked after the dedicated
            // filenames above, matching vitest's own precedence (dedicated
            // config wins over the shared `vite.config.*`).
            "vite.config.ts",
            "vite.config.js",
            "vite.config.mjs",
            "vite.config.cjs",
            "vite.config.mts",
            "vite.config.cts",
        ]),
        toolchain::JEST => Ok(&[
            "jest.config.js",
            "jest.config.ts",
            "jest.config.mjs",
            "jest.config.cjs",
            "jest.config.json",
            // `package.json`'s own `"jest"` field (jest's other documented
            // config location) is handled separately by
            // `importgraph::find_nearest_jest_package_json_config` — checked
            // by `test_deps_config` only once none of these dedicated
            // filenames are found on the same ancestor chain.
        ]),
        other => anyhow::bail!(
            "js_test: unsupported testrunner {other:?} — expected \"vitest\" or \"jest\" (should \
             have been rejected earlier by toolchain::is_supported_testrunner)"
        ),
    }
}

/// Reject an addr arg or a resolved/cached config field — `js_test`'s `file`
/// arg, `js_bundle`'s `entry` arg/`entry_file`/`outdir`, `package.json`'s own
/// `"main"`, or any of these read back out of a cached `TargetDef` — that is
/// anything other than a plain workspace-relative path: absolute, or
/// `..`-escaping. `Path::join` silently *replaces* the base when the joined
/// argument is absolute, so an unvalidated e.g. `file=/etc/passwd` addr would
/// otherwise resolve to the literal host path in both `Provider::get`
/// (`workspace_root.join(...)`) and a driver's `run()`
/// (`sandbox_ws_dir.join(...)`) — a direct violation of architecture.md's
/// target-isolation invariant ("It sees only its declared inputs; no ambient
/// filesystem access"). Mirrors `hbuiltins::pluginfs::normalize_path`'s
/// escape rejection (this crate cannot depend on `builtins`, and the
/// fs-provider's own protection never fires here: the caller's value is a
/// raw config string consumed directly, not a `fs:file` dep-group addr the
/// engine resolves through it).
///
/// `field` names the caller's own field/arg (e.g. `"entry_file"`,
/// `"file arg"`, `"package.json main"`) so the error always names the value
/// that actually failed — a code-quality review MAJOR previously found this
/// hardcoding `"js_test"` even when validating an unrelated `js_bundle`
/// field, actively misleading during debugging.
pub(crate) fn reject_path_escape(field: &str, path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !Path::new(path).is_absolute(),
        "{field} {path:?} must be a workspace-relative path, not absolute"
    );
    anyhow::ensure!(
        !path.split('/').any(|c| c == ".."),
        "{field} {path:?} must not contain a `..` path component"
    );
    Ok(())
}

/// A validated workspace-relative path must additionally live under the
/// addressed package's own directory — otherwise
/// `//packages/a:js_test@file=<path>` (or, since M6,
/// `//packages/a:js_bundle@entry=<path>`) could address any other real,
/// existing file anywhere in the workspace (e.g. a sibling package's source
/// file never surfaced by `Provider::list`), confined to the workspace but
/// still never a real target for the addressed package. `package` empty
/// means the root package (everything not already claimed by a nested
/// `package.json` is "under" it).
fn path_under_package(package: &str, path: &str) -> bool {
    if package.is_empty() {
        return true;
    }
    path.strip_prefix(package)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Build the `deps` map (`""` = the test file's own runtime-transitive
/// first-party closure within its owning package —
/// `importgraph::build_test_closure` over `ImportGraph::runtime_edges`,
/// always including the test file itself; `"external"` = every file that
/// closure reaches just outside the package boundary, one-hop only, plus a
/// lockfile-resolved third-party addr for any closure member's unresolved
/// bare specifier — the lockfile-driven `deps::resolve_one_dependency`
/// mechanism, **never** by walking `oxc_resolver` paths against an ambient
/// `node_modules` on disk: `Provider::get` always runs before `js_install`
/// ever executes, so a fresh checkout has no `node_modules` at all, and an M3
/// review caught exactly this mistake in an earlier draft of
/// `typecheck_deps_config` — see that function's identical on-demand
/// third-party handling for the precedent this mirrors; `"runner_config"` =
/// the resolved test-runner config file, if any) plus that config's own
/// workspace-relative path and raw content, for one `js_test` target.
///
/// Deliberately split out from [`Provider::test_config`] so it never touches
/// the host test-runner toolchain — unit-testable without a real
/// `vitest`/`jest` binary. This is the task's "single most important test in
/// this milestone": proving per-test-file, not per-package, Input scoping —
/// see this module's tests.
///
/// `pkg` is the package's workspace-relative path (`""` for the root);
/// `test_file_rel` is the one test file's workspace-relative path. `graph` is
/// `pkg`'s [`importgraph::ImportGraph`] — built once by
/// `Provider::import_graph` and shared with `deps_config`/`typecheck_config`
/// rather than rebuilt here; see that method's doc for why.
/// `lockfile`/`resolved_graph`/`member_addrs_by_name`/`os`/`arch` mirror
/// `typecheck_deps_config`'s identically-named parameters.
///
/// **Known scope trim, disclosed rather than silent — and a real gap, not
/// merely a narrower one**: the tsconfig that shaped how `graph` resolved
/// `paths`/`baseUrl`/`extends`-aware specifiers is not declared as its own
/// `js_test` Input, nor hashed, the way `js_typecheck`'s
/// `"tsconfig"`/`tsconfig_content` pair is. It is tempting to reason that
/// `js_test` runs source directly (not through `tsc`) so this only affects
/// import *resolution*, not behavior — but that reasoning is wrong: the
/// recommended default runner, vitest, transforms TS via Vite's
/// esbuild-based transform, which reads the nearest `tsconfig.json` itself at
/// transform time for options
/// (`jsx`/`jsxFactory`/`target`/`useDefineForClassFields`/
/// `experimentalDecorators`) that change the *emitted, executed* JS — not
/// just which file a specifier resolves to. Toggling one of those between
/// two commits, with the resolved import/closure addr set unchanged, is
/// silently invisible to this target's cache key today (M4 hermeticity
/// review). Accepted as a known trim for this milestone rather than fixed —
/// mirroring `js_typecheck`'s `tsconfig_content` fix is the natural
/// follow-up. TODO M4+.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors typecheck_deps_config's own lockfile/graph/member/platform parameter set, \
              needed here too for the same on-demand third-party-input resolution, plus \
              `testrunner` for the jest-package.json-field config fallback"
)]
fn test_deps_config(
    workspace_root: &Path,
    pkg: &str,
    lockfile_pkg: &str,
    test_file_rel: &str,
    graph: &importgraph::ImportGraph,
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    member_addrs_by_name: &BTreeMap<String, String>,
    os: &str,
    arch: &str,
    testrunner: &str,
    runner_config_candidates: &[&str],
) -> anyhow::Result<(HashMap<String, Value>, String, String)> {
    let pkg_dir = if pkg.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(pkg)
    };

    let manifest = package_json::read_package_manifest(&pkg_dir.join(PACKAGE_JSON))
        .with_context(|| format!("reading {pkg:?}'s package.json for js_test Input scoping"))?;

    // See `typecheck_deps_config`'s identical canonicalization requirement:
    // `edge.resolved`/`extends` targets are realpath'd, so every containment
    // check must compare against a canonicalized `workspace_root`.
    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {workspace_root:?}"))?;

    // Dedicated filenames first (`vitest.config.*`/`vite.config.*`, or
    // `jest.config.*`); jest's other documented config location —
    // `package.json`'s own `"jest"` field — is checked only once none of
    // those are found on the same ancestor chain, matching jest's own
    // precedence (hermeticity M4 review: this fallback was previously
    // entirely missing, so a project configured this way got an unhashed
    // `runner_config_path == ""` no matter what the real config said).
    let runner_config = importgraph::find_nearest_test_runner_config(
        workspace_root,
        &pkg_dir,
        runner_config_candidates,
    )
    .or_else(|| {
        if testrunner == toolchain::JEST {
            importgraph::find_nearest_jest_package_json_config(workspace_root, &pkg_dir)
        } else {
            None
        }
    });
    let (runner_config_path_rel, runner_config_content) = match &runner_config {
        Some(p) => {
            let rel = p
                .strip_prefix(workspace_root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            let content = std::fs::read_to_string(p)
                .with_context(|| format!("reading test-runner config {p:?}"))?;
            (rel, content)
        }
        None => (String::new(), String::new()),
    };

    // Additional files the resolved config's own content *names* or
    // *imports* — `setupFiles`/`setupFilesAfterEnv`/`globalSetup`/
    // `globalTeardown` entries, and a relative `import`/`require` of a
    // shared base config — recursively, since a base config can itself name
    // or import more (hermeticity M4 review BLOCKER: previously these were
    // never discovered, declared, staged, or hashed at all, so e.g. editing
    // a `setupFiles` target changed real test behavior — mocks, globals —
    // without busting the cache for any `js_test` target that shared it).
    // See `resolve_runner_config_referenced_files`'s doc for exactly what is
    // (and, in the disclosed-trim sense, is not) followed.
    let mut runner_config_ref_paths_rel: Vec<String> = Vec::new();
    // The config's own bare (third-party) imports — e.g. vitest.config.ts's
    // `import react from '@vitejs/plugin-react'` — resolved into `deps`'s
    // `"external"` group below, the same way `closure.bare_specifiers` (the
    // test *file's* own unresolved bare imports) already are. Without this,
    // a plugin the config needs just to be loaded (JSX transform, Lingui
    // macros, SVG imports, a browser-mode provider, …) was never staged in
    // the sandbox at all, regardless of whether the test file's own source
    // ever imported it.
    let mut runner_config_bare_specifiers: Vec<importgraph::BareSpecifierSite> = Vec::new();
    if let Some(p) = &runner_config {
        let scan = importgraph::resolve_runner_config_referenced_files(p, &runner_config_content)
            .with_context(|| {
            format!("scanning test-runner config {p:?} for referenced files")
        })?;
        for f in scan.files {
            runner_config_ref_paths_rel.push(
                f.strip_prefix(workspace_root)
                    .unwrap_or(&f)
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
        // `BareSpecifierSite::file` is workspace-relative everywhere else it's
        // used (`closure.bare_specifiers`, `graph.unresolved_bare_specifiers`)
        // — `resolve_runner_config_referenced_files` operates on absolute
        // paths internally (it has no `workspace_root` of its own to convert
        // against), so normalize here, matching `runner_config_ref_paths_rel`
        // above. Only affects error-message diagnostics, not resolution.
        runner_config_bare_specifiers = scan
            .bare_specifiers
            .into_iter()
            .map(|site| importgraph::BareSpecifierSite {
                file: Path::new(&site.file)
                    .strip_prefix(workspace_root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or(site.file),
                ..site
            })
            .collect();
    }

    // Phantom-dependency check: a workspace member requesting only `js_test`
    // (never `js_package_info`/`js_typecheck`) must not skip it — same
    // rationale `typecheck_deps_config` documents for its own identical call.
    // `graph` is the caller-supplied, `Provider::import_graph`-cached graph —
    // see this function's doc.
    let declared_closure = importgraph::transitive_declared_closure(
        &manifest,
        lockfile_pkg,
        lockfile,
        resolved_graph,
        os,
        arch,
    )?;
    importgraph::check_phantom_dependencies(workspace_root, pkg, graph, &declared_closure)
        .with_context(|| {
            format!("cross-checking {pkg:?}'s import graph against its declared dependencies")
        })?;
    // `check_phantom_dependencies` above only walks the test file's own
    // import graph (`graph`) — the runner config's own bare imports are a
    // separate source (see `runner_config_bare_specifiers`'s doc), so they
    // get the same declared-dependency cross-check here, by hand, rather
    // than silently passing through to `resolve_one_dependency` below (whose
    // hard-error wording assumes the name is already known-declared, which
    // isn't true for these).
    for site in &runner_config_bare_specifiers {
        anyhow::ensure!(
            declared_closure.contains(&site.package_name),
            "{pkg:?}: {file:?} imports `{specifier}`, which resolves to the third-party \
             package `{name}` — but `{name}` is not declared in {pkg:?}'s package.json \
             (`dependencies`/`devDependencies`). This is a phantom dependency: it may only \
             work today because another package's install hoisted `{name}` into reach; \
             declare it explicitly or the build is not hermetic across package \
             managers/layouts.",
            file = site.file,
            specifier = site.specifier,
            name = site.package_name,
        );
    }

    let closure = importgraph::build_test_closure(graph, &canonical_root, pkg, test_file_rel)
        .with_context(|| {
            format!("building test closure for {pkg:?}'s test file {test_file_rel:?}")
        })?;

    let mut deps: HashMap<String, Value> = HashMap::new();
    deps.insert(
        String::new(),
        Value::List(
            closure
                .files
                .iter()
                .map(|p| Value::String(hbuiltins::pluginfs::file_addr(p).format()))
                .collect(),
        ),
    );

    let mut external_addrs: BTreeSet<String> = BTreeSet::new();
    for f in &closure.external_files {
        // `build_test_closure` doesn't distinguish a genuine first-party
        // workspace-sibling file from an edge that only resolved because a
        // real `node_modules` happens to exist on this host — both land in
        // `external_files` alike (it just checks "outside my own package
        // dir"). Classify here, the same way `bundle_closure_step`/
        // `typecheck_deps_config` already do for their own runtime/type
        // edges: a `node_modules`-landed path must go through the
        // lockfile-driven `resolve_one_dependency` resolution, never a raw
        // `fs:file` at the ambient path.
        match classify_resolved_edge(
            Path::new(f),
            f,
            "js_test",
            pkg,
            lockfile_pkg,
            &manifest,
            lockfile,
            resolved_graph,
            member_addrs_by_name,
            os,
            arch,
        )? {
            EdgeClassification::ThirdParty(resolved) => {
                if let Some((_, addr)) = resolved {
                    external_addrs.insert(addr);
                }
                continue;
            }
            EdgeClassification::FirstParty => {}
        }
        external_addrs.insert(hbuiltins::pluginfs::file_addr(f).format());
    }
    // Mirrors `typecheck_deps_config`'s identical on-demand third-party
    // handling for an unresolved bare specifier: `check_phantom_dependencies`
    // above already proved every one of these names is declared; a `None`
    // here means it's a declared `optionalDependencies` entry that doesn't
    // apply to this platform/lockfile state.
    for site in closure
        .bare_specifiers
        .iter()
        .chain(&runner_config_bare_specifiers)
    {
        if let Some(addr) = deps::resolve_one_dependency(
            pkg,
            lockfile_pkg,
            &site.package_name,
            &manifest,
            lockfile,
            resolved_graph,
            member_addrs_by_name,
            os,
            arch,
        )
        .with_context(|| {
            format!(
                "resolving {:?}'s unresolved import of `{}` for js_test",
                site.file, site.package_name
            )
        })? {
            external_addrs.insert(addr);
        }
    }
    // Beyond what the test file/runner config directly import: a resolved
    // third-party package's *own* internal dependencies (`axios` needing
    // `follow-redirects`) must also be relocated into this package's
    // `node_modules`, or the moment the real vitest/jest run reaches that
    // package's own code, it hits the identical `Cannot find module`
    // failure one edge deeper — see `deps::resolve_transitive_closure`'s
    // doc.
    if let (Some(lf), Some(rg)) = (lockfile, resolved_graph) {
        external_addrs.extend(
            deps::resolve_transitive_closure(pkg, lockfile_pkg, &manifest, lf, rg, os, arch)
                .with_context(|| {
                    format!("resolving {pkg:?}'s transitive third-party closure for js_test")
                })?,
        );
    }
    if !external_addrs.is_empty() {
        deps.insert(
            "external".to_string(),
            Value::List(external_addrs.into_iter().map(Value::String).collect()),
        );
    }
    if !runner_config_path_rel.is_empty() {
        let mut runner_config_addrs = vec![Value::String(
            hbuiltins::pluginfs::file_addr(&runner_config_path_rel).format(),
        )];
        for rel in &runner_config_ref_paths_rel {
            runner_config_addrs.push(Value::String(hbuiltins::pluginfs::file_addr(rel).format()));
        }
        deps.insert(
            "runner_config".to_string(),
            Value::List(runner_config_addrs),
        );
    }

    Ok((deps, runner_config_path_rel, runner_config_content))
}

/// Candidate config filenames for `linter`'s ancestor-chain walk (see
/// `importgraph::find_nearest_lint_config`). oxlint has exactly one
/// dedicated config filename; eslint's list checks the modern flat-config
/// filenames first (eslint 9's own default resolution order — a project
/// with both a flat and a legacy config uses the flat one), falling back to
/// every legacy `.eslintrc.*` extension eslint itself accepts. Errors on an
/// unsupported `linter` rather than guessing — callers only ever reach this
/// after `toolchain::is_supported_linter` has already validated it (see
/// `Provider::resolved_host_linter`), so this should never actually trigger
/// in practice, but a fallible return keeps that an enforced invariant
/// rather than an assumed one.
fn lint_config_candidates(linter: &str) -> anyhow::Result<&'static [&'static str]> {
    match linter {
        toolchain::OXLINT => Ok(&[".oxlintrc.json"]),
        toolchain::ESLINT => Ok(&[
            "eslint.config.js",
            "eslint.config.mjs",
            "eslint.config.cjs",
            "eslint.config.ts",
            "eslint.config.mts",
            "eslint.config.cts",
            ".eslintrc.js",
            ".eslintrc.cjs",
            ".eslintrc.yaml",
            ".eslintrc.yml",
            ".eslintrc.json",
            ".eslintrc",
        ]),
        other => anyhow::bail!(
            "js_lint: unsupported linter {other:?} — expected \"oxlint\" or \"eslint\" (should \
             have been rejected earlier by toolchain::is_supported_linter)"
        ),
    }
}

/// Result of [`lint_deps_config`]: the `deps` map (see that function's doc)
/// plus the resolved linter config's/tsconfig's own workspace-relative path
/// and raw content, for a `js_lint` target. A plain tuple would work but
/// clippy (rightly) flags a 5-element one as too easy to mis-order at the
/// call site; a named struct makes each field self-documenting instead.
///
/// `Debug` is derived (code-quality M5 review NIT: most sibling internal
/// state in this file derives it, and its absence made `{:?}`/`expect_err`
/// in a test fail to compile) — private type, so this isn't a `rust.md`
/// violation either way, just consistency.
#[derive(Debug)]
struct LintDepsConfig {
    deps: HashMap<String, Value>,
    config_path: String,
    config_content: String,
    tsconfig_path: String,
    tsconfig_content: String,
}

/// Build the `deps` map (`""` = the package's own first-party source files
/// (no tsconfig-`include`/`exclude` filtering — a linter operates on raw
/// source files directly, unlike `tsc`'s project-scoped compilation);
/// `"config"` = the resolved linter config file, if any; `"tsconfig"` =
/// (eslint type-aware rules only) the tsconfig(s) named by
/// `parserOptions.project` plus their whole `extends` chain, exactly the
/// same Input/hash treatment `js_typecheck` gives its own tsconfig — see
/// `driver_lint.rs` module docs' "Inputs / cache key" section, and this is
/// the specific gap the M5 task calls out by name: a type-aware eslint
/// config's type information comes from that tsconfig the same way `tsc`'s
/// does, so a change to it (or anywhere in its `extends` chain) must bust
/// this target's cache the same way it busts `js_typecheck`'s;
/// `"eslint_plugins"` = (eslint only) every `extends`/`plugins` entry that
/// names an npm package, resolved through the lockfile
/// (`deps::resolve_one_dependency`) — never treated as a raw filesystem
/// path, the exact M3/M4-review-class mistake this milestone's task named
/// again for this driver) plus that config's own workspace-relative path and
/// raw content, for one `js_lint` target.
///
/// Deliberately split out from [`Provider::lint_config`] so it never touches
/// the host linter binary — unit-testable without a real `oxlint`/`eslint`
/// installed, mirroring `typecheck_deps_config`/`test_deps_config`'s
/// identical split for the identical reason.
///
/// `pkg` is the package's workspace-relative path (`""` for the root), used
/// for reading its own files. `lockfile_pkg` is that same package relative
/// to `lockfile`'s own root instead — not always the same value, since a
/// workspace may contain more than one independent npm/pnpm project (see
/// `Provider::lockfile_relative_pkg`'s doc) — and is what every
/// `Lockfile`-touching call below actually needs.
/// `lockfile`/`resolved_graph`/`member_addrs_by_name`/`os`/`arch` mirror
/// `typecheck_deps_config`'s identically-named parameters (only consulted
/// for eslint's `extends`/`plugins` package resolution).
///
/// **Known scope trim, disclosed rather than silent**: a package's own
/// ignore rules (`.eslintignore`, `.oxlintignore`, an `ignorePatterns` config
/// field) are not applied when collecting the `""` source-file group — every
/// first-party source file `importgraph::package_source_files` finds is
/// declared, so an ignored file still costs a declared Input (over-inclusion,
/// never a missed one). TODO M5+: fold ignore-pattern filtering in, mirroring
/// `typecheck_deps_config`'s tsconfig-`include`/`exclude` treatment.
///
/// Also deliberately does not wire [`deps::resolve_transitive_closure`] the
/// way `typecheck_deps_config`/`test_deps_config` do: unlike those, this
/// resolves names extracted from the *config's own text*
/// (`extends`/`plugins`), never from an oxc-parsed import graph, so a
/// resolved plugin's own further `require`s are never reachable from this
/// scan at all — narrower scope than the other three, TODO M5+ if a real
/// eslint-plugin-with-its-own-third-party-deps case motivates it.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors typecheck_deps_config's/test_deps_config's own lockfile/member/platform \
              parameter set, needed here too for the same on-demand third-party-input \
              resolution, plus `linter` to select the config-file search + eslint-only \
              type-aware/extends handling"
)]
fn lint_deps_config(
    walker: &CachedWalker,
    workspace_root: &Path,
    pkg: &str,
    lockfile_pkg: &str,
    linter: &str,
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    member_addrs_by_name: &BTreeMap<String, String>,
    os: &str,
    arch: &str,
) -> anyhow::Result<LintDepsConfig> {
    let pkg_dir = if pkg.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(pkg)
    };

    let manifest = package_json::read_package_manifest(&pkg_dir.join(PACKAGE_JSON))
        .with_context(|| format!("reading {pkg:?}'s package.json for js_lint Input scoping"))?;

    // See `typecheck_deps_config`'s identical canonicalization requirement:
    // `extends`-chain targets are realpath'd, so every containment check
    // must compare against a canonicalized `workspace_root`.
    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {workspace_root:?}"))?;

    let src_files = importgraph::package_source_files(walker, workspace_root, pkg)
        .with_context(|| format!("collecting first-party source files for {pkg:?}"))?;
    let mut src_rel: BTreeSet<String> = BTreeSet::new();
    for f in &src_files {
        let rel = f
            .strip_prefix(workspace_root)
            .unwrap_or(f)
            .to_string_lossy()
            .replace('\\', "/");
        src_rel.insert(rel);
    }

    // Deliberately no `package.json`-field config fallback here: see
    // `importgraph::find_nearest_package_json_field_config`'s doc for why an
    // earlier version's oxlint/eslint fallback was removed rather than kept
    // (a feature-quality M5 review finding — neither tool actually reads a
    // `package.json` field when invoked with `-c <that package.json>`, the
    // way `driver_lint.rs::run` always invokes them).
    let candidates = lint_config_candidates(linter)?;
    let config_path = importgraph::find_nearest_lint_config(workspace_root, &pkg_dir, candidates);

    let mut deps: HashMap<String, Value> = HashMap::new();
    deps.insert(
        String::new(),
        Value::List(
            src_rel
                .iter()
                .map(|p| Value::String(hbuiltins::pluginfs::file_addr(p).format()))
                .collect(),
        ),
    );

    let (config_path_rel, config_content) = match &config_path {
        Some(p) => {
            let rel = p
                .strip_prefix(workspace_root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/");
            let content =
                std::fs::read_to_string(p).with_context(|| format!("reading lint config {p:?}"))?;
            (rel, content)
        }
        None => (String::new(), String::new()),
    };

    let mut tsconfig_rel: BTreeSet<String> = BTreeSet::new();
    let mut tsconfig_path_rel = String::new();
    let mut tsconfig_content = String::new();

    if let Some(config_path) = &config_path {
        // Invariant: `config_path_rel` is the relative path of a file that
        // `find_nearest_lint_config` confirmed exists (see the `config_path`
        // match above), so it is never empty here — no need to guard the
        // insert (code-quality/feature-quality M5 review finding: an earlier
        // `!config_path_rel.is_empty()` check was dead code with no test
        // exercising the branch it guarded against).
        debug_assert!(
            !config_path_rel.is_empty(),
            "config_path is Some, so config_path_rel must be a non-empty relative path"
        );
        deps.insert(
            "config".to_string(),
            Value::List(vec![Value::String(
                hbuiltins::pluginfs::file_addr(&config_path_rel).format(),
            )]),
        );

        if linter == toolchain::ESLINT {
            // Type-aware rules: fold the tsconfig(s) named by every
            // `parserOptions.project` occurrence plus their whole `extends`
            // chain into the Input set/hash the same way `js_typecheck` does
            // — see this function's doc. Every occurrence, not just the
            // first, is resolved (code-quality M5 review finding): a
            // multi-entry flat config (e.g. a separate override block per
            // `src/**`/`test/**`, each with its own `parserOptions.project`)
            // would otherwise have every tsconfig but the first silently
            // invisible to the declared Input set/cache key.
            let projects = importgraph::detect_eslint_type_aware(config_path, &config_content)
                .with_context(|| format!("scanning {config_path:?} for parserOptions.project"))?;
            if !projects.is_empty() {
                let config_dir = config_path.parent().unwrap_or(config_path);
                let mut project_paths: Vec<PathBuf> = Vec::new();
                for project in &projects {
                    match project {
                        importgraph::EslintProjectOption::AutoDetect => {
                            project_paths.extend(importgraph::find_nearest_tsconfig(
                                workspace_root,
                                &pkg_dir,
                            ));
                        }
                        importgraph::EslintProjectOption::Paths(paths) => {
                            project_paths.extend(paths.iter().map(|p| config_dir.join(p)));
                        }
                    }
                }
                anyhow::ensure!(
                    !project_paths.is_empty(),
                    "js_lint: {config_path:?} configures a type-aware `parserOptions.project`, \
                     but no tsconfig could be found — `project: true` requires a tsconfig.json \
                     somewhere on {pkg:?}'s ancestor chain"
                );
                for leaf in &project_paths {
                    anyhow::ensure!(
                        leaf.is_file(),
                        "js_lint: {config_path:?} names tsconfig {leaf:?} via \
                         `parserOptions.project`, but it does not exist"
                    );
                    // Canonicalize: `parserOptions.project` values commonly
                    // carry a `./` prefix (`config_dir.join(p)` doesn't
                    // normalize that away), and `resolve_tsconfig_extends_chain`'s
                    // own ancestor paths are already realpath'd (see that
                    // function's doc) — comparing/stripping against a
                    // non-canonical leaf would otherwise produce a
                    // `packages/a/./tsconfig.json`-shaped rel path instead of
                    // the clean one `js_typecheck`'s identical treatment
                    // produces.
                    let leaf = leaf
                        .canonicalize()
                        .with_context(|| format!("canonicalize tsconfig {leaf:?}"))?;
                    // Hard error (never a silent same-path fallback) on
                    // workspace-root escape — a hermeticity + code-quality M5
                    // review finding: an eslint config with
                    // `parserOptions: { project: "/etc/hostname" }` (or any
                    // `../`-heavy path) previously canonicalized successfully
                    // and then had `strip_prefix(...).unwrap_or(&leaf)` fall
                    // back to the raw absolute host path instead of erroring,
                    // reading and hashing an arbitrary host file into
                    // `js_lint`'s cache key. Mirrors this same file's
                    // `types_addrs` cross-package-edge check just above and
                    // `resolve_extends_specifier`'s `canonicalize_within` in
                    // `importgraph.rs`, both of which hard-error rather than
                    // fall back.
                    let leaf_rel = leaf
                        .strip_prefix(&canonical_root)
                        .with_context(|| {
                            format!(
                                "js_lint: {config_path:?} names tsconfig {leaf:?} via \
                                 `parserOptions.project`, which resolved outside the workspace \
                                 root ({canonical_root:?}) — cannot express it as a declared \
                                 js_lint input"
                            )
                        })?
                        .to_string_lossy()
                        .replace('\\', "/");
                    if tsconfig_path_rel.is_empty() {
                        tsconfig_path_rel.clone_from(&leaf_rel);
                    }
                    if tsconfig_rel.insert(leaf_rel) {
                        let content = std::fs::read_to_string(&leaf)
                            .with_context(|| format!("reading tsconfig {leaf:?}"))?;
                        tsconfig_content.push('\n');
                        tsconfig_content.push_str(&content);
                    }
                    let chain = importgraph::resolve_tsconfig_extends_chain(&canonical_root, &leaf)
                        .with_context(|| {
                            format!(
                                "resolving tsconfig extends chain for {pkg:?}'s js_lint (via \
                                 {config_path:?})"
                            )
                        })?;
                    for ancestor in &chain {
                        let rel = ancestor
                            .strip_prefix(&canonical_root)
                            .unwrap_or(ancestor)
                            .to_string_lossy()
                            .replace('\\', "/");
                        if tsconfig_rel.insert(rel) {
                            let content = std::fs::read_to_string(ancestor).with_context(|| {
                                format!("reading extended tsconfig {ancestor:?}")
                            })?;
                            tsconfig_content.push('\n');
                            tsconfig_content.push_str(&content);
                        }
                    }
                }
            }

            // `extends`/`plugins` npm packages — resolved via the lockfile,
            // never treated as raw filesystem paths (see
            // `importgraph::extract_eslint_module_refs`'s doc).
            let names = importgraph::extract_eslint_module_refs(config_path, &config_content)
                .with_context(|| format!("scanning {config_path:?} for extends/plugins"))?;
            let mut plugin_addrs: BTreeSet<String> = BTreeSet::new();
            for name in names {
                if let Some(addr) = deps::resolve_one_dependency(
                    pkg,
                    lockfile_pkg,
                    &name,
                    &manifest,
                    lockfile,
                    resolved_graph,
                    member_addrs_by_name,
                    os,
                    arch,
                )
                .with_context(|| format!("resolving eslint config package `{name}` for js_lint"))?
                {
                    plugin_addrs.insert(addr);
                }
            }
            if !plugin_addrs.is_empty() {
                deps.insert(
                    "eslint_plugins".to_string(),
                    Value::List(plugin_addrs.into_iter().map(Value::String).collect()),
                );
            }

            // Relative-path shared eslint config files: a legacy config's own
            // relative `extends`/`plugins` entry, or a modern flat config's
            // relative `import`/`require` of a shared base config — both
            // name a local sibling file rather than an npm package, and must
            // be declared as Inputs the same way `test_deps_config` already
            // does for a shared test-runner config (a hermeticity M5 review
            // finding: editing only the referenced base config previously
            // left `js_lint`'s cache key unchanged). See
            // `importgraph::resolve_eslint_config_referenced_files`'s doc.
            let referenced_configs =
                importgraph::resolve_eslint_config_referenced_files(config_path, &config_content)
                    .with_context(|| {
                    format!("scanning {config_path:?} for referenced eslint config files")
                })?;
            let mut config_ref_addrs: BTreeSet<String> = BTreeSet::new();
            for file in referenced_configs {
                // Same hard-error-not-fallback containment discipline as the
                // `parserOptions.project` leaf just above: a referenced
                // config file that resolves outside the workspace root must
                // never be silently read/hashed as an arbitrary host file.
                let canonical_file = file
                    .canonicalize()
                    .with_context(|| format!("canonicalize referenced eslint config {file:?}"))?;
                let rel = canonical_file
                    .strip_prefix(&canonical_root)
                    .with_context(|| {
                        format!(
                            "js_lint: {config_path:?} references {file:?}, which resolved \
                             outside the workspace root ({canonical_root:?}) — cannot express \
                             it as a declared js_lint input"
                        )
                    })?
                    .to_string_lossy()
                    .replace('\\', "/");
                config_ref_addrs.insert(hbuiltins::pluginfs::file_addr(&rel).format());
            }
            if !config_ref_addrs.is_empty() {
                deps.insert(
                    "config_refs".to_string(),
                    Value::List(config_ref_addrs.into_iter().map(Value::String).collect()),
                );
            }
        }
    }

    if !tsconfig_rel.is_empty() {
        deps.insert(
            "tsconfig".to_string(),
            Value::List(
                tsconfig_rel
                    .iter()
                    .map(|p| Value::String(hbuiltins::pluginfs::file_addr(p).format()))
                    .collect(),
            ),
        );
    }

    Ok(LintDepsConfig {
        deps,
        config_path: config_path_rel,
        config_content,
        tsconfig_path: tsconfig_path_rel,
        tsconfig_content,
    })
}

/// The default npm registry tarball URL for a `(name, version)` — used when
/// the lockfile doesn't record one directly (pnpm's common case for a plain
/// registry dependency; npm's `package-lock.json` always records one, which
/// takes precedence when present). Scoped packages (`@scope/pkg`) publish
/// their tarball under the scope directory but name the file after the
/// unscoped basename only, e.g. `@esbuild/darwin-arm64@0.19.0` →
/// `.../@esbuild/darwin-arm64/-/darwin-arm64-0.19.0.tgz`.
fn default_registry_url(name: &str, version: &str) -> String {
    let basename = name.rsplit('/').next().unwrap_or(name);
    format!("https://registry.npmjs.org/{name}/-/{basename}-{version}.tgz")
}

/// Recursively enumerate every directory at or below `dir` containing a
/// lockfile file (`filename`) — every independent npm/pnpm project root in
/// the workspace, however deeply nested, unlike
/// [`Provider::find_lockfile_root`]'s ancestor-only search from one known
/// package. Continues walking *past* a found lockfile root — a heph
/// workspace can contain more than one independent project side by side or
/// nested, and this is a full-tree discovery, not a first-match search — but
/// still prunes `node_modules`/dot-dirs/`skip`, so it never descends into a
/// project's own installed dependencies looking for more.
///
/// [`Provider::find_resolved_graph_for`] runs this walk unconditionally, on
/// every call — a cache hit for one root proves nothing about whether some
/// other, not-yet-resolved root also resolves the same `(name, version)`
/// differently, so a cache-only fast path that skips this walk when a match
/// already exists would make its ambiguity check order-dependent. Its own
/// per-`Provider` result cache (`Provider::lockfile_roots_cache`) is what
/// keeps this cheap across repeated calls, not conditional skipping.
fn collect_lockfile_roots(
    walker: &CachedWalker,
    dir: &Path,
    workspace_root: &Path,
    filename: &str,
    skip: &Ignore,
    out: &mut Vec<PathBuf>,
) {
    let Ok(listing) = walker.read_dir(dir) else {
        return;
    };

    if listing
        .entries
        .iter()
        .any(|e| e.kind == EntryKind::File && e.name == filename)
    {
        out.push(dir.to_path_buf());
    }

    for entry in &listing.entries {
        if entry.kind != EntryKind::Dir {
            continue;
        }
        if is_skipped_dir_name(&entry.name) {
            continue;
        }
        let entry_path = dir.join(&entry.name);
        let rel = entry_path
            .strip_prefix(workspace_root)
            .unwrap_or(&entry_path);
        if skip.prune_dir(&entry_path, rel) {
            continue;
        }
        collect_lockfile_roots(walker, &entry_path, workspace_root, filename, skip, out);
    }
}

/// Recursively enumerate `package.json`-anchored directories at or below
/// `dir`. Unlike the Go plugin's `go.mod` walk, there is no "under an
/// ancestor" flag to propagate: every `package.json` directory is its own
/// self-contained package, so each directory is checked independently and the
/// walk always continues into subdirectories (bounded only by `skip` and the
/// hardcoded `node_modules`/dot-dir prune).
///
/// Reads each directory once through the shared walker (an unchanged tree
/// skips the `readdir` syscall) and detects `package.json` presence off that
/// same listing rather than a second `stat` per directory.
fn collect_js_packages(
    walker: &CachedWalker,
    dir: &Path,
    workspace_root: &Path,
    skip: &Ignore,
    result: &mut Vec<anyhow::Result<PkgBuf>>,
) {
    let listing = match walker.read_dir(dir) {
        Ok(l) => l,
        Err(e) => {
            result.push(Err(e.context(format!("read_dir {}", dir.display()))));
            return;
        }
    };

    let has_package_json = listing
        .entries
        .iter()
        .any(|e| e.kind == EntryKind::File && e.name == PACKAGE_JSON);
    if has_package_json {
        let rel = dir.strip_prefix(workspace_root).unwrap_or(dir);
        // A package identifier, so `to_str` rather than a lossy render that
        // could fold two distinct directories onto one `\u{FFFD}` name — see
        // the same check in the Go/buildfile providers. Unreachable while
        // every walked component comes from `CachedWalker::read_dir`, which
        // rejects non-UTF-8 names; kept so the invariant is asserted rather
        // than assumed.
        match rel.to_str() {
            Some(pkg) => result.push(Ok(PkgBuf::from(pkg))),
            None => {
                result.push(Err(anyhow::anyhow!(
                    "package path is not valid UTF-8: '{}' (under workspace root '{}')",
                    dir.display(),
                    workspace_root.display()
                )));
                return;
            }
        }
    }

    for entry in &listing.entries {
        if entry.kind != EntryKind::Dir {
            continue;
        }
        if is_skipped_dir_name(&entry.name) {
            continue;
        }
        let entry_path = dir.join(&entry.name);
        let rel = entry_path
            .strip_prefix(workspace_root)
            .unwrap_or(&entry_path);
        if skip.prune_dir(&entry_path, rel) {
            continue;
        }
        collect_js_packages(walker, &entry_path, workspace_root, skip, result);
    }
}

/// Read a package's own `"name"` field, for the `js_package_info` target
/// config. Blocking (sync file IO); callers route it through
/// `hcore::blocking::run`.
fn read_package_name_blocking(package_json: &Path) -> anyhow::Result<String> {
    workspace::read_package_name(package_json)
}

/// Resolve `member_addrs_by_name` (workspace-member package name →
/// `package_info` target addr string) — the identical workspace-member
/// discovery `Provider::deps_config`/`typecheck_config`/`test_config`/
/// `lint_config` each need inside their own `hcore::blocking::run` closure,
/// so an import that never resolved on disk can still be attributed to a
/// workspace sibling by name. Factored out to a free function (not `&self`)
/// for the same reason those closures already redo this discovery rather
/// than call `Provider::workspace_members` directly: every call site already
/// runs on the blocking pool, and `&self` can't be moved into a `'static`
/// blocking job.
fn member_addrs_by_name_blocking(
    walker: &CachedWalker,
    workspace_root: &Path,
    skip: &Ignore,
    pkgmanager: PkgManager,
) -> anyhow::Result<BTreeMap<String, String>> {
    let patterns = match pkgmanager {
        PkgManager::Npm => workspace::read_npm_workspace_globs(workspace_root)?,
        PkgManager::Pnpm => workspace::read_pnpm_workspace_globs(workspace_root)?,
    };
    if patterns.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut packages = Vec::new();
    collect_js_packages(walker, workspace_root, workspace_root, skip, &mut packages);
    let packages: Vec<PkgBuf> = packages.into_iter().collect::<anyhow::Result<_>>()?;
    Ok(
        workspace::resolve_members(workspace_root, &packages, &patterns)?
            .into_iter()
            .map(|m| (m.name, m.addr.format()))
            .collect(),
    )
}

fn empty_list_responses() -> Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send> {
    Box::new(std::iter::empty())
}

fn empty_list_package_responses()
-> Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send> {
    Box::new(std::iter::empty())
}

impl ProviderTrait for Provider {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: "js".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        req: ListRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListResponse>> + Send>>>
    {
        Box::pin(async move {
            if self
                .skip
                .prunes_package(&self.workspace_root, Path::new(req.package.as_str()))
            {
                return Ok(empty_list_responses());
            }

            let package_json = self
                .workspace_root
                .join(req.package.as_str())
                .join(PACKAGE_JSON);
            let exists =
                hcore::blocking::run(enclose!((package_json) move || package_json.is_file())).await;
            if !exists {
                return Ok(empty_list_responses());
            }

            let addr = Addr::new(
                req.package.clone(),
                PACKAGE_INFO_TARGET.to_string(),
                Default::default(),
            );
            let mut responses: Vec<anyhow::Result<ListResponse>> = vec![Ok(ListResponse { addr })];

            // `js_lint` is a second per-package target kind alongside
            // `package_info` — always present once the package exists (no
            // additional discovery needed, unlike `js_test`'s per-file
            // globbing), same as `js_typecheck` would be if it were listed
            // here too.
            responses.push(Ok(ListResponse {
                addr: Addr::new(
                    req.package.clone(),
                    LINT_TARGET.to_string(),
                    Default::default(),
                ),
            }));

            // The on-disk `node_modules` sync target — see
            // `NODE_MODULES_SYNC_TARGET`'s doc. Always listed alongside
            // `package_info`; an empty dependency set just produces an empty
            // sync (harmless, never executed unless requested directly).
            responses.push(Ok(ListResponse {
                addr: Addr::new(
                    req.package.clone(),
                    NODE_MODULES_SYNC_TARGET.to_string(),
                    Default::default(),
                ),
            }));

            // One `js_test` target per matched test file — the milestone's
            // stated per-test-file (not per-package) granularity; see
            // `driver_test.rs` module docs. Test discovery is an optional,
            // additive listing on top of the always-present
            // `PACKAGE_INFO_TARGET` entry above: a bad `test_glob` or an
            // FS-walk error under this one package must not take
            // `package_info` (and every other target kind that lives in this
            // package) down with it, so a failure here is surfaced as its
            // own per-entry error — mirroring `collect_js_packages`'s
            // identical per-entry error handling — rather than propagated
            // with `?`.
            match self.discover_test_files(&req.package).await {
                Ok(test_files) => {
                    for file in test_files {
                        let mut args = BTreeMap::new();
                        args.insert("file".to_string(), file);
                        responses.push(Ok(ListResponse {
                            addr: Addr::new(req.package.clone(), TEST_TARGET.to_string(), args),
                        }));
                    }
                }
                Err(e) => {
                    responses.push(Err(e.context(format!(
                        "discovering js_test files for {}",
                        req.package.as_str()
                    ))));
                }
            }

            // A default (bare, no `format=`/`target=`/`entry=` addr args —
            // resolved to `esm`/`node`/`package.json`'s own `"main"` by
            // `Provider::get`) `js_bundle` target, listed only when the
            // package actually has a usable default entry point — same
            // "optional, additive listing" shape `js_test`'s discovery has
            // just above; an entry-resolution failure here must not take
            // `package_info` (or any other target kind in this package) down
            // with it. Addressing a non-default variant/entry
            // (`//pkg:js_bundle@format=cjs`, `@entry=…`) always works via
            // `Provider::get` regardless of what's listed here — mirrors
            // `js_test`'s identical "not every valid addr is enumerated"
            // shape for an explicit `file=` override.
            match self.default_entry_for_package(&req.package).await {
                Ok(Some(_)) => {
                    responses.push(Ok(ListResponse {
                        addr: Addr::new(
                            req.package.clone(),
                            BUNDLE_TARGET.to_string(),
                            Default::default(),
                        ),
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    responses.push(Err(e.context(format!(
                        "resolving the default js_bundle entry point for {}",
                        req.package.as_str()
                    ))));
                }
            }

            Ok(Box::new(responses.into_iter())
                as Box<
                    dyn Iterator<Item = anyhow::Result<ListResponse>> + Send,
                >)
        })
    }

    fn list_packages<'a>(
        &'a self,
        req: ListPackagesRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<
        'a,
        anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<ListPackageResponse>> + Send>>,
    > {
        Box::pin(async move {
            let prefix = req.prefix.as_str();
            let search_dir = if prefix.is_empty() {
                self.workspace_root.clone()
            } else {
                self.workspace_root.join(prefix)
            };
            if !search_dir.exists() {
                return Ok(empty_list_package_responses());
            }

            let prefix = prefix.to_string();
            let packages = hcore::blocking::run(enclose!((self.workspace_root => workspace_root, self.skip => skip, self.walker => walker, prefix) move || {
                let search_dir = if prefix.is_empty() {
                    workspace_root.clone()
                } else {
                    workspace_root.join(&prefix)
                };
                let mut result = Vec::new();
                collect_js_packages(&walker, &search_dir, &workspace_root, &skip, &mut result);
                result
            }))
            .await;

            let responses: Vec<anyhow::Result<ListPackageResponse>> = packages
                .into_iter()
                .map(|r| r.map(|pkg| ListPackageResponse { pkg }))
                .collect();
            Ok(Box::new(responses.into_iter())
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
            // Third-party `js_install` targets live in a synthetic
            // `@heph/js/thirdparty/…` package namespace, never under a real
            // discovered package — checked first so it short-circuits
            // before the `skip`/`package.json` checks below, neither of
            // which apply to it.
            if req.addr.name == thirdparty::INSTALL_TARGET
                && thirdparty::parse_thirdparty_pkg(req.addr.package.as_str()).is_some()
            {
                let target_spec = self
                    .thirdparty_install_spec(&req.addr)
                    .await
                    .map_err(GetError::Other)?;
                return Ok(GetResponse { target_spec });
            }

            // Same treatment for the relocated-`node_modules` namespace —
            // see `Provider::node_modules_group_spec`'s doc. Also synthetic,
            // also checked before any real-package handling below.
            if req.addr.name == thirdparty::NODE_MODULES_TARGET
                && req.addr.package.as_str() == thirdparty::NODE_MODULES_PKG
            {
                let Some(relocation) = thirdparty::parse_node_modules_addr(&req.addr) else {
                    return Err(GetError::Other(anyhow::anyhow!(
                        "malformed js node_modules-relocation addr {}: missing a required arg \
                         (expected pkg/local/name/version/os/arch)",
                        req.addr.format()
                    )));
                };
                return Ok(GetResponse {
                    target_spec: self.node_modules_group_spec(&req.addr, &relocation),
                });
            }

            // `js_typecheck` is a second per-package target kind alongside
            // `package_info`, checked before the `PACKAGE_INFO_TARGET` gate
            // below so it isn't rejected as an unknown target name.
            if req.addr.name == TYPECHECK_TARGET {
                if self
                    .skip
                    .prunes_package(&self.workspace_root, Path::new(req.addr.package.as_str()))
                {
                    return Err(GetError::NotFound);
                }
                let package_json = self
                    .workspace_root
                    .join(req.addr.package.as_str())
                    .join(PACKAGE_JSON);
                if !package_json.is_file() {
                    return Err(GetError::NotFound);
                }
                let config = self
                    .typecheck_config(&req.addr.package)
                    .await
                    .with_context(|| {
                        format!("resolving js_typecheck config for {}", req.addr.format())
                    })
                    .map_err(GetError::Other)?;
                return Ok(GetResponse {
                    target_spec: TargetSpec {
                        addr: req.addr.clone(),
                        driver: "js_typecheck".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                        approval: Default::default(),
                    },
                });
            }

            // `js_lint` is a third per-package target kind alongside
            // `package_info`/`js_typecheck`, checked before the
            // `PACKAGE_INFO_TARGET` gate below for the same reason
            // `TYPECHECK_TARGET` is.
            if req.addr.name == LINT_TARGET {
                if self
                    .skip
                    .prunes_package(&self.workspace_root, Path::new(req.addr.package.as_str()))
                {
                    return Err(GetError::NotFound);
                }
                let package_json = self
                    .workspace_root
                    .join(req.addr.package.as_str())
                    .join(PACKAGE_JSON);
                if !package_json.is_file() {
                    return Err(GetError::NotFound);
                }
                let config = self
                    .lint_config(&req.addr.package)
                    .await
                    .with_context(|| format!("resolving js_lint config for {}", req.addr.format()))
                    .map_err(GetError::Other)?;
                return Ok(GetResponse {
                    target_spec: TargetSpec {
                        addr: req.addr.clone(),
                        driver: "js_lint".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                        approval: Default::default(),
                    },
                });
            }

            // The on-disk `node_modules` sync target — see
            // `NODE_MODULES_SYNC_TARGET`'s doc. Checked before the
            // `PACKAGE_INFO_TARGET` gate below for the same reason
            // `TYPECHECK_TARGET` is.
            if req.addr.name == NODE_MODULES_SYNC_TARGET {
                if self
                    .skip
                    .prunes_package(&self.workspace_root, Path::new(req.addr.package.as_str()))
                {
                    return Err(GetError::NotFound);
                }
                let package_json = self
                    .workspace_root
                    .join(req.addr.package.as_str())
                    .join(PACKAGE_JSON);
                if !package_json.is_file() {
                    return Err(GetError::NotFound);
                }
                let target_spec = self
                    .node_modules_sync_spec(&req.addr.package)
                    .await
                    .with_context(|| {
                        format!(
                            "resolving node_modules sync config for {}",
                            req.addr.format()
                        )
                    })
                    .map_err(GetError::Other)?;
                return Ok(GetResponse { target_spec });
            }

            // `js_test` is a fourth per-package-*file* target kind: one addr
            // per matched test file, distinguished by the `file` addr arg —
            // see `driver_test.rs` module docs. Checked before the
            // `PACKAGE_INFO_TARGET` gate below for the same reason
            // `TYPECHECK_TARGET` is.
            if req.addr.name == TEST_TARGET {
                if self
                    .skip
                    .prunes_package(&self.workspace_root, Path::new(req.addr.package.as_str()))
                {
                    return Err(GetError::NotFound);
                }
                let package_json = self
                    .workspace_root
                    .join(req.addr.package.as_str())
                    .join(PACKAGE_JSON);
                if !package_json.is_file() {
                    return Err(GetError::NotFound);
                }
                let test_file = req.addr.args.get("file").cloned().ok_or_else(|| {
                    GetError::Other(anyhow::anyhow!(
                        "js_test addr {} is missing its required `file` arg",
                        req.addr.format()
                    ))
                })?;
                // Validated *before* ever touching the filesystem: an
                // absolute or `..`-escaping `file` arg must never reach
                // `workspace_root.join(...)` below — see `reject_path_escape`'s
                // doc for why (a code-quality review BLOCKER — `Path::join`
                // silently replaces the base for an absolute argument).
                reject_path_escape("file arg", &test_file).map_err(GetError::Other)?;
                if !path_under_package(req.addr.package.as_str(), &test_file) {
                    return Err(GetError::Other(anyhow::anyhow!(
                        "js_test addr {} names file {test_file:?} outside its own package {:?}",
                        req.addr.format(),
                        req.addr.package.as_str()
                    )));
                }
                let test_file_abs = self.workspace_root.join(&test_file);
                let is_file = hcore::blocking::run(enclose!((test_file_abs) move || {
                    test_file_abs.is_file()
                }))
                .await;
                if !is_file {
                    return Err(GetError::NotFound);
                }
                let config = self
                    .test_config(&req.addr.package, &test_file)
                    .await
                    .with_context(|| format!("resolving js_test config for {}", req.addr.format()))
                    .map_err(GetError::Other)?;
                return Ok(GetResponse {
                    target_spec: TargetSpec {
                        addr: req.addr.clone(),
                        driver: "js_test".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                        approval: Default::default(),
                    },
                });
            }

            // `js_bundle` (M6) is a fifth per-package target kind: one addr
            // per package, with `format`/`target`/`entry` addr args scoped
            // to it alone — see `driver_bundle.rs` module docs for why these
            // are plain addr args with a flat default, not a
            // `provider_state`/ancestry-resolved variant. Checked before the
            // `PACKAGE_INFO_TARGET` gate below for the same reason
            // `TYPECHECK_TARGET`/`LINT_TARGET`/`TEST_TARGET` are.
            if req.addr.name == BUNDLE_TARGET {
                if self
                    .skip
                    .prunes_package(&self.workspace_root, Path::new(req.addr.package.as_str()))
                {
                    return Err(GetError::NotFound);
                }
                let package_json = self
                    .workspace_root
                    .join(req.addr.package.as_str())
                    .join(PACKAGE_JSON);
                if !package_json.is_file() {
                    return Err(GetError::NotFound);
                }

                let format = req
                    .addr
                    .args
                    .get("format")
                    .cloned()
                    .unwrap_or_else(|| "esm".to_string());
                if format != "esm" && format != "cjs" {
                    return Err(GetError::Other(anyhow::anyhow!(
                        "js_bundle addr {} has unsupported `format` arg {format:?} — expected \
                         \"esm\" or \"cjs\"",
                        req.addr.format()
                    )));
                }
                let target_env = req
                    .addr
                    .args
                    .get("target")
                    .cloned()
                    .unwrap_or_else(|| "node".to_string());
                if target_env != "node" && target_env != "browser" {
                    return Err(GetError::Other(anyhow::anyhow!(
                        "js_bundle addr {} has unsupported `target` arg {target_env:?} — \
                         expected \"node\" or \"browser\"",
                        req.addr.format()
                    )));
                }

                let entry_file = match req.addr.args.get("entry").cloned() {
                    Some(entry) => {
                        // Validated *before* ever touching the filesystem —
                        // see `js_test`'s identical `file` arg handling
                        // above for why (a code-quality review BLOCKER).
                        reject_path_escape("entry arg", &entry).map_err(GetError::Other)?;
                        if !path_under_package(req.addr.package.as_str(), &entry) {
                            return Err(GetError::Other(anyhow::anyhow!(
                                "js_bundle addr {} names entry {entry:?} outside its own \
                                 package {:?}",
                                req.addr.format(),
                                req.addr.package.as_str()
                            )));
                        }
                        let entry_abs = self.workspace_root.join(&entry);
                        let is_file = hcore::blocking::run(enclose!((entry_abs) move || {
                            entry_abs.is_file()
                        }))
                        .await;
                        if !is_file {
                            return Err(GetError::NotFound);
                        }
                        entry
                    }
                    None => match self
                        .default_entry_for_package(&req.addr.package)
                        .await
                        .with_context(|| {
                            format!(
                                "resolving the default js_bundle entry point for {}",
                                req.addr.format()
                            )
                        })
                        .map_err(GetError::Other)?
                    {
                        Some(entry) => entry,
                        None => {
                            return Err(GetError::Other(anyhow::anyhow!(
                                "js_bundle addr {} has no entry point: package.json has no \
                                 usable \"main\" field (or it names a file that doesn't exist) \
                                 and no `entry=` addr arg was given",
                                req.addr.format()
                            )));
                        }
                    },
                };

                let config = self
                    .bundle_config(&req.addr.package, &entry_file, &format, &target_env)
                    .await
                    .with_context(|| {
                        format!("resolving js_bundle config for {}", req.addr.format())
                    })
                    .map_err(GetError::Other)?;
                return Ok(GetResponse {
                    target_spec: TargetSpec {
                        addr: req.addr.clone(),
                        driver: "js_bundle".to_string(),
                        config,
                        labels: vec![],
                        transitive: Default::default(),
                        approval: Default::default(),
                    },
                });
            }

            if req.addr.name != PACKAGE_INFO_TARGET {
                return Err(GetError::NotFound);
            }
            if self
                .skip
                .prunes_package(&self.workspace_root, Path::new(req.addr.package.as_str()))
            {
                return Err(GetError::NotFound);
            }

            let package_json = self
                .workspace_root
                .join(req.addr.package.as_str())
                .join(PACKAGE_JSON);
            if !package_json.is_file() {
                return Err(GetError::NotFound);
            }

            let name = hcore::blocking::run(enclose!((package_json) move || {
                read_package_name_blocking(&package_json)
            }))
            .await
            .with_context(|| format!("reading package name for {}", req.addr.format()))
            .map_err(GetError::Other)?;

            let deps_config = self
                .deps_config(&req.addr.package)
                .await
                .with_context(|| format!("resolving dependencies of {}", req.addr.format()))
                .map_err(GetError::Other)?;

            let mut config: HashMap<String, Value> = HashMap::new();
            config.insert("name".to_string(), Value::String(name));
            config.insert("deps".to_string(), deps_config);

            Ok(GetResponse {
                target_spec: TargetSpec {
                    addr: req.addr.clone(),
                    driver: "js_package_info".to_string(),
                    config,
                    labels: vec![],
                    transitive: Default::default(),
                    approval: Default::default(),
                },
            })
        })
    }

    fn probe<'a>(
        &'a self,
        _req: ProbeRequest,
        _ctoken: &'a (dyn Cancellable + Send + Sync),
    ) -> BoxFuture<'a, anyhow::Result<ProbeResponse>> {
        // No provider-state axis exists yet (no variants in M0's scope — see
        // ai-docs/js-plugin-plan.md's "Variants" section: module format/target
        // env only apply to bundle targets, a later milestone).
        Box::pin(async move { Ok(ProbeResponse { states: vec![] }) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::hasync::StdCancellationToken;
    use hplugin::provider::{ListPackagesRequest, ListRequest, NoopExecutor};
    use std::fs;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    fn ctoken() -> StdCancellationToken {
        StdCancellationToken::new()
    }

    /// `GetError` implements neither `Debug` nor `Display` (it wraps a
    /// non-`Debug` `TargetSpec`/`GetResponse` on the `Ok` side via
    /// `Result::expect_err`'s bound, and its own `Other` variant's inner
    /// `anyhow::Error` is the only formattable part) — assert on a
    /// `Provider::get` failure by matching `GetError::Other` directly and
    /// formatting its inner error, rather than via `.expect_err()`/`{err:#}`
    /// the way a plain `anyhow::Result` failure elsewhere in this module can.
    fn expect_get_other_error(result: Result<GetResponse, GetError>, msg: &str) -> String {
        match result {
            Err(GetError::Other(e)) => format!("{e:#}"),
            Err(GetError::NotFound) => panic!("{msg}: got GetError::NotFound, expected Other(_)"),
            Ok(_) => panic!("{msg}: got Ok(_), expected an error"),
        }
    }

    /// Synthetic pnpm workspace: root + two members, one nested `node_modules`
    /// package.json (must never surface as a package), plus a dep dir the
    /// glob doesn't cover.
    fn pnpm_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\n",
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        // A dependency vendored under node_modules ships its own package.json
        // — this must never be treated as a workspace member (nor even listed
        // as a discoverable package at all).
        write(
            dir.path(),
            "packages/a/node_modules/dep/package.json",
            r#"{"name": "dep"}"#,
        );
        dir
    }

    fn npm_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": ["packages/*"]}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        write(
            dir.path(),
            "packages/a/node_modules/dep/package.json",
            r#"{"name": "dep"}"#,
        );
        dir
    }

    // ---- graph_cache: Provider::import_graph memoization ----
    //
    // M2/M4 review-flagged perf issue: `deps_config`/`typecheck_config`/
    // `test_config` each independently called
    // `importgraph::build_package_import_graph` — a full oxc_parser parse +
    // oxc_resolver resolve of every first-party file in the package — from
    // scratch, on every single `Provider::get` call. `Provider::import_graph`
    // fixes this with a per-package cache; these tests prove it actually
    // memoizes (the same "prove it, don't just structurally add it" gap the
    // M2 review flagged for `ResolveCache`), not merely that the type exists.

    /// `deps_config` (the real, public, tsc/vitest-free entry point) followed
    /// by a direct `Provider::import_graph` call — standing in for what
    /// `typecheck_config`/`test_config` each do internally as the very first
    /// thing they reach for the same package (see both methods' bodies) —
    /// must build the underlying import graph exactly once between them.
    ///
    /// `typecheck_config`/`test_config` are not driven directly here because
    /// both require a real host `tsc`/`vitest`/`jest` binary this environment
    /// does not provision (see `get_resolves_js_typecheck_target_end_to_end`'s
    /// `#[ignore]` for the identical reason); since both route through this
    /// exact `import_graph` method before ever touching their own
    /// tool-specific logic, proving the cache is shared here proves the fix
    /// for all three real callers.
    #[tokio::test]
    async fn import_graph_is_shared_across_independent_callers() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let pkg = PkgBuf::from("packages/a");

        provider
            .deps_config(&pkg)
            .await
            .expect("deps_config for packages/a");
        assert_eq!(
            provider
                .graph_build_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "deps_config's own import_graph call must have built the graph exactly once"
        );

        provider
            .import_graph(&pkg)
            .await
            .expect("import_graph for packages/a");
        assert_eq!(
            provider
                .graph_build_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a second caller for the SAME package must hit the cache, not rebuild"
        );
    }

    /// Different packages must not share one build slot — proving the cache
    /// is keyed per-package rather than a single memo that would happen to
    /// look "correct" for one caller. Also exercises that re-fetching an
    /// already-cached package doesn't rebuild, independent of insertion order.
    #[tokio::test]
    async fn import_graph_is_not_shared_across_different_packages() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export const y = 2;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);

        provider
            .import_graph(&PkgBuf::from("packages/a"))
            .await
            .expect("import_graph for packages/a");
        provider
            .import_graph(&PkgBuf::from("packages/b"))
            .await
            .expect("import_graph for packages/b");
        assert_eq!(
            provider
                .graph_build_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "two different packages must each get their own build"
        );

        provider
            .import_graph(&PkgBuf::from("packages/a"))
            .await
            .expect("import_graph for packages/a again");
        assert_eq!(
            provider
                .graph_build_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "re-fetching an already-cached package must not rebuild"
        );
    }

    /// The two tests above only ever `.await` one call before issuing the
    /// next — they prove "a second *sequential* call for the same package
    /// hits the cache," not "two *simultaneously racing* callers for the
    /// same package coalesce onto one build," which is the actual property
    /// `graph_cache`'s doc comment claims and every one of this crate's three
    /// M5 reviews flagged as asserted-but-untested. Drive many concurrent
    /// `import_graph` calls for the identical package via `join_all` and
    /// assert exactly one build happened.
    #[tokio::test]
    async fn import_graph_concurrent_callers_for_the_same_package_coalesce_onto_one_build() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let pkg = PkgBuf::from("packages/a");

        let futures = (0..16).map(|_| provider.import_graph(&pkg));
        let results = futures::future::join_all(futures).await;
        for r in results {
            r.expect("concurrent import_graph call for packages/a");
        }

        assert_eq!(
            provider
                .graph_build_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "many callers racing the SAME package must single-flight onto exactly one build"
        );
    }

    /// code-quality/feature-quality M5 review finding: `member_addrs_by_name_blocking`
    /// (a full recursive workspace walk + `package.json` parse of every
    /// package, plus a glob-match) was previously redone from scratch inside
    /// each of `deps_config`/`typecheck_config`/`test_config`/`lint_config`'s
    /// own blocking closure on every single call — the identical
    /// "recompute-on-every-call" shape `graph_cache` fixes for the import
    /// graph, left unfixed for workspace-member discovery. `member_addrs_cache`
    /// backs `Provider::member_addrs_by_name` with a `tokio::sync::OnceCell`
    /// exactly like `tsc_cache`/`testrunner_cache`/`linter_cache`; two calls
    /// on the same `Provider` must return the identical cached `Arc`, not two
    /// independently-built maps.
    #[tokio::test]
    async fn member_addrs_by_name_is_cached_across_calls_on_the_same_provider() {
        let dir = pnpm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);

        let first = provider
            .member_addrs_by_name()
            .await
            .expect("member_addrs_by_name first call");
        let second = provider
            .member_addrs_by_name()
            .await
            .expect("member_addrs_by_name second call");

        assert!(
            Arc::ptr_eq(&first, &second),
            "a second call must reuse the cached Arc, not rebuild the workspace-member map"
        );
        assert!(!first.is_empty(), "sanity: the pnpm fixture has members");
    }

    #[test]
    fn pnpm_workspace_members_excludes_node_modules() {
        let dir = pnpm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let mut members = provider.workspace_members().expect("resolve members");
        members.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn npm_workspace_members_excludes_node_modules() {
        let dir = npm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let mut members = provider.workspace_members().expect("resolve members");
        members.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn packages_under_discovers_every_package_json_including_root() {
        let dir = pnpm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let mut packages: Vec<String> = provider
            .packages_under("")
            .expect("collect packages")
            .into_iter()
            .map(|p| p.as_str().to_string())
            .collect();
        packages.sort();
        // Root ("") + both workspace members. node_modules/dep is excluded
        // even though `packages_under` doesn't filter by workspace glob — the
        // walk itself never descends into node_modules.
        assert_eq!(
            packages,
            vec![
                "".to_string(),
                "packages/a".to_string(),
                "packages/b".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn list_packages_provider_trait_excludes_node_modules() {
        let dir = pnpm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let ct = ctoken();
        let iter = provider
            .list_packages(
                ListPackagesRequest {
                    prefix: PkgBuf::from(""),
                },
                &ct,
            )
            .await
            .expect("list_packages");
        let mut pkgs: Vec<String> = iter
            .map(|r| r.expect("no per-entry error").pkg.as_str().to_string())
            .collect();
        pkgs.sort();
        assert_eq!(
            pkgs,
            vec![
                "".to_string(),
                "packages/a".to_string(),
                "packages/b".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn list_provider_trait_lists_package_info_target() {
        let dir = pnpm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let iter = provider
            .list(
                ListRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from("packages/a"),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("list");
        let addrs: Vec<Addr> = iter.map(|r| r.expect("no per-entry error").addr).collect();
        // `package_info` + `js_lint` + `node_modules` — see `Provider::list`'s
        // doc for why `js_lint`/`node_modules` are listed unconditionally
        // alongside `package_info` (no `js_test`-style per-file discovery
        // needed for either).
        assert_eq!(addrs.len(), 3);
        assert_eq!(addrs[0].name, PACKAGE_INFO_TARGET);
        assert_eq!(addrs[0].package.as_str(), "packages/a");
        assert!(
            addrs.iter().any(|a| a.name == LINT_TARGET),
            "js_lint must be listed alongside package_info: {addrs:?}"
        );
        assert!(
            addrs.iter().any(|a| a.name == NODE_MODULES_SYNC_TARGET),
            "node_modules sync must be listed alongside package_info: {addrs:?}"
        );
    }

    #[tokio::test]
    async fn list_provider_trait_empty_for_non_package_dir() {
        let dir = pnpm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let iter = provider
            .list(
                ListRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from("packages"),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("list");
        assert_eq!(iter.count(), 0);
    }

    #[tokio::test]
    async fn get_resolves_package_info_target_with_name() {
        let dir = pnpm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = Addr::new(
            PkgBuf::from("packages/a"),
            PACKAGE_INFO_TARGET.to_string(),
            Default::default(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get");
        assert_eq!(resp.target_spec.driver, "js_package_info");
        assert_eq!(
            resp.target_spec.config.get("name"),
            Some(&Value::String("a".to_string()))
        );
    }

    #[tokio::test]
    async fn get_not_found_for_unknown_target_name() {
        let dir = pnpm_fixture();
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = Addr::new(
            PkgBuf::from("packages/a"),
            "js_install".to_string(),
            Default::default(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        // `GetResponse` carries no `Debug` impl, so assert on a bool rather
        // than `expect_err`/`unwrap_err`.
        assert!(
            matches!(result, Err(GetError::NotFound)),
            "js_install does not exist yet in M0 — expected GetError::NotFound"
        );
    }

    #[test]
    fn no_workspace_config_yields_no_members() {
        // Plain npm package.json with no `workspaces` field: not a monorepo,
        // zero workspace members (but still a discoverable package).
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let members = provider.workspace_members().expect("resolve members");
        assert!(members.is_empty());
    }

    // ---- M1: end-to-end lockfile-driven dependency wiring ----
    //
    // A synthetic pnpm workspace and a synthetic npm workspace, each with a
    // real lockfile on disk, driven through the actual `Provider::get` +
    // `JsInstallDriver::parse` pipeline — not just the unit-level
    // `deps`/`lockfile` tests above. See `ai-docs/js-plugin-plan.md`'s M1
    // milestone note and this task's required assertions: (a) correct
    // per-package target addrs, (b) the `js_install` cache key tracks the
    // lockfile entry, (c) an unallowlisted install script fails loudly.

    use crate::pluginjs::JsInstallDriver;
    use hdriver_support::driver_managed::ManagedDriver;

    fn npm_e2e_fixture(lodash_integrity: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": ["packages/*"]}"#,
        );
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(
            dir.path(),
            "package-lock.json",
            &format!(
                r#"{{
                    "lockfileVersion": 3,
                    "packages": {{
                        "": {{ "name": "root", "workspaces": ["packages/*"] }},
                        "packages/a": {{ "name": "a", "dependencies": {{ "lodash": "^4.17.21" }} }},
                        "node_modules/lodash": {{
                            "version": "4.17.21",
                            "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                            "integrity": "{lodash_integrity}"
                        }}
                    }}
                }}"#
            ),
        );
        dir
    }

    fn pnpm_e2e_fixture(lodash_integrity: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "pnpm-workspace.yaml",
            "packages:\n  - packages/*\n",
        );
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(
            dir.path(),
            "pnpm-lock.yaml",
            &format!(
                "lockfileVersion: '9.0'\n\
                 importers:\n\
                 \x20\x20.: {{}}\n\
                 \x20\x20packages/a:\n\
                 \x20\x20\x20\x20dependencies:\n\
                 \x20\x20\x20\x20\x20\x20lodash:\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20specifier: ^4.17.21\n\
                 \x20\x20\x20\x20\x20\x20\x20\x20version: 4.17.21\n\
                 packages:\n\
                 \x20\x20lodash@4.17.21:\n\
                 \x20\x20\x20\x20resolution: {{integrity: {lodash_integrity}}}\n\
                 snapshots:\n\
                 \x20\x20lodash@4.17.21: {{}}\n"
            ),
        );
        dir
    }

    async fn get_deps_addrs(provider: &Provider, pkg: &str) -> Vec<String> {
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(
                        PkgBuf::from(pkg),
                        PACKAGE_INFO_TARGET.to_string(),
                        Default::default(),
                    ),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get package_info");
        match resp.target_spec.config.get("deps") {
            Some(Value::Map(groups)) => groups
                .values()
                .flat_map(|v| match v {
                    Value::List(items) => items
                        .iter()
                        .map(|i| match i {
                            Value::String(s) => s.clone(),
                            other => panic!("expected string addr, got {other:?}"),
                        })
                        .collect::<Vec<_>>(),
                    other => panic!("expected list, got {other:?}"),
                })
                .collect(),
            other => panic!("expected `deps` map, got {other:?}"),
        }
    }

    /// (a) A workspace member's third-party dependency becomes a correctly
    /// addressed `js_install` target — for both managers.
    #[tokio::test]
    async fn npm_e2e_wires_third_party_dep_to_js_install_addr() {
        let dir = npm_e2e_fixture("sha512-abc");
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let addrs = get_deps_addrs(&provider, "packages/a").await;
        assert_eq!(addrs.len(), 1);
        // The relocated `node_modules` group addr, not `js_install`'s own
        // raw addr — see `thirdparty::node_modules_addr`'s doc.
        assert!(addrs[0].contains("@heph/js/node_modules"), "{}", addrs[0]);
        assert!(addrs[0].contains("name=lodash"), "{}", addrs[0]);
        assert!(addrs[0].contains("version=4.17.21"), "{}", addrs[0]);
        assert!(addrs[0].contains("pkg=packages/a"), "{}", addrs[0]);
    }

    /// Stands in for a real npm tarball extraction (no network needed) in
    /// the `group`-relocation e2e tests below — everything downstream of
    /// this (the `Provider::get` dispatch, the `strip_prefix`/`prefix`
    /// transform) is the real production code path.
    struct FakeInstallArtifact {
        paths: Vec<String>,
    }
    impl hcore::hartifactcontent::Content for FakeInstallArtifact {
        fn reader(&self) -> anyhow::Result<Box<dyn std::io::Read>> {
            anyhow::bail!("not used")
        }
        fn walk(
            &self,
        ) -> anyhow::Result<
            Box<dyn Iterator<Item = anyhow::Result<hcore::hartifactcontent::WalkEntry>> + '_>,
        > {
            Ok(Box::new(self.paths.iter().map(|p| {
                Ok(hcore::hartifactcontent::WalkEntry {
                    path: PathBuf::from(p),
                    kind: hcore::hartifactcontent::WalkEntryKind::File {
                        data: Box::new(std::io::Cursor::new(Vec::new())),
                        x: false,
                        size: 0,
                    },
                })
            })))
        }
        fn hashout(&self) -> anyhow::Result<String> {
            Ok("fake-install-hash".to_string())
        }
        fn entry_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
            Ok(self.paths.iter().map(PathBuf::from).collect())
        }
    }

    /// The materialization gap this whole fix closes, proven end to end —
    /// not just that the right addr is *declared*, but that resolving it
    /// through the real `group` driver actually relocates the underlying
    /// `js_install` download's files to `<consuming_pkg>/node_modules/<name>/…`,
    /// the only path Node's own module resolution ever looks at.
    #[tokio::test]
    async fn npm_e2e_third_party_dep_materializes_at_consuming_pkg_node_modules() {
        let dir = npm_e2e_fixture("sha512-abc");
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let addrs = get_deps_addrs(&provider, "packages/a").await;
        assert_eq!(addrs.len(), 1);
        let node_modules_addr =
            hmodel::htaddr::parse_addr(&addrs[0]).expect("parse relocated addr");

        let ct = ctoken();
        let spec = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: node_modules_addr,
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ct,
            )
            .await
            .expect("get node_modules group spec")
            .target_spec;
        assert_eq!(spec.driver, "group");

        let parsed = hplugin::driver::Driver::parse(
            &hbuiltins::plugingroup::Driver,
            hplugin::driver::ParseRequest {
                request_id: "test".to_string(),
                target_spec: Arc::new(spec),
            },
            &ct,
        )
        .await
        .expect("parse group spec");
        assert_eq!(parsed.target_def.inputs.len(), 1);

        // The underlying `js_install` download's own synthetic package
        // path — what its files are packed as, before relocation.
        let install_pkg = thirdparty::thirdparty_pkg("lodash", "4.17.21");
        let fake_artifact = FakeInstallArtifact {
            paths: vec![
                format!("{}/package.json", install_pkg.as_str()),
                format!("{}/index.js", install_pkg.as_str()),
            ],
        };
        let run_input = hplugin::driver::RunInput {
            artifact: hplugin::driver::inputartifact::InputArtifact {
                r#type: hplugin::driver::inputartifact::Type::Dep,
                origin_id: "dep0".to_string(),
                content: Arc::new(fake_artifact),
            },
            origin_id: "dep0".to_string(),
            source_addr: thirdparty::thirdparty_addr(
                "lodash",
                "4.17.21",
                &platform::current_os(),
                &platform::current_arch(),
            ),
            filters: vec![],
            annotations: Default::default(),
        };
        let hashin = "test-hashin".to_string();
        let resp = hplugin::driver::Driver::run(
            &hbuiltins::plugingroup::Driver,
            hplugin::driver::RunRequest {
                request_id: &"test".to_string(),
                target: &parsed.target_def,
                tree_root_path: PathBuf::new(),
                inputs: vec![run_input],
                hashin: &hashin,
                stdin: None,
                stdout: None,
                stderr: None,
                sandbox_dir: PathBuf::new(),
            },
            &ct,
        )
        .await
        .expect("run group relocation");

        assert_eq!(resp.artifacts.len(), 1);
        let mut relocated: Vec<String> =
            hcore::hartifactcontent::Content::entry_paths(&resp.artifacts[0])
                .expect("relocated entry paths")
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
        relocated.sort();
        assert_eq!(
            relocated,
            vec![
                "packages/a/node_modules/lodash/index.js".to_string(),
                "packages/a/node_modules/lodash/package.json".to_string(),
            ],
            "js_install's download must land at packages/a/node_modules/lodash, the only path \
             Node's own module resolution looks at"
        );
        for p in &relocated {
            assert!(
                !p.contains("@heph/js/thirdparty"),
                "the synthetic js_install path must not survive relocation: {p}"
            );
        }
    }

    /// The `consuming_pkg == ""` corner case: a single-package repo (or a
    /// script living directly at the workspace root) with no nested package
    /// directory at all. `node_modules_addr`'s `pkg` arg is a plain empty
    /// `Addr` arg value here, not a sentinel (see that fn's doc) — this is
    /// the one path that would silently break if the empty-value round trip
    /// or `node_modules_group_spec`'s `consuming_pkg.is_empty()` branch ever
    /// regressed, and until now nothing exercised it.
    #[tokio::test]
    async fn npm_e2e_third_party_dep_materializes_at_workspace_root_node_modules() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "dependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root", "dependencies": { "lodash": "^4.17.21" } },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let addrs = get_deps_addrs(&provider, "").await;
        assert_eq!(addrs.len(), 1);
        assert!(addrs[0].contains("name=lodash"), "{}", addrs[0]);
        // The empty consuming-package arg round-trips as a plain, quoted
        // empty `Addr` value — `pkg=""`, not any sentinel spelling.
        assert!(addrs[0].contains("pkg=\"\""), "{}", addrs[0]);
        let node_modules_addr =
            hmodel::htaddr::parse_addr(&addrs[0]).expect("parse relocated addr");

        let ct = ctoken();
        let spec = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: node_modules_addr,
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ct,
            )
            .await
            .expect("get node_modules group spec")
            .target_spec;

        let parsed = hplugin::driver::Driver::parse(
            &hbuiltins::plugingroup::Driver,
            hplugin::driver::ParseRequest {
                request_id: "test".to_string(),
                target_spec: Arc::new(spec),
            },
            &ct,
        )
        .await
        .expect("parse group spec");

        let install_pkg = thirdparty::thirdparty_pkg("lodash", "4.17.21");
        let fake_artifact = FakeInstallArtifact {
            paths: vec![format!("{}/index.js", install_pkg.as_str())],
        };
        let run_input = hplugin::driver::RunInput {
            artifact: hplugin::driver::inputartifact::InputArtifact {
                r#type: hplugin::driver::inputartifact::Type::Dep,
                origin_id: "dep0".to_string(),
                content: Arc::new(fake_artifact),
            },
            origin_id: "dep0".to_string(),
            source_addr: thirdparty::thirdparty_addr(
                "lodash",
                "4.17.21",
                &platform::current_os(),
                &platform::current_arch(),
            ),
            filters: vec![],
            annotations: Default::default(),
        };
        let hashin = "test-hashin".to_string();
        let resp = hplugin::driver::Driver::run(
            &hbuiltins::plugingroup::Driver,
            hplugin::driver::RunRequest {
                request_id: &"test".to_string(),
                target: &parsed.target_def,
                tree_root_path: PathBuf::new(),
                inputs: vec![run_input],
                hashin: &hashin,
                stdin: None,
                stdout: None,
                stderr: None,
                sandbox_dir: PathBuf::new(),
            },
            &ct,
        )
        .await
        .expect("run group relocation");

        let relocated: Vec<String> =
            hcore::hartifactcontent::Content::entry_paths(&resp.artifacts[0])
                .expect("relocated entry paths")
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
        assert_eq!(
            relocated,
            vec!["node_modules/lodash/index.js".to_string()],
            "a workspace-root consumer's node_modules must not be nested under any package \
             prefix: {relocated:?}"
        );
    }

    /// The IDE-visibility target itself, end to end: `//packages/a:node_modules`
    /// resolves to a `group` spec that (a) is `codegen = "copy"` — the only
    /// thing that makes `heph run` actually write to real disk — and (b)
    /// aggregates *both* `a`'s direct dependency (`outer`) and `outer`'s own
    /// transitive dependency (`inner`), proving this target and
    /// `test_deps_config`'s transitive-closure wiring agree on what a
    /// package's full third-party dependency set is.
    #[tokio::test]
    async fn npm_e2e_node_modules_sync_target_aggregates_direct_and_transitive_deps_with_codegen_copy()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"outer": "^1.0.0"}}"#,
        );
        write(
            dir.path(),
            "packages/a/package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "a" },
                    "node_modules/outer": {
                        "version": "1.0.0",
                        "integrity": "sha512-outer",
                        "dependencies": { "inner": "2.0.0" }
                    },
                    "node_modules/inner": {
                        "version": "2.0.0",
                        "integrity": "sha512-inner"
                    }
                }
            }"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let addr = Addr::new(
            PkgBuf::from("packages/a"),
            NODE_MODULES_SYNC_TARGET.to_string(),
            Default::default(),
        );
        let spec = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ct,
            )
            .await
            .expect("get node_modules sync spec")
            .target_spec;
        assert_eq!(spec.driver, "group");
        assert_eq!(
            spec.config.get("codegen"),
            Some(&Value::String("copy".to_string())),
            "{:?}",
            spec.config
        );

        let parsed = hplugin::driver::Driver::parse(
            &hbuiltins::plugingroup::Driver,
            hplugin::driver::ParseRequest {
                request_id: "test".to_string(),
                target_spec: Arc::new(spec),
            },
            &ct,
        )
        .await
        .expect("parse node_modules sync spec");
        assert!(
            !parsed.target_def.transparent,
            "codegen requires a real (non-transparent) target"
        );
        assert_eq!(parsed.target_def.outputs.len(), 1);
        assert_eq!(
            parsed.target_def.outputs[0].paths[0].codegen_tree,
            hplugin::driver::targetdef::path::CodegenMode::Copy
        );

        let dep_names: Vec<String> = parsed
            .target_def
            .inputs
            .iter()
            .map(|i| i.r#ref.r#ref.format())
            .collect();
        assert!(
            dep_names.iter().any(|a| a.contains("name=outer")),
            "the direct dependency must be aggregated: {dep_names:?}"
        );
        assert!(
            dep_names.iter().any(|a| a.contains("name=inner")),
            "outer's own transitive dependency must be aggregated too: {dep_names:?}"
        );
    }

    /// The real-world bug report this fix exists for: a repo with more than
    /// one independent npm project, `mgmt/backoffice` being one of them —
    /// its own `package.json` *and* `package-lock.json`, not npm
    /// `workspaces` members of anything at the heph workspace root (which
    /// has neither file at all here, on purpose). `Provider::workspace_root`
    /// is `dir.path()` — the *heph* root, not `mgmt/backoffice`'s own — so
    /// this only passes if the lockfile is discovered by ancestor search
    /// from the package being resolved, not read from one fixed path.
    #[tokio::test]
    async fn npm_e2e_discovers_lockfile_for_an_independent_project_root_not_at_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Deliberately no package.json/package-lock.json at dir.path() itself.
        write(
            dir.path(),
            "mgmt/backoffice/package.json",
            r#"{"name": "backoffice", "dependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(
            dir.path(),
            "mgmt/backoffice/package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "backoffice" },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let addrs = get_deps_addrs(&provider, "mgmt/backoffice").await;
        assert_eq!(addrs.len(), 1);
        assert!(
            addrs[0].contains("@heph/js/node_modules")
                && addrs[0].contains("name=lodash")
                && addrs[0].contains("version=4.17.21")
                && addrs[0].contains("pkg=mgmt/backoffice"),
            "a package with its own independent lockfile, nested anywhere under the heph \
             workspace root, must still resolve its third-party deps: {}",
            addrs[0]
        );

        // And the full addr -> TargetSpec -> TargetDef pipeline resolves too.
        // Note this runs with `mgmt/backoffice`'s root already warm in
        // `resolved_graph_cache` from `get_deps_addrs` above — it does not
        // by itself exercise `find_resolved_graph_for` from a genuinely cold
        // `Provider`; `npm_e2e_ambiguous_integrity_across_independent_projects_fails_loudly`
        // below covers that (its error path), and
        // `npm_e2e_cold_provider_resolves_js_install_via_workspace_walk`
        // covers it for the success path.
        let hash = get_js_install_hash(&provider).await;
        assert!(!hash.is_empty());
    }

    /// Two independent projects, side by side, each with its own lockfile
    /// pinning a *different* version of the same package name — proves
    /// `Provider::lockfile`'s per-root cache doesn't cross-contaminate: each
    /// package's own deps resolve against its own lockfile, not whichever
    /// one happened to be discovered/cached first.
    #[tokio::test]
    async fn npm_e2e_two_independent_projects_resolve_against_their_own_lockfiles() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (pkg, version) in [("mgmt/backoffice", "4.17.21"), ("mgmt/frontend", "4.17.20")] {
            write(
                dir.path(),
                &format!("{pkg}/package.json"),
                &format!(r#"{{"name": "{pkg}", "dependencies": {{"lodash": "^4.0.0"}}}}"#),
            );
            write(
                dir.path(),
                &format!("{pkg}/package-lock.json"),
                &format!(
                    r#"{{
                        "lockfileVersion": 3,
                        "packages": {{
                            "": {{ "name": "{pkg}" }},
                            "node_modules/lodash": {{
                                "version": "{version}",
                                "resolved": "https://registry.npmjs.org/lodash/-/lodash-{version}.tgz",
                                "integrity": "sha512-{version}"
                            }}
                        }}
                    }}"#
                ),
            );
        }

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let backoffice_addrs = get_deps_addrs(&provider, "mgmt/backoffice").await;
        let frontend_addrs = get_deps_addrs(&provider, "mgmt/frontend").await;
        assert!(
            backoffice_addrs
                .iter()
                .any(|a| a.contains("name=lodash") && a.contains("version=4.17.21")),
            "{backoffice_addrs:?}"
        );
        assert!(
            frontend_addrs
                .iter()
                .any(|a| a.contains("name=lodash") && a.contains("version=4.17.20")),
            "{frontend_addrs:?}"
        );
    }

    /// The hermeticity BLOCKER this fix closes: `thirdparty_addr`'s scheme
    /// (bare `name@version`, no project scoping) is only a valid cache key
    /// when a published package's own metadata is genuinely the same
    /// regardless of which project's lockfile recorded it — no longer
    /// guaranteed once one `Provider` can discover more than one
    /// independent lockfile. Two projects here pin the identical
    /// `lodash@4.17.21` `(name, version)` but with *different* `integrity`
    /// (a different registry/mirror, or a real supply-chain divergence) —
    /// resolving the shared `js_install` addr must fail loudly naming both
    /// roots, never silently pick one (which would build non-deterministically
    /// across runs, depending on `HashMap` iteration order/walk order).
    #[tokio::test]
    async fn npm_e2e_ambiguous_integrity_across_independent_projects_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (pkg, integrity) in [
            ("mgmt/backoffice", "sha512-aaa"),
            ("mgmt/frontend", "sha512-bbb"),
        ] {
            write(
                dir.path(),
                &format!("{pkg}/package.json"),
                &format!(r#"{{"name": "{pkg}", "dependencies": {{"lodash": "^4.17.21"}}}}"#),
            );
            write(
                dir.path(),
                &format!("{pkg}/package-lock.json"),
                &format!(
                    r#"{{
                        "lockfileVersion": 3,
                        "packages": {{
                            "": {{ "name": "{pkg}" }},
                            "node_modules/lodash": {{
                                "version": "4.17.21",
                                "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                                "integrity": "{integrity}"
                            }}
                        }}
                    }}"#
                ),
            );
        }

        // A cold `Provider`, going straight for the shared `js_install` addr
        // with nothing having cached either root yet — forces
        // `find_resolved_graph_for`'s full-workspace-walk path, which
        // unconditionally discovers *both* projects' roots.
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "lodash",
            "4.17.21",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        let msg = match result {
            Err(GetError::Other(e)) => format!("{e:#}"),
            Err(GetError::NotFound) => panic!("expected an ambiguity error, got NotFound"),
            Ok(_) => panic!(
                "ambiguous integrity across independent projects must be a hard error, got Ok"
            ),
        };
        assert!(msg.contains("lodash@4.17.21"), "{msg}");
        assert!(
            msg.contains("sha512-aaa") && msg.contains("sha512-bbb"),
            "{msg}"
        );
    }

    /// A real, previously-undiscovered false positive of the check above:
    /// one root's lockfile entry for the shared `(name, version)` has empty
    /// `integrity`/no `resolved` (a degenerate/dedup record — a real,
    /// observed npm lockfile shape, not hypothetical; confirmed live in a
    /// real workspace, across hundreds of packages simultaneously). This is
    /// *not* a genuine ambiguity — the empty entry never pinned any content
    /// to disagree with — so resolution must succeed, using the one root
    /// that actually resolved real content, not hard-fail the way two
    /// *equally real but differing* entries correctly do above.
    #[tokio::test]
    async fn npm_e2e_one_root_has_empty_integrity_entry_resolves_via_the_real_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "mgmt/backoffice/package.json",
            r#"{"name": "backoffice", "dependencies": {"yup": "^1.7.1"}}"#,
        );
        write(
            dir.path(),
            "mgmt/backoffice/package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "backoffice" },
                    "node_modules/yup": {
                        "version": "1.7.1"
                    }
                }
            }"#,
        );
        write(
            dir.path(),
            "mgmt/frontend/package.json",
            r#"{"name": "frontend", "dependencies": {"yup": "^1.7.1"}}"#,
        );
        write(
            dir.path(),
            "mgmt/frontend/package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "frontend" },
                    "node_modules/yup": {
                        "version": "1.7.1",
                        "resolved": "https://registry.npmjs.org/yup/-/yup-1.7.1.tgz",
                        "integrity": "sha512-real"
                    }
                }
            }"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "yup",
            "1.7.1",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("must resolve, not error — an empty-integrity entry is not a real conflict");
        assert_eq!(resp.target_spec.driver, "js_install");
        assert_eq!(
            resp.target_spec.config.get("integrity"),
            Some(&Value::String("sha512-real".to_string())),
            "must use the root with real content, not the empty-integrity one: {:?}",
            resp.target_spec.config
        );
    }

    /// The other half of the empty-integrity shape: when *no* discovered
    /// lockfile root has real integrity for this package — a genuinely
    /// single-project package whose own lockfile never recorded one (a
    /// real, live npm bug: `npm install`, unlike `npm ci`, can strip
    /// `resolved`/`integrity` from an existing entry when it's satisfied
    /// from npm's local cache — npm/cli#4263/#4460/#6301) — there is no
    /// "real" entry anywhere to fall back to. `Provider::get` still resolves
    /// the `js_install` spec (with an empty `integrity`) rather than block
    /// the install on a lockfile heph didn't write; `driver_install`'s own
    /// tests cover what happens with that empty value at fetch time.
    #[tokio::test]
    async fn npm_e2e_package_with_no_integrity_anywhere_still_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "mgmt/backoffice/package.json",
            r#"{"name": "backoffice", "devDependencies": {"ts-log": "^2.2.3"}}"#,
        );
        write(
            dir.path(),
            "mgmt/backoffice/package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "backoffice" },
                    "node_modules/ts-log": {
                        "version": "2.2.5",
                        "dev": true,
                        "license": "MIT"
                    }
                }
            }"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "ts-log",
            "2.2.5",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("must resolve — proceed unverified rather than block on a lockfile heph didn't write");
        assert_eq!(resp.target_spec.driver, "js_install");
        assert_eq!(
            resp.target_spec.config.get("integrity"),
            Some(&Value::String(String::new())),
            "no integrity anywhere means the spec carries an empty one through, not a fabricated one: {:?}",
            resp.target_spec.config
        );
    }

    /// A hermeticity review caught a more insidious version of the bug the
    /// previous test fixes: an earlier draft of that fix dropped the
    /// *entire* entry from comparison whenever its `integrity` was empty —
    /// not just the `integrity` field — so a root with empty `integrity`
    /// but a real, genuinely different `resolved` URL (an internal mirror,
    /// say) would silently pass unchecked, using an unrelated project's
    /// `resolved` for this root's own dependency. Empty `integrity` alone
    /// must never exempt a root from having its *other* real, populated
    /// fields compared — this must still fail loudly.
    #[tokio::test]
    async fn npm_e2e_empty_integrity_with_real_diverging_resolved_still_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "mgmt/backoffice/package.json",
            r#"{"name": "backoffice", "dependencies": {"yup": "^1.7.1"}}"#,
        );
        write(
            dir.path(),
            "mgmt/backoffice/package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "backoffice" },
                    "node_modules/yup": {
                        "version": "1.7.1",
                        "resolved": "https://internal-mirror.corp/yup/-/yup-1.7.1.tgz"
                    }
                }
            }"#,
        );
        write(
            dir.path(),
            "mgmt/frontend/package.json",
            r#"{"name": "frontend", "dependencies": {"yup": "^1.7.1"}}"#,
        );
        write(
            dir.path(),
            "mgmt/frontend/package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "frontend" },
                    "node_modules/yup": {
                        "version": "1.7.1",
                        "resolved": "https://registry.npmjs.org/yup/-/yup-1.7.1.tgz",
                        "integrity": "sha512-real"
                    }
                }
            }"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "yup",
            "1.7.1",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        let msg = match result {
            Err(GetError::Other(e)) => format!("{e:#}"),
            Err(GetError::NotFound) => panic!("expected an ambiguity error, got NotFound"),
            Ok(_) => panic!(
                "empty integrity on one side must not exempt a real, differing resolved URL \
                 from the ambiguity check, got Ok"
            ),
        };
        assert!(msg.contains("yup@1.7.1"), "{msg}");
        assert!(
            msg.contains("internal-mirror.corp") && msg.contains("registry.npmjs.org"),
            "{msg}"
        );
    }

    /// The false positive observed live, right after both prior fixes
    /// shipped: two independent, unrelated projects both genuinely resolve
    /// the *identical* published package — same `integrity`, same
    /// `resolved` URL — but the package's own `dependencies` map differs,
    /// because each project's `npm install` ran at a different time and
    /// this shared package's *own* transitive dependency happened to
    /// resolve to a different patch version. That is not a conflict about
    /// *this* package's content (`integrity` already proves the tarball
    /// bytes are identical) — `dependencies` is graph-traversal bookkeeping
    /// about *other* packages, never part of `js_install`'s own cache key,
    /// and must never be compared here.
    #[tokio::test]
    async fn npm_e2e_same_integrity_different_own_dependencies_resolves_without_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (pkg, is_typed_array_version) in
            [("mgmt/backoffice", "1.1.13"), ("mgmt/frontend", "1.1.14")]
        {
            write(
                dir.path(),
                &format!("{pkg}/package.json"),
                &format!(
                    r#"{{"name": "{pkg}", "dependencies": {{"typed-array-buffer": "^1.0.3"}}}}"#
                ),
            );
            write(
                dir.path(),
                &format!("{pkg}/package-lock.json"),
                &format!(
                    r#"{{
                        "lockfileVersion": 3,
                        "packages": {{
                            "": {{ "name": "{pkg}" }},
                            "node_modules/typed-array-buffer": {{
                                "version": "1.0.3",
                                "resolved": "https://registry.npmjs.org/typed-array-buffer/-/typed-array-buffer-1.0.3.tgz",
                                "integrity": "sha512-same",
                                "dependencies": {{ "is-typed-array": "{is_typed_array_version}" }}
                            }},
                            "node_modules/is-typed-array": {{
                                "version": "{is_typed_array_version}",
                                "integrity": "sha512-abc"
                            }}
                        }}
                    }}"#
                ),
            );
        }

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "typed-array-buffer",
            "1.0.3",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect(
                "must resolve, not error — differing dependencies of the *same* byte-identical \
                 package is not an ambiguity",
            );
        assert_eq!(
            resp.target_spec.config.get("integrity"),
            Some(&Value::String("sha512-same".to_string()))
        );
    }

    /// Same defect class as the `dependencies` test above,
    /// `optional_dependencies` — added alongside `dependencies` in
    /// `ResolvedPackage` for lifecycle-script sibling resolution — has the
    /// exact same divergence property (two independent `npm install`s of
    /// the identical published tarball can legitimately record different
    /// resolved optional-dependency versions), so it must be excluded from
    /// `entries_agree_where_comparable` for the identical reason, not merely
    /// by omission from the comparison.
    #[tokio::test]
    async fn npm_e2e_same_integrity_different_own_optional_dependencies_resolves_without_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (pkg, is_typed_array_version) in
            [("mgmt/backoffice", "1.1.13"), ("mgmt/frontend", "1.1.14")]
        {
            write(
                dir.path(),
                &format!("{pkg}/package.json"),
                &format!(
                    r#"{{"name": "{pkg}", "dependencies": {{"typed-array-buffer": "^1.0.3"}}}}"#
                ),
            );
            write(
                dir.path(),
                &format!("{pkg}/package-lock.json"),
                &format!(
                    r#"{{
                        "lockfileVersion": 3,
                        "packages": {{
                            "": {{ "name": "{pkg}" }},
                            "node_modules/typed-array-buffer": {{
                                "version": "1.0.3",
                                "resolved": "https://registry.npmjs.org/typed-array-buffer/-/typed-array-buffer-1.0.3.tgz",
                                "integrity": "sha512-same",
                                "optionalDependencies": {{ "is-typed-array": "{is_typed_array_version}" }}
                            }},
                            "node_modules/is-typed-array": {{
                                "version": "{is_typed_array_version}",
                                "integrity": "sha512-abc"
                            }}
                        }}
                    }}"#
                ),
            );
        }

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "typed-array-buffer",
            "1.0.3",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect(
                "must resolve, not error — differing optional_dependencies of the *same* \
                 byte-identical package is not an ambiguity",
            );
        assert_eq!(
            resp.target_spec.config.get("integrity"),
            Some(&Value::String("sha512-same".to_string()))
        );
    }

    /// Same defect class as `npm_e2e_ambiguous_integrity_across_independent_
    /// projects_fails_loudly`, but on `resolved` (the tarball URL) rather
    /// than `integrity`: two projects can validly agree on `integrity`
    /// (byte-identical content) while recording a different `resolved` — a
    /// public registry vs. an internal mirror serving the same tarball, say.
    /// `resolved` still feeds `JsInstallDef`'s hash directly
    /// (`driver_install.rs`), so picking either root's entry silently would
    /// make `js_install`'s cache key nondeterministic across runs of an
    /// unchanged tree. Both roots are resolved (and therefore cached) via
    /// `get_deps_addrs` *before* the shared addr is requested, mirroring the
    /// realistic staggered-resolution order — this is exactly the scenario
    /// the ambiguity check must catch even when a cache hit already exists
    /// for one root.
    #[tokio::test]
    async fn npm_e2e_ambiguous_resolved_url_across_independent_projects_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (pkg, resolved) in [
            (
                "mgmt/backoffice",
                "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
            ),
            (
                "mgmt/frontend",
                "https://mirror.example.internal/lodash/-/lodash-4.17.21.tgz",
            ),
        ] {
            write(
                dir.path(),
                &format!("{pkg}/package.json"),
                &format!(r#"{{"name": "{pkg}", "dependencies": {{"lodash": "^4.17.21"}}}}"#),
            );
            write(
                dir.path(),
                &format!("{pkg}/package-lock.json"),
                &format!(
                    r#"{{
                        "lockfileVersion": 3,
                        "packages": {{
                            "": {{ "name": "{pkg}" }},
                            "node_modules/lodash": {{
                                "version": "4.17.21",
                                "resolved": "{resolved}",
                                "integrity": "sha512-same"
                            }}
                        }}
                    }}"#
                ),
            );
        }

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        // Warm both roots' `resolved_graph_cache` entries first — the
        // ordering that defeated the pre-fix `if matches.is_empty()` guard.
        get_deps_addrs(&provider, "mgmt/backoffice").await;
        get_deps_addrs(&provider, "mgmt/frontend").await;

        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "lodash",
            "4.17.21",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        let msg = match result {
            Err(GetError::Other(e)) => format!("{e:#}"),
            Err(GetError::NotFound) => panic!("expected an ambiguity error, got NotFound"),
            Ok(_) => panic!(
                "ambiguous resolved URL across independent projects must be a hard error, got Ok"
            ),
        };
        assert!(msg.contains("lodash@4.17.21"), "{msg}");
        assert!(
            msg.contains("registry.npmjs.org") && msg.contains("mirror.example.internal"),
            "{msg}"
        );
    }

    /// The specific regression the BLOCKER fix in `find_resolved_graph_for`
    /// targets: **one** of the two conflicting roots is already cached (via
    /// an ordinary `get_deps_addrs` resolution), the other has never been
    /// touched at all when the shared addr is requested. The removed
    /// `if matches.is_empty()` gate would see the cache scan's one match,
    /// treat `matches` as already non-empty, and return early — skipping
    /// the walk entirely and never discovering (let alone comparing against)
    /// the untouched second root. This is the scenario the other two
    /// ambiguity tests don't cover: both leave `matches` either fully empty
    /// before the walk (cold `Provider`) or fully populated by the cache
    /// scan alone (both roots pre-warmed) — neither depends on the walk
    /// running while `matches` is non-empty but incomplete, which is the one
    /// line this whole fix hinges on.
    #[tokio::test]
    async fn npm_e2e_ambiguous_integrity_one_root_cached_other_cold_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (pkg, integrity) in [
            ("mgmt/backoffice", "sha512-aaa"),
            ("mgmt/frontend", "sha512-bbb"),
        ] {
            write(
                dir.path(),
                &format!("{pkg}/package.json"),
                &format!(r#"{{"name": "{pkg}", "dependencies": {{"lodash": "^4.17.21"}}}}"#),
            );
            write(
                dir.path(),
                &format!("{pkg}/package-lock.json"),
                &format!(
                    r#"{{
                        "lockfileVersion": 3,
                        "packages": {{
                            "": {{ "name": "{pkg}" }},
                            "node_modules/lodash": {{
                                "version": "4.17.21",
                                "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                                "integrity": "{integrity}"
                            }}
                        }}
                    }}"#
                ),
            );
        }

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        // Only `mgmt/backoffice`'s root is resolved (and therefore cached)
        // before the shared addr is requested — `mgmt/frontend`'s lockfile
        // is never touched by anything else in this test.
        get_deps_addrs(&provider, "mgmt/backoffice").await;

        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "lodash",
            "4.17.21",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        let msg = match result {
            Err(GetError::Other(e)) => format!("{e:#}"),
            Err(GetError::NotFound) => panic!("expected an ambiguity error, got NotFound"),
            Ok(_) => panic!(
                "ambiguous integrity across independent projects must be a hard error even when \
                 only one of the two conflicting roots was already cached, got Ok"
            ),
        };
        assert!(msg.contains("lodash@4.17.21"), "{msg}");
        assert!(
            msg.contains("sha512-aaa") && msg.contains("sha512-bbb"),
            "{msg}"
        );
    }

    /// The success counterpart to
    /// `npm_e2e_ambiguous_integrity_across_independent_projects_fails_loudly`:
    /// a single independent project, nested away from the heph workspace
    /// root, resolved straight off a cold `Provider` with nothing cached —
    /// `find_resolved_graph_for`'s unconditional workspace walk must find
    /// this project's lockfile root and return its `js_install` target on
    /// the very first call, not merely fail to error.
    #[tokio::test]
    async fn npm_e2e_cold_provider_resolves_js_install_via_workspace_walk() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "mgmt/backoffice/package.json",
            r#"{"name": "backoffice", "dependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(
            dir.path(),
            "mgmt/backoffice/package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "backoffice" },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let hash = get_js_install_hash(&provider).await;
        assert!(!hash.is_empty());
    }

    #[tokio::test]
    async fn pnpm_e2e_wires_third_party_dep_to_js_install_addr() {
        let dir = pnpm_e2e_fixture("sha512-abc");
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let addrs = get_deps_addrs(&provider, "packages/a").await;
        assert_eq!(addrs.len(), 1);
        assert!(addrs[0].contains("@heph/js/node_modules"), "{}", addrs[0]);
        assert!(addrs[0].contains("name=lodash"), "{}", addrs[0]);
        assert!(addrs[0].contains("version=4.17.21"), "{}", addrs[0]);
        assert!(addrs[0].contains("pkg=packages/a"), "{}", addrs[0]);
    }

    /// Resolve the `js_install` `TargetSpec` for `lodash@4.17.21` straight off
    /// the provider (mirroring what the engine does when it follows the dep
    /// addr `npm_e2e_wires_third_party_dep_to_js_install_addr` just produced),
    /// then parse it — the full addr → `TargetSpec` → `TargetDef` pipeline.
    async fn get_js_install_hash(provider: &Provider) -> Vec<u8> {
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "lodash",
            "4.17.21",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: addr.clone(),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_install target_spec");
        assert_eq!(resp.target_spec.driver, "js_install");

        let parsed = JsInstallDriver::new()
            .parse(
                hplugin::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(resp.target_spec),
                },
                &ct,
            )
            .await
            .expect("parse js_install target_spec");
        parsed.target_def.hash
    }

    /// (b) The `js_install` cache key (its `TargetDef::hash`) changes when
    /// the lockfile's integrity entry changes, and is stable when it does
    /// not — mirroring the Go plugin's
    /// `import_path_order_does_not_affect_compile_def_hash`-style tests, but
    /// end to end from the lockfile file on disk.
    #[tokio::test]
    async fn npm_e2e_install_hash_tracks_lockfile_integrity() {
        let dir_a = npm_e2e_fixture("sha512-abc");
        let dir_b = npm_e2e_fixture("sha512-abc");
        let dir_c = npm_e2e_fixture("sha512-different");

        let hash_a =
            get_js_install_hash(&Provider::new(dir_a.path().to_path_buf(), PkgManager::Npm)).await;
        let hash_b =
            get_js_install_hash(&Provider::new(dir_b.path().to_path_buf(), PkgManager::Npm)).await;
        let hash_c =
            get_js_install_hash(&Provider::new(dir_c.path().to_path_buf(), PkgManager::Npm)).await;

        assert_eq!(
            hash_a, hash_b,
            "identical lockfile entries must hash identically"
        );
        assert_ne!(
            hash_a, hash_c,
            "a changed integrity entry must change the cache key"
        );
    }

    #[tokio::test]
    async fn pnpm_e2e_install_hash_tracks_lockfile_integrity() {
        let dir_a = pnpm_e2e_fixture("sha512-abc");
        let dir_b = pnpm_e2e_fixture("sha512-abc");
        let dir_c = pnpm_e2e_fixture("sha512-different");

        let hash_a =
            get_js_install_hash(&Provider::new(dir_a.path().to_path_buf(), PkgManager::Pnpm)).await;
        let hash_b =
            get_js_install_hash(&Provider::new(dir_b.path().to_path_buf(), PkgManager::Pnpm)).await;
        let hash_c =
            get_js_install_hash(&Provider::new(dir_c.path().to_path_buf(), PkgManager::Pnpm)).await;

        assert_eq!(
            hash_a, hash_b,
            "identical lockfile entries must hash identically"
        );
        assert_ne!(
            hash_a, hash_c,
            "a changed integrity entry must change the cache key"
        );
    }

    /// (c) A package the lockfile marks as requiring an install script,
    /// which is not on the provider's `allow_scripts` allowlist, must fail
    /// loudly (naming the package) rather than silently installing without
    /// running it — driven end to end from the provider's `get` through
    /// `JsInstallDriver::parse`.
    #[tokio::test]
    async fn npm_e2e_unallowlisted_install_script_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": ["packages/*"]}"#,
        );
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"native-thing": "^1.0.0"}}"#,
        );
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root", "workspaces": ["packages/*"] },
                    "packages/a": { "name": "a", "dependencies": { "native-thing": "^1.0.0" } },
                    "node_modules/native-thing": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/native-thing/-/native-thing-1.0.0.tgz",
                        "integrity": "sha512-xyz",
                        "hasInstallScript": true
                    }
                }
            }"#,
        );
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);

        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "native-thing",
            "1.0.0",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_install target_spec");

        let err = JsInstallDriver::new()
            .parse(
                hplugin::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(resp.target_spec),
                },
                &ct,
            )
            .await
            .err()
            .expect("unallowlisted install script must fail parse, not silently succeed");
        let msg = format!("{err:#}");
        assert!(msg.contains("native-thing"), "must name the package: {msg}");
        assert!(
            msg.contains("allow_scripts"),
            "must point at the fix: {msg}"
        );
    }

    /// The same package as above, but allow-listed via the provider's
    /// `allow_scripts` option — parsing must succeed (the allowlist actually
    /// takes effect, this isn't just a permanently-closed gate).
    #[tokio::test]
    async fn npm_e2e_allowlisted_install_script_parses_successfully() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/native-thing": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/native-thing/-/native-thing-1.0.0.tgz",
                        "integrity": "sha512-xyz",
                        "hasInstallScript": true
                    }
                }
            }"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Npm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["native-thing".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );

        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "native-thing",
            "1.0.0",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_install target_spec");

        JsInstallDriver::new()
            .parse(
                hplugin::driver::ParseRequest {
                    request_id: "test".to_string(),
                    target_spec: Arc::new(resp.target_spec),
                },
                &ct,
            )
            .await
            .expect("allow-listed install script must parse successfully");
    }

    /// A `js_install` target with no lifecycle script at all must never
    /// resolve sibling dependencies — even when the lockfile records
    /// `optionalDependencies` for it — the overwhelming common case must
    /// cost nothing (see `thirdparty_install_spec`'s own doc on this).
    #[tokio::test]
    async fn npm_e2e_package_without_lifecycle_script_never_resolves_sibling_deps() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                        "integrity": "sha512-abc",
                        "optionalDependencies": { "left-pad": "1.0.0" }
                    },
                    "node_modules/left-pad": {
                        "version": "1.0.0",
                        "integrity": "sha512-leftpad"
                    }
                }
            }"#,
        );
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "lodash",
            "4.17.21",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_install target_spec");
        assert_eq!(
            resp.target_spec.config.get("deps"),
            Some(&Value::List(vec![]))
        );
        assert_eq!(
            resp.target_spec.config.get("skipped_deps"),
            Some(&Value::List(vec![]))
        );
    }

    /// A lifecycle-script package's platform-matching `optionalDependencies`
    /// sibling must be wired as a relocated `node_modules` dep — the exact
    /// esbuild/`@esbuild/linux-x64` shape confirmed live.
    #[tokio::test]
    async fn npm_e2e_lifecycle_script_package_wires_matching_optional_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/esbuild": {
                        "version": "0.25.12",
                        "resolved": "https://registry.npmjs.org/esbuild/-/esbuild-0.25.12.tgz",
                        "integrity": "sha512-loader",
                        "hasInstallScript": true,
                        "optionalDependencies": { "@esbuild/linux-x64": "0.25.12" }
                    },
                    "node_modules/@esbuild/linux-x64": {
                        "version": "0.25.12",
                        "resolved": "https://registry.npmjs.org/@esbuild/linux-x64/-/linux-x64-0.25.12.tgz",
                        "integrity": "sha512-native",
                        "os": ["linux"],
                        "cpu": ["x64"]
                    }
                }
            }"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Npm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["esbuild".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr("esbuild", "0.25.12", "linux", "amd64");
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_install target_spec");

        let expected_dep_addr = thirdparty::node_modules_addr(
            "",
            "@esbuild/linux-x64",
            "@esbuild/linux-x64",
            "0.25.12",
            "linux",
            "amd64",
        )
        .format();
        assert_eq!(
            resp.target_spec.config.get("deps"),
            Some(&Value::List(vec![Value::String(expected_dep_addr)]))
        );
        assert_eq!(
            resp.target_spec.config.get("skipped_deps"),
            Some(&Value::List(vec![])),
            "a matching sibling must not also be recorded as skipped"
        );
    }

    /// Confirmed live: `@sentry/cli`'s postinstall requires `which`, and
    /// `which` itself `require()`s `isexe` — a dependency of a dependency,
    /// never a *direct* edge of `@sentry/cli` at all. Sibling resolution
    /// must walk the whole transitive closure, not just the script-owning
    /// package's own `dependencies`, or the second hop (`isexe`) is simply
    /// never materialized and the script fails with `Cannot find module`.
    #[tokio::test]
    async fn npm_e2e_lifecycle_script_package_wires_transitive_required_dependency_two_hops_deep() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/@sentry/cli": {
                        "version": "3.5.0",
                        "resolved": "https://registry.npmjs.org/@sentry/cli/-/cli-3.5.0.tgz",
                        "integrity": "sha512-cli",
                        "hasInstallScript": true,
                        "dependencies": { "which": "2.0.2" }
                    },
                    "node_modules/which": {
                        "version": "2.0.2",
                        "resolved": "https://registry.npmjs.org/which/-/which-2.0.2.tgz",
                        "integrity": "sha512-which",
                        "dependencies": { "isexe": "2.0.0" }
                    },
                    "node_modules/isexe": {
                        "version": "2.0.0",
                        "resolved": "https://registry.npmjs.org/isexe/-/isexe-2.0.0.tgz",
                        "integrity": "sha512-isexe"
                    }
                }
            }"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Npm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["@sentry/cli".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr("@sentry/cli", "3.5.0", "linux", "amd64");
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_install target_spec");

        let expected_which =
            thirdparty::node_modules_addr("", "which", "which", "2.0.2", "linux", "amd64").format();
        let expected_isexe =
            thirdparty::node_modules_addr("", "isexe", "isexe", "2.0.0", "linux", "amd64").format();
        let Some(Value::List(deps)) = resp.target_spec.config.get("deps") else {
            panic!(
                "expected deps to be a List: {:?}",
                resp.target_spec.config.get("deps")
            );
        };
        assert!(
            deps.contains(&Value::String(expected_which)),
            "direct dependency `which` must be wired: {deps:?}"
        );
        assert!(
            deps.contains(&Value::String(expected_isexe)),
            "transitive dependency `isexe` (which's own dependency) must be wired: {deps:?}"
        );
    }

    /// The other half: a platform-*mismatched* `optionalDependencies`
    /// sibling must be silently skipped, never wired as a dependency, but
    /// the reason must still be recorded for later diagnosability — never a
    /// hard error, since this is the expected, common shape (esbuild
    /// records a sibling for every platform, only one of which ever
    /// applies).
    #[tokio::test]
    async fn npm_e2e_lifecycle_script_package_skips_platform_mismatched_optional_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/esbuild": {
                        "version": "0.25.12",
                        "resolved": "https://registry.npmjs.org/esbuild/-/esbuild-0.25.12.tgz",
                        "integrity": "sha512-loader",
                        "hasInstallScript": true,
                        "optionalDependencies": { "@esbuild/win32-x64": "0.25.12" }
                    },
                    "node_modules/@esbuild/win32-x64": {
                        "version": "0.25.12",
                        "integrity": "sha512-native-win",
                        "os": ["win32"],
                        "cpu": ["x64"]
                    }
                }
            }"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Npm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["esbuild".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr("esbuild", "0.25.12", "linux", "amd64");
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("a platform-mismatched optional sibling must never be a hard error");

        assert_eq!(
            resp.target_spec.config.get("deps"),
            Some(&Value::List(vec![])),
            "a mismatched sibling must never be wired as a dependency"
        );
        let Some(Value::List(skipped)) = resp.target_spec.config.get("skipped_deps") else {
            panic!(
                "expected skipped_deps to be a List: {:?}",
                resp.target_spec.config
            );
        };
        assert_eq!(skipped.len(), 1);
        let Value::String(reason) = &skipped[0] else {
            panic!("expected a String reason: {skipped:?}");
        };
        assert!(reason.contains("@esbuild/win32-x64"), "{reason}");
    }

    /// A *required* dependency that has no lockfile entry anywhere is
    /// **not** detectable as a hard error at this layer, and this pins that
    /// down deliberately rather than leaving it an unstated gap:
    /// `resolved.dependencies` is built by `resolve_npm_edges`, which
    /// already silently drops any edge it can't resolve — for *every*
    /// consumer of that field, not only lifecycle-script sibling
    /// resolution (a third-party package's own internal `dependencies` can
    /// legitimately reference git/tarball deps and other shapes this
    /// milestone's lockfile parsing doesn't resolve). By the time this
    /// code sees `resolved.dependencies`, an entirely-unresolvable name
    /// has already vanished — indistinguishable from the package simply
    /// having no such dependency. Only a *resolved-but-platform-mismatched*
    /// required dependency (the next test) is actually catchable here.
    #[tokio::test]
    async fn npm_e2e_lifecycle_script_required_dependency_missing_from_lockfile_is_silently_absent()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/native-thing": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/native-thing/-/native-thing-1.0.0.tgz",
                        "integrity": "sha512-xyz",
                        "hasInstallScript": true,
                        "dependencies": { "missing-helper": "1.0.0" }
                    }
                }
            }"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Npm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["native-thing".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "native-thing",
            "1.0.0",
            &platform::current_os(),
            &platform::current_arch(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("must resolve — an entirely-unresolvable required dep can't be caught here");
        assert_eq!(
            resp.target_spec.config.get("deps"),
            Some(&Value::List(vec![])),
            "the unresolvable dependency was already dropped upstream, before this code ever \
             sees it"
        );
    }

    /// Same required-dependency asymmetry, but for a platform mismatch
    /// rather than a missing lockfile entry — a required dependency
    /// restricted away from the current platform is exactly as actionable
    /// as one that's simply absent.
    #[tokio::test]
    async fn npm_e2e_lifecycle_script_package_required_dependency_platform_mismatch_is_a_hard_error()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/native-thing": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/native-thing/-/native-thing-1.0.0.tgz",
                        "integrity": "sha512-xyz",
                        "hasInstallScript": true,
                        "dependencies": { "win-only-helper": "1.0.0" }
                    },
                    "node_modules/win-only-helper": {
                        "version": "1.0.0",
                        "integrity": "sha512-winhelper",
                        "os": ["win32"]
                    }
                }
            }"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Npm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["native-thing".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr("native-thing", "1.0.0", "linux", "amd64");
        let err = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .err()
            .expect(
                "a required dependency restricted away from the current platform must be a hard \
                 error",
            );
        let msg = match err {
            GetError::Other(e) => format!("{e:#}"),
            other => panic!("expected GetError::Other, got {other:?}"),
        };
        assert!(msg.contains("win-only-helper"), "{msg}");
    }

    /// The transitive walk must terminate and never declare a package as
    /// its own `Input`: a dependency cycle back to the script-owning
    /// package by name (`helper` depends on `root-script`, its own
    /// consumer — a real, if unusual, npm graph shape) must be silently
    /// dropped rather than producing a second `node_modules/root-script`
    /// `Input` pointing at this exact `js_install` target — that would be
    /// the target depending on itself. A diamond (`helper-a` and `helper-b`
    /// both depending on `shared`) must also resolve `shared` exactly once.
    #[tokio::test]
    async fn npm_e2e_lifecycle_script_package_transitive_walk_dedupes_diamond_and_skips_cycle_back_to_root()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/root-script": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/root-script/-/root-script-1.0.0.tgz",
                        "integrity": "sha512-rootscript",
                        "hasInstallScript": true,
                        "dependencies": { "helper-a": "1.0.0", "helper-b": "1.0.0" }
                    },
                    "node_modules/helper-a": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/helper-a/-/helper-a-1.0.0.tgz",
                        "integrity": "sha512-helpera",
                        "dependencies": { "shared": "1.0.0", "root-script": "1.0.0" }
                    },
                    "node_modules/helper-b": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/helper-b/-/helper-b-1.0.0.tgz",
                        "integrity": "sha512-helperb",
                        "dependencies": { "shared": "1.0.0" }
                    },
                    "node_modules/shared": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/shared/-/shared-1.0.0.tgz",
                        "integrity": "sha512-shared"
                    }
                }
            }"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Npm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["root-script".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr("root-script", "1.0.0", "linux", "amd64");
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_install target_spec — a cycle back to the root must never error");

        let Some(Value::List(deps)) = resp.target_spec.config.get("deps") else {
            panic!(
                "expected deps to be a List: {:?}",
                resp.target_spec.config.get("deps")
            );
        };
        let root_addr = thirdparty::node_modules_addr(
            "",
            "root-script",
            "root-script",
            "1.0.0",
            "linux",
            "amd64",
        )
        .format();
        assert!(
            !deps.contains(&Value::String(root_addr)),
            "a cycle back to the script-owning package must never become a second Input \
             pointing at this exact target: {deps:?}"
        );
        let shared_count = deps
            .iter()
            .filter(|d| {
                d == &&Value::String(
                    thirdparty::node_modules_addr(
                        "", "shared", "shared", "1.0.0", "linux", "amd64",
                    )
                    .format(),
                )
            })
            .count();
        assert_eq!(
            shared_count, 1,
            "a diamond dependency (shared via both helper-a and helper-b) must resolve once: \
             {deps:?}"
        );
    }

    /// A `code-quality` review caught this live: `resolve_npm_edges`
    /// (`lockfile.rs`) keys a required-dependency edge by the *declared*
    /// name, but an `npm:`-aliased entry (`"foo": "npm:bar@1.2.3"`) records
    /// its *real* name (`bar`) in the lockfile, and `resolved_graph()`'s own
    /// top-level loop keys `ResolvedGraph::packages` by that real name — the
    /// two disagree for an alias, so the graph_key `resolved.dependencies`
    /// computes for `foo` is never actually present in `graph.packages`.
    /// An earlier draft of this code treated that as an impossible internal
    /// invariant and `.expect()`-panicked; it must instead be a normal,
    /// diagnosable `anyhow::Result` error, the same as any other
    /// lockfile-inconsistency this file reports.
    #[tokio::test]
    async fn npm_e2e_lifecycle_script_required_dependency_via_npm_alias_is_a_clean_error_not_a_panic()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/native-thing": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/native-thing/-/native-thing-1.0.0.tgz",
                        "integrity": "sha512-xyz",
                        "hasInstallScript": true,
                        "dependencies": { "foo": "npm:bar@1.2.3" }
                    },
                    "node_modules/foo": {
                        "name": "bar",
                        "version": "1.2.3",
                        "resolved": "https://registry.npmjs.org/bar/-/bar-1.2.3.tgz",
                        "integrity": "sha512-bar"
                    }
                }
            }"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Npm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["native-thing".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr("native-thing", "1.0.0", "linux", "amd64");
        // The point of this test is that `.await` returns an `Err`, not
        // that the process panics — `tokio::test` would report a panicked
        // task as a test failure either way, but asserting the specific
        // `Err` shape is what proves this is `anyhow::Result`'s normal
        // error path, not a caught panic.
        let err = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .err()
            .expect("an npm-aliased required dependency must be a clean error, not a panic");
        let msg = match err {
            GetError::Other(e) => format!("{e:#}"),
            other => panic!("expected GetError::Other, got {other:?}"),
        };
        assert!(msg.contains("foo"), "{msg}");
        assert!(msg.contains("alias"), "{msg}");
    }

    /// The pnpm-side counterpart to the npm-alias test above, and the
    /// second reproduction a `code-quality` review found for the same
    /// underlying bug class: `resolve_pnpm_edges` (`lockfile.rs`) computes
    /// a snapshot's own dependency edge purely by string-splitting the
    /// `{name: version}` pair it's given, with no cross-check against
    /// `packages:` at all — a malformed/stale `pnpm-lock.yaml` (a
    /// `dependencies:` entry in `snapshots:` with no corresponding
    /// `packages:` entry) produces a graph_key with nothing behind it. Same
    /// requirement as the npm case: a clean `anyhow::Result` error, never a
    /// process panic.
    #[tokio::test]
    async fn pnpm_e2e_lifecycle_script_required_dependency_missing_from_packages_is_a_clean_error_not_a_panic()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(
            dir.path(),
            "pnpm-lock.yaml",
            r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      native-thing:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  native-thing@1.0.0:
    resolution: {integrity: sha512-xyz}
    requiresBuild: true
snapshots:
  native-thing@1.0.0:
    dependencies:
      missing-helper: 1.0.0
"#,
        );
        let provider = Provider::with_config(
            dir.path().to_path_buf(),
            Config {
                pkgmanager: PkgManager::Pnpm,
                skip: Arc::new(Ignore::default()),
                walker: Arc::new(CachedWalker::disabled()),
                allow_scripts: vec!["native-thing".to_string()],
                tstool: toolchain::HOST.to_string(),
                testrunner: toolchain::VITEST.to_string(),
                test_glob: Vec::new(),
                linter: toolchain::OXLINT.to_string(),
                bundler: toolchain::ESBUILD.to_string(),
            },
        );
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr("native-thing", "1.0.0", "linux", "amd64");
        let err = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr,
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .err()
            .expect(
                "a snapshot dependency edge with no matching packages entry must be a clean \
                 error, not a panic",
            );
        let msg = match err {
            GetError::Other(e) => format!("{e:#}"),
            other => panic!("expected GetError::Other, got {other:?}"),
        };
        assert!(msg.contains("missing-helper"), "{msg}");
    }

    /// An npm workspace where `packages/a` declares a dependency on
    /// `native-thing`, resolved in the lockfile to a package restricted to
    /// `os=["win32"] cpu=["ia32"]` — a platform combination none of heph's
    /// three supported targets (`x86_64`/`aarch64`-linux-gnu,
    /// `aarch64-apple-darwin`) ever match, so this fixture exercises the
    /// mismatch deterministically regardless of which of them runs the test.
    /// `optional` toggles whether the dependency is declared in
    /// `optionalDependencies` (must be silently skipped) or `dependencies`
    /// (must hard-fail) — see `deps::resolve_package_deps`'s doc comment.
    fn npm_platform_restricted_fixture(optional: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": ["packages/*"]}"#,
        );
        let dep_field = if optional {
            "optionalDependencies"
        } else {
            "dependencies"
        };
        write(
            dir.path(),
            "packages/a/package.json",
            &format!(r#"{{"name": "a", "{dep_field}": {{"native-thing": "^1.0.0"}}}}"#),
        );
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root", "workspaces": ["packages/*"] },
                    "packages/a": { "name": "a" },
                    "node_modules/native-thing": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/native-thing/-/native-thing-1.0.0.tgz",
                        "integrity": "sha512-xyz",
                        "os": ["win32"],
                        "cpu": ["ia32"]
                    }
                }
            }"#,
        );
        dir
    }

    /// The bug this fixes: a platform-restricted `optionalDependencies` entry
    /// that IS resolved in the lockfile for a platform other than the one
    /// running the build (the flagship `optionalDependencies` use case — one
    /// npm package per platform, e.g. `@esbuild/darwin-arm64`) must be
    /// silently omitted from the package's deps, never wired as a
    /// `js_install` target-dep edge and never a hard `Provider::get` error.
    #[tokio::test]
    async fn npm_e2e_platform_mismatched_optional_dep_is_silently_omitted() {
        let dir = npm_platform_restricted_fixture(true);
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let addrs = get_deps_addrs(&provider, "packages/a").await;
        assert!(
            addrs.is_empty(),
            "a platform-mismatched optional dep must not be wired as a js_install dep: {addrs:?}"
        );
    }

    /// The same dependency, restriction, and platform mismatch as above, but
    /// declared as a required (non-optional) dependency this time — a
    /// required dependency that cannot be installed on this platform is a
    /// real, actionable problem and `Provider::get` must still hard-fail for
    /// it, naming the package.
    #[tokio::test]
    async fn npm_e2e_platform_mismatched_required_dep_is_a_hard_error() {
        let dir = npm_platform_restricted_fixture(false);
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(
                        PkgBuf::from("packages/a"),
                        PACKAGE_INFO_TARGET.to_string(),
                        Default::default(),
                    ),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        match result {
            Err(GetError::Other(e)) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("native-thing"), "must name the package: {msg}");
            }
            Ok(_) => panic!(
                "a platform-restricted required dep must fail Provider::get, not succeed silently"
            ),
            Err(GetError::NotFound) => {
                panic!("expected GetError::Other (a resolution failure), got NotFound")
            }
        }
    }

    // ---- M2: import-graph cross-validation wired into `Provider::get` ----
    //
    // `deps_config` now also builds the package's real import graph and
    // cross-checks it against its declared dependencies (see
    // `importgraph.rs`). These drive that end to end through the actual
    // `Provider::get` pipeline, not just `importgraph`'s own unit tests.

    /// A first-party source file imports a package present in `node_modules`
    /// (as an npm hoist would leave it) but never declared in the package's
    /// own `package.json` — `Provider::get` must fail loudly, naming the
    /// file, specifier, and package, rather than silently trusting M1's
    /// package.json-only wiring.
    #[tokio::test]
    async fn get_hard_fails_on_phantom_thirdparty_import_from_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": ["packages/*"]}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import _ from 'lodash';\n",
        );
        // Present on disk (as a hoisted install would leave it), but
        // `packages/a`'s package.json never declares it.
        write(
            dir.path(),
            "node_modules/lodash/package.json",
            r#"{"name": "lodash", "main": "index.js"}"#,
        );
        write(
            dir.path(),
            "node_modules/lodash/index.js",
            "module.exports = {};\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(
                        PkgBuf::from("packages/a"),
                        PACKAGE_INFO_TARGET.to_string(),
                        Default::default(),
                    ),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        match result {
            Err(GetError::Other(e)) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("packages/a/src/index.ts"), "{msg}");
                assert!(msg.contains("lodash"), "{msg}");
            }
            Ok(_) => {
                panic!("a phantom third-party import must fail Provider::get, not succeed silently")
            }
            Err(GetError::NotFound) => {
                panic!("expected GetError::Other (a resolution failure), got NotFound")
            }
        }
    }

    /// The same import, but `lodash` is properly declared this time —
    /// `Provider::get` must succeed.
    #[tokio::test]
    async fn get_succeeds_when_source_import_matches_a_declared_dependency() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": ["packages/*"]}"#,
        );
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import _ from 'lodash';\n",
        );
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root", "workspaces": ["packages/*"] },
                    "packages/a": { "name": "a", "dependencies": { "lodash": "^4.17.21" } },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        );
        write(
            dir.path(),
            "node_modules/lodash/package.json",
            r#"{"name": "lodash", "main": "index.js"}"#,
        );
        write(
            dir.path(),
            "node_modules/lodash/index.js",
            "module.exports = {};\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(
                        PkgBuf::from("packages/a"),
                        PACKAGE_INFO_TARGET.to_string(),
                        Default::default(),
                    ),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("a properly declared import must not fail Provider::get");
    }

    // ---- M3: `js_typecheck` Input scoping (`typecheck_deps_config`) ----
    //
    // Per the task, this is "the single most important test in this
    // milestone": a scoping mistake here either silently under-caches (a
    // `.d.ts` dependency change never busts the cache) or over-caches (every
    // package re-keys on any workspace file touch, defeating per-package
    // granularity). Deliberately does not touch `tsc` at all — see
    // `typecheck_deps_config`'s own doc comment — so it runs unconditionally,
    // unlike the driver-level `run()` tests in `driver_typecheck.rs`.

    fn dep_addrs(config: &HashMap<String, Value>, group: &str) -> Vec<String> {
        match config.get(group) {
            Some(Value::List(items)) => items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => panic!("expected string addr, got {other:?}"),
                })
                .collect(),
            None => vec![],
            other => panic!("expected list for group {group:?}, got {other:?}"),
        }
    }

    /// Build the same [`importgraph::ImportGraph`] `Provider::import_graph`
    /// would, for tests that call `typecheck_deps_config`/`test_deps_config`
    /// directly against a bare fixture rather than through a `Provider` (and
    /// therefore without its per-package cache — these tests exercise the
    /// pure Input-scoping logic, not the cache itself; the cache has its own
    /// dedicated tests, see `import_graph_is_shared_across_independent_callers`).
    fn build_graph_for_test(
        walker: &CachedWalker,
        workspace_root: &Path,
        pkg: &str,
    ) -> importgraph::ImportGraph {
        let pkg_dir = if pkg.is_empty() {
            workspace_root.to_path_buf()
        } else {
            workspace_root.join(pkg)
        };
        let tsconfig = importgraph::find_nearest_tsconfig(workspace_root, &pkg_dir);
        let import_resolvers = resolvers::Resolvers::new(tsconfig.as_deref());
        let resolve_cache = importgraph::ResolveCache::new();
        importgraph::build_package_import_graph(
            walker,
            workspace_root,
            pkg,
            &import_resolvers,
            &resolve_cache,
            tsconfig.as_deref(),
        )
        .expect("build import graph for test")
    }

    /// `typecheck_deps_config` with no lockfile/workspace-member context —
    /// what most of these tests need, since they exercise scoping behavior
    /// that doesn't touch an unresolved third-party/sibling import.
    fn call_typecheck_deps_config(
        walker: &CachedWalker,
        workspace_root: &Path,
        pkg: &str,
    ) -> anyhow::Result<(HashMap<String, Value>, String, String)> {
        let graph = build_graph_for_test(walker, workspace_root, pkg);
        typecheck_deps_config(
            walker,
            workspace_root,
            pkg,
            pkg,
            &graph,
            None,
            None,
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
    }

    #[test]
    fn typecheck_deps_config_scopes_inputs_to_firstparty_and_resolved_type_edge_not_whole_workspace()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        // Ambient on disk too (a real host install would leave this behind),
        // but resolution must go through the lockfile below regardless — see
        // `bundle_closure_step`'s identical fix: an edge that only resolved
        // via ambient `node_modules` is still routed through
        // `resolve_one_dependency`, never wired as a raw `fs:file` at that
        // path.
        write(
            dir.path(),
            "node_modules/pkg/package.json",
            r#"{"name": "pkg", "main": "index.js", "types": "index.d.ts"}"#,
        );
        write(
            dir.path(),
            "node_modules/pkg/index.js",
            "module.exports.x = 1;\n",
        );
        write(
            dir.path(),
            "node_modules/pkg/index.d.ts",
            "export declare const x: number;\n",
        );
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"pkg": "^1.0.0"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import type { x } from 'pkg';\nexport const y = 1;\n",
        );
        // An unrelated file elsewhere in the workspace — must NOT appear in
        // packages/a's declared inputs, proving the scoping is per-package,
        // not workspace-wide (the point of this whole test).
        write(
            dir.path(),
            "packages/c/unrelated.ts",
            "export const unrelated = 1;\n",
        );

        let lockfile = Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/pkg": {
                        "version": "1.0.0",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        )
        .expect("parse lockfile");
        let resolved_graph = lockfile.resolved_graph().unwrap();

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let (deps, tsconfig_path, tsconfig_content) = typecheck_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a",
            &graph,
            Some(&lockfile),
            Some(&resolved_graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .expect("build typecheck deps config");

        assert!(
            tsconfig_path.is_empty(),
            "no tsconfig anywhere on the ancestor chain: {tsconfig_path:?}"
        );
        assert!(tsconfig_content.is_empty());
        assert!(!deps.contains_key("tsconfig"));

        let src_addrs = dep_addrs(&deps, "");
        assert_eq!(src_addrs.len(), 1, "{src_addrs:?}");
        assert!(
            src_addrs[0].contains("packages/a/src/index.ts"),
            "{src_addrs:?}"
        );

        let type_addrs = dep_addrs(&deps, "types");
        assert_eq!(type_addrs.len(), 1, "{type_addrs:?}");
        assert!(
            type_addrs[0].contains("name=pkg") && type_addrs[0].contains("@heph/js/node_modules"),
            "{type_addrs:?}"
        );

        for addr in src_addrs.iter().chain(type_addrs.iter()) {
            assert!(
                !addr.contains("unrelated"),
                "an unrelated workspace file must not be a declared input: {addr}"
            );
        }
    }

    #[test]
    fn typecheck_deps_config_includes_tsconfig_group_and_content_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/tsconfig.json",
            r#"{"compilerOptions":{"strict":true}}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, tsconfig_path, tsconfig_content) =
            call_typecheck_deps_config(&walker, dir.path(), "packages/a")
                .expect("build typecheck deps config");

        assert_eq!(tsconfig_path, "packages/a/tsconfig.json");
        assert_eq!(tsconfig_content, r#"{"compilerOptions":{"strict":true}}"#);
        let tsconfig_addrs = dep_addrs(&deps, "tsconfig");
        assert_eq!(tsconfig_addrs.len(), 1);
        assert!(tsconfig_addrs[0].contains("packages/a/tsconfig.json"));
    }

    /// Same "nearest ancestor" walk-up rule real `tsc` resolution uses (and
    /// the same one `find_nearest_tsconfig` already implements for the
    /// import graph) — a package without its own tsconfig inherits the
    /// nearest ancestor's, *provided* that ancestor's `include` provably
    /// confines it to this package (see `check_tsconfig_scope`'s doc) — an
    /// unscoped shared config is covered separately below, since trusting it
    /// unconditionally would be unsound whenever another package sits under
    /// the same ancestor.
    #[test]
    fn typecheck_deps_config_walks_up_to_nearest_ancestor_tsconfig() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "tsconfig.json",
            r#"{"compilerOptions":{"strict":false},"include":["packages/a/**/*"]}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let (_deps, tsconfig_path, tsconfig_content) =
            call_typecheck_deps_config(&walker, dir.path(), "packages/a")
                .expect("build typecheck deps config");
        assert_eq!(tsconfig_path, "tsconfig.json");
        assert_eq!(
            tsconfig_content,
            r#"{"compilerOptions":{"strict":false},"include":["packages/a/**/*"]}"#
        );
    }

    /// Hermeticity/feature-quality M3 review finding: a shared ancestor
    /// tsconfig with no `include`/`files` at all defaults (per real `tsc`
    /// semantics) to every source file under *its own* directory — which, for
    /// a package with no tsconfig of its own, is broader than just that
    /// package whenever a sibling package sits under the same ancestor. This
    /// must be a loud `Provider::get` error, not a silently unsound
    /// per-package Input set.
    #[test]
    fn typecheck_deps_config_rejects_unscoped_shared_ancestor_tsconfig() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "tsconfig.json",
            r#"{"compilerOptions":{"strict":false}}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        // A sibling package under the very same (unscoped) ancestor
        // tsconfig — proof this shape is genuinely ambiguous, not merely
        // theoretically so.
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export const y = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let err = call_typecheck_deps_config(&walker, dir.path(), "packages/a")
            .expect_err("an unscoped shared ancestor tsconfig must not be silently trusted");
        let msg = format!("{err:#}");
        assert!(msg.contains("include"), "{msg}");
    }

    /// An ancestor tsconfig whose `include` reaches beyond the requesting
    /// package (covers a sibling too) is exactly as unsound as no `include`
    /// at all — this is the actual shared-program case named in the
    /// hermeticity review, not just the no-`include` default.
    #[test]
    fn typecheck_deps_config_rejects_ancestor_tsconfig_include_reaching_a_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "tsconfig.json",
            r#"{"include":["packages/a/**/*","packages/b/**/*"]}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);

        let walker = CachedWalker::disabled();
        let err = call_typecheck_deps_config(&walker, dir.path(), "packages/a").expect_err(
            "a shared tsconfig `include` reaching a sibling package must not be silently trusted",
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("packages/b/**/*"), "{msg}");
    }

    /// Code-quality M3 review finding: a plain (non-`import type`) import
    /// that resolves to a workspace-sibling file must still be a declared
    /// Input — `tsc` reads the imported file's types regardless of whether
    /// the importing syntax used `import type`.
    #[test]
    fn typecheck_deps_config_scopes_plain_runtime_import_of_workspace_sibling() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"b": "workspace:*"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import { helper } from '../../b/helper';\nexport const y = helper();\n",
        );
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        write(
            dir.path(),
            "packages/b/helper.ts",
            "export function helper(): number { return 1; }\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, _tsconfig_path, _tsconfig_content) =
            call_typecheck_deps_config(&walker, dir.path(), "packages/a")
                .expect("build typecheck deps config");

        let type_addrs = dep_addrs(&deps, "types");
        assert!(
            type_addrs
                .iter()
                .any(|a| a.contains("packages/b/helper.ts")),
            "a plain runtime import of a workspace sibling must be a declared Input: \
             {type_addrs:?}"
        );
    }

    /// Feature-quality M3 review finding: an import naming a third-party
    /// package that never resolves on disk at all (no `node_modules`
    /// installed — the realistic steady state before an out-of-band
    /// install) must still become a declared Input, via the same
    /// lockfile-driven addressing `Provider::deps_config` already uses for
    /// runtime deps — not silently omitted just because there's nothing to
    /// walk on disk.
    #[test]
    fn typecheck_deps_config_declares_thirdparty_type_input_with_no_ambient_node_modules() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"zod": "^3.0.0"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import type { z } from 'zod';\nexport const y = 1;\n",
        );
        // Deliberately no `node_modules` anywhere in this fixture.

        let lockfile = Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/zod": {
                        "version": "3.0.0",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        )
        .expect("parse lockfile");
        let resolved_graph = lockfile.resolved_graph().unwrap();

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let (deps, _tsconfig_path, _tsconfig_content) = typecheck_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a",
            &graph,
            Some(&lockfile),
            Some(&resolved_graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .expect("build typecheck deps config");

        let type_addrs = dep_addrs(&deps, "types");
        assert!(
            type_addrs
                .iter()
                .any(|a| a.contains("name=zod") && a.contains("@heph/js/node_modules")),
            "an unresolved third-party type import must still declare a relocated \
             node_modules Input even absent ambient node_modules: {type_addrs:?}"
        );
    }

    /// The `js_typecheck` analog of `test_deps_config_declares_transitive_third_party_closure_
    /// not_just_direct_imports`: `a`'s source only reaches `outer` via a
    /// `.d.ts` chain (`import type`), but the lockfile records `outer`
    /// itself depending on `inner` — `tsc` follows a resolved package's own
    /// type-declaration imports transitively, so `inner` must be declared
    /// too, not just the directly-imported name.
    #[test]
    fn typecheck_deps_config_declares_transitive_third_party_closure_not_just_direct_imports() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"outer": "^1.0.0"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import type { x } from 'outer';\nexport const y = 1;\n",
        );

        let lockfile = Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/outer": {
                        "version": "1.0.0",
                        "integrity": "sha512-outer",
                        "dependencies": { "inner": "2.0.0" }
                    },
                    "node_modules/inner": {
                        "version": "2.0.0",
                        "integrity": "sha512-inner"
                    }
                }
            }"#,
        )
        .expect("parse lockfile");
        let resolved_graph = lockfile.resolved_graph().unwrap();

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let (deps, _tsconfig_path, _tsconfig_content) = typecheck_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a",
            &graph,
            Some(&lockfile),
            Some(&resolved_graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .expect("build typecheck deps config");

        let type_addrs = dep_addrs(&deps, "types");
        assert!(
            type_addrs
                .iter()
                .any(|a| a.contains("name=outer") && a.contains("@heph/js/node_modules")),
            "the directly-imported package must still be declared: {type_addrs:?}"
        );
        assert!(
            type_addrs
                .iter()
                .any(|a| a.contains("name=inner") && a.contains("@heph/js/node_modules")),
            "a resolved package's own transitive dependency must also be declared, or `tsc` \
             hits an unresolved import one edge deeper: {type_addrs:?}"
        );
    }

    /// Hermeticity M3 review finding: a tsconfig's `extends` chain is a real
    /// config file `tsc --project` merges in — it must be declared as an
    /// additional Input (so the sandbox actually stages it) and its content
    /// must bust the cache the same way the leaf's own content does.
    #[test]
    fn typecheck_deps_config_declares_and_hashes_tsconfig_extends_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "tsconfig.base.json",
            r#"{"compilerOptions":{"strict":false}}"#,
        );
        write(
            dir.path(),
            "packages/a/tsconfig.json",
            r#"{"extends":"../../tsconfig.base.json","include":["src/**/*"]}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, _tsconfig_path, tsconfig_content) =
            call_typecheck_deps_config(&walker, dir.path(), "packages/a")
                .expect("build typecheck deps config");

        let tsconfig_addrs = dep_addrs(&deps, "tsconfig");
        assert_eq!(tsconfig_addrs.len(), 2, "{tsconfig_addrs:?}");
        assert!(
            tsconfig_addrs
                .iter()
                .any(|a| a.contains("packages/a/tsconfig.json"))
        );
        assert!(
            tsconfig_addrs
                .iter()
                .any(|a| a.contains("tsconfig.base.json")),
            "the extends-chain ancestor must be a declared Input too: {tsconfig_addrs:?}"
        );
        assert!(
            tsconfig_content.contains("strict"),
            "the extended base config's content must be folded into the hashed content: \
             {tsconfig_content:?}"
        );

        // Editing only the base config (leaf byte-identical) must change the
        // hashed content — this is the actual cache-soundness claim, not
        // just "the file is listed".
        write(
            dir.path(),
            "tsconfig.base.json",
            r#"{"compilerOptions":{"strict":true}}"#,
        );
        let (_deps2, _p2, tsconfig_content2) =
            call_typecheck_deps_config(&walker, dir.path(), "packages/a")
                .expect("build typecheck deps config after editing the base config");
        assert_ne!(
            tsconfig_content, tsconfig_content2,
            "editing only the extended base config must change the hashed tsconfig content"
        );
    }

    /// Feature-quality/hermeticity M3 review finding: the first-party Input
    /// set must honor the tsconfig's own `include`/`exclude`, not just walk
    /// every source file under the package directory — otherwise editing a
    /// file `tsc` never reads (excluded) still busts the cache.
    #[test]
    fn typecheck_deps_config_honors_tsconfig_exclude() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/tsconfig.json",
            r#"{"exclude":["src/legacy/**"]}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/src/legacy/old.ts",
            "export const old = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, _tsconfig_path, _tsconfig_content) =
            call_typecheck_deps_config(&walker, dir.path(), "packages/a")
                .expect("build typecheck deps config");

        let src_addrs = dep_addrs(&deps, "");
        assert!(
            src_addrs.iter().any(|a| a.contains("src/index.ts")),
            "{src_addrs:?}"
        );
        assert!(
            !src_addrs.iter().any(|a| a.contains("legacy")),
            "an excluded file must not be a declared Input: {src_addrs:?}"
        );
    }

    // ---- `Provider::get` end to end for `js_typecheck` — gated on a real
    // `tsc` binary being available in this devenv (querying `tsc --version`
    // is unavoidably a real subprocess call — see `toolchain.rs` module
    // docs), unlike the Input-scoping tests above.

    fn find_real_tsc_for_test() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("tsc");
            if std::fs::metadata(&cand)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return Some(cand);
            }
        }
        None
    }

    #[tokio::test]
    #[ignore = "requires a real `tsc` on PATH — devenv.nix provisions no Node/TypeScript \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with TypeScript installed"]
    async fn get_resolves_js_typecheck_target_end_to_end() {
        find_real_tsc_for_test().expect(
            "this test is #[ignore]d precisely because `tsc` isn't guaranteed on PATH — it was \
             run explicitly, so a missing `tsc` here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(
                        PkgBuf::from("packages/a"),
                        TYPECHECK_TARGET.to_string(),
                        Default::default(),
                    ),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_typecheck target_spec");
        assert_eq!(resp.target_spec.driver, "js_typecheck");
        assert!(resp.target_spec.config.contains_key("tsc_version"));
    }

    // ---- js_test: test_deps_config Input scoping (no real vitest/jest needed) ----

    /// `test_deps_config` with no lockfile/workspace-member context, matching
    /// vitest's default config filenames — what most of these tests need,
    /// since they exercise scoping behavior that doesn't touch an unresolved
    /// third-party/sibling import.
    fn call_test_deps_config(
        walker: &CachedWalker,
        workspace_root: &Path,
        pkg: &str,
        test_file_rel: &str,
    ) -> anyhow::Result<(HashMap<String, Value>, String, String)> {
        let graph = build_graph_for_test(walker, workspace_root, pkg);
        test_deps_config(
            workspace_root,
            pkg,
            pkg,
            test_file_rel,
            &graph,
            None,
            None,
            &BTreeMap::new(),
            "linux",
            "amd64",
            toolchain::VITEST,
            runner_config_candidates(toolchain::VITEST)?,
        )
    }

    /// The single most important test in this milestone (per the task): two
    /// test files in the *same package*, each importing a different sibling
    /// source file, must get disjoint `""`-group Input sets — proving
    /// per-test-file, not per-package, granularity. This is exactly what
    /// Turborepo/Nx cannot do (package granularity only).
    #[test]
    fn test_deps_config_scopes_to_one_test_file_not_the_whole_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(dir.path(), "packages/a/src/a.ts", "export const a = 1;\n");
        write(dir.path(), "packages/a/src/b.ts", "export const b = 2;\n");
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "import { a } from './a';\ntest('a', () => a);\n",
        );
        write(
            dir.path(),
            "packages/a/src/b.test.ts",
            "import { b } from './b';\ntest('b', () => b);\n",
        );

        let walker = CachedWalker::disabled();
        let (deps_a, _, _) = call_test_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build test deps config for a.test.ts");
        let (deps_b, _, _) = call_test_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a/src/b.test.ts",
        )
        .expect("build test deps config for b.test.ts");

        let src_a = dep_addrs(&deps_a, "");
        let src_b = dep_addrs(&deps_b, "");

        assert!(
            src_a.iter().any(|a| a.contains("a.test.ts"))
                && src_a.iter().any(|a| a.contains("src/a.ts")),
            "{src_a:?}"
        );
        assert!(
            !src_a
                .iter()
                .any(|a| a.contains("b.test.ts") || a.contains("src/b.ts")),
            "a.test.ts's own closure must not include b.test.ts or b.ts: {src_a:?}"
        );

        assert!(
            src_b.iter().any(|a| a.contains("b.test.ts"))
                && src_b.iter().any(|a| a.contains("src/b.ts")),
            "{src_b:?}"
        );
        assert!(
            !src_b
                .iter()
                .any(|a| a.contains("a.test.ts") || a.contains("src/a.ts")),
            "b.test.ts's own closure must not include a.test.ts or a.ts: {src_b:?}"
        );
    }

    /// A file reached transitively (not just directly imported) must still
    /// be declared — the closure follows the whole chain, not one hop.
    #[test]
    fn test_deps_config_includes_transitively_imported_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/deep.ts",
            "export const deep = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/src/helper.ts",
            "export { deep } from './deep';\n",
        );
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "import { deep } from './helper';\ntest('a', () => deep);\n",
        );
        // An unrelated workspace file elsewhere entirely — must not appear.
        write(
            dir.path(),
            "packages/c/unrelated.ts",
            "export const z = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, _, _) = call_test_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build test deps config");

        let src_addrs = dep_addrs(&deps, "");
        assert!(
            src_addrs.iter().any(|a| a.contains("helper.ts")),
            "{src_addrs:?}"
        );
        assert!(
            src_addrs.iter().any(|a| a.contains("deep.ts")),
            "a transitively-reached file must be declared: {src_addrs:?}"
        );
        assert!(
            !src_addrs.iter().any(|a| a.contains("unrelated")),
            "an unrelated workspace file must not be a declared input: {src_addrs:?}"
        );
    }

    #[test]
    fn test_deps_config_includes_runner_config_group_and_content_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "vitest.config.ts",
            "export default { test: {} };\n",
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => 1);\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, runner_config_path, runner_config_content) = call_test_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build test deps config");

        assert_eq!(runner_config_path, "vitest.config.ts");
        assert_eq!(runner_config_content, "export default { test: {} };\n");
        let runner_config_addrs = dep_addrs(&deps, "runner_config");
        assert_eq!(runner_config_addrs.len(), 1);
        assert!(runner_config_addrs[0].contains("vitest.config.ts"));
    }

    #[test]
    fn test_deps_config_no_runner_config_group_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => 1);\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, runner_config_path, runner_config_content) = call_test_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build test deps config");

        assert!(runner_config_path.is_empty());
        assert!(runner_config_content.is_empty());
        assert!(!deps.contains_key("runner_config"));
    }

    /// The completeness gap beyond direct imports: `a`'s test file only
    /// imports `outer`, but the lockfile records `outer` itself depending on
    /// `inner` — real npm packages routinely `require`/`import` their own
    /// dependencies internally (`axios` needing `follow-redirects`, the
    /// motivating real-world case). Both must be declared, or the moment
    /// `outer`'s own code runs under vitest, it hits `Cannot find module
    /// 'inner'` one edge deeper than anything a first-party import graph
    /// alone would ever see.
    #[test]
    fn test_deps_config_declares_transitive_third_party_closure_not_just_direct_imports() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"outer": "^1.0.0"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "import outer from 'outer';\ntest('a', () => outer());\n",
        );

        let lockfile = Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/outer": {
                        "version": "1.0.0",
                        "integrity": "sha512-outer",
                        "dependencies": { "inner": "2.0.0" }
                    },
                    "node_modules/inner": {
                        "version": "2.0.0",
                        "integrity": "sha512-inner"
                    }
                }
            }"#,
        )
        .expect("parse lockfile");
        let resolved_graph = lockfile.resolved_graph().unwrap();

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let (deps, _, _) = test_deps_config(
            dir.path(),
            "packages/a",
            "packages/a",
            "packages/a/src/a.test.ts",
            &graph,
            Some(&lockfile),
            Some(&resolved_graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
            toolchain::VITEST,
            runner_config_candidates(toolchain::VITEST).expect("vitest is supported"),
        )
        .expect("build test deps config");

        let external_addrs = dep_addrs(&deps, "external");
        assert!(
            external_addrs
                .iter()
                .any(|a| a.contains("name=outer") && a.contains("@heph/js/node_modules")),
            "the directly-imported package must still be declared: {external_addrs:?}"
        );
        assert!(
            external_addrs
                .iter()
                .any(|a| a.contains("name=inner") && a.contains("@heph/js/node_modules")),
            "a resolved package's own transitive dependency (never directly imported by \
             first-party code) must also be declared, or the real vitest run hits `Cannot \
             find module` one edge deeper: {external_addrs:?}"
        );
    }

    /// Confirmed live: `vitest` (a `devDependency`, never imported by the
    /// test file's own source at all — reachable only via
    /// `resolve_transitive_closure`'s manifest-declared seed) depends on
    /// `vite`, which depends on `rolldown`, which ships its native binding
    /// as an `optionalDependencies` entry per platform. The current-platform
    /// binding must be declared even though it's three hops from the seed
    /// and the intermediate edge is optional, not required; the
    /// other-platform sibling must never be, so the sandbox never collides
    /// on it.
    #[test]
    fn test_deps_config_declares_transitive_optional_dependency_matching_current_platform() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "devDependencies": {"vitest": "^1.0.0"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => 1);\n",
        );

        let lockfile = Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/vitest": {
                        "version": "1.0.0",
                        "integrity": "sha512-vitest",
                        "dependencies": { "vite": "6.0.0" }
                    },
                    "node_modules/vite": {
                        "version": "6.0.0",
                        "integrity": "sha512-vite",
                        "dependencies": { "rolldown": "1.0.0" }
                    },
                    "node_modules/rolldown": {
                        "version": "1.0.0",
                        "integrity": "sha512-rolldown",
                        "optionalDependencies": {
                            "@rolldown/binding-linux-x64-gnu": "1.0.0",
                            "@rolldown/binding-darwin-arm64": "1.0.0"
                        }
                    },
                    "node_modules/@rolldown/binding-linux-x64-gnu": {
                        "version": "1.0.0",
                        "integrity": "sha512-linux",
                        "os": ["linux"],
                        "cpu": ["x64"]
                    },
                    "node_modules/@rolldown/binding-darwin-arm64": {
                        "version": "1.0.0",
                        "integrity": "sha512-darwin",
                        "os": ["darwin"],
                        "cpu": ["arm64"]
                    }
                }
            }"#,
        )
        .expect("parse lockfile");
        let resolved_graph = lockfile.resolved_graph().unwrap();

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let (deps, _, _) = test_deps_config(
            dir.path(),
            "packages/a",
            "packages/a",
            "packages/a/src/a.test.ts",
            &graph,
            Some(&lockfile),
            Some(&resolved_graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
            toolchain::VITEST,
            runner_config_candidates(toolchain::VITEST).expect("vitest is supported"),
        )
        .expect("build test deps config");

        let external_addrs = dep_addrs(&deps, "external");
        assert!(
            external_addrs
                .iter()
                .any(|a| a.contains("name=@rolldown/binding-linux-x64-gnu")),
            "the current-platform optional binding, three hops from the devDependency seed, \
             must be declared: {external_addrs:?}"
        );
        assert!(
            !external_addrs
                .iter()
                .any(|a| a.contains("name=@rolldown/binding-darwin-arm64")),
            "the other-platform sibling must never be declared: {external_addrs:?}"
        );
    }

    /// Same lesson as `typecheck_deps_config_declares_thirdparty_type_input_with_no_ambient_node_modules`:
    /// an unresolved third-party import (no `node_modules` on disk at all)
    /// must still be resolved to a `js_install` Input via the lockfile —
    /// never by walking `oxc_resolver` paths against an absent ambient
    /// `node_modules` (the M3-review lesson this task explicitly calls out).
    #[test]
    fn test_deps_config_declares_thirdparty_input_with_no_ambient_node_modules() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "devDependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "import _ from 'lodash';\ntest('a', () => _.identity(1));\n",
        );
        // Deliberately no `node_modules` anywhere in this fixture.

        let lockfile = Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        )
        .expect("parse lockfile");
        let resolved_graph = lockfile.resolved_graph().unwrap();

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let (deps, _, _) = test_deps_config(
            dir.path(),
            "packages/a",
            "packages/a",
            "packages/a/src/a.test.ts",
            &graph,
            Some(&lockfile),
            Some(&resolved_graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
            toolchain::VITEST,
            runner_config_candidates(toolchain::VITEST).expect("vitest is supported"),
        )
        .expect("build test deps config");

        let external_addrs = dep_addrs(&deps, "external");
        assert!(
            external_addrs
                .iter()
                .any(|a| a.contains("name=lodash") && a.contains("@heph/js/node_modules")),
            "an unresolved third-party import must still declare a relocated node_modules \
             Input even absent ambient node_modules: {external_addrs:?}"
        );
    }

    /// The exact bug a real vitest.config.ts hit: `@vitejs/plugin-react` was
    /// never staged in the sandbox at all — the test *file* never imports
    /// it, only the runner config does, and only the file's own bare
    /// imports were ever fed through `deps::resolve_one_dependency`. Proves
    /// `test_deps_config` now declares a `js_install` Input for a plugin
    /// the *config* imports, not just ones the test file imports.
    ///
    /// The config lives at the *workspace root*, shared across packages via
    /// `find_nearest_test_runner_config`'s ancestor walk (see
    /// `find_nearest_test_runner_config_walks_up_like_tsconfig`) — a real,
    /// common monorepo shape, and deliberately not inside `packages/a`
    /// itself: a package-local config would already be swept into `graph`
    /// by `package_source_files`'s own directory walk and validated by the
    /// pre-existing `check_phantom_dependencies` call, which would make this
    /// test pass for the wrong reason and not actually exercise the new
    /// runner-config-specific staging/check this fixes.
    #[test]
    fn test_deps_config_declares_third_party_input_from_runner_configs_own_plugin_import() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "devDependencies": {"@vitejs/plugin-react": "^4.0.0"}}"#,
        );
        write(
            dir.path(),
            "vitest.config.ts",
            "import react from '@vitejs/plugin-react';\n\
             export default { plugins: [react()], test: {} };\n",
        );
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => {});\n",
        );
        // Deliberately no `node_modules` anywhere in this fixture — same
        // "no ambient install" scenario the sibling test above proves for a
        // test file's own import.

        let lockfile = Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/@vitejs/plugin-react": {
                        "version": "4.0.0",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        )
        .expect("parse lockfile");
        let resolved_graph = lockfile.resolved_graph().unwrap();

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let (deps, _, _) = test_deps_config(
            dir.path(),
            "packages/a",
            "packages/a",
            "packages/a/src/a.test.ts",
            &graph,
            Some(&lockfile),
            Some(&resolved_graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
            toolchain::VITEST,
            runner_config_candidates(toolchain::VITEST).expect("vitest is supported"),
        )
        .expect("build test deps config");

        let external_addrs = dep_addrs(&deps, "external");
        assert!(
            external_addrs
                .iter()
                .any(|a| a.contains("name=@vitejs/plugin-react")
                    && a.contains("@heph/js/node_modules")),
            "a plugin only the runner config imports must still declare a relocated \
             node_modules Input: {external_addrs:?}"
        );
    }

    /// The other half, same shared-root-config shape as the sibling test
    /// above: a runner-config plugin that is genuinely undeclared (not
    /// merely absent from disk) must still hard-fail as a phantom
    /// dependency — this is not a free pass around
    /// `check_phantom_dependencies`, it's a second, equally strict check for
    /// a source `check_phantom_dependencies` itself never sees (a config
    /// file outside the package's own directory is never part of `graph`).
    #[test]
    fn test_deps_config_rejects_an_undeclared_runner_config_plugin_as_phantom() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "vitest.config.ts",
            "import react from '@vitejs/plugin-react';\n\
             export default { plugins: [react()], test: {} };\n",
        );
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => {});\n",
        );

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let err = test_deps_config(
            dir.path(),
            "packages/a",
            "packages/a",
            "packages/a/src/a.test.ts",
            &graph,
            None,
            None,
            &BTreeMap::new(),
            "linux",
            "amd64",
            toolchain::VITEST,
            runner_config_candidates(toolchain::VITEST).expect("vitest is supported"),
        )
        .expect_err("an undeclared runner-config plugin must be a phantom-dependency error");
        let msg = format!("{err:#}");
        assert!(msg.contains("@vitejs/plugin-react"), "{msg}");
        assert!(msg.contains("phantom dependency"), "{msg}");
    }

    #[test]
    fn runner_config_candidates_covers_both_supported_runners() {
        assert!(
            runner_config_candidates(toolchain::VITEST)
                .expect("vitest is supported")
                .contains(&"vitest.config.ts")
        );
        assert!(
            runner_config_candidates(toolchain::JEST)
                .expect("jest is supported")
                .contains(&"jest.config.js")
        );
    }

    #[test]
    fn runner_config_candidates_errors_on_unsupported_testrunner() {
        runner_config_candidates("mocha").expect_err("mocha must not be supported");
    }

    // ---- Provider::list / Provider::get: js_test per-test-file target discovery ----

    #[tokio::test]
    async fn list_discovers_one_js_test_target_per_matched_test_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/src/index.test.ts",
            "test('x', () => 1);\n",
        );
        write(
            dir.path(),
            "packages/a/src/other.spec.ts",
            "test('y', () => 1);\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let iter = provider
            .list(
                ListRequest {
                    request_id: "test".to_string(),
                    package: PkgBuf::from("packages/a"),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("list");
        let addrs: Vec<Addr> = iter.map(|r| r.expect("no per-entry error").addr).collect();

        let test_addrs: Vec<&Addr> = addrs.iter().filter(|a| a.name == TEST_TARGET).collect();
        assert_eq!(test_addrs.len(), 2, "{addrs:?}");
        let files: Vec<&String> = test_addrs
            .iter()
            .filter_map(|a| a.args.get("file"))
            .collect();
        assert!(files.iter().any(|f| f.contains("index.test.ts")));
        assert!(files.iter().any(|f| f.contains("other.spec.ts")));
        // The package_info target must still be present too.
        assert!(addrs.iter().any(|a| a.name == PACKAGE_INFO_TARGET));
    }

    #[tokio::test]
    async fn get_js_test_not_found_for_nonexistent_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert(
            "file".to_string(),
            "packages/a/src/does-not-exist.test.ts".to_string(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), TEST_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // ---- `js_test` addr `file` arg: path-escape / sandbox-isolation
    // rejection (code-quality review BLOCKER) ----

    #[test]
    fn reject_path_escape_allows_plain_relative_path() {
        reject_path_escape("file", "packages/a/src/index.test.ts")
            .expect("plain workspace-relative path is fine");
    }

    #[test]
    fn reject_path_escape_rejects_absolute_path() {
        reject_path_escape("file", "/etc/passwd").expect_err("absolute path must be rejected");
    }

    #[test]
    fn reject_path_escape_rejects_dotdot_escape_anywhere_in_the_path() {
        reject_path_escape("file", "../../../../etc/passwd")
            .expect_err("a leading `..` escape must be rejected");
        reject_path_escape("file", "packages/a/../../../etc/passwd").expect_err(
            "a `..` component anywhere in the path must be rejected, not just a leading one",
        );
    }

    #[test]
    fn path_under_package_confines_to_the_addressed_package() {
        assert!(path_under_package(
            "packages/a",
            "packages/a/src/index.test.ts"
        ));
        assert!(!path_under_package("packages/a", "packages/b/src/index.ts"));
        // A sibling directory that merely shares a prefix with the package
        // name must not be treated as "under" it.
        assert!(!path_under_package(
            "packages/a",
            "packages/a-other/src/index.ts"
        ));
        assert!(path_under_package("", "packages/a/src/index.test.ts"));
    }

    #[tokio::test]
    async fn get_js_test_rejects_absolute_file_arg() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert("file".to_string(), "/etc/passwd".to_string());
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), TEST_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        assert!(
            matches!(result, Err(GetError::Other(_))),
            "an absolute `file` arg must be rejected outright, never resolved against the real \
             host filesystem via `Path::join`'s absolute-replaces-base semantics"
        );
    }

    #[tokio::test]
    async fn get_js_test_rejects_dotdot_escaping_file_arg() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert(
            "file".to_string(),
            "packages/a/../../../../../../etc/passwd".to_string(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), TEST_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        assert!(
            matches!(result, Err(GetError::Other(_))),
            "a `..`-escaping `file` arg must be rejected"
        );
    }

    #[tokio::test]
    async fn get_js_test_rejects_file_outside_addressed_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert("file".to_string(), "packages/b/src/index.ts".to_string());
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), TEST_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        assert!(
            matches!(result, Err(GetError::Other(_))),
            "a real, existing file belonging to a different package must not be addressable as \
             packages/a's own js_test target"
        );
    }

    // ---- `Provider::from_options`: fail fast on an unsupported `testrunner`
    // (feature-quality review) ----

    #[test]
    fn from_options_rejects_unsupported_testrunner() {
        let mut opts: hplugin::config::Options = BTreeMap::new();
        opts.insert(
            "pkgmanager".to_string(),
            serde_yaml::Value::String("npm".to_string()),
        );
        opts.insert(
            "testrunner".to_string(),
            serde_yaml::Value::String("mocha".to_string()),
        );
        let walker = Arc::new(CachedWalker::disabled());
        let result =
            Provider::from_options(PathBuf::from("/does-not-matter"), &[], &[], &opts, walker);
        match result {
            Err(err) => assert!(
                err.to_string().contains("testrunner"),
                "error should name the rejected option: {err}"
            ),
            Ok(_) => panic!(
                "mocha is not a supported testrunner — Provider::from_options must fail fast at \
                 construction time, not defer to Provider::get"
            ),
        }
    }

    // ---- hermeticity M4 review: `js_test`'s runner-config resolution/scoping ----

    #[test]
    fn runner_config_candidates_includes_vite_config_fallback_for_vitest() {
        let candidates = runner_config_candidates(toolchain::VITEST).expect("vitest is supported");
        assert!(
            candidates.contains(&"vite.config.ts"),
            "vitest's documented `vite.config.ts`-only fallback must be checked too: {candidates:?}"
        );
    }

    #[test]
    fn test_deps_config_falls_back_to_vite_config_when_no_dedicated_vitest_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "vite.config.ts",
            "export default { test: { environment: 'node' } };\n",
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => 1);\n",
        );

        let walker = CachedWalker::disabled();
        let (_, runner_config_path, runner_config_content) = call_test_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build test deps config");

        assert_eq!(runner_config_path, "vite.config.ts");
        assert!(runner_config_content.contains("environment"));
    }

    #[test]
    fn test_deps_config_falls_back_to_jest_field_in_package_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "jest": {"testEnvironment": "node"}}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => 1);\n",
        );

        let walker = CachedWalker::disabled();
        let graph = build_graph_for_test(&walker, dir.path(), "packages/a");
        let (deps, runner_config_path, runner_config_content) = test_deps_config(
            dir.path(),
            "packages/a",
            "packages/a",
            "packages/a/src/a.test.ts",
            &graph,
            None,
            None,
            &BTreeMap::new(),
            "linux",
            "amd64",
            toolchain::JEST,
            runner_config_candidates(toolchain::JEST).expect("jest is supported"),
        )
        .expect("build test deps config");

        assert_eq!(runner_config_path, "package.json");
        assert!(runner_config_content.contains("testEnvironment"));
        let runner_config_addrs = dep_addrs(&deps, "runner_config");
        assert_eq!(runner_config_addrs.len(), 1);
        assert!(runner_config_addrs[0].contains("package.json"));
    }

    #[test]
    fn test_deps_config_declares_setup_files_referenced_by_runner_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "vitest.config.ts",
            "export default { test: { setupFiles: ['./vitest.setup.ts'] } };\n",
        );
        write(
            dir.path(),
            "vitest.setup.ts",
            "globalThis.__setup = true;\n",
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => 1);\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, _, _) = call_test_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build test deps config");

        let runner_config_addrs = dep_addrs(&deps, "runner_config");
        assert_eq!(runner_config_addrs.len(), 2, "{runner_config_addrs:?}");
        assert!(
            runner_config_addrs
                .iter()
                .any(|a| a.contains("vitest.config.ts"))
        );
        assert!(
            runner_config_addrs
                .iter()
                .any(|a| a.contains("vitest.setup.ts")),
            "a setupFiles-referenced file must be declared as its own Input too, or editing it \
             would produce a stale cache hit: {runner_config_addrs:?}"
        );
    }

    #[test]
    fn test_deps_config_declares_base_config_reached_via_relative_import() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "vitest.config.base.ts",
            "export default { test: { globals: true } };\n",
        );
        write(
            dir.path(),
            "vitest.config.ts",
            "import base from './vitest.config.base';\nexport default base;\n",
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "test('a', () => 1);\n",
        );

        let walker = CachedWalker::disabled();
        let (deps, _, _) = call_test_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build test deps config");

        let runner_config_addrs = dep_addrs(&deps, "runner_config");
        assert!(
            runner_config_addrs
                .iter()
                .any(|a| a.contains("vitest.config.base.ts")),
            "a shared base config reached via a relative import inside the leaf config must be \
             declared too: {runner_config_addrs:?}"
        );
    }

    // ---- js_lint: lint_deps_config Input scoping (no real oxlint/eslint
    // needed) ----

    /// `lint_deps_config` with no lockfile/workspace-member context — what
    /// most of these tests need, since they exercise scoping behavior that
    /// doesn't touch an unresolved eslint `extends`/`plugins` package.
    fn call_lint_deps_config(
        walker: &CachedWalker,
        workspace_root: &Path,
        pkg: &str,
        linter: &str,
    ) -> anyhow::Result<LintDepsConfig> {
        lint_deps_config(
            walker,
            workspace_root,
            pkg,
            pkg,
            linter,
            None,
            None,
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
    }

    #[test]
    fn lint_deps_config_declares_first_party_source_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        // An unrelated file elsewhere in the workspace must NOT appear —
        // proving per-package, not workspace-wide, scoping.
        write(
            dir.path(),
            "packages/b/unrelated.ts",
            "export const unrelated = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::OXLINT)
            .expect("build lint deps config");

        let src_addrs = dep_addrs(&result.deps, "");
        assert_eq!(src_addrs.len(), 1, "{src_addrs:?}");
        assert!(src_addrs[0].contains("packages/a/src/index.ts"));
        assert!(!src_addrs.iter().any(|a| a.contains("unrelated")));
    }

    #[test]
    fn lint_deps_config_oxlint_includes_config_group_and_content_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/.oxlintrc.json",
            r#"{"rules":{"no-console":"error"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::OXLINT)
            .expect("build lint deps config");

        assert_eq!(result.config_path, "packages/a/.oxlintrc.json");
        assert_eq!(result.config_content, r#"{"rules":{"no-console":"error"}}"#);
        let config_addrs = dep_addrs(&result.deps, "config");
        assert_eq!(config_addrs.len(), 1);
        assert!(config_addrs[0].contains("packages/a/.oxlintrc.json"));
        // oxlint has no type-aware rules — no tsconfig group at all.
        assert!(!result.deps.contains_key("tsconfig"));
        assert!(result.tsconfig_path.is_empty());
    }

    #[test]
    fn lint_deps_config_oxlint_no_config_group_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::OXLINT)
            .expect("build lint deps config");

        assert!(result.config_path.is_empty());
        assert!(result.config_content.is_empty());
        assert!(!result.deps.contains_key("config"));
    }

    /// Feature-quality M5 review finding: an earlier version fell back to a
    /// `package.json`'s own `"oxlint"`/`"eslintConfig"` field when no
    /// dedicated config file was found, then passed that `package.json`
    /// straight to `-c` — a shape neither tool actually reads that way (see
    /// `importgraph::find_nearest_package_json_field_config`'s doc). No
    /// dedicated `.oxlintrc.json` here, so `js_lint` must run with no config
    /// at all rather than fabricating one from the `"oxlint"` field.
    #[test]
    fn lint_deps_config_oxlint_has_no_package_json_field_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "oxlint": {"rules": {"no-console": "error"}}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::OXLINT)
            .expect("build lint deps config");

        assert!(result.config_path.is_empty());
        assert!(result.config_content.is_empty());
        assert!(!result.deps.contains_key("config"));
    }

    /// Same as above, for eslint's `"eslintConfig"` field.
    #[test]
    fn lint_deps_config_eslint_has_no_package_json_field_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "eslintConfig": {"rules": {"no-console": "error"}}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::ESLINT)
            .expect("build lint deps config");

        assert!(result.config_path.is_empty());
        assert!(result.config_content.is_empty());
        assert!(!result.deps.contains_key("config"));
    }

    /// **The specific M5 gap**: an eslint config with type-aware rules
    /// configured (`parserOptions.project`) must declare/hash the tsconfig
    /// AND its whole `extends` chain, exactly like `js_typecheck` — proven
    /// the same way `typecheck_deps_config_declares_and_hashes_tsconfig_extends_chain`
    /// proves it for `js_typecheck`: editing only the extends-chain
    /// ancestor (leaf byte-identical) must change the hashed content.
    #[test]
    fn lint_deps_config_eslint_type_aware_declares_and_hashes_tsconfig_extends_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "tsconfig.base.json",
            r#"{"compilerOptions":{"strict":false}}"#,
        );
        write(
            dir.path(),
            "packages/a/tsconfig.json",
            r#"{"extends":"../../tsconfig.base.json"}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/eslint.config.js",
            "export default [{ languageOptions: { parserOptions: { project: \
             './tsconfig.json' } } }];\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::ESLINT)
            .expect("build lint deps config");

        assert_eq!(result.tsconfig_path, "packages/a/tsconfig.json");
        let tsconfig_addrs = dep_addrs(&result.deps, "tsconfig");
        assert_eq!(tsconfig_addrs.len(), 2, "{tsconfig_addrs:?}");
        assert!(
            tsconfig_addrs
                .iter()
                .any(|a| a.contains("packages/a/tsconfig.json"))
        );
        assert!(
            tsconfig_addrs
                .iter()
                .any(|a| a.contains("tsconfig.base.json")),
            "the extends-chain ancestor must be a declared Input too: {tsconfig_addrs:?}"
        );
        assert!(
            result.tsconfig_content.contains("strict"),
            "the extended base config's content must be folded into the hashed content: {:?}",
            result.tsconfig_content
        );

        // Editing only the base config (leaf byte-identical) must change the
        // hashed content — the actual cache-soundness claim, not just "the
        // file is listed".
        write(
            dir.path(),
            "tsconfig.base.json",
            r#"{"compilerOptions":{"strict":true}}"#,
        );
        let result2 = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::ESLINT)
            .expect("build lint deps config after editing the base config");
        assert_ne!(
            result.tsconfig_content, result2.tsconfig_content,
            "editing only the extended base config must change the hashed tsconfig content"
        );
    }

    #[test]
    fn lint_deps_config_eslint_not_type_aware_when_no_project_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/eslint.config.js",
            "export default [{ rules: { semi: 'error' } }];\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::ESLINT)
            .expect("build lint deps config");

        assert!(result.tsconfig_path.is_empty());
        assert!(result.tsconfig_content.is_empty());
        assert!(!result.deps.contains_key("tsconfig"));
    }

    /// `extends`/`plugins` npm packages must resolve via the lockfile
    /// (`deps::resolve_one_dependency`), not be silently dropped or treated
    /// as filesystem paths.
    #[test]
    fn lint_deps_config_eslint_extends_plugins_resolved_via_lockfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "devDependencies": {"eslint-plugin-react-hooks": "^4.0.0"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/eslint.config.js",
            "import reactHooks from 'eslint-plugin-react-hooks';\nexport default [{ plugins: \
             { 'react-hooks': reactHooks } }];\n",
        );

        let lockfile = Lockfile::parse(
            PkgManager::Npm,
            r#"{
                "packages": {
                    "": {},
                    "packages/a": { "name": "a" },
                    "node_modules/eslint-plugin-react-hooks": {
                        "version": "4.6.0",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        )
        .expect("parse lockfile");
        let resolved_graph = lockfile.resolved_graph().unwrap();

        let walker = CachedWalker::disabled();
        let result = lint_deps_config(
            &walker,
            dir.path(),
            "packages/a",
            "packages/a",
            toolchain::ESLINT,
            Some(&lockfile),
            Some(&resolved_graph),
            &BTreeMap::new(),
            "linux",
            "amd64",
        )
        .expect("build lint deps config");

        let plugin_addrs = dep_addrs(&result.deps, "eslint_plugins");
        assert!(
            plugin_addrs
                .iter()
                .any(|a| a.contains("name=eslint-plugin-react-hooks")
                    && a.contains("@heph/js/node_modules")),
            "an eslint config's `plugins` import must resolve to a relocated node_modules \
             Input via the lockfile: {plugin_addrs:?}"
        );
    }

    /// Code-quality M5 review finding: a multi-entry flat config (separate
    /// override blocks, each with its own `parserOptions.project`) must have
    /// **every** named tsconfig declared/hashed, not just the first. Before
    /// the fix, `detect_eslint_type_aware` stopped at the first match, so
    /// `tsconfig.test.json` here was invisible to the declared Input set.
    #[test]
    fn lint_deps_config_eslint_multi_entry_project_all_resolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/tsconfig.json",
            r#"{"compilerOptions":{}}"#,
        );
        write(
            dir.path(),
            "packages/a/tsconfig.test.json",
            r#"{"compilerOptions":{"types":["jest"]}}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/eslint.config.js",
            "export default [\n\
             { files: ['src/**/*.ts'], languageOptions: { parserOptions: { project: \
             './tsconfig.json' } } },\n\
             { files: ['test/**/*.ts'], languageOptions: { parserOptions: { project: \
             './tsconfig.test.json' } } },\n\
             ];\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::ESLINT)
            .expect("build lint deps config");

        let tsconfig_addrs = dep_addrs(&result.deps, "tsconfig");
        assert!(
            tsconfig_addrs
                .iter()
                .any(|a| a.contains("packages/a/tsconfig.json")),
            "{tsconfig_addrs:?}"
        );
        assert!(
            tsconfig_addrs
                .iter()
                .any(|a| a.contains("packages/a/tsconfig.test.json")),
            "the second override block's own tsconfig must also be a declared Input, not just \
             the first entry's: {tsconfig_addrs:?}"
        );
        assert!(
            result.tsconfig_content.contains("jest"),
            "the second tsconfig's content must be folded into the hashed content: {:?}",
            result.tsconfig_content
        );
    }

    /// Hermeticity + code-quality M5 review finding: a `parserOptions.project`
    /// value that resolves outside the workspace root must hard-error, never
    /// silently fall back to reading/hashing the arbitrary absolute host
    /// path. `config_dir.join(absolute_path)` yields `absolute_path` verbatim
    /// (`Path::join`'s documented behavior), so an absolute-path
    /// `parserOptions.project` is the simplest deterministic repro of the
    /// escape without relying on a specific number of `..` segments.
    #[test]
    fn lint_deps_config_eslint_parser_options_project_outside_workspace_root_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        write(outside.path(), "tsconfig.json", r#"{"compilerOptions":{}}"#);
        let outside_tsconfig = outside
            .path()
            .join("tsconfig.json")
            .to_string_lossy()
            .replace('\\', "/");

        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/eslint.config.js",
            &format!(
                "export default [{{ languageOptions: {{ parserOptions: {{ project: {:?} }} }} \
                 }}];\n",
                outside_tsconfig
            ),
        );

        let walker = CachedWalker::disabled();
        let err = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::ESLINT)
            .expect_err(
                "a parserOptions.project resolving outside the workspace root must be a hard \
                 error, not a silent same-path fallback",
            );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("outside the workspace root"),
            "error must name the actual failure, not just \"not found\": {msg}"
        );
    }

    /// Hermeticity M5 review finding: a legacy eslint config's own relative
    /// `extends` entry (`"extends": "./base.eslintrc.json"`) names a local
    /// sibling config file, not an npm package — it must be declared as an
    /// Input the same way `test_deps_config` already does for a shared
    /// test-runner config, so editing only the base config busts the cache.
    #[test]
    fn lint_deps_config_eslint_legacy_relative_extends_declared_as_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/base.eslintrc.json",
            r#"{"rules":{"no-console":"error"}}"#,
        );
        write(
            dir.path(),
            "packages/a/.eslintrc.json",
            r#"{"extends":"./base.eslintrc.json"}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::ESLINT)
            .expect("build lint deps config");

        let ref_addrs = dep_addrs(&result.deps, "config_refs");
        assert!(
            ref_addrs
                .iter()
                .any(|a| a.contains("packages/a/base.eslintrc.json")),
            "a legacy config's relative `extends` must be a declared js_lint Input: {ref_addrs:?}"
        );
    }

    /// Same gap, for a modern flat config's own relative `import`/`require`
    /// of a shared base config.
    #[test]
    fn lint_deps_config_eslint_flat_config_relative_import_declared_as_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/eslint-base.js",
            "export default [{ rules: { 'no-console': 'error' } }];\n",
        );
        write(
            dir.path(),
            "packages/a/eslint.config.js",
            "import base from './eslint-base.js';\nexport default [...base];\n",
        );
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let walker = CachedWalker::disabled();
        let result = call_lint_deps_config(&walker, dir.path(), "packages/a", toolchain::ESLINT)
            .expect("build lint deps config");

        let ref_addrs = dep_addrs(&result.deps, "config_refs");
        assert!(
            ref_addrs
                .iter()
                .any(|a| a.contains("packages/a/eslint-base.js")),
            "a flat config's relative import of a shared base config must be a declared js_lint \
             Input: {ref_addrs:?}"
        );
    }

    // ---- `Provider::get` end to end for `js_lint` — gated on a real
    // oxlint binary being available in this devenv (querying `--version` is
    // unavoidably a real subprocess call), unlike the Input-scoping tests
    // above.

    fn find_real_oxlint_for_test() -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("oxlint");
            if std::fs::metadata(&cand)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return Some(cand);
            }
        }
        None
    }

    #[tokio::test]
    #[ignore = "requires a real `oxlint` on PATH — devenv.nix provisions no Node/oxlint \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with oxlint installed"]
    async fn get_resolves_js_lint_target_end_to_end() {
        find_real_oxlint_for_test().expect(
            "this test is #[ignore]d precisely because `oxlint` isn't guaranteed on PATH — it \
             was run explicitly, so a missing `oxlint` here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(
                        PkgBuf::from("packages/a"),
                        LINT_TARGET.to_string(),
                        Default::default(),
                    ),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_lint target_spec");
        assert_eq!(resp.target_spec.driver, "js_lint");
        assert!(resp.target_spec.config.contains_key("linter_version"));
    }

    // ---- `Provider::get` end to end for `js_test` — gated on a real
    // vitest/jest binary being available in this devenv (querying
    // `--version` is unavoidably a real subprocess call), unlike the
    // Input-scoping tests above.

    fn find_real_bin_for_test(name: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(name);
            if std::fs::metadata(&cand)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                return Some(cand);
            }
        }
        None
    }

    #[tokio::test]
    #[ignore = "requires a real `vitest` on PATH — devenv.nix provisions no Node/vitest \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with vitest installed"]
    async fn get_resolves_js_test_target_end_to_end() {
        find_real_bin_for_test("vitest").expect(
            "this test is #[ignore]d precisely because vitest isn't guaranteed on PATH — it was \
             run explicitly, so a missing vitest here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.test.ts",
            "test('x', () => 1);\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert(
            "file".to_string(),
            "packages/a/src/index.test.ts".to_string(),
        );
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), TEST_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await
            .expect("get js_test target_spec");
        assert_eq!(resp.target_spec.driver, "js_test");
        assert!(resp.target_spec.config.contains_key("runner_version"));
    }

    // ---- M6: `js_bundle` entry-point validation, cross-package closure, ----
    // ---- and variant addr-arg wiring                                    ----
    //
    // `bundle_closure` (like `typecheck_deps_config`/`test_deps_config`) is
    // deliberately bundler-binary-free, so its Input-scoping is testable
    // unconditionally — this is the "single most important test" shape this
    // milestone's task calls out, applied to `js_bundle`'s own differentiator:
    // a *cross-package* transitive closure, not a one-hop trim.

    /// The package's own `package.json` `"main"` becomes the default entry
    /// — proven directly against `default_entry_for_package` (no real
    /// bundler binary needed; the full `Provider::get` path is additionally
    /// covered, `#[ignore]`d, by `get_resolves_js_bundle_target_end_to_end`
    /// below). `list_discovers_js_bundle_target_only_when_main_resolves`
    /// proves the same default drives `Provider::list`'s discovery.
    #[tokio::test]
    async fn default_entry_for_package_uses_package_json_main() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let entry = provider
            .default_entry_for_package(&PkgBuf::from("packages/a"))
            .await
            .expect("default_entry_for_package");
        assert_eq!(entry.as_deref(), Some("packages/a/src/index.ts"));
    }

    #[tokio::test]
    async fn get_js_bundle_errors_when_no_main_and_no_entry_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(
                        PkgBuf::from("packages/a"),
                        BUNDLE_TARGET.to_string(),
                        Default::default(),
                    ),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        let msg = expect_get_other_error(
            result,
            "no main field and no entry= override must fail, not silently succeed",
        );
        assert!(msg.contains("entry point") || msg.contains("main"), "{msg}");
    }

    /// Task requirement: entry-point path-escape validation gets its own
    /// unconditional test, mirroring `js_test`'s `file` addr arg tests.
    #[tokio::test]
    async fn get_js_bundle_rejects_dotdot_escaping_entry_arg() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert(
            "entry".to_string(),
            "packages/a/../../../etc/passwd".to_string(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), BUNDLE_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        let msg = expect_get_other_error(result, "a `..`-escaping entry arg must be rejected");
        assert!(msg.contains(".."));
    }

    #[tokio::test]
    async fn get_js_bundle_rejects_absolute_entry_arg() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert("entry".to_string(), "/etc/passwd".to_string());
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), BUNDLE_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        let msg = expect_get_other_error(result, "an absolute entry arg must be rejected");
        assert!(msg.contains("absolute"));
    }

    #[tokio::test]
    async fn get_js_bundle_rejects_entry_outside_addressed_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert("entry".to_string(), "packages/b/src/index.ts".to_string());
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), BUNDLE_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        let msg = expect_get_other_error(
            result,
            "an entry outside the addressed package must be rejected",
        );
        assert!(msg.contains("outside its own package"));
    }

    #[tokio::test]
    async fn get_js_bundle_not_found_for_nonexistent_entry_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let mut args = BTreeMap::new();
        args.insert(
            "entry".to_string(),
            "packages/a/src/does-not-exist.ts".to_string(),
        );
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), BUNDLE_TARGET.to_string(), args),
                    states: vec![],
                    executor,
                },
                &ct,
            )
            .await;
        assert!(matches!(result, Err(GetError::NotFound)));
    }

    // Task requirement: esm/cjs (and node/browser) must produce genuinely
    // different `js_bundle` config. Proven two ways: `driver_bundle.rs`'s
    // `parse_hash_changes_between_esm_and_cjs`/`parse_hash_changes_between_node_and_browser`
    // (no real bundler needed — those exercise `JsBundleDef::hash` directly),
    // and, end to end through `Provider::get`'s own addr-arg parsing,
    // `get_js_bundle_format_and_target_addr_args_flow_into_config_e2e` below
    // (`#[ignore]`d — reaching a *successful* `Provider::get` return
    // additionally resolves+queries the host `esbuild` binary via
    // `bundle_config`, unlike the rejection-path tests above which fail
    // before ever reaching it).

    #[tokio::test]
    async fn get_js_bundle_rejects_unsupported_format_arg() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let mut args = BTreeMap::new();
        args.insert("format".to_string(), "umd".to_string());
        let result = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), BUNDLE_TARGET.to_string(), args),
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ct,
            )
            .await;
        let msg = expect_get_other_error(result, "an unsupported format addr arg must be rejected");
        assert!(msg.contains("umd"));
    }

    /// `Provider::list` only lists the default `js_bundle` target for a
    /// package with a usable `"main"` — mirrors `js_test`'s "no matched
    /// files, no listed target" shape. An explicit `entry=` addr still works
    /// via `Provider::get` regardless (proven by the tests above).
    #[tokio::test]
    async fn list_discovers_js_bundle_target_only_when_main_resolves() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        // No "main" field at all.
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        // A "main" field naming a file that doesn't exist.
        write(
            dir.path(),
            "packages/c/package.json",
            r#"{"name": "c", "main": "src/missing.ts"}"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();

        for (pkg, expect_bundle) in [
            ("packages/a", true),
            ("packages/b", false),
            ("packages/c", false),
        ] {
            let responses = provider
                .list(
                    ListRequest {
                        request_id: "test".to_string(),
                        package: PkgBuf::from(pkg),
                        states: vec![],
                        executor: Arc::new(NoopExecutor),
                    },
                    &ct,
                )
                .await
                .expect("list")
                .collect::<anyhow::Result<Vec<_>>>()
                .expect("no per-entry errors");
            let has_bundle = responses.iter().any(|r| r.addr.name == BUNDLE_TARGET);
            let addr_names: Vec<&str> = responses.iter().map(|r| r.addr.name.as_str()).collect();
            assert_eq!(
                has_bundle, expect_bundle,
                "{pkg}: expected js_bundle listed = {expect_bundle}, listed target names = \
                 {addr_names:?}"
            );
        }
    }

    // ---- `bundle_closure`: cross-package transitive-closure scoping ----

    /// The closure must recurse *through* a sibling workspace package, not
    /// stop at one hop the way `build_test_closure` deliberately does for
    /// `js_test`/`js_typecheck` — this is `js_bundle`'s stated
    /// differentiator (see `driver_bundle.rs` module docs). Fixture: `a`'s
    /// entry reaches sibling `b` via a relative import (the same shape
    /// `importgraph.rs`'s own
    /// `build_test_closure_records_cross_package_import_as_external_one_hop`
    /// fixture uses to cross a package boundary, without needing a real
    /// `node_modules` symlink), `b`'s own entry imports a second first-party
    /// file `b/src/helper.ts` — both must be in the closure. A third,
    /// unrelated workspace file must NOT be in the closure, proving the
    /// whole-graph-not-whole-workspace scoping the task calls for.
    #[tokio::test]
    async fn bundle_closure_recurses_across_package_boundaries_and_excludes_unrelated_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "workspaces": ["packages/*"]}"#,
        );
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "dependencies": {"b": "workspace:*"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import { helper } from '../../b/src/index';\nhelper();\n",
        );
        write(dir.path(), "packages/b/package.json", r#"{"name": "b"}"#);
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export { helper } from './helper';\n",
        );
        write(
            dir.path(),
            "packages/b/src/helper.ts",
            "export function helper() {}\n",
        );
        // Unrelated workspace content nothing in the closure ever imports.
        write(
            dir.path(),
            "packages/a/src/unrelated.ts",
            "export const unrelated = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let closure = provider
            .bundle_closure("packages/a", "packages/a/src/index.ts")
            .await
            .expect("bundle_closure over the cross-package fixture");
        let files = &closure.files;

        assert!(
            files.contains("packages/a/src/index.ts"),
            "entry file itself must be in the closure: {files:?}"
        );
        assert!(
            files.contains("packages/b/src/index.ts"),
            "the sibling package's own entry, reached one hop out, must be in the closure: \
             {files:?}"
        );
        assert!(
            files.contains("packages/b/src/helper.ts"),
            "a file the sibling package's entry itself imports — reached TWO hops from js_bundle's \
             own entry — must also be in the closure; this is the whole point of not reusing \
             build_test_closure's one-hop trim: {files:?}"
        );
        assert!(
            !files.contains("packages/a/src/unrelated.ts"),
            "a workspace file nothing in the closure imports must NOT be declared as an input \
             (whole-graph, not whole-workspace): {files:?}"
        );
        assert!(
            closure.external_addrs.is_empty(),
            "every import in this fixture resolved to first-party content; nothing should have \
             been treated as third-party: {:?}",
            closure.external_addrs
        );
    }

    /// An unresolved bare specifier (the realistic steady state on a fresh
    /// checkout with no `node_modules` installed yet) inside the closure
    /// must resolve to the third-party package's `js_install` addr via the
    /// lockfile-driven mechanism — never left silently unaddressed, and
    /// never resolved by walking an ambient `node_modules` on disk (bug
    /// class (b) this milestone's task calls out).
    #[tokio::test]
    async fn bundle_closure_resolves_thirdparty_import_via_lockfile_with_no_ambient_node_modules() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "dependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(dir.path(), "src/index.ts", "import _ from 'lodash';\n");
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root", "dependencies": { "lodash": "^4.17.21" } },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        );
        // Deliberately no `node_modules/lodash` on disk — the realistic
        // fresh-checkout state this test is named for.

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let closure = provider
            .bundle_closure("", "src/index.ts")
            .await
            .expect("bundle_closure with a lockfile-resolved, not-yet-installed third-party dep");

        assert!(closure.files.contains("src/index.ts"));
        assert_eq!(
            closure.external_addrs.len(),
            1,
            "{:?}",
            closure.external_addrs
        );
        let addr = closure
            .external_addrs
            .iter()
            .next()
            .expect("one external addr");
        assert!(
            addr.contains("@heph/js/node_modules")
                && addr.contains("name=lodash")
                && addr.contains("version=4.17.21"),
            "must be the lockfile-resolved relocated node_modules addr, not an ambient \
             node_modules path: {addr}"
        );
        assert_eq!(
            closure.external_names,
            BTreeSet::from(["lodash".to_string()]),
            "the bare specifier name esbuild's own --external:<name> flag needs must be \
             captured alongside the resolved addr: {:?}",
            closure.external_names
        );
    }

    // ---- bundler config discovery (bundle_deps_config: no real bundler binary needed) ----

    #[tokio::test]
    async fn bundle_deps_config_declares_bundler_config_when_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/esbuild.config.json",
            r#"{"external": ["react", "react-dom"]}"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let result = provider
            .bundle_deps_config(&PkgBuf::from("packages/a"), "packages/a/src/index.ts")
            .await
            .expect("bundle_deps_config with a real esbuild.config.json present");

        assert_eq!(result.bundler_config_path, "packages/a/esbuild.config.json");
        assert_eq!(result.external, vec!["react", "react-dom"]);
        assert!(
            dep_addrs(&result.deps, "bundler_config")
                .iter()
                .any(|a| a.contains("esbuild.config.json")),
            "the resolved bundler config file must be a declared \"bundler_config\" input: \
             {:?}",
            result.deps
        );
    }

    #[tokio::test]
    async fn bundle_deps_config_no_bundler_config_group_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let result = provider
            .bundle_deps_config(&PkgBuf::from("packages/a"), "packages/a/src/index.ts")
            .await
            .expect("bundle_deps_config with no esbuild.config.json");

        assert_eq!(result.bundler_config_path, "");
        assert_eq!(result.bundler_config_content, "");
        assert!(result.external.is_empty());
        assert!(dep_addrs(&result.deps, "bundler_config").is_empty());
    }

    /// Feature-quality M6 review BLOCKER: `--external` bundler flags were
    /// derived only from a bundler config file's own opt-in `"external"`
    /// array — the closure's own discovered third-party bare specifiers
    /// (the realistic case for essentially every real npm dependency) never
    /// reached it, so `esbuild --bundle` hard-failed on every real
    /// third-party import. Proves the union: a closure-discovered name
    /// (`lodash`, no config file at all) and a config-file-only name
    /// (`react`, via the config's `"external"` array) both end up in
    /// `BundleDepsConfig::external`.
    #[tokio::test]
    async fn bundle_deps_config_externals_union_closure_discovered_and_config_file_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name": "root", "dependencies": {"lodash": "^4.17.21"}}"#,
        );
        write(dir.path(), "src/index.ts", "import _ from 'lodash';\n");
        write(
            dir.path(),
            "package-lock.json",
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root", "dependencies": { "lodash": "^4.17.21" } },
                    "node_modules/lodash": {
                        "version": "4.17.21",
                        "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                        "integrity": "sha512-abc"
                    }
                }
            }"#,
        );
        // "moment" is never actually imported — an opt-in-only config entry
        // (e.g. externalizing a peer dep the entry doesn't itself import
        // yet) must still survive the union, not be dropped in favor of the
        // closure's own discoveries.
        write(
            dir.path(),
            "esbuild.config.json",
            r#"{"external": ["moment"]}"#,
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let result = provider
            .bundle_deps_config(&PkgBuf::from(""), "src/index.ts")
            .await
            .expect(
                "bundle_deps_config over a mixed closure-discovered/config-file external fixture",
            );

        assert_eq!(
            result.external,
            vec!["lodash".to_string(), "moment".to_string()],
            "must union the closure's own discovered third-party name (lodash, no config entry) \
             with the bundler config's opt-in name (moment, never actually imported): {:?}",
            result.external
        );
    }

    /// Code-quality M6 review BLOCKER: `js_bundle` never declared/staged/
    /// hashed the entry package's own tsconfig, even though esbuild reads
    /// `compilerOptions` (`paths`/`baseUrl`/`jsx`/decorators/target) from it
    /// the same way `tsc` does — a package using a tsconfig `paths` alias
    /// (a mainstream TS-monorepo pattern) would fail at real `esbuild`
    /// execution because the sandbox never had a `tsconfig.json` at all.
    /// Proves the resolved tsconfig is both declared as a `"tsconfig"` Input
    /// (so it's staged) and returned for direct hashing, for a package whose
    /// entry point only resolves via a `paths` alias.
    #[tokio::test]
    async fn bundle_deps_config_declares_and_hashes_tsconfig_for_a_paths_aliased_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/tsconfig.json",
            r#"{"compilerOptions": {"baseUrl": ".", "paths": {"@app/*": ["src/*"]}}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export { helper } from '@app/utils';\n",
        );
        write(
            dir.path(),
            "packages/a/src/utils.ts",
            "export function helper() {}\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let result = provider
            .bundle_deps_config(&PkgBuf::from("packages/a"), "packages/a/src/index.ts")
            .await
            .expect("bundle_deps_config over a paths-aliased entry point");

        assert_eq!(result.tsconfig_path, "packages/a/tsconfig.json");
        assert!(
            result.tsconfig_content.contains("@app/*"),
            "the resolved tsconfig's own raw content must be hashed directly: {:?}",
            result.tsconfig_content
        );
        assert!(
            dep_addrs(&result.deps, "tsconfig")
                .iter()
                .any(|a| a.contains("packages/a/tsconfig.json")),
            "the resolved tsconfig must be a declared \"tsconfig\" input so it's staged into the \
             sandbox at the path esbuild expects: {:?}",
            result.deps
        );
    }

    /// No tsconfig anywhere on the ancestor chain: `bundle_deps_config` must
    /// not declare a `"tsconfig"` group or fail — mirrors
    /// `bundle_deps_config_no_bundler_config_group_when_absent`'s identical
    /// "absence is not an error" shape for the bundler config.
    #[tokio::test]
    async fn bundle_deps_config_no_tsconfig_group_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let result = provider
            .bundle_deps_config(&PkgBuf::from("packages/a"), "packages/a/src/index.ts")
            .await
            .expect("bundle_deps_config with no tsconfig.json");

        assert_eq!(result.tsconfig_path, "");
        assert_eq!(result.tsconfig_content, "");
        assert!(dep_addrs(&result.deps, "tsconfig").is_empty());
    }

    /// Feature-quality/hermeticity M6 review finding: `bundle_closure`'s BFS
    /// is provably invariant across `js_bundle`'s `format`/`target` variant
    /// axis for the same entry point, but was recomputed from scratch on
    /// every `Provider::get` — unlike every other expensive per-target-kind
    /// computation in this file. Proves two independent `bundle_closure`
    /// calls for the same `(entry_pkg, entry_file_rel)` share one BFS
    /// (mirrors `import_graph_is_shared_across_independent_callers`'s own
    /// `graph_build_count` proof technique, one layer up: a shared
    /// `import_graph` call underneath is *also* memoized, so a second
    /// `bundle_closure` call that actually re-walks would still show as a
    /// second `import_graph` build).
    #[tokio::test]
    async fn bundle_closure_is_shared_across_independent_callers() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);

        provider
            .bundle_closure("packages/a", "packages/a/src/index.ts")
            .await
            .expect("first bundle_closure call");
        assert_eq!(
            provider
                .graph_build_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "first bundle_closure call must build the import graph exactly once"
        );

        provider
            .bundle_closure("packages/a", "packages/a/src/index.ts")
            .await
            .expect("second bundle_closure call for the same entry point (a different variant)");
        assert_eq!(
            provider
                .graph_build_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a second bundle_closure call for the same (entry_pkg, entry_file_rel) — e.g. a \
             sibling format=cjs variant of the same package — must reuse the memoized closure, \
             not re-walk the BFS (and, one layer down, not re-build the import graph either)"
        );
    }

    // ---- run() precondition: Provider::get end to end, gated on a real ----
    // ---- esbuild binary being available in this devenv                 ----
    //
    // Everything above (bundle_closure/bundle_deps_config) tests Input-scoping
    // behavior and needs no real bundler. `Provider::get` for `js_bundle`
    // additionally resolves+queries the host `esbuild` binary's own
    // `--version` (see `resolved_host_bundler`), so these two mirror
    // `get_resolves_js_typecheck_target_end_to_end`'s identical `#[ignore]`
    // gating and reasoning.

    #[tokio::test]
    #[ignore = "requires a real `esbuild` on PATH — devenv.nix provisions no Node/esbuild \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with esbuild installed"]
    async fn get_resolves_js_bundle_target_end_to_end() {
        find_real_bin_for_test("esbuild").expect(
            "this test is #[ignore]d precisely because esbuild isn't guaranteed on PATH — it \
             was run explicitly, so a missing esbuild here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(
                        PkgBuf::from("packages/a"),
                        BUNDLE_TARGET.to_string(),
                        Default::default(),
                    ),
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ct,
            )
            .await
            .expect("get js_bundle target_spec");
        assert_eq!(resp.target_spec.driver, "js_bundle");
        assert!(resp.target_spec.config.contains_key("bundler_version"));
    }

    #[tokio::test]
    #[ignore = "requires a real `esbuild` on PATH — devenv.nix provisions no Node/esbuild \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with esbuild installed"]
    async fn get_js_bundle_format_and_target_addr_args_flow_into_config_e2e() {
        find_real_bin_for_test("esbuild").expect(
            "this test is #[ignore]d precisely because esbuild isn't guaranteed on PATH — it \
             was run explicitly, so a missing esbuild here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();

        let mut args = BTreeMap::new();
        args.insert("format".to_string(), "cjs".to_string());
        args.insert("target".to_string(), "browser".to_string());
        let resp = provider
            .get(
                GetRequest {
                    request_id: "test".to_string(),
                    addr: Addr::new(PkgBuf::from("packages/a"), BUNDLE_TARGET.to_string(), args),
                    states: vec![],
                    executor: Arc::new(NoopExecutor),
                },
                &ct,
            )
            .await
            .expect("get js_bundle target_spec with explicit format/target args");
        assert_eq!(
            resp.target_spec.config.get("format"),
            Some(&Value::String("cjs".to_string()))
        );
        assert_eq!(
            resp.target_spec.config.get("target"),
            Some(&Value::String("browser".to_string()))
        );
    }

    /// Feature-quality M6 review BLOCKER: `outdir` was computed purely from
    /// the package path, with no `format`/`target` component, so
    /// `js_bundle@format=esm` and `js_bundle@format=cjs` for the same
    /// package declared the identical `Content::DirPath` output — the
    /// milestone's own headline dual-format-publish use case collided on
    /// the same declared output directory. Proves two variants of the same
    /// package now produce distinct `outdir` values.
    #[tokio::test]
    #[ignore = "requires a real `esbuild` on PATH — devenv.nix provisions no Node/esbuild \
                toolchain (see toolchain.rs module docs); run explicitly with \
                `cargo test -- --ignored` on a host with esbuild installed"]
    async fn get_js_bundle_variants_of_the_same_package_get_distinct_outdirs() {
        find_real_bin_for_test("esbuild").expect(
            "this test is #[ignore]d precisely because esbuild isn't guaranteed on PATH — it \
             was run explicitly, so a missing esbuild here is a real failure, not a skip",
        );

        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name": "a", "main": "src/index.ts"}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );

        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Npm);
        let ct = ctoken();

        let get_outdir = |args: BTreeMap<String, String>| {
            let provider = &provider;
            let ct = &ct;
            async move {
                let resp = provider
                    .get(
                        GetRequest {
                            request_id: "test".to_string(),
                            addr: Addr::new(
                                PkgBuf::from("packages/a"),
                                BUNDLE_TARGET.to_string(),
                                args,
                            ),
                            states: vec![],
                            executor: Arc::new(NoopExecutor),
                        },
                        ct,
                    )
                    .await
                    .expect("get js_bundle target_spec");
                match resp.target_spec.config.get("outdir") {
                    Some(Value::String(s)) => s.clone(),
                    other => panic!("expected outdir to be a string, got {other:?}"),
                }
            }
        };

        let esm_node_outdir = get_outdir(BTreeMap::new()).await;

        let mut cjs_browser_args = BTreeMap::new();
        cjs_browser_args.insert("format".to_string(), "cjs".to_string());
        cjs_browser_args.insert("target".to_string(), "browser".to_string());
        let cjs_browser_outdir = get_outdir(cjs_browser_args).await;

        assert_ne!(
            esm_node_outdir, cjs_browser_outdir,
            "two variants of the same package must not declare the same output directory — the \
             default esm/node outdir was {esm_node_outdir:?}, cjs/browser was \
             {cjs_browser_outdir:?}"
        );
    }
}
