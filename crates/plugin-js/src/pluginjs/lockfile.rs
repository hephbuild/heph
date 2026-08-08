//! Lockfile parsing into one manager-agnostic resolved dependency graph — see
//! `ai-docs/js-plugin-plan.md`'s "Package manager support" section, item 1,
//! and the M1 milestone note.
//!
//! npm's `package-lock.json` (lockfileVersion 3) is already a flattened,
//! fully-resolved graph keyed by a `node_modules`-relative path (mirroring
//! Node's own directory-based module resolution): looking up a dependency
//! is an ancestor-directory walk over that path space, exactly as Node's own
//! `require`/`import` resolver does. pnpm's `pnpm-lock.yaml` instead splits
//! package metadata (`packages`) from the resolved per-workspace-member
//! import graph (`importers`) and per-package dependency edges
//! (`snapshots`); the version a workspace member's `package.json` range
//! resolves to lives only in `importers`, not derivable from `packages`
//! alone. Both feed the same [`ResolvedGraph`].
//!
//! **Scope note (M1):** only the modern pnpm lockfile shape
//! (`importers`/`packages`/`snapshots`, lockfileVersion 9.x) is supported —
//! parsing fails loudly, naming the file, when `importers` is absent, rather
//! than silently misreading an older shape. Peer-dependency-suffixed
//! resolutions (`foo@1.2.3(react@18.0.0)`) are recognized and the suffix is
//! stripped for graph-key purposes, but a package resolved *differently* per
//! peer context is folded onto one node — full node-resolution parity
//! (multiple resolutions of the same `(name, version)`, git/tarball deps) is
//! M2 scope (the oxc-based resolver), not this milestone's.

use anyhow::Context;
use std::collections::{BTreeMap, HashSet, VecDeque};

use crate::pluginjs::package_json::PackageManifest;
use crate::pluginjs::workspace::PkgManager;

/// One resolved node: an exact `(name, version)` with the bytes that make it
/// hermetic (`integrity`) and the edges to its own resolved dependencies.
/// Common to both lockfile formats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    /// Subresource-Integrity-format hash (`"sha512-…"` / `"sha1-…"`)
    /// verifying the published tarball's bytes. Empty for a degenerate
    /// lockfile entry — a real, observed shape (e.g. a peer-suffixed/deduped
    /// graph key folded onto one node picking up a variant with no
    /// resolution data of its own), not merely hypothetical malformed input
    /// — every genuinely-resolved entry carries one. **Not itself proof that
    /// `resolved`/`os`/`cpu`/`dependencies`/`has_install_script` are also
    /// empty/default** — each field is parsed independently (see
    /// `NpmLockfile`/`PnpmLockfile`'s own `resolved_graph`); a caller that
    /// needs to treat "no integrity" as "nothing here worth comparing" must
    /// check each field it cares about on its own terms, the way
    /// `Provider::find_resolved_graph_for`'s `entries_agree_where_comparable`
    /// does, rather than skipping the whole entry on this field alone.
    pub integrity: String,
    /// Tarball URL, when the lockfile records one directly (npm always
    /// does). `None` when the lockfile omits it (pnpm's common case for a
    /// plain registry dependency, or the same degenerate-entry shape
    /// `integrity`'s doc describes) — callers derive the default npm
    /// registry URL from `name`/`version`.
    pub resolved: Option<String>,
    /// Declared dependency edges: name → the [`graph_key`] of the resolved
    /// package it points to.
    pub dependencies: BTreeMap<String, String>,
    /// `package.json` `os` restriction (Node `process.platform` values,
    /// e.g. `"darwin"`). Empty = unrestricted.
    pub os: Vec<String>,
    /// `package.json` `cpu` restriction (Node `process.arch` values, e.g.
    /// `"arm64"`, `"x64"`). Empty = unrestricted.
    pub cpu: Vec<String>,
    /// Whether the package declares an `install`/`preinstall`/`postinstall`
    /// lifecycle script (npm's `hasInstallScript`; pnpm's closest signal is
    /// `requiresBuild`).
    pub has_install_script: bool,
}

/// `"name@version"` — the stable key into [`ResolvedGraph::packages`].
pub fn graph_key(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

/// The manager-agnostic resolved dependency graph every lockfile parser
/// produces. See module docs.
#[derive(Debug, Clone, Default)]
pub struct ResolvedGraph {
    pub packages: BTreeMap<String, ResolvedPackage>,
}

impl ResolvedGraph {
    pub fn get(&self, name: &str, version: &str) -> Option<&ResolvedPackage> {
        self.packages.get(&graph_key(name, version))
    }

    /// BFS outward from `seed_keys` (graph keys already known to be reachable
    /// — e.g. a consumer's own direct dependencies, resolved), mapping every
    /// reachable package's name to its resolved version.
    ///
    /// Deterministic given a deterministic seed order: each
    /// `ResolvedPackage::dependencies` is itself a `BTreeMap`, so sibling
    /// edges are always walked in the same (sorted) order regardless of
    /// process/host — a name reachable at more than one version keeps the
    /// one first reached in that fixed BFS order (`entry().or_insert`),
    /// never an arbitrary or ambient-filesystem-dependent pick.
    pub fn transitive_reachable(
        &self,
        seed_keys: impl IntoIterator<Item = String>,
    ) -> BTreeMap<String, String> {
        let mut result: BTreeMap<String, String> = BTreeMap::new();
        let mut seen_keys: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        for key in seed_keys {
            if seen_keys.insert(key.clone()) {
                queue.push_back(key);
            }
        }
        while let Some(key) = queue.pop_front() {
            let Some(pkg) = self.packages.get(&key) else {
                continue;
            };
            result
                .entry(pkg.name.clone())
                .or_insert_with(|| pkg.version.clone());
            for dep_key in pkg.dependencies.values() {
                if seen_keys.insert(dep_key.clone()) {
                    queue.push_back(dep_key.clone());
                }
            }
        }
        result
    }
}

/// Graph keys for `manifest`'s own direct dependencies
/// (`dependencies`/`devDependencies`/`peerDependencies`; `dependencies`
/// already folds in `optionalDependencies` — see [`PackageManifest`]),
/// resolved through `lockfile`. The seed set [`resolve_transitive`] and
/// [`crate::pluginjs::importgraph::transitive_declared_closure`] both BFS
/// outward from — the two must use the same seeds, or a package one accepts
/// could differ from what the other can wire an Input for.
pub(crate) fn direct_dep_seed_keys(
    lockfile: &Lockfile,
    from_pkg: &str,
    manifest: &PackageManifest,
) -> anyhow::Result<Vec<String>> {
    let names = manifest
        .dependencies
        .keys()
        .chain(manifest.dev_dependencies.keys())
        .chain(manifest.peer_dependencies.keys());
    let mut keys = Vec::new();
    for name in names {
        if let Some(DepResolution::ThirdParty { name, version }) =
            lockfile.resolve_dependency(from_pkg, name)?
        {
            keys.push(graph_key(&name, &version));
        }
    }
    Ok(keys)
}

/// [`Lockfile::resolve_dependency`], with a transitive fallback for when the
/// direct/importer-only lookup misses: BFS outward from `manifest`'s own
/// direct dependencies (via [`direct_dep_seed_keys`] and
/// [`ResolvedGraph::transitive_reachable`]) looking for `name`.
///
/// A package reached this way — e.g. `@eslint/js`, pulled in because a
/// declared `devDependency` (`typescript-eslint`) depends on it — is fully
/// determined by this already-hashed lockfile, not by ambient `node_modules`
/// hoisting, so wiring an Input for it here is exactly as hermetic as wiring
/// one for a direct dependency. This exists specifically so
/// `deps::resolve_one_dependency`'s Input-wiring agrees with
/// `importgraph::transitive_declared_closure`'s phantom-dependency check —
/// a package the check accepts must always be one this can also resolve an
/// addr for, or `Provider::get` would succeed while producing a `TargetDef`
/// missing the Input the actual `tsc`/`vitest`/`esbuild` run needs, and
/// whether that gap is later papered over by ambient `node_modules` (on a
/// host where something happens to have installed it) becomes exactly the
/// same-source-different-host non-determinism this whole subsystem exists
/// to close.
///
/// Unlike the phantom check (which only asks "is `name` reachable at all"),
/// this picks a specific *version* to wire an Input for — the module docs'
/// M2-scope note applies here for real: a name resolved differently per
/// peer context is folded onto one node, so the version picked is whichever
/// one BFS reaches first (deterministic, see [`ResolvedGraph::transitive_reachable`]'s
/// doc — never ambient or host-dependent), not necessarily "the" version if
/// more than one is genuinely in the graph.
pub fn resolve_transitive(
    lockfile: &Lockfile,
    resolved_graph: &ResolvedGraph,
    from_pkg: &str,
    manifest: &PackageManifest,
    name: &str,
) -> anyhow::Result<Option<DepResolution>> {
    if let Some(direct) = lockfile.resolve_dependency(from_pkg, name)? {
        return Ok(Some(direct));
    }
    let seed_keys = direct_dep_seed_keys(lockfile, from_pkg, manifest)?;
    Ok(resolved_graph
        .transitive_reachable(seed_keys)
        .get(name)
        .map(|version| DepResolution::ThirdParty {
            name: name.to_string(),
            version: version.clone(),
        }))
}

/// What a package's declared dependency on `name` resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepResolution {
    /// A third-party package resolved via the lockfile.
    ThirdParty { name: String, version: String },
    /// A `link:`/`file:` self-reference to another workspace member (pnpm
    /// spells this `link:../other`). Wiring resolves this against the
    /// discovered workspace-member set by name, not the third-party graph.
    Workspace,
}

/// A parsed lockfile, exposing the two things dependency wiring needs
/// uniformly: the flattened [`ResolvedGraph`] (every third-party package
/// this workspace installs) and per-consumer resolution (which exact
/// `(name, version)` a given package's declared dependency name resolves
/// to).
#[derive(Debug, Clone)]
pub enum Lockfile {
    Npm(NpmLockfile),
    Pnpm(PnpmLockfile),
}

impl Lockfile {
    /// Parse `contents` as the lockfile format the given package manager
    /// uses.
    pub fn parse(pkgmanager: PkgManager, contents: &str) -> anyhow::Result<Self> {
        match pkgmanager {
            PkgManager::Npm => Ok(Lockfile::Npm(NpmLockfile::parse(contents)?)),
            PkgManager::Pnpm => Ok(Lockfile::Pnpm(PnpmLockfile::parse(contents)?)),
        }
    }

    /// The lockfile's filename, for locating it under the workspace root.
    pub fn filename(pkgmanager: PkgManager) -> &'static str {
        match pkgmanager {
            PkgManager::Npm => "package-lock.json",
            PkgManager::Pnpm => "pnpm-lock.yaml",
        }
    }

    pub fn resolved_graph(&self) -> ResolvedGraph {
        match self {
            Lockfile::Npm(l) => l.resolved_graph(),
            Lockfile::Pnpm(l) => l.resolved_graph(),
        }
    }

    /// Resolve `name` as declared by the package at `from_pkg` (workspace-root
    /// relative; `""` for the root). `Ok(None)` means the lockfile has no
    /// resolution for this name from this package — callers decide whether
    /// that's an expected skip (an unmatched-platform optional dependency)
    /// or a hard error (a required dependency missing from a stale
    /// lockfile).
    pub fn resolve_dependency(
        &self,
        from_pkg: &str,
        name: &str,
    ) -> anyhow::Result<Option<DepResolution>> {
        match self {
            Lockfile::Npm(l) => Ok(l.resolve_dependency(from_pkg, name)),
            Lockfile::Pnpm(l) => l.resolve_dependency(from_pkg, name),
        }
    }
}

// ---------------------------------------------------------------------
// npm (package-lock.json, lockfileVersion 3)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct NpmLockRaw {
    #[serde(default)]
    packages: BTreeMap<String, NpmPackageRaw>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NpmPackageRaw {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    resolved: Option<String>,
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    has_install_script: bool,
    #[serde(default)]
    link: bool,
}

/// Parsed `package-lock.json`, keyed by its own `node_modules`-relative path
/// space (e.g. `""` for the root, `"packages/a"` for a workspace member,
/// `"node_modules/lodash"` for a hoisted third-party package).
#[derive(Debug, Clone)]
pub struct NpmLockfile {
    packages: BTreeMap<String, NpmPackageRaw>,
}

impl NpmLockfile {
    pub fn parse(contents: &str) -> anyhow::Result<Self> {
        let raw: NpmLockRaw = serde_json::from_str(contents).context("parse package-lock.json")?;
        Ok(Self {
            packages: raw.packages,
        })
    }

    /// Every ancestor `node_modules` candidate path for `from`, nearest
    /// first — the same directory-walk Node's own resolver performs.
    /// `from = ""` (the workspace root) yields just `["node_modules"]`.
    fn ancestor_node_modules(from: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = from;
        loop {
            out.push(if cur.is_empty() {
                "node_modules".to_string()
            } else {
                format!("{cur}/node_modules")
            });
            if cur.is_empty() {
                break;
            }
            cur = cur.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
        out
    }

    /// Resolve `name` as it would be required from directory `from_path`
    /// (a `node_modules`-relative path, e.g. `""` root or `"packages/a"`):
    /// the first ancestor `node_modules/<name>` entry the lockfile records.
    fn resolve_path(&self, from_path: &str, name: &str) -> Option<&NpmPackageRaw> {
        for candidate in Self::ancestor_node_modules(from_path) {
            let path = format!("{candidate}/{name}");
            if let Some(entry) = self.packages.get(&path) {
                return Some(entry);
            }
        }
        None
    }

    fn resolve_dependency(&self, from_pkg: &str, name: &str) -> Option<DepResolution> {
        let entry = self.resolve_path(from_pkg, name)?;
        if entry.link {
            return Some(DepResolution::Workspace);
        }
        let version = entry.version.clone()?;
        Some(DepResolution::ThirdParty {
            name: name.to_string(),
            version,
        })
    }

    pub fn resolved_graph(&self) -> ResolvedGraph {
        let mut packages = BTreeMap::new();
        for (path, entry) in &self.packages {
            // Only real `node_modules/…` entries are fetchable third-party
            // packages; the root (`""`) and workspace-member paths (e.g.
            // `"packages/a"`) describe local directories, not tarballs, and
            // a `link: true` entry is a symlink to one of those, not content
            // of its own.
            let Some((_, suffix)) = path.rsplit_once("node_modules/") else {
                continue;
            };
            if entry.link {
                continue;
            }
            let Some(version) = &entry.version else {
                continue;
            };
            let name = entry.name.clone().unwrap_or_else(|| suffix.to_string());
            let key = graph_key(&name, version);
            packages.insert(
                key,
                ResolvedPackage {
                    name,
                    version: version.clone(),
                    integrity: entry.integrity.clone().unwrap_or_default(),
                    resolved: entry.resolved.clone(),
                    dependencies: resolve_npm_edges(self, path, &entry.dependencies),
                    os: entry.os.clone(),
                    cpu: entry.cpu.clone(),
                    has_install_script: entry.has_install_script,
                },
            );
        }
        ResolvedGraph { packages }
    }
}

fn resolve_npm_edges(
    lock: &NpmLockfile,
    from_path: &str,
    declared: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in declared.keys() {
        if let Some(entry) = lock.resolve_path(from_path, name)
            && !entry.link
            && let Some(version) = &entry.version
        {
            out.insert(name.clone(), graph_key(name, version));
        }
        // A miss here (optional/platform-mismatched dep never installed, or
        // a git/tarball dep) is not an error at this layer — `resolve_graph`
        // only records edges the lockfile actually resolved; M1's wiring
        // only walks direct package.json deps, so an under-resolved
        // transitive edge here does not yet affect target-dep correctness.
    }
    out
}

// ---------------------------------------------------------------------
// pnpm (pnpm-lock.yaml)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct PnpmLockRaw {
    #[serde(default)]
    importers: BTreeMap<String, PnpmImporterRaw>,
    #[serde(default)]
    packages: BTreeMap<String, PnpmPackageMetaRaw>,
    #[serde(default)]
    snapshots: BTreeMap<String, PnpmSnapshotRaw>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PnpmImporterRaw {
    #[serde(default)]
    dependencies: BTreeMap<String, PnpmDepRefRaw>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, PnpmDepRefRaw>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, PnpmDepRefRaw>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct PnpmDepRefRaw {
    #[serde(default)]
    version: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PnpmPackageMetaRaw {
    #[serde(default)]
    resolution: PnpmResolutionRaw,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    requires_build: bool,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct PnpmResolutionRaw {
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    tarball: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct PnpmSnapshotRaw {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
}

/// Parsed `pnpm-lock.yaml` (modern `importers`/`packages`/`snapshots`
/// shape). See module docs for the version-shape scope note.
#[derive(Debug, Clone)]
pub struct PnpmLockfile {
    importers: BTreeMap<String, PnpmImporterRaw>,
    packages: BTreeMap<String, PnpmPackageMetaRaw>,
    snapshots: BTreeMap<String, PnpmSnapshotRaw>,
}

/// pnpm spells the workspace root importer `"."`, not `""`; every other
/// heph package path (`""` for the root elsewhere, `"packages/a"` for a
/// member) matches directly.
fn pnpm_importer_key(pkg: &str) -> &str {
    if pkg.is_empty() { "." } else { pkg }
}

/// Strip a resolved version's optional peer-dependency suffix
/// (`"18.2.0(react@18.2.0)"` → `"18.2.0"`) and a scoped/plain package key's
/// suffix the same way (`"@scope/pkg@1.0.0(peer@1)"` → up to the version).
/// See module docs: full per-peer-context multi-resolution is M2 scope —
/// this folds every peer-suffixed resolution onto one `(name, version)` node.
fn strip_peer_suffix(s: &str) -> &str {
    s.split('(').next().unwrap_or(s)
}

/// Split a pnpm `packages`/`snapshots` key into `(name, version)`. Handles
/// scoped names (`@scope/pkg@1.0.0`) by skipping the leading `@` before
/// searching for the name/version separator.
fn split_pnpm_key(key: &str) -> Option<(String, String)> {
    let key = strip_peer_suffix(key);
    if let Some(rest) = key.strip_prefix('@') {
        let at = rest.find('@')?;
        Some((
            format!("@{}", rest.get(..at)?),
            rest.get(at + 1..)?.to_string(),
        ))
    } else {
        let at = key.find('@')?;
        Some((key.get(..at)?.to_string(), key.get(at + 1..)?.to_string()))
    }
}

impl PnpmLockfile {
    pub fn parse(contents: &str) -> anyhow::Result<Self> {
        let raw: PnpmLockRaw = serde_yaml::from_str(contents).context("parse pnpm-lock.yaml")?;
        if raw.importers.is_empty() {
            anyhow::bail!(
                "pnpm-lock.yaml has no `importers` section — only the modern \
                 importers/packages/snapshots lockfile shape (pnpm ~v8+) is supported; \
                 regenerate the lockfile with a current pnpm to use it with heph"
            );
        }
        Ok(Self {
            importers: raw.importers,
            packages: raw.packages,
            snapshots: raw.snapshots,
        })
    }

    fn resolve_dependency(
        &self,
        from_pkg: &str,
        name: &str,
    ) -> anyhow::Result<Option<DepResolution>> {
        let Some(importer) = self.importers.get(pnpm_importer_key(from_pkg)) else {
            return Ok(None);
        };
        let dep_ref = importer
            .dependencies
            .get(name)
            .or_else(|| importer.dev_dependencies.get(name))
            .or_else(|| importer.optional_dependencies.get(name));
        let Some(dep_ref) = dep_ref else {
            return Ok(None);
        };
        if dep_ref.version.starts_with("link:") || dep_ref.version.starts_with("file:") {
            return Ok(Some(DepResolution::Workspace));
        }
        let version = strip_peer_suffix(&dep_ref.version).to_string();
        Ok(Some(DepResolution::ThirdParty {
            name: name.to_string(),
            version,
        }))
    }

    pub fn resolved_graph(&self) -> ResolvedGraph {
        let mut packages = BTreeMap::new();
        for (key, meta) in &self.packages {
            let Some((name, version)) = split_pnpm_key(key) else {
                continue;
            };
            let dependencies = self
                .snapshots
                .get(key)
                .map(|snap| {
                    snap.dependencies
                        .iter()
                        .filter_map(|(dep_name, dep_version)| {
                            let (dep_name2, dep_version2) =
                                split_pnpm_key(&format!("{dep_name}@{dep_version}"))?;
                            Some((dep_name.clone(), graph_key(&dep_name2, &dep_version2)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            packages.insert(
                graph_key(&name, &version),
                ResolvedPackage {
                    name,
                    version,
                    integrity: meta.resolution.integrity.clone().unwrap_or_default(),
                    resolved: meta.resolution.tarball.clone(),
                    dependencies,
                    os: meta.os.clone(),
                    cpu: meta.cpu.clone(),
                    has_install_script: meta.requires_build,
                },
            );
        }
        ResolvedGraph { packages }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- npm ----

    fn npm_fixture() -> &'static str {
        r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root", "dependencies": { "lodash": "^4.17.21" } },
                "packages/a": { "name": "a", "dependencies": { "lodash": "^4.17.21", "b": "workspace:*" } },
                "node_modules/a": { "resolved": "packages/a", "link": true },
                "node_modules/lodash": {
                    "version": "4.17.21",
                    "resolved": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
                    "integrity": "sha512-abc",
                    "dependencies": {}
                },
                "node_modules/@esbuild/darwin-arm64": {
                    "version": "0.19.0",
                    "resolved": "https://registry.npmjs.org/@esbuild/darwin-arm64/-/darwin-arm64-0.19.0.tgz",
                    "integrity": "sha512-def",
                    "os": ["darwin"],
                    "cpu": ["arm64"],
                    "hasInstallScript": true
                }
            }
        }"#
    }

    #[test]
    fn npm_resolved_graph_excludes_root_and_local_and_links() {
        let lock = NpmLockfile::parse(npm_fixture()).unwrap();
        let graph = lock.resolved_graph();
        assert_eq!(graph.packages.len(), 2, "{:?}", graph.packages.keys());
        assert!(graph.get("lodash", "4.17.21").is_some());
        assert!(graph.get("@esbuild/darwin-arm64", "0.19.0").is_some());
    }

    #[test]
    fn npm_resolved_graph_carries_integrity_and_platform() {
        let lock = NpmLockfile::parse(npm_fixture()).unwrap();
        let graph = lock.resolved_graph();
        let esbuild = graph.get("@esbuild/darwin-arm64", "0.19.0").unwrap();
        assert_eq!(esbuild.integrity, "sha512-def");
        assert_eq!(esbuild.os, vec!["darwin".to_string()]);
        assert_eq!(esbuild.cpu, vec!["arm64".to_string()]);
        assert!(esbuild.has_install_script);
    }

    #[test]
    fn npm_resolve_dependency_from_root_finds_hoisted_package() {
        let lock = NpmLockfile::parse(npm_fixture()).unwrap();
        let got = lock.resolve_dependency("", "lodash").unwrap();
        assert_eq!(
            got,
            DepResolution::ThirdParty {
                name: "lodash".to_string(),
                version: "4.17.21".to_string()
            }
        );
    }

    #[test]
    fn npm_resolve_dependency_from_workspace_member_walks_up_to_root() {
        let lock = NpmLockfile::parse(npm_fixture()).unwrap();
        // "packages/a" declares lodash but only the root hoists it — the
        // ancestor walk must find "node_modules/lodash", not fail.
        let got = lock.resolve_dependency("packages/a", "lodash").unwrap();
        assert!(matches!(got, DepResolution::ThirdParty { .. }));
    }

    #[test]
    fn npm_resolve_dependency_link_is_workspace() {
        let lock = NpmLockfile::parse(npm_fixture()).unwrap();
        let got = lock.resolve_dependency("packages/a", "a").unwrap();
        assert_eq!(got, DepResolution::Workspace);
    }

    #[test]
    fn npm_resolve_dependency_unknown_name_is_none() {
        let lock = NpmLockfile::parse(npm_fixture()).unwrap();
        assert_eq!(lock.resolve_dependency("", "not-a-real-pkg"), None);
    }

    // ---- pnpm ----

    fn pnpm_fixture() -> &'static str {
        r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      lodash:
        specifier: ^4.17.21
        version: 4.17.21
  packages/a:
    dependencies:
      lodash:
        specifier: ^4.17.21
        version: 4.17.21
      b:
        specifier: workspace:*
        version: link:../b
packages:
  lodash@4.17.21:
    resolution: {integrity: sha512-abc}
  '@esbuild/darwin-arm64@0.19.0':
    resolution: {integrity: sha512-def}
    cpu: [arm64]
    os: [darwin]
    requiresBuild: true
snapshots:
  lodash@4.17.21: {}
  '@esbuild/darwin-arm64@0.19.0': {}
"#
    }

    #[test]
    fn pnpm_requires_importers_section() {
        let err = PnpmLockfile::parse("lockfileVersion: '6.0'\npackages: {}\n").unwrap_err();
        assert!(format!("{err:#}").contains("importers"));
    }

    #[test]
    fn pnpm_resolved_graph_carries_integrity_and_platform() {
        let lock = PnpmLockfile::parse(pnpm_fixture()).unwrap();
        let graph = lock.resolved_graph();
        assert!(graph.get("lodash", "4.17.21").is_some());
        let esbuild = graph.get("@esbuild/darwin-arm64", "0.19.0").unwrap();
        assert_eq!(esbuild.integrity, "sha512-def");
        assert_eq!(esbuild.os, vec!["darwin".to_string()]);
        assert_eq!(esbuild.cpu, vec!["arm64".to_string()]);
        assert!(esbuild.has_install_script);
    }

    #[test]
    fn pnpm_resolve_dependency_from_member_importer() {
        let lock = PnpmLockfile::parse(pnpm_fixture()).unwrap();
        let got = lock.resolve_dependency("packages/a", "lodash").unwrap();
        assert_eq!(
            got,
            Some(DepResolution::ThirdParty {
                name: "lodash".to_string(),
                version: "4.17.21".to_string()
            })
        );
    }

    #[test]
    fn pnpm_resolve_dependency_link_is_workspace() {
        let lock = PnpmLockfile::parse(pnpm_fixture()).unwrap();
        let got = lock.resolve_dependency("packages/a", "b").unwrap();
        assert_eq!(got, Some(DepResolution::Workspace));
    }

    #[test]
    fn pnpm_resolve_dependency_unknown_importer_is_none() {
        let lock = PnpmLockfile::parse(pnpm_fixture()).unwrap();
        assert_eq!(
            lock.resolve_dependency("packages/missing", "lodash")
                .unwrap(),
            None
        );
    }

    #[test]
    fn split_pnpm_key_handles_scoped_and_peer_suffix() {
        assert_eq!(
            split_pnpm_key("@esbuild/darwin-arm64@0.19.0"),
            Some(("@esbuild/darwin-arm64".to_string(), "0.19.0".to_string()))
        );
        assert_eq!(
            split_pnpm_key("foo@1.2.3(react@18.0.0)"),
            Some(("foo".to_string(), "1.2.3".to_string()))
        );
    }

    // ---- Lockfile dispatch ----

    #[test]
    fn lockfile_parse_dispatches_by_pkgmanager() {
        let npm = Lockfile::parse(PkgManager::Npm, npm_fixture()).unwrap();
        assert!(matches!(npm, Lockfile::Npm(_)));
        let pnpm = Lockfile::parse(PkgManager::Pnpm, pnpm_fixture()).unwrap();
        assert!(matches!(pnpm, Lockfile::Pnpm(_)));
    }

    #[test]
    fn lockfile_filename_matches_manager() {
        assert_eq!(Lockfile::filename(PkgManager::Npm), "package-lock.json");
        assert_eq!(Lockfile::filename(PkgManager::Pnpm), "pnpm-lock.yaml");
    }

    // ---- transitive resolution ----

    fn transitive_manifest() -> PackageManifest {
        PackageManifest {
            name: "a".to_string(),
            main: None,
            dependencies: BTreeMap::new(),
            dev_dependencies: [("typescript-eslint".to_string(), "^8.0.0".to_string())].into(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
        }
    }

    /// pnpm's strict, non-flat `node_modules` means the *direct* lookup
    /// (`resolve_dependency`, `importers`-only by design — see the module
    /// docs' "M1" note) never finds a name that isn't itself in `a`'s own
    /// `importers` entry, unlike npm's ancestor-`node_modules` walk. This is
    /// exactly the case `resolve_transitive` exists to cover on pnpm.
    fn pnpm_transitive_fixture() -> &'static str {
        r#"
lockfileVersion: '9.0'
importers:
  packages/a:
    devDependencies:
      typescript-eslint:
        specifier: ^8.0.0
        version: 8.0.0
packages:
  typescript-eslint@8.0.0:
    resolution: {integrity: sha512-abc}
  '@eslint/js@9.0.0':
    resolution: {integrity: sha512-def}
  unrelated@1.0.0:
    resolution: {integrity: sha512-ghi}
snapshots:
  typescript-eslint@8.0.0:
    dependencies:
      '@eslint/js': 9.0.0
  '@eslint/js@9.0.0': {}
  unrelated@1.0.0: {}
"#
    }

    #[test]
    fn pnpm_direct_lookup_does_not_see_a_transitive_name() {
        let lock = Lockfile::parse(PkgManager::Pnpm, pnpm_transitive_fixture()).unwrap();
        assert_eq!(
            lock.resolve_dependency("packages/a", "@eslint/js").unwrap(),
            None,
            "pnpm's importers-only lookup must not find a name it never declared directly"
        );
    }

    #[test]
    fn resolve_transitive_finds_a_pnpm_companion_package() {
        let lock = Lockfile::parse(PkgManager::Pnpm, pnpm_transitive_fixture()).unwrap();
        let graph = lock.resolved_graph();
        let manifest = transitive_manifest();
        let got = resolve_transitive(&lock, &graph, "packages/a", &manifest, "@eslint/js")
            .unwrap()
            .expect("reachable through the declared typescript-eslint");
        assert_eq!(
            got,
            DepResolution::ThirdParty {
                name: "@eslint/js".to_string(),
                version: "9.0.0".to_string()
            }
        );
    }

    #[test]
    fn resolve_transitive_does_not_find_an_unrelated_pnpm_package() {
        let lock = Lockfile::parse(PkgManager::Pnpm, pnpm_transitive_fixture()).unwrap();
        let graph = lock.resolved_graph();
        let manifest = transitive_manifest();
        assert_eq!(
            resolve_transitive(&lock, &graph, "packages/a", &manifest, "unrelated").unwrap(),
            None,
            "a package with no edge from anything `a` declares is still a genuine phantom, \
             not merely present somewhere in the lockfile"
        );
    }

    #[test]
    fn resolve_transitive_prefers_the_direct_lookup_when_it_hits() {
        // npm's fixture already declares `lodash` directly from `packages/a`
        // — the transitive fallback must not shadow that with some other
        // resolution path; it only kicks in when the direct lookup misses.
        let lock = Lockfile::parse(PkgManager::Npm, npm_fixture()).unwrap();
        let graph = lock.resolved_graph();
        let manifest = PackageManifest {
            name: "a".to_string(),
            main: None,
            dependencies: [("lodash".to_string(), "^4.17.21".to_string())].into(),
            dev_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
        };
        let got = resolve_transitive(&lock, &graph, "packages/a", &manifest, "lodash").unwrap();
        assert_eq!(
            got,
            Some(DepResolution::ThirdParty {
                name: "lodash".to_string(),
                version: "4.17.21".to_string()
            })
        );
    }
}
