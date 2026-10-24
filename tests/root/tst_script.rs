//! `tst` must run every pass and fail if any did.
//!
//! The script lives in `devenv.nix` and has no test of its own otherwise: a
//! regression to `|| true` would turn CI green over a red suite, and one back
//! to `&&` would hide everything after the first failing pass again. Both are a
//! one-word edit. This runs the script's own text against a stub `cargo` that
//! records each call and fails the one it is told to.

use std::path::Path;
use std::process::Command;

/// The body of `scripts.tst.exec`, exactly as devenv runs it.
fn tst_script() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("devenv.nix");
    let devenv = std::fs::read_to_string(&path).expect("read devenv.nix");
    let (_, after) = devenv
        .split_once("scripts.tst.exec = ''")
        .expect("devenv.nix defines scripts.tst.exec as a '' string");
    let (body, _) = after.split_once("'';").expect("the tst script is closed");
    body.to_string()
}

/// Run `tst` with a stub `cargo` that exits 1 on the call numbered `fail_on`
/// (1-based; 0 never fails). Returns the exit code and each call's arguments.
fn run_tst(fail_on: usize, args: &[&str]) -> (i32, Vec<String>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("calls");
    let stub = dir.path().join("cargo");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{log}'\nn=$(wc -l < '{log}')\n[ \"$n\" -eq {fail_on} ] && exit 1\nexit 0\n",
            log = log.display(),
        ),
    )
    .expect("write stub cargo");
    let mut perms = std::fs::metadata(&stub).expect("stat stub").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&stub, perms).expect("chmod stub");
    let script = dir.path().join("tst");
    std::fs::write(&script, tst_script()).expect("write script");

    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let status = Command::new("bash")
        .arg(&script)
        .args(args)
        .env("PATH", path)
        .status()
        .expect("run bash");
    let calls = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    (status.code().unwrap_or(-1), calls)
}

#[test]
fn every_pass_runs_and_any_failure_fails_the_script() {
    for fail_on in 1..=3 {
        let (code, calls) = run_tst(fail_on, &[]);
        assert_eq!(
            calls.len(),
            3,
            "pass {fail_on} failing stopped the rest: {calls:?}"
        );
        assert_ne!(code, 0, "pass {fail_on} failed and tst exited 0");
    }
    let (code, calls) = run_tst(0, &[]);
    assert_eq!((code, calls.len()), (0, 3));
}

#[test]
fn every_pass_keeps_going_and_gets_the_extra_args() {
    let (_, calls) = run_tst(0, &["some_filter"]);
    for call in &calls {
        assert!(call.contains("--no-fail-fast"), "{call}");
        assert!(call.ends_with("some_filter"), "{call}");
    }
}
