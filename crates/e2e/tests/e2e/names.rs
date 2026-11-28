//! The target name grammar (error-reporting D8, P19): the address parser's
//! (no `@`, `:`, `|` or space) plus no `/`, checked in `target()` and again by
//! the engine on every provider's spec, since a name is a directory of the
//! cache and a cdylib provider never runs `target()`'s check.

use crate::common::Workspace;
use crate::source_location::{fail_in, text};
use heph::pluginstatictarget::Target;
use std::collections::HashMap;

/// The one failure of requesting `//c:ok` when `c/BUILD` declares a target
/// named `name` (a Starlark literal), as text.
async fn declare(name: &str) -> String {
    let ws = Workspace::new();
    ws.write_build_file(
        "c",
        &format!(
            "target(name = {name}, driver = \"bash\", run = \"echo ok > $OUT\", out = \"o.txt\")\n\
             target(name = \"ok\", driver = \"bash\", run = \"echo ok > $OUT\", out = \"o.txt\")\n"
        ),
    );
    let rs = ws.engine.new_state_with_events(false, None);
    only(fail_in(&ws, &rs, &["//c:ok"]).await)
}

const RULE: &str = "a name may not contain `@`, `:`, `|`, `/` or a space";

async fn assert_refused(name: &str, shown: &str, what: &str) {
    let msg = declare(&format!("{name:?}")).await;
    let want = format!("target name `{shown}` contains {what}; {RULE}");
    assert!(msg.contains(&want), "want {want:?} in: {msg}");
}

#[tokio::test]
async fn name_with_space_refused() {
    assert_refused("a b", "a b", "a space").await;
}

#[tokio::test]
async fn name_with_colon_refused() {
    assert_refused("a:b", "a:b", "`:`").await;
}

#[tokio::test]
async fn name_with_slash_refused() {
    assert_refused("a/b", "a/b", "`/`").await;
}

#[tokio::test]
async fn name_with_at_refused() {
    assert_refused("a@b", "a@b", "`@`").await;
}

#[tokio::test]
async fn name_with_pipe_refused() {
    assert_refused("a|b", "a|b", "`|`").await;
}

/// EVAL-23: a non-string `name` was dropped and reported as an empty one.
#[tokio::test]
async fn non_string_name_refused_with_type() {
    let msg = declare("1").await;
    assert!(
        msg.contains("target() `name` must be a string, not int `1`"),
        "{msg}"
    );
}

/// The `out` of a target the engine resolves and builds.
async fn builds(ws: &Workspace, addr: &str) -> String {
    crate::common::run_drain(&ws.engine, addr).await
}

#[tokio::test]
async fn unicode_name_accepted() {
    let ws = Workspace::new();
    ws.write_build_file(
        "c",
        "target(name = \"café-ü\", driver = \"bash\", run = \"echo ok > $OUT\", out = \"o.txt\")\n",
    );
    assert!(!builds(&ws, "//c:café-ü").await.is_empty());
}

/// A `heph.core.matrix` cell's name is the caller's string, and the usual
/// shape (`.`, `=`, `,`, `-`) stays valid.
#[tokio::test]
async fn matrix_cell_name_accepted() {
    let ws = Workspace::new();
    ws.write_build_file(
        "c",
        r#"
def _cell(go, os):
    n = "build.go=" + go + ",os=" + os
    target(name = n, driver = "bash", run = "echo " + go + " > $OUT", out = "o.txt")
    return ":" + n

M = heph.core.matrix(axes = {"go": ["1.23"], "os": ["linux-amd64"]}, cell = _cell)
"#,
    );
    assert!(
        !builds(&ws, "//c:build.go=1.23,os=linux-amd64")
            .await
            .is_empty()
    );
}

/// A static provider's target: it never runs `target()`'s check.
fn provided(addr: &str) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some("echo ok > $OUT".to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        ..Default::default()
    }
}

/// The one failure of requesting `addr` from a provider that serves it.
async fn provider_refuses(addr: &str) -> String {
    let ws = Workspace::with_static(vec![provided(addr)]).expect("workspace");
    let rs = ws.engine.new_state_with_events(false, None);
    only(fail_in(&ws, &rs, &[addr]).await)
}

/// The text of the one failure in `failures`.
fn only(failures: Vec<std::sync::Arc<hplugin::error::TargetFailure>>) -> String {
    assert_eq!(failures.len(), 1, "one failure");
    failures.first().map(|f| text(f)).unwrap_or_default()
}

/// `//p:a/b` would cache inside `//p:a`'s directory.
#[tokio::test]
async fn provider_spec_name_with_slash_refused() {
    let msg = provider_refuses("//p:a/b").await;
    assert!(
        msg.contains(&format!("target name `a/b` contains `/`; {RULE}")),
        "{msg}"
    );
}

/// `x/../../../y` would climb out of `<home>/cache`.
#[tokio::test]
async fn provider_spec_name_dotdot_refused() {
    let msg = provider_refuses("//p:x/../../../y").await;
    assert!(
        msg.contains(&format!("target name `x/../../../y` contains `/`; {RULE}")),
        "{msg}"
    );
}
