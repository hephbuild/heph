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
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

use crate::pluginjs::package_json::PackageManifest;
use crate::pluginjs::platform;
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
    /// [`entries_agree_where_comparable`] does, rather than skipping the
    /// whole entry on this field alone.
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
    /// `optionalDependencies` edges — same shape as [`Self::dependencies`],
    /// but only ever consumed where a miss is expected and tolerated (e.g.
    /// `Provider::thirdparty_install_spec`'s lifecycle-script sibling
    /// resolution): a name here that the lockfile never actually resolved
    /// (the common case — a platform-specific native-binary package for
    /// every *other* platform) simply doesn't appear as a key, the same way
    /// `resolve_npm_edges`/`resolved_graph`'s pnpm equivalent already drops
    /// any edge that doesn't resolve. Kept as a field separate from
    /// `dependencies` (not merged into it), because the two are consumed
    /// with opposite failure semantics wherever both matter — see
    /// `deps::resolve_one_dependency`'s doc for the established required-vs-
    /// optional asymmetry this mirrors.
    pub optional_dependencies: BTreeMap<String, String>,
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

/// One package [`ResolvedGraph::transitive_reachable`] says must be
/// materialized. `nested_under: []` (empty) is the ordinary, overwhelmingly
/// common case — placed flat, directly in the consuming package's own
/// `node_modules/<name>`, exactly as before this type existed. A non-empty
/// chain is a diamond-dependency override: each element names a package
/// this entry must be nested one `node_modules/` level deeper inside,
/// outermost first (`node_modules/<chain[0]>/node_modules/<chain[1]>/…`),
/// because the innermost chain element's own dependency on this name
/// resolves to a different version than what wins flat elsewhere in the
/// same closure — see `transitive_reachable`'s doc for the full mechanism.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransitiveEntry {
    pub name: String,
    pub version: String,
    pub nested_under: Vec<String>,
}

impl ResolvedGraph {
    pub fn get(&self, name: &str, version: &str) -> Option<&ResolvedPackage> {
        self.packages.get(&graph_key(name, version))
    }

    /// BFS outward from `seed_keys` (graph keys already known to be reachable
    /// — e.g. a consumer's own direct dependencies, resolved), returning
    /// every reachable package to materialize — see [`TransitiveEntry`].
    ///
    /// Walks both `dependencies` (unconditionally) and `optional_dependencies`
    /// (only an entry that resolves in this graph *and* matches `os`/`arch`)
    /// — confirmed live: `vite` depends on `rolldown`, which ships its native
    /// binding as an `optionalDependencies` entry per platform
    /// (`@rolldown/binding-linux-x64-gnu`, say); walking `dependencies` alone
    /// reaches `rolldown` but never its own binding, so the binding is never
    /// relocated into the sandbox and `vite` fails to load at require-time —
    /// not a lifecycle-script failure, an ordinary module-resolution one, one
    /// edge past what this closure used to declare. An unresolvable or
    /// platform-mismatched optional entry is expected (a native-binary
    /// package for another platform) and is silently skipped rather than
    /// walked further, mirroring [`crate::pluginjs::deps::resolve_one_dependency`]'s
    /// established required-vs-optional asymmetry — this is the "expected
    /// and tolerated" consumer [`ResolvedPackage::optional_dependencies`]'s
    /// own doc already names.
    ///
    /// **Diamond dependencies.** Real npm/pnpm lockfiles routinely resolve
    /// the same name to *different* versions for different consumers — npm's
    /// own nested-`node_modules` overrides exist exactly for this (confirmed
    /// live: `estree-walker` resolved to v2.0.2 for most consumers but
    /// v3.0.3 for `@module-federation/vite`/`@vitest/mocker` specifically;
    /// also confirmed live at depth 2: `@netskope-ui/core`, itself an
    /// override, has its own dependency on `@floating-ui/react` that
    /// diverges *again* from that name's own flat default). Each
    /// `ResolvedPackage`'s own `dependencies`/`optional_dependencies`
    /// already correctly captures this — [`resolve_npm_edges`] resolves each
    /// node's own edges via *that node's own* ancestor-`node_modules` walk,
    /// so two different nodes' own edges for the same declared name can
    /// legitimately point at two different graph keys. This walk runs in two
    /// passes to surface that correctly instead of flattening it away:
    ///
    /// 1. **Flat pass** (exactly the walk this function always did): BFS the
    ///    reachable node set, first-reached-wins per name — deterministic
    ///    given deterministic seed order and `BTreeMap`-ordered edges. This
    ///    produces the *default* placement for every reachable name and is
    ///    unconditionally correct for the overwhelming majority of names,
    ///    which never conflict.
    /// 2. **Override pass**: starting from every flat (depth-0) node, walk
    ///    its own edges and compare each against the flat pass's choice for
    ///    that name. Agreement means nothing extra is needed — Node's own
    ///    ancestor `node_modules` walk from inside that node's own placement
    ///    naturally continues up to the flat placement (this is checked only
    ///    against the *flat* default, not against every intermediate
    ///    ancestor's own overrides — correct for the overwhelming majority
    ///    of chains, where each level's divergence is from the ordinary flat
    ///    default rather than from another override two-or-more levels up).
    ///    A genuine divergence means this edge's target must be nested one
    ///    level inside the *visiting* node's own placement, and its own
    ///    edges are then walked the same way — so a chain of any length is
    ///    supported, not just one level. A node reached as a divergent
    ///    target from more than one distinct placement gets its own
    ///    independently-computed subtree at *each* placement (never
    ///    deduplicated across placements — two different overriding parents
    ///    can need genuinely different nested contents below them). Cycles
    ///    in the "who overrides whom" relation, and any other pathological
    ///    input, are bounded by [`MAX_NESTED_DEPTH`] — exceeding it is a
    ///    loud, named error rather than an infinite walk or a silent
    ///    mis-placement. An edge whose target graph key has no entry in this
    ///    graph at all (an `npm:`-aliased dependency's declaring key
    ///    diverging from the resolved package's own real name — see
    ///    [`resolve_npm_edges`]'s "a miss here is not an error at this
    ///    layer" doc) is skipped, never a panic: this walk can only
    ///    materialize a package that's actually in `self.packages`.
    pub fn transitive_reachable(
        &self,
        seed_keys: impl IntoIterator<Item = String>,
        os: &str,
        arch: &str,
    ) -> anyhow::Result<Vec<TransitiveEntry>> {
        // Pass 1: flat reachability — unchanged from this function's
        // original shape, first-reached-wins per name.
        let mut flat: BTreeMap<String, String> = BTreeMap::new();
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
            flat.entry(pkg.name.clone()).or_insert_with(|| key.clone());
            for dep_key in pkg.dependencies.values() {
                if seen_keys.insert(dep_key.clone()) {
                    queue.push_back(dep_key.clone());
                }
            }
            for dep_key in pkg.optional_dependencies.values() {
                let Some(dep_pkg) = self.packages.get(dep_key) else {
                    continue;
                };
                if platform::matches_platform(&dep_pkg.os, &dep_pkg.cpu, os, arch)
                    && seen_keys.insert(dep_key.clone())
                {
                    queue.push_back(dep_key.clone());
                }
            }
        }

        let mut entries: BTreeSet<TransitiveEntry> = BTreeSet::new();
        for (name, key) in &flat {
            let pkg = self
                .packages
                .get(key)
                .expect("every value in `flat` was looked up from `self.packages` in pass 1");
            entries.insert(TransitiveEntry {
                name: name.clone(),
                version: pkg.version.clone(),
                nested_under: Vec::new(),
            });
        }

        // Pass 2: per-edge divergence detection against the flat map,
        // recursed to whatever depth the graph actually needs — see this
        // function's doc. Work items are (graph_key, placement path) pairs,
        // not bare graph keys: a node reached as a divergent target from
        // more than one distinct placement needs its own independently
        // re-walked subtree at each one (a naive single-visit-per-node walk
        // would silently drop every occurrence but the first). `visited`
        // dedupes on the exact (key, path) pair, which — combined with the
        // `MAX_NESTED_DEPTH` cap below — bounds the walk even for a cyclic
        // "who overrides whom" relation.
        let mut work: VecDeque<(String, Vec<String>)> =
            flat.values().map(|key| (key.clone(), Vec::new())).collect();
        let mut visited: HashSet<(String, Vec<String>)> = HashSet::new();
        while let Some((key, path)) = work.pop_front() {
            if !visited.insert((key.clone(), path.clone())) {
                continue;
            }
            let pkg = self
                .packages
                .get(&key)
                .expect("every key ever pushed onto `work` was confirmed present below");
            let edges = pkg
                .dependencies
                .iter()
                .chain(pkg.optional_dependencies.iter().filter(|(_, dep_key)| {
                    self.packages.get(*dep_key).is_some_and(|dep_pkg| {
                        platform::matches_platform(&dep_pkg.os, &dep_pkg.cpu, os, arch)
                    })
                }));
            for (child_name, child_key) in edges {
                // An edge whose own target isn't in this graph at all (an
                // `npm:`-aliased dependency's declaring key diverging from
                // the resolved package's real name — see
                // `resolve_npm_edges`'s doc) is never materializable,
                // regardless of what the flat map says for this name —
                // skip it the same way pass 1 already tolerates an
                // unresolvable edge, rather than comparing/recording
                // something `self.packages.get` can never find later.
                let Some(child_pkg) = self.packages.get(child_key) else {
                    continue;
                };
                let Some(flat_key) = flat.get(child_name) else {
                    // No node anywhere in the closure resolved this name at
                    // all (an entirely-unresolvable edge — `resolve_npm_edges`
                    // already drops those upstream); nothing to compare
                    // against, nothing to override.
                    continue;
                };
                if child_key == flat_key {
                    continue;
                }
                anyhow::ensure!(
                    path.len() < MAX_NESTED_DEPTH,
                    "js provider: diamond-dependency conflict chain exceeds this milestone's \
                     {MAX_NESTED_DEPTH}-level nesting limit while resolving `{child_name}` under \
                     {path:?} — this is almost certainly a cycle in the lockfile's own override \
                     structure, not a legitimate dependency tree. Regenerate the lockfile."
                );
                let mut child_path = path.clone();
                child_path.push(pkg.name.clone());
                entries.insert(TransitiveEntry {
                    name: child_name.clone(),
                    version: child_pkg.version.clone(),
                    nested_under: child_path.clone(),
                });
                work.push_back((child_key.clone(), child_path));
            }
        }
        Ok(entries.into_iter().collect())
    }
}

/// Safety cap on [`ResolvedGraph::transitive_reachable`]'s override-nesting
/// depth — real confirmed diamond-dependency chains have been observed up
/// to depth 2 (`@netskope-ui/core` → `@floating-ui/react`); this is set far
/// above any legitimate chain, existing only to turn a cyclic or otherwise
/// pathological "who overrides whom" relation into a loud, named error
/// instead of unbounded work.
const MAX_NESTED_DEPTH: usize = 16;

/// Whether two `ResolvedPackage` entries for the same `(name, version)` are
/// consistent enough to treat as "the same package" — used both across
/// independent lockfile roots ([`crate::pluginjs::provider::Provider::find_resolved_graph_for`]'s
/// ambiguity check) and within a single lockfile, when more than one
/// `packages` path collapses onto the same graph key (a nested npm
/// dedup/hoisting pointer, or a pnpm peer-suffix variant — see the two
/// `resolved_graph()` impls below).
///
/// Compares exactly the fields that feed `js_install`'s own cache key
/// (`driver_install.rs`'s `JsInstallDef`: `name`/`version` are already equal
/// by construction — both sides were looked up by the same `(name,
/// version)` — leaving `integrity`, `resolved`, and `has_install_script`) —
/// nothing else. `os`/`cpu`/`dependencies`/`optional_dependencies` are
/// deliberately never compared:
///
/// - `os`/`cpu` gate *whether* an install happens at all
///   (`Provider::thirdparty_install_spec`'s `platform::matches_platform`
///   check, run separately against whichever entry this function accepts),
///   never part of `JsInstallDef` itself — and, being fields of the
///   published `package.json` `integrity` itself verifies, they cannot
///   differ between two entries that agree on `integrity` without a
///   lockfile parser bug, which this check is not the place to catch.
/// - `dependencies`/`optional_dependencies` are pure graph-traversal
///   bookkeeping (which *other* packages this one's own imports/optional
///   siblings resolve to) — never part of `JsInstallDef`, and legitimately
///   different between two independently `npm install`ed lockfiles even for
///   the *exact same* published tarball: a shared package's own transitive
///   dependency can resolve to a different patch version depending on what
///   else was installed and when. Comparing `dependencies` produced
///   false-positive ambiguity errors across hundreds of packages in a real
///   workspace — including pairs with byte-identical `integrity` and
///   `resolved` — before this fix; `optional_dependencies` has the exact
///   same shape and the exact same divergence property, so it is excluded
///   for the identical reason, not merely by omission.
///
/// `integrity`/`resolved` each get a narrow, independent exemption: when
/// either side's `integrity` is empty, that field alone is skipped; when
/// either side's `resolved` is `None`, that field alone is skipped. An entry
/// that never actually pinned this package's content on its own (a
/// peer-suffixed/deduped graph key folded onto one node picking up a
/// variant with no resolution data of its own is a real, observed shape)
/// naturally has *neither* signal, not just one, and can't confirm or deny
/// a conflict on a field it never recorded — but this exemption is
/// per-field, not per-entry: an entry with empty `integrity` but a real,
/// populated `resolved` pointing at an internal mirror still conflicts
/// against a different entry's real, differing `resolved`.
pub(crate) fn entries_agree_where_comparable(a: &ResolvedPackage, b: &ResolvedPackage) -> bool {
    let integrity_agrees =
        a.integrity.is_empty() || b.integrity.is_empty() || a.integrity == b.integrity;
    let resolved_agrees = a.resolved.is_none() || b.resolved.is_none() || a.resolved == b.resolved;
    integrity_agrees && resolved_agrees && a.has_install_script == b.has_install_script
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
    os: &str,
    arch: &str,
) -> anyhow::Result<Option<DepResolution>> {
    if let Some(direct) = lockfile.resolve_dependency(from_pkg, name)? {
        return Ok(Some(direct));
    }
    let seed_keys = direct_dep_seed_keys(lockfile, from_pkg, manifest)?;
    // Only the *flat* (depth-0) entry ever applies here: first-party source
    // is never physically nested inside a third-party override's own
    // directory, so it only ever reaches `name` via an ordinary ancestor
    // `node_modules` walk — exactly what the flat placement is for. See
    // `TransitiveEntry`'s doc.
    Ok(resolved_graph
        .transitive_reachable(seed_keys, os, arch)?
        .into_iter()
        .find(|e| e.nested_under.is_empty() && e.name == name)
        .map(|e| DepResolution::ThirdParty {
            name: e.name,
            version: e.version,
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

    pub fn resolved_graph(&self) -> anyhow::Result<ResolvedGraph> {
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
    optional_dependencies: BTreeMap<String, String>,
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

    pub fn resolved_graph(&self) -> anyhow::Result<ResolvedGraph> {
        let mut packages: BTreeMap<String, ResolvedPackage> = BTreeMap::new();
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
            let resolved = ResolvedPackage {
                name: name.clone(),
                version: version.clone(),
                integrity: entry.integrity.clone().unwrap_or_default(),
                resolved: entry.resolved.clone(),
                dependencies: resolve_npm_edges(self, path, &entry.dependencies),
                optional_dependencies: resolve_npm_edges(self, path, &entry.optional_dependencies),
                os: entry.os.clone(),
                cpu: entry.cpu.clone(),
                has_install_script: entry.has_install_script,
            };
            // More than one `node_modules/.../<name>` path can resolve to
            // the same `(name, version)` — npm's own dedup/hoisting can
            // leave a nested "pointer" entry with no integrity/resolved of
            // its own (redundant with a real entry elsewhere in the same
            // lockfile) alongside the genuinely-resolved one (confirmed
            // live: `js_install` failing to verify an empty integrity
            // string for a package the lockfile actually does pin,
            // elsewhere in the same file). Iteration is in path-alphabetical
            // order, not "real entry first", so this can't just keep
            // whichever is seen first — it must actively prefer the
            // non-degenerate side, and hard-fail rather than silently pick
            // between two *both* non-degenerate entries that disagree
            // (`entries_agree_where_comparable` — same fields, same
            // reasoning, as the cross-lockfile-root ambiguity check this
            // mirrors).
            match packages.entry(key) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(resolved);
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    anyhow::ensure!(
                        entries_agree_where_comparable(o.get(), &resolved),
                        "js provider: {name}@{version} has two disagreeing entries within \
                         the same lockfile — {:?} at one `node_modules` path records \
                         integrity {:?} resolved from {:?}, another records integrity {:?} \
                         resolved from {:?}. This is not a cross-project ambiguity; the \
                         lockfile itself is inconsistent for this package — regenerate it",
                        path,
                        o.get().integrity,
                        o.get().resolved,
                        resolved.integrity,
                        resolved.resolved,
                    );
                    if o.get().integrity.is_empty() && !resolved.integrity.is_empty() {
                        o.insert(resolved);
                    }
                }
            }
        }
        Ok(ResolvedGraph { packages })
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
#[serde(rename_all = "camelCase")]
struct PnpmSnapshotRaw {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
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

    pub fn resolved_graph(&self) -> anyhow::Result<ResolvedGraph> {
        // Shared by both `dependencies` and `optional_dependencies` below —
        // a pnpm snapshot's own edge map (`{dep_name: dep_version}`) resolves
        // to graph keys identically regardless of which field it came from;
        // only the *caller* (`resolved_graph` here vs
        // `Provider::thirdparty_install_spec`, much later) treats a miss
        // differently.
        fn resolve_pnpm_edges(declared: &BTreeMap<String, String>) -> BTreeMap<String, String> {
            declared
                .iter()
                .filter_map(|(dep_name, dep_version)| {
                    let (dep_name2, dep_version2) =
                        split_pnpm_key(&format!("{dep_name}@{dep_version}"))?;
                    Some((dep_name.clone(), graph_key(&dep_name2, &dep_version2)))
                })
                .collect()
        }

        let mut packages: BTreeMap<String, ResolvedPackage> = BTreeMap::new();
        for (key, meta) in &self.packages {
            let Some((name, version)) = split_pnpm_key(key) else {
                continue;
            };
            let snapshot = self.snapshots.get(key);
            let dependencies = snapshot
                .map(|snap| resolve_pnpm_edges(&snap.dependencies))
                .unwrap_or_default();
            let optional_dependencies = snapshot
                .map(|snap| resolve_pnpm_edges(&snap.optional_dependencies))
                .unwrap_or_default();
            let resolved_pkg = ResolvedPackage {
                name: name.clone(),
                version: version.clone(),
                integrity: meta.resolution.integrity.clone().unwrap_or_default(),
                resolved: meta.resolution.tarball.clone(),
                dependencies,
                optional_dependencies,
                os: meta.os.clone(),
                cpu: meta.cpu.clone(),
                has_install_script: meta.requires_build,
            };
            // Peer-dep variants of the same (name, version) — e.g.
            // `foo@1.2.3(react@18.0.0)` and `foo@1.2.3(react@17.0.0)` —
            // collapse to the same graph_key via `split_pnpm_key`'s
            // peer-suffix strip. Same rule, same reasoning, as the npm
            // side's collision handling: prefer whichever variant actually
            // has integrity, and hard-fail rather than silently pick
            // between two non-degenerate variants that disagree.
            match packages.entry(graph_key(&name, &version)) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(resolved_pkg);
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    anyhow::ensure!(
                        entries_agree_where_comparable(o.get(), &resolved_pkg),
                        "js provider: {name}@{version} has two disagreeing peer-dep-variant \
                         entries within the same lockfile — {:?} records integrity {:?} \
                         resolved from {:?}, another variant records integrity {:?} resolved \
                         from {:?}. This is not a cross-project ambiguity; the lockfile \
                         itself is inconsistent for this package — regenerate it",
                        key,
                        o.get().integrity,
                        o.get().resolved,
                        resolved_pkg.integrity,
                        resolved_pkg.resolved,
                    );
                    if o.get().integrity.is_empty() && !resolved_pkg.integrity.is_empty() {
                        o.insert(resolved_pkg);
                    }
                }
            }
        }
        Ok(ResolvedGraph { packages })
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
        let graph = lock.resolved_graph().unwrap();
        assert_eq!(graph.packages.len(), 2, "{:?}", graph.packages.keys());
        assert!(graph.get("lodash", "4.17.21").is_some());
        assert!(graph.get("@esbuild/darwin-arm64", "0.19.0").is_some());
    }

    #[test]
    fn npm_resolved_graph_carries_integrity_and_platform() {
        let lock = NpmLockfile::parse(npm_fixture()).unwrap();
        let graph = lock.resolved_graph().unwrap();
        let esbuild = graph.get("@esbuild/darwin-arm64", "0.19.0").unwrap();
        assert_eq!(esbuild.integrity, "sha512-def");
        assert_eq!(esbuild.os, vec!["darwin".to_string()]);
        assert_eq!(esbuild.cpu, vec!["arm64".to_string()]);
        assert!(esbuild.has_install_script);
    }

    #[test]
    fn npm_resolved_graph_resolves_optional_dependencies_edges_separately_from_dependencies() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/esbuild": {
                        "version": "0.25.12",
                        "integrity": "sha512-loader",
                        "dependencies": { "left-pad": "1.0.0" },
                        "optionalDependencies": { "@esbuild/linux-x64": "0.25.12" }
                    },
                    "node_modules/left-pad": {
                        "version": "1.0.0",
                        "integrity": "sha512-leftpad"
                    },
                    "node_modules/@esbuild/linux-x64": {
                        "version": "0.25.12",
                        "integrity": "sha512-native",
                        "os": ["linux"],
                        "cpu": ["x64"]
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();
        let esbuild = graph.get("esbuild", "0.25.12").unwrap();
        assert_eq!(
            esbuild.dependencies.get("left-pad").map(String::as_str),
            Some("left-pad@1.0.0"),
            "required dependencies must still resolve unaffected: {:?}",
            esbuild.dependencies
        );
        assert_eq!(
            esbuild
                .optional_dependencies
                .get("@esbuild/linux-x64")
                .map(String::as_str),
            Some("@esbuild/linux-x64@0.25.12"),
            "optionalDependencies must resolve into their own field: {:?}",
            esbuild.optional_dependencies
        );
        assert!(
            !esbuild.dependencies.contains_key("@esbuild/linux-x64"),
            "an optional edge must never leak into the required dependencies map"
        );
    }

    #[test]
    fn npm_resolved_graph_prefers_real_entry_when_degenerate_path_sorts_after() {
        // "node_modules/widget" < "zzz/node_modules/widget" alphabetically,
        // so the degenerate (no integrity) dedup-pointer entry is visited
        // *second* — exactly the ordering that clobbered a real entry
        // before this was fixed.
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/widget": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/widget/-/widget-1.0.0.tgz",
                        "integrity": "sha512-real"
                    },
                    "zzz/node_modules/widget": {
                        "version": "1.0.0"
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();
        let widget = graph.get("widget", "1.0.0").unwrap();
        assert_eq!(widget.integrity, "sha512-real");
        assert_eq!(
            widget.resolved.as_deref(),
            Some("https://registry.npmjs.org/widget/-/widget-1.0.0.tgz")
        );
    }

    #[test]
    fn npm_resolved_graph_prefers_real_entry_when_degenerate_path_sorts_before() {
        // Reverse ordering from the test above — proves the fix isn't
        // accidentally order-dependent in the other direction.
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "aaa/node_modules/widget": {
                        "version": "1.0.0"
                    },
                    "node_modules/widget": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/widget/-/widget-1.0.0.tgz",
                        "integrity": "sha512-real"
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();
        let widget = graph.get("widget", "1.0.0").unwrap();
        assert_eq!(widget.integrity, "sha512-real");
        assert_eq!(
            widget.resolved.as_deref(),
            Some("https://registry.npmjs.org/widget/-/widget-1.0.0.tgz")
        );
    }

    #[test]
    fn npm_resolved_graph_errors_on_two_real_entries_that_disagree() {
        // Both paths record real (non-empty) integrity for the same
        // (name, version), but a different one — a genuinely inconsistent
        // lockfile, not a degenerate-dedup-pointer shape. Must fail loudly
        // rather than silently pick whichever path sorts first.
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/widget": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/widget/-/widget-1.0.0.tgz",
                        "integrity": "sha512-aaa"
                    },
                    "packages/b/node_modules/widget": {
                        "version": "1.0.0",
                        "resolved": "https://internal-mirror.example/widget-1.0.0.tgz",
                        "integrity": "sha512-bbb"
                    }
                }
            }"#,
        )
        .unwrap();
        let err = lock.resolved_graph().unwrap_err();
        assert!(
            format!("{err:#}").contains("widget"),
            "error should name the conflicting package: {err:#}"
        );
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
        let graph = lock.resolved_graph().unwrap();
        assert!(graph.get("lodash", "4.17.21").is_some());
        let esbuild = graph.get("@esbuild/darwin-arm64", "0.19.0").unwrap();
        assert_eq!(esbuild.integrity, "sha512-def");
        assert_eq!(esbuild.os, vec!["darwin".to_string()]);
        assert_eq!(esbuild.cpu, vec!["arm64".to_string()]);
        assert!(esbuild.has_install_script);
    }

    #[test]
    fn pnpm_resolved_graph_resolves_optional_dependencies_edges_separately_from_dependencies() {
        let lock = PnpmLockfile::parse(
            r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      esbuild:
        specifier: ^0.25.12
        version: 0.25.12
packages:
  esbuild@0.25.12:
    resolution: {integrity: sha512-loader}
  left-pad@1.0.0:
    resolution: {integrity: sha512-leftpad}
  '@esbuild/linux-x64@0.25.12':
    resolution: {integrity: sha512-native}
    cpu: [x64]
    os: [linux]
snapshots:
  esbuild@0.25.12:
    dependencies:
      left-pad: 1.0.0
    optionalDependencies:
      '@esbuild/linux-x64': 0.25.12
  left-pad@1.0.0: {}
  '@esbuild/linux-x64@0.25.12': {}
"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();
        let esbuild = graph.get("esbuild", "0.25.12").unwrap();
        assert_eq!(
            esbuild.dependencies.get("left-pad").map(String::as_str),
            Some("left-pad@1.0.0"),
            "required dependencies must still resolve unaffected: {:?}",
            esbuild.dependencies
        );
        assert_eq!(
            esbuild
                .optional_dependencies
                .get("@esbuild/linux-x64")
                .map(String::as_str),
            Some("@esbuild/linux-x64@0.25.12"),
            "optionalDependencies must resolve into their own field: {:?}",
            esbuild.optional_dependencies
        );
        assert!(
            !esbuild.dependencies.contains_key("@esbuild/linux-x64"),
            "an optional edge must never leak into the required dependencies map"
        );
    }

    #[test]
    fn pnpm_resolved_graph_prefers_real_entry_on_peer_suffix_collision() {
        // Two peer-dep variants of the same (name, version) collapse to one
        // graph_key via `split_pnpm_key`'s peer-suffix strip; a variant
        // with no recorded integrity must never win over one that has it.
        let lock = PnpmLockfile::parse(
            r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      widget:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  widget@1.0.0(react@17.0.0):
    resolution: {}
  widget@1.0.0(react@18.0.0):
    resolution: {integrity: sha512-real}
snapshots:
  widget@1.0.0(react@17.0.0): {}
  widget@1.0.0(react@18.0.0): {}
"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();
        let widget = graph.get("widget", "1.0.0").unwrap();
        assert_eq!(widget.integrity, "sha512-real");
    }

    #[test]
    fn pnpm_resolved_graph_prefers_real_entry_regardless_of_peer_suffix_sort_order() {
        // Mirror image of the test above: the degenerate variant's peer
        // suffix ("react@99.0.0") sorts lexically *after* the real
        // variant's ("react@1.0.0") — the ordering under which a plain
        // `BTreeMap::insert` would already (accidentally) keep the real
        // entry. Proves the fix's `.entry()`/`and_modify` logic is
        // order-independent, not just favorably tested once.
        let lock = PnpmLockfile::parse(
            r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      widget:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  widget@1.0.0(react@1.0.0):
    resolution: {integrity: sha512-real}
  widget@1.0.0(react@99.0.0):
    resolution: {}
snapshots:
  widget@1.0.0(react@1.0.0): {}
  widget@1.0.0(react@99.0.0): {}
"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();
        let widget = graph.get("widget", "1.0.0").unwrap();
        assert_eq!(widget.integrity, "sha512-real");
    }

    #[test]
    fn pnpm_resolved_graph_errors_on_two_real_variants_that_disagree() {
        // Both peer-dep variants record real (non-empty) integrity, but a
        // different one — a genuinely inconsistent lockfile. Must fail
        // loudly rather than silently pick one.
        let lock = PnpmLockfile::parse(
            r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      widget:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  widget@1.0.0(react@17.0.0):
    resolution: {integrity: sha512-aaa}
  widget@1.0.0(react@18.0.0):
    resolution: {integrity: sha512-bbb}
snapshots:
  widget@1.0.0(react@17.0.0): {}
  widget@1.0.0(react@18.0.0): {}
"#,
        )
        .unwrap();
        let err = lock.resolved_graph().unwrap_err();
        assert!(
            format!("{err:#}").contains("widget"),
            "error should name the conflicting package: {err:#}"
        );
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
        let graph = lock.resolved_graph().unwrap();
        let manifest = transitive_manifest();
        let got = resolve_transitive(
            &lock,
            &graph,
            "packages/a",
            &manifest,
            "@eslint/js",
            "linux",
            "amd64",
        )
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
        let graph = lock.resolved_graph().unwrap();
        let manifest = transitive_manifest();
        assert_eq!(
            resolve_transitive(
                &lock,
                &graph,
                "packages/a",
                &manifest,
                "unrelated",
                "linux",
                "amd64"
            )
            .unwrap(),
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
        let graph = lock.resolved_graph().unwrap();
        let manifest = PackageManifest {
            name: "a".to_string(),
            main: None,
            dependencies: [("lodash".to_string(), "^4.17.21".to_string())].into(),
            dev_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
        };
        let got = resolve_transitive(
            &lock,
            &graph,
            "packages/a",
            &manifest,
            "lodash",
            "linux",
            "amd64",
        )
        .unwrap();
        assert_eq!(
            got,
            Some(DepResolution::ThirdParty {
                name: "lodash".to_string(),
                version: "4.17.21".to_string()
            })
        );
    }

    /// Confirmed live: `vite` depends on `rolldown`, which ships its native
    /// binding as an `optionalDependencies` entry per platform
    /// (`@rolldown/binding-linux-x64-gnu`, say) — `dependencies` alone
    /// reaches `rolldown` but never its own binding, so the binding was
    /// never relocated into the sandbox and `vite` failed to load at
    /// require-time. `transitive_reachable` must walk a platform-*matching*
    /// `optionalDependencies` edge exactly like a `dependencies` one.
    #[test]
    fn transitive_reachable_walks_a_platform_matching_optional_dependency() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
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
        .unwrap();
        let graph = lock.resolved_graph().unwrap();

        let reachable = graph
            .transitive_reachable(vec![graph_key("vite", "6.0.0")], "linux", "amd64")
            .unwrap();
        let names: BTreeSet<&str> = reachable.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains("rolldown"), "{reachable:?}");
        assert!(
            names.contains("@rolldown/binding-linux-x64-gnu"),
            "the current-platform optional binding must be walked transitively, two hops past \
             the seed: {reachable:?}"
        );
        assert!(
            !names.contains("@rolldown/binding-darwin-arm64"),
            "an optional dependency restricted to a different platform must never be walked: \
             {reachable:?}"
        );
    }

    /// The unresolvable-optional-edge half of the same guarantee: an
    /// `optionalDependencies` entry with no resolved graph entry at all
    /// (never installed on any platform, or a stale lockfile) must be
    /// skipped, not treated as a hard failure — mirrors every other
    /// `optionalDependencies` consumer's asymmetry in this crate.
    #[test]
    fn transitive_reachable_skips_an_unresolvable_optional_dependency() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/rolldown": {
                        "version": "1.0.0",
                        "integrity": "sha512-rolldown",
                        "optionalDependencies": { "missing-binding": "1.0.0" }
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();

        let reachable = graph
            .transitive_reachable(vec![graph_key("rolldown", "1.0.0")], "linux", "amd64")
            .unwrap();
        let names: BTreeSet<&str> = reachable.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains("rolldown"), "{reachable:?}");
        assert!(
            !names.contains("missing-binding"),
            "an optional dependency absent from the lockfile entirely must be skipped, not \
             error: {reachable:?}"
        );
    }

    /// Confirmed live: `estree-walker` resolved to v2.0.2 for most consumers
    /// but v3.0.3 for `@module-federation/vite` specifically, via npm's own
    /// nested `node_modules` override — the exact real-world shape this
    /// mechanism exists to close. `@module-federation/vite`'s own edge for
    /// `estree-walker` must produce a depth-1 override nested under its own
    /// placement; the flat default (v2.0.2) must still be the plain entry
    /// for everyone else.
    #[test]
    fn transitive_reachable_nests_a_diamond_dependency_override_one_level() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/other": {
                        "version": "1.0.0",
                        "integrity": "sha512-other",
                        "dependencies": { "estree-walker": "2.0.2" }
                    },
                    "node_modules/@module-federation/vite": {
                        "version": "1.0.0",
                        "integrity": "sha512-mfvite",
                        "dependencies": { "estree-walker": "3.0.3" }
                    },
                    "node_modules/@module-federation/vite/node_modules/estree-walker": {
                        "version": "3.0.3",
                        "integrity": "sha512-estree3"
                    },
                    "node_modules/estree-walker": {
                        "version": "2.0.2",
                        "integrity": "sha512-estree2"
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();

        // `other` seeded first so its edge to the root `estree-walker@2.0.2`
        // establishes the flat default before `@module-federation/vite`'s
        // own diverging edge is examined.
        let reachable = graph
            .transitive_reachable(
                vec![
                    graph_key("other", "1.0.0"),
                    graph_key("@module-federation/vite", "1.0.0"),
                ],
                "linux",
                "amd64",
            )
            .unwrap();

        assert!(
            reachable.contains(&TransitiveEntry {
                name: "estree-walker".to_string(),
                version: "2.0.2".to_string(),
                nested_under: Vec::new(),
            }),
            "the flat default must stay 2.0.2: {reachable:?}"
        );
        assert!(
            reachable.contains(&TransitiveEntry {
                name: "estree-walker".to_string(),
                version: "3.0.3".to_string(),
                nested_under: vec!["@module-federation/vite".to_string()],
            }),
            "the override must be nested under @module-federation/vite's own placement: \
             {reachable:?}"
        );
    }

    /// Two different, unrelated intermediate packages both needing the same
    /// divergent version of a name — each must get its own independently
    /// nested override, not just whichever one a BFS visits first (a naive
    /// single-visit-per-node implementation would silently drop the second).
    #[test]
    fn transitive_reachable_nests_the_same_override_under_two_different_parents() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/root-consumer": {
                        "version": "1.0.0",
                        "integrity": "sha512-root",
                        "dependencies": { "shared": "1.0.0" }
                    },
                    "node_modules/consumer-a": {
                        "version": "1.0.0",
                        "integrity": "sha512-a",
                        "dependencies": { "shared": "2.0.0" }
                    },
                    "node_modules/consumer-b": {
                        "version": "1.0.0",
                        "integrity": "sha512-b",
                        "dependencies": { "shared": "2.0.0" }
                    },
                    "node_modules/consumer-a/node_modules/shared": {
                        "version": "2.0.0",
                        "integrity": "sha512-shared2"
                    },
                    "node_modules/consumer-b/node_modules/shared": {
                        "version": "2.0.0",
                        "integrity": "sha512-shared2"
                    },
                    "node_modules/shared": {
                        "version": "1.0.0",
                        "integrity": "sha512-shared1"
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();

        // `root-consumer` seeded first so its edge to the root `shared@1.0.0`
        // establishes the flat default before either consumer's own
        // diverging edge is examined.
        let reachable = graph
            .transitive_reachable(
                vec![
                    graph_key("root-consumer", "1.0.0"),
                    graph_key("consumer-a", "1.0.0"),
                    graph_key("consumer-b", "1.0.0"),
                ],
                "linux",
                "amd64",
            )
            .unwrap();

        assert!(
            reachable.contains(&TransitiveEntry {
                name: "shared".to_string(),
                version: "2.0.0".to_string(),
                nested_under: vec!["consumer-a".to_string()],
            }),
            "{reachable:?}"
        );
        assert!(
            reachable.contains(&TransitiveEntry {
                name: "shared".to_string(),
                version: "2.0.0".to_string(),
                nested_under: vec!["consumer-b".to_string()],
            }),
            "consumer-b's own identical-version override must not be dropped just because \
             consumer-a's was already recorded: {reachable:?}"
        );
    }

    /// Repeated calls with the identical seed order must return
    /// byte-identical, identically-*ordered* output — every internal
    /// collection this walk builds must be `BTreeMap`/`BTreeSet`, never a
    /// `HashMap`/`HashSet` (which reseeds its hasher per-instance and would
    /// make two otherwise-identical calls silently disagree on iteration
    /// order). This is the reproducibility property real callers actually
    /// rely on: `direct_dep_seed_keys` always derives seeds from a
    /// `BTreeMap`-iterated manifest, so a fixed manifest always produces the
    /// same seed order — but *any* fixed order must stay stable call over
    /// call, not just the one production happens to use.
    #[test]
    fn transitive_reachable_is_stable_across_repeated_calls_with_the_same_seed_order() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/@module-federation/vite": {
                        "version": "1.0.0",
                        "integrity": "sha512-mfvite",
                        "dependencies": { "estree-walker": "3.0.3" }
                    },
                    "node_modules/@module-federation/vite/node_modules/estree-walker": {
                        "version": "3.0.3",
                        "integrity": "sha512-estree3"
                    },
                    "node_modules/other": {
                        "version": "1.0.0",
                        "integrity": "sha512-other",
                        "dependencies": { "estree-walker": "2.0.2" }
                    },
                    "node_modules/estree-walker": {
                        "version": "2.0.2",
                        "integrity": "sha512-estree2"
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();
        let seeds = vec![
            graph_key("@module-federation/vite", "1.0.0"),
            graph_key("other", "1.0.0"),
        ];

        let first = graph
            .transitive_reachable(seeds.clone(), "linux", "amd64")
            .unwrap();
        let second = graph.transitive_reachable(seeds, "linux", "amd64").unwrap();
        assert_eq!(first, second);
    }

    /// An `npm:`-aliased dependency's declaring key can diverge from the
    /// resolved package's real name (`resolve_npm_edges`'s established
    /// "a miss here is not an error at this layer" case), producing an edge
    /// whose graph key has no entry in `self.packages` at all — a
    /// code-quality review caught that an earlier draft `.expect()`-panicked
    /// materializing such an edge as an override instead of skipping it the
    /// same way pass 1 already does for an unresolvable required edge.
    #[test]
    fn transitive_reachable_skips_an_edge_whose_key_is_unresolvable_rather_than_panicking() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/foo": {
                        "version": "1.0.0",
                        "integrity": "sha512-foo",
                        "dependencies": { "lodash": "npm:lodash-es@4.17.21" }
                    },
                    "node_modules/bar": {
                        "version": "1.0.0",
                        "integrity": "sha512-bar",
                        "dependencies": { "lodash": "4.17.20" }
                    },
                    "node_modules/bar/node_modules/lodash": {
                        "version": "4.17.20",
                        "integrity": "sha512-lodash4172"
                    },
                    "node_modules/lodash-es": {
                        "version": "4.17.21",
                        "integrity": "sha512-lodashes"
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();

        // `bar` seeded first so its edge to the root `lodash@4.17.20`
        // establishes the flat default for `lodash` before `foo`'s own
        // aliased (and therefore dangling-graph-key) edge is examined.
        let reachable = graph
            .transitive_reachable(
                vec![graph_key("bar", "1.0.0"), graph_key("foo", "1.0.0")],
                "linux",
                "amd64",
            )
            .expect("an unresolvable aliased edge must be skipped, never a panic");

        assert!(
            reachable.contains(&TransitiveEntry {
                name: "lodash".to_string(),
                version: "4.17.20".to_string(),
                nested_under: Vec::new(),
            }),
            "{reachable:?}"
        );
        assert!(
            !reachable
                .iter()
                .any(|e| e.nested_under == vec!["foo".to_string()]),
            "foo's own aliased-but-dangling edge must never become an override: {reachable:?}"
        );
    }

    /// The exact real-world shape reported live: `@netskope-ui/core` is
    /// itself an override (its own name resolves to a different version by
    /// default elsewhere in the closure), and `@netskope-ui/core`'s own
    /// dependency on `@floating-ui/react` *also* diverges from that name's
    /// flat default — a genuine depth-2 chain. This must resolve, not hard
    /// fail: the earlier milestone capped nesting at one level and treated
    /// this as an error, which broke on the first real repo it was tested
    /// against.
    #[test]
    fn transitive_reachable_resolves_a_depth_two_diamond_dependency_chain() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/other": {
                        "version": "1.0.0",
                        "integrity": "sha512-other",
                        "dependencies": {
                            "@netskope-ui/core": "12.2.2",
                            "@floating-ui/react": "0.27.19"
                        }
                    },
                    "node_modules/@netskope-ui/core": {
                        "version": "12.2.2",
                        "integrity": "sha512-core222"
                    },
                    "node_modules/@floating-ui/react": {
                        "version": "0.27.19",
                        "integrity": "sha512-floating02719"
                    },
                    "node_modules/consumer": {
                        "version": "1.0.0",
                        "integrity": "sha512-consumer",
                        "dependencies": { "@netskope-ui/core": "12.2.0" }
                    },
                    "node_modules/consumer/node_modules/@netskope-ui/core": {
                        "version": "12.2.0",
                        "integrity": "sha512-core220",
                        "dependencies": { "@floating-ui/react": "0.24.8" }
                    },
                    "node_modules/consumer/node_modules/@netskope-ui/core/node_modules/@floating-ui/react": {
                        "version": "0.24.8",
                        "integrity": "sha512-floating0248"
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();

        // `other` seeded first so its edges establish both flat defaults
        // (@netskope-ui/core@12.2.2, @floating-ui/react@0.27.19) before
        // `consumer`'s own diverging chain is examined.
        let reachable = graph
            .transitive_reachable(
                vec![graph_key("other", "1.0.0"), graph_key("consumer", "1.0.0")],
                "linux",
                "amd64",
            )
            .expect("a depth-2 chain must resolve, not hard fail");

        assert!(
            reachable.contains(&TransitiveEntry {
                name: "@netskope-ui/core".to_string(),
                version: "12.2.2".to_string(),
                nested_under: Vec::new(),
            }),
            "the flat default must stay 12.2.2: {reachable:?}"
        );
        assert!(
            reachable.contains(&TransitiveEntry {
                name: "@netskope-ui/core".to_string(),
                version: "12.2.0".to_string(),
                nested_under: vec!["consumer".to_string()],
            }),
            "the override must be nested under consumer's own placement: {reachable:?}"
        );
        assert!(
            reachable.contains(&TransitiveEntry {
                name: "@floating-ui/react".to_string(),
                version: "0.27.19".to_string(),
                nested_under: Vec::new(),
            }),
            "the flat default must stay 0.27.19: {reachable:?}"
        );
        assert!(
            reachable.contains(&TransitiveEntry {
                name: "@floating-ui/react".to_string(),
                version: "0.24.8".to_string(),
                nested_under: vec!["consumer".to_string(), "@netskope-ui/core".to_string()],
            }),
            "the depth-2 override must be nested under consumer/node_modules/@netskope-ui/core's \
             own placement: {reachable:?}"
        );
    }

    /// A cyclic "who overrides whom" relation (a pathological, likely
    /// hand-corrupted lockfile — real npm/pnpm never produce this shape)
    /// must hit the `MAX_NESTED_DEPTH` safety cap and fail loudly rather
    /// than loop forever.
    #[test]
    fn transitive_reachable_hard_fails_past_the_max_nesting_depth() {
        let lock = NpmLockfile::parse(
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/other": {
                        "version": "1.0.0",
                        "integrity": "sha512-other",
                        "dependencies": { "a": "1.0.0", "b": "1.0.0" }
                    },
                    "node_modules/a": {
                        "version": "1.0.0",
                        "integrity": "sha512-a1"
                    },
                    "node_modules/b": {
                        "version": "1.0.0",
                        "integrity": "sha512-b1"
                    },
                    "node_modules/consumer": {
                        "version": "1.0.0",
                        "integrity": "sha512-consumer",
                        "dependencies": { "a": "2.0.0" }
                    },
                    "node_modules/consumer/node_modules/a": {
                        "version": "2.0.0",
                        "integrity": "sha512-a2",
                        "dependencies": { "b": "2.0.0" }
                    },
                    "node_modules/consumer/node_modules/a/node_modules/b": {
                        "version": "2.0.0",
                        "integrity": "sha512-b2",
                        "dependencies": { "a": "2.0.0" }
                    }
                }
            }"#,
        )
        .unwrap();
        let graph = lock.resolved_graph().unwrap();

        let err = graph
            .transitive_reachable(
                vec![graph_key("other", "1.0.0"), graph_key("consumer", "1.0.0")],
                "linux",
                "amd64",
            )
            .expect_err(
                "a@2.0.0 -> b@2.0.0 -> a@2.0.0 is a genuine cycle in the override relation and \
                 must hit the depth cap, never loop forever",
            );
        assert!(format!("{err:#}").contains("nesting limit"));
    }
}
