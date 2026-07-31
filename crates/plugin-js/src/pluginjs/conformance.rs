//! Conformance corpus for the import-graph resolver (`resolvers.rs`), per
//! `ai-docs/js-plugin-plan.md`'s "Correctness safety valve" section: "A
//! conformance corpus of real npm packages with tricky `exports` maps,
//! checked against actual Node resolution in CI. Divergence fails the
//! build."
//!
//! ## Node availability: honest accounting
//!
//! `devenv.nix` does not provision a Node.js toolchain (this crate's own
//! `js_install` driver downloads Node packages as *build inputs*, not as a
//! host tool `plugin-js` itself may assume). A `node` binary happening to be
//! on `PATH` (e.g. via a developer's own `nvm`) is therefore not something
//! `cargo test -p plugin-js` can depend on in every environment this suite
//! runs in (a future CI runner, a contributor's machine, ...). Making these
//! tests hard-require `node` would trade a resolver hermeticity problem for
//! a test-suite hermeticity problem.
//!
//! So each fixture below carries **two** layers of truth:
//!
//! 1. A hard-coded `expected` resolution, always checked, so the suite is
//!    fully runnable and fails loudly on a resolver regression with zero
//!    external dependencies. Every fixture's doc comment names the exact
//!    `node`/`node --experimental-import-meta-resolve` invocation used to
//!    derive that value, and the Node version it was run against — recorded
//!    at authoring time in this session: **Node.js v18.12.1** (`.nvm`), see
//!    each fixture below for the literal command and output.
//! 2. A best-effort **live** re-derivation via [`node_require_resolve`] /
//!    [`node_import_meta_resolve`], which shells out to `node` if and only if
//!    one is found on `PATH`, and is silently skipped (not a test failure)
//!    otherwise. When it runs, it must agree with the hard-coded `expected`
//!    value — this is what actually caught transcription mistakes while this
//!    file was being written, and is what would catch real Node semantics
//!    drifting out from under the hard-coded corpus in the future on any
//!    machine that does have Node installed.
//!
//! Honest framing for anyone reading a test failure here: if `node` was on
//! `PATH` when this suite ran, every one of these fixtures was in fact
//! checked against real, running Node.js — not merely against a
//! spec-derived guess. If it wasn't, the hard-coded expectations are still
//! checked, but only as good as the one-time verification recorded above.
//!
//! ## Edge cases covered
//!
//! - Conditional `exports` with multiple simultaneously-active conditions,
//!   and the condition-**order**-sensitivity gotcha: the winning branch is
//!   the first *object key* (in the object's own declaration order) that
//!   names an active condition — **not** the requester's own condition list
//!   in priority order, and not "most specific". Reordering the keys changes
//!   the winner even though the same conditions are active on both sides.
//! - Wildcard `"*"` subpath patterns: an exact literal key always wins over
//!   any pattern key for the same subpath, and among competing patterns the
//!   one with the longer literal prefix before `*` wins (Node's
//!   `patternKeyCompare`), regardless of which pattern is declared first.
//! - Array condition fallback: an array of conditional exports objects is a
//!   genuine fallback chain on **condition mismatch** (an earlier entry
//!   whose only key names an inactive condition is skipped). It is
//!   deliberately **not** a fallback on file-existence — an entry that *is*
//!   condition-matched but names a file missing on disk is a hard failure,
//!   with no attempt to move on to the next array entry.
//! - A `null`-blocked subpath (`"./internal/*": null`) is unconditionally
//!   unresolvable, regardless of whether a real file sits at that path.
//! - Self-referencing imports: a package importing its own name resolves
//!   through its own `exports` map, found by walking up from the importing
//!   file to the nearest ancestor `package.json` with a matching `name` —
//!   this works even when the package is not inside any `node_modules` at
//!   all (that's the whole point of the feature).
//! - The `"imports"` field (`#internal` specifiers): plain, and combined with
//!   both a wildcard subpath and per-condition (ESM vs CJS) branching.

use crate::pluginjs::resolvers::{ModuleContext, ResolveOutcome, Resolvers};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, contents).expect("write fixture file");
}

/// Canonicalize the tempdir root before using it as a resolution base or
/// building an expected result path — see `resolvers.rs`'s identical helper:
/// `oxc_resolver`'s `symlinks: true` realpaths its output, so on macOS
/// (`$TMPDIR` behind `/tmp` -> `/private/tmp`) an expected path built from
/// the non-canonical root would mismatch by exactly that symlink hop.
fn root(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().canonicalize().expect("canonicalize tempdir")
}

/// Whether a `node` binary is reachable on `PATH` right now. Cheap (one
/// short-lived subprocess), called once per fixture — this crate has no
/// perf-sensitive test path, so no need to cache it across fixtures.
fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Outcome of a live cross-check against real Node.
#[derive(Debug, PartialEq, Eq)]
enum NodeOutcome {
    Resolved(PathBuf),
    /// Node itself threw (module not found, blocked export path, ...) — a
    /// legitimate outcome for the null-blocked-subpath fixture, not merely a
    /// harness failure.
    Threw,
}

/// Live-cross-check a CJS `require(specifier)` as if issued from a file
/// located in `from_dir`, via real Node's own `require.resolve`. Returns
/// `None` (skip the live check entirely) only when no `node` binary is on
/// `PATH` — see module docs.
fn node_require_resolve(from_dir: &Path, specifier: &str) -> Option<NodeOutcome> {
    if !node_available() {
        return None;
    }
    std::fs::create_dir_all(from_dir).expect("create cjs probe script dir");
    let script = from_dir.join(".conformance_probe.cjs");
    std::fs::write(
        &script,
        format!("console.log(require.resolve({specifier:?}));\n"),
    )
    .expect("write cjs probe script");
    let output = Command::new("node")
        .arg(&script)
        .output()
        .expect("spawn node for cjs probe");
    drop(std::fs::remove_file(&script));
    if !output.status.success() {
        return Some(NodeOutcome::Threw);
    }
    let stdout = String::from_utf8(output.stdout).expect("node stdout is utf8");
    Some(NodeOutcome::Resolved(PathBuf::from(stdout.trim())))
}

/// Live-cross-check an ESM `import(specifier)` as if issued from a file
/// located in `from_dir`, via real Node's `import.meta.resolve` (run under
/// `--experimental-import-meta-resolve`, required on Node 18; unflagged on
/// Node 20.6+ but the flag is accepted-and-ignored there too). `None` only
/// when no `node` binary is on `PATH`.
fn node_import_meta_resolve(from_dir: &Path, specifier: &str) -> Option<NodeOutcome> {
    if !node_available() {
        return None;
    }
    std::fs::create_dir_all(from_dir).expect("create esm probe script dir");
    let script = from_dir.join(".conformance_probe.mjs");
    std::fs::write(
        &script,
        format!("console.log(await import.meta.resolve({specifier:?}));\n"),
    )
    .expect("write esm probe script");
    let output = Command::new("node")
        .arg("--experimental-import-meta-resolve")
        .arg(&script)
        .output()
        .expect("spawn node for esm probe");
    drop(std::fs::remove_file(&script));
    if !output.status.success() {
        return Some(NodeOutcome::Threw);
    }
    let stdout = String::from_utf8(output.stdout).expect("node stdout is utf8");
    let path = stdout
        .trim()
        .strip_prefix("file://")
        .unwrap_or(stdout.trim());
    Some(NodeOutcome::Resolved(PathBuf::from(path)))
}

/// Assert `outcome` matches `expected`, and — only if `node` is on `PATH` —
/// assert the live Node cross-check agrees too. Centralizes the "two layers
/// of truth" pattern described in the module docs for every fixture that
/// expects a successful resolution.
fn assert_resolved_and_cross_check(
    outcome: ResolveOutcome,
    expected: &Path,
    live: Option<NodeOutcome>,
) {
    assert_eq!(
        outcome,
        ResolveOutcome::Resolved(expected.to_path_buf()),
        "plugin-js resolver diverged from the recorded Node-derived expectation"
    );
    if let Some(live) = live {
        assert_eq!(
            live,
            NodeOutcome::Resolved(expected.to_path_buf()),
            "live Node cross-check diverged from the hard-coded expectation — the corpus itself \
             is stale, not (necessarily) the resolver"
        );
    }
}

/// Same, for a fixture where the correct behavior is an unresolvable /
/// blocked specifier — asserts `plugin-js` reports [`ResolveOutcome::Unresolved`]
/// and, if live-checked, that real Node also threw rather than resolving.
fn assert_unresolved_and_cross_check(outcome: ResolveOutcome, live: Option<NodeOutcome>) {
    assert_eq!(
        outcome,
        ResolveOutcome::Unresolved,
        "plugin-js resolver diverged from the recorded Node-derived expectation (expected \
         unresolvable/blocked)"
    );
    if let Some(live) = live {
        assert_eq!(
            live,
            NodeOutcome::Threw,
            "live Node cross-check diverged from the hard-coded expectation — Node actually \
             resolved this specifier where the corpus expected a hard failure"
        );
    }
}

/// Condition-order-sensitivity: the winning branch is the first *object key*
/// (declaration order) naming an active condition, not the requester's own
/// condition priority. `"node"` is declared before `"import"`, and both are
/// active for an ESM `import`, so `"node"`'s branch wins even though the
/// specifier is reached via `import`.
///
/// Node-derived via, in a fixture with this exact `exports` map:
/// `node --experimental-import-meta-resolve resolve_esm.mjs` (containing
/// `console.log(await import.meta.resolve('pkg'))`), from a directory
/// alongside `node_modules/pkg` — printed
/// `file://.../node_modules/pkg/node-out.js` (Node.js v18.12.1).
#[test]
fn conditional_exports_condition_order_sensitivity_object_key_order_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "node_modules/pkg/package.json",
        r#"{
            "name": "pkg",
            "exports": {
                ".": {
                    "node": "./node-out.js",
                    "import": "./import-out.mjs",
                    "default": "./default-out.js"
                }
            }
        }"#,
    );
    write(&root, "node_modules/pkg/node-out.js", "module.exports.x=1;");
    write(
        &root,
        "node_modules/pkg/import-out.mjs",
        "export const x=1;",
    );
    write(
        &root,
        "node_modules/pkg/default-out.js",
        "module.exports.x=1;",
    );

    let resolvers = Resolvers::new(None);
    let outcome = resolvers.resolve_runtime(ModuleContext::Esm, &root, "pkg");
    let expected = root.join("node_modules/pkg/node-out.js");
    let live = node_import_meta_resolve(&root, "pkg");
    assert_resolved_and_cross_check(outcome, &expected, live);
}

/// An exact literal subpath key always wins over a wildcard pattern key that
/// would also match, regardless of declaration order (the pattern is
/// declared first here, and still loses).
///
/// Node-derived via `node resolve_cjs.cjs` (containing
/// `require.resolve('pkg/features/special.js')`) — printed
/// `.../node_modules/pkg/src/special-override.js`, not the pattern
/// expansion `.../src/features/special.js` (Node.js v18.12.1).
#[test]
fn wildcard_subpath_exact_literal_key_beats_pattern_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "node_modules/pkg/package.json",
        r#"{
            "name": "pkg",
            "exports": {
                "./features/*.js": "./src/features/*.js",
                "./features/special.js": "./src/special-override.js"
            }
        }"#,
    );
    write(
        &root,
        "node_modules/pkg/src/features/other.js",
        "module.exports.x=1;",
    );
    write(
        &root,
        "node_modules/pkg/src/special-override.js",
        "module.exports.override=1;",
    );

    let resolvers = Resolvers::new(None);

    // The exact key wins.
    let outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &root, "pkg/features/special.js");
    let expected = root.join("node_modules/pkg/src/special-override.js");
    let live = node_require_resolve(&root, "pkg/features/special.js");
    assert_resolved_and_cross_check(outcome, &expected, live);

    // A non-exact subpath still expands through the pattern.
    let outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &root, "pkg/features/other.js");
    let expected = root.join("node_modules/pkg/src/features/other.js");
    let live = node_require_resolve(&root, "pkg/features/other.js");
    assert_resolved_and_cross_check(outcome, &expected, live);
}

/// Among two overlapping wildcard patterns, the one with the longer literal
/// prefix before `*` wins — Node's `patternKeyCompare` sorts expansion keys
/// by specificity before matching, independent of the object's own
/// declaration order (the more specific key is declared second here, and
/// still wins).
///
/// Node-derived via `node resolve_cjs.cjs` (containing
/// `require.resolve('pkg/features/special/thing')`) — printed
/// `.../node_modules/pkg/src/special/thing.js` (the more specific pattern),
/// not `.../src/generic/special/thing.js` (Node.js v18.12.1).
#[test]
fn wildcard_subpath_longest_prefix_pattern_wins_specificity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "node_modules/pkg/package.json",
        r#"{
            "name": "pkg",
            "exports": {
                "./features/*": "./src/generic/*.js",
                "./features/special/*": "./src/special/*.js"
            }
        }"#,
    );
    write(
        &root,
        "node_modules/pkg/src/generic/other.js",
        "module.exports.x=1;",
    );
    write(
        &root,
        "node_modules/pkg/src/special/thing.js",
        "module.exports.x=1;",
    );

    let resolvers = Resolvers::new(None);

    // The more specific pattern wins for a subpath both patterns could match.
    let outcome =
        resolvers.resolve_runtime(ModuleContext::Cjs, &root, "pkg/features/special/thing");
    let expected = root.join("node_modules/pkg/src/special/thing.js");
    let live = node_require_resolve(&root, "pkg/features/special/thing");
    assert_resolved_and_cross_check(outcome, &expected, live);

    // A subpath only the generic pattern matches still expands correctly.
    let outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &root, "pkg/features/other");
    let expected = root.join("node_modules/pkg/src/generic/other.js");
    let live = node_require_resolve(&root, "pkg/features/other");
    assert_resolved_and_cross_check(outcome, &expected, live);
}

/// Array-format `exports` is a fallback chain on **condition mismatch**: the
/// first array entry is an object whose only key (`"deno"`) names a
/// condition that's never active for this resolver, so it's skipped
/// entirely (not a "missing file" case — no file matching it even exists),
/// falling through to the second, unconditional entry.
///
/// Node-derived via `node --experimental-import-meta-resolve resolve_esm.mjs`
/// (containing `console.log(await import.meta.resolve('pkg'))`) — printed
/// `.../node_modules/pkg/universal.js` (Node.js v18.12.1).
#[test]
fn array_exports_condition_mismatch_falls_through_to_next_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "node_modules/pkg/package.json",
        r#"{
            "name": "pkg",
            "exports": {
                ".": [
                    { "deno": "./deno-only.js" },
                    "./universal.js"
                ]
            }
        }"#,
    );
    write(
        &root,
        "node_modules/pkg/universal.js",
        "module.exports.x=1;",
    );

    let resolvers = Resolvers::new(None);
    let outcome = resolvers.resolve_runtime(ModuleContext::Esm, &root, "pkg");
    let expected = root.join("node_modules/pkg/universal.js");
    let live = node_import_meta_resolve(&root, "pkg");
    assert_resolved_and_cross_check(outcome, &expected, live);
}

/// The array-fallback in the previous fixture is a fallback on *condition*
/// mismatch only — it is deliberately **not** a fallback on file existence.
/// Here the first entry's condition (`"import"`) **is** active, so it wins
/// outright even though the file it names doesn't exist on disk; Node does
/// not "try the next entry" the way it would for a genuinely unmatched
/// condition.
///
/// Node-derived via `node --experimental-import-meta-resolve resolve_esm.mjs`
/// — Node hard-throws `ERR_MODULE_NOT_FOUND` for `missing.mjs`, it does
/// *not* fall through to the second entry (`fallback.mjs`, which does exist)
/// (Node.js v18.12.1).
#[test]
fn array_exports_matched_entry_missing_on_disk_hard_fails_no_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "node_modules/pkg/package.json",
        r#"{
            "name": "pkg",
            "exports": {
                ".": {
                    "import": ["./missing.mjs", "./fallback.mjs"]
                }
            }
        }"#,
    );
    // Deliberately do NOT create missing.mjs.
    write(&root, "node_modules/pkg/fallback.mjs", "export const x=1;");

    let resolvers = Resolvers::new(None);
    let outcome = resolvers.resolve_runtime(ModuleContext::Esm, &root, "pkg");
    let live = node_import_meta_resolve(&root, "pkg");
    assert_unresolved_and_cross_check(outcome, live);
}

/// A `null`-blocked subpath is unconditionally unresolvable — Node reports
/// `ERR_PACKAGE_PATH_NOT_EXPORTED` regardless of whether a real file sits at
/// that path on disk (one does, here, precisely to prove the block isn't a
/// "file missing" coincidence).
///
/// Node-derived via `node resolve_cjs.cjs` (containing a try/catch around
/// `require.resolve('pkg/internal/secret')`) — printed
/// `THREW: ERR_PACKAGE_PATH_NOT_EXPORTED` (Node.js v18.12.1).
#[test]
fn null_blocked_subpath_is_not_exported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "node_modules/pkg/package.json",
        r#"{
            "name": "pkg",
            "exports": {
                ".": "./index.js",
                "./internal/*": null
            }
        }"#,
    );
    write(&root, "node_modules/pkg/index.js", "module.exports.x=1;");
    write(
        &root,
        "node_modules/pkg/internal/secret.js",
        "module.exports.secret=1;",
    );

    let resolvers = Resolvers::new(None);
    let outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &root, "pkg/internal/secret");
    let live = node_require_resolve(&root, "pkg/internal/secret");
    assert_unresolved_and_cross_check(outcome, live);
}

/// Self-referencing imports: a package can `require`/`import` its own
/// package `name` and be routed through its own `exports` map — found by
/// walking up from the importing file to the nearest ancestor
/// `package.json` whose `name` matches. This works with the package sitting
/// *anywhere* (not inside any `node_modules`), which is the entire point of
/// the feature (a package's own test files self-referencing its own public
/// API surface exactly as an external consumer would).
///
/// Node-derived via `node resolve_cjs.cjs` (containing
/// `require.resolve('pkg/util')`), run from `pkg/src/` where `pkg/` (with a
/// `name: "pkg"` `package.json`) is *not* inside any `node_modules` — printed
/// `.../pkg/lib/util.js` (Node.js v18.12.1).
#[test]
fn self_reference_import_own_name_through_own_exports_map() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "pkg/package.json",
        r#"{
            "name": "pkg",
            "exports": {
                ".": "./src/index.js",
                "./util": "./lib/util.js"
            }
        }"#,
    );
    write(&root, "pkg/src/index.js", "module.exports.x=1;");
    write(&root, "pkg/lib/util.js", "module.exports.util=1;");

    let resolvers = Resolvers::new(None);
    let from_dir = root.join("pkg/src");
    let outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &from_dir, "pkg/util");
    let expected = root.join("pkg/lib/util.js");
    let live = node_require_resolve(&from_dir, "pkg/util");
    assert_resolved_and_cross_check(outcome, &expected, live);
}

/// The `"imports"` field (`#internal` specifiers): a private, package-local
/// mapping never visible to external consumers, resolved the same
/// ancestor-`package.json`-walk way as self-reference.
///
/// Node-derived via `node resolve_cjs.cjs` (containing
/// `require.resolve('#internal-utils')`), run from `pkg/src/` — printed
/// `.../pkg/vendor/utils.js` (Node.js v18.12.1).
#[test]
fn imports_field_hash_specifier_plain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "pkg/package.json",
        r##"{
            "name": "pkg",
            "imports": {
                "#internal-utils": "./vendor/utils.js"
            }
        }"##,
    );
    write(&root, "pkg/vendor/utils.js", "module.exports.util=1;");

    let resolvers = Resolvers::new(None);
    let from_dir = root.join("pkg/src");
    let outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &from_dir, "#internal-utils");
    let expected = root.join("pkg/vendor/utils.js");
    let live = node_require_resolve(&from_dir, "#internal-utils");
    assert_resolved_and_cross_check(outcome, &expected, live);
}

/// The `"imports"` field combined with both a wildcard subpath and
/// per-condition branching — resolves to a *different* file for an ESM
/// `import` than for a CJS `require`, exactly like a third-party
/// conditional `exports` map (`resolvers.rs`'s
/// `conditional_exports_resolves_differently_for_esm_and_cjs` test), just
/// reached through `"imports"` instead.
///
/// Node-derived via `node resolve_cjs.cjs` (`require.resolve('#lib/thing')`,
/// printed `.../pkg/vendor/impl/thing.cjs`) and
/// `node --experimental-import-meta-resolve resolve_esm.mjs`
/// (`await import.meta.resolve('#lib/thing')`, printed
/// `.../pkg/vendor/impl/thing.mjs`), both run from `pkg/src/` (Node.js
/// v18.12.1).
#[test]
fn imports_field_hash_specifier_wildcard_and_condition_esm_and_cjs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = root(&dir);
    write(
        &root,
        "pkg/package.json",
        r##"{
            "name": "pkg",
            "imports": {
                "#lib/*": {
                    "import": "./vendor/impl/*.mjs",
                    "require": "./vendor/impl/*.cjs"
                }
            }
        }"##,
    );
    write(&root, "pkg/vendor/impl/thing.mjs", "export const x=1;");
    write(&root, "pkg/vendor/impl/thing.cjs", "module.exports.x=1;");

    let resolvers = Resolvers::new(None);
    let from_dir = root.join("pkg/src");

    let cjs_outcome = resolvers.resolve_runtime(ModuleContext::Cjs, &from_dir, "#lib/thing");
    let cjs_expected = root.join("pkg/vendor/impl/thing.cjs");
    let cjs_live = node_require_resolve(&from_dir, "#lib/thing");
    assert_resolved_and_cross_check(cjs_outcome, &cjs_expected, cjs_live);

    let esm_outcome = resolvers.resolve_runtime(ModuleContext::Esm, &from_dir, "#lib/thing");
    let esm_expected = root.join("pkg/vendor/impl/thing.mjs");
    let esm_live = node_import_meta_resolve(&from_dir, "#lib/thing");
    assert_resolved_and_cross_check(esm_outcome, &esm_expected, esm_live);
}
