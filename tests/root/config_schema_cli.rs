//! `heph tool config-schema` exits 0 and prints exactly
//! `config_json_schema_text()`, outside a workspace and next to a config with
//! a key this binary does not know.
//!
//! A cargo-built `heph` reports `DEV_VERSION`, so its self-upgrade opts out
//! before it looks for a workspace: these tests do not reach
//! `no_config_tolerable`, whose predicate the unit tests in `src/main.rs`
//! cover. What they cover is the command itself: it reads no config, and its
//! stdout is the rendered schema byte for byte.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

/// `heph tool config-schema` run in `cwd`, the self-upgrade left on as a user
/// would have it.
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

fn assert_prints_the_schema(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected =
        heph::engine::config_yaml::config_json_schema_text().expect("render the config schema");
    assert!(
        String::from_utf8_lossy(&out.stdout) == expected,
        "stdout is not config_json_schema_text():\n{}",
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
    // A config with a key this binary does not know: a command that loaded
    // it would fail, and an editor asking for the schema is often looking at
    // exactly such a file.
    let ws = tempfile::tempdir().expect("tempdir");
    std::fs::write(ws.path().join(".hephconfig"), "bogus: 1\n").expect("write config");
    assert_prints_the_schema(&run_in(ws.path()));
}
