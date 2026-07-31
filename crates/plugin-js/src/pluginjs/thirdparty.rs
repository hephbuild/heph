//! Addressing for third-party `js_install` targets: one hermetic target per
//! `(name, version)` (integrity is re-derived from the lockfile at
//! `Provider::get` time, not encoded in the addr itself — a lockfile can't
//! record two different integrity hashes for the same `(name, version)`
//! without being corrupt). Mirrors the Go plugin's
//! `@heph/go/thirdparty/<module>@<version>` synthetic-package convention
//! (see `crates/plugin-go/src/plugingo/thirdparty.rs`).
//!
//! The platform is always part of the addr (as `goos`/`goarch` args), unlike
//! Go's per-package variant machinery which only varies when a package
//! actually needs it — see `ai-docs/js-plugin-plan.md`'s Variants section
//! ("Cache key MUST include target platform … confirmed, not optional") and
//! ``driver_install``'s module docs for why this stays unconditional rather
//! than special-cased per package.

use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use std::collections::BTreeMap;

/// Target name every third-party `js_install` target uses.
pub const INSTALL_TARGET: &str = "js_install";

/// The synthetic package a `(name, version)` third-party install lives
/// under, e.g. `@heph/js/thirdparty/lodash@4.17.21`.
pub fn thirdparty_pkg(name: &str, version: &str) -> PkgBuf {
    PkgBuf::from(format!("@heph/js/thirdparty/{name}@{version}"))
}

/// The full `js_install` target address for a resolved third-party
/// dependency, pinned to `goos`/`goarch`.
pub fn thirdparty_addr(name: &str, version: &str, goos: &str, goarch: &str) -> Addr {
    let mut args = BTreeMap::new();
    args.insert("goos".to_string(), goos.to_string());
    args.insert("goarch".to_string(), goarch.to_string());
    Addr::new(
        thirdparty_pkg(name, version),
        INSTALL_TARGET.to_string(),
        args,
    )
}

/// Parse `@heph/js/thirdparty/<name>@<version>` back into `(name, version)`.
/// `None` if `pkg` isn't under that namespace, or the trailing `@version`
/// separator is missing.
pub fn parse_thirdparty_pkg(pkg: &str) -> Option<(&str, &str)> {
    let rest = pkg.strip_prefix("@heph/js/thirdparty/")?;
    // Scoped package names embed their own `@`, so split on the *last* `@`
    // (the version separator), not the first.
    let at = rest.rfind('@')?;
    Some((rest.get(..at)?, rest.get(at + 1..)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thirdparty_pkg_formats_name_and_version() {
        assert_eq!(
            thirdparty_pkg("lodash", "4.17.21").as_str(),
            "@heph/js/thirdparty/lodash@4.17.21"
        );
    }

    #[test]
    fn parse_thirdparty_pkg_roundtrips_plain_name() {
        let pkg = thirdparty_pkg("lodash", "4.17.21");
        assert_eq!(
            parse_thirdparty_pkg(pkg.as_str()),
            Some(("lodash", "4.17.21"))
        );
    }

    #[test]
    fn parse_thirdparty_pkg_roundtrips_scoped_name() {
        let pkg = thirdparty_pkg("@esbuild/darwin-arm64", "0.19.0");
        assert_eq!(
            parse_thirdparty_pkg(pkg.as_str()),
            Some(("@esbuild/darwin-arm64", "0.19.0"))
        );
    }

    #[test]
    fn parse_thirdparty_pkg_rejects_other_namespaces() {
        assert_eq!(parse_thirdparty_pkg("packages/a"), None);
    }

    #[test]
    fn thirdparty_addr_carries_platform_args() {
        let addr = thirdparty_addr("lodash", "4.17.21", "linux", "amd64");
        assert_eq!(addr.args.get("goos").map(String::as_str), Some("linux"));
        assert_eq!(addr.args.get("goarch").map(String::as_str), Some("amd64"));
    }
}
