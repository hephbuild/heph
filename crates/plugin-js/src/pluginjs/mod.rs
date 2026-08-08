#[cfg(test)]
mod conformance;
mod deps;
mod driver_bundle;
mod driver_install;
mod driver_lint;
mod driver_package_info;
mod driver_test;
mod driver_typecheck;
mod importgraph;
mod importparse;
mod lockfile;
mod package_json;
mod platform;
mod provider;
mod resolvers;
mod thirdparty;
mod toolchain;
mod workspace;

pub use driver_bundle::JsBundleDriver;
pub use driver_install::JsInstallDriver;
pub use driver_lint::JsLintDriver;
pub use driver_package_info::JsPackageInfoDriver;
pub use driver_test::JsTestDriver;
pub use driver_typecheck::JsTypecheckDriver;
pub use provider::{Config, Provider};
pub use workspace::{PkgManager, WorkspaceMember};

/// Filename that anchors a candidate JS/TS package. Mirrors how the Go plugin
/// anchors package discovery on `go.mod`, except every `package.json`
/// directory is its own self-contained package boundary — there is no
/// ancestor-propagation flag to track (a nested `package.json` is always
/// another, independent package, never a continuation of its parent's).
pub const PACKAGE_JSON: &str = "package.json";

/// Target name of the M0 no-op "package info" target every discovered
/// package lists. Carries no build behavior yet — it exists to prove a
/// discovered package resolves through `Provider::get` end to end. Real
/// `js_install`/`js_typecheck`/... targets land in later milestones (see
/// `ai-docs/js-plugin-plan.md`).
pub const PACKAGE_INFO_TARGET: &str = "package_info";

/// Target name of the M3 `js_typecheck` target: runs `tsc --noEmit` (or the
/// nearest ancestor tsconfig's equivalent) over one package at a time. See
/// `driver_typecheck.rs` module docs.
pub const TYPECHECK_TARGET: &str = "js_typecheck";

/// Target name of the M4 `js_test` target: runs the configured test runner
/// (`vitest` default, `jest` alt) against one test file at a time — one
/// target address per matched test file (distinguished by a `file` addr
/// arg), never one per package. See `driver_test.rs` module docs.
pub const TEST_TARGET: &str = "js_test";

/// Target name of the M5 `js_lint` target: runs the configured linter
/// (`oxlint` default, `eslint` alt) over one package at a time — per-package
/// granularity, the same as `js_typecheck` (see `driver_lint.rs` module docs
/// for why per-file caching isn't worth the bookkeeping here either).
pub const LINT_TARGET: &str = "js_lint";

/// Target name of the M6 `js_bundle` target: runs the configured bundler
/// (`esbuild` default) over one package's entry point at a time — whole
/// first-party-transitive-closure granularity, deliberately not per-file or
/// per-package (see `driver_bundle.rs` module docs' "Inputs / cache key"
/// section for why per-file incrementality is explicitly not the goal here).
pub const BUNDLE_TARGET: &str = "js_bundle";

/// Target name of the on-disk `node_modules` sync target: an aggregating
/// `group` (see `crates/builtins/src/plugingroup`) over every third-party
/// dependency this package resolves (direct and transitive — see
/// `deps::resolve_transitive_closure`), each already relocated to its own
/// `<pkg>/node_modules/<name>` by `thirdparty::node_modules_addr`, with
/// `codegen = "copy"` so `heph run //pkg:node_modules` actually materializes
/// the result onto real disk — the only way an IDE (which reads the real
/// filesystem, never heph's own sandbox/cache) can see a hermetically
/// resolved dependency at all. Unlike every other target this provider
/// lists, nothing else in the graph ever depends on this one; it exists
/// solely to be requested directly. See `Provider::node_modules_sync_spec`'s
/// doc for why write-back requires that.
pub const NODE_MODULES_SYNC_TARGET: &str = "node_modules";

/// Directory names that are never a package boundary or workspace member,
/// however deep they appear: `node_modules` is third-party/manager-owned
/// content, never a first-party package this provider should discover, and a
/// `.`-prefixed directory (`.git`, `.turbo`, ...) is tooling state.
pub(crate) fn is_skipped_dir_name(name: &str) -> bool {
    name == "node_modules" || name.starts_with('.')
}
