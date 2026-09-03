// Shared integration-test helper module: each test binary `mod common`s this but
// uses only some helpers, so per-binary dead-code/unused-import warnings are
// expected here (the items are exercised across the suite).
// `allow`, not `expect`: whether an item is dead or an import unused varies per
// test binary, so an `expect` would be unfulfilled in some of them.
#![allow(
    dead_code,
    unused_imports,
    reason = "shared test harness; each test binary uses a different subset"
)]

use anyhow::Context as _;
use heph::pluginbuildfile;
use heph::pluginexec;
use heph::pluginhttp;
use heph::pluginstatictarget;
use htestkit::{Workspace, WorkspaceBuilder, copy_dir_to_tempdir};
use plugin_go::plugingo;
use std::path::PathBuf;
use tempfile::TempDir;

pub use htestkit::{artifact_bytes, artifact_paths, artifact_string};

macro_rules! require_go {
    () => {
        if !crate::common::go_available() {
            crate::common::no_go_or_panic();
            return Ok(());
        }
    };
}
pub(crate) use require_go;

pub fn go_available() -> bool {
    std::process::Command::new("go")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skipping is fine on a dev machine without Go; in CI it is a broken job.
///
/// Every test in this crate needs `go` on PATH, so a runner without it turns the
/// whole suite green in a quarter of a second — indistinguishable from a suite
/// that passed, and exactly how the macOS leg went untested for as long as it
/// did. Under `CI` this is a hard failure instead.
pub fn no_go_or_panic() {
    assert!(
        std::env::var_os("CI").is_none(),
        "go is not on PATH, so this test would silently skip. In CI that is a \
         broken job, not a skip: the devenv shell provides `pkgs.go` (see \
         devenv.nix), so reaching this means the test is not running inside it."
    );
    eprintln!("skipping: go not in PATH");
}

/// Whether a linked ELF executable declares a `PT_INTERP` program header — i.e.
/// needs a dynamic loader (`/lib/ld-linux-<arch>.so.1`) present at exec time.
///
/// This is the distinction `file(1)` reports as `dynamically linked, interpreter
/// …` vs `statically linked`, and the one that decides whether the binary runs
/// in a `FROM scratch` image. A pure-Go `-buildmode=pie` link has no `DT_NEEDED`
/// entries yet still carries `PT_INTERP`, so "does it link against libc" is the
/// wrong question — this is the right one.
///
/// Mach-O has no equivalent (every darwin executable is dynamically linked
/// against libSystem), so callers gate this on a Linux host.
pub fn has_interp(data: &[u8]) -> anyhow::Result<bool> {
    use object::read::elf::{FileHeader as _, ProgramHeader as _};
    let header =
        object::elf::FileHeader64::<object::Endianness>::parse(data).context("parse as ELF64")?;
    let endian = header.endian().context("ELF endianness")?;
    Ok(header
        .program_headers(endian, data)
        .context("read ELF program headers")?
        .iter()
        .any(|ph| ph.p_type(endian) == object::elf::PT_INTERP))
}

pub fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

pub fn fixture(name: &str) -> anyhow::Result<TempDir> {
    copy_dir_to_tempdir(&testdata(name))
}

/// Absolute path to the host C compiler, or `None` if there isn't one.
///
/// Only a race build that needs cgo (linux — see `plugingo::factors::cgo_required`)
/// ever resolves the `cc` target this backs; on darwin nothing asks for it.
fn cc_bin_path() -> Option<String> {
    ["cc", "gcc", "clang"]
        .iter()
        .find_map(|name| which::which(name).ok())
        .map(|p| p.to_string_lossy().into_owned())
}

fn go_bin_path() -> String {
    let output = std::process::Command::new("go")
        .args(["env", "GOROOT"])
        .output()
        .expect("go env GOROOT");
    let goroot = String::from_utf8(output.stdout)
        .expect("utf8 goroot")
        .trim()
        .to_string();
    format!("{goroot}/bin/go")
}

/// Pinned hermetic Go toolchain version the e2e suite builds against. Taken from
/// the provider's own constant rather than spelled again: a second copy that
/// drifted would key [`HERMETIC_GO_CHECKSUMS`] to a version nothing downloads,
/// and a checksum entry that misses is not an error — the SDK is then fetched
/// unverified (the driver only warns), so the suite would still pass while
/// testing an unverified toolchain.
pub const HERMETIC_GO: &str = plugingo::DEFAULT_GO_VERSION;
/// `gotool` sentinel selecting the host `go`, from the provider's own constant.
pub const HOST_GO: &str = plugingo::HOST;

/// SDK tarball SHA-256 for [`HERMETIC_GO`] on each `(goos, goarch)` CI builds
/// on. The provider has no built-in table — hermetic builds must supply these
/// via the `checksums` config option — so the suite injects them.
///
/// Only the platform pair is spelled here; the `"<version>/<goos>/<goarch>"`
/// key is rendered by `plugingo::checksum_key` from [`HERMETIC_GO`]
/// (see [`sdk_checksums_for`]), so bumping the version cannot leave a stale key
/// behind that silently matches nothing. Bumping it *does* leave stale hashes —
/// but those fail the download closed, loudly, which is the point.
///
/// Sourced from <https://go.dev/dl/?mode=json>.
const HERMETIC_GO_CHECKSUMS: &[(&str, &str, &str)] = &[
    (
        "linux",
        "amd64",
        "675c26c449cbb18fc24b74650de1eabbae6e16f64326fd85a283fb3b58280685",
    ),
    (
        "linux",
        "arm64",
        "51798d2c42d0e1c6ed7fd9f48728b4193abac9e8aad6dbac2fe96a81f5909bda",
    ),
    (
        "darwin",
        "amd64",
        "d3314e25496e4381d71a5c51d2907e7af655d199f6780b549f015bd85fef4986",
    ),
    (
        "darwin",
        "arm64",
        "90493b3bbd5e10f91d12153198bf1994fd756399b4fec93b49b0c6e2acdeeb3e",
    ),
];

/// Checksums to put in the go provider `Config` for a given `gotool`: the
/// hermetic set for a pinned version, empty for `host` (no SDK download).
fn sdk_checksums_for(gotool: &str) -> std::collections::HashMap<String, String> {
    if gotool == HOST_GO {
        return std::collections::HashMap::new();
    }
    HERMETIC_GO_CHECKSUMS
        .iter()
        .map(|(goos, goarch, sha)| {
            (
                plugingo::checksum_key(HERMETIC_GO, goos, goarch),
                sha.to_string(),
            )
        })
        .collect()
}

/// A minimal provider that injects the Go build *variants* every e2e workspace
/// needs, without editing each fixture's BUILD file. Its `probe` returns, for the
/// root package, a `provider="go"` state declaring three variants:
///   - `host`: the build host's own GOOS/GOARCH (the default the suite uses),
///   - `linux_amd64`: pinned linux/amd64 (for the build-tag cross-compile tests),
///   - `linux_amd64_pie`: as `linux_amd64`, but `buildmode = "pie"`.
///
/// The buildmode tests use the pinned pair rather than `host` so they assert on
/// an ELF whatever the build host is — the distinction they care about
/// (`PT_INTERP` or not) has no Mach-O equivalent.
///
/// Every other endpoint is empty — targets/packages come from the real providers.
struct VariantInjector;

impl VariantInjector {
    fn variants_state() -> hplugin::provider::State {
        use hcore::htvalue::Value;
        let variant = |goos: &str, goarch: &str| {
            Value::Map(std::collections::HashMap::from([
                ("goos".to_string(), Value::String(goos.to_string())),
                ("goarch".to_string(), Value::String(goarch.to_string())),
            ]))
        };
        let linux_amd64_pie = Value::Map(std::collections::HashMap::from([
            ("goos".to_string(), Value::String("linux".to_string())),
            ("goarch".to_string(), Value::String("amd64".to_string())),
            ("buildmode".to_string(), Value::String("pie".to_string())),
        ]));
        let variants = Value::Map(std::collections::HashMap::from([
            (
                "host".to_string(),
                variant(hcore::htplatform::os(), hcore::htplatform::arch()),
            ),
            ("linux_amd64".to_string(), variant("linux", "amd64")),
            ("linux_amd64_pie".to_string(), linux_amd64_pie),
        ]));
        hplugin::provider::State {
            package: hmodel::htpkg::PkgBuf::from(""),
            provider: "go".to_string(),
            state: std::collections::HashMap::from([("variants".to_string(), variants)]),
        }
    }
}

impl hplugin::provider::Provider for VariantInjector {
    fn config(
        &self,
        _req: hplugin::provider::ConfigRequest,
    ) -> anyhow::Result<hplugin::provider::ConfigResponse> {
        Ok(hplugin::provider::ConfigResponse {
            name: "go-variant-injector".to_string(),
        })
    }

    fn list<'a>(
        &'a self,
        _req: hplugin::provider::ListRequest,
        _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<
        'a,
        anyhow::Result<
            Box<dyn Iterator<Item = anyhow::Result<hplugin::provider::ListResponse>> + Send>,
        >,
    > {
        Box::pin(async move { Ok(Box::new(std::iter::empty()) as Box<_>) })
    }

    fn list_packages<'a>(
        &'a self,
        _req: hplugin::provider::ListPackagesRequest,
        _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<
        'a,
        anyhow::Result<
            Box<dyn Iterator<Item = anyhow::Result<hplugin::provider::ListPackageResponse>> + Send>,
        >,
    > {
        Box::pin(async move { Ok(Box::new(std::iter::empty()) as Box<_>) })
    }

    fn get<'a>(
        &'a self,
        _req: hplugin::provider::GetRequest,
        _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<
        'a,
        Result<hplugin::provider::GetResponse, hplugin::provider::GetError>,
    > {
        Box::pin(async move { Err(hplugin::provider::GetError::NotFound) })
    }

    fn probe<'a>(
        &'a self,
        req: hplugin::provider::ProbeRequest,
        _ctoken: &'a (dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> futures::future::BoxFuture<'a, anyhow::Result<hplugin::provider::ProbeResponse>> {
        Box::pin(async move {
            let states = if req.package.as_str().is_empty() {
                vec![Self::variants_state()]
            } else {
                vec![]
            };
            Ok(hplugin::provider::ProbeResponse { states })
        })
    }
}

/// Default workspace: builds with the **host** `go` (gotool = "host"). This is
/// what almost every e2e test should use — it reuses the host toolchain's
/// prebuilt std, so it stages no hermetic SDK and builds no std from source,
/// which is dramatically less disk and time. (Staging a full hermetic SDK + std
/// tree per isolated test workspace is what exhausted the CI runner disk.) Only
/// the few tests that specifically exercise the hermetic toolchain use
/// [`make_workspace_hermetic`]. Requires `go` on PATH (guard with `require_go!`).
pub fn make_workspace(dir: TempDir) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, false, true, &[], HOST_GO)
}

/// Alias for [`make_workspace`] kept for call sites that spell the host toolchain
/// explicitly.
pub fn make_workspace_host(dir: TempDir) -> anyhow::Result<Workspace> {
    make_workspace(dir)
}

/// Build the workspace against the pinned **hermetic** Go SDK ([`HERMETIC_GO`]) —
/// downloaded and staged, with std compiled from source. Expensive (disk + time),
/// so reserve it for the *select few* tests that must prove the hermetic
/// toolchain path itself; everything else uses [`make_workspace`] (host `go`).
pub fn make_workspace_hermetic(dir: TempDir) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, false, true, &[], HERMETIC_GO)
}

/// A published heph release whose `heph-govet_<goos>_<goarch>` assets the lint
/// suite downloads (via the built-in `http_fetch` driver, as a real workspace
/// does). Pinned rather than "latest" so the fetch is reproducible.
pub const GOVET_RELEASE: &str = "v1.0.0-alpha-build.177.721+g21169b25";

/// The `govet` provider option pointing at [`GOVET_RELEASE`]'s download target.
pub fn govet_addr() -> String {
    format!("//@heph/go/govet/{GOVET_RELEASE}:heph-govet")
}

/// Like [`make_workspace`] but with `fs.skip` entries, mirroring a config file's
/// `fs: { skip: [...] }`. Used to reproduce a codegen target whose generated Go
/// package lives under a skipped subtree (e.g. a generated `gen/**` tree).
/// Host toolchain (`gotool = "host"`), with every Go tool defaulting to the
/// exec runner at `runner`. See [`make_workspace_ordered_runner`].
pub fn make_workspace_host_under_runner(dir: TempDir, runner: &str) -> anyhow::Result<Workspace> {
    make_workspace_ordered_runner(dir, true, true, &[], HOST_GO, Some(runner))
}

pub fn make_workspace_fs_skip(dir: TempDir, skip: &[&str]) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, false, true, skip, HOST_GO)
}

/// Same as [`make_workspace`] but registers the **go provider before** the
/// buildfile provider. Provider order = registration order, so with go first a
/// `get_spec` for a buildfile target in a Go package dir asks the go provider
/// first — exercising the engine's cycle-containment path.
///
/// `foreign_name_guard` toggles the go provider's
/// [`plugingo::Config::foreign_name_guard`]: pass `false` to let the go provider
/// over-claim foreign names (so the engine's cycle containment is what's tested).
pub fn make_workspace_go_first(
    dir: TempDir,
    foreign_name_guard: bool,
) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, true, foreign_name_guard, &[], HOST_GO)
}

fn make_workspace_ordered(
    dir: TempDir,
    go_first: bool,
    foreign_name_guard: bool,
    fs_skip: &[&str],
    gotool: &str,
) -> anyhow::Result<Workspace> {
    make_workspace_ordered_runner(dir, go_first, foreign_name_guard, fs_skip, gotool, None)
}

/// A workspace whose Go **tool** drivers default to `runner` — the plugin's
/// `runner:` config-yaml option, which is the only way a generated Go target
/// ever names one. `None` is the plain host, as every other fixture uses.
///
/// Registers the `devenv_runner` driver too, so a fixture can declare the
/// runner target it points at.
fn make_workspace_ordered_runner(
    dir: TempDir,
    go_first: bool,
    foreign_name_guard: bool,
    fs_skip: &[&str],
    gotool: &str,
    runner: Option<&str>,
) -> anyhow::Result<Workspace> {
    let runner = runner.map(str::to_string);
    let provider_runner = runner.clone();
    let gotool = gotool.to_string();
    let go_bin = go_bin_path();
    let cc_bin = cc_bin_path();
    // `fs` is auto-registered by `Engine::new`.
    let mut b = WorkspaceBuilder::from_dir(dir).with_fs_skip(fs_skip.iter().copied());

    // Inject the Go build variants (`host`, `linux_amd64`) every fixture builds
    // against, so targets resolve `@v=…` without each BUILD declaring them.
    b = b.with_provider(|_| Box::new(VariantInjector));

    if go_first {
        let gotool = gotool.clone();
        let provider_runner = provider_runner.clone();
        b = b.with_provider(move |init| {
            Box::new(
                plugingo::Provider::with_config(
                    init.root.to_path_buf(),
                    plugingo::Config {
                        foreign_name_guard,
                        sdk_checksums: sdk_checksums_for(&gotool),
                        go_version: gotool,
                        govet: govet_addr(),
                        // The bash half of the runner: the std install and the
                        // thirdparty download invoke `go` too, and a build whose
                        // compile drivers moved into the environment while these
                        // stayed on the host fails on mismatched object versions.
                        runner: provider_runner.clone(),
                        ..Default::default()
                    },
                    init.runtime.clone(),
                )
                .expect("plugingo provider"),
            )
        });
    }

    b = b
        .with_provider(|init| {
            Box::new(pluginbuildfile::Provider::new(
                init.root.to_path_buf(),
                init.runtime.clone(),
            ))
        })
        .with_provider(move |_| {
            let mut targets = vec![pluginstatictarget::Target {
                addr: "//@heph/bin:go".to_string(),
                driver: "bash".to_string(),
                run: Some(format!("cp -p \"{go_bin}\" go")),
                out: std::collections::HashMap::from([(String::new(), vec!["go".to_string()])]),
                codegen: None,
                deps: Default::default(),
                labels: vec![],
                ..Default::default()
            }];
            // Stand-in for the hostbin `//@heph/bin:cc` a real workspace uses,
            // which a race build stages where race needs cgo. A *shim* rather
            // than a copy of the binary: gcc locates its own subprograms
            // relative to argv[0], so a copied driver can't find `cc1`.
            if let Some(cc_bin) = &cc_bin {
                targets.push(pluginstatictarget::Target {
                    addr: "//@heph/bin:cc".to_string(),
                    driver: "bash".to_string(),
                    run: Some(format!(
                        "printf '#!/bin/sh\\nexec \"{cc_bin}\" \"$@\"\\n' > cc\nchmod +x cc"
                    )),
                    out: std::collections::HashMap::from([(String::new(), vec!["cc".to_string()])]),
                    codegen: None,
                    deps: Default::default(),
                    labels: vec![],
                    ..Default::default()
                });
            }
            Box::new(pluginstatictarget::Provider::new(targets).expect("static provider"))
        });

    if !go_first {
        let gotool = gotool.clone();
        let provider_runner = provider_runner.clone();
        b = b.with_provider(move |init| {
            Box::new(
                plugingo::Provider::with_config(
                    init.root.to_path_buf(),
                    plugingo::Config {
                        foreign_name_guard,
                        sdk_checksums: sdk_checksums_for(&gotool),
                        go_version: gotool,
                        govet: govet_addr(),
                        // The bash half of the runner: the std install and the
                        // thirdparty download invoke `go` too, and a build whose
                        // compile drivers moved into the environment while these
                        // stayed on the host fails on mismatched object versions.
                        runner: provider_runner.clone(),
                        ..Default::default()
                    },
                    init.runtime.clone(),
                )
                .expect("plugingo provider"),
            )
        });
    }

    let r = || runner.clone();
    b.with_managed_driver(Box::new(pluginexec::Driver::new_bash().with_host_path()))
        .with_managed_driver(Box::new(pluginexec::Driver::new_exec().with_host_path()))
        .with_managed_driver(Box::new(hplugin_devenv::plugindevenv::Driver::new()))
        .with_managed_driver(Box::new(
            plugingo::GoGolistDriver::new().with_default_runner(r()),
        ))
        .with_managed_driver(Box::new(plugingo::GoToolchainDriver))
        .with_managed_driver(Box::new(pluginhttp::Driver))
        .with_managed_driver(Box::new(
            plugingo::GoCompileDriver::new().with_default_runner(r()),
        ))
        .with_managed_driver(Box::new(plugingo::GoTestmainDriver))
        .with_managed_driver(Box::new(
            plugingo::GoLintDriver::new().with_default_runner(r()),
        ))
        .with_managed_driver(Box::new(plugingo::GoLintGateDriver::new()))
        .with_managed_driver(Box::new(plugingo::GoLintFixDriver::new()))
        .with_managed_driver(Box::new(
            plugingo::GoFormatDriver::new().with_default_runner(r()),
        ))
        .with_managed_driver(Box::new(
            plugingo::GoFormatCheckDriver::new().with_default_runner(runner),
        ))
        .build()
        .context("build plugingo workspace")
}
