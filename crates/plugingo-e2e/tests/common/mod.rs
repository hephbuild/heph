// Shared integration-test helper module: each test binary `mod common`s this but
// uses only some helpers, so per-binary dead-code/unused-import warnings are
// expected here (the items are exercised across the suite).
#![allow(dead_code, unused_imports)]

use anyhow::Context as _;
use heph::pluginbuildfile;
use heph::pluginexec;
use heph::pluginstatictarget;
use htestkit::{Workspace, WorkspaceBuilder, copy_dir_to_tempdir};
use plugin_go::plugingo;
use std::path::PathBuf;
use tempfile::TempDir;

pub use htestkit::{artifact_paths, artifact_string};

macro_rules! require_go {
    () => {
        if !crate::common::go_available() {
            eprintln!("skipping: go not in PATH");
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

pub fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

pub fn fixture(name: &str) -> anyhow::Result<TempDir> {
    copy_dir_to_tempdir(&testdata(name))
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

/// Pinned hermetic Go toolchain version the e2e suite builds against. Mirrors
/// `plugingo::toolchain::DEFAULT_GO_VERSION`.
pub const HERMETIC_GO: &str = "1.26.4";
/// `gotool` sentinel selecting the host `go` (mirrors `plugingo::toolchain::HOST`).
pub const HOST_GO: &str = "host";

/// SDK tarball checksums for [`HERMETIC_GO`], keyed `"<version>/<goos>/<goarch>"`
/// (see `plugingo::toolchain::checksum_key`). The provider has no built-in table
/// — hermetic builds must supply these via the `checksums` config option — so
/// the suite injects them for the host platforms CI runs on. Sourced from
/// <https://go.dev/dl/?mode=json>.
const HERMETIC_GO_CHECKSUMS: &[(&str, &str)] = &[
    (
        "1.26.4/linux/amd64",
        "1153d3d50e0ac764b447adfe05c2bcf08e889d42a02e0fe0259bd47f6733ad7f",
    ),
    (
        "1.26.4/linux/arm64",
        "ef758ae7c6cf9267c9c0ef080b8965f453d89ab2d25d9eb22de4405925238768",
    ),
    (
        "1.26.4/darwin/amd64",
        "05dc9b5f9997744520aaebb3d5deaa7c755371aebbfb7f97c2511a9f3367538d",
    ),
    (
        "1.26.4/darwin/arm64",
        "b62ad2b6d7d2464f12a5bcad7ff47f19d08325773b5efd21610e445a05a9bf53",
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
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub fn make_workspace(dir: TempDir) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, false, true, &[], HERMETIC_GO)
}

/// Build the workspace using the **host** `go` (gotool = "host") instead of a
/// hermetic SDK. Requires `go` on PATH (guard call sites with `require_go!`).
pub fn make_workspace_host(dir: TempDir) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, false, true, &[], HOST_GO)
}

/// Like [`make_workspace`] but with `fs.skip` entries, mirroring a config file's
/// `fs: { skip: [...] }`. Used to reproduce a codegen target whose generated Go
/// package lives under a skipped subtree (e.g. a generated `gen/**` tree).
pub fn make_workspace_fs_skip(dir: TempDir, skip: &[&str]) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, false, true, skip, HERMETIC_GO)
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
    make_workspace_ordered(dir, true, foreign_name_guard, &[], HERMETIC_GO)
}

fn make_workspace_ordered(
    dir: TempDir,
    go_first: bool,
    foreign_name_guard: bool,
    fs_skip: &[&str],
    gotool: &str,
) -> anyhow::Result<Workspace> {
    let gotool = gotool.to_string();
    let go_bin = go_bin_path();
    // `fs` is auto-registered by `Engine::new`.
    let mut b = WorkspaceBuilder::from_dir(dir).with_fs_skip(fs_skip.iter().copied());

    if go_first {
        let gotool = gotool.clone();
        b = b.with_provider(move |init| {
            Box::new(
                plugingo::Provider::with_config(
                    init.root.to_path_buf(),
                    plugingo::Config {
                        foreign_name_guard,
                        sdk_checksums: sdk_checksums_for(&gotool),
                        go_version: gotool,
                        ..Default::default()
                    },
                )
                .expect("plugingo provider"),
            )
        });
    }

    b = b
        .with_provider(|init| Box::new(pluginbuildfile::Provider::new(init.root.to_path_buf())))
        .with_provider(move |_| {
            Box::new(
                pluginstatictarget::Provider::new(vec![pluginstatictarget::Target {
                    addr: "//@heph/bin:go".to_string(),
                    driver: "bash".to_string(),
                    run: Some(format!("cp -p \"{go_bin}\" go")),
                    out: std::collections::HashMap::from([(String::new(), vec!["go".to_string()])]),
                    codegen: None,
                    deps: Default::default(),
                    labels: vec![],
                }])
                .expect("static provider"),
            )
        });

    if !go_first {
        let gotool = gotool.clone();
        b = b.with_provider(move |init| {
            Box::new(
                plugingo::Provider::with_config(
                    init.root.to_path_buf(),
                    plugingo::Config {
                        foreign_name_guard,
                        sdk_checksums: sdk_checksums_for(&gotool),
                        go_version: gotool,
                        ..Default::default()
                    },
                )
                .expect("plugingo provider"),
            )
        });
    }

    b.with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .with_managed_driver(Box::new(pluginexec::Driver::new_sh()))
        .with_managed_driver(Box::new(pluginexec::Driver::new_exec()))
        .with_managed_driver(Box::new(plugingo::GoGolistDriver::new()))
        .with_managed_driver(Box::new(plugingo::GoToolchainDriver))
        .with_managed_driver(Box::new(plugingo::GoEmbedDriver))
        .build()
        .context("build plugingo workspace")
}
