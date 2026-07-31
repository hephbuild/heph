#[cfg(test)]
mod conformance;
mod deps;
mod driver_install;
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

pub use driver_install::JsInstallDriver;
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

/// Directory names that are never a package boundary or workspace member,
/// however deep they appear: `node_modules` is third-party/manager-owned
/// content, never a first-party package this provider should discover, and a
/// `.`-prefixed directory (`.git`, `.turbo`, ...) is tooling state.
pub(crate) fn is_skipped_dir_name(name: &str) -> bool {
    name == "node_modules" || name.starts_with('.')
}
