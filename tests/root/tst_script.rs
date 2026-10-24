//! `tst` must run every pass and fail if any did.
//!
//! The script lives in `devenv.nix` and has no test of its own otherwise: a
//! regression to `|| true` would turn CI green over a red suite, and one back
//! to `&&` would hide everything after the first failing pass again. Both are a
//! one-word edit. This runs the script's own text against a stub `cargo` that
//! records each call and fails the one it is told to.
//!
//! Each of the three passes is two calls — `cargo nextest run`, then
//! `cargo test --doc`, because nextest does not run doctests — so a full `tst`
//! is six calls.

use std::path::Path;
use std::process::Command;

/// Calls per `tst`: three passes, each a nextest run and a doctest run.
const CALLS: usize = 6;

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
///
/// `TST_SCOPE` is cleared unless `scope` sets it: CI runs this test inside a
/// step that exports one, and the default-scope assertions must not read it.
fn run_tst(fail_on: usize, args: &[&str], scope: Option<&str>) -> (i32, Vec<String>) {
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
    let mut cmd = Command::new("bash");
    cmd.arg(&script).args(args).env("PATH", path);
    match scope {
        Some(scope) => cmd.env("TST_SCOPE", scope),
        None => cmd.env_remove("TST_SCOPE"),
    };
    let status = cmd.status().expect("run bash");
    let calls = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    (status.code().unwrap_or(-1), calls)
}

#[test]
fn every_pass_runs_and_any_failure_fails_the_script() {
    for fail_on in 1..=CALLS {
        let (code, calls) = run_tst(fail_on, &[], None);
        assert_eq!(
            calls.len(),
            CALLS,
            "call {fail_on} failing stopped the rest: {calls:?}"
        );
        assert_ne!(code, 0, "call {fail_on} failed and tst exited 0");
    }
    let (code, calls) = run_tst(0, &[], None);
    assert_eq!((code, calls.len()), (0, CALLS));
}

#[test]
fn every_pass_keeps_going_and_gets_the_extra_args() {
    let (_, calls) = run_tst(0, &["some_filter", "--", "--exact"], None);
    assert_eq!(calls.len(), CALLS, "{calls:?}");
    for call in &calls {
        assert!(call.contains("--no-fail-fast"), "{call}");
        assert!(call.ends_with("some_filter -- --exact"), "{call}");
    }
}

/// Each pass is a nextest run followed by a doctest run over the same crates.
/// Dropping the doc call loses the doctests silently: nextest does not run them.
#[test]
fn each_pass_runs_nextest_then_the_doctests_of_the_same_scope() {
    let (_, calls) = run_tst(0, &[], None);
    for pair in calls.chunks(2) {
        let [nextest, doc] = pair else {
            panic!("an odd number of calls: {calls:?}");
        };
        let nextest_scope = nextest
            .strip_prefix("nextest run ")
            .unwrap_or_else(|| panic!("not a nextest run: {nextest}"));
        let doc_scope = doc
            .strip_prefix("test ")
            .unwrap_or_else(|| panic!("not a cargo test: {doc}"));
        assert!(doc_scope.contains("--doc"), "{doc}");
        // The package selection is everything after the shared flags; the doc
        // call must select at least what the nextest call did.
        let selection = |call: &str| -> Vec<String> {
            call.split_whitespace()
                .filter(|w| !w.starts_with("--no-") && *w != "--locked" && *w != "--doc")
                .map(str::to_string)
                .collect()
        };
        assert_eq!(
            selection(nextest_scope),
            selection(doc_scope),
            "the doctest call does not cover the nextest call's crates"
        );
    }
}

/// A bare `tst` that runs no test at all is a broken selection, not a pass. A
/// filter, though, legitimately matches nothing in the two one-crate feature
/// passes, and must not fail them.
#[test]
fn no_tests_fails_a_bare_run_and_warns_under_a_filter() {
    let nextest_calls = |calls: Vec<String>| -> Vec<String> {
        calls
            .into_iter()
            .filter(|c| c.starts_with("nextest run"))
            .collect()
    };

    let bare = nextest_calls(run_tst(0, &[], None).1);
    assert_eq!(bare.len(), 3, "{bare:?}");
    for call in &bare {
        assert!(call.contains("--no-tests=fail"), "{call}");
    }

    let filtered = nextest_calls(run_tst(0, &["some_filter"], None).1);
    assert_eq!(filtered.len(), 3, "{filtered:?}");
    for call in &filtered {
        assert!(call.contains("--no-tests=warn"), "{call}");
    }
}

#[test]
fn the_default_first_pass_is_the_workspace_without_bin_e2e() {
    let (_, calls) = run_tst(0, &[], None);
    for call in calls.iter().take(2) {
        assert!(call.contains(" --workspace --exclude bin-e2e"), "{call}");
    }
}

/// `TST_SCOPE` replaces the first pass's crates — both its calls — and leaves
/// the feature passes alone.
#[test]
fn tst_scope_replaces_the_first_pass_scope_and_only_that() {
    let (code, calls) = run_tst(0, &["f"], Some("-p engine -p e2e"));
    assert_eq!((code, calls.len()), (0, CALLS), "{calls:?}");
    for call in calls.iter().take(2) {
        assert!(call.contains(" -p engine -p e2e f"), "{call}");
        assert!(!call.contains("--workspace"), "{call}");
    }
    for call in calls.iter().skip(2) {
        assert!(!call.contains("-p engine"), "{call}");
        assert!(
            call.contains("-p plugin-stabby --features host")
                || call.contains("-p plugin-sdk --features stabby"),
            "{call}"
        );
    }
}
