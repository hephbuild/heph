//! Hermetic Go toolchain provisioning.
//!
//! The Go provider no longer reads a `go` binary, `GOROOT`, or any `go env`
//! value from the host. Instead it synthesizes a `//@heph/go/toolchain/<version>:go`
//! target that downloads a *pinned* Go SDK tarball for the host platform from
//! `go.dev/dl`, verifies its SHA-256, and extracts the SDK tree as a cacheable
//! directory output. The version is configurable (the `goversion` provider
//! option, default [`DEFAULT_GO_VERSION`]). Every build/list/test target then
//! takes that SDK as a dependency and points `GOROOT` at the staged tree
//! (see [`addr_util::go_sdk_dep`] / [`addr_util::go_run_prelude`]).
//!
//! The SDK is one cacheable output (the full tree: `go` + `pkg/tool` + `lib` +
//! `src` + version/env metadata; `api/test/doc/misc` excluded — nothing reads
//! them). Consumers don't copy it: it is staged read-only once and exposed to
//! each sandbox via a directory symlink (`hdriver_support::stage`, opted in via
//! [`addr_util::go_sdk_read_only_config`]), so its size is irrelevant per
//! consumer — there is nothing to gain from a trimmed per-consumer subset.
//!
//! This is the in-Rust analogue of how the `v1` Go plugin builds the standard
//! library from source in-sandbox — except here the toolchain itself is also
//! hermetic, so the build depends on nothing host-installed.

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

/// Default Go release when no version is requested. Override per-build via the
/// `goversion` provider option or by addressing a versioned toolchain package
/// directly (`//@heph/go/toolchain/<version>:go`). Any requested version must
/// have checksums in [`SDK_CHECKSUMS`].
pub const DEFAULT_GO_VERSION: &str = "1.26.4";

/// Base provider package for the hermetic toolchain. The concrete target lives
/// at `{TOOLCHAIN_PKG_PREFIX}/<version>` (e.g. `@heph/go/toolchain/1.26.4`).
pub const TOOLCHAIN_PKG_PREFIX: &str = "@heph/go/toolchain";
pub const TOOLCHAIN_NAME: &str = "go";

/// Driver name registered for the toolchain download.
pub const TOOLCHAIN_DRIVER: &str = "go_toolchain";

/// Directory (relative to the toolchain target's package) the SDK extracts to.
/// The official tarball unpacks a top-level `go/`, so the SDK root — and thus
/// `GOROOT` for every consumer — is `$WORKSPACE_ROOT/{pkg}/go`.
pub const SDK_DIR: &str = "go";

/// Top-level GOROOT entries the SDK output exposes — everything a consumer ever
/// reads: the `go` command, the compiler/linker (`pkg/tool`), `lib/`, the std
/// sources (`src/`, needed by `go list` and std-from-source), and the version/
/// env metadata. Paths are relative to the toolchain package; a trailing slash
/// marks a directory tree. `api/`, `test/`, `doc/`, `misc/` are deliberately
/// omitted — no consumer reads them, so they never reach the stage.
///
/// There is a single output group: the SDK is staged read-only and exposed to
/// each sandbox via a directory symlink (see `hdriver_support::stage`), so its
/// size is irrelevant to consumers — there is nothing to gain from trimming a
/// per-consumer subset (an earlier `tool`-vs-full split did, back when the SDK
/// was byte-copied into every sandbox).
fn toolchain_entries() -> Vec<String> {
    ["bin/", "pkg/", "lib/", "src/", "go.env", "VERSION"]
        .iter()
        .map(|e| format!("{SDK_DIR}/{e}"))
        .collect()
}

/// SHA-256 of `go{version}.{goos}-{goarch}.tar.gz`, keyed by `(version, goos,
/// goarch)`. Sourced from <https://go.dev/dl/?mode=json>. To support a new Go
/// version, add its four rows here; a missing checksum fails the build closed
/// (no silent unverified download).
const SDK_CHECKSUMS: &[(&str, &str, &str, &str)] = &[
    (
        "1.26.4",
        "linux",
        "amd64",
        "1153d3d50e0ac764b447adfe05c2bcf08e889d42a02e0fe0259bd47f6733ad7f",
    ),
    (
        "1.26.4",
        "linux",
        "arm64",
        "ef758ae7c6cf9267c9c0ef080b8965f453d89ab2d25d9eb22de4405925238768",
    ),
    (
        "1.26.4",
        "darwin",
        "amd64",
        "05dc9b5f9997744520aaebb3d5deaa7c755371aebbfb7f97c2511a9f3367538d",
    ),
    (
        "1.26.4",
        "darwin",
        "arm64",
        "b62ad2b6d7d2464f12a5bcad7ff47f19d08325773b5efd21610e445a05a9bf53",
    ),
];

/// SHA-256 for `version` on `(goos, goarch)`, or `None` if unsupported.
pub fn sdk_checksum(version: &str, goos: &str, goarch: &str) -> Option<&'static str> {
    SDK_CHECKSUMS
        .iter()
        .find(|(v, os, arch, _)| *v == version && *os == goos && *arch == goarch)
        .map(|(_, _, _, sum)| *sum)
}

/// Tarball download URL for `version` on `(goos, goarch)`.
pub fn sdk_url(version: &str, goos: &str, goarch: &str) -> String {
    format!("https://go.dev/dl/go{version}.{goos}-{goarch}.tar.gz")
}

/// Provider package holding the toolchain for `version`,
/// e.g. `@heph/go/toolchain/1.26.4`.
pub fn toolchain_pkg(version: &str) -> String {
    format!("{TOOLCHAIN_PKG_PREFIX}/{version}")
}

/// Parse the Go version out of a toolchain package path, or `None` if `pkg` is
/// not a toolchain package. `@heph/go/toolchain/1.26.4` → `Some("1.26.4")`; the
/// bare `@heph/go/toolchain` → `Some(DEFAULT_GO_VERSION)`. Rejects nested paths.
pub fn version_from_pkg(pkg: &str) -> Option<&str> {
    if pkg == TOOLCHAIN_PKG_PREFIX {
        return Some(DEFAULT_GO_VERSION);
    }
    let rest = pkg.strip_prefix(TOOLCHAIN_PKG_PREFIX)?.strip_prefix('/')?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    Some(rest)
}

/// The `//@heph/go/toolchain/<version>:go` address. The toolchain runs on (and
/// is keyed by) the *host* platform — Go cross-compiles to any GOOS/GOARCH from
/// one SDK, so target factors never enter here.
pub fn toolchain_addr(version: &str) -> Addr {
    Addr::new(
        PkgBuf::from(toolchain_pkg(version)),
        TOOLCHAIN_NAME.to_string(),
        std::collections::BTreeMap::from([
            ("os".to_string(), current_goos()),
            ("arch".to_string(), current_goarch()),
        ]),
    )
}

/// Workspace-relative path the SDK for `version` is staged at in every consumer
/// sandbox, i.e. the value of `GOROOT`. Per-version so multiple toolchains can
/// coexist in one build graph without colliding.
pub fn staged_goroot(version: &str) -> String {
    format!("{}/{SDK_DIR}", toolchain_pkg(version))
}

/// Build the `TargetSpec` for the toolchain download target for `version`.
/// `host_goos` / `host_goarch` are the platform the SDK runs on; the checksum
/// must exist for that triple (else a clear error).
pub fn build_spec(
    addr: Addr,
    version: &str,
    host_goos: &str,
    host_goarch: &str,
) -> anyhow::Result<TargetSpec> {
    let sha256 = sdk_checksum(version, host_goos, host_goarch).ok_or_else(|| {
        anyhow::anyhow!(
            "no pinned Go {version} SDK for host platform {host_goos}/{host_goarch}; \
             add its checksum to plugingo::toolchain::SDK_CHECKSUMS"
        )
    })?;

    let mut config: HashMap<String, Value> = HashMap::new();
    config.insert("version".to_string(), Value::String(version.to_string()));
    config.insert("goos".to_string(), Value::String(host_goos.to_string()));
    config.insert("goarch".to_string(), Value::String(host_goarch.to_string()));
    config.insert("sha256".to_string(), Value::String(sha256.to_string()));
    // Single curated output group over the extracted SDK tree (everything a
    // consumer reads; api/test/doc/misc excluded). Consumers symlink it in
    // read-only, so there is no per-consumer copy to trim.
    let entries = Value::List(toolchain_entries().into_iter().map(Value::String).collect());
    config.insert(
        "out".to_string(),
        Value::Map(HashMap::from([(String::new(), entries)])),
    );

    Ok(TargetSpec {
        addr,
        driver: TOOLCHAIN_DRIVER.to_string(),
        config,
        labels: vec!["go-toolchain".to_string()],
        transitive: Default::default(),
    })
}

/// Config for a `go_toolchain` target (engine-generated by the Go provider).
#[derive(Spec)]
struct GoToolchainSpec {
    /// Pinned Go release, e.g. `1.26.4`.
    #[spec(required)]
    version: String,
    /// Host GOOS the SDK runs on.
    #[spec(required)]
    goos: String,
    /// Host GOARCH the SDK runs on.
    #[spec(required)]
    goarch: String,
    /// Expected SHA-256 of the downloaded tarball (hex).
    #[spec(required)]
    sha256: String,
    /// Declared outputs, grouped by name → list of output paths.
    out: HashMap<String, Vec<String>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct GoToolchainDef {
    version: String,
    goos: String,
    goarch: String,
    sha256: String,
}

/// Bump to invalidate cached toolchain artifacts when the extract layout or
/// output-group partitioning changes. v2: split into `""`/`"tool"` groups.
const GO_TOOLCHAIN_FORMAT_VERSION: u32 = 3;

impl Hash for GoToolchainDef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        GO_TOOLCHAIN_FORMAT_VERSION.hash(state);
        self.version.hash(state);
        self.goos.hash(state);
        self.goarch.hash(state);
        self.sha256.hash(state);
    }
}

pub struct GoToolchainDriver;

#[async_trait]
impl ManagedDriver for GoToolchainDriver {
    fn config(&self, _req: ConfigRequest) -> anyhow::Result<ConfigResponse> {
        Ok(ConfigResponse {
            name: TOOLCHAIN_DRIVER.to_string(),
        })
    }

    fn schema(&self) -> hplugin::driver::DriverSchema {
        GoToolchainSpec::schema()
    }

    async fn parse(
        &self,
        req: ParseRequest,
        _ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<ParseResponse> {
        let pkg = req.target_spec.addr.package.clone();
        let pkg_str = pkg.as_str();

        let spec = GoToolchainSpec::from(req.target_spec.config.clone())
            .context("parse go_toolchain config")?;

        let def = GoToolchainDef {
            version: spec.version,
            goos: spec.goos,
            goarch: spec.goarch,
            sha256: spec.sha256,
        };

        let hash = {
            let mut h = DebugHasher::new(Xxh3Default::new(), || {
                format!("go_toolchain_{}", req.target_spec.addr.format())
            });
            def.hash(&mut h);
            format!("{:x}", h.finish()).into_bytes()
        };

        // Output paths are workspace-relative: prepend the owning package, and
        // classify a trailing-slash entry as a directory tree (the SDK root).
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
                        let content = if let Some(dir) = full.strip_suffix('/') {
                            Content::DirPath(dir.to_string())
                        } else {
                            Content::FilePath(full)
                        };
                        Path {
                            content,
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
                // No inputs: the SDK is fetched from the network, not from
                // other targets or the host filesystem.
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
        let def = req.request.target.def_de::<GoToolchainDef>();
        let url = sdk_url(&def.version, &def.goos, &def.goarch);
        let dest = req.sandbox_pkg_dir.clone();
        let expected = def.sha256.clone();
        let version = def.version.clone();

        // Download + verify + extract is blocking IO/CPU; keep it off the async
        // runtime. Cancellation is honored by racing the join handle against the
        // token (the blocking work itself can't be interrupted mid-syscall, but
        // we stop awaiting it promptly on cancel).
        let work = tokio::task::spawn_blocking(move || {
            download_verify_extract(&url, &expected, &version, &dest)
        });

        let extract = async {
            work.await
                .context("go toolchain download task panicked")?
                .with_context(|| format!("provision Go {} SDK", def.version))
        };

        tokio::select! {
            r = extract => r?,
            () = ctoken.cancelled() => anyhow::bail!("go toolchain download cancelled"),
        }

        Ok(ManagedRunResponse { artifacts: vec![] })
    }
}

/// Download `url`, verify it against `expected_sha256`, and extract the SDK tree
/// into `dest` (producing `dest/go/...`). Pure blocking work.
fn download_verify_extract(
    url: &str,
    expected_sha256: &str,
    version: &str,
    dest: &std::path::Path,
) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .context("build http client")?;
    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("download {url}"))?;
    let bytes = resp
        .bytes()
        .with_context(|| format!("read body of {url}"))?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got = format!("{:x}", hasher.finalize());
    if got != expected_sha256 {
        anyhow::bail!(
            "Go {version} SDK checksum mismatch for {url}: expected {expected_sha256}, got {got}"
        );
    }

    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes.as_ref()));
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    archive
        .unpack(dest)
        .with_context(|| format!("extract Go {version} SDK into {dest:?}"))?;

    // Sanity-check the layout we promise downstream consumers.
    let go_bin = dest.join(SDK_DIR).join("bin").join("go");
    if !go_bin.exists() {
        anyhow::bail!("extracted Go SDK is missing {go_bin:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checksums_present_for_supported_platforms() {
        for (os, arch) in [
            ("linux", "amd64"),
            ("linux", "arm64"),
            ("darwin", "amd64"),
            ("darwin", "arm64"),
        ] {
            let sum = sdk_checksum(DEFAULT_GO_VERSION, os, arch).expect("checksum present");
            assert_eq!(sum.len(), 64, "sha256 must be 64 hex chars for {os}/{arch}");
            assert!(sum.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn test_checksum_absent_for_unsupported_platform_or_version() {
        assert!(sdk_checksum(DEFAULT_GO_VERSION, "plan9", "386").is_none());
        assert!(sdk_checksum("0.0.0", "linux", "amd64").is_none());
    }

    #[test]
    fn test_sdk_url_format() {
        assert_eq!(
            sdk_url("1.25.0", "linux", "amd64"),
            "https://go.dev/dl/go1.25.0.linux-amd64.tar.gz"
        );
    }

    #[test]
    fn test_version_from_pkg() {
        assert_eq!(
            version_from_pkg("@heph/go/toolchain/1.26.4"),
            Some("1.26.4")
        );
        // Bare package defaults.
        assert_eq!(
            version_from_pkg("@heph/go/toolchain"),
            Some(DEFAULT_GO_VERSION)
        );
        // Not a toolchain package, or nested.
        assert_eq!(version_from_pkg("mylib"), None);
        assert_eq!(version_from_pkg("@heph/go/toolchain/1.26.4/extra"), None);
    }

    #[test]
    fn test_staged_goroot_is_versioned() {
        assert_eq!(staged_goroot("1.26.4"), "@heph/go/toolchain/1.26.4/go");
    }

    #[test]
    fn test_toolchain_addr_carries_version_and_host_platform() {
        let addr = toolchain_addr("1.26.4");
        assert_eq!(addr.package.as_str(), "@heph/go/toolchain/1.26.4");
        assert_eq!(addr.name, TOOLCHAIN_NAME);
        assert!(addr.args.contains_key("os"));
        assert!(addr.args.contains_key("arch"));
    }

    fn out_group<'a>(spec: &'a TargetSpec, group: &str) -> Vec<&'a str> {
        let out = match spec.config.get("out").unwrap() {
            Value::Map(m) => m,
            _ => panic!("out must be a map"),
        };
        match out.get(group).unwrap() {
            Value::List(v) => v
                .iter()
                .map(|e| match e {
                    Value::String(s) => s.as_str(),
                    _ => panic!("group entry must be a string"),
                })
                .collect(),
            _ => panic!("group {group} must be a list"),
        }
    }

    #[test]
    fn test_build_spec_sets_dirpath_output_and_driver() {
        let spec = build_spec(
            toolchain_addr(DEFAULT_GO_VERSION),
            DEFAULT_GO_VERSION,
            "linux",
            "amd64",
        )
        .unwrap();
        assert_eq!(spec.driver, TOOLCHAIN_DRIVER);
        // Single group exposes everything a consumer reads.
        let g = out_group(&spec, "");
        for needed in ["go/bin/", "go/pkg/", "go/lib/", "go/src/"] {
            assert!(
                g.contains(&needed),
                "SDK output must include {needed}: {g:?}"
            );
        }
    }

    #[test]
    fn test_output_omits_unused_dirs() {
        let spec = build_spec(
            toolchain_addr(DEFAULT_GO_VERSION),
            DEFAULT_GO_VERSION,
            "linux",
            "amd64",
        )
        .unwrap();
        // api/test/doc/misc are read by no consumer, so the SDK output never
        // collects them (keeps the read-only stage entry lean).
        for unused in ["go/api", "go/test", "go/doc", "go/misc"] {
            assert!(
                !out_group(&spec, "").iter().any(|p| p.starts_with(unused)),
                "SDK output must not collect unused {unused}"
            );
        }
    }

    #[test]
    fn test_build_spec_embeds_version_and_checksum() {
        let spec = build_spec(toolchain_addr("1.26.4"), "1.26.4", "darwin", "arm64").unwrap();
        assert!(matches!(
            spec.config.get("version"),
            Some(Value::String(s)) if s == "1.26.4"
        ));
        assert!(matches!(
            spec.config.get("sha256"),
            Some(Value::String(s)) if s == sdk_checksum("1.26.4", "darwin", "arm64").unwrap()
        ));
    }

    #[test]
    fn test_build_spec_unsupported_errors() {
        assert!(build_spec(toolchain_addr("1.26.4"), "1.26.4", "plan9", "386").is_err());
        assert!(build_spec(toolchain_addr("0.0.0"), "0.0.0", "linux", "amd64").is_err());
    }
}
