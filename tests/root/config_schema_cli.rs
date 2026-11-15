//! `heph tool config-schema` prints the `.hephconfig` JSON Schema, from
//! anywhere: outside a workspace, and inside one whose config does not load.
//!
//! Drives the built `heph` binary, because what is under test is that the
//! process gets past the self-upgrade's workspace check and what it prints.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

/// `heph tool config-schema` run in `cwd`. The self-upgrade is left on: its
/// refusal to run outside a workspace is part of what is tested.
fn run_in(cwd: &Path) -> Output {
    let home = tempfile::tempdir().expect("home");
    Command::new(heph_bin())
        .args(["tool", "config-schema"])
        .current_dir(cwd)
        .env("HOME", home.path())
        .env("HEPH_CWD", cwd)
        .env_remove("HEPH_NO_SELF_UPDATE")
        .env_remove("HEPH_PROFILES")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .env("HEPH_NO_SESSION", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph")
}

fn golden() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/hephconfig.schema.json");
    std::fs::read_to_string(path).expect("read docs/hephconfig.schema.json")
}

fn assert_prints_the_schema(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout) == golden(),
        "stdout is not docs/hephconfig.schema.json:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn prints_the_schema_outside_a_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_prints_the_schema(&run_in(dir.path()));
}

#[test]
fn prints_the_schema_without_loading_the_config() {
    // A config with a key the parser refuses: a command that loaded it would
    // fail, and an editor asking for the schema is often looking at exactly
    // such a file.
    let ws = tempfile::tempdir().expect("tempdir");
    std::fs::write(ws.path().join(".hephconfig"), "bogus: 1\n").expect("write config");
    assert_prints_the_schema(&run_in(ws.path()));
}
