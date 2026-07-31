use crate::pluginjs::lockfile::{self, Lockfile, ResolvedGraph};
use crate::pluginjs::workspace::{self, PkgManager, WorkspaceMember};
use crate::pluginjs::{
    PACKAGE_INFO_TARGET, PACKAGE_JSON, TYPECHECK_TARGET, deps, importgraph, is_skipped_dir_name,
    package_json, platform, resolvers, thirdparty, toolchain,
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
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::OnceCell;

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
}

impl Config {
    fn new(pkgmanager: PkgManager) -> Self {
        Self {
            pkgmanager,
            skip: Arc::new(Ignore::default()),
            walker: Arc::new(CachedWalker::disabled()),
            allow_scripts: Vec::new(),
            tstool: toolchain::HOST.to_string(),
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
    /// Lazily parsed lockfile (`None` when the workspace has none) and its
    /// derived [`ResolvedGraph`] — each `Provider::get` for a third-party
    /// `js_install` addr or a package's declared deps would otherwise
    /// re-read and re-parse the whole lockfile from scratch.
    lockfile_cache: OnceCell<Option<Arc<Lockfile>>>,
    resolved_graph_cache: OnceCell<Option<Arc<ResolvedGraph>>>,
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
}

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
            &["pkgmanager", "skip", "allow_scripts", "tstool"],
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

        Ok(Self::with_config(
            workspace_root,
            Config {
                pkgmanager,
                skip,
                walker,
                allow_scripts,
                tstool,
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
            lockfile_cache: OnceCell::new(),
            resolved_graph_cache: OnceCell::new(),
            tsc_cache: OnceCell::new(),
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
    /// lifetime. `None` when the workspace has no lockfile file at all — not
    /// every workspace needs one (a package with zero third-party
    /// dependencies), so its absence is only an error at the point a
    /// resolution is actually attempted against it (see `deps::resolve_package_deps`).
    async fn lockfile(&self) -> anyhow::Result<Option<Arc<Lockfile>>> {
        let pkgmanager = self.pkgmanager;
        let workspace_root = self.workspace_root.clone();
        let result = self
            .lockfile_cache
            .get_or_try_init(|| async move {
                let path = workspace_root.join(Lockfile::filename(pkgmanager));
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

    /// The lockfile's flattened [`ResolvedGraph`], parsed once and cached
    /// alongside the lockfile itself.
    async fn resolved_graph(&self) -> anyhow::Result<Option<Arc<ResolvedGraph>>> {
        let lockfile = self.lockfile().await?;
        let result = self
            .resolved_graph_cache
            .get_or_try_init(|| async move {
                anyhow::Ok(lockfile.as_ref().map(|lf| Arc::new(lf.resolved_graph())))
            })
            .await?;
        Ok(result.clone())
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
    /// (`importgraph::declared_closure`). M1's `package.json`-declaration
    /// path above is still what maps a specifier to an addr; this is the
    /// correctness check on top of it — a resolved-but-undeclared import is
    /// a hermeticity violation and fails `Provider::get` loudly, per
    /// `ai-docs/js-plugin-plan.md`'s Hermeticity section. See
    /// `importgraph.rs` module docs for why an *unresolvable* specifier is
    /// deliberately not treated the same way.
    async fn deps_config(&self, pkg: &PkgBuf) -> anyhow::Result<Value> {
        let lockfile = self.lockfile().await?;
        let resolved_graph = self.resolved_graph().await?;
        let workspace_root = self.workspace_root.clone();
        let walker = Arc::clone(&self.walker);
        let skip = Arc::clone(&self.skip);
        let pkgmanager = self.pkgmanager;
        let pkg_str = pkg.as_str().to_string();
        let goos = platform::current_goos();
        let goarch = platform::current_goarch();

        hcore::blocking::run(move || -> anyhow::Result<Value> {
            let package_json_path = workspace_root.join(&pkg_str).join(PACKAGE_JSON);
            let manifest = package_json::read_package_manifest(&package_json_path)
                .with_context(|| format!("reading dependencies of {pkg_str:?}"))?;

            // Workspace-member discovery, redone here rather than reusing
            // `Provider::workspace_members` — this whole closure already
            // runs on the blocking pool, so it calls the same free functions
            // `workspace_members` itself calls rather than the `&self`
            // method (which can't be moved into a `'static` blocking job).
            let patterns = match pkgmanager {
                PkgManager::Npm => workspace::read_npm_workspace_globs(&workspace_root)?,
                PkgManager::Pnpm => workspace::read_pnpm_workspace_globs(&workspace_root)?,
            };
            let member_addrs_by_name: BTreeMap<String, String> = if patterns.is_empty() {
                BTreeMap::new()
            } else {
                let mut packages = Vec::new();
                collect_js_packages(
                    &walker,
                    &workspace_root,
                    &workspace_root,
                    &skip,
                    &mut packages,
                );
                let packages: Vec<PkgBuf> = packages.into_iter().collect::<anyhow::Result<_>>()?;
                workspace::resolve_members(&workspace_root, &packages, &patterns)?
                    .into_iter()
                    .map(|m| (m.name, m.addr.format()))
                    .collect()
            };

            let resolved = deps::resolve_package_deps(
                &pkg_str,
                &manifest,
                lockfile.as_deref(),
                resolved_graph.as_deref(),
                &member_addrs_by_name,
                &goos,
                &goarch,
            )?;

            // M2: cross-validate the declared-dependency wiring above against
            // the package's real import graph — see this method's doc
            // comment and `importgraph.rs` module docs.
            let tsconfig =
                importgraph::find_nearest_tsconfig(&workspace_root, &workspace_root.join(&pkg_str));
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
            let declared_closure = importgraph::declared_closure(&manifest);
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
        let (tsc_bin, tsc_version) = self.resolved_host_tsc().await?.as_ref().clone();
        let lockfile = self.lockfile().await?;
        let resolved_graph = self.resolved_graph().await?;
        let workspace_root = self.workspace_root.clone();
        let walker = Arc::clone(&self.walker);
        let skip = Arc::clone(&self.skip);
        let pkgmanager = self.pkgmanager;
        let pkg_str = pkg.as_str().to_string();
        let goos = platform::current_goos();
        let goarch = platform::current_goarch();

        hcore::blocking::run(move || -> anyhow::Result<HashMap<String, Value>> {
            // Same workspace-member discovery `Provider::deps_config` does
            // in its own blocking closure — needed here too so an import
            // that never resolved on disk (no ambient `node_modules`) can
            // still be attributed to a workspace sibling by name (see
            // `typecheck_deps_config`'s doc).
            let patterns = match pkgmanager {
                PkgManager::Npm => workspace::read_npm_workspace_globs(&workspace_root)?,
                PkgManager::Pnpm => workspace::read_pnpm_workspace_globs(&workspace_root)?,
            };
            let member_addrs_by_name: BTreeMap<String, String> = if patterns.is_empty() {
                BTreeMap::new()
            } else {
                let mut packages = Vec::new();
                collect_js_packages(
                    &walker,
                    &workspace_root,
                    &workspace_root,
                    &skip,
                    &mut packages,
                );
                let packages: Vec<PkgBuf> = packages.into_iter().collect::<anyhow::Result<_>>()?;
                workspace::resolve_members(&workspace_root, &packages, &patterns)?
                    .into_iter()
                    .map(|m| (m.name, m.addr.format()))
                    .collect()
            };

            let (deps, tsconfig_path, tsconfig_content) = typecheck_deps_config(
                &walker,
                &workspace_root,
                &pkg_str,
                lockfile.as_deref(),
                resolved_graph.as_deref(),
                &member_addrs_by_name,
                &goos,
                &goarch,
            )
            .with_context(|| format!("building js_typecheck inputs for {pkg_str:?}"))?;

            let mut config: HashMap<String, Value> = HashMap::new();
            config.insert(
                "tsc_bin".to_string(),
                Value::String(tsc_bin.to_string_lossy().into_owned()),
            );
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

    /// Build the `js_install` `TargetSpec` for a third-party
    /// `@heph/js/thirdparty/<name>@<version>` addr, resolving `(name,
    /// version)` against the lockfile's [`ResolvedGraph`] for its
    /// integrity/tarball-URL/platform-restriction/install-script metadata.
    async fn thirdparty_install_spec(&self, addr: &Addr) -> anyhow::Result<TargetSpec> {
        let (name, version) =
            thirdparty::parse_thirdparty_pkg(addr.package.as_str()).ok_or_else(|| {
                anyhow::anyhow!("not a js thirdparty addr: {}", addr.package.as_str())
            })?;
        let goos = addr
            .args
            .get("goos")
            .cloned()
            .unwrap_or_else(platform::current_goos);
        let goarch = addr
            .args
            .get("goarch")
            .cloned()
            .unwrap_or_else(platform::current_goarch);

        let graph = self.resolved_graph().await?.ok_or_else(|| {
            anyhow::anyhow!(
                "js provider: no {} found at the workspace root — cannot resolve third-party \
                 package {name}@{version}",
                Lockfile::filename(self.pkgmanager)
            )
        })?;
        let resolved = graph.get(name, version).ok_or_else(|| {
            anyhow::anyhow!(
                "js provider: {name}@{version} not found in the lockfile — is it stale?"
            )
        })?;

        anyhow::ensure!(
            platform::matches_platform(&resolved.os, &resolved.cpu, &goos, &goarch),
            "js provider: {name}@{version} is restricted to os={:?} cpu={:?}, which does not \
             include the requested platform {goos}/{goarch}",
            resolved.os,
            resolved.cpu
        );

        let resolved_url = resolved
            .resolved
            .clone()
            .unwrap_or_else(|| default_registry_url(name, version));
        let scripts_allowed = self.scripts_allowed_for(name, version);

        let mut config: HashMap<String, Value> = HashMap::new();
        config.insert("name".to_string(), Value::String(name.to_string()));
        config.insert("version".to_string(), Value::String(version.to_string()));
        config.insert(
            "integrity".to_string(),
            Value::String(resolved.integrity.clone()),
        );
        config.insert("resolved".to_string(), Value::String(resolved_url));
        config.insert("goos".to_string(), Value::String(goos));
        config.insert("goarch".to_string(), Value::String(goarch));
        config.insert(
            "has_install_script".to_string(),
            Value::Bool(resolved.has_install_script),
        );
        config.insert("scripts_allowed".to_string(), Value::Bool(scripts_allowed));

        Ok(TargetSpec {
            addr: addr.clone(),
            driver: "js_install".to_string(),
            config,
            labels: vec![],
            transitive: Default::default(),
            approval: Default::default(),
        })
    }
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
/// `lockfile`/`resolved_graph`/`member_addrs_by_name`/`goos`/`goarch` are
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
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    member_addrs_by_name: &BTreeMap<String, String>,
    goos: &str,
    goarch: &str,
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

    let import_resolvers = resolvers::Resolvers::new(tsconfig.as_deref());
    let resolve_cache = importgraph::ResolveCache::new();
    let graph = importgraph::build_package_import_graph(
        walker,
        workspace_root,
        pkg,
        &import_resolvers,
        &resolve_cache,
        tsconfig.as_deref(),
    )
    .with_context(|| format!("building import graph for {pkg:?}"))?;

    // Phantom-dependency check: `Provider::deps_config` (the `js_package_info`
    // target) already runs this, but a workspace member requesting only
    // `js_typecheck` (never `js_package_info`) must not skip it — this is
    // also what justifies treating every name reached below via
    // `member_addrs_by_name`/the lockfile as genuinely declared, rather than
    // re-deriving that from scratch.
    let declared_closure = importgraph::declared_closure(&manifest);
    importgraph::check_phantom_dependencies(workspace_root, pkg, &graph, &declared_closure)
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
            &site.package_name,
            &manifest,
            lockfile,
            resolved_graph,
            member_addrs_by_name,
            goos,
            goarch,
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
            Ok(Box::new(std::iter::once(Ok(ListResponse { addr })))
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
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].name, PACKAGE_INFO_TARGET);
        assert_eq!(addrs[0].package.as_str(), "packages/a");
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
        assert!(
            addrs[0].contains("@heph/js/thirdparty/lodash@4.17.21:js_install"),
            "{}",
            addrs[0]
        );
    }

    #[tokio::test]
    async fn pnpm_e2e_wires_third_party_dep_to_js_install_addr() {
        let dir = pnpm_e2e_fixture("sha512-abc");
        let provider = Provider::new(dir.path().to_path_buf(), PkgManager::Pnpm);
        let addrs = get_deps_addrs(&provider, "packages/a").await;
        assert_eq!(addrs.len(), 1);
        assert!(
            addrs[0].contains("@heph/js/thirdparty/lodash@4.17.21:js_install"),
            "{}",
            addrs[0]
        );
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
            &platform::current_goos(),
            &platform::current_goarch(),
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
            &platform::current_goos(),
            &platform::current_goarch(),
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
            },
        );

        let ct = ctoken();
        let executor = Arc::new(NoopExecutor);
        let addr = thirdparty::thirdparty_addr(
            "native-thing",
            "1.0.0",
            &platform::current_goos(),
            &platform::current_goarch(),
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

    /// `typecheck_deps_config` with no lockfile/workspace-member context —
    /// what most of these tests need, since they exercise scoping behavior
    /// that doesn't touch an unresolved third-party/sibling import.
    fn call_typecheck_deps_config(
        walker: &CachedWalker,
        workspace_root: &Path,
        pkg: &str,
    ) -> anyhow::Result<(HashMap<String, Value>, String, String)> {
        typecheck_deps_config(
            walker,
            workspace_root,
            pkg,
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

        let walker = CachedWalker::disabled();
        let (deps, tsconfig_path, tsconfig_content) =
            call_typecheck_deps_config(&walker, dir.path(), "packages/a")
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
            type_addrs[0].contains("node_modules/pkg/index.d.ts"),
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
        let resolved_graph = lockfile.resolved_graph();

        let walker = CachedWalker::disabled();
        let (deps, _tsconfig_path, _tsconfig_content) = typecheck_deps_config(
            &walker,
            dir.path(),
            "packages/a",
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
                .any(|a| a.contains("zod") && a.contains("js_install")),
            "an unresolved third-party type import must still declare a js_install Input even \
             absent ambient node_modules: {type_addrs:?}"
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
}
