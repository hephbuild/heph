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

/// An `archive.create` [`Format`] option: one of the [`Format::WRITABLE`]
/// spellings. A read-only one (`tar.xz`, `gz`) is refused at parse, saying
/// so, rather than as an unknown spelling.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WritableFormatArg(pub(crate) Format);

impl FromSpecValue for WritableFormatArg {
    fn from_spec_value(v: &hcore::htvalue::Value) -> anyhow::Result<Self> {
        let raw = String::from_spec_value(v)?;
        match Format::parse(&raw) {
            Ok(f) if f.is_writable() => Ok(Self(f)),
            Ok(f) => Err(read_only(f)),
            Err(_) => anyhow::bail!(
                "unknown archive format {raw:?} (one of: {})",
                writable_spellings()
            ),
        }
    }

    fn spec_param_type() -> ParamType {
        ParamType::one_of(Format::WRITABLE.iter().map(|f| f.as_str()))
    }
}

/// The formats `archive.create` writes, for messages.
pub(crate) fn writable_spellings() -> String {
    Format::WRITABLE.map(Format::as_str).join(", ")
}

/// `archive.create` was asked for a format it only reads.
pub(crate) fn read_only(f: Format) -> anyhow::Error {
    anyhow::anyhow!(
        "archive.create cannot write a {:?}: archive.extract reads it, archive.create writes \
         one of: {}",
        f.as_str(),
        writable_spellings()
    )
}

/// A path option (`out`, `strip_prefix`, `prefix`) is relative, `/`-separated
/// and has no `.`/`..`/empty component, so it names one place on every
/// platform and cannot reach outside the tree it is relative to. It holds no
/// NUL, which no path can, and which the def hashes use as a separator.
pub(crate) fn check_rel(path: &str) -> anyhow::Result<()> {
    if path.is_empty() {
        anyhow::bail!("must not be empty");
    }
    if path.contains('\0') {
        anyhow::bail!("{path:?} must not contain a NUL byte");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_rel_refuses_what_names_no_single_relative_path() {
        for ok in ["a", "a/b", "a.b/c..d", ".hidden"] {
            check_rel(ok).unwrap_or_else(|e| panic!("{ok:?}: {e:#}"));
        }
        for (bad, want) in [
            ("", "empty"),
            ("/abs", "relative"),
            ("a//b", "components"),
            ("a/./b", "components"),
            ("../a", "components"),
            ("a/", "components"),
            // Would collide with `out = "o", executable = True` in extract's
            // def hash, which appends `\0executable` after `out`.
            ("o\0executable", "NUL"),
        ] {
            let msg = format!("{:#}", check_rel(bad).unwrap_err());
            assert!(msg.contains(want), "{bad:?}: {msg}");
        }
    }
}
