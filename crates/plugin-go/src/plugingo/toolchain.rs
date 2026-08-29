//! Hermetic Go toolchain provisioning.
//!
//! The toolchain is chosen by the required `gotool` provider option. With a
//! pinned version (e.g. `gotool = "1.27.0"`) the provider is fully hermetic: it
//! synthesizes a `//@heph/go/toolchain/<version>:go` target that downloads that
//! Go SDK tarball for the host platform from `go.dev/dl`, verifies its SHA-256,
//! and extracts the SDK tree as a cacheable directory output; every
//! build/list/test target deps that SDK and points `GOROOT` at the staged tree
//! (see [`addr_util::go_sdk_dep`] / [`addr_util::go_run_prelude`]) — reading no
//! `go`/`GOROOT`/`go env` from the host.
//!
//! With `gotool = "host"` ([`HOST`]) the provider instead uses the host `go`
//! (resolved from `PATH` / `go env GOROOT` inside the sandbox): no SDK target,
//! no `gosdk` dep, host env passed through. Non-hermetic by construction.
//!
//! With `gotool = "//pkg:go"` ([`is_target_ref`]) the toolchain comes from
//! another *target* — e.g. `//@heph/bin:go` (host `go` exposed by the hostbin
//! provider) or a `//some/pkg:go` built by the nix driver. The build deps that
//! target in the `gosdk` group exactly like the hermetic SDK, but its staged
//! path is not known ahead of time, so `go`/`GOROOT` are resolved from the
//! staged output at runtime (auto-detecting a GOROOT directory vs. a bare `go`
//! binary; see [`addr_util::go_goroot_prelude`] and the golist driver). How
//! hermetic the result is then depends entirely on what that target produces.
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
use hdriver_support::driver_managed::{
    ManagedDriver, ManagedRunInput, ManagedRunRequest, ManagedRunResponse,
};
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

/// A convenient pinned Go release for tests and as a documented example. The
/// provider requires the toolchain to be chosen explicitly (`gotool` option),
/// so this is not an implicit default for real builds — only a constant the
/// test helpers and examples reference. A hermetic version's tarball SHA-256 may
/// be supplied via the provider's optional `checksums` config option (see
/// [`checksum_key`]) to enforce verification; there is no built-in checksum
/// table, and an absent entry downloads unverified.
pub const DEFAULT_GO_VERSION: &str = "1.27.0";

/// Frozen Go version the def-hash **golden** tests build their canonical defs
/// from. Deliberately *not* [`DEFAULT_GO_VERSION`]: those goldens exist to catch
/// an accidental change to the def-hash *format*, and the toolchain version is a
/// legitimate hash input, so tracking the default would move every golden on
/// every Go bump — training the reader to re-stamp the constant and dissolving
/// the guard. Bumping the pinned toolchain must not touch a golden; this never
/// changes.
#[cfg(test)]
pub(crate) const HASH_GOLDEN_GO_VERSION: &str = "1.26.4";

/// Sentinel toolchain spec selecting the **host** `go` (read from `PATH` /
/// `go env GOROOT` inside the sandbox) instead of a hermetic pinned SDK. Chosen
/// via `gotool = "host"`. Any other `gotool` value is taken as a pinned hermetic
/// version. Threaded as the `go_version` string everywhere a toolchain is wired;
/// branch on it with [`is_host`].
pub const HOST: &str = "host";

/// Whether `spec` selects the host toolchain (vs. a hermetic pinned version).
pub fn is_host(spec: &str) -> bool {
    spec == HOST
}

/// Whether `spec` selects a **target** toolchain: an explicit target address
/// providing the `go` toolchain, distinguished by a leading `//`. Examples:
/// `//@heph/bin:go` (host `go` exposed by the hostbin provider) or
/// `//some/pkg:go` (a `go` built by the nix driver). The build deps that target
/// in the `gosdk` group, stages its single output, and resolves `go`/`GOROOT`
/// from it at runtime — auto-detecting a full GOROOT tree (a directory whose
/// `bin/go` is used) vs. a bare `go` binary (a file), with `GOROOT` taken from
/// whatever that `go` reports.
pub fn is_target_ref(spec: &str) -> bool {
    spec.starts_with("//")
}

/// The three ways the required `gotool` provider option selects the toolchain.
/// Threaded everywhere as the `go_version` string; classify with [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolchain<'a> {
    /// `gotool = "host"` — host `go` resolved from `PATH` / `go env GOROOT`.
    Host,
    /// `gotool = "//pkg:go"` — a target producing the toolchain (hostbin, nix, …).
    Target(&'a str),
    /// `gotool = "1.27.0"` — a pinned hermetic SDK downloaded from go.dev.
    Hermetic(&'a str),
}

/// Classify a `gotool` value into the toolchain it selects. `"host"` →
/// [`Toolchain::Host`]; anything starting with `//` → [`Toolchain::Target`];
/// everything else is taken as a pinned hermetic version.
pub fn classify(spec: &str) -> Toolchain<'_> {
    if is_host(spec) {
        Toolchain::Host
    } else if is_target_ref(spec) {
        Toolchain::Target(spec)
    } else {
        Toolchain::Hermetic(spec)
    }
}

/// Base provider package for the hermetic toolchain. The concrete target lives
/// at `{TOOLCHAIN_PKG_PREFIX}/<version>` (e.g. `@heph/go/toolchain/1.27.0`).
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

/// Lookup key for a tarball SHA-256 in the provider's `checksums` config map:
/// `"<version>/<goos>/<goarch>"`, e.g. `"1.27.0/darwin/arm64"`. There is no
/// built-in checksum table — each version's checksum is supplied via config
/// (`checksums:` under the go plugin's `options:`), keeping the binary free of
/// release-specific data and letting users pin new versions without a source
/// change. Checksums are **optional**: a missing entry downloads the SDK
/// unverified (the driver logs a warning); supply one to enforce verification.
/// Sourced from <https://go.dev/dl/?mode=json>.
pub fn checksum_key(version: &str, goos: &str, goarch: &str) -> String {
    format!("{version}/{goos}/{goarch}")
}

/// Tarball download URL for `version` on `(goos, goarch)`.
pub fn sdk_url(version: &str, goos: &str, goarch: &str) -> String {
    format!("https://go.dev/dl/go{version}.{goos}-{goarch}.tar.gz")
}

/// Provider package holding the toolchain for `version`,
/// e.g. `@heph/go/toolchain/1.27.0`.
pub fn toolchain_pkg(version: &str) -> String {
    format!("{TOOLCHAIN_PKG_PREFIX}/{version}")
}

/// Parse the Go version out of a toolchain package path, or `None` if `pkg` is
/// not a toolchain package. `@heph/go/toolchain/1.27.0` → `Some("1.27.0")`; the
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

/// The host toolchain, resolved **in the environment the tools will run in**.
///
/// `gotool = "host"` means "whatever `go` this build's environment provides".
/// Without a runner that is heph's own `PATH`, as it always was. With one it has
/// to be the runner's — reading heph's `PATH` there would compile with the
/// laptop's toolchain inside the named environment, under a cache key that
/// claims the environment's. That is a silently wrong build, not a degraded one.
///
/// `go env GOROOT` is asked *through the runner*, and the binary is taken as
/// `$GOROOT/bin/go` — the same derivation the shell prelude already uses for
/// host mode (`addr_util::go_goroot_prelude`), so the two paths cannot disagree
/// about which `go` a host build used.
///
/// One probe is a subprocess per target, which for a `go list`-heavy build is
/// the wrong order of magnitude — hence the cache. It holds a single slot: one
/// build uses one toolchain in one environment, so a single entry is very nearly
/// a 100% hit rate and cannot grow, and a key that does not match simply
/// re-probes. The lock is held **across** the probe so concurrent targets at
/// build start queue behind the first rather than each spawning their own.
#[derive(Default)]
pub(crate) struct HostGoCache {
    slot: tokio::sync::Mutex<Option<(String, (std::path::PathBuf, std::path::PathBuf))>>,
}

impl HostGoCache {
    async fn resolve(
        &self,
        runner: hexecrunner::RunnerRef<'_>,
        driver: &str,
        ctoken: &(dyn Cancellable + Send + Sync),
    ) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
        // Two host builds under different runners are different toolchains, so
        // the runner is the key. `None` is the local host.
        let key = runner
            .addr
            .map(hmodel::htaddr::Addr::format)
            .unwrap_or_else(|| "local".to_string());
        let mut slot = self.slot.lock().await;
        if let Some((cached_key, resolved)) = slot.as_ref()
            && cached_key == &key
        {
            return Ok(resolved.clone());
        }
        let resolved = probe_host_go(runner, driver, ctoken).await?;
        *slot = Some((key, resolved.clone()));
        Ok(resolved)
    }
}

/// Ask the environment where its Go lives. See [`HostGoCache`].
async fn probe_host_go(
    runner: hexecrunner::RunnerRef<'_>,
    driver: &str,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    // No runner: resolve from heph's own PATH exactly as before, so a workspace
    // that names no runner sees no change at all — including the error text when
    // there is no `go`.
    if runner.addr.is_none() {
        let go_bin = resolve_host_go()?;
        let goroot = host_goroot(&go_bin)?;
        return Ok((goroot, go_bin));
    }

    let spec = hproc::proc_exec::Spec {
        // Bare name on purpose: the runner's `PATH` is what must resolve it, and
        // it is the same `PATH` that the `go` answering below is found on.
        program: std::path::PathBuf::from("go"),
        args: vec!["env".into(), "GOROOT".into()],
        env: vec![],
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
        stdin: hproc::proc_exec::StdioSpec::Null,
        stdout: hproc::proc_exec::StdioSpec::Piped,
        stderr: hproc::proc_exec::StdioSpec::Piped,
        setsid: false,
        ctty: false,
    };
    // Name the runner in the failure. Under a runner the program that failed to
    // exec is the runner's, not `go` — a bare `No such file or directory` here
    // reads as "no Go toolchain" when it can equally mean the runner's own
    // prefix or launch command is missing.
    let via = runner
        .addr
        .map(|a| format!(" (via exec runner {})", a.format()))
        .unwrap_or_default();
    let out = hexecrunner::output(runner, spec, ctoken)
        .await
        .with_context(|| format!("{driver}: run `go env GOROOT`{via}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{driver}: `go env GOROOT` failed in the runner's environment: {}\n  \
             `gotool: \"host\"` takes the `go` that environment provides, so it must have one \
             on its PATH — install it there, or pin a toolchain with `gotool: \"<version>\"`.",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let goroot = String::from_utf8(out.stdout)
        .with_context(|| format!("{driver}: `go env GOROOT` output is not utf8"))?
        .trim()
        .to_string();
    if goroot.is_empty() {
        anyhow::bail!("{driver}: `go env GOROOT` returned empty in the runner's environment");
    }
    let goroot = std::path::PathBuf::from(goroot);
    let go_bin = goroot.join("bin").join("go");
    Ok((goroot, go_bin))
}

/// Resolve the host `go` binary from this process's `PATH` — used when the
/// provider selects `gotool = "host"`.
pub(crate) fn resolve_host_go() -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| anyhow::anyhow!("go host toolchain: PATH not set"))?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("go");
        if std::fs::metadata(&cand)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            return Ok(cand);
        }
    }
    anyhow::bail!("go host toolchain: `go` not found on PATH")
}

/// Query `GOROOT` from the host `go` binary.
pub(crate) fn host_goroot(go_bin: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    let out = std::process::Command::new(go_bin)
        .args(["env", "GOROOT"])
        .output()
        .with_context(|| format!("run {go_bin:?} env GOROOT"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`{go_bin:?} env GOROOT` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let goroot = String::from_utf8(out.stdout)
        .context("go env GOROOT output is not utf8")?
        .trim()
        .to_string();
    if goroot.is_empty() {
        anyhow::bail!("`{go_bin:?} env GOROOT` returned empty");
    }
    Ok(std::path::PathBuf::from(goroot))
}

/// Resolve the `go` binary from a target-ref toolchain (`gotool = "//pkg:go"`)
/// staged into this sandbox via the `gosdk` dep. The dep's output is either a
/// full GOROOT tree (we pick its `bin/go`) or a single `go` binary (a hostbin or
/// nix wrapper); when both shapes appear, prefer a `.../bin/go`.
pub(crate) fn resolve_target_go(inputs: &[ManagedRunInput]) -> anyhow::Result<std::path::PathBuf> {
    let prefix = format!("dep|{}|", crate::plugingo::addr_util::GO_SDK_DEP_GROUP);
    let gosdk = inputs
        .iter()
        .find(|m| m.input.origin_id.starts_with(&prefix))
        .context("go toolchain: target `gosdk` dep not staged")?;

    // Only paths are needed to locate the `go` binary — `entry_paths` is
    // header-only for tar-backed content, so we don't read the whole SDK tree
    // (~thousands of files) just to match a filename.
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for rel in gosdk
        .input
        .artifact
        .content
        .as_ref()
        .entry_paths()
        .context("enumerate target toolchain output")?
    {
        if rel.file_name().and_then(|n| n.to_str()) == Some("go") {
            candidates.push(gosdk.unpack_root.join(&rel));
        }
    }

    pick_go_binary(candidates).context("go toolchain: no `go` binary in target toolchain output")
}

/// Pick the `go` binary among the toolchain output's `go`-named entries,
/// preferring a `.../bin/go` (a full GOROOT tree) over a bare `go` wrapper
/// (hostbin/nix). Returns `None` when no candidate is present.
pub(crate) fn pick_go_binary(
    mut candidates: Vec<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    // `false` (parent is `bin`) sorts before `true`. Stable so ties keep input order.
    candidates.sort_by_key(|p| {
        p.parent()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            != Some("bin")
    });
    candidates.into_iter().next()
}

/// Resolve `(GOROOT, go binary)` for the toolchain selected by `version`, shared
/// by every driver that runs a `go` subprocess (`go_golist`, `go_compile`, …):
/// - **Host** (`gotool = "host"`): host `go` from `PATH`; GOROOT is what it reports.
/// - **Target** (`gotool = "//pkg:go"`): the `go` staged by the `gosdk` dep at a
///   path discovered from the dep's output; GOROOT is what it reports.
/// - **Hermetic** (`gotool = "1.27.0"`): the SDK staged at the deterministic
///   `staged_goroot` path under `sandbox_ws_dir`.
///
/// `driver` names the caller for the hermetic "not staged" error message.
pub(crate) async fn resolve_toolchain_go(
    version: &str,
    inputs: &[ManagedRunInput],
    sandbox_ws_dir: &std::path::Path,
    driver: &str,
    runner: hexecrunner::RunnerRef<'_>,
    host_go: &HostGoCache,
    ctoken: &(dyn Cancellable + Send + Sync),
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    match classify(version) {
        Toolchain::Host => host_go.resolve(runner, driver, ctoken).await,
        Toolchain::Target(_) => {
            let go_bin = resolve_target_go(inputs)?;
            let goroot = host_goroot(&go_bin)?;
            Ok((goroot, go_bin))
        }
        Toolchain::Hermetic(v) => {
            let goroot = sandbox_ws_dir.join(staged_goroot(v));
            let go_bin = goroot.join("bin").join("go");
            if !go_bin.exists() {
                anyhow::bail!(
                    "{driver}: hermetic go binary missing at {go_bin:?} (gosdk dep not staged?)"
                );
            }
            Ok((goroot, go_bin))
        }
    }
}

/// Build the `TargetSpec` for the toolchain download target for `version`.
/// `host_goos` / `host_goarch` are the platform the SDK runs on; `sha256` is the
/// expected tarball checksum the caller resolved from the provider's `checksums`
/// config (see [`checksum_key`]).
pub fn build_spec(
    addr: Addr,
    version: &str,
    host_goos: &str,
    host_goarch: &str,
    sha256: &str,
) -> TargetSpec {
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

    TargetSpec {
        addr,
        driver: TOOLCHAIN_DRIVER.to_string(),
        config,
        labels: vec!["go-toolchain".to_string()],
        ..Default::default()
    }
}

/// Config for a `go_toolchain` target (engine-generated by the Go provider).
#[derive(Spec)]
struct GoToolchainSpec {
    /// Pinned Go release, e.g. `1.27.0`.
    #[spec(required)]
    version: String,
    /// Host GOOS the SDK runs on.
    #[spec(required)]
    goos: String,
    /// Host GOARCH the SDK runs on.
    #[spec(required)]
    goarch: String,
    /// Expected SHA-256 of the downloaded tarball (hex). Empty = download
    /// unverified (no `checksums` entry was configured for this version/platform).
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

        let spec =
            GoToolchainSpec::from(&req.target_spec.config).context("parse go_toolchain config")?;

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
    let got = hex::encode(hasher.finalize());
    verify_checksum(expected_sha256, &got, version, url)?;

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

/// Compare the downloaded tarball's `got` SHA-256 against the `expected` one.
/// Checksum verification is optional: an empty `expected` (no `checksums` entry
/// configured for this version/platform) means the SDK is downloaded
/// **unverified** — allowed, but logged as a warning since it drops the
/// supply-chain guarantee. A non-empty `expected` that doesn't match fails the
/// build closed.
fn verify_checksum(expected: &str, got: &str, version: &str, url: &str) -> anyhow::Result<()> {
    if expected.is_empty() {
        tracing::warn!(
            version,
            url,
            "downloading Go {version} SDK without checksum verification — no `checksums` entry \
             configured for this version/platform; add one (sha256 from \
             https://go.dev/dl/?mode=json) to restore the supply-chain guarantee"
        );
        return Ok(());
    }
    if got != expected {
        anyhow::bail!(
            "Go {version} SDK checksum mismatch for {url}: expected {expected}, got {got}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod host_go_tests {
    use super::*;
    use hexecrunner::{PrepareOutcome, RunnerHost, RunnerRef, SpecRewrite};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stands in for "the environment the runner describes".
    ///
    /// Its whole job is to answer a bare `go` with a `go` that is **not** on
    /// heph's `PATH` — which is precisely the thing `gotool = "host"` under a
    /// runner has to get right, and precisely what it got wrong before: the
    /// binary was resolved from heph's own `PATH` and then executed inside the
    /// runner's environment.
    struct FakeEnv {
        go: std::path::PathBuf,
        probes: Arc<AtomicUsize>,
        /// The registry is process-global and every test in this binary installs
        /// into it, so a host that claimed every request would serve its
        /// neighbours' too. Each test gets its own request id.
        request_id: String,
    }

    #[async_trait]
    impl RunnerHost for FakeEnv {
        fn owns(&self, request_id: &str) -> bool {
            request_id == self.request_id
        }
        fn alive(&self) -> bool {
            true
        }
        async fn prepare(
            &self,
            _request_id: &str,
            _addr: &Addr,
            mut rewrite: SpecRewrite,
            _ctoken: &(dyn Cancellable + Send + Sync),
        ) -> anyhow::Result<PrepareOutcome> {
            assert_eq!(
                rewrite.program,
                std::path::PathBuf::from("go"),
                "the probe must ask the environment for a bare `go`, not hand it a host path"
            );
            self.probes.fetch_add(1, Ordering::SeqCst);
            rewrite.program = self.go.clone();
            Ok(PrepareOutcome {
                rewrite,
                supplies_environment: true,
            })
        }
    }

    /// A `go` that reports `goroot` and nothing else.
    fn fake_go(dir: &std::path::Path, goroot: &str) -> std::path::PathBuf {
        let go = dir.join("fake-go");
        std::fs::write(&go, format!("#!/bin/sh\necho {goroot}\n")).expect("write fake go");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&go, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        go
    }

    fn addr() -> Addr {
        Addr::new(
            PkgBuf::from("tools/devenv"),
            "runner".to_string(),
            Default::default(),
        )
    }

    /// **The fix.** `GOROOT` comes from the `go` the *runner's* environment
    /// provides, and the binary is that environment's, not the host's.
    #[tokio::test]
    async fn host_mode_under_a_runner_asks_the_runners_go() {
        const REQ: &str = "req-host-go";
        let dir = tempfile::tempdir().expect("tempdir");
        let probes = Arc::new(AtomicUsize::new(0));
        hexecrunner::install_host(Arc::new(FakeEnv {
            go: fake_go(dir.path(), "/env/goroot"),
            probes: Arc::clone(&probes),
            request_id: REQ.to_string(),
        }));

        let ct = hcore::hasync::StdCancellationToken::new();
        let a = addr();
        let cache = HostGoCache::default();
        let (goroot, go_bin) = cache
            .resolve(RunnerRef::target(REQ, &a), "go_golist", &ct)
            .await
            .expect("resolve");

        assert_eq!(goroot, std::path::PathBuf::from("/env/goroot"));
        assert_eq!(
            go_bin,
            std::path::PathBuf::from("/env/goroot/bin/go"),
            "the binary must be derived from the environment's GOROOT, the same way the shell \
             prelude does it"
        );
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    /// One probe serves the whole build. Without this a `go list`-heavy build
    /// pays a subprocess per package for a value that cannot change within it.
    #[tokio::test]
    async fn the_probe_is_paid_once_per_environment() {
        const REQ: &str = "req-once";
        let dir = tempfile::tempdir().expect("tempdir");
        let probes = Arc::new(AtomicUsize::new(0));
        hexecrunner::install_host(Arc::new(FakeEnv {
            go: fake_go(dir.path(), "/env/goroot"),
            probes: Arc::clone(&probes),
            request_id: REQ.to_string(),
        }));

        let ct = hcore::hasync::StdCancellationToken::new();
        let a = addr();
        let cache = HostGoCache::default();
        for _ in 0..5 {
            cache
                .resolve(RunnerRef::target(REQ, &a), "go_golist", &ct)
                .await
                .expect("resolve");
        }
        assert_eq!(
            probes.load(Ordering::SeqCst),
            1,
            "five targets in one environment must share one probe"
        );
    }

    /// **No runner: nothing changes.** A workspace that names none must still
    /// resolve from heph's own `PATH` and report the same `GOROOT` it always
    /// did — this path is not routed through a runner at all, and the whole
    /// change is meant to be invisible to it.
    #[tokio::test]
    async fn host_mode_without_a_runner_still_uses_this_process_path() {
        let Ok(expected) = resolve_host_go() else {
            eprintln!("skipping: no `go` on PATH to compare against");
            return;
        };
        let ct = hcore::hasync::StdCancellationToken::new();
        let (goroot, go_bin) = HostGoCache::default()
            .resolve(RunnerRef::local(), "go_golist", &ct)
            .await
            .expect("resolve");
        assert_eq!(go_bin, expected);
        assert_eq!(goroot, host_goroot(&expected).expect("goroot"));
    }

    /// A different runner is a different toolchain, so it must not be served
    /// the previous one's answer — the cache is keyed on the runner, and a
    /// single slot must re-probe rather than return a stale GOROOT.
    #[tokio::test]
    async fn a_different_runner_re_probes() {
        const REQ: &str = "req-reprobe";
        let dir = tempfile::tempdir().expect("tempdir");
        let probes = Arc::new(AtomicUsize::new(0));
        hexecrunner::install_host(Arc::new(FakeEnv {
            go: fake_go(dir.path(), "/env/goroot"),
            probes: Arc::clone(&probes),
            request_id: REQ.to_string(),
        }));

        let ct = hcore::hasync::StdCancellationToken::new();
        let cache = HostGoCache::default();
        let one = addr();
        let two = Addr::new(
            PkgBuf::from("tools/other"),
            "runner".to_string(),
            Default::default(),
        );
        cache
            .resolve(RunnerRef::target(REQ, &one), "go_golist", &ct)
            .await
            .expect("one");
        cache
            .resolve(RunnerRef::target(REQ, &two), "go_golist", &ct)
            .await
            .expect("two");
        assert_eq!(probes.load(Ordering::SeqCst), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checksum_key_format() {
        assert_eq!(
            checksum_key("1.27.0", "darwin", "arm64"),
            "1.27.0/darwin/arm64"
        );
    }

    #[test]
    fn test_verify_checksum_empty_expected_skips() {
        // No configured checksum → unverified download is allowed (warns).
        assert!(verify_checksum("", "anything", "1.27.0", "http://x").is_ok());
    }

    #[test]
    fn test_verify_checksum_match_ok_mismatch_fails() {
        assert!(verify_checksum("abc", "abc", "1.27.0", "http://x").is_ok());
        let err = verify_checksum("abc", "def", "1.27.0", "http://x").unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
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
        // A version that is deliberately *not* `DEFAULT_GO_VERSION`, so the
        // explicit case cannot pass by accidentally agreeing with the default
        // the bare-prefix case below asserts.
        assert_eq!(
            version_from_pkg("@heph/go/toolchain/1.25.0"),
            Some("1.25.0")
        );
        // Bare package defaults.
        assert_eq!(
            version_from_pkg("@heph/go/toolchain"),
            Some(DEFAULT_GO_VERSION)
        );
        // Not a toolchain package, or nested.
        assert_eq!(version_from_pkg("mylib"), None);
        assert_eq!(version_from_pkg("@heph/go/toolchain/1.27.0/extra"), None);
    }

    #[test]
    fn test_classify_distinguishes_host_target_hermetic() {
        assert_eq!(classify("host"), Toolchain::Host);
        assert_eq!(classify("1.27.0"), Toolchain::Hermetic("1.27.0"));
        assert_eq!(
            classify("//@heph/bin:go"),
            Toolchain::Target("//@heph/bin:go")
        );
        assert_eq!(
            classify("//some/pkg:go"),
            Toolchain::Target("//some/pkg:go")
        );
        // A bare version is never mistaken for a target ref.
        assert!(!is_target_ref("1.27.0"));
        assert!(!is_target_ref("host"));
        assert!(is_target_ref("//@heph/bin:go"));
    }

    #[test]
    fn test_pick_go_binary_prefers_goroot_tree_bin_go() {
        use std::path::PathBuf;
        // hostbin wrapper only → use it.
        assert_eq!(
            pick_go_binary(vec![PathBuf::from("__heph/hostbin/go")]),
            Some(PathBuf::from("__heph/hostbin/go"))
        );
        // Both a wrapper and a full tree → prefer the tree's bin/go.
        assert_eq!(
            pick_go_binary(vec![
                PathBuf::from("ws/wrap/go"),
                PathBuf::from("ws/sdk/go/bin/go"),
            ]),
            Some(PathBuf::from("ws/sdk/go/bin/go"))
        );
        // No candidates → None.
        assert_eq!(pick_go_binary(vec![]), None);
    }

    #[test]
    fn test_staged_goroot_is_versioned() {
        assert_eq!(staged_goroot("1.27.0"), "@heph/go/toolchain/1.27.0/go");
    }

    #[test]
    fn test_toolchain_addr_carries_version_and_host_platform() {
        let addr = toolchain_addr("1.27.0");
        assert_eq!(addr.package.as_str(), "@heph/go/toolchain/1.27.0");
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
            "deadbeef",
        );
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
            "deadbeef",
        );
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
        let spec = build_spec(
            toolchain_addr("1.27.0"),
            "1.27.0",
            "darwin",
            "arm64",
            "abc123",
        );
        assert!(matches!(
            spec.config.get("version"),
            Some(Value::String(s)) if s == "1.27.0"
        ));
        assert!(matches!(
            spec.config.get("sha256"),
            Some(Value::String(s)) if s == "abc123"
        ));
    }
}
