//! `heph tool gen-gitignore` was removed with no alias (Decision 7): a hidden
//! stub keeps the name only to fail with exit 2 and name its replacement,
//! forwarding `--json`.
//!
//! Drives the built `heph` binary, because what is under test is the
//! process's exit status and what it prints.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

fn run(args: &[&str]) -> (Output, tempfile::TempDir) {
    let ws = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        ws.path().join(".hephconfig2"),
        "plugins:\n  - builtin: buildfile\n  - builtin: exec\n",
    )
    .expect("write config");
    let out = Command::new(heph_bin())
        .args(args)
        .current_dir(ws.path())
        .env("HOME", home.path())
        .env("HEPH_CWD", ws.path())
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph");
    (out, ws)
}

fn describe(out: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn gen_gitignore_removed_fails_with_hint() {
    for args in [
        &["tool", "gen-gitignore"][..],
        // An old invocation with arguments still reaches the message.
        &["tool", "gen-gitignore", "//pkg/..."][..],
    ] {
        let (out, ws) = run(args);
        assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("heph tool gen-gitignore was removed")
                && stderr.contains("use heph tool codegen gitignore"),
            "{}",
            describe(&out)
        );
        assert!(
            !ws.path().join(".gitignore").exists(),
            "the stub writes nothing"
        );
    }
}

#[test]
fn gen_gitignore_stub_forwards_json() {
    let (out, _ws) = run(&["tool", "gen-gitignore", "--json"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is one JSON document ({e}): {}", describe(&out)));
    assert_eq!(
        doc,
        serde_json::json!({
            "schema": 1,
            "error": "command_removed",
            "use": "heph tool codegen gitignore",
        })
    );
}
