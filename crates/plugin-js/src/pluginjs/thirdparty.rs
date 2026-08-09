//! Addressing for third-party `js_install` targets: one hermetic target per
//! `(name, version)` (integrity is re-derived from the lockfile at
//! `Provider::get` time, not encoded in the addr itself — a lockfile can't
//! record two different integrity hashes for the same `(name, version)`
//! without being corrupt). Mirrors the Go plugin's
//! `@heph/go/thirdparty/<module>@<version>` synthetic-package convention
//! (see `crates/plugin-go/src/plugingo/thirdparty.rs`).
//!
//! The platform is always part of the addr (as `os`/`arch` args), unlike
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
/// dependency, pinned to `os`/`arch`.
pub fn thirdparty_addr(name: &str, version: &str, os: &str, arch: &str) -> Addr {
    let mut args = BTreeMap::new();
    args.insert("os".to_string(), os.to_string());
    args.insert("arch".to_string(), arch.to_string());
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

/// The fixed package namespace every relocated-`node_modules` `group` target
/// lives under — see [`node_modules_addr`]'s doc for why this doesn't encode
/// anything variable in the package string itself.
pub const NODE_MODULES_PKG: &str = "@heph/js/node_modules";

/// Target name every relocated-`node_modules` target uses — the driver name
/// this addr always resolves to (see [`crate::pluginjs::provider`]'s
/// `Provider::get` dispatch).
pub const NODE_MODULES_TARGET: &str = "group";

/// The full `group` target address that relocates a resolved third-party
/// `js_install` download into `<consuming_pkg>/node_modules/<local_name>`
/// inside a consumer's sandbox — see `Provider::get`'s dispatch for this
/// namespace, and `ai-docs/js-plugin-plan.md`'s "Per-target `node_modules`
/// reconstruction" note for why this exists at all: `js_install`'s own
/// output lives at the synthetic path `thirdparty_pkg(resolved_name,
/// version)`, which nothing else ever visits, so every consumer needs its
/// own relocated view.
///
/// **Every variable part is an `Addr` arg, never folded into the package
/// string.** An earlier draft of this addr encoded `consuming_pkg`/
/// `local_name`/`version` positionally inside the package path
/// (`@heph/js/node_modules/<consuming_pkg>/<local_name>@<version>`) — a
/// hermeticity review caught that this is only unambiguously splittable
/// when `consuming_pkg` itself never contains a scoped-looking segment
/// (e.g. a real package nested at `apps/@special` breaks the split
/// silently, wiring the wrong dependency with no parse error at all). Args
/// have no such ambiguity: each is delimited exactly by `parse_addr`'s own
/// grammar (mirrors this module's existing `os`/`arch` args on
/// [`thirdparty_addr`]).
///
/// **`local_name` and `resolved_name` are deliberately two different
/// strings, not one.** For an ordinary (non-aliased) dependency they're
/// equal, but an npm/pnpm alias (`"my-alias": "npm:real-pkg@1.0.0"`) makes
/// them diverge: `local_name` (`"my-alias"`) is what the consumer's own
/// `package.json`/import actually names — and therefore what
/// `require`/`import` will look for under `node_modules/` — while
/// `resolved_name` (`"real-pkg"`) is the published package `js_install`
/// actually downloads. Using `resolved_name` for the node_modules directory
/// name would place the files where nothing looks for them; using
/// `local_name` for the `js_install` dep would ask the registry for a
/// package that was never published. See [`parse_node_modules_addr`]'s doc
/// for the reverse operation.
pub fn node_modules_addr(
    consuming_pkg: &str,
    local_name: &str,
    resolved_name: &str,
    version: &str,
    os: &str,
    arch: &str,
) -> Addr {
    node_modules_addr_impl(
        consuming_pkg,
        local_name,
        resolved_name,
        version,
        os,
        arch,
        None,
    )
}

/// Like [`node_modules_addr`], but nested one level inside another
/// `node_modules_addr` relocation's own materialized directory
/// (`<consuming_pkg>/node_modules/<parent_local_name>/node_modules/<local_name>`)
/// — used for a depth-1 diamond-dependency override, when `resolved_name`'s
/// resolution as seen from `parent_local_name`'s own dependencies diverges
/// from what wins by default elsewhere in the same consuming package's
/// closure. See `lockfile::TransitiveEntry`'s doc for the full mechanism and
/// why nesting never goes deeper than one level; `parent_local_name` always
/// names a *flat* (non-nested) `node_modules_addr` — never another override.
pub fn nested_node_modules_addr(
    consuming_pkg: &str,
    parent_local_name: &str,
    local_name: &str,
    resolved_name: &str,
    version: &str,
    os: &str,
    arch: &str,
) -> Addr {
    node_modules_addr_impl(
        consuming_pkg,
        local_name,
        resolved_name,
        version,
        os,
        arch,
        Some(parent_local_name),
    )
}

/// Shared by [`node_modules_addr`]/[`nested_node_modules_addr`] — `nested_under:
/// None` produces an addr byte-identical to what [`node_modules_addr`]
/// always has, so the overwhelmingly common flat (no-conflict) case never
/// changes shape, hashes, or cache key across this addr gaining the nested
/// case at all.
fn node_modules_addr_impl(
    consuming_pkg: &str,
    local_name: &str,
    resolved_name: &str,
    version: &str,
    os: &str,
    arch: &str,
    nested_under: Option<&str>,
) -> Addr {
    let mut args = BTreeMap::new();
    // `pkg` is `""` for the workspace-root consuming package — a plain
    // empty `Addr` arg value, not a sentinel: `Addr`'s own `Display`
    // quotes an empty/special value (`pkg=""`) and `parse_addr` reads it
    // straight back, so this round-trips exactly like every other arg.
    args.insert("pkg".to_string(), consuming_pkg.to_string());
    args.insert("local".to_string(), local_name.to_string());
    args.insert("name".to_string(), resolved_name.to_string());
    args.insert("version".to_string(), version.to_string());
    args.insert("os".to_string(), os.to_string());
    args.insert("arch".to_string(), arch.to_string());
    if let Some(parent) = nested_under {
        args.insert("nested_under".to_string(), parent.to_string());
    }
    Addr::new(
        PkgBuf::from(NODE_MODULES_PKG),
        NODE_MODULES_TARGET.to_string(),
        args,
    )
}

/// One [`node_modules_addr`]/[`nested_node_modules_addr`], parsed back into
/// its parts — see those functions' docs for what each one means and why
/// `local_name`/`resolved_name` are kept distinct.
pub struct NodeModulesRelocation {
    pub consuming_pkg: String,
    pub local_name: String,
    pub resolved_name: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    /// `Some(parent_local_name)` for a [`nested_node_modules_addr`]
    /// (depth-1 diamond-dependency override); `None` for an ordinary flat
    /// [`node_modules_addr`].
    pub nested_under: Option<String>,
}

/// The inverse of [`node_modules_addr`]/[`nested_node_modules_addr`]. `None`
/// if `addr` isn't under [`NODE_MODULES_PKG`]/[`NODE_MODULES_TARGET`], or is
/// missing a required arg (a malformed/foreign addr that happens to share
/// the namespace — fail closed rather than guess).
pub fn parse_node_modules_addr(addr: &Addr) -> Option<NodeModulesRelocation> {
    if addr.package.as_str() != NODE_MODULES_PKG || addr.name != NODE_MODULES_TARGET {
        return None;
    }
    Some(NodeModulesRelocation {
        consuming_pkg: addr.args.get("pkg")?.clone(),
        local_name: addr.args.get("local")?.clone(),
        resolved_name: addr.args.get("name")?.clone(),
        version: addr.args.get("version")?.clone(),
        os: addr.args.get("os")?.clone(),
        arch: addr.args.get("arch")?.clone(),
        nested_under: addr.args.get("nested_under").cloned(),
    })
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
        assert_eq!(addr.args.get("os").map(String::as_str), Some("linux"));
        assert_eq!(addr.args.get("arch").map(String::as_str), Some("amd64"));
    }

    #[test]
    fn node_modules_addr_has_no_nested_under_arg() {
        let addr = node_modules_addr(
            "packages/a",
            "lodash",
            "lodash",
            "4.17.21",
            "linux",
            "amd64",
        );
        assert!(
            !addr.args.contains_key("nested_under"),
            "the flat (no-conflict) case must never carry this arg at all — its absence is what \
             keeps the flat case's addr, and therefore its cache key, byte-identical to before \
             this arg existed"
        );
    }

    /// Confirmed live: a diamond-dependency override's `parent_local_name`
    /// is itself a scoped package name (`@module-federation/vite`) — an
    /// earlier draft of this addr's own doc records a prior hermeticity
    /// incident from folding a variable part into one delimited string
    /// where a scoped name could ambiguously re-split. `nested_under` is its
    /// own dedicated `Addr` arg, not joined with anything, so a scoped
    /// parent name round-trips with zero ambiguity.
    #[test]
    fn nested_node_modules_addr_roundtrips_a_scoped_parent_name() {
        let addr = nested_node_modules_addr(
            "packages/a",
            "@module-federation/vite",
            "estree-walker",
            "estree-walker",
            "3.0.3",
            "linux",
            "amd64",
        );
        let parsed = parse_node_modules_addr(&addr).expect("parses back");
        assert_eq!(parsed.consuming_pkg, "packages/a");
        assert_eq!(parsed.local_name, "estree-walker");
        assert_eq!(parsed.resolved_name, "estree-walker");
        assert_eq!(parsed.version, "3.0.3");
        assert_eq!(
            parsed.nested_under.as_deref(),
            Some("@module-federation/vite")
        );
    }

    #[test]
    fn parse_node_modules_addr_flat_case_has_no_nested_under() {
        let addr = node_modules_addr(
            "packages/a",
            "lodash",
            "lodash",
            "4.17.21",
            "linux",
            "amd64",
        );
        let parsed = parse_node_modules_addr(&addr).expect("parses back");
        assert_eq!(parsed.nested_under, None);
    }
}
