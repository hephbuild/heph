#[cfg(test)]
mod conformance;
mod deps;
mod driver_install;
mod driver_package_info;
mod importgraph;
mod importparse;
mod lockfile;
mod package_json;
mod platform;
mod provider;
mod resolvers;
mod thirdparty;
mod workspace;

pub use driver_install::JsInstallDriver;
pub use driver_package_info::JsPackageInfoDriver;
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

/// Directory names that are never a package boundary or workspace member,
/// however deep they appear: `node_modules` is third-party/manager-owned
/// content, never a first-party package this provider should discover, and a
/// `.`-prefixed directory (`.git`, `.turbo`, ...) is tooling state.
pub(crate) fn is_skipped_dir_name(name: &str) -> bool {
    name == "node_modules" || name.starts_with('.')
}
