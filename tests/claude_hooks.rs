//! Guards on `.claude/hooks/gate`, the PreToolUse hook that stops an agent
//! from running `tst`/`e2e` or polling with `sleep` unless it opts in.
//!
//! The decision logic is tested in Go, next to it (`gate_test.go`); this runs
//! that suite so CI — which runs `tst`, not `go test` — covers it, and then
//! drives the hook exactly as Claude Code does: the command string from
//! `.claude/settings.json`, through `sh`, JSON on stdin, decision on stdout.
//! That second half is what catches a hook that compiles and passes its unit
//! tests but is wired wrong, or quietly needs the network.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn gate_dir() -> PathBuf {
    root().join(".claude/hooks/gate")
}

#[test]
fn go_unit_tests_pass() {
    let out = Command::new("go")
        .args(["test", "-mod=vendor", "./..."])
        .current_dir(gate_dir())
        .env("GOWORK", "off")
        .env("GOPROXY", "off")
        .output()
        .expect("run go test (go is provided by devenv)");
    assert!(
        out.status.success(),
        "go test failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The command registered for PreToolUse(Bash) in `.claude/settings.json`.
fn hook_command() -> String {
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root().join(".claude/settings.json")).expect("read settings"),
    )
    .expect("parse settings");
    settings
        .pointer("/hooks/PreToolUse")
        .and_then(|v| v.as_array())
        .expect("PreToolUse hooks")
        .iter()
        .filter(|m| m.get("matcher").is_some_and(|v| v == "Bash"))
        .flat_map(|m| {
            m.get("hooks")
                .and_then(|v| v.as_array())
                .expect("hooks")
                .iter()
        })
        .map(|h| {
            h.get("command")
                .and_then(|v| v.as_str())
                .expect("command")
                .to_owned()
        })
        .find(|c| c.contains(".claude/hooks/gate"))
        .expect("the gate is registered as a PreToolUse(Bash) hook")
}

fn run_hook(command: &str) -> (Option<i32>, String) {
    let input = serde_json::json!({ "tool_name": "Bash", "tool_input": { "command": command } });
    let mut child = Command::new("sh")
        .args(["-c", &hook_command()])
        .env("CLAUDE_PROJECT_DIR", root())
        // Vendored: the hook must never need the network at tool-call time.
        .env("GOPROXY", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.to_string().as_bytes())
        .expect("write hook input");
    let out = child.wait_with_output().expect("wait hook");
    assert!(
        out.stderr.is_empty(),
        "hook wrote to stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn registered_hook_denies_and_allows() {
    let (code, stdout) = run_hook("cd crates && e2e --test tui_pty");
    assert_eq!(
        code,
        Some(0),
        "a decision is JSON on stdout, not an exit code"
    );
    let decision: serde_json::Value = serde_json::from_str(&stdout).expect("hook output is JSON");
    assert_eq!(
        decision.pointer("/hookSpecificOutput/permissionDecision"),
        Some(&serde_json::json!("deny"))
    );
    assert!(
        decision
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(|v| v.as_str())
            .is_some_and(|r| r.contains("HEPH_FULL_SUITE=1")),
        "the reason names the opt-in: {stdout}"
    );

    let (code, stdout) = run_hook("HEPH_FULL_SUITE=1 e2e");
    assert_eq!((code, stdout.as_str()), (Some(0), ""));
}
