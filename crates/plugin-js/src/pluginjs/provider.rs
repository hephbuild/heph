use crate::pluginjs::lockfile::{self, Lockfile, ResolvedGraph};
use crate::pluginjs::workspace::{self, PkgManager, WorkspaceMember};
use crate::pluginjs::{
    PACKAGE_INFO_TARGET, PACKAGE_JSON, deps, is_skipped_dir_name, package_json, platform,
    thirdparty,
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
}

impl Config {
    fn new(pkgmanager: PkgManager) -> Self {
        Self {
            pkgmanager,
            skip: Arc::new(Ignore::default()),
            walker: Arc::new(CachedWalker::disabled()),
            allow_scripts: Vec::new(),
        }
    }
}

pub struct Provider {
    workspace_root: PathBuf,
    pkgmanager: PkgManager,
    skip: Arc<Ignore>,
    walker: Arc<CachedWalker>,
    allow_scripts: Vec<String>,
    /// Lazily parsed lockfile (`None` when the workspace has none) and its
    /// derived [`ResolvedGraph`] — each `Provider::get` for a third-party
    /// `js_install` addr or a package's declared deps would otherwise
    /// re-read and re-parse the whole lockfile from scratch.
    lockfile_cache: OnceCell<Option<Arc<Lockfile>>>,
    resolved_graph_cache: OnceCell<Option<Arc<ResolvedGraph>>>,
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
            &["pkgmanager", "skip", "allow_scripts"],
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

        Ok(Self::with_config(
            workspace_root,
            Config {
                pkgmanager,
                skip,
                walker,
                allow_scripts,
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
            lockfile_cache: OnceCell::new(),
            resolved_graph_cache: OnceCell::new(),
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
    async fn deps_config(&self, pkg: &PkgBuf) -> anyhow::Result<Value> {
        let lockfile = self.lockfile().await?;
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
                &member_addrs_by_name,
                &goos,
                &goarch,
            )?;

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
}
