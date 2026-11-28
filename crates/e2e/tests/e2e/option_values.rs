//! Option values that used to be accepted and silently meant something else
//! (error-reporting D8): each is refused at parse, before anything runs.

use crate::common::Workspace;
use crate::source_location::{fail_in, text};

/// The one failure of requesting `//p:t` when `p/BUILD` holds `build`, as text.
async fn fails(build: &str) -> String {
    let ws = Workspace::new();
    ws.write_build_file("p", build);
    ws.write_file("q/q.txt", "q");
    let rs = ws.engine.new_state_with_events(false, None);
    let failures = fail_in(&ws, &rs, &["//p:t"]).await;
    assert_eq!(failures.len(), 1, "one failure");
    failures.first().map(|f| text(f)).unwrap_or_default()
}

/// OPT-06: `env = "X"` became a variable with an empty name, and the run
/// went ahead without `X`.
#[tokio::test]
async fn env_string_is_a_type_error() {
    let msg =
        fails(r#"target(name = "t", driver = "bash", run = "env > $OUT", out = "o", env = "X")"#)
            .await;
    assert!(
        msg.contains("expected {string: string} (NAME: value), got: \"X\""),
        "{msg}"
    );
}

/// OPT-22: `heph.fs.glob("../q/*.txt")` from `p` answered `q/q.txt`, which
/// every caller read as `p/q/q.txt`, a different and missing file.
#[tokio::test]
async fn glob_outside_package_refused() {
    let msg = fails(
        r#"target(name = "t", driver = "bash", run = "echo %s > $OUT" % " ".join(heph.fs.glob("../q/*.txt")), out = "o")"#,
    )
    .await;
    assert!(
        msg.contains(
            "heph.fs.glob(\"../q/*.txt\"): a glob cannot leave its package `p`; use a target in `//q`"
        ),
        "{msg}"
    );
}

/// A `..` that comes back into the package is still inside it.
#[tokio::test]
async fn glob_back_into_own_package_accepted() {
    let ws = Workspace::new();
    ws.write_build_file(
        "p",
        r#"target(name = "t", driver = "bash", run = "echo %s > $OUT" % " ".join(heph.fs.glob("../p/*.txt")), out = "o")"#,
    );
    ws.write_file("p/a.txt", "a");
    ws.run("//p:t")
        .await
        .expect("a glob that stays in `p` builds");
}

/// OPT-27: `out = "/tmp/o"` was re-rooted to `p/tmp/o` and failed only after
/// the run, on an `lstat`.
#[tokio::test]
async fn absolute_out_refused_at_parse() {
    let msg =
        fails(r#"target(name = "t", driver = "bash", run = "echo hi > /tmp/o", out = "/tmp/o")"#)
            .await;
    assert!(
        msg.contains("\"/tmp/o\" is absolute; outputs are relative to the package"),
        "{msg}"
    );
    assert!(!msg.contains("lstat"), "refused before the run: {msg}");
}
