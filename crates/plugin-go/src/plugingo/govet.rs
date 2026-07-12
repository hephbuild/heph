//! Provisioning of the `heph-govet` analysis/format driver binary.
//!
//! `go_lint` / `go_format` shell out to `heph-govet` — the `x/tools` unitchecker
//! (plus the formatter backends) built from `tools/heph-govet`. That binary is
//! *heph's own*, not the user's: a workspace consuming the go plugin has no
//! `tools/heph-govet` package to build it from. So the plugin provisions it the
//! same way it provisions the Go SDK: a synthesized
//! `//@heph/go/govet/<tag>:heph-govet` target that downloads the prebuilt binary
//! published alongside the heph release this plugin was built from, verifies its
//! SHA-256, and exposes it as one cacheable file output.
//!
//! Which binary is chosen ([`Config::govet`](crate::plugingo::Config)):
//!
//! - unset on a released build → the release matching this plugin's own
//!   [`hcore::version::VERSION`] — the CI run that built this plugin published
//!   `heph-govet_<os>_<arch>` in the same release, so the two always agree.
//! - `govet = "<tag>"` → that release's binary (pin/override).
//! - `govet = "source"` ([`SOURCE`]) → build `//tools/heph-govet:build` from the
//!   workspace instead of downloading. Only usable inside the heph repo itself;
//!   it is the default for dev builds (no release exists for `v0.0.0-dev`).
//!
//! Integrity: the expected SHA-256 of each published binary is **baked into this
//! plugin at compile time** (`HEPH_GOVET_SHA256_<OS>_<ARCH>`, set by CI from the
//! binaries it just built — see [`baked_sha256`]). Nothing to configure, and a
//! tampered release asset fails the build. A locally-built plugin has no baked
//! checksum, so an explicitly-pinned `govet = "<tag>"` there downloads unverified
//! (the driver warns) unless a `checksums` entry supplies one (see
//! [`checksum_key`]).

use crate::plugingo::factors::{current_goarch, current_goos};
use anyhow::Context;
use async_trait::async_trait;
use hcore::debug_hash::DebugHasher;
use hcore::hasync::Cancellable;
use hcore::htvalue::Value;
use hdriver_support::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use hmodel::htaddr::Addr;
use hmodel::htpkg::PkgBuf;
use hplugin::driver::targetdef::path::{CodegenMode, Content, Path};
use hplugin::driver::targetdef::{CacheConfig, Output, TargetDef};
use hplugin::driver::{
    ApplyTransitiveRequest, ApplyTransitiveResponse, ConfigRequest, ConfigResponse, ParseRequest,
    ParseResponse,
};
use hplugin::htspec::Spec;
use hplugin::provider::TargetSpec;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3Default;

/// Sentinel `govet` spec selecting a from-source build of `//tools/heph-govet:build`
/// instead of a release download. Only resolvable inside the heph repo (that
/// package exists nowhere else); the default for dev builds.
pub const SOURCE: &str = "source";

/// The dev-build version stamped into [`hcore::version::VERSION`] when
/// `HEPH_BUILD_VERSION` is unset. No release carries this tag, so there is no
/// `heph-govet` asset to download for it.
const DEV_VERSION: &str = "v0.0.0-dev";

/// Base URL of the published release artifacts (same release the heph binary and
/// the plugin cdylibs are published in).
const ARTIFACTS_BASE: &str = "https://github.com/hephbuild/heph-artifacts-v1/releases/download";

/// Base provider package for the downloaded binary. The concrete target lives at
/// `{GOVET_PKG_PREFIX}/<tag>` (e.g. `@heph/go/govet/v0.1.234`).
pub const GOVET_PKG_PREFIX: &str = "@heph/go/govet";
/// Target (and staged file) name of the analysis driver binary.
pub const GOVET_NAME: &str = "heph-govet";
/// Driver name registered for the download.
pub const GOVET_DRIVER: &str = "go_govet";

/// The `govet` spec a provider defaults to when the option is unset: this
/// plugin's own release tag, or [`SOURCE`] for a dev build (no release exists to
/// download from, and a dev build is by construction a heph-repo checkout).
pub fn default_spec() -> String {
    if hcore::version::VERSION == DEV_VERSION {
        SOURCE.to_string()
    } else {
        hcore::version::VERSION.to_string()
    }
}

/// Whether `spec` selects the from-source build rather than a release download.
pub fn is_source(spec: &str) -> bool {
    spec == SOURCE
}

/// Release asset name for `(goos, goarch)`, e.g. `heph-govet_darwin_arm64`.
pub fn asset_name(goos: &str, goarch: &str) -> String {
    format!("{GOVET_NAME}_{goos}_{goarch}")
}

/// Download URL of the `heph-govet` binary published in release `tag`.
pub fn govet_url(tag: &str, goos: &str, goarch: &str) -> String {
    format!("{ARTIFACTS_BASE}/{tag}/{}", asset_name(goos, goarch))
}

/// Lookup key for a `heph-govet` binary SHA-256 in the provider's `checksums`
/// config map: `"govet/<tag>/<goos>/<goarch>"`. Namespaced against the Go SDK
/// keys (`"<version>/<goos>/<goarch>"`) that share the map. Only needed to pin a
/// tag other than the plugin's own — the matching release's checksums are baked
/// into the binary (see [`baked_sha256`]).
pub fn checksum_key(tag: &str, goos: &str, goarch: &str) -> String {
    format!("govet/{tag}/{goos}/{goarch}")
}

/// SHA-256 of the `heph-govet` asset for `(goos, goarch)` in *this build's own*
/// release, stamped at compile time by CI (which hashes the binaries it just
/// built, before compiling the plugin against them). Empty when unset — a
/// locally-built plugin, where no release matches this source tree anyway.
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
/// wins (it can pin a tag this build knows nothing about), else the checksum
/// baked in for this build's own release, else empty → unverified (the driver
/// warns). A `checksums` entry for the plugin's own tag simply agrees with the
/// baked value.
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

/// The `//@heph/go/govet/<tag>:heph-govet` address. The binary runs on (and is
/// keyed by) the *host* platform — it analyzes code for any GOOS/GOARCH but
/// always executes natively, so the analyzed target's factors never enter here.
pub fn govet_addr(tag: &str) -> Addr {
    Addr::new(
        PkgBuf::from(govet_pkg(tag)),
        GOVET_NAME.to_string(),
        std::collections::BTreeMap::from([
            ("os".to_string(), current_goos()),
            ("arch".to_string(), current_goarch()),
        ]),
    )
}

/// Build the `TargetSpec` for the `heph-govet` download target for `tag`.
/// `sha256` is the expected checksum the caller resolved via [`expected_sha256`]
/// (empty → unverified download).
pub fn build_spec(addr: Addr, tag: &str, goos: &str, goarch: &str, sha256: &str) -> TargetSpec {
    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("tag".to_string(), Value::String(tag.to_string()));
    config.insert("goos".to_string(), Value::String(goos.to_string()));
    config.insert("goarch".to_string(), Value::String(goarch.to_string()));
    config.insert("sha256".to_string(), Value::String(sha256.to_string()));
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(
            String::new(),
            Value::List(vec![Value::String(GOVET_NAME.to_string())]),
        )])),
    );

    TargetSpec {
        addr,
        driver: GOVET_DRIVER.to_string(),
        config,
        labels: vec!["go-govet".to_string()],
        ..Default::default()
    }
}

/// Config for a `go_govet` target (engine-generated by the Go provider).
#[derive(Spec)]
struct GoGovetSpec {
    /// heph release tag the binary is published in, e.g. `v0.1.234`.
    #[spec(required)]
    tag: String,
    /// Host GOOS the binary runs on.
    #[spec(required)]
    goos: String,
    /// Host GOARCH the binary runs on.
    #[spec(required)]
    goarch: String,
    /// Expected SHA-256 of the downloaded binary (hex). Empty = download
    /// unverified.
    #[spec(required)]
    sha256: String,
    /// Declared outputs, grouped by name → list of output paths.
    out: HashMap<String, Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GoGovetDef {
    tag: String,
    goos: String,
    goarch: String,
    sha256: String,
}

/// Bump to invalidate cached `heph-govet` artifacts when the download layout
/// changes.
const GO_GOVET_FORMAT_VERSION: u32 = 1;

impl Hash for GoGovetDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        GO_GOVET_FORMAT_VERSION.hash(state);
        self.tag.hash(state);
        self.goos.hash(state);
        self.goarch.hash(state);
        self.sha256.hash(state);
    }
}

pub struct GoGovetDriver;

#[async_trait]
impl ManagedDriver for GoGovetDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: GOVET_DRIVER.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        GoGovetSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let pkg = req.target_spec.addr.package.clone();
        let pkg_str = pkg.as_str();

        let spec =
            GoGovetSpec::from(req.target_spec.config.clone()).context("parse go_govet config")?;

        let def = GoGovetDef {
            tag: spec.tag,
            goos: spec.goos,
            goarch: spec.goarch,
            sha256: spec.sha256,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("go_govet_{}", req.target_spec.addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        let outputs = spec
            .out
            .iter()
            .map(|(group, paths)| Output {
                group: group.clone(),
                paths: paths
                    .iter()
                    .map(|p| {
                        let full = if pkg_str.is_empty() {
                            p.clone()
                        } else {
                            format!("{pkg_str}/{p}")
                        };
                        Path {
                            content: Content::FilePath(full),
                            codegen_tree: CodegenMode::None,
                            collect: true,
                        }
                    })
                    .collect(),
            })
            .collect();

        Ok(ParseResponse {
            target_def: TargetDef {
                addr: req.target_spec.addr.clone(),
                labels: req.target_spec.labels.clone(),
                raw_def: Arc::new(def),
                // No inputs: fetched from the network, not from other targets or
                // the host filesystem.
                inputs: vec![],
                outputs,
                support_files: vec![],
                cache: CacheConfig::on(true),
                pty: false,
                hash,
                transparent: false,
            },
        })
    }

    async fn apply_transitive(
        &self,
        req: ApplyTransitiveRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ApplyTransitiveResponse> {
        Ok(ApplyTransitiveResponse {
            target_def: req.target_def,
        })
    }

    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let def = req.request.target.def_de::<GoGovetDef>();
        let url = govet_url(&def.tag, &def.goos, &def.goarch);
        let dest = req.sandbox_pkg_dir.join(GOVET_NAME);
        let expected = def.sha256.clone();
        let tag = def.tag.clone();

        // Download + verify + write is blocking IO; keep it off the async runtime.
        // Cancellation is honored by racing the join handle against the token.
        let work =
            tokio::task::spawn_blocking(move || download_verify(&url, &expected, &tag, &dest));

        let fetch = async {
            work.await
                .context("heph-govet download task panicked")?
                .with_context(|| format!("provision heph-govet {}", def.tag))
        };

        tokio::select! {
            r = fetch => r?,
            () = ctoken.cancelled() => anyhow::bail!("heph-govet download cancelled"),
        }

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// Download `url`, verify it against `expected_sha256`, and write it to `dest`
/// as an executable. Pure blocking work.
fn download_verify(
    url: &str,
    expected_sha256: &str,
    tag: &str,
    dest: &std::path::Path,
) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .context("build http client")?;
    let bytes = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| {
            format!(
                "download {url} — no heph-govet asset for release {tag} on this platform; pin a \
                 released `govet = \"<tag>\"` or use `govet = \"source\"`"
            )
        })?
        .bytes()
        .with_context(|| format!("read body of {url}"))?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got = format!("{:x}", hasher.finalize());
    verify_checksum(expected_sha256, &got, tag, url)?;

    write_executable(dest, &bytes).with_context(|| format!("write heph-govet to {dest:?}"))
}

/// Write `bytes` to `dest` with the executable bit set — the lint/format drivers
/// exec this file directly.
fn write_executable(dest: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::write(dest, bytes).context("write file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .context("chmod 0755")?;
    }
    Ok(())
}

/// Compare the downloaded binary's `got` SHA-256 against the `expected` one. An
/// empty `expected` (a locally-built plugin with no baked checksum, pinning a tag
/// with no `checksums` entry) downloads **unverified** — allowed, but warned,
/// since it drops the supply-chain guarantee. A mismatch fails closed.
fn verify_checksum(expected: &str, got: &str, tag: &str, url: &str) -> anyhow::Result<()> {
    if expected.is_empty() {
        tracing::warn!(
            tag,
            url,
            "downloading heph-govet {tag} without checksum verification — this plugin has no \
             checksum baked in for this platform (locally-built?) and no `checksums` entry \
             (`{}`) is configured",
            checksum_key(tag, "<goos>", "<goarch>")
        );
        return Ok(());
    }
    if got != expected {
        anyhow::bail!(
            "heph-govet {tag} checksum mismatch for {url}: expected {expected}, got {got}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_govet_url_format() {
        assert_eq!(
            govet_url("v0.1.234", "darwin", "arm64"),
            "https://github.com/hephbuild/heph-artifacts-v1/releases/download/v0.1.234/heph-govet_darwin_arm64"
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
        // Unknown tag/platform falls back to the baked checksum (empty in a
        // local build, which is what the test binary is).
        assert_eq!(
            expected_sha256(&checksums, "v2.0.0", "linux", "amd64"),
            baked_sha256("linux", "amd64")
        );
    }

    #[test]
    fn test_verify_checksum_empty_expected_skips() {
        assert!(verify_checksum("", "anything", "v1.0.0", "http://x").is_ok());
    }

    #[test]
    fn test_verify_checksum_match_ok_mismatch_fails() {
        assert!(verify_checksum("abc", "abc", "v1.0.0", "http://x").is_ok());
        let err = verify_checksum("abc", "def", "v1.0.0", "http://x").unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
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
    fn test_govet_addr_carries_tag_and_host_platform() {
        let addr = govet_addr("v0.1.234");
        assert_eq!(addr.package.as_str(), "@heph/go/govet/v0.1.234");
        assert_eq!(addr.name, GOVET_NAME);
        assert_eq!(addr.args.get("os"), Some(&current_goos()));
        assert_eq!(addr.args.get("arch"), Some(&current_goarch()));
    }

    #[test]
    fn test_default_spec_is_source_on_dev_builds() {
        // The test binary is never built from a release, so the default must be
        // the from-source escape hatch (no release exists to download from).
        assert!(is_source(&default_spec()));
    }

    #[test]
    fn test_build_spec_declares_single_file_output_and_driver() {
        let spec = build_spec(govet_addr("v1.0.0"), "v1.0.0", "linux", "amd64", "deadbeef");
        assert_eq!(spec.driver, GOVET_DRIVER);
        assert!(matches!(
            spec.config.get("tag"),
            Some(Value::String(s)) if s == "v1.0.0"
        ));
        assert!(matches!(
            spec.config.get("sha256"),
            Some(Value::String(s)) if s == "deadbeef"
        ));
        let out = match spec.config.get("out").expect("out") {
            Value::Map(m) => m,
            other => panic!("out must be a map, got {other:?}"),
        };
        match out.get("").expect("default group") {
            Value::List(paths) => assert_eq!(paths, &[Value::String(GOVET_NAME.to_string())]),
            other => panic!("group must be a list, got {other:?}"),
        }
    }

    /// Serve `body` (or a 404 when `status` says so) to the next `n` requests on
    /// an ephemeral loopback port, and return its base URL. Exercises the real
    /// download path — reqwest, status handling, checksum — with no network.
    fn serve(status: &'static str, body: &'static [u8], n: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        std::thread::spawn(move || {
            for _ in 0..n {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                // Read the request head; the body is empty (GET).
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut sock, &mut buf);
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut sock, head.as_bytes());
                let _ = std::io::Write::write_all(&mut sock, body);
            }
        });
        url
    }

    const BIN: &[u8] = b"\x7fELF-pretend-this-is-heph-govet";
    /// sha256 of `BIN`.
    const BIN_SHA: &str = "c0b3d54bd9ba5373cd84fb62f3fe08532430080e43ba73040189364326d99e2c";

    #[test]
    fn test_download_verify_writes_verified_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join(GOVET_NAME);
        let url = serve("200 OK", BIN, 1);

        download_verify(&url, BIN_SHA, "v1.0.0", &dest).expect("download");

        assert_eq!(std::fs::read(&dest).expect("read"), BIN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).expect("stat").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "downloaded binary must be executable");
        }
    }

    #[test]
    fn test_download_verify_rejects_tampered_asset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join(GOVET_NAME);
        let url = serve("200 OK", b"tampered", 1);

        let err = download_verify(&url, BIN_SHA, "v1.0.0", &dest)
            .expect_err("checksum mismatch must fail the build");
        assert!(
            format!("{err:#}").contains("checksum mismatch"),
            "got: {err:#}"
        );
        assert!(!dest.exists(), "a mismatched asset must not be written");
    }

    #[test]
    fn test_download_verify_missing_asset_explains_the_fix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join(GOVET_NAME);
        let url = serve("404 Not Found", b"", 1);

        let err = download_verify(&url, BIN_SHA, "v1.0.0", &dest).expect_err("404 must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no heph-govet asset for release v1.0.0"),
            "got: {msg}"
        );
        assert!(
            msg.contains("source"),
            "must point at the escape hatch: {msg}"
        );
    }

    #[test]
    fn test_write_executable_sets_exec_bit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join(GOVET_NAME);
        write_executable(&dest, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).expect("stat").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "binary must be executable: {mode:o}");
        }
    }
}
