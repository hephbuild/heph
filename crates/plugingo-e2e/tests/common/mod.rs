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

pub fn make_workspace(dir: TempDir) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, false, true, &[])
}

/// Like [`make_workspace`] but with `fs.skip` entries, mirroring a config file's
/// `fs: { skip: [...] }`. Used to reproduce a codegen target whose generated Go
/// package lives under a skipped subtree (e.g. a generated `gen/**` tree).
pub fn make_workspace_fs_skip(dir: TempDir, skip: &[&str]) -> anyhow::Result<Workspace> {
    make_workspace_ordered(dir, false, true, skip)
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
    make_workspace_ordered(dir, true, foreign_name_guard, &[])
}

fn make_workspace_ordered(
    dir: TempDir,
    go_first: bool,
    foreign_name_guard: bool,
    fs_skip: &[&str],
) -> anyhow::Result<Workspace> {
    let go_bin = go_bin_path();
    // `fs` is auto-registered by `Engine::new`.
    let mut b = WorkspaceBuilder::from_dir(dir).with_fs_skip(fs_skip.iter().copied());

    if go_first {
        b = b.with_provider(move |init| {
            Box::new(
                plugingo::Provider::with_config(
                    init.root.to_path_buf(),
                    plugingo::Config {
                        foreign_name_guard,
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
        b = b.with_provider(move |init| {
            Box::new(
                plugingo::Provider::with_config(
                    init.root.to_path_buf(),
                    plugingo::Config {
                        foreign_name_guard,
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
