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
use crate::pluginjs::lockfile::{self, Lockfile, ResolvedGraph};
use crate::pluginjs::package_json::PackageManifest;
use crate::pluginjs::resolvers::{ResolveOutcome, Resolvers};
use crate::pluginjs::{PACKAGE_JSON, is_skipped_dir_name};
use anyhow::Context;
use hwalk::{CachedWalker, EntryKind};
use oxc_allocator::Allocator;
use oxc_ast::ast::{ArrayExpressionElement, Expression, PropertyKey};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use wax::{Glob, Program as _};

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

/// [`declared_closure`], widened to every package name reachable by walking
/// the lockfile's resolved dependency graph outward from `manifest`'s own
/// direct dependencies.
///
/// A package pulled in this way — e.g. `@eslint/js`, reachable because
/// `typescript-eslint` (a real, declared `devDependency`) depends on it — is
/// not a phantom dependency in the sense [`check_phantom_dependencies`]
/// exists to catch: its presence is fully determined by the lockfile, which
/// is itself an already-hashed input, not by workspace-wide `node_modules`
/// hoisting that could place a *different* version (or nothing at all) in
/// reach on another host or package manager. Only names that aren't
/// reachable from *anything* this manifest declares still count as phantom.
///
/// `lockfile`/`resolved_graph` are `None` for a package with no lockfile
/// entry yet (nothing installed) — falls back to [`declared_closure`]
/// unchanged, same as before this widening existed.
pub fn transitive_declared_closure(
    manifest: &PackageManifest,
    pkg: &str,
    lockfile: Option<&Lockfile>,
    resolved_graph: Option<&ResolvedGraph>,
    os: &str,
    arch: &str,
) -> HashSet<String> {
    let mut set = declared_closure(manifest);
    let (Some(lockfile), Some(resolved_graph)) = (lockfile, resolved_graph) else {
        return set;
    };
    // Same seed computation `deps::resolve_one_dependency`'s
    // `lockfile::resolve_transitive` fallback uses, so a package this check
    // accepts is always one that path can also wire an Input for — see
    // `resolve_transitive`'s doc for why that agreement matters. A seed
    // resolution error here just means one fewer package widens the closure
    // (never a silent-wrong-build either way: a name that stays unwidened
    // still fails loudly, as a phantom dependency, at this function's own
    // caller — `Lockfile::resolve_dependency` has no fallible path today,
    // so this can't actually happen yet regardless).
    let seed_keys = lockfile::direct_dep_seed_keys(lockfile, pkg, manifest).unwrap_or_default();
    set.extend(
        resolved_graph
            .transitive_reachable(seed_keys, os, arch)
            .into_keys(),
    );
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

/// Read and parse a tsconfig file as JSONC (comments, trailing commas —
/// conventional for real-world `tsconfig.json` files; a plain `serde_json`
/// parse would otherwise choke and force callers into an overly conservative
/// fallback far more often than necessary). Shared by [`bare_specifier_guard`],
/// [`read_tsconfig_fields`], and [`resolve_tsconfig_extends_chain`] so all
/// three agree on exactly what counts as "this tsconfig's own JSON".
fn read_tsconfig_jsonc(path: &Path) -> anyhow::Result<serde_json::Value> {
    let mut text = std::fs::read_to_string(path)
        .with_context(|| format!("reading tsconfig {}", path.display()))?;
    json_strip_comments::strip(&mut text)
        .map_err(|e| anyhow::anyhow!("stripping comments from {}: {e}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing tsconfig {}", path.display()))
}

fn bare_specifier_guard(tsconfig: Option<&Path>) -> BareSpecifierGuard {
    let Some(tsconfig) = tsconfig else {
        return BareSpecifierGuard::Unrestricted;
    };
    let Ok(value) = read_tsconfig_jsonc(tsconfig) else {
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

/// Walk up from `pkg_dir` (inclusive) to `workspace_root`, returning the
/// first directory's `candidates` entry (checked in order at each level)
/// that exists as a file. Shared walk-up logic behind [`find_nearest_tsconfig`]
/// and [`find_nearest_test_runner_config`] — `js_test`'s runner config
/// (`vitest.config.ts` / `jest.config.js`) is walked up the same way a
/// package's tsconfig is, per `ai-docs/js-plugin-plan.md`'s `js_test` milestone
/// note. Also reused by `Provider::find_lockfile_root` for lockfile discovery
/// — same walk-up-by-presence shape, one candidate.
pub(crate) fn find_nearest_file(
    workspace_root: &Path,
    pkg_dir: &Path,
    candidates: &[&str],
) -> Option<PathBuf> {
    let mut dir = pkg_dir;
    loop {
        for name in candidates {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
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

/// Walk up from `pkg_dir` (inclusive) to `workspace_root` looking for the
/// nearest `tsconfig.json` — used to configure `Resolvers`' `paths`/
/// `baseUrl`/`extends` support. `None` if no workspace directory on that
/// ancestor chain has one.
pub fn find_nearest_tsconfig(workspace_root: &Path, pkg_dir: &Path) -> Option<PathBuf> {
    find_nearest_file(workspace_root, pkg_dir, &["tsconfig.json"])
}

/// Walk up from `pkg_dir` (inclusive) to `workspace_root` looking for the
/// nearest test-runner config file among `candidates` (e.g.
/// `["vitest.config.ts", "vitest.config.js", ...]`) — the same ancestor walk
/// [`find_nearest_tsconfig`] performs, generalized to whatever filenames the
/// configured `testrunner` uses. `None` if no workspace directory on that
/// ancestor chain has any of them.
pub fn find_nearest_test_runner_config(
    workspace_root: &Path,
    pkg_dir: &Path,
    candidates: &[&str],
) -> Option<PathBuf> {
    find_nearest_file(workspace_root, pkg_dir, candidates)
}

/// Same ancestor walk, generalized once more to `js_lint`'s linter config
/// files (e.g. `[".oxlintrc.json"]` for oxlint, the flat/legacy `eslint.
/// config.*`/`.eslintrc.*` candidate list for eslint) — see `driver_lint.rs`
/// module docs.
pub fn find_nearest_lint_config(
    workspace_root: &Path,
    pkg_dir: &Path,
    candidates: &[&str],
) -> Option<PathBuf> {
    find_nearest_file(workspace_root, pkg_dir, candidates)
}

/// Same ancestor walk, generalized once more to `js_bundle`'s bundler config
/// file — see `driver_bundle.rs` module docs. Only `esbuild.config.json` is
/// recognized in this milestone (the esbuild CLI has no auto-discovered
/// config file convention of its own; a JS/TS esbuild config driven through
/// its Node API is out of scope for this CLI-based driver — a disclosed gap,
/// not a silently missing feature).
pub fn find_nearest_bundler_config(
    workspace_root: &Path,
    pkg_dir: &Path,
    candidates: &[&str],
) -> Option<PathBuf> {
    find_nearest_file(workspace_root, pkg_dir, candidates)
}

/// Walk up from `pkg_dir` (inclusive) to `workspace_root` looking for the
/// nearest `package.json` whose own `"jest"` field is present — jest's other
/// documented config location, alongside the dedicated `jest.config.*`
/// filenames [`find_nearest_test_runner_config`] already checks. Called by
/// `test_deps_config` only once none of those dedicated filenames are found
/// on the same ancestor chain, matching jest's own precedence (a dedicated
/// config file wins over the shared `package.json`). `None` if no ancestor's
/// `package.json` carries a `"jest"` field (or fails to parse as JSON, or
/// doesn't exist) — a hermeticity M4 review finding: this fallback was
/// previously entirely absent, so a project configured this way had its real
/// config invisible to the declared Input set and the cache key.
pub fn find_nearest_jest_package_json_config(
    workspace_root: &Path,
    pkg_dir: &Path,
) -> Option<PathBuf> {
    find_nearest_package_json_field_config(workspace_root, pkg_dir, "jest")
}

/// **Deliberately no `js_lint` analog of this fallback for oxlint/eslint**:
/// an earlier version of this module also walked up for a `package.json`
/// `"oxlint"`/`"eslintConfig"` field the same way, on the theory that it
/// mirrored jest's own `package.json`-field config location above. It didn't
/// — a feature-quality M5 review caught that neither tool actually reads a
/// `package.json` this way when invoked with `-c <that package.json>`:
/// oxlint's documented auto-discovery is exactly `.oxlintrc.json` /
/// `.oxlintrc.jsonc` / `oxlint.config.{ts,mts}`, nothing about a
/// `package.json` field; eslint's `--config`/`-c` flag only accepts a real
/// `eslint.config.*`/`.eslintrc.*` file; ESLint's real legacy
/// `"eslintConfig"` field is picked up only by its own automatic cascading
/// discovery when `--config` is *not* passed, which `js_lint`'s `-c`-always
/// invocation shape (`driver_lint.rs::run`) never leaves room for. Passing
/// `-c <package.json>` handed either tool a file shape it doesn't parse as
/// config, so the fallback was removed rather than kept and made to somehow
/// work — jest's own `package.json`-field fallback below is unaffected
/// (jest's test-runner CLI has no equivalent single-`-c`-flag constraint).
///
/// Shared ancestor walk behind [`find_nearest_jest_package_json_config`]:
/// walk up from `pkg_dir` (inclusive) to `workspace_root` for the nearest
/// `package.json` that carries a top-level `field` key.
fn find_nearest_package_json_field_config(
    workspace_root: &Path,
    pkg_dir: &Path,
    field: &str,
) -> Option<PathBuf> {
    let mut dir = pkg_dir;
    loop {
        let candidate = dir.join(PACKAGE_JSON);
        if candidate.is_file()
            && let Ok(text) = std::fs::read_to_string(&candidate)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && value.get(field).is_some()
        {
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

/// Test-runner config keys whose value names one or more first-party files
/// the real runner reads at test time, beyond the config file's own leaf
/// bytes — `setupFiles`/`globalSetup`/`globalTeardown` (jest and vitest both
/// support these); `setupFilesAfterEnv` is jest-only, harmless to also look
/// for under vitest. See [`extract_runner_config_referenced_paths`] for the
/// extraction itself, and this crate's M4 hermeticity-review note for why
/// this exists: a `setupFiles` entry mutates real test behavior (mocks,
/// globals) without touching the test file, its import closure, or the
/// config's own leaf bytes' *meaning* to a casual diff reader — it must
/// still bust the cache.
///
/// **Known scope trim, disclosed rather than silent**: `moduleNameMapper`
/// (jest) / `resolve.alias` (vitest) mock-path values and a custom
/// `testEnvironment` module are not extracted — the former's values commonly
/// carry regex backreferences (`$1`) this static scan can't resolve without
/// evaluating the mapping, and the latter is usually a package name, not a
/// path. TODO M4+.
const RUNNER_CONFIG_FILE_KEYS: &[&str] = &[
    "setupFiles",
    "setupFilesAfterEnv",
    "globalSetup",
    "globalTeardown",
];

/// Statically extract every string-literal value named by
/// [`RUNNER_CONFIG_FILE_KEYS`] anywhere in `content` — a single string
/// (`setupFiles: './setup.ts'`) or an array of strings (`setupFiles:
/// ['./setup.ts']`). Deliberately not scoped to "the exported config
/// object" specifically: this crate doesn't evaluate JS, so rather than
/// trying to precisely locate the one object a `defineConfig(...)`/
/// `module.exports = ...` call wraps (which can nest arbitrarily), it scans
/// every object literal in the file. Over-inclusion (a same-named key that
/// happens to appear elsewhere) costs one extra declared Input;
/// under-inclusion is the actual hermeticity bug this exists to close, so
/// the asymmetry is deliberate.
///
/// A config file that fails to parse (invalid syntax, or an extension this
/// parser doesn't recognize) yields an empty list rather than an error — the
/// leaf config's own raw bytes are already declared/hashed regardless of
/// this scan's success (`test_deps_config`), so a parse failure here only
/// means the extra references go undetected, not that the whole `js_test`
/// target becomes unbuildable.
fn extract_runner_config_referenced_paths(path: &Path, content: &str) -> Vec<String> {
    let Ok(source_type) = SourceType::from_path(path) else {
        return Vec::new();
    };
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, content, source_type).parse();
    if ret.panicked {
        return Vec::new();
    }
    let mut visitor = RunnerConfigRefVisitor::default();
    visitor.visit_program(&ret.program);
    visitor.paths
}

#[derive(Default)]
struct RunnerConfigRefVisitor {
    paths: Vec<String>,
}

impl RunnerConfigRefVisitor {
    fn push_value(&mut self, value: &Expression<'_>) {
        match value {
            Expression::StringLiteral(s) => self.paths.push(s.value.as_str().to_string()),
            Expression::ArrayExpression(arr) => {
                for el in &arr.elements {
                    if let ArrayExpressionElement::StringLiteral(s) = el {
                        self.paths.push(s.value.as_str().to_string());
                    }
                }
            }
            _ => {}
        }
    }
}

impl<'a> Visit<'a> for RunnerConfigRefVisitor {
    fn visit_object_property(&mut self, it: &oxc_ast::ast::ObjectProperty<'a>) {
        let key_name = match &it.key {
            PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
            PropertyKey::StringLiteral(s) => Some(s.value.as_str()),
            _ => None,
        };
        if key_name.is_some_and(|name| RUNNER_CONFIG_FILE_KEYS.contains(&name)) {
            self.push_value(&it.value);
        }
        walk::walk_object_property(self, it);
    }
}

/// Probe `candidate_no_ext` against Node/esbuild's common extension set for
/// an extensionless specifier, then its own `index.*` if it names a
/// directory — the same shape [`resolvers::Resolvers`] handles for real
/// import resolution, kept separate and deliberately simpler here (no
/// `package.json` `exports` map, no `tsconfig` `paths`) since a runner
/// config's own referenced files are always plain relative paths in
/// practice, never package-style specifiers.
fn probe_first_party_path(candidate_no_ext: &Path) -> Option<PathBuf> {
    const EXTS: &[&str] = &["", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".json"];
    for ext in EXTS {
        let candidate = if ext.is_empty() {
            candidate_no_ext.to_path_buf()
        } else {
            let mut s = candidate_no_ext.as_os_str().to_os_string();
            s.push(ext);
            PathBuf::from(s)
        };
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for index in &[
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
        "index.mjs",
        "index.cjs",
    ] {
        let candidate = candidate_no_ext.join(index);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve one `RUNNER_CONFIG_FILE_KEYS` value (`raw`) against `config_dir` —
/// handling jest's `<rootDir>/...` token (approximated as `config_dir`,
/// jest's own default when no explicit `rootDir` override exists) as well as
/// a plain relative path. `None` for anything else (a bare module specifier
/// naming a third-party setup package — not this scan's job; see
/// [`RUNNER_CONFIG_FILE_KEYS`]'s doc).
fn resolve_config_value_path(config_dir: &Path, raw: &str) -> Option<PathBuf> {
    if let Some(rest) = raw.strip_prefix("<rootDir>") {
        return probe_first_party_path(&config_dir.join(rest.trim_start_matches('/')));
    }
    if raw.starts_with("./") || raw.starts_with("../") {
        return probe_first_party_path(&config_dir.join(raw));
    }
    None
}

/// Resolve one relative `import`/`require` specifier found *inside* a config
/// file against that file's own directory. Deliberately only ever a relative
/// specifier (`./...`/`../...`) — a bare specifier (`import { defineConfig }
/// from 'vitest/config'`) names the config's own npm dependency, which is
/// `js_install`'s concern, not this one-hop config-file scan's.
fn resolve_config_import_specifier(config_dir: &Path, specifier: &str) -> Option<PathBuf> {
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return None;
    }
    probe_first_party_path(&config_dir.join(specifier))
}

/// [`resolve_runner_config_referenced_files`]'s result: every additional
/// first-party file the config transitively names/imports, plus every bare
/// (third-party) specifier encountered along the way.
#[derive(Debug)]
pub struct RunnerConfigScan {
    pub files: Vec<PathBuf>,
    /// A bare import/require inside the config file (e.g. `import react
    /// from '@vitejs/plugin-react'`) — not followed as a file (it names a
    /// real npm dependency, not another config to recurse into), but
    /// collected here for the same reason [`build_test_closure`]'s own
    /// `bare_specifiers` are: a plugin the config itself imports (Vite's
    /// `@vitejs/plugin-react`, `vite-plugin-svgr`, Lingui's
    /// `@lingui/vite-plugin`, a browser-mode provider like
    /// `@vitest/browser-playwright`, …) has to be staged in the sandbox and
    /// declared as an Input the same as any other third-party dependency
    /// the test actually needs, or resolving/loading the config itself
    /// fails the moment vitest/jest tries to import it — regardless of
    /// whether the test file's own source ever touches it.
    pub bare_specifiers: Vec<BareSpecifierSite>,
}

/// Recursively resolve every additional first-party file a test-runner
/// config's own content names or imports: [`RUNNER_CONFIG_FILE_KEYS`]
/// entries, plus a relative `import`/`require` of a shared base config
/// (`import base from '../../vitest.config.base'`) — which may itself name
/// or import more, so each newly-resolved file is scanned the same way in
/// turn. Bounded depth + a visited set guard against a cyclic/self-importing
/// config; in practice a real config chain is one or two files deep. See
/// [`RunnerConfigScan::bare_specifiers`] for the config's own third-party
/// imports, which this also collects but does not recurse into.
pub fn resolve_runner_config_referenced_files(
    config_path: &Path,
    config_content: &str,
) -> anyhow::Result<RunnerConfigScan> {
    const MAX_DEPTH: usize = 4;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    let mut bare_specifiers: Vec<BareSpecifierSite> = Vec::new();
    let mut queue: VecDeque<(PathBuf, String, usize)> = VecDeque::new();
    seen.insert(config_path.to_path_buf());
    queue.push_back((config_path.to_path_buf(), config_content.to_string(), 0));

    while let Some((path, content, depth)) = queue.pop_front() {
        if depth >= MAX_DEPTH {
            continue;
        }
        let dir = path.parent().unwrap_or(Path::new(""));

        for raw in extract_runner_config_referenced_paths(&path, &content) {
            if let Some(resolved) = resolve_config_value_path(dir, &raw) {
                enqueue_referenced_config_file(resolved, depth, &mut seen, &mut found, &mut queue);
            }
        }

        // A config file with an unrecognized extension (e.g. `.json`, which
        // `importparse::parse_file_imports` can't parse as a module) simply
        // yields no import sites here — not an error, since `.json` config
        // files (jest's `jest.config.json`) never `import` anything anyway.
        if let Ok(imports) = importparse::parse_file_imports(&path, &content) {
            for site in imports.sites {
                if let Some(resolved) = resolve_config_import_specifier(dir, &site.specifier) {
                    enqueue_referenced_config_file(
                        resolved, depth, &mut seen, &mut found, &mut queue,
                    );
                } else if let Some(package_name) = bare_specifier_package_name(&site.specifier) {
                    bare_specifiers.push(BareSpecifierSite {
                        file: path.to_string_lossy().replace('\\', "/"),
                        specifier: site.specifier,
                        package_name,
                    });
                }
            }
        }
    }

    Ok(RunnerConfigScan {
        files: found.into_iter().collect(),
        bare_specifiers,
    })
}

/// Record a newly-resolved referenced-config file (if not already seen) and
/// queue it for its own scan one depth deeper — the shared "insert once,
/// re-read, re-enqueue" step [`resolve_runner_config_referenced_files`]'s two
/// call sites (config-value paths, config-file imports) both need.
fn enqueue_referenced_config_file(
    resolved: PathBuf,
    depth: usize,
    seen: &mut HashSet<PathBuf>,
    found: &mut BTreeSet<PathBuf>,
    queue: &mut VecDeque<(PathBuf, String, usize)>,
) {
    if seen.insert(resolved.clone()) {
        found.insert(resolved.clone());
        if let Ok(next_content) = std::fs::read_to_string(&resolved) {
            queue.push_back((resolved, next_content, depth + 1));
        }
    }
}

/// Object property key naming `@typescript-eslint/parser`'s `project`
/// option — present (under `parserOptions` in a legacy `.eslintrc.*`, or
/// nested under `languageOptions.parserOptions` in a modern flat config)
/// exactly when an eslint config turns on type-aware ("type-checked") rules.
/// Scanned the same broadly-over-precisely way [`RUNNER_CONFIG_FILE_KEYS`] is
/// (see that constant's doc for the rationale): a same-named key elsewhere in
/// the file costs one extra declared/hashed Input (the tsconfig this function
/// doesn't actually need), while missing a real one would silently drop the
/// tsconfig extends chain from `js_lint`'s cache key for an eslint config
/// that *does* type-check — see `driver_lint.rs` module docs for why this
/// gap matters (an M3/M4-review-class mistake, called out again for this
/// driver in the M5 task).
const ESLINT_PROJECT_KEY: &str = "project";

/// One `parserOptions.project` value found by [`detect_eslint_type_aware`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EslintProjectOption {
    /// `project: true` (or any value that isn't a string/array of strings,
    /// e.g. a computed expression this static scan can't evaluate) —
    /// `@typescript-eslint/parser`'s own documented "figure out the nearest
    /// tsconfig yourself" shorthand. Carries no explicit path; the caller
    /// falls back to [`find_nearest_tsconfig`].
    AutoDetect,
    /// One or more explicit tsconfig paths (relative to the config file's own
    /// directory) — a bare string, or an array of strings
    /// (`typescript-eslint` accepts either).
    Paths(Vec<String>),
}

/// Whether `config_path`'s content configures eslint type-aware rules (one or
/// more `parserOptions.project` entries — see [`ESLINT_PROJECT_KEY`]'s doc),
/// and if so, which tsconfig(s) each names. An empty `Vec` means no `project`
/// key was found anywhere in the file — not type-aware, so no
/// tsconfig/extends-chain Input is needed for this `js_lint` target.
///
/// **Every** occurrence in the file is collected, not just the first: a flat
/// config commonly has more than one — e.g. separate override blocks for
/// `src/**` and `test/**`, each with its own `parserOptions.project` naming a
/// different tsconfig (`tsconfig.json` vs `tsconfig.test.json`). Stopping at
/// the first match (an earlier version of this function did) silently
/// dropped every tsconfig named by a later entry from the declared Input
/// set/cache key — a code-quality M5 review finding, the exact
/// declared-Input-vs-real-tool-read mismatch class this crate's M3/M4
/// reviews already caught twice for other drivers.
///
/// JSON/YAML (`.eslintrc.json`/`.yml`/`.yaml`, or the extensionless
/// `.eslintrc`, itself JSON per eslint's own convention) is parsed as data
/// and walked recursively for every `"project"` key at any depth. A JS/TS/mjs/cjs
/// config (modern flat `eslint.config.*`, or a legacy `.eslintrc.js`) is
/// parsed with `oxc_parser` and scanned for every object property named
/// `project`, mirroring [`RunnerConfigRefVisitor`]'s shape. Either parse
/// failing yields `Ok(vec![])` — the leaf config's own raw bytes are already a
/// declared/hashed Input regardless (`lint_deps_config`), so a parse failure
/// here only means a real type-aware config goes undetected, not that the
/// whole `js_lint` target becomes unbuildable.
pub fn detect_eslint_type_aware(
    config_path: &Path,
    content: &str,
) -> anyhow::Result<Vec<EslintProjectOption>> {
    let ext = config_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "json" | "yml" | "yaml" | "" => {
            let value: serde_json::Value = if ext == "yml" || ext == "yaml" {
                match serde_yaml::from_str(content) {
                    Ok(v) => v,
                    Err(_) => return Ok(Vec::new()),
                }
            } else {
                match serde_json::from_str(content) {
                    Ok(v) => v,
                    Err(_) => return Ok(Vec::new()),
                }
            };
            let mut found = Vec::new();
            collect_project_keys(&value, &mut found);
            Ok(found)
        }
        _ => {
            let Ok(source_type) = SourceType::from_path(config_path) else {
                return Ok(Vec::new());
            };
            let allocator = Allocator::default();
            let ret = Parser::new(&allocator, content, source_type).parse();
            if ret.panicked {
                return Ok(Vec::new());
            }
            let mut visitor = EslintProjectVisitor::default();
            visitor.visit_program(&ret.program);
            Ok(visitor.found)
        }
    }
}

/// Recursively collect every `"project"` key found in a parsed JSON/YAML
/// config value, at any depth — see [`detect_eslint_type_aware`]'s doc for
/// why this is intentionally not scoped to exactly `parserOptions.project`,
/// and for why every occurrence (not just the first) is collected.
fn collect_project_keys(value: &serde_json::Value, out: &mut Vec<EslintProjectOption>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(v) = map.get(ESLINT_PROJECT_KEY) {
                out.push(json_value_to_project_option(v));
            }
            for v in map.values() {
                collect_project_keys(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_project_keys(item, out);
            }
        }
        _ => {}
    }
}

fn json_value_to_project_option(v: &serde_json::Value) -> EslintProjectOption {
    match v {
        serde_json::Value::String(s) => EslintProjectOption::Paths(vec![s.clone()]),
        serde_json::Value::Array(items) => {
            let paths: Vec<String> = items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect();
            if paths.is_empty() {
                EslintProjectOption::AutoDetect
            } else {
                EslintProjectOption::Paths(paths)
            }
        }
        _ => EslintProjectOption::AutoDetect,
    }
}

/// Scans a parsed JS/TS eslint config's AST for every object property named
/// [`ESLINT_PROJECT_KEY`] — the flat-config/legacy-JS-config counterpart to
/// [`collect_project_keys`]'s JSON/YAML walk. Mirrors [`RunnerConfigRefVisitor`]'s
/// shape (see that type's doc). Collects **every** occurrence, not just the
/// first — see [`detect_eslint_type_aware`]'s doc for why a multi-entry flat
/// config (separate `parserOptions.project` per override block) needs all of
/// them.
#[derive(Default)]
struct EslintProjectVisitor {
    found: Vec<EslintProjectOption>,
}

impl EslintProjectVisitor {
    fn note(&mut self, value: &Expression<'_>) {
        let option = match value {
            Expression::StringLiteral(s) => {
                EslintProjectOption::Paths(vec![s.value.as_str().to_string()])
            }
            Expression::ArrayExpression(arr) => {
                let paths: Vec<String> = arr
                    .elements
                    .iter()
                    .filter_map(|el| match el {
                        ArrayExpressionElement::StringLiteral(s) => {
                            Some(s.value.as_str().to_string())
                        }
                        _ => None,
                    })
                    .collect();
                if paths.is_empty() {
                    EslintProjectOption::AutoDetect
                } else {
                    EslintProjectOption::Paths(paths)
                }
            }
            _ => EslintProjectOption::AutoDetect,
        };
        self.found.push(option);
    }
}

impl<'a> Visit<'a> for EslintProjectVisitor {
    fn visit_object_property(&mut self, it: &oxc_ast::ast::ObjectProperty<'a>) {
        let key_name = match &it.key {
            PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
            PropertyKey::StringLiteral(s) => Some(s.value.as_str()),
            _ => None,
        };
        if key_name == Some(ESLINT_PROJECT_KEY) {
            self.note(&it.value);
        }
        walk::walk_object_property(self, it);
    }
}

/// Which `package.json` naming convention applies to a legacy eslint config's
/// `extends`/`plugins` string entry — see [`eslint_module_name`]'s doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EslintRefKind {
    Extends,
    Plugin,
}

const ESLINT_REF_KEYS: &[&str] = &["extends", "plugins"];

/// Extract every npm package name a dedicated eslint config file's
/// `extends`/`plugins` (legacy `.eslintrc.*`) or top-level bare
/// `import`/`require` (modern flat `eslint.config.*`) references. See
/// `driver_lint.rs`'s "Inputs / cache key" section for why these must be
/// resolved via the lockfile (`deps::resolve_one_dependency`), never treated
/// as raw filesystem paths — the exact M3/M4-review-class mistake named
/// again for this driver.
///
/// The two config shapes are **not** interchangeable, and are dispatched by
/// `config_path`'s own basename, not merely its extension (both can be a
/// plain `.js` file): legacy config's `extends`/`plugins` are string/
/// array-of-string values naming a package by eslint's own documented
/// shorthand convention (`"airbnb"` → `eslint-config-airbnb`, `"react"` in
/// `plugins` → `eslint-plugin-react`) — see [`eslint_module_name`], applied
/// here. Modern flat config has no `extends` key at all (a shared config is
/// spread into the exported array, e.g. `...tseslint.configs.recommended`)
/// and its own `plugins` is an *object* mapping a plugin key to an
/// already-`import`-ed module, not a string array — so for a flat config
/// (`eslint.config.*`, detected by filename), this instead collects the
/// file's own bare `import`/`require` specifiers verbatim: no naming-guess
/// needed, since in that shape the specifier already *is* the real package
/// name (`import reactHooks from 'eslint-plugin-react-hooks'`).
///
/// A relative-path `extends`/`plugins` entry (`"extends":
/// "./base.eslintrc.json"`) names a local sibling config file, not an npm
/// package — filtered out of this function's own result (below) and instead
/// resolved as a declared first-party Input by
/// [`resolve_eslint_config_referenced_files`], which also follows a modern
/// flat config's own relative `import`/`require` of a shared base config.
/// See that function's doc — this is `lint_deps_config`'s actual mechanism
/// for both shapes; a hermeticity M5 review caught this doc previously
/// claiming that mechanism already ran over the leaf config when it did not.
pub fn extract_eslint_module_refs(
    config_path: &Path,
    content: &str,
) -> anyhow::Result<Vec<String>> {
    let basename = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if basename.starts_with("eslint.config") {
        return extract_flat_config_bare_imports(config_path, content);
    }

    let ext = config_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let raw_values: Vec<(&'static str, String)> = match ext {
        "json" | "yml" | "yaml" | "" => {
            let value: serde_json::Value = if ext == "yml" || ext == "yaml" {
                match serde_yaml::from_str(content) {
                    Ok(v) => v,
                    Err(_) => return Ok(Vec::new()),
                }
            } else {
                match serde_json::from_str(content) {
                    Ok(v) => v,
                    Err(_) => return Ok(Vec::new()),
                }
            };
            let mut out = Vec::new();
            collect_eslint_ref_values(&value, &mut out);
            out
        }
        _ => {
            let Ok(source_type) = SourceType::from_path(config_path) else {
                return Ok(Vec::new());
            };
            let allocator = Allocator::default();
            let ret = Parser::new(&allocator, content, source_type).parse();
            if ret.panicked {
                return Ok(Vec::new());
            }
            let mut visitor = EslintRefVisitor::default();
            visitor.visit_program(&ret.program);
            visitor.values
        }
    };

    Ok(raw_values
        .into_iter()
        .filter_map(|(key, raw)| {
            if raw.starts_with('.') || Path::new(&raw).is_absolute() {
                return None;
            }
            if raw == "eslint:recommended" || raw == "eslint:all" {
                return None;
            }
            let kind = if key == "plugins" {
                EslintRefKind::Plugin
            } else {
                EslintRefKind::Extends
            };
            Some(eslint_module_name(&raw, kind))
        })
        .collect())
}

/// Recursively resolve every additional first-party file an eslint config's
/// own content references beyond its own leaf bytes — the file-based
/// counterpart to [`extract_eslint_module_refs`]'s npm-package resolution.
/// Two shapes, both followed:
///
/// - A modern flat config's own relative `import`/`require` of a shared base
///   config (`import base from './eslint-base.js'`) — reuses
///   [`resolve_runner_config_referenced_files`]'s generic relative-import
///   walk (recursive: a base config's own further relative imports are
///   followed too).
/// - A legacy config's relative `extends`/`plugins` string value
///   (`"extends": "./base.eslintrc.json"`) — names a local sibling file, not
///   an npm package (that half is [`extract_eslint_module_refs`]'s job), and
///   is followed here the same recursive way, bounded by `MAX_DEPTH` and a
///   `seen` set against a cyclic/self-extending config.
///
/// This closes the M5 hermeticity gap named in this crate's review history:
/// editing only a shared base config (relative `extends`, or a flat config's
/// relative import) previously left `js_lint`'s declared Input set/cache key
/// unchanged, serving a stale cached result even though a fresh run of the
/// real linter would pick up the edit. See `lint_deps_config`'s doc for how
/// the result is declared/hashed.
pub fn resolve_eslint_config_referenced_files(
    config_path: &Path,
    config_content: &str,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
    seen.insert(config_path.to_path_buf());

    // Modern flat config's own relative `import`/`require` chain (recursive
    // — `resolve_runner_config_referenced_files` already walks depth).
    // `bare_specifiers` is unused here: `lint_deps_config`'s dedicated
    // `eslint_plugins` field already resolves the *leaf* config's own
    // `extends`/`plugins` values through the lockfile, covering the common
    // case. It does not currently re-scan a relatively-imported *base*
    // config's own plugin imports (`extract_eslint_module_refs` only ever
    // runs on the leaf), so a plugin imported only by a shared base config
    // is a real, pre-existing gap this call doesn't close either — unlike
    // `test_deps_config`'s equivalent runner-config case, which does
    // (`resolve_runner_config_referenced_files`'s own `bare_specifiers`
    // there gets validated and staged). Left as a known gap rather than
    // silently claimed as covered; fixing it means threading
    // `bare_specifiers` through the same declared-dependency check/staging
    // `test_deps_config` now has, for every file in the chain, not just
    // this one's leaf.
    for f in resolve_runner_config_referenced_files(config_path, config_content)?.files {
        if seen.insert(f.clone()) {
            found.insert(f);
        }
    }

    // Legacy config's relative `extends`/`plugins` string values, followed
    // recursively (a base config can itself `extend` another).
    const MAX_DEPTH: usize = 4;
    let mut queue: VecDeque<(PathBuf, String, usize)> = VecDeque::new();
    queue.push_back((config_path.to_path_buf(), config_content.to_string(), 0));
    while let Some((path, content, depth)) = queue.pop_front() {
        if depth >= MAX_DEPTH {
            continue;
        }
        let dir = path.parent().unwrap_or(Path::new(""));
        for raw in extract_eslint_relative_ref_values(&path, &content) {
            let Some(resolved) = probe_first_party_path(&dir.join(&raw)) else {
                continue;
            };
            if seen.insert(resolved.clone()) {
                found.insert(resolved.clone());
                if let Ok(next_content) = std::fs::read_to_string(&resolved) {
                    queue.push_back((resolved, next_content, depth + 1));
                }
            }
        }
    }

    Ok(found.into_iter().collect())
}

/// The legacy-config half of [`resolve_eslint_config_referenced_files`]:
/// every raw `extends`/`plugins` value that looks like a relative filesystem
/// path (starts with `.`), unfiltered otherwise — mirrors
/// [`extract_eslint_module_refs`]'s own raw-value extraction (via the same
/// [`collect_eslint_ref_values`]/[`EslintRefVisitor`]), just keeping the
/// opposite half of that function's `raw.starts_with('.')` filter. Flat
/// configs (`eslint.config.*`) have no `extends`/`plugins` string key at all
/// (see `extract_eslint_module_refs`'s doc) — the relative-import chain
/// [`resolve_runner_config_referenced_files`] already walks is that shape's
/// equivalent, so this returns nothing for a flat config's own basename.
fn extract_eslint_relative_ref_values(config_path: &Path, content: &str) -> Vec<String> {
    let basename = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if basename.starts_with("eslint.config") {
        return Vec::new();
    }

    let ext = config_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let raw_values: Vec<(&'static str, String)> = match ext {
        "json" | "yml" | "yaml" | "" => {
            let value: serde_json::Value = if ext == "yml" || ext == "yaml" {
                match serde_yaml::from_str(content) {
                    Ok(v) => v,
                    Err(_) => return Vec::new(),
                }
            } else {
                match serde_json::from_str(content) {
                    Ok(v) => v,
                    Err(_) => return Vec::new(),
                }
            };
            let mut out = Vec::new();
            collect_eslint_ref_values(&value, &mut out);
            out
        }
        _ => {
            let Ok(source_type) = SourceType::from_path(config_path) else {
                return Vec::new();
            };
            let allocator = Allocator::default();
            let ret = Parser::new(&allocator, content, source_type).parse();
            if ret.panicked {
                return Vec::new();
            }
            let mut visitor = EslintRefVisitor::default();
            visitor.visit_program(&ret.program);
            visitor.values
        }
    };

    raw_values
        .into_iter()
        .filter_map(|(_key, raw)| raw.starts_with('.').then_some(raw))
        .collect()
}

/// The flat-config half of [`extract_eslint_module_refs`]: every bare (not
/// relative, not absolute, not a Node builtin) `import`/`require` specifier
/// the config file's own top level names, verbatim — see that function's
/// doc for why no naming-convention guess is needed here. Reuses
/// [`bare_specifier_package_name`] (the same bare-specifier→package-name
/// extraction the hermetic phantom-dependency check uses) so a subpath
/// import (`import foo from 'eslint-plugin-foo/configs/recommended'`) is
/// correctly attributed to the `eslint-plugin-foo` package, not its full
/// subpath.
fn extract_flat_config_bare_imports(
    config_path: &Path,
    content: &str,
) -> anyhow::Result<Vec<String>> {
    let Ok(parsed) = importparse::parse_file_imports(config_path, content) else {
        // A parse failure here only means these references go undetected —
        // see `extract_eslint_module_refs`'s doc for the same "declared
        // leaf-config bytes are hashed regardless" reasoning
        // `detect_eslint_type_aware` already documents.
        return Ok(Vec::new());
    };
    let mut names: BTreeSet<String> = BTreeSet::new();
    for site in parsed.sites {
        if let Some(name) = bare_specifier_package_name(&site.specifier) {
            names.insert(name);
        }
    }
    Ok(names.into_iter().collect())
}

fn collect_eslint_ref_values(value: &serde_json::Value, out: &mut Vec<(&'static str, String)>) {
    match value {
        serde_json::Value::Object(map) => {
            for key in ESLINT_REF_KEYS {
                if let Some(v) = map.get(*key) {
                    push_string_or_array(key, v, out);
                }
            }
            for v in map.values() {
                collect_eslint_ref_values(v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                collect_eslint_ref_values(v, out);
            }
        }
        _ => {}
    }
}

fn push_string_or_array(
    key: &'static str,
    value: &serde_json::Value,
    out: &mut Vec<(&'static str, String)>,
) {
    match value {
        serde_json::Value::String(s) => out.push((key, s.clone())),
        serde_json::Value::Array(items) => {
            for i in items {
                if let Some(s) = i.as_str() {
                    out.push((key, s.to_string()));
                }
            }
        }
        _ => {}
    }
}

/// Scans a parsed JS/TS eslint config's AST for an object property named
/// `"extends"` or `"plugins"` — the flat-config/legacy-JS-config counterpart
/// to [`collect_eslint_ref_values`]'s JSON/YAML walk. Mirrors
/// [`RunnerConfigRefVisitor`]'s/[`EslintProjectVisitor`]'s shape.
#[derive(Default)]
struct EslintRefVisitor {
    values: Vec<(&'static str, String)>,
}

impl EslintRefVisitor {
    fn push_value(&mut self, key: &'static str, value: &Expression<'_>) {
        match value {
            Expression::StringLiteral(s) => self.values.push((key, s.value.as_str().to_string())),
            Expression::ArrayExpression(arr) => {
                for el in &arr.elements {
                    if let ArrayExpressionElement::StringLiteral(s) = el {
                        self.values.push((key, s.value.as_str().to_string()));
                    }
                }
            }
            _ => {}
        }
    }
}

impl<'a> Visit<'a> for EslintRefVisitor {
    fn visit_object_property(&mut self, it: &oxc_ast::ast::ObjectProperty<'a>) {
        let key_name = match &it.key {
            PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
            PropertyKey::StringLiteral(s) => Some(s.value.as_str()),
            _ => None,
        };
        let key = match key_name {
            Some("extends") => Some("extends"),
            Some("plugins") => Some("plugins"),
            _ => None,
        };
        if let Some(key) = key {
            self.push_value(key, &it.value);
        }
        walk::walk_object_property(self, it);
    }
}

/// Map a raw `extends`/`plugins` string (already filtered to "names an npm
/// package" by [`extract_eslint_module_refs`]) to the actual npm package
/// name, per eslint's own documented shorthand-naming convention (a
/// `"plugin:react/recommended"` `extends` entry names the `plugins` package
/// `react` refers to, resolved recursively as [`EslintRefKind::Plugin`]).
///
/// Best-effort, not exhaustive: eslint itself resolves a shorthand name via
/// `require.resolve` against a handful of candidate names in order; this
/// picks the single most common one rather than replicating that whole
/// fallback chain. A package published under a genuinely unconventional name
/// would not be found this way — `deps::resolve_one_dependency`'s own "no
/// lockfile resolution" error then names the *guessed* package, not the
/// original config string, so this is a disclosed rough edge, not a silent
/// one.
pub fn eslint_module_name(raw: &str, kind: EslintRefKind) -> String {
    if let Some(plugin_part) = raw.strip_prefix("plugin:") {
        let name = plugin_part.split('/').next().unwrap_or(plugin_part);
        return eslint_module_name(name, EslintRefKind::Plugin);
    }
    let (scoped_infix, bare_prefix) = match kind {
        EslintRefKind::Extends => ("eslint-config", "eslint-config-"),
        EslintRefKind::Plugin => ("eslint-plugin", "eslint-plugin-"),
    };
    if let Some(rest) = raw.strip_prefix('@') {
        return match rest.split_once('/') {
            None => format!("@{rest}/{scoped_infix}"),
            Some((_scope, name)) if name.starts_with(scoped_infix) => raw.to_string(),
            Some((scope, name)) => format!("@{scope}/{scoped_infix}-{name}"),
        };
    }
    if raw.starts_with(bare_prefix) {
        raw.to_string()
    } else {
        format!("{bare_prefix}{raw}")
    }
}

/// A tsconfig's own `include`/`exclude`/`files` fields (leaf-level only —
/// **not** merged across an `extends` chain; see
/// [`resolve_tsconfig_extends_chain`] for the sibling gap this leaves and why
/// it's an accepted trim, not a silent one). Used both to bound
/// `js_typecheck`'s declared first-party Input set to what `tsc` actually
/// reads (see `provider.rs`'s `typecheck_deps_config`) and, via
/// [`check_tsconfig_scope`], to verify a *shared* tsconfig actually scopes to
/// one package before trusting a per-package Input set built from it at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TsconfigFields {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub files: Vec<String>,
}

pub fn read_tsconfig_fields(path: &Path) -> anyhow::Result<TsconfigFields> {
    let value = read_tsconfig_jsonc(path)?;
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(TsconfigFields {
        include: strings("include"),
        exclude: strings("exclude"),
        files: strings("files"),
    })
}

/// Whether `tsconfig_path`'s own effective scope (its `include`/`files`
/// fields — see [`TsconfigFields`]'s doc for the extends-merge trim) can be
/// trusted to reach only files under `pkg_dir`.
///
/// A package's **own** tsconfig (found directly in `pkg_dir` by
/// [`find_nearest_tsconfig`]) is always trusted: even with no
/// `include`/`files` at all, `tsc`'s own default scope — every source file
/// under the config's directory — is bounded by `pkg_dir` itself, so it can
/// never reach outside it.
///
/// A **shared/ancestor** tsconfig (found by walking up past `pkg_dir`,
/// meaning the package has no tsconfig of its own) is different: `tsc`'s
/// default scope is bounded by the *ancestor's* directory, which may
/// legitimately contain other, unrelated packages — the classic
/// single-shared-root-tsconfig monorepo shape. A per-package `js_typecheck`
/// Input set built only from this package's own files would be unsound
/// there — see `ai-docs/js-plugin-plan.md`'s "Correctness safety valve". So a
/// shared tsconfig is only trusted when it declares its own `include`/
/// `files`, **and** every entry's literal (non-wildcard) prefix is confined
/// to `pkg_dir` or a subdirectory of it (checked textually — proving a
/// pattern *can't* reach outside the package without needing a full
/// workspace walk here). Anything else (no `include`/`files` at all, or an
/// entry reaching outside `pkg_dir`) is rejected with an actionable error
/// rather than silently assumed safe.
pub fn check_tsconfig_scope(
    tsconfig_path: &Path,
    tsconfig_dir: &Path,
    pkg_dir: &Path,
    fields: &TsconfigFields,
) -> anyhow::Result<()> {
    if tsconfig_dir == pkg_dir {
        return Ok(());
    }
    let pkg_rel = pkg_dir
        .strip_prefix(tsconfig_dir)
        .unwrap_or(pkg_dir)
        .to_string_lossy()
        .replace('\\', "/");
    let entries: Vec<&String> = fields.include.iter().chain(fields.files.iter()).collect();
    anyhow::ensure!(
        !entries.is_empty(),
        "js_typecheck: {pkg_dir:?} has no tsconfig.json of its own, and the nearest ancestor \
         one ({tsconfig_path:?}) declares no `include`/`files` — its default scope is every \
         source file under {tsconfig_dir:?}, which may reach other packages heph cannot safely \
         attribute to just {pkg_dir:?}. Add an `include`/`files` field to {tsconfig_path:?} \
         scoping it to {pkg_rel:?}, or give {pkg_dir:?} its own tsconfig.json.",
    );
    for entry in entries {
        anyhow::ensure!(
            pattern_confined_to_pkg(entry, &pkg_rel),
            "js_typecheck: {tsconfig_path:?}'s `include`/`files` entry {entry:?} is not confined \
             to {pkg_rel:?} — this shared tsconfig may cover more than one package, which \
             `js_typecheck`'s per-package Input scoping cannot safely represent. Scope \
             {tsconfig_path:?}'s `include`/`files` to {pkg_rel:?} only, or give {pkg_dir:?} its \
             own tsconfig.json.",
        );
    }
    Ok(())
}

/// Whether `pattern` (an `include`/`files` entry, relative to the tsconfig's
/// own directory) is textually confined to `pkg_rel` (the package's relative
/// path from that same directory) or a subdirectory of it. A conservative,
/// glob-*prefix* check rather than full glob evaluation — see
/// [`check_tsconfig_scope`]'s doc for why that's the right trade-off here.
fn pattern_confined_to_pkg(pattern: &str, pkg_rel: &str) -> bool {
    let pkg_rel = pkg_rel.trim_end_matches('/');
    if pkg_rel.is_empty() {
        // The package *is* the tsconfig's own directory (the root package)
        // — nothing can be "outside" it.
        return true;
    }
    let literal_prefix = pattern
        .split(['*', '?'])
        .next()
        .unwrap_or(pattern)
        .trim_end_matches('/');
    literal_prefix == pkg_rel || literal_prefix.starts_with(&format!("{pkg_rel}/"))
}

/// Filter `files` (absolute paths) down to the ones matching `fields`'
/// `include`/`files`/`exclude`, interpreted relative to `tsconfig_dir` the
/// same way `tsc` itself resolves them. `include` and `files` both empty
/// means "no restriction" (`tsc`'s own default: every file under the
/// tsconfig's directory, modulo `exclude`) — `files` is always included
/// verbatim (an explicit file list is never subject to `exclude`, matching
/// `tsc`'s own semantics).
pub fn filter_by_tsconfig_fields(
    files: Vec<PathBuf>,
    tsconfig_dir: &Path,
    fields: &TsconfigFields,
) -> anyhow::Result<Vec<PathBuf>> {
    if fields.include.is_empty() && fields.files.is_empty() && fields.exclude.is_empty() {
        return Ok(files);
    }
    let include_globs: Vec<Glob<'_>> = fields
        .include
        .iter()
        .map(|p| Glob::new(p).with_context(|| format!("invalid tsconfig include glob {p:?}")))
        .collect::<anyhow::Result<_>>()?;
    let exclude_globs: Vec<Glob<'_>> = fields
        .exclude
        .iter()
        .map(|p| Glob::new(p).with_context(|| format!("invalid tsconfig exclude glob {p:?}")))
        .collect::<anyhow::Result<_>>()?;
    let explicit_files: HashSet<PathBuf> =
        fields.files.iter().map(|f| tsconfig_dir.join(f)).collect();
    // No `include`/`files` at all: `tsc`'s own default is "every file under
    // the tsconfig's directory" — i.e. unrestricted here too, modulo
    // `exclude` below.
    let unrestricted_include = fields.include.is_empty() && fields.files.is_empty();
    Ok(files
        .into_iter()
        .filter(|f| {
            if explicit_files.contains(f) {
                return true;
            }
            let rel = f.strip_prefix(tsconfig_dir).unwrap_or(f);
            (unrestricted_include || include_globs.iter().any(|g| g.is_match(rel)))
                && !exclude_globs.iter().any(|g| g.is_match(rel))
        })
        .collect())
}

/// Resolve `leaf`'s `extends` chain (JSONC-aware; each ancestor's own
/// `extends` is followed too, cycle-guarded), returning every ancestor
/// config file the chain reaches — nearest first, **excluding** `leaf`
/// itself. Declared as additional `"tsconfig"` `js_typecheck` Inputs and
/// folded into its content hash (see `provider.rs`'s `typecheck_deps_config`):
/// `tsc --project` merges every ancestor's `compilerOptions` into the
/// effective program, so a change to any of them must bust the cache the
/// same way a change to the leaf itself does.
///
/// A relative `extends` entry (`"./foo"`, `"../bar.json"`) is resolved
/// against the referencing config's own directory, same as `tsc` itself
/// (trying the path as given, then with a `.json` extension appended). A
/// bare package-name entry (TypeScript's shareable-config convention, e.g.
/// `"@org/tsconfig-base"`) is resolved by walking up `node_modules`
/// directories from the referencing config towards `workspace_root`,
/// matching Node's own package-resolution walk — this reintroduces the same
/// ambient-`node_modules` dependency `typecheck_deps_config`'s third-party
/// handling already has for package imports (see that function's doc), not a
/// new one.
///
/// An entry that cannot be resolved at all fails the whole call rather than
/// being silently skipped: `tsc` cannot run at all without it, so pretending
/// the Input set is complete while omitting a real config file `tsc` needs
/// would be the exact silent-cache-poisoning failure mode this milestone is
/// scoped to avoid.
///
/// TS 5.0's `extends` array (multiple base configs layered in order) is
/// followed for declaring/hashing purposes (every entry is resolved and
/// included), but only the *first* entry is recursed into for its own
/// further `extends` — see the inline comment at that branch for why.
pub fn resolve_tsconfig_extends_chain(
    workspace_root: &Path,
    leaf: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    let canonical_root = workspace_root
        .canonicalize()
        .with_context(|| format!("canonicalize workspace root {}", workspace_root.display()))?;
    let mut chain = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    if let Ok(c) = leaf.canonicalize() {
        seen.insert(c);
    }
    let mut current = leaf.to_path_buf();
    loop {
        let value = read_tsconfig_jsonc(&current)?;
        let Some(extends) = value.get("extends") else {
            break;
        };
        let dir = current.parent().unwrap_or(&current).to_path_buf();
        let next = match extends {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(items) => {
                // Every entry is a real config file `tsc` merges in, so every
                // one is declared/hashed — but only the first is walked
                // further up for its *own* `extends` (matching the single
                // chain a single-string `extends` would produce; fully
                // replicating TS's multi-extends *merge order* is scoped out
                // — see this function's doc).
                for item in items.iter().skip(1) {
                    if let Some(s) = item.as_str() {
                        let resolved = resolve_extends_specifier(&canonical_root, &dir, s)?;
                        if seen.insert(resolved.clone()) {
                            chain.push(resolved);
                        }
                    }
                }
                items.first().and_then(|v| v.as_str()).map(str::to_string)
            }
            _ => None,
        };
        let Some(specifier) = next else { break };
        let resolved = resolve_extends_specifier(&canonical_root, &dir, &specifier)?;
        anyhow::ensure!(
            seen.insert(resolved.clone()),
            "tsconfig extends cycle detected while resolving {}",
            leaf.display()
        );
        chain.push(resolved.clone());
        current = resolved;
    }
    Ok(chain)
}

fn resolve_extends_specifier(
    canonical_workspace_root: &Path,
    from_dir: &Path,
    specifier: &str,
) -> anyhow::Result<PathBuf> {
    if specifier.starts_with('.') || Path::new(specifier).is_absolute() {
        let candidate = from_dir.join(specifier);
        let candidate = if candidate.extension().is_some() {
            candidate
        } else {
            candidate.with_extension("json")
        };
        anyhow::ensure!(
            candidate.is_file(),
            "tsconfig `extends: {specifier:?}` (from {}) not found at {}",
            from_dir.display(),
            candidate.display()
        );
        return canonicalize_within(canonical_workspace_root, &candidate);
    }
    // Bare package-name `extends` — resolved via `node_modules`, walking up
    // from `from_dir` towards the workspace root. See this function's doc
    // for why an unresolved entry is a hard error, not a silent skip.
    let mut dir = from_dir;
    loop {
        let base = dir.join("node_modules").join(specifier);
        for candidate in [
            base.clone(),
            base.with_extension("json"),
            base.join("tsconfig.json"),
        ] {
            if candidate.is_file() {
                return canonicalize_within(canonical_workspace_root, &candidate);
            }
        }
        if dir == canonical_workspace_root {
            break;
        }
        match dir.parent() {
            Some(parent)
                if parent.starts_with(canonical_workspace_root)
                    || parent == canonical_workspace_root =>
            {
                dir = parent;
            }
            _ => break,
        }
    }
    anyhow::bail!(
        "tsconfig `extends: {specifier:?}` (from {}) could not be resolved — no matching file \
         found under any ancestor `node_modules` up to the workspace root ({}); is \
         `node_modules` installed?",
        from_dir.display(),
        canonical_workspace_root.display()
    )
}

fn canonicalize_within(canonical_workspace_root: &Path, path: &Path) -> anyhow::Result<PathBuf> {
    let c = path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?;
    anyhow::ensure!(
        c.starts_with(canonical_workspace_root),
        "tsconfig extends resolved to {} which is outside the workspace root ({}) — cannot \
         express it as a declared js_typecheck input",
        c.display(),
        canonical_workspace_root.display()
    );
    Ok(c)
}

/// Every first-party source file (`SOURCE_EXTENSIONS`) directly owned by
/// workspace-root-relative package `pkg`, bounded by nested `package.json`
/// boundaries — the same walk [`build_package_import_graph`] performs
/// internally to seed its own edge walk. Exposed separately so a caller that
/// needs the file *list* itself (e.g. `js_typecheck`'s Input declaration —
/// see `provider.rs`'s `typecheck_deps_config`) doesn't have to re-implement
/// the walk or re-derive it from `ImportGraph`'s edges (which record only
/// files that *contain* an import, not every source file in the package).
pub fn package_source_files(
    walker: &CachedWalker,
    workspace_root: &Path,
    pkg: &str,
) -> anyhow::Result<Vec<PathBuf>> {
    let pkg_dir = if pkg.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(pkg)
    };
    let mut files = Vec::new();
    collect_source_files(walker, &pkg_dir, true, &mut files)?;
    Ok(files)
}

/// Every first-party source file directly owned by `pkg` (see
/// [`package_source_files`]) whose path, relative to the package directory,
/// matches at least one of `patterns` (wax glob syntax) — `js_test`'s
/// per-test-file target discovery
/// (`ai-docs/js-plugin-plan.md`'s `js_test` milestone: "one `js_test` target
/// per test file", matched by a configurable glob).
///
/// Returned paths are workspace-root-relative strings, sorted, so target
/// discovery is deterministic across filesystem-walk order/platform — same
/// discipline as `resolve_members`/`typecheck_deps_config`'s own sorted
/// output.
pub fn discover_test_files(
    walker: &CachedWalker,
    workspace_root: &Path,
    pkg: &str,
    patterns: &[String],
) -> anyhow::Result<Vec<String>> {
    let pkg_dir = if pkg.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(pkg)
    };
    let globs: Vec<Glob<'_>> = patterns
        .iter()
        .map(|p| Glob::new(p).with_context(|| format!("invalid js_test test_glob {p:?}")))
        .collect::<anyhow::Result<_>>()?;

    let files = package_source_files(walker, workspace_root, pkg)?;
    let mut matched: Vec<String> = files
        .into_iter()
        .filter_map(|f| {
            let rel = f.strip_prefix(&pkg_dir).unwrap_or(&f);
            if globs.iter().any(|g| g.is_match(rel)) {
                Some(
                    f.strip_prefix(workspace_root)
                        .unwrap_or(&f)
                        .to_string_lossy()
                        .replace('\\', "/"),
                )
            } else {
                None
            }
        })
        .collect();
    matched.sort();
    Ok(matched)
}

/// One test file's own runtime-transitive first-party closure within its
/// owning package, plus everything it reaches just outside that boundary —
/// see [`build_test_closure`].
#[derive(Debug, Clone, Default)]
pub struct TestClosure {
    /// Workspace-relative paths of every first-party file, *within the same
    /// package*, transitively reachable from the test file via
    /// `ImportGraph::runtime_edges` — always includes the test file itself.
    /// This is the per-test-file (not per-package) declared `Input` set that
    /// is this milestone's stated differentiator over Turborepo/Nx (see
    /// `ai-docs/js-plugin-plan.md`'s "Caching / incrementality" section).
    pub files: BTreeSet<String>,
    /// Workspace-relative paths of every file a closure member's own edge
    /// resolved *outside* the owning package (a workspace sibling, or a
    /// third-party package reached while `node_modules` happens to be
    /// installed) — recorded but **not** recursed into further, the same
    /// "one-hop" trim `js_typecheck`'s type-edge handling already accepts
    /// (see `driver_typecheck.rs` module docs' "Known scope trims"). TODO
    /// M4+: recurse into a workspace sibling's own import graph once
    /// cross-package graph construction exists.
    pub external_files: BTreeSet<String>,
    /// Bare specifiers that never resolved on disk, restricted to sites
    /// inside `files` — the ambient-`node_modules`-absent counterpart to
    /// `external_files`, resolved by package name via
    /// `deps::resolve_one_dependency` at the call site (see `provider.rs`'s
    /// `test_deps_config`), mirroring `typecheck_deps_config`'s identical
    /// on-demand third-party handling.
    pub bare_specifiers: Vec<BareSpecifierSite>,
}

/// BFS `test_file_rel`'s own `ImportGraph::runtime_edges` closure, bounded to
/// files first-party-owned by `pkg` (`canonical_workspace_root.join(pkg)`) —
/// see [`TestClosure`]'s doc for exactly what counts as "in" vs. "one-hop
/// external". `graph` must be `pkg`'s own whole-package import graph (as
/// built by [`build_package_import_graph`]) — this never re-parses source
/// itself, only walks the edges already resolved for `pkg`.
///
/// `canonical_workspace_root` must already be canonicalized (`edge.resolved`
/// is realpath'd by `oxc_resolver` — see `check_phantom_dependencies`'s
/// identical canonicalization requirement and its doc for why comparing
/// against a non-canonical root silently breaks containment on a host where
/// an ancestor of the workspace is itself a symlink).
pub fn build_test_closure(
    graph: &ImportGraph,
    canonical_workspace_root: &Path,
    pkg: &str,
    test_file_rel: &str,
) -> anyhow::Result<TestClosure> {
    let pkg_dir = if pkg.is_empty() {
        canonical_workspace_root.to_path_buf()
    } else {
        canonical_workspace_root.join(pkg)
    };

    let mut edges_by_file: HashMap<&str, Vec<&ResolvedEdge>> = HashMap::new();
    for edge in &graph.runtime_edges {
        edges_by_file
            .entry(edge.file.as_str())
            .or_default()
            .push(edge);
    }
    let mut bare_by_file: HashMap<&str, Vec<&BareSpecifierSite>> = HashMap::new();
    for site in &graph.unresolved_bare_specifiers {
        bare_by_file
            .entry(site.file.as_str())
            .or_default()
            .push(site);
    }

    let mut closure = TestClosure::default();
    closure.files.insert(test_file_rel.to_string());
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(test_file_rel.to_string());

    while let Some(current) = queue.pop_front() {
        if let Some(sites) = bare_by_file.get(current.as_str()) {
            for site in sites {
                closure.bare_specifiers.push((*site).clone());
            }
        }
        let Some(edges) = edges_by_file.get(current.as_str()) else {
            continue;
        };
        for edge in edges {
            // `starts_with(&pkg_dir)` alone isn't enough: a package that has
            // a real, ambient `node_modules` physically nested inside its
            // own directory (a common on-disk shape — dedup/version-conflict
            // nesting, or a stray local install) resolves a third-party
            // import to a path that's *inside* `pkg_dir` too. That must
            // still classify as third-party — `Provider::test_deps_config`
            // sends `external_files` through `classify_resolved_edge`
            // (lockfile-driven `resolve_one_dependency`, landing on the
            // relocated `js/node_modules:group` addr) but takes `files`
            // straight to a raw `fs:file` Input with no such check; letting
            // a `node_modules`-nested-but-physically-inside-pkg_dir edge
            // through as `files` means both the raw on-disk file *and* the
            // relocated group target end up declaring the exact same
            // sandbox path, an output collision at run time.
            if edge.resolved.starts_with(&pkg_dir)
                && thirdparty_pkg_name_from_path(&edge.resolved).is_none()
            {
                let rel = edge
                    .resolved
                    .strip_prefix(canonical_workspace_root)
                    .with_context(|| {
                        format!(
                            "js_test: {:?} resolved to {:?}, inside the package directory but \
                             outside the canonicalized workspace root ({:?}) — cannot express it \
                             as a declared js_test input",
                            edge.file, edge.resolved, canonical_workspace_root
                        )
                    })?
                    .to_string_lossy()
                    .replace('\\', "/");
                if closure.files.insert(rel.clone()) {
                    queue.push_back(rel);
                }
            } else {
                let rel = edge
                    .resolved
                    .strip_prefix(canonical_workspace_root)
                    .with_context(|| {
                        format!(
                            "js_test: {:?} imports from {:?}, which resolved outside the \
                             workspace root ({:?}) — cannot express it as a declared js_test \
                             input (this typically means node_modules is a symlink to a global \
                             store outside the workspace)",
                            edge.file, edge.resolved, canonical_workspace_root
                        )
                    })?
                    .to_string_lossy()
                    .replace('\\', "/");
                closure.external_files.insert(rel);
            }
        }
    }

    Ok(closure)
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
    let files = package_source_files(walker, workspace_root, pkg)?;

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
pub(crate) fn thirdparty_pkg_name_from_path(resolved: &Path) -> Option<String> {
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
///
/// `pub(crate)`, not private: `provider.rs`'s `js_bundle` cross-package
/// closure walk (`Provider::bundle_closure`) reuses this to find which
/// sibling package a first-party edge crossed into, so it knows which
/// package's own `ImportGraph` to fetch next — the exact "which package owns
/// this resolved path" question [`check_phantom_dependencies`] already
/// answers for a one-hop edge.
pub(crate) fn firstparty_owning_pkg_dir(resolved: &Path, workspace_root: &Path) -> Option<PathBuf> {
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
            main: None,
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

    /// A minimal npm `package-lock.json` for `transitive_declared_closure`'s
    /// own tests: package `a` declares `typescript-eslint`, which the
    /// lockfile resolves to depend on `@eslint/js` — the exact
    /// companion-package pattern (a real ESLint flat-config monorepo hitting
    /// `array_exports_matched_entry_missing_on_disk_hard_fails_no_fallback`'s
    /// sibling bug report) this closure exists to stop flagging as phantom.
    /// `unrelated` has no edge from anything `a` declares, so it stays a
    /// genuine phantom even after the widening.
    fn transitive_fixture() -> Lockfile {
        Lockfile::parse(
            crate::pluginjs::workspace::PkgManager::Npm,
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "packages/a": { "name": "a", "devDependencies": { "typescript-eslint": "^8.0.0" } },
                    "node_modules/typescript-eslint": {
                        "version": "8.0.0",
                        "resolved": "https://registry.npmjs.org/typescript-eslint/-/typescript-eslint-8.0.0.tgz",
                        "integrity": "sha512-abc",
                        "dependencies": { "@eslint/js": "9.0.0" }
                    },
                    "node_modules/@eslint/js": {
                        "version": "9.0.0",
                        "resolved": "https://registry.npmjs.org/@eslint/js/-/js-9.0.0.tgz",
                        "integrity": "sha512-def",
                        "dependencies": {}
                    },
                    "node_modules/unrelated": {
                        "version": "1.0.0",
                        "resolved": "https://registry.npmjs.org/unrelated/-/unrelated-1.0.0.tgz",
                        "integrity": "sha512-ghi",
                        "dependencies": {}
                    }
                }
            }"#,
        )
        .expect("parse fixture lockfile")
    }

    #[test]
    fn transitive_declared_closure_includes_a_dependency_of_a_declared_dependency() {
        let lockfile = transitive_fixture();
        let resolved_graph = lockfile.resolved_graph().unwrap();
        let manifest = manifest("a", &[], &["typescript-eslint"]);

        // The narrower, direct-only closure does not know about `@eslint/js`
        // at all — this is the exact gap `transitive_declared_closure` fills.
        assert!(!declared_closure(&manifest).contains("@eslint/js"));

        let widened = transitive_declared_closure(
            &manifest,
            "packages/a",
            Some(&lockfile),
            Some(&resolved_graph),
            "linux",
            "amd64",
        );
        assert!(
            widened.contains("@eslint/js"),
            "a package reachable through a declared dependency's own lockfile-resolved \
             dependencies must not be flagged as phantom: {widened:?}"
        );
        assert!(widened.contains("typescript-eslint"), "{widened:?}");
    }

    #[test]
    fn transitive_declared_closure_does_not_include_an_unrelated_package() {
        let lockfile = transitive_fixture();
        let resolved_graph = lockfile.resolved_graph().unwrap();
        let manifest = manifest("a", &[], &["typescript-eslint"]);

        let widened = transitive_declared_closure(
            &manifest,
            "packages/a",
            Some(&lockfile),
            Some(&resolved_graph),
            "linux",
            "amd64",
        );
        assert!(
            !widened.contains("unrelated"),
            "a package with no edge from anything `a` declares is still a genuine phantom \
             dependency, not merely hoisted into reach: {widened:?}"
        );
    }

    #[test]
    fn transitive_declared_closure_falls_back_to_direct_only_without_a_lockfile() {
        let manifest = manifest("a", &[], &["typescript-eslint"]);
        let widened =
            transitive_declared_closure(&manifest, "packages/a", None, None, "linux", "amd64");
        assert_eq!(widened, declared_closure(&manifest));
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

    // ---- js_test: discover_test_files / find_nearest_test_runner_config / build_test_closure ----

    #[test]
    fn discover_test_files_matches_configured_globs_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        write(
            dir.path(),
            "packages/a/src/index.test.ts",
            "import { x } from './index'; test('x', () => x);\n",
        );
        write(
            dir.path(),
            "packages/a/src/other.spec.tsx",
            "test('y', () => 1);\n",
        );

        let patterns = vec![
            "**/*.test.{ts,tsx,js,jsx}".to_string(),
            "**/*.spec.{ts,tsx,js,jsx}".to_string(),
        ];
        let files =
            discover_test_files(&walker(), dir.path(), "packages/a", &patterns).expect("discover");
        assert_eq!(
            files,
            vec![
                "packages/a/src/index.test.ts".to_string(),
                "packages/a/src/other.spec.tsx".to_string(),
            ]
        );
    }

    #[test]
    fn discover_test_files_empty_when_no_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/index.ts",
            "export const x = 1;\n",
        );
        let patterns = vec!["**/*.test.{ts,tsx,js,jsx}".to_string()];
        let files =
            discover_test_files(&walker(), dir.path(), "packages/a", &patterns).expect("discover");
        assert!(files.is_empty());
    }

    #[test]
    fn find_nearest_test_runner_config_walks_up_like_tsconfig() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "vitest.config.ts", "export default {};\n");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        let found = find_nearest_test_runner_config(
            dir.path(),
            &dir.path().join("packages/a"),
            &["vitest.config.ts", "vitest.config.js"],
        )
        .expect("found");
        assert_eq!(found, dir.path().join("vitest.config.ts"));
    }

    #[test]
    fn find_nearest_test_runner_config_none_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        assert!(
            find_nearest_test_runner_config(
                dir.path(),
                &dir.path().join("packages/a"),
                &["vitest.config.ts"],
            )
            .is_none()
        );
    }

    #[test]
    fn find_nearest_jest_package_json_config_walks_up_to_the_ancestor_that_has_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name":"root","jest":{"testEnvironment":"node"}}"#,
        );
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        let found =
            find_nearest_jest_package_json_config(dir.path(), &dir.path().join("packages/a"))
                .expect("found");
        assert_eq!(found, dir.path().join("package.json"));
    }

    #[test]
    fn find_nearest_jest_package_json_config_prefers_closest() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "package.json",
            r#"{"name":"root","jest":{"testEnvironment":"node"}}"#,
        );
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name":"a","jest":{"testEnvironment":"jsdom"}}"#,
        );
        let found =
            find_nearest_jest_package_json_config(dir.path(), &dir.path().join("packages/a"))
                .expect("found");
        assert_eq!(found, dir.path().join("packages/a/package.json"));
    }

    #[test]
    fn find_nearest_jest_package_json_config_none_without_a_jest_field() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name":"root"}"#);
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        assert!(
            find_nearest_jest_package_json_config(dir.path(), &dir.path().join("packages/a"))
                .is_none()
        );
    }

    #[test]
    fn extract_runner_config_referenced_paths_finds_single_string_and_array_values() {
        let path = Path::new("vitest.config.ts");
        let content = "export default { test: { \
                        setupFiles: './single-setup.ts', \
                        setupFilesAfterEnv: ['./after-env-1.ts', './after-env-2.ts'], \
                        globalSetup: './global-setup.ts', \
                        unrelatedKey: './not-collected.ts' \
                        } };\n";
        let mut found = extract_runner_config_referenced_paths(path, content);
        found.sort();
        assert_eq!(
            found,
            vec![
                "./after-env-1.ts",
                "./after-env-2.ts",
                "./global-setup.ts",
                "./single-setup.ts",
            ]
        );
    }

    #[test]
    fn extract_runner_config_referenced_paths_empty_on_unparseable_content() {
        let path = Path::new("jest.config.js");
        let found = extract_runner_config_referenced_paths(path, "not valid js {{{");
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn resolve_runner_config_referenced_files_resolves_root_dir_token_and_recurses_into_base_config()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "vitest.config.base.ts",
            "export default { test: { setupFiles: ['<rootDir>/base.setup.ts'] } };\n",
        );
        write(dir.path(), "base.setup.ts", "globalThis.__base = true;\n");
        write(
            dir.path(),
            "vitest.config.ts",
            "import base from './vitest.config.base';\n\
             export default { ...base, test: { ...base.test, setupFiles: ['./leaf.setup.ts'] } };\n",
        );
        write(dir.path(), "leaf.setup.ts", "globalThis.__leaf = true;\n");

        let config_path = dir.path().join("vitest.config.ts");
        let content = std::fs::read_to_string(&config_path).expect("read fixture");
        let mut scan = resolve_runner_config_referenced_files(&config_path, &content)
            .expect("resolve referenced files");
        scan.files.sort();

        assert_eq!(
            scan.files,
            vec![
                dir.path().join("base.setup.ts"),
                dir.path().join("leaf.setup.ts"),
                dir.path().join("vitest.config.base.ts"),
            ]
        );
        assert!(
            scan.bare_specifiers.is_empty(),
            "{:?}",
            scan.bare_specifiers
        );
    }

    #[test]
    fn resolve_runner_config_referenced_files_empty_when_config_names_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("vitest.config.ts");
        let content = "export default { test: {} };\n";
        write(dir.path(), "vitest.config.ts", content);
        let scan = resolve_runner_config_referenced_files(&config_path, content)
            .expect("resolve referenced files");
        assert!(scan.files.is_empty(), "{:?}", scan.files);
        assert!(
            scan.bare_specifiers.is_empty(),
            "{:?}",
            scan.bare_specifiers
        );
    }

    /// The gap this session's fix closes: a bare (third-party) import inside
    /// the runner config itself — e.g. vitest.config.ts's own `import react
    /// from '@vitejs/plugin-react'` — is collected, not silently dropped,
    /// so `test_deps_config` can stage it in the sandbox the same way it
    /// already stages the test file's own third-party imports.
    #[test]
    fn resolve_runner_config_referenced_files_collects_bare_specifiers_without_following_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("vitest.config.ts");
        let content = "import react from '@vitejs/plugin-react';\n\
             export default { plugins: [react()], test: {} };\n";
        write(dir.path(), "vitest.config.ts", content);
        let scan = resolve_runner_config_referenced_files(&config_path, content)
            .expect("resolve referenced files");
        assert!(
            scan.files.is_empty(),
            "a bare specifier must not be followed as a file: {:?}",
            scan.files
        );
        assert_eq!(scan.bare_specifiers.len(), 1);
        assert_eq!(scan.bare_specifiers[0].package_name, "@vitejs/plugin-react");
        assert_eq!(scan.bare_specifiers[0].specifier, "@vitejs/plugin-react");
    }

    /// The single most important test in this milestone (per the task): the
    /// closure for one test file must include only files transitively
    /// reachable *from that file*, not every file in the package — an
    /// unrelated sibling test file (and the source it alone imports) must
    /// stay out of the closure entirely.
    #[test]
    fn build_test_closure_is_scoped_to_the_one_test_files_own_imports() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
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

        let canonical_root = dir.path().canonicalize().expect("canonicalize");
        let closure = build_test_closure(
            &graph,
            &canonical_root,
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build closure");

        assert!(closure.files.contains("packages/a/src/a.test.ts"));
        assert!(closure.files.contains("packages/a/src/a.ts"));
        assert!(
            !closure.files.contains("packages/a/src/b.ts"),
            "b.ts is only imported by b.test.ts, never by a.test.ts: {:?}",
            closure.files
        );
        assert!(
            !closure.files.contains("packages/a/src/b.test.ts"),
            "an unrelated sibling test file must never appear in a.test.ts's own closure: {:?}",
            closure.files
        );
    }

    /// A file reached transitively (test -> helper -> deep) must also be in
    /// the closure, not just the test file's own direct imports.
    #[test]
    fn build_test_closure_follows_transitive_first_party_imports() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
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

        let canonical_root = dir.path().canonicalize().expect("canonicalize");
        let closure = build_test_closure(
            &graph,
            &canonical_root,
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build closure");

        assert!(closure.files.contains("packages/a/src/helper.ts"));
        assert!(
            closure.files.contains("packages/a/src/deep.ts"),
            "a transitively-reached file must be in the closure: {:?}",
            closure.files
        );
    }

    /// An import that resolves outside the owning package (a workspace
    /// sibling) lands in `external_files`, one-hop only — it is not itself
    /// recursed into.
    #[test]
    fn build_test_closure_records_cross_package_import_as_external_one_hop() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name":"a","dependencies":{"b":"workspace:*"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "import { x } from '../../b/src/index';\ntest('a', () => x);\n",
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

        let canonical_root = dir.path().canonicalize().expect("canonicalize");
        let closure = build_test_closure(
            &graph,
            &canonical_root,
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build closure");

        assert!(
            closure.external_files.contains("packages/b/src/index.ts"),
            "{:?}",
            closure.external_files
        );
        assert!(!closure.files.contains("packages/b/src/index.ts"));
    }

    /// Confirmed live: a scoped third-party package physically nested
    /// *inside* the owning package's own `node_modules` (a real on-disk
    /// shape — version-conflict dedup, or a stray local install) resolves
    /// to a path that satisfies `starts_with(pkg_dir)` just like a genuine
    /// first-party file. It must still land in `external_files`, never
    /// `files` — `files` is turned straight into a raw `fs:file` Input with
    /// no further classification, while `Provider::test_deps_config` sends
    /// every `external_files` entry through `classify_resolved_edge`
    /// (landing on the lockfile-driven relocated `js/node_modules:group`
    /// addr instead). Landing this edge in `files` produces two different
    /// targets both declaring the identical sandbox path — an output
    /// collision at run time, not a test-time symptom.
    #[test]
    fn build_test_closure_records_import_from_ambient_nested_node_modules_as_external() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name":"a"}"#);
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "import { x } from '@apollo/client';\ntest('a', () => x);\n",
        );
        write(
            dir.path(),
            "packages/a/node_modules/@apollo/client/package.json",
            r#"{"name":"@apollo/client","version":"4.2.3","main":"core/index.js"}"#,
        );
        write(
            dir.path(),
            "packages/a/node_modules/@apollo/client/core/index.js",
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

        let canonical_root = dir.path().canonicalize().expect("canonicalize");
        let closure = build_test_closure(
            &graph,
            &canonical_root,
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build closure");

        assert!(
            closure
                .external_files
                .contains("packages/a/node_modules/@apollo/client/core/index.js"),
            "an ambient-nested third-party file must be classified external, not first-party: \
             {:?}",
            closure.external_files
        );
        assert!(
            !closure
                .files
                .contains("packages/a/node_modules/@apollo/client/core/index.js"),
            "must never also land in `files` — that's what produces the sandbox collision: {:?}",
            closure.files
        );
    }

    /// Pins the *boundary* of the accepted "one-hop external" trim (see this
    /// module's `TestClosure` doc and `driver_test.rs`'s module docs): the
    /// externally-reached file itself (`packages/b/src/index.ts`) is
    /// declared, but its own further import (`packages/b/src/helper.ts`,
    /// reached only because `index.ts` re-exports it — the common
    /// barrel-file pattern) is not walked or declared anywhere, in either
    /// `files` or `external_files`. A feature-quality M4 review flagged this
    /// as untested: without this test, nothing pins *where* the accepted
    /// trim stops, so a future change that accidentally started following
    /// one more hop (or stopped following the first) would not be caught.
    #[test]
    fn build_test_closure_does_not_follow_the_cross_package_files_own_further_imports() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "packages/a/package.json",
            r#"{"name":"a","dependencies":{"b":"workspace:*"}}"#,
        );
        write(
            dir.path(),
            "packages/a/src/a.test.ts",
            "import { x } from '../../b/src/index';\ntest('a', () => x);\n",
        );
        write(dir.path(), "packages/b/package.json", r#"{"name":"b"}"#);
        // `index.ts` re-exports `helper.ts` — the common barrel-file
        // pattern. A test reaching only `index.ts` directly must not also
        // get `helper.ts`'s own content folded into its declared Input set.
        write(
            dir.path(),
            "packages/b/src/index.ts",
            "export { x } from './helper';\n",
        );
        write(
            dir.path(),
            "packages/b/src/helper.ts",
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

        let canonical_root = dir.path().canonicalize().expect("canonicalize");
        let closure = build_test_closure(
            &graph,
            &canonical_root,
            "packages/a",
            "packages/a/src/a.test.ts",
        )
        .expect("build closure");

        assert!(
            closure.external_files.contains("packages/b/src/index.ts"),
            "the one-hop external file itself must still be declared: {:?}",
            closure.external_files
        );
        assert!(
            !closure.external_files.contains("packages/b/src/helper.ts")
                && !closure.files.contains("packages/b/src/helper.ts"),
            "the externally-reached file's own further import must NOT be followed — this pins \
             the accepted one-hop trim's boundary: files={:?} external_files={:?}",
            closure.files,
            closure.external_files
        );
    }

    // ---- find_nearest_lint_config ----

    #[test]
    fn find_nearest_lint_config_finds_dot_oxlintrc_at_package_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        write(dir.path(), "packages/a/.oxlintrc.json", "{}");
        let found = find_nearest_lint_config(
            dir.path(),
            &dir.path().join("packages/a"),
            &[".oxlintrc.json"],
        )
        .expect("find config");
        assert_eq!(found, dir.path().join("packages/a/.oxlintrc.json"));
    }

    #[test]
    fn find_nearest_lint_config_walks_up_to_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "package.json", r#"{"name": "root"}"#);
        write(dir.path(), ".oxlintrc.json", "{}");
        write(dir.path(), "packages/a/package.json", r#"{"name": "a"}"#);
        let found = find_nearest_lint_config(
            dir.path(),
            &dir.path().join("packages/a"),
            &[".oxlintrc.json"],
        )
        .expect("find ancestor config");
        assert_eq!(found, dir.path().join(".oxlintrc.json"));
    }

    // ---- detect_eslint_type_aware ----

    #[test]
    fn detect_eslint_type_aware_none_when_no_project_key() {
        let result = detect_eslint_type_aware(
            Path::new("eslint.config.js"),
            "export default [{ rules: { semi: 'error' } }];",
        )
        .expect("scan config");
        assert_eq!(result, Vec::new());
    }

    #[test]
    fn detect_eslint_type_aware_flat_config_string_project() {
        let content = r#"
            export default [
              {
                languageOptions: {
                  parserOptions: {
                    project: './tsconfig.json',
                  },
                },
              },
            ];
        "#;
        let result =
            detect_eslint_type_aware(Path::new("eslint.config.js"), content).expect("scan config");
        assert_eq!(
            result,
            vec![EslintProjectOption::Paths(vec![
                "./tsconfig.json".to_string()
            ])]
        );
    }

    #[test]
    fn detect_eslint_type_aware_flat_config_boolean_project_is_auto_detect() {
        let content = r#"
            export default [
              { languageOptions: { parserOptions: { project: true } } },
            ];
        "#;
        let result =
            detect_eslint_type_aware(Path::new("eslint.config.js"), content).expect("scan config");
        assert_eq!(result, vec![EslintProjectOption::AutoDetect]);
    }

    #[test]
    fn detect_eslint_type_aware_flat_config_array_of_projects() {
        let content = r#"
            export default [
              {
                languageOptions: {
                  parserOptions: { project: ['./tsconfig.json', './tsconfig.test.json'] },
                },
              },
            ];
        "#;
        let result =
            detect_eslint_type_aware(Path::new("eslint.config.js"), content).expect("scan config");
        assert_eq!(
            result,
            vec![EslintProjectOption::Paths(vec![
                "./tsconfig.json".to_string(),
                "./tsconfig.test.json".to_string()
            ])]
        );
    }

    /// **The specific code-quality M5 gap**: a multi-entry flat config
    /// (separate override blocks, each with its own `parserOptions.project`)
    /// must have every occurrence collected, not just the first.
    #[test]
    fn detect_eslint_type_aware_flat_config_multiple_entries_all_collected() {
        let content = r#"
            export default [
              { files: ['src/**/*.ts'], languageOptions: { parserOptions: { project: './tsconfig.json' } } },
              { files: ['test/**/*.ts'], languageOptions: { parserOptions: { project: './tsconfig.test.json' } } },
            ];
        "#;
        let result =
            detect_eslint_type_aware(Path::new("eslint.config.js"), content).expect("scan config");
        assert_eq!(
            result,
            vec![
                EslintProjectOption::Paths(vec!["./tsconfig.json".to_string()]),
                EslintProjectOption::Paths(vec!["./tsconfig.test.json".to_string()]),
            ],
            "both override blocks' own `parserOptions.project` must be collected: {result:?}"
        );
    }

    #[test]
    fn detect_eslint_type_aware_legacy_json_config() {
        let content = r#"{
            "parserOptions": { "project": "./tsconfig.json" },
            "extends": ["eslint:recommended"]
        }"#;
        let result =
            detect_eslint_type_aware(Path::new(".eslintrc.json"), content).expect("scan config");
        assert_eq!(
            result,
            vec![EslintProjectOption::Paths(vec![
                "./tsconfig.json".to_string()
            ])]
        );
    }

    #[test]
    fn detect_eslint_type_aware_legacy_yaml_config() {
        let content = "parserOptions:\n  project: ./tsconfig.json\n";
        let result =
            detect_eslint_type_aware(Path::new(".eslintrc.yaml"), content).expect("scan config");
        assert_eq!(
            result,
            vec![EslintProjectOption::Paths(vec![
                "./tsconfig.json".to_string()
            ])]
        );
    }

    #[test]
    fn detect_eslint_type_aware_legacy_json_no_project_is_none() {
        let content = r#"{ "extends": ["eslint:recommended"] }"#;
        let result =
            detect_eslint_type_aware(Path::new(".eslintrc.json"), content).expect("scan config");
        assert_eq!(result, Vec::new());
    }

    // ---- eslint_module_name ----

    #[test]
    fn eslint_module_name_maps_bare_extends_to_eslint_config_prefix() {
        assert_eq!(
            eslint_module_name("airbnb", EslintRefKind::Extends),
            "eslint-config-airbnb"
        );
    }

    #[test]
    fn eslint_module_name_leaves_already_prefixed_extends_alone() {
        assert_eq!(
            eslint_module_name("eslint-config-airbnb", EslintRefKind::Extends),
            "eslint-config-airbnb"
        );
    }

    #[test]
    fn eslint_module_name_maps_bare_plugin_to_eslint_plugin_prefix() {
        assert_eq!(
            eslint_module_name("react", EslintRefKind::Plugin),
            "eslint-plugin-react"
        );
    }

    #[test]
    fn eslint_module_name_resolves_plugin_colon_extends_shorthand() {
        assert_eq!(
            eslint_module_name("plugin:react/recommended", EslintRefKind::Extends),
            "eslint-plugin-react"
        );
    }

    #[test]
    fn eslint_module_name_maps_scoped_extends_and_plugins() {
        assert_eq!(
            eslint_module_name("@myorg/foo", EslintRefKind::Extends),
            "@myorg/eslint-config-foo"
        );
        assert_eq!(
            eslint_module_name("@myorg", EslintRefKind::Extends),
            "@myorg/eslint-config"
        );
        assert_eq!(
            eslint_module_name("@myorg/eslint-config-foo", EslintRefKind::Extends),
            "@myorg/eslint-config-foo"
        );
        assert_eq!(
            eslint_module_name("@myorg/foo", EslintRefKind::Plugin),
            "@myorg/eslint-plugin-foo"
        );
    }

    // ---- extract_eslint_module_refs ----

    #[test]
    fn extract_eslint_module_refs_flat_config_scans_bare_imports() {
        // Real flat config has no `extends` key at all (a shared config is
        // spread in) and `plugins` is an object of already-imported modules
        // — the package names come from the file's own `import` specifiers.
        let content = r#"
            import js from '@eslint/js';
            import reactHooks from 'eslint-plugin-react-hooks';
            import './local-helper.js';
            export default [
              js.configs.recommended,
              { plugins: { 'react-hooks': reactHooks } },
            ];
        "#;
        let mut refs = extract_eslint_module_refs(Path::new("eslint.config.js"), content)
            .expect("scan config");
        refs.sort();
        assert_eq!(
            refs,
            vec![
                "@eslint/js".to_string(),
                "eslint-plugin-react-hooks".to_string()
            ]
        );
    }

    #[test]
    fn extract_eslint_module_refs_legacy_json_config() {
        let content = r#"{
            "extends": ["eslint:recommended", "airbnb"],
            "plugins": ["react-hooks"]
        }"#;
        let mut refs =
            extract_eslint_module_refs(Path::new(".eslintrc.json"), content).expect("scan config");
        refs.sort();
        // "eslint:recommended" is a built-in sentinel, filtered out; the
        // rest are mapped through eslint's own naming convention.
        assert_eq!(
            refs,
            vec![
                "eslint-config-airbnb".to_string(),
                "eslint-plugin-react-hooks".to_string(),
            ]
        );
    }

    #[test]
    fn extract_eslint_module_refs_legacy_js_config_scans_extends_plugins_keys() {
        // A legacy `.eslintrc.js`/`.cjs` is still JS-shaped, but (unlike flat
        // config) genuinely does use `extends`/`plugins` string arrays —
        // dispatched by basename, not extension.
        let content = r#"
            module.exports = {
              extends: ['plugin:react/recommended'],
              plugins: ['react-hooks'],
            };
        "#;
        let mut refs =
            extract_eslint_module_refs(Path::new(".eslintrc.js"), content).expect("scan config");
        refs.sort();
        assert_eq!(
            refs,
            vec![
                "eslint-plugin-react".to_string(),
                "eslint-plugin-react-hooks".to_string(),
            ]
        );
    }

    #[test]
    fn extract_eslint_module_refs_skips_relative_paths() {
        let content = r#"{ "extends": ["./base.eslintrc.json"] }"#;
        let refs =
            extract_eslint_module_refs(Path::new(".eslintrc.json"), content).expect("scan config");
        assert!(refs.is_empty(), "{refs:?}");
    }

    // ---- Ad hoc perf measurement (not a regression test; see task) ----
    //
    // Standalone, self-contained timing of `build_package_import_graph`
    // across a synthetic multi-package workspace. NOT wired into
    // `crates/bench`'s Tier A/B baseline-vs-candidate harness (that's a
    // separate, bigger project) — this is a one-off measurement to answer
    // "is the import-graph build actually fast" for the perf-measurement
    // task. `#[ignore]`d so `tst` never runs it; run explicitly:
    //
    //   cargo test -p plugin-js --lib pluginjs::importgraph::tests::adhoc_bench \
    //     -- --ignored --nocapture --test-threads=1
    mod adhoc_bench {
        use super::*;
        use std::sync::Arc;
        use std::time::Instant;

        /// Build a synthetic workspace of `n_pkgs` packages, `files_per_pkg`
        /// source files each. Each file has: 2 relative same-package imports
        /// (to other files in the same package, wrapping), 1 relative
        /// cross-package import (first-party edge into a sibling package —
        /// pnpm/npm workspaces resolve these via `node_modules` symlinks in
        /// reality, but the resolver doesn't care how it got there, only that
        /// the specifier resolves to a real path, so a direct relative path
        /// exercises the same resolution cost), and 1 bare specifier
        /// (`lodash`, third-party, deliberately left unresolvable since no
        /// real `node_modules` is staged — this is the worst case for
        /// resolution cost: `oxc_resolver` must walk every ancestor
        /// `node_modules` directory before giving up).
        fn build_corpus(dir: &Path, n_pkgs: usize, files_per_pkg: usize) {
            for i in 0..n_pkgs {
                write(
                    dir,
                    &format!("packages/pkg{i}/package.json"),
                    &format!(r#"{{"name":"pkg{i}","dependencies":{{"lodash":"^4.0.0"}}}}"#),
                );
                for j in 0..files_per_pkg {
                    let next_local = (j + 1) % files_per_pkg;
                    let next_pkg = (i + 1) % n_pkgs;
                    let content = format!(
                        "import {{ v{next_local} }} from \"./file{next_local}\";\n\
                         import {{ x0 }} from \"../pkg{next_pkg}/src/file0\";\n\
                         import lodash from \"lodash\";\n\
                         export const v{j} = 1;\n"
                    );
                    write(dir, &format!("packages/pkg{i}/src/file{j}.ts"), &content);
                }
            }
        }

        /// Cold (first build) vs warm (`ResolveCache`/graph reused — this
        /// mimics `Provider::import_graph`'s per-package `OnceCell`, which on
        /// a warm hit just returns the cached `Arc` with no re-parse/re-resolve
        /// at all) timings for one workspace size. Prints absolute numbers;
        /// asserts nothing about wall-clock (perf numbers vary by machine —
        /// this is a measurement tool, not a regression gate).
        fn run_cold_warm(label: &str, n_pkgs: usize, files_per_pkg: usize) {
            let dir = tempfile::tempdir().expect("tempdir");
            build_corpus(dir.path(), n_pkgs, files_per_pkg);
            let w = walker();

            // Cold: one fresh Resolvers + ResolveCache per package, exactly
            // as `Provider::import_graph` does on a graph_cache miss.
            let cold_start = Instant::now();
            let mut total_edges = 0usize;
            for i in 0..n_pkgs {
                let pkg = format!("packages/pkg{i}");
                let resolvers = Resolvers::new(None);
                let resolve_cache = ResolveCache::new();
                let graph = build_package_import_graph(
                    &w,
                    dir.path(),
                    &pkg,
                    &resolvers,
                    &resolve_cache,
                    None,
                )
                .expect("build import graph");
                total_edges += graph.runtime_edges.len();
            }
            let cold = cold_start.elapsed();

            // Warm: what `Provider::import_graph`'s `OnceCell` actually saves
            // on a second request for the same package is the ENTIRE
            // parse+resolve pass (it never re-enters `build_package_import_graph`
            // at all) — so the honest "warm" number here is a second
            // Arc-cached lookup, which is why we don't even call
            // `build_package_import_graph` again: that would measure a
            // *different, uncached* pipeline (fresh `ResolveCache`), not what
            // M5's cache actually delivers. Instead we time the trivial
            // `OnceCell::get` re-fetch cost directly against the same graphs
            // built above, to make the "what does the cache save" comparison
            // honest rather than re-running the expensive path a second time.
            let cached: Vec<Arc<ImportGraph>> = (0..n_pkgs)
                .map(|i| {
                    let pkg = format!("packages/pkg{i}");
                    let resolvers = Resolvers::new(None);
                    let resolve_cache = ResolveCache::new();
                    Arc::new(
                        build_package_import_graph(
                            &w,
                            dir.path(),
                            &pkg,
                            &resolvers,
                            &resolve_cache,
                            None,
                        )
                        .expect("build import graph"),
                    )
                })
                .collect();
            let warm_start = Instant::now();
            let mut warm_edges = 0usize;
            for g in &cached {
                warm_edges += g.runtime_edges.len();
            }
            let warm = warm_start.elapsed();

            let total_files = n_pkgs * files_per_pkg;
            println!(
                "[adhoc-bench] {label}: {n_pkgs} pkgs x {files_per_pkg} files = {total_files} files\n\
                 \x20 cold total  = {cold:?}  ({:.3} ms/pkg, {:.4} ms/file)\n\
                 \x20 warm total  = {warm:?}  (Arc-cache re-fetch; ~0 by construction — the real\n\
                 \x20              saving IS the {cold:?} avoided, not a re-run)\n\
                 \x20 edges       = {total_edges} cold, {warm_edges} warm (sanity: equal)",
                cold.as_secs_f64() * 1000.0 / n_pkgs as f64,
                cold.as_secs_f64() * 1000.0 / total_files as f64,
            );
            assert_eq!(total_edges, warm_edges, "sanity: same graphs");
        }

        #[test]
        #[ignore]
        fn adhoc_bench_cold_warm_small() {
            run_cold_warm("small", 20, 15);
        }

        #[test]
        #[ignore]
        fn adhoc_bench_cold_warm_large() {
            run_cold_warm("large", 150, 15);
        }

        /// Splits the cold-build cost into parse time (`importparse::parse_file_imports`,
        /// pure `oxc_parser`) vs resolve time (`ResolveCache::get_or_resolve`,
        /// `oxc_resolver`) by manually replicating `build_package_import_graph`'s
        /// loop with two separate `Instant` accumulators — the production
        /// function doesn't expose this split itself, so this is a deliberate
        /// duplicate of its loop body for measurement purposes only.
        #[test]
        #[ignore]
        fn adhoc_bench_parse_vs_resolve_phase() {
            let n_pkgs = 150;
            let files_per_pkg = 15;
            let dir = tempfile::tempdir().expect("tempdir");
            build_corpus(dir.path(), n_pkgs, files_per_pkg);
            let w = walker();

            let mut parse_total = std::time::Duration::ZERO;
            let mut resolve_total = std::time::Duration::ZERO;
            let mut file_count = 0usize;
            let mut site_count = 0usize;

            for i in 0..n_pkgs {
                let pkg = format!("packages/pkg{i}");
                let resolvers = Resolvers::new(None);
                let resolve_cache = ResolveCache::new();
                let files = package_source_files(&w, dir.path(), &pkg).expect("list files");
                for file in files {
                    file_count += 1;
                    let text = std::fs::read_to_string(&file).expect("read file");

                    let t0 = Instant::now();
                    let parsed =
                        importparse::parse_file_imports(&file, &text).expect("parse imports");
                    parse_total += t0.elapsed();

                    let dir_of_file = file.parent().unwrap_or(&file);
                    let t1 = Instant::now();
                    for site in &parsed.sites {
                        site_count += 1;
                        let flavor = if site.type_only {
                            CacheFlavor::Types
                        } else {
                            match site.context {
                                ModuleContext::Esm => CacheFlavor::RuntimeEsm,
                                ModuleContext::Cjs => CacheFlavor::RuntimeCjs,
                            }
                        };
                        let base = if site.type_only { &file } else { dir_of_file };
                        let _ =
                            resolve_cache.get_or_resolve(&resolvers, flavor, base, &site.specifier);
                    }
                    resolve_total += t1.elapsed();
                }
            }

            let total = parse_total + resolve_total;
            println!(
                "[adhoc-bench] phase split: {n_pkgs} pkgs x {files_per_pkg} files = {file_count} files, {site_count} import sites\n\
                 \x20 parse   = {parse_total:?}  ({:.1}% of parse+resolve, {:.4} ms/file)\n\
                 \x20 resolve = {resolve_total:?}  ({:.1}% of parse+resolve, {:.4} ms/site)",
                parse_total.as_secs_f64() / total.as_secs_f64() * 100.0,
                parse_total.as_secs_f64() * 1000.0 / file_count as f64,
                resolve_total.as_secs_f64() / total.as_secs_f64() * 100.0,
                resolve_total.as_secs_f64() * 1000.0 / site_count as f64,
            );
        }
    }
}
