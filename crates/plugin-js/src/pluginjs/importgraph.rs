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
/// note.
fn find_nearest_file(
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
    let mut dir = pkg_dir;
    loop {
        let candidate = dir.join(PACKAGE_JSON);
        if candidate.is_file()
            && let Ok(text) = std::fs::read_to_string(&candidate)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && value.get("jest").is_some()
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

/// Recursively resolve every additional first-party file a test-runner
/// config's own content names or imports: [`RUNNER_CONFIG_FILE_KEYS`]
/// entries, plus a relative `import`/`require` of a shared base config
/// (`import base from '../../vitest.config.base'`) — which may itself name
/// or import more, so each newly-resolved file is scanned the same way in
/// turn. Bounded depth + a visited set guard against a cyclic/self-importing
/// config; in practice a real config chain is one or two files deep.
///
/// **Known scope trim, disclosed rather than silent**: only a *relative*
/// import/require inside the config file is followed — a shared base config
/// pulled in via a bare package specifier (e.g. an internal
/// `@myorg/test-config` package) is not, the same "third-party is
/// `js_install`'s job" boundary `test_deps_config` draws elsewhere for the
/// test file's own closure.
pub fn resolve_runner_config_referenced_files(
    config_path: &Path,
    config_content: &str,
) -> anyhow::Result<Vec<PathBuf>> {
    const MAX_DEPTH: usize = 4;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut found: BTreeSet<PathBuf> = BTreeSet::new();
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
                }
            }
        }
    }

    Ok(found.into_iter().collect())
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
            if edge.resolved.starts_with(&pkg_dir) {
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
        let mut refs = resolve_runner_config_referenced_files(&config_path, &content)
            .expect("resolve referenced files");
        refs.sort();

        assert_eq!(
            refs,
            vec![
                dir.path().join("base.setup.ts"),
                dir.path().join("leaf.setup.ts"),
                dir.path().join("vitest.config.base.ts"),
            ]
        );
    }

    #[test]
    fn resolve_runner_config_referenced_files_empty_when_config_names_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("vitest.config.ts");
        let content = "export default { test: {} };\n";
        write(dir.path(), "vitest.config.ts", content);
        let refs = resolve_runner_config_referenced_files(&config_path, content)
            .expect("resolve referenced files");
        assert!(refs.is_empty(), "{refs:?}");
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
}
