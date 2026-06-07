use anyhow::Context as _;
use heph::pluginbuildfile;
use heph::pluginexec;
use heph::plugingo;
use heph::pluginstatictarget;
use heph_testkit::{Workspace, WorkspaceBuilder, copy_dir_to_tempdir};
use std::path::PathBuf;
use tempfile::TempDir;

pub use heph_testkit::{artifact_paths, artifact_string};

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
    let go_bin = go_bin_path();
    // `fs` is auto-registered by `Engine::new`.
    WorkspaceBuilder::from_dir(dir)
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
        })
        .with_provider(|init| {
            Box::new(plugingo::Provider::new(init.root.to_path_buf()).expect("plugingo provider"))
        })
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .with_managed_driver(Box::new(pluginexec::Driver::new_sh()))
        .with_managed_driver(Box::new(pluginexec::Driver::new_exec()))
        .with_managed_driver(Box::new(plugingo::GoGolistDriver::new("//@heph/bin:go")))
        .with_managed_driver(Box::new(plugingo::GoEmbedDriver))
        .build()
        .context("build plugingo workspace")
}
