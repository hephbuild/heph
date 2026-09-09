//! `heph __auth-helper` as a **process**.
//!
//! This is here rather than in `crates/e2e` because none of it has an in-process
//! form. The helper is dispatched *before* the argument parser, from `argv_os`,
//! so nothing that goes through clap can exercise it; it is invoked by a foreign
//! tool that owns its argv and its stdin; and its whole contract is about the
//! process — stdout is somebody else's document, stderr is the diagnostic, and
//! the exit status is what the calling SDK branches on.
//!
//! It is also the only way to reach the real callback at all: the config heph
//! writes names `std::env::current_exe()`, which inside an in-process test is the
//! test harness rather than `heph`.

mod common;

use common::{Dist, Workspace, describe};
use std::io::Write as _;
use std::process::Stdio;

/// A credential presented through the git helper, plus a target that copies the
/// pin heph wrote out to a path the test can read afterwards.
///
/// The chain is a `printf`: no network, no vendor CLI, and material that is the
/// same on every host. The pin's own path is the tail of the `GIT_CONFIG_VALUE_0`
/// heph injected, which is how the fixture finds it without heph needing a
/// command that prints one.
fn fixture(dest: &std::path::Path) -> String {
    format!(
        r#"
target(
    name    = "cred",
    driver  = "credential",
    sources = [heph.auth.exec(
        ["sh", "-c", "printf '{{\"token\":\"bin-e2e-material\",\"username\":\"u\",\"expires_in\":3600}}'"],
        fields  = {{"token": "token", "username": "username"}},
        expires = "expires_in",
    )],
    present = heph.auth.git(["git.example"]),
)

target(
    name        = "copy-pin",
    driver      = "bash",
    credentials = ["//auth:cred"],
    cache       = False,
    out         = [],
    run         = ["cp \"${{GIT_CONFIG_VALUE_0##*--pin }}\" {dest}"],
)
"#,
        dest = dest.display()
    )
}

/// Run the fixture and return a durable copy of the pin.
///
/// The pin itself lives inside that run's sandbox and is deleted when the target
/// finishes — that lifetime *is* the design, a presented credential is destroyed
/// with its run — so the fixture copies it out rather than the test weakening it.
fn staged_pin(ws: &Workspace, dist: &Dist) -> std::path::PathBuf {
    let dest = ws.root().join("pin-copy.json");
    ws.write("auth/BUILD", &fixture(&dest))
        .expect("write BUILD");
    let out = ws.run(dist, &["run", "//auth:copy-pin"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        dest.is_file(),
        "the run did not produce a pin: {}",
        describe(&out)
    );
    dest
}

/// The git dialect, end to end as a process: heph's own argv, the calling tool's
/// stdin, and `key=value` framing on stdout.
#[test]
fn the_git_helper_answers_the_protocol_as_a_process() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let pin = staged_pin(&ws, &dist);

    let mut child = ws
        .cmd(
            &dist,
            &[
                "__auth-helper",
                "git",
                "--pin",
                &pin.to_string_lossy(),
                "get",
            ],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the helper");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"protocol=https\nhost=git.example\n\n")
        .expect("write the git request");
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "{}", describe(&out));

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains("password=bin-e2e-material"), "{stdout}");
    assert!(stdout.contains("username=u"), "{stdout}");
    assert!(stdout.contains("password_expiry_utc="), "{stdout}");
    // git reads until a blank line.
    assert!(stdout.ends_with("\n\n"), "{stdout:?}");
    // Nothing but the protocol on stdout, and no noise on stderr — a helper's
    // stdout is parsed by somebody else's implementation.
    assert!(
        String::from_utf8_lossy(&out.stderr).trim().is_empty(),
        "{}",
        describe(&out)
    );
}

/// The write half of the protocol succeeds and does nothing. A tool must not be
/// able to substitute an identity into heph's store.
#[test]
fn store_and_erase_are_accepted_and_change_nothing() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    for op in ["store", "erase"] {
        // Deliberately a path that does not exist: these must return before
        // reading the pin at all.
        let out = ws
            .run(
                &dist,
                &["__auth-helper", "git", "--pin", "/nonexistent/pin.json", op],
            )
            .expect("run");
        assert!(out.status.success(), "{op}: {}", describe(&out));
        assert!(out.stdout.is_empty(), "{op}: {}", describe(&out));
    }
}

/// A helper invoked after its run has finished — the pin is gone with the
/// sandbox — fails with a diagnostic on **stderr** and a non-zero status, not
/// with half a document on stdout.
#[test]
fn a_missing_pin_fails_on_stderr_and_writes_nothing_to_stdout() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let out = ws
        .run(
            &dist,
            &["__auth-helper", "aws", "--pin", "/nonexistent/pin.json"],
        )
        .expect("run");
    assert!(!out.status.success(), "{}", describe(&out));
    assert!(
        out.stdout.is_empty(),
        "stdout is the tool's document: {}",
        describe(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("credential pin"), "{stderr}");
}

/// The dispatch happens before the argument parser, so `__auth-helper` is not a
/// clap subcommand and cannot be shadowed by one.
#[test]
fn the_helper_is_not_a_visible_subcommand() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    let out = ws.run(&dist, &["--help"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !help.contains("__auth-helper"),
        "a hidden subcommand must not be advertised: {help}"
    );
    assert!(help.contains("auth"), "but `heph auth` is: {help}");
}

// ---------------------------------------------------------------------------
// `heph auth` as a command — the exit status is the contract
// ---------------------------------------------------------------------------

/// The preflight's whole point: a caller branches on the **exit status** and only
/// then parses to learn what to run. That is an exit code, so it is only
/// observable here.
#[test]
fn auth_status_exits_non_zero_when_a_credential_is_unavailable() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "auth/BUILD",
        r#"
target(name = "ok", driver = "credential",
       sources = [heph.auth.exec(["sh", "-c", "printf 'v'"])],
       present = {"env": {"OK": "${value}"}})
target(name = "missing", driver = "credential",
       sources = [heph.auth.env(["HEPH_BIN_E2E_SURELY_UNSET"])],
       present = {"env": {"M": "${heph_bin_e2e_surely_unset}"}})
"#,
    )
    .expect("write BUILD");

    let out = ws
        .run(&dist, &["auth", "status", "//auth/...", "--json"])
        .expect("run");
    assert!(
        !out.status.success(),
        "one unavailable credential must fail the preflight: {}",
        describe(&out)
    );
    // …and the JSON is still on stdout, so the same invocation answers both
    // "is everything fine?" and "what do I do about it?".
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let rows: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("--json must be parseable ({e}): {}", describe(&out)));
    let rows = rows.as_array().expect("an array of rows");
    assert_eq!(rows.len(), 2, "{stdout}");
    let missing = rows
        .iter()
        .find(|r| r["addr"] == "//auth:missing")
        .expect("the unavailable row");
    assert_eq!(missing["state"], "unavailable", "{stdout}");
    let ok = rows
        .iter()
        .find(|r| r["addr"] == "//auth:ok")
        .expect("the available row");
    assert_eq!(ok["state"], "ok", "{stdout}");
}

/// Every row available means exit 0, so `heph auth status && heph run …` is a
/// usable shape.
#[test]
fn auth_status_exits_zero_when_every_credential_applies() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "auth/BUILD",
        r#"target(name = "ok", driver = "credential",
       sources = [heph.auth.exec(["sh", "-c", "printf 'v'"])],
       present = {"env": {"OK": "${value}"}})"#,
    )
    .expect("write BUILD");
    let out = ws
        .run(&dist, &["auth", "status", "//auth/..."])
        .expect("run");
    assert!(out.status.success(), "{}", describe(&out));
}

/// A workspace with no credentials is not a failure — there is nothing
/// unavailable — and it says so rather than printing an empty table.
#[test]
fn auth_status_on_a_workspace_with_no_credentials_succeeds() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "app/BUILD",
        r#"target(name = "a", driver = "bash", run = "true", out = [])"#,
    )
    .expect("write BUILD");
    let out = ws.run(&dist, &["auth", "status"]).expect("run");
    assert!(out.status.success(), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no `credential` targets"),
        "{}",
        describe(&out)
    );
}

/// `explain` prints the walk and fails when nothing applies — the same structure
/// the build failure prints, so the diagnostic and the failure cannot drift.
#[test]
fn auth_explain_prints_the_walk_and_fails_when_nothing_applies() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    ws.write(
        "auth/BUILD",
        r#"target(name = "t", driver = "credential",
       sources = [
           heph.auth.oidc("github_actions", audience = "x"),
           heph.auth.env(["HEPH_BIN_E2E_SURELY_UNSET"]),
       ],
       present = {"env": {"T": "${id_token}"}})"#,
    )
    .expect("write BUILD");
    let out = ws
        .run(&dist, &["auth", "explain", "//auth:t"])
        .expect("run");
    assert!(!out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(stdout.contains("oidc(github_actions)"), "{stdout}");
    assert!(stdout.contains("skipped:"), "{stdout}");
    assert!(
        stdout.contains("id-token: write"),
        "the fix rides on the line that failed: {stdout}"
    );
}

/// `logout` on a store that was never written is a no-op, not an error.
#[test]
fn auth_logout_is_idempotent() {
    let dist = Dist::locate();
    let ws = Workspace::new().expect("workspace");
    for _ in 0..2 {
        let out = ws.run(&dist, &["auth", "logout"]).expect("run");
        assert!(out.status.success(), "{}", describe(&out));
    }
}
