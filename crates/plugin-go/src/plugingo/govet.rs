//! Provisioning of the `heph-govet` analysis/format driver binary.
//!
//! `go_lint` / `go_format` exec `heph-govet` — the `x/tools` unitchecker plus the
//! formatter backends, whose source lives in heph's own `tools/heph-govet`. Which
//! binary they exec is the provider's `govet` option: **an addr**, so the tool is
//! an ordinary target like any other.
//!
//! - **Default** — `//@heph/go/govet/<tag>:heph-govet`, a target this provider
//!   synthesizes ([`build_spec`]) on the built-in `http_fetch` driver: it
//!   downloads the `heph-govet_<goos>_<goarch>` asset published in heph release
//!   `<tag>` and verifies its SHA-256. `<tag>` is the release this plugin itself
//!   was built from ([`hcore::version::VERSION`]) — the CI run that built the
//!   plugin published the binary, so the two always agree. Consumers of the go
//!   plugin need nothing checked in: they have no `tools/heph-govet` to build.
//!
//! - **From source** — point `govet` at a build target instead, e.g.
//!   `govet = "//tools/heph-govet:build"` inside heph's own repo (what its tests
//!   and dev builds use — `v0.0.0-dev` has no release to download from). Any addr
//!   producing a single executable works.
//!
//! The URL is templated over the addr's args (`heph-govet_{goos}_{goarch}`), so
//! one target definition serves every host platform and each renders to its own
//! cache entry — see `hplugin_http::pluginhttp`.
//!
//! Integrity: the expected SHA-256 of each published asset is **baked into this
//! plugin at compile time** (`HEPH_GOVET_SHA256_<OS>_<ARCH>`, set by CI from the
//! binaries it just built — see [`baked_sha256`]), so there is nothing to
//! configure and a tampered asset fails the build closed. A locally-built plugin
//! has no baked checksum; pinning some other release's tag there fetches
//! unverified (the driver warns) unless a `checksums` entry supplies one (see
//! [`checksum_key`]).

use hcore::htvalue::Value;
use hmodel::htaddr::Addr;
use hplugin::provider::TargetSpec;
use std::collections::HashMap;

// Only the test-only `govet_addr` builds an addr from scratch; production resolves
// it from the `govet` option instead (see `ProviderInner::govet_tool_addr`).
#[cfg(test)]
use crate::plugingo::factors::{current_goarch, current_goos};
#[cfg(test)]
use hmodel::htpkg::PkgBuf;

/// The dev-build version stamped into [`hcore::version::VERSION`] when
/// `HEPH_BUILD_VERSION` is unset. No release carries this tag, so a dev build has
/// no `heph-govet` asset to download and must point `govet` at a source build.
const DEV_VERSION: &str = "v0.0.0-dev";

/// Base URL of the published release artifacts (the same release the heph binary
/// and the plugin cdylibs ship in).
const ARTIFACTS_BASE: &str = "https://github.com/hephbuild/heph-artifacts-v1/releases/download";

/// Driver backing the default (download) govet target: the always-on built-in
/// `http_fetch` (`hplugin_http::pluginhttp`). Named, not imported: this plugin is
/// built as a standalone cdylib and resolves the driver through the engine's
/// registry, exactly as a BUILD file would.
const HTTP_FETCH_DRIVER: &str = "http_fetch";

/// Base provider package for the downloaded binary. The concrete target lives at
/// `{GOVET_PKG_PREFIX}/<tag>` (e.g. `@heph/go/govet/v0.1.234`).
pub const GOVET_PKG_PREFIX: &str = "@heph/go/govet";
/// Target (and staged file) name of the analysis driver binary.
pub const GOVET_NAME: &str = "heph-govet";

/// The addr the `govet` option defaults to: this plugin's own release download
/// target. On a dev build the tag is `v0.0.0-dev`, which no release publishes —
/// heph's own repo overrides the option with `//tools/heph-govet:build`. Resolving
/// the dev target fails with that fix in the message (see [`is_dev_tag`]) rather
/// than 404-ing mid-build.
pub fn default_addr() -> String {
    format!("//{}:{GOVET_NAME}", govet_pkg(hcore::version::VERSION))
}

/// Whether `tag` is the dev-build version — a release that was never published, so
/// it has no `heph-govet` asset. The provider surfaces this when the govet target
/// is *resolved* (not when a lint spec merely names it, which must keep working on
/// a dev build so bulk spec walks do), telling the user to point `govet` at a
/// source build.
pub fn is_dev_tag(tag: &str) -> bool {
    tag == DEV_VERSION
}

/// Release asset name for `(goos, goarch)`, e.g. `heph-govet_darwin_arm64`.
pub fn asset_name(goos: &str, goarch: &str) -> String {
    format!("{GOVET_NAME}_{goos}_{goarch}")
}

/// Download URL *template* of the `heph-govet` binary published in release `tag`.
/// `{goos}` / `{goarch}` are rendered by the `http_fetch` driver from the target
/// addr's args, so one target serves every host platform.
pub fn url_template(tag: &str) -> String {
    format!(
        "{ARTIFACTS_BASE}/{tag}/{}",
        asset_name("{goos}", "{goarch}")
    )
}

/// Lookup key for a `heph-govet` binary SHA-256 in the provider's `checksums`
/// config map: `"govet/<tag>/<goos>/<goarch>"`. Namespaced against the Go SDK
/// keys (`"<version>/<goos>/<goarch>"`) that share the map. Only needed to pin a
/// release other than this plugin's own — that one's checksums are baked in (see
/// [`baked_sha256`]).
pub fn checksum_key(tag: &str, goos: &str, goarch: &str) -> String {
    format!("govet/{tag}/{goos}/{goarch}")
}

/// SHA-256 of the `heph-govet` asset for `(goos, goarch)` in *this build's own*
/// release, stamped at compile time by CI (which hashes the binaries it just
/// built, before compiling the plugin against them). Empty when unset — a
/// locally-built plugin, which matches no release anyway.
pub fn baked_sha256(goos: &str, goarch: &str) -> &'static str {
    match (goos, goarch) {
        ("linux", "amd64") => option_env!("HEPH_GOVET_SHA256_LINUX_AMD64"),
        ("linux", "arm64") => option_env!("HEPH_GOVET_SHA256_LINUX_ARM64"),
        ("darwin", "amd64") => option_env!("HEPH_GOVET_SHA256_DARWIN_AMD64"),
        ("darwin", "arm64") => option_env!("HEPH_GOVET_SHA256_DARWIN_ARM64"),
        _ => None,
    }
    .unwrap_or("")
}

/// Expected SHA-256 for `tag` on `(goos, goarch)`: an explicit `checksums` entry
/// wins (it can pin a release this build knows nothing about), else the checksum
/// baked in for this build's own release, else empty → fetched unverified (the
/// driver warns).
pub fn expected_sha256(
    checksums: &HashMap<String, String>,
    tag: &str,
    goos: &str,
    goarch: &str,
) -> String {
    match checksums.get(&checksum_key(tag, goos, goarch)) {
        Some(sha) => sha.clone(),
        None => baked_sha256(goos, goarch).to_string(),
    }
}

/// Provider package holding the binary for `tag`, e.g. `@heph/go/govet/v0.1.234`.
pub fn govet_pkg(tag: &str) -> String {
    format!("{GOVET_PKG_PREFIX}/{tag}")
}

/// Parse the release tag out of a govet package path, or `None` if `pkg` is not
/// one. Unlike the toolchain package there is no bare-prefix default: the tag is
/// always explicit (it is the plugin's own version).
pub fn tag_from_pkg(pkg: &str) -> Option<&str> {
    let rest = pkg.strip_prefix(GOVET_PKG_PREFIX)?.strip_prefix('/')?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(rest)
}

/// The `//@heph/go/govet/<tag>:heph-govet` addr for the host platform. The binary
/// runs on (and is keyed by) the *host*: it analyzes code for any GOOS/GOARCH but
/// always executes natively, so the analyzed target's factors never enter here.
/// Args are the go factor args (`goos`/`goarch`) the rest of the plugin uses —
/// the URL template renders from them.
///
/// Production resolves the tool through the `govet` option instead (an addr
/// string, host factors added by `ProviderInner::govet_tool_addr`); this is the
/// same addr, built directly, for tests.
#[cfg(test)]
pub fn govet_addr(tag: &str) -> Addr {
    Addr::new(
        PkgBuf::from(govet_pkg(tag)),
        GOVET_NAME.to_string(),
        std::collections::BTreeMap::from([
            ("goos".to_string(), current_goos()),
            ("goarch".to_string(), current_goarch()),
        ]),
    )
}

/// Build the `TargetSpec` of the `heph-govet` download target for `tag`: an
/// `http_fetch` over the release asset, marked executable (the lint/format
/// drivers exec it). `sha256` is what the caller resolved via [`expected_sha256`]
/// for this addr's platform (empty → unverified).
pub fn build_spec(addr: Addr, tag: &str, sha256: &str) -> TargetSpec {
    let config = HashMap::from([
        ("url".to_string(), Value::String(url_template(tag))),
        ("sha256".to_string(), Value::String(sha256.to_string())),
        ("out".to_string(), Value::String(GOVET_NAME.to_string())),
        ("executable".to_string(), Value::Bool(true)),
    ]);

    TargetSpec {
        addr,
        driver: HTTP_FETCH_DRIVER.to_string(),
        config,
        labels: vec!["go-govet".to_string()],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_template_renders_platform_from_addr_args() {
        // The driver substitutes {goos}/{goarch} from the addr args — one target
        // definition, one asset per host.
        assert_eq!(
            url_template("v0.1.234"),
            "https://github.com/hephbuild/heph-artifacts-v1/releases/download/v0.1.234/heph-govet_{goos}_{goarch}"
        );
    }

    #[test]
    fn test_checksum_key_is_namespaced_against_sdk_keys() {
        // The `checksums` map is shared with the Go SDK, whose keys are
        // `<version>/<goos>/<goarch>` — the `govet/` prefix keeps them disjoint.
        assert_eq!(
            checksum_key("v0.1.234", "linux", "amd64"),
            "govet/v0.1.234/linux/amd64"
        );
    }

    #[test]
    fn test_expected_sha256_prefers_configured_over_baked() {
        let checksums =
            HashMap::from([(checksum_key("v1.0.0", "linux", "amd64"), "abc".to_string())]);
        assert_eq!(
            expected_sha256(&checksums, "v1.0.0", "linux", "amd64"),
            "abc"
        );
        // Unknown tag/platform falls back to the baked checksum (empty in a local
        // build, which is what the test binary is).
        assert_eq!(
            expected_sha256(&checksums, "v2.0.0", "linux", "amd64"),
            baked_sha256("linux", "amd64")
        );
    }

    #[test]
    fn test_tag_from_pkg() {
        assert_eq!(tag_from_pkg("@heph/go/govet/v0.1.234"), Some("v0.1.234"));
        // No bare-prefix default, and no nesting.
        assert_eq!(tag_from_pkg("@heph/go/govet"), None);
        assert_eq!(tag_from_pkg("@heph/go/govet/v1/extra"), None);
        assert_eq!(tag_from_pkg("mylib"), None);
    }

    #[test]
    fn test_govet_addr_carries_tag_and_host_factors() {
        let addr = govet_addr("v0.1.234");
        assert_eq!(addr.package.as_str(), "@heph/go/govet/v0.1.234");
        assert_eq!(addr.name, GOVET_NAME);
        // Go factor args, so host injection is uniform with the source-build addr
        // — and they are what the URL template renders from.
        assert_eq!(addr.args.get("goos"), Some(&current_goos()));
        assert_eq!(addr.args.get("goarch"), Some(&current_goarch()));
    }

    #[test]
    fn test_default_addr_is_this_builds_release_download_target() {
        let addr = hmodel::htaddr::parse_addr(&default_addr()).expect("parse default addr");
        assert_eq!(addr.name, GOVET_NAME);
        assert_eq!(
            tag_from_pkg(addr.package.as_str()),
            Some(hcore::version::VERSION)
        );
        // The test binary is never built from a release, so the default addr is
        // the (nonexistent) dev tag — resolving it must surface that, not 404.
        assert!(is_dev_tag(
            tag_from_pkg(addr.package.as_str()).expect("tag")
        ));
    }

    #[test]
    fn test_build_spec_is_an_executable_http_fetch() {
        let spec = build_spec(govet_addr("v1.0.0"), "v1.0.0", "deadbeef");
        assert_eq!(spec.driver, "http_fetch");
        assert!(matches!(
            spec.config.get("url"),
            Some(Value::String(u)) if u.contains("/v1.0.0/heph-govet_{goos}_{goarch}")
        ));
        assert!(matches!(
            spec.config.get("sha256"),
            Some(Value::String(s)) if s == "deadbeef"
        ));
        assert!(matches!(
            spec.config.get("out"),
            Some(Value::String(o)) if o == GOVET_NAME
        ));
        assert_eq!(spec.config.get("executable"), Some(&Value::Bool(true)));
    }
}
