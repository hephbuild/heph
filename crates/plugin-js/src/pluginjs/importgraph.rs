//! M2's import-graph orchestration: walk a package's own first-party source
//! files, extract every static import/require/dynamic-import specifier
//! (`importparse.rs`), resolve each one under real Node/TypeScript semantics
//! (`resolvers.rs`), and cross-check every resolved edge that lands in
//! another package against that package's declared-dependency closure — the
//! phantom-dependency detector `ai-docs/js-plugin-plan.md`'s "Hermeticity"
//! section calls for.
//!
//! ## Why this is heph's job, not the package manager's
//!
//! pnpm's non-flat `node_modules` layout means an undeclared package is
//! usually simply not *resolvable* at all — pnpm polices this by
//! construction. npm's classic flat-hoisted `node_modules` has no such
//! guard: a transitive dependency's package can be `require`-able from a
//! sibling package purely because hoisting happened to place it within
//! reach, even though it was never declared. Cross-checking the actual
//! resolved-to filesystem path against `package.json`'s declared fields is
//! the one mechanism that catches this on *both* managers uniformly, since
//! it doesn't depend on which one produced the lockfile/`node_modules` tree.
//!
//! ## What counts as "declared"
//!
//! [`declared_closure`]: every name in `package.json`'s `dependencies`
//! (which already folds in `optionalDependencies`, see `package_json.rs`)
//! and `devDependencies`, plus the package's own `name` (Node's own
//! "self-reference" feature — a package may `import` its own `name` to reach
//! its own `exports` map without that ever being a "dependency" to declare).
//!
//! `devDependencies` are included deliberately: a package's *test* files
//! commonly import dev-only tooling (a test runner, `@types/*` packages) that
//! would otherwise flag as phantom despite being entirely legitimate.
//!
//! ## Why an unresolvable specifier does not itself fail the check
//!
//! Only a specifier that **did** resolve to a concrete filesystem path is
//! checked against the declared closure — that resolution is proof of
//! exactly which package the import actually lands in. A specifier that
//! failed to resolve (see `resolvers.rs`'s [`ResolveOutcome::Unresolved`])
//! proves nothing either way: real code has legitimately-unresolvable
//! specifiers at static-analysis time (an optional peer dependency behind a
//! `try { require(...) } catch {}`, a platform-conditional `require`, ...).
//! Per `ai-docs/js-plugin-plan.md`'s "Correctness safety valve", the real
//! toolchain remains the ground truth at execution time — heph's resolver
//! only informs the dependency graph and cache key. Hard-failing
//! `Provider::get` (which every target resolution needs to succeed through)
//! over an unresolvable-but-legitimate specifier would be strictly worse
//! than the alternative of leaving it unresolved and unchecked.
//!
//! ## Memoization
//!
//! [`ResolveCache`] memoizes `Resolvers::resolve_*` results keyed by `(base
//! directory or file, specifier, resolution flavor)`, per the task's
//! "memoize per (specifier, resolution-context) so a hot import reached from
//! many files is resolved once" instruction — reusing the Go plugin's
//! `import_closure`/`compose_closures` *pattern* (memoize the resolution,
//! not a downstream engine call) but not its machinery: the Go precedent's
//! async `Memoizer` exists specifically to dedupe *concurrent, cross-task*
//! `executor.result(addr)` calls into the engine and to avoid caching that
//! engine round-trip itself (so a real dependency cycle surfaces as a cycle
//! error rather than a hidden memoizer deadlock — see
//! `crates/plugin-go/src/plugingo/provider.rs`'s `pkg_cache` doc comment).
//! This walk never calls back into the engine per node at all — resolving an
//! import is a pure, synchronous, local filesystem operation — so there is
//! no engine-recursion cycle to protect against, and a plain synchronous
//! cache is the correct, simpler tool for the job. The cache lives for one
//! `Provider::get` call (one package); it is not persisted across the
//! `Provider`'s lifetime — see "Deliberate scope trims" below.
//!
//! ## Deliberate scope trims (stated, not silent)
//!
//! - Walks exactly `.js`/`.jsx`/`.ts`/`.tsx` first-party files, matching the
//!   task's literal scope — `.mjs`/`.cjs`/`.mts`/`.cts` first-party *sources*
//!   are not walked for their own outgoing edges (though all of them remain
//!   valid *resolution targets* another file's import can land on — see
//!   `resolvers.rs`'s `EXTENSIONS`). TODO M2+: widen the walk set if a real
//!   workspace needs first-party `.mts`/`.cts` sources checked.
//! - `Resolvers` are rebuilt fresh per `Provider::get` call rather than
//!   cached on the `Provider` across its lifetime (unlike `lockfile_cache`/
//!   `resolved_graph_cache`). Building one is cheap (no upfront filesystem
//!   work — `oxc_resolver`'s own internal cache starts empty and populates as
//!   `.resolve()` is called), but cross-request warm-cache reuse is left as a
//!   perf follow-up rather than added speculatively.
//! - `heph inspect resolve` (the trace command `ai-docs/js-plugin-plan.md`'s
//!   M2 milestone note mentions) is **not implemented by this change** — out
//!   of the 8 concrete deliverables this task was scoped to, it wasn't
//!   listed. Tracked as an explicit TODO, not silently dropped.
//!   `conformance.rs`'s synthetic-fixture corpus checked against `oxc_resolver`
//!   *is* implemented (see that module) — but its live cross-check against
//!   actual Node resolution only runs opportunistically, when `node` happens
//!   to be on `PATH`; nothing in this repo pins/provisions a Node toolchain
//!   for it, so `ai-docs/js-plugin-plan.md`'s "checked against actual Node
//!   resolution in CI, divergence fails the build" is not yet a guarantee —
//!   see that module's own doc for the honest, current state of that gap.
//!
//! ## Hermeticity: checking a bare specifier's *name*, not just where it
//! ## happens to resolve on disk
//!
//! `Provider::get` (and therefore this whole walk) runs at spec-resolution
//! time, strictly before any target — including `js_install` — ever executes
//! (see architecture.md). On a fresh checkout, no `node_modules` exists
//! anywhere yet, so every third-party specifier `oxc_resolver` is asked to
//! resolve on disk comes back [`ResolveOutcome::Unresolved`]. If the phantom
//! check only ever looked at *resolved* edges, it would silently do nothing
//! at all on exactly the most common real-world entry point (clone, then
//! build) — and, worse, whether it caught anything on a second run would
//! depend on incidental host state (did something install `node_modules`
//! here before, is it stale, is it hoisted-flat vs pnpm's non-flat layout) —
//! the same source, lockfile, and package.json producing a different
//! `Provider::get` outcome on different machines. That's exactly the
//! "ambient filesystem access" architecture.md's isolation model rules out.
//!
//! So a bare (non-relative, non-absolute, non-builtin, non-`#subpath`)
//! specifier is *also* checked directly against [`declared_closure`] by the
//! package name its own text names — Node's own package-name grammar
//! ([`bare_specifier_package_name`]) — independent of whether it happened to
//! resolve on this particular host. This runs whenever the disk resolution
//! came back `Unresolved`; a specifier that *did* resolve is already covered
//! by the existing (disk-based, opportunistically stronger, but
//! ambient-state-dependent) [`check_one_edge`] path. The two together mean a
//! phantom third-party import is now caught the same way on every host,
//! whether or not `node_modules` happens to be present.
//!
//! The one thing this name-based check must not do is misfire on a
//! TypeScript `tsconfig.json` path alias (`"@app/*": ["src/*"]`) that
//! *looks* bare-specifier-shaped but isn't an npm package at all — an
//! unresolvable alias (a typo, a moved file) would otherwise be reported as
//! an undeclared dependency named after the alias, which is exactly the kind
//! of over-detection that blocks a correct build. [`BareSpecifierGuard`]
//! reads the package's own (single-file, non-`extends`-following) tsconfig
//! for a `baseUrl`/`paths`/`extends` that could remap bare specifiers, and
//! disables the name-based check entirely for that package when it can't be
//! sure — conservative (some coverage given up for tsconfig-configured
//! packages with an `extends` chain or a bare `baseUrl`), but never a false
//! positive. See that type's doc for exactly what it can and can't see.

use crate::pluginjs::importparse::{self, ModuleContext, ParsedImports};
use crate::pluginjs::package_json::PackageManifest;
use crate::pluginjs::resolvers::{ResolveOutcome, Resolvers};
use crate::pluginjs::{PACKAGE_JSON, is_skipped_dir_name};
use anyhow::Context;
use hwalk::{CachedWalker, EntryKind};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// First-party source extensions this walk parses for outgoing edges — see
/// module docs' "Deliberate scope trims".
const SOURCE_EXTENSIONS: &[&str] = &["js", "jsx", "ts", "tsx"];

/// One resolved import edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEdge {
    /// Workspace-relative path of the file containing the import.
    pub file: String,
    pub specifier: String,
    pub resolved: PathBuf,
}

/// A bare specifier that did **not** resolve on disk, but whose own text
/// names an npm package per Node's package-name grammar — checked against
/// [`declared_closure`] directly, regardless of ambient `node_modules`
/// state. See module docs' "Hermeticity" section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BareSpecifierSite {
    /// Workspace-relative path of the file containing the import.
    pub file: String,
    pub specifier: String,
    /// The package name extracted from `specifier`'s own text (e.g. `"lodash"`
    /// from `"lodash/fp"`, `"@scope/pkg"` from `"@scope/pkg/sub"`).
    pub package_name: String,
}

/// The two separate graphs for one package — see module docs and
/// `resolvers.rs` for why they're kept apart.
#[derive(Debug, Clone, Default)]
pub struct ImportGraph {
    pub runtime_edges: Vec<ResolvedEdge>,
    pub type_edges: Vec<ResolvedEdge>,
    /// Count of dynamic `import()` call sites with a non-literal argument —
    /// coarsened, not resolved; see `importparse.rs` module docs.
    pub unresolved_dynamic_imports: usize,
    /// Bare specifiers that failed to resolve on disk but are still checked
    /// against the declared closure by name — see module docs' "Hermeticity"
    /// section and [`BareSpecifierGuard`].
    pub unresolved_bare_specifiers: Vec<BareSpecifierSite>,
}

/// Which resolution flavor a cache entry belongs to — the fourth element of
/// [`ResolveCache`]'s key (base path + specifier already distinguish most
/// collisions, but a runtime-ESM and runtime-CJS resolution of the same
/// specifier from the same directory can legitimately differ, see
/// `resolvers.rs`'s conditional-`exports` test).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CacheFlavor {
    RuntimeEsm,
    RuntimeCjs,
    Types,
}

/// Memoizes [`Resolvers`] output — see module docs' "Memoization" section.
#[derive(Default)]
pub struct ResolveCache {
    entries: Mutex<HashMap<(PathBuf, String, CacheFlavor), ResolveOutcome>>,
}

impl ResolveCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get_or_resolve(
        &self,
        resolvers: &Resolvers,
        flavor: CacheFlavor,
        base: &Path,
        specifier: &str,
    ) -> ResolveOutcome {
        let key = (base.to_path_buf(), specifier.to_string(), flavor);
        if let Some(hit) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return hit.clone();
        }
        let outcome = match flavor {
            CacheFlavor::RuntimeEsm => {
                resolvers.resolve_runtime(ModuleContext::Esm, base, specifier)
            }
            CacheFlavor::RuntimeCjs => {
                resolvers.resolve_runtime(ModuleContext::Cjs, base, specifier)
            }
            CacheFlavor::Types => resolvers.resolve_types(base, specifier),
        };
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, outcome.clone());
        outcome
    }
}

/// Every name a package is allowed to resolve an import into: its own
/// declared `dependencies` (already folds in `optionalDependencies`),
/// `devDependencies`, and `peerDependencies` (a peer dep is not this
/// package's own install dependency — `deps::resolve_package_deps` never
/// wires it — but importing it is a legitimate, extremely common pattern for
/// a component/plugin library, so it counts as declared here), plus the
/// package's own name (Node's own "self-reference" feature). See module
/// docs.
pub fn declared_closure(manifest: &PackageManifest) -> HashSet<String> {
    let mut set: HashSet<String> = manifest.dependencies.keys().cloned().collect();
    set.extend(manifest.dev_dependencies.keys().cloned());
    set.extend(manifest.peer_dependencies.keys().cloned());
    set.insert(manifest.name.clone());
    set
}

/// Extract the npm package name a **bare** specifier's own text names, per
/// Node's package-name grammar — `"lodash"` from `"lodash/fp"`, `"@scope/pkg"`
/// from `"@scope/pkg/sub"` — independent of whether it resolves to anything
/// on disk. `None` for anything that isn't a genuine bare package specifier:
/// relative (`.`/`..`), absolute (`/`), a Node builtin (`fs`, `node:fs`, …),
/// a subpath-imports specifier (`#internal/...`), or a malformed scope
/// (`@/foo` — npm scope names can't be empty, so this is a bundler/tsconfig
/// alias convention, never a real scoped package).
fn bare_specifier_package_name(specifier: &str) -> Option<String> {
    if specifier.is_empty()
        || specifier.starts_with('.')
        || specifier.starts_with('/')
        || specifier.starts_with('#')
        || nodejs_built_in_modules::is_nodejs_builtin_module(specifier)
    {
        return None;
    }
    if let Some(rest) = specifier.strip_prefix('@') {
        let mut parts = rest.splitn(2, '/');
        let scope = parts.next().filter(|s| !s.is_empty())?;
        let name = parts.next().filter(|s| !s.is_empty())?;
        let name = name.split('/').next().unwrap_or(name);
        return Some(format!("@{scope}/{name}"));
    }
    let name = specifier.split('/').next()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Whether a package's tsconfig could remap a bare specifier to something
/// other than an npm package of the same name — governs whether
/// [`bare_specifier_package_name`]'s hermetic check is safe to run at all for
/// a given specifier in that package. See module docs' "Hermeticity"
/// section.
///
/// Deliberately conservative and **single-file only**: it reads exactly the
/// tsconfig `build_package_import_graph` was already handed (the nearest one
/// found by [`find_nearest_tsconfig`]), not any `extends` chain — a
/// `baseUrl`/`paths` inherited through `extends` is invisible to it, so
/// `extends` disables the check entirely rather than risk missing an
/// inherited alias and false-positiving on it. This trades some coverage
/// (packages with a multi-file tsconfig setup don't get the new
/// no-`node_modules`-needed protection) for zero new false positives, which
/// is the correct side to err on for an over-detection risk (see
/// `package_json.rs`'s `peer_dependencies` doc for why over-detection is
/// treated as seriously as under-detection here).
#[derive(Debug, Clone, PartialEq, Eq)]
enum BareSpecifierGuard {
    /// No tsconfig, or one with neither `baseUrl`, `paths`, nor `extends`:
    /// nothing can remap a bare specifier, so the check runs unconditionally.
    Unrestricted,
    /// An explicit `paths` map with no `baseUrl`/`extends` — exclude any
    /// specifier matching one of these patterns (each is the literal
    /// `paths` key, e.g. `"@app/*"`; see [`tsconfig_pattern_matches`]).
    ExceptPatterns(Vec<String>),
    /// A `baseUrl`, an `extends`, or an unparseable tsconfig — non-relative
    /// resolution is too permissive to safely guess at from specifier text
    /// alone, so the hermetic bare-specifier check is skipped entirely for
    /// this package (the disk-based check still applies whenever
    /// `node_modules` happens to be present).
    Disabled,
}

impl BareSpecifierGuard {
    fn allows(&self, specifier: &str) -> bool {
        match self {
            BareSpecifierGuard::Unrestricted => true,
            BareSpecifierGuard::ExceptPatterns(patterns) => !patterns
                .iter()
                .any(|p| tsconfig_pattern_matches(p, specifier)),
            BareSpecifierGuard::Disabled => false,
        }
    }
}

/// A tsconfig `paths` key matches a specifier the same way TypeScript's own
/// (at most one `*`) pattern matching does: a literal key matches exactly;
/// a key containing `*` matches any specifier sharing its prefix and suffix
/// around the star.
fn tsconfig_pattern_matches(pattern: &str, specifier: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == specifier,
        Some((prefix, suffix)) => {
            specifier.len() >= prefix.len() + suffix.len()
                && specifier.starts_with(prefix)
                && specifier.ends_with(suffix)
        }
    }
}

fn bare_specifier_guard(tsconfig: Option<&Path>) -> BareSpecifierGuard {
    let Some(tsconfig) = tsconfig else {
        return BareSpecifierGuard::Unrestricted;
    };
    let Ok(mut text) = std::fs::read_to_string(tsconfig) else {
        return BareSpecifierGuard::Disabled;
    };
    // tsconfig.json is conventionally JSONC (comments, trailing commas); a
    // plain `serde_json` parse would otherwise choke on a real-world file
    // and force `Disabled` far more often than necessary.
    if json_strip_comments::strip(&mut text).is_err() {
        return BareSpecifierGuard::Disabled;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return BareSpecifierGuard::Disabled;
    };
    let compiler_options = value.get("compilerOptions");
    if value.get("extends").is_some() {
        return BareSpecifierGuard::Disabled;
    }
    if compiler_options
        .and_then(|c| c.get("baseUrl"))
        .is_some_and(|v| v.is_string())
    {
        return BareSpecifierGuard::Disabled;
    }
    let Some(paths) = compiler_options
        .and_then(|c| c.get("paths"))
        .and_then(|p| p.as_object())
    else {
        return BareSpecifierGuard::Unrestricted;
    };
    BareSpecifierGuard::ExceptPatterns(paths.keys().cloned().collect())
}

/// Walk up from `pkg_dir` (inclusive) to `workspace_root` looking for the
/// nearest `tsconfig.json` — used to configure `Resolvers`' `paths`/
/// `baseUrl`/`extends` support. `None` if no workspace directory on that
/// ancestor chain has one.
pub fn find_nearest_tsconfig(workspace_root: &Path, pkg_dir: &Path) -> Option<PathBuf> {
    let mut dir = pkg_dir;
    loop {
        let candidate = dir.join("tsconfig.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir == workspace_root {
            return None;
        }
        match dir.parent() {
            Some(parent) if parent.starts_with(workspace_root) || parent == workspace_root => {
                dir = parent;
            }
            _ => return None,
        }
    }
}

/// Build the import graph for every first-party source file directly owned
/// by `pkg_dir` (workspace-root-relative path `pkg`), bounded by nested
/// `package.json` boundaries (a subdirectory with its own `package.json` is a
/// separate package, walked separately by its own `Provider::get`).
///
/// `tsconfig` is the same nearest-tsconfig path (if any) the caller already
/// resolved via [`find_nearest_tsconfig`] to build `resolvers` — reused here
/// to compute a [`BareSpecifierGuard`] for the hermetic bare-specifier check
/// (module docs' "Hermeticity" section).
pub fn build_package_import_graph(
    walker: &CachedWalker,
    workspace_root: &Path,
    pkg: &str,
    resolvers: &Resolvers,
    cache: &ResolveCache,
    tsconfig: Option<&Path>,
) -> anyhow::Result<ImportGraph> {
    let pkg_dir = if pkg.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(pkg)
    };

    let mut files = Vec::new();
    collect_source_files(walker, &pkg_dir, true, &mut files)?;

    let guard = bare_specifier_guard(tsconfig);
    let mut graph = ImportGraph::default();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let parsed: ParsedImports = importparse::parse_file_imports(&file, &text)
            .with_context(|| format!("parsing imports in {}", file.display()))?;
        graph.unresolved_dynamic_imports += parsed.unresolved_dynamic_imports;

        let file_rel = file
            .strip_prefix(workspace_root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        let dir = file.parent().unwrap_or(&file);

        for site in parsed.sites {
            if site.type_only {
                let outcome =
                    cache.get_or_resolve(resolvers, CacheFlavor::Types, &file, &site.specifier);
                match outcome {
                    ResolveOutcome::Resolved(resolved) => {
                        graph.type_edges.push(ResolvedEdge {
                            file: file_rel.clone(),
                            specifier: site.specifier,
                            resolved,
                        });
                    }
                    ResolveOutcome::Builtin => {}
                    ResolveOutcome::Unresolved => push_if_bare(
                        &mut graph.unresolved_bare_specifiers,
                        &guard,
                        &file_rel,
                        site.specifier,
                    ),
                }
                continue;
            }
            let cache_flavor = match site.context {
                ModuleContext::Esm => CacheFlavor::RuntimeEsm,
                ModuleContext::Cjs => CacheFlavor::RuntimeCjs,
            };
            let outcome = cache.get_or_resolve(resolvers, cache_flavor, dir, &site.specifier);
            match outcome {
                ResolveOutcome::Resolved(resolved) => {
                    graph.runtime_edges.push(ResolvedEdge {
                        file: file_rel.clone(),
                        specifier: site.specifier,
                        resolved,
                    });
                }
                ResolveOutcome::Builtin => {}
                ResolveOutcome::Unresolved => push_if_bare(
                    &mut graph.unresolved_bare_specifiers,
                    &guard,
                    &file_rel,
                    site.specifier,
                ),
            }
        }
    }

    Ok(graph)
}

/// If `specifier` names an npm package by its own text ([`bare_specifier_package_name`])
/// and `guard` doesn't rule it out as a possible tsconfig alias, record it as
/// a site to hermetically cross-check against the declared closure — see
/// module docs' "Hermeticity" section.
fn push_if_bare(
    out: &mut Vec<BareSpecifierSite>,
    guard: &BareSpecifierGuard,
    file_rel: &str,
    specifier: String,
) {
    if !guard.allows(&specifier) {
        return;
    }
    if let Some(package_name) = bare_specifier_package_name(&specifier) {
        out.push(BareSpecifierSite {
            file: file_rel.to_string(),
            specifier,
            package_name,
        });
    }
}

/// Recursively collect `SOURCE_EXTENSIONS` files under `dir`, stopping at any
/// nested `package.json` boundary (unless `is_root`, since `dir` itself is
/// always expected to have one — the package this graph is being built for).
fn collect_source_files(
    walker: &CachedWalker,
    dir: &Path,
    is_root: bool,
    out: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    let listing = walker
        .read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?;

    if !is_root
        && listing
            .entries
            .iter()
            .any(|e| e.kind == EntryKind::File && e.name == PACKAGE_JSON)
    {
        return Ok(());
    }

    for entry in &listing.entries {
        let path = dir.join(&entry.name);
        match entry.kind {
            EntryKind::Dir => {
                if is_skipped_dir_name(&entry.name) {
                    continue;
                }
                collect_source_files(walker, &path, false, out)?;
            }
            EntryKind::File => {
                if let Some(ext) = Path::new(&entry.name).extension().and_then(|e| e.to_str())
                    && SOURCE_EXTENSIONS.contains(&ext)
                {
                    out.push(path);
                }
            }
            // Symlinks and other special entries are neither a package
            // boundary nor a source file this walk can read as one — same
            // "not a directory, not a plain file" treatment `collect_js_packages`
            // gives them implicitly by only branching on `Dir`/`File`.
            EntryKind::Symlink | EntryKind::Other => {}
        }
    }
    Ok(())
}

/// If `resolved` landed inside a `node_modules/` tree, the third-party
/// package name it belongs to (handling scoped `@scope/name` packages) —
/// found from the *last* `node_modules/` path segment, so a package's own
/// private nested dependency (`node_modules/a/node_modules/b`) is correctly
/// attributed to `b`, not `a`.
fn thirdparty_pkg_name_from_path(resolved: &Path) -> Option<String> {
    let s = resolved.to_str()?;
    let (_, rest) = s.rsplit_once("node_modules/")?;
    let mut parts = rest.splitn(3, '/');
    let first = parts.next()?;
    if first.starts_with('@') {
        let second = parts.next()?;
        Some(format!("{first}/{second}"))
    } else {
        Some(first.to_string())
    }
}

/// If `resolved` is a first-party path (not under any `node_modules/`), the
/// workspace-relative directory of the nearest ancestor `package.json` that
/// owns it — `None` if it escaped `workspace_root` entirely (shouldn't
/// happen for a first-party resolution, but resolution is not proof of
/// that).
fn firstparty_owning_pkg_dir(resolved: &Path, workspace_root: &Path) -> Option<PathBuf> {
    let mut dir = if resolved.is_dir() {
        resolved
    } else {
        resolved.parent()?
    };
    loop {
        if dir.join(PACKAGE_JSON).is_file() {
            return dir.strip_prefix(workspace_root).ok().map(Path::to_path_buf);
        }
        if dir == workspace_root {
            return None;
        }
        dir = dir.parent()?;
        if !dir.starts_with(workspace_root) {
            return None;
        }
    }
}

/// Cross-check every edge in `graph` against `declared` (see
/// [`declared_closure`]). Hard errors, naming the file, specifier, and
/// undeclared package, on the first violation found — this is the
/// phantom-dependency detector itself.
pub fn check_phantom_dependencies(
    workspace_root: &Path,
    pkg: &str,
    graph: &ImportGraph,
    declared: &HashSet<String>,
) -> anyhow::Result<()> {
    // Canonicalize once: `edge.resolved` has already been realpath'd by
    // `oxc_resolver` (Node itself follows symlinks by default, see
    // `resolvers.rs`'s `symlinks: true`), so comparing it against a
    // non-canonicalized `workspace_root` silently breaks the first-party
    // containment check whenever any ancestor of the workspace is itself a
    // symlink (e.g. macOS's `/tmp` -> `/private/tmp`) — not merely a
    // test-fixture quirk: on an affected host this would make
    // `firstparty_owning_pkg_dir` return `None` for every resolved edge,
    // silently turning the phantom-dependency check into a no-op instead of
    // failing loudly.
    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {}", workspace_root.display()))?;
    for edge in graph.runtime_edges.iter().chain(graph.type_edges.iter()) {
        check_one_edge(&canonical_root, pkg, edge, declared)?;
    }
    for site in &graph.unresolved_bare_specifiers {
        anyhow::ensure!(
            declared.contains(&site.package_name),
            "{pkg:?}: {file:?} imports `{specifier}`, which names the third-party package \
             `{name}` — but `{name}` is not declared in {pkg:?}'s package.json \
             (`dependencies`/`devDependencies`/`peerDependencies`). This specifier did not \
             resolve on disk (no `node_modules` installed yet?), but its own text already names \
             the package it refers to, so this is checked regardless of local install state: \
             declare it explicitly or the build is not hermetic across hosts.",
            file = site.file,
            specifier = site.specifier,
            name = site.package_name,
        );
    }
    Ok(())
}

fn check_one_edge(
    workspace_root: &Path,
    pkg: &str,
    edge: &ResolvedEdge,
    declared: &HashSet<String>,
) -> anyhow::Result<()> {
    if let Some(name) = thirdparty_pkg_name_from_path(&edge.resolved) {
        anyhow::ensure!(
            declared.contains(&name),
            "{pkg:?}: {file:?} imports `{specifier}`, which resolves to the third-party \
             package `{name}` — but `{name}` is not declared in {pkg:?}'s package.json \
             (`dependencies`/`devDependencies`). This is a phantom dependency: it may only work \
             today because another package's install hoisted `{name}` into reach; declare it \
             explicitly or the build is not hermetic across package managers/layouts.",
            file = edge.file,
            specifier = edge.specifier,
        );
        return Ok(());
    }

    if let Some(owning_pkg_dir) = firstparty_owning_pkg_dir(&edge.resolved, workspace_root) {
        let owning_pkg = owning_pkg_dir.to_string_lossy().replace('\\', "/");
        if owning_pkg != pkg {
            let owning_package_json = workspace_root.join(&owning_pkg_dir).join(PACKAGE_JSON);
            let owning_name = crate::pluginjs::workspace::read_package_name(&owning_package_json)
                .with_context(|| {
                format!(
                    "reading package name of {owning_pkg:?} (owner of {} resolved from \
                             {pkg:?}'s {:?})",
                    edge.resolved.display(),
                    edge.file
                )
            })?;
            anyhow::ensure!(
                declared.contains(&owning_name),
                "{pkg:?}: {file:?} imports `{specifier}`, which resolves into workspace package \
                 `{owning_pkg}` (package name `{owning_name}`) — but `{owning_name}` is not \
                 declared in {pkg:?}'s package.json (`dependencies`/`devDependencies`). This is a \
                 phantom dependency: declare it explicitly, or the import graph and the declared \
                 dependency graph disagree about what {pkg:?} actually depends on.",
                file = edge.file,
                specifier = edge.specifier,
            );
        }
        return Ok(());
    }

    // Resolved, but neither under any `node_modules/` nor inside
    // `workspace_root` at all — `firstparty_owning_pkg_dir`'s own doc admits
    // this "shouldn't happen for a first-party resolution, but resolution is
    // not proof of that" (e.g. a `NODE_PATH`-style escape, or a symlink that
    // leads outside the workspace). Treating this as a silent pass would let
    // a real phantom dependency reached this way sail through unchecked on
    // any host where it happens to resolve — fail closed instead, per this
    // crate's own "fail or fix, never ignore" rule.
    anyhow::bail!(
        "{pkg:?}: {file:?} imports `{specifier}`, which resolved to {resolved:?} — a path this \
         phantom-dependency check cannot classify as either a third-party `node_modules` \
         package or a first-party workspace package (it resolved outside `node_modules` and \
         outside the workspace root entirely). Treating an unclassifiable resolution as \
         compliant would silently defeat this hermeticity check; declare what {pkg:?} actually \
         depends on, or investigate why this specifier resolves outside the workspace.",
        file = edge.file,
        specifier = edge.specifier,
        resolved = edge.resolved,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pluginjs::resolvers::Resolvers;
    use hwalk::CachedWalker;
    use std::fs;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write fixture file");
    }

    fn manifest(name: &str, deps: &[&str], dev_deps: &[&str]) -> PackageManifest {
        manifest_with_peers(name, deps, dev_deps, &[])
    }

    fn manifest_with_peers(
        name: &str,
        deps: &[&str],
        dev_deps: &[&str],
        peer_deps: &[&str],
    ) -> PackageManifest {
        PackageManifest {
            name: name.to_string(),
            dependencies: deps
                .iter()
                .map(|d| (d.to_string(), "*".to_string()))
                .collect(),
            dev_dependencies: dev_deps
                .iter()
                .map(|d| (d.to_string(), "*".to_string()))
                .collect(),
            optional_dependencies: Default::default(),
            peer_dependencies: peer_deps
                .iter()
                .map(|d| (d.to_string(), "*".to_string()))
                .collect(),
        }
    }

    fn walker() -> CachedWalker {
        CachedWalker::disabled()
    }

    // ---- collect_source_files / build_package_import_graph plumbing ----

    #[test]
    fn find_nearest_tsconfig_walks_up_to_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "tsconfig.json", "{}");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        let found =
            find_nearest_tsconfig(dir.path(), &dir.path().join("packages/a")).expect("found");
        assert_eq!(found, dir.path().join("tsconfig.json"));
    }

    #[test]
    fn find_nearest_tsconfig_prefers_closest() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "tsconfig.json", "{}");
        write(dir.path(), "packages/a/tsconfig.json", "{}");
        let found =
            find_nearest_tsconfig(dir.path(), &dir.path().join("packages/a")).expect("found");
        assert_eq!(found, dir.path().join("packages/a/tsconfig.json"));
    }

    #[test]
    fn find_nearest_tsconfig_none_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        assert!(find_nearest_tsconfig(dir.path(), &dir.path().join("packages/a")).is_none());
    }

    /// A file importing a properly-declared workspace sibling: passes.
    #[test]
    fn declared_workspace_sibling_import_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name":"a","dependencies":{"b":"workspace:*"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import { x } from '../../b/src/index';\n",
        );
        write(dir.path(), "packages/b/package.json", r#"{"name":"b"}"#);
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export const x = 1;\n",
        );

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph");
        assert_eq!(graph.runtime_edges.len(), 1);

        let declared = declared_closure(&manifest("a", &["b"], &[]));
        check_phantom_dependencies(dir.path(), "packages/a", &graph, &declared)
            .expect("declared sibling import must pass");
    }

    /// A file importing an UNDECLARED package present only via
    /// hoisting/phantom resolution (simulated: present in node_modules but
    /// never declared in package.json) must hard-fail, naming the file,
    /// specifier, and package.
    #[test]
    fn phantom_thirdparty_import_hard_fails_naming_file_specifier_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import _ from 'lodash';\n",
        );
        // Present on disk (as a hoisted npm install would leave it) but never
        // declared by packages/a's package.json.
        write(
            dir.path(),
            "node_modules/lodash/package.json",
            r#"{"name":"lodash","main":"index.js"}"#,
        );
        write(
            dir.path(),
            "node_modules/lodash/index.js",
            "module.exports = {};\n",
        );

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph");
        assert_eq!(graph.runtime_edges.len(), 1);

        let declared = declared_closure(&manifest("a", &[], &[]));
        let err = check_phantom_dependencies(dir.path(), "packages/a", &graph, &declared)
            .expect_err("undeclared phantom dependency must hard-fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("packages/a/src/index.ts"), "{msg}");
        assert!(msg.contains("lodash"), "{msg}");
    }

    /// A workspace sibling import that IS resolvable on disk but never
    /// declared is exactly as much a phantom dependency as a third-party one.
    #[test]
    fn phantom_workspace_sibling_import_hard_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import { x } from '../../b/src/index';\n",
        );
        write(dir.path(), "packages/b/package.json", r#"{"name":"b"}"#);
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export const x = 1;\n",
        );

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph");

        // `b` is never declared this time.
        let declared = declared_closure(&manifest("a", &[], &[]));
        let err = check_phantom_dependencies(dir.path(), "packages/a", &graph, &declared)
            .expect_err("undeclared workspace sibling import must hard-fail");
        let msg = format!("{err:#}");
        assert!(msg.contains('b'), "{msg}");
    }

    /// A type-only import lands in the type graph but not the runtime graph.
    #[test]
    fn type_only_import_lands_only_in_type_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import type { Foo } from '../../b/src/index';\n",
        );
        write(dir.path(), "packages/b/package.json", r#"{"name":"b"}"#);
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export type Foo = number;\n",
        );

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph");

        assert!(
            graph.runtime_edges.is_empty(),
            "a type-only import must not appear in the runtime graph: {:?}",
            graph.runtime_edges
        );
        assert_eq!(graph.type_edges.len(), 1);
    }

    /// A dynamic `import()` with a non-literal argument is coarsened (see
    /// `importparse.rs`): counted, not resolved, and does not fail the build.
    #[test]
    fn dynamic_import_with_non_literal_argument_is_coarsened_not_a_hard_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export function load(lang: string) { return import(`./locales/${lang}.js`); }\n",
        );

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph must not fail on a non-literal dynamic import");

        assert!(graph.runtime_edges.is_empty());
        assert_eq!(graph.unresolved_dynamic_imports, 1);

        let declared = declared_closure(&manifest("a", &[], &[]));
        check_phantom_dependencies(dir.path(), "packages/a", &graph, &declared)
            .expect("no edge means nothing to phantom-check");
    }

    /// The walk must not cross into a nested package's own directory.
    #[test]
    fn walk_stops_at_nested_package_json_boundary() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/vendored/package.json",
            r#"{"name":"vendored"}"#,
        );
        write(
            dir.path(),
            "packages/a/vendored/index.ts",
            "import _ from 'lodash';\n",
        );

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph");
        assert!(
            graph.runtime_edges.is_empty() && graph.type_edges.is_empty(),
            "must not have parsed the nested package's own file: {graph:?}"
        );
    }

    // ---- hermeticity: bare-specifier name check, independent of ambient
    // node_modules (see module docs' "Hermeticity" section) ----

    #[test]
    fn bare_specifier_package_name_extracts_plain_and_scoped_names() {
        assert_eq!(
            bare_specifier_package_name("lodash/fp").as_deref(),
            Some("lodash")
        );
        assert_eq!(
            bare_specifier_package_name("@scope/pkg/sub").as_deref(),
            Some("@scope/pkg")
        );
        assert_eq!(
            bare_specifier_package_name("lodash").as_deref(),
            Some("lodash")
        );
    }

    #[test]
    fn bare_specifier_package_name_excludes_relative_absolute_builtin_and_subpath() {
        assert_eq!(bare_specifier_package_name("./foo"), None);
        assert_eq!(bare_specifier_package_name("../foo"), None);
        assert_eq!(bare_specifier_package_name("/abs/foo"), None);
        assert_eq!(bare_specifier_package_name("fs"), None);
        assert_eq!(bare_specifier_package_name("node:fs"), None);
        assert_eq!(bare_specifier_package_name("#internal/thing"), None);
        // Not a valid npm scope shape (empty scope name) -- a bundler/tsconfig
        // alias convention (`@/components/Foo`), never a real scoped package.
        assert_eq!(bare_specifier_package_name("@/components/Foo"), None);
    }

    /// The scenario the hermeticity/feature-quality reviews called out: a
    /// fresh checkout with **no `node_modules` anywhere** (so every
    /// third-party specifier resolves to `Unresolved`, not just this one).
    /// An undeclared bare specifier must still be caught by name — the whole
    /// point of the bare-specifier check is that it does not depend on
    /// `node_modules` having been installed.
    #[test]
    fn undeclared_bare_specifier_is_caught_even_with_no_node_modules_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import _ from 'lodash';\n",
        );
        // Deliberately no `node_modules` anywhere in `dir` -- a fresh
        // checkout that has never run an install.

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph");
        assert!(
            graph.runtime_edges.is_empty(),
            "with no node_modules on disk, the specifier must not resolve: {:?}",
            graph.runtime_edges
        );
        assert_eq!(graph.unresolved_bare_specifiers.len(), 1);
        assert_eq!(graph.unresolved_bare_specifiers[0].package_name, "lodash");

        let declared = declared_closure(&manifest("a", &[], &[]));
        let err = check_phantom_dependencies(dir.path(), "packages/a", &graph, &declared)
            .expect_err("an undeclared bare specifier must hard-fail even with no node_modules");
        let msg = format!("{err:#}");
        assert!(msg.contains("packages/a/src/index.ts"), "{msg}");
        assert!(msg.contains("lodash"), "{msg}");
    }

    /// Same fresh-checkout, no-`node_modules` scenario, but `lodash` IS
    /// declared this time -- must pass.
    #[test]
    fn declared_bare_specifier_passes_even_with_no_node_modules_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import _ from 'lodash';\n",
        );

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph");

        let declared = declared_closure(&manifest("a", &["lodash"], &[]));
        check_phantom_dependencies(dir.path(), "packages/a", &graph, &declared)
            .expect("a declared bare specifier must pass even with no node_modules on disk");
    }

    /// A `peerDependencies` entry counts as declared for phantom-check
    /// purposes (a peer dep is a legitimate, common thing to import) even
    /// though it is never wired as a target dependency by `deps.rs`.
    #[test]
    fn peer_dependency_counts_as_declared_for_phantom_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "import React from 'react';\n",
        );

        let resolvers = Resolvers::new(None);
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            None,
        )
        .expect("build graph");

        let declared = declared_closure(&manifest_with_peers("a", &[], &[], &["react"]));
        check_phantom_dependencies(dir.path(), "packages/a", &graph, &declared)
            .expect("a peerDependencies entry must count as declared");
    }

    /// A tsconfig path alias (`"@app/*": ["src/*"]`) that fails to resolve
    /// (e.g. a typo or a moved file) must NOT be misreported as an
    /// undeclared third-party package named after the alias -- that would be
    /// exactly the over-detection that blocks a correct build.
    #[test]
    fn unresolvable_tsconfig_path_alias_is_not_a_false_positive_phantom() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "tsconfig.json",
            r#"{
                "compilerOptions": { "paths": { "@app/*": ["src/*"] } }
            }"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            // `@app/does-not-exist` matches the `@app/*` pattern but has no
            // real target -- unresolvable, but must not be reported as an
            // undeclared `@app/does-not-exist` npm dependency.
            "import x from '@app/does-not-exist';\n",
        );

        let resolvers = Resolvers::new(Some(&dir.path().join("tsconfig.json")));
        let cache = ResolveCache::new();
        let graph = build_package_import_graph(
            &walker(),
            dir.path(),
            "packages/a",
            &resolvers,
            &cache,
            Some(&dir.path().join("tsconfig.json")),
        )
        .expect("build graph");
        assert!(
            graph.unresolved_bare_specifiers.is_empty(),
            "a tsconfig path-alias-shaped specifier must not be treated as a bare npm \
             specifier: {:?}",
            graph.unresolved_bare_specifiers
        );

        let declared = declared_closure(&manifest("a", &[], &[]));
        check_phantom_dependencies(dir.path(), "packages/a", &graph, &declared)
            .expect("an unresolvable tsconfig path alias must not false-positive as phantom");
    }

    /// A resolved edge that lands neither under `node_modules/` nor inside
    /// the workspace root at all (e.g. a `NODE_PATH`-style escape, or a
    /// symlink leading outside the workspace) must hard-fail rather than be
    /// silently treated as compliant -- `firstparty_owning_pkg_dir`'s own doc
    /// admits this "shouldn't happen ... but resolution is not proof of
    /// that".
    #[test]
    fn unclassifiable_resolved_edge_is_a_hard_error_not_a_silent_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        let canonical_root = dir.path().canonicalize().expect("canonicalize");

        // Neither under any `node_modules/` component nor inside the
        // workspace root at all.
        let escaped = std::env::temp_dir().join("definitely-outside-the-workspace.ts");
        let edge = ResolvedEdge {
            file: "packages/a/src/index.ts".to_string(),
            specifier: "escaped".to_string(),
            resolved: escaped,
        };

        let declared = declared_closure(&manifest("a", &[], &[]));
        let err = check_one_edge(&canonical_root, "packages/a", &edge, &declared)
            .expect_err("an unclassifiable resolved edge must hard-fail, not silently pass");
        let msg = format!("{err:#}");
        assert!(msg.contains("packages/a/src/index.ts"), "{msg}");
        assert!(msg.contains("escaped"), "{msg}");
    }
}
