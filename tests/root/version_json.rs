//! `heph version --json`, the pilot of the output framework, end to end
//! through the binary: the document, `--fields help` from `main`, a bad
//! `--fields`/`--jq` at parse time, and a `--jq` runtime error's exit 2 from
//! `exit_code_for`. Outside a workspace, as `version` must work there.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn heph(args: &[&str]) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_heph")))
        .arg("version")
        .args(args)
        .current_dir(dir.path())
        .env("HOME", home.path())
        .env("HEPH_CWD", dir.path())
        .env_remove("HEPH_PROFILES")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .env("HEPH_NO_SESSION", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("utf-8 stdout")
}

#[test]
fn version_json_is_one_compact_record() {
    let out = heph(&["--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "piped: one compact line: {text}");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(doc["output_contract"], 2);
    assert!(doc["version"].is_string(), "{doc}");
    // The text output is unchanged and agrees.
    let plain = heph(&[]);
    assert_eq!(
        stdout(&plain).trim_end(),
        doc["version"].as_str().unwrap_or_default()
    );
}

#[test]
fn version_jq_and_fields() {
    let out = heph(&["--jq", ".output_contract"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "2\n");

    let out = heph(&["--fields", "output_contract"]);
    assert_eq!(stdout(&out), "{\"output_contract\":2}\n");

    let out = heph(&["--fields", "help"]);
    assert_eq!(out.status.code(), Some(0));
    let table = stdout(&out);
    assert!(table.contains("heph version --json"), "{table}");
    assert!(table.contains("output_contract"), "{table}");
}

#[test]
fn version_output_errors_exit_2() {
    for args in [
        &["--fields", "nope"][..],
        &["--jq", ".a |"],
        &["--jq", "env"],
        // A runtime error: through `exit_code_for`, not clap.
        &["--jq", "error(\"boom\")"],
    ] {
        let out = heph(args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(stdout(&out), "", "{args:?}");
    }
}
