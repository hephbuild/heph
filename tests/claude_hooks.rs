//! Guards on `.claude/hooks/gate-heavy-commands.sh`, the PreToolUse hook that
//! stops an agent from running `tst`/`e2e` or polling with `sleep` unless it
//! opts in.
//!
//! Both directions matter. A gate that misses `cd x && e2e` is a release
//! build nobody asked for; a gate that fires on `cargo test -p e2e` or a
//! commit message mentioning `tst` blocks ordinary work, and the agent learns
//! to route around it.
//!
//! Runs the script under `/bin/bash` where it exists — on macOS that is bash
//! 3.2, the oldest shell the hook has to survive.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run_hook(command: &str) -> (i32, String) {
    let input = serde_json::json!({ "tool_name": "Bash", "tool_input": { "command": command } });
    run_hook_raw(&input.to_string())
}

fn run_hook_raw(input: &str) -> (i32, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/hooks/gate-heavy-commands.sh");
    let bash = if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "bash"
    };

    let mut child = Command::new(bash)
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write hook input");
    let out = child.wait_with_output().expect("wait hook");
    (
        out.status.code().expect("hook exited by signal"),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn blocks_full_suite_and_polling() {
    for cmd in [
        "tst",
        "e2e",
        "cd crates && e2e --test tui_pty",
        "timeout 600 tst 2>&1 | tail -5",
        "devenv shell -- e2e",
        "FOO=1 e2e",
        "echo $(tst)",
        "(cd a; e2e)",
        "sleep 45; gh pr checks 1",
        "sleep 60s",
        // A heredoc ends; what follows it is a command again.
        "cat <<EOF > f\nx\nEOF\ntst",
        "grep x <<< 'here string' && e2e",
    ] {
        let (code, stderr) = run_hook(cmd);
        assert_eq!(code, 2, "should block {cmd:?}");
        assert!(
            stderr.starts_with("Blocked:"),
            "{cmd:?} gave no reason: {stderr:?}"
        );
    }
}

#[test]
fn allows_opt_in_and_lookalikes() {
    for cmd in [
        "HEPH_FULL_SUITE=1 tst",
        "HEPH_FULL_SUITE=1 e2e --test tui_pty",
        "cargo test -p e2e some_test",
        "cargo test --test tst",
        "ls crates/bin-e2e",
        "git commit -m 'make tst faster'",
        "until [ -f x ]; do sleep 5; done",
        "sleep 10",
        "lint",
        // Heredoc bodies are data, not commands — this one is how the hook
        // first misfired, on a CLAUDE.md edit.
        "python3 - <<'EOF'\ntst\n`e2e` runs on every push\nsleep 99\nEOF\necho done",
        "cat > f <<-EOF\n\te2e\n\tEOF",
        "git commit -m 'the `tst` suite'",
    ] {
        let (code, stderr) = run_hook(cmd);
        assert_eq!(code, 0, "should allow {cmd:?}: {stderr}");
    }
}

/// The hook must fail open: a hook that errors on input it does not
/// understand would block every Bash call in the session.
#[test]
fn unparseable_input_is_allowed() {
    for input in ["", "not json", "{}", r#"{"tool_input":{}}"#] {
        let (code, stderr) = run_hook_raw(input);
        assert_eq!(code, 0, "input {input:?}: {stderr}");
    }
}
