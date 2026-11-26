//! `heph tool codegen … --json` at the process boundary: findings (exit 1)
//! still print their record, a failure (exit 2) leaves stdout empty and says
//! why on stderr, and a `--jq` filter that fails while it runs is exit 2,
//! never `--check`'s 1.
//!
//! Drives the built `heph` binary, because what is under test is the exit
//! status `main` turns a `CodegenExit` into and what reaches each stream.

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A `copy` target writing `gen.txt` in package `p`: with no section yet,
/// `gitignore --check` finds it stale.
const COPY_TARGET: &str = r#"
target(
    name = "gen",
    driver = "bash",
    run = "printf v1 > $OUT",
    out = "gen.txt",
    codegen = "copy",
)
"#;

fn workspace() -> tempfile::TempDir {
    let ws = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        ws.path().join(".hephconfig2"),
        "plugins:\n  - builtin: buildfile\n  - builtin: exec\n  - builtin: bash\n",
    )
    .expect("write config");
    std::fs::create_dir_all(ws.path().join("p")).expect("mkdir p");
    std::fs::write(ws.path().join("p/BUILD"), COPY_TARGET).expect("write BUILD");
    ws
}

fn run(ws: &Path, args: &[&str]) -> Output {
    let home = tempfile::tempdir().expect("home");
    Command::new(env!("CARGO_BIN_EXE_heph"))
        .args(args)
        .current_dir(ws)
        .env("HOME", home.path())
        .env("HEPH_CWD", ws)
        .env("HEPH_NO_SELF_UPDATE", "1")
        .env("HEPH_DISABLE_TELEMETRY", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run heph")
}

fn describe(out: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const CHECK: [&str; 4] = ["tool", "codegen", "gitignore", "--check"];

#[test]
fn findings_print_the_record_and_exit_1() {
    let ws = workspace();
    let out = run(ws.path(), &[&CHECK[..], &["--json"]].concat());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.lines().count(),
        1,
        "piped: one line\n{}",
        describe(&out)
    );
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON record ({e}): {}", describe(&out)));
    assert_eq!(doc["stale"], true, "{doc}");
    assert_eq!(doc["written"], false, "{doc}");
    assert!(doc.get("schema").is_none(), "no envelope: {doc}");

    let out = run(ws.path(), &[&CHECK[..], &["--jq", ".reasons[]"]].concat());
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "lines\n",
        "{}",
        describe(&out)
    );
}

#[test]
fn a_failure_leaves_stdout_empty_and_exits_2() {
    let ws = workspace();
    std::fs::create_dir_all(ws.path().join("q")).expect("mkdir q");
    std::fs::write(ws.path().join("q/BUILD"), "target(name = \n").expect("broken BUILD");
    let out = run(ws.path(), &[&CHECK[..], &["--json"]].concat());
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(out.stdout.is_empty(), "{}", describe(&out));
    assert!(
        !out.stderr.is_empty(),
        "the error is on stderr: {}",
        describe(&out)
    );
}

#[test]
fn a_jq_runtime_error_exits_2_not_findings() {
    let ws = workspace();
    let out = run(
        ws.path(),
        &[&CHECK[..], &["--jq", ".stale | error"]].concat(),
    );
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(out.stdout.is_empty(), "{}", describe(&out));
}
