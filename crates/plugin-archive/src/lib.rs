//! The builtin `archive` plugin: two managed drivers over `crates/archive`.
//!
//! - **`archive.extract`** ([`extract`]): unpack the one archive file a `src`
//!   target produced into a directory output, through the hardened
//!   [`harchive::unpack`].
//! - **`archive.create`** ([`create`]): pack the outputs of `deps` into one
//!   archive file, through the reproducible [`harchive::write`].
//!
//! Both run in-process, off the async workers, and stop on cancellation
//! mid-entry. The user-facing description is `docs/ARCHIVE.md`.
pub mod create;
pub mod extract;

// The `htspec` derive macros expand to `crate::htvalue` / `crate::htspec`.
pub(crate) use hcore::htvalue;
pub(crate) use hplugin::htspec;

use harchive::Format;
use hcore::htvalue::signature::ParamType;
use hplugin::htspec::FromSpecValue;

/// The plugin's name: its drivers are `archive.extract` and `archive.create`.
pub const PLUGIN_NAME: &str = "archive";

/// An archive [`Format`] option: one of its spellings. A newtype because both
/// `Format` and [`FromSpecValue`] are other crates'.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FormatArg(pub(crate) Format);

impl FromSpecValue for FormatArg {
    fn from_spec_value(v: &hcore::htvalue::Value) -> anyhow::Result<Self> {
        Format::parse(&String::from_spec_value(v)?).map(Self)
    }

    fn spec_param_type() -> ParamType {
        ParamType::one_of(Format::ALL.iter().map(|f| f.as_str()))
    }
}

/// A path option (`out`, `strip_prefix`, `prefix`) is relative, `/`-separated
/// and has no `.`/`..`/empty component, so it names one place on every
/// platform and cannot reach outside the tree it is relative to.
pub(crate) fn check_rel(path: &str) -> anyhow::Result<()> {
    if path.is_empty() {
        anyhow::bail!("must not be empty");
    }
    if path.starts_with('/') {
        anyhow::bail!("{path:?} must be relative");
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        anyhow::bail!("{path:?} must not contain empty, `.` or `..` components");
    }
    Ok(())
}

/// `out` joined under the target's package: the workspace-relative path an
/// output is declared at.
pub(crate) fn in_package(pkg: &str, out: &str) -> String {
    if pkg.is_empty() {
        out.to_string()
    } else {
        format!("{pkg}/{out}")
    }
}
