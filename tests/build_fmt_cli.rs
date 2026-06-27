//! End-to-end coverage for `heph tool build-fmt`: the stdin (`-`) mode, the
//! read-only `--check` mode (non-zero exit when unformatted), and in-place
//! rewriting (idempotent — a second run is clean).
//!
//! These drive the built `heph` binary against a temp workspace, so they need
//! the compiled binary and a valid config — heavier than a unit test. They are
//! `#[ignore]`d by default (the formatter's behavior is covered env-free by the
//! `xstarlark-fmt` golden tests and the `plugin-buildfile` unit tests); run them
//! explicitly with `cargo test -p heph --test build_fmt_cli -- --ignored`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn heph_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heph"))
}

/// A minimal heph workspace using the filesystem buildfile provider with its
/// default patterns (`BUILD` and `*.BUILD`).
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(".hephconfig2"),
        "plugins:\n  - builtin: buildfile\n  - builtin: exec\n",
    )
    .expect("write config");
    dir
}

const MESSY: &str = "target(name=\"a\",deps=[\"x\",\"y\"])\n";
// Default indent is 4 spaces; these workspaces don't configure `indent`.
const FORMATTED: &str = "target(\n    name = \"a\",\n    deps = [\"x\", \"y\"],\n)\n";

/// Run `heph tool build-fmt -` feeding `input` on stdin; return stdout.
fn run_stdin(input: &str) -> String {
    let mut child = Command::new(heph_bin())
        .args(["tool", "build-fmt", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "stdin mode should succeed");
    String::from_utf8(out.stdout).expect("utf8")
}

#[test]
#[ignore = "drives the built heph binary against a temp workspace; run with --ignored"]
fn stdin_mode_formats() {
    assert_eq!(run_stdin(MESSY), FORMATTED);
}

#[test]
#[ignore = "drives the built heph binary against a temp workspace; run with --ignored"]
fn stdin_mode_passes_through_skip_file() {
    let src = "# heph:fmt skip-file\ntarget(  name=\"a\"  )\n";
    assert_eq!(run_stdin(src), src);
}

#[test]
#[ignore = "drives the built heph binary against a temp workspace; run with --ignored"]
fn check_mode_reports_unformatted_and_fails() {
    let ws = workspace();
    std::fs::write(ws.path().join("BUILD"), MESSY).expect("write BUILD");

    let out = Command::new(heph_bin())
        .args(["tool", "build-fmt", "--check", "--no-tui"])
        .current_dir(ws.path())
        .output()
        .expect("run");

    assert!(
        !out.status.success(),
        "check should fail on unformatted file"
    );
    // The unformatted BUILD must remain untouched in check mode.
    assert_eq!(
        std::fs::read_to_string(ws.path().join("BUILD")).unwrap(),
        MESSY
    );
}

#[test]
#[ignore = "drives the built heph binary against a temp workspace; run with --ignored"]
fn in_place_rewrites_and_is_idempotent() {
    let ws = workspace();
    let build = ws.path().join("BUILD");
    std::fs::write(&build, MESSY).expect("write BUILD");

    let out = Command::new(heph_bin())
        .args(["tool", "build-fmt", "--no-tui"])
        .current_dir(ws.path())
        .output()
        .expect("run");
    assert!(out.status.success(), "in-place run should succeed");
    assert_eq!(std::fs::read_to_string(&build).unwrap(), FORMATTED);

    // A second run is a clean no-op: --check now passes.
    let check = Command::new(heph_bin())
        .args(["tool", "build-fmt", "--check", "--no-tui"])
        .current_dir(ws.path())
        .output()
        .expect("run");
    assert!(check.status.success(), "formatted tree should pass --check");
}

#[test]
#[ignore = "drives the built heph binary against a temp workspace; run with --ignored"]
fn formats_dot_build_files_by_default() {
    // `*.BUILD` is a default-supported pattern: a `lib.BUILD` file in a package
    // must be formatted alongside a plain `BUILD`.
    let ws = workspace();
    std::fs::create_dir(ws.path().join("pkg")).expect("mkdir");
    let dot_build = ws.path().join("pkg").join("lib.BUILD");
    std::fs::write(&dot_build, MESSY).expect("write lib.BUILD");

    let out = Command::new(heph_bin())
        .args(["tool", "build-fmt", "--no-tui"])
        .current_dir(ws.path())
        .output()
        .expect("run");
    assert!(out.status.success(), "in-place run should succeed");
    assert_eq!(std::fs::read_to_string(&dot_build).unwrap(), FORMATTED);
}
