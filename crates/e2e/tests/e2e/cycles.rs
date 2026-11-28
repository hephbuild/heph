//! Dependency cycles (error-reporting D8, GRAPH-05/06/07): every entry point
//! that walks the graph detects a cycle instead of waiting on itself, and the
//! error names the whole cycle.
#![expect(
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use hcore::hmemoizer::downcast_chain_ref;
use heph::engine::{OutputMatcher, ResultOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htmatcher::Matcher;
use hplugin::error::{CycleError, TargetFailure};
use std::sync::Arc;
use std::time::Duration;

fn addr(a: &str) -> Addr {
    parse_addr(a).unwrap_or_else(|e| panic!("parse {a}: {e:#}"))
}

/// `cyc/BUILD`: `self` depends on itself, `p ↔ q`, `w → x → y → z → w`.
fn cyc_ws() -> Workspace {
    let ws = Workspace::new();
    let t = |name: &str, dep: &str| {
        format!(
            r#"target(name = "{name}", driver = "bash", run = "echo ok > $OUT", out = "o.txt", deps = [":{dep}"])"#
        )
    };
    let build = [
        t("self", "self"),
        t("p", "q"),
        t("q", "p"),
        t("w", "x"),
        t("x", "y"),
        t("y", "z"),
        t("z", "w"),
    ]
    .join("\n");
    ws.write_build_file("cyc", &build);
    ws
}

/// `heph inspect hashin <a>`: `meta` on a fresh request, which must fail
/// with a [`CycleError`] well within 5 s rather than wait on itself.
async fn hashin_errors_with_cycle(a: &str) -> CycleError {
    let ws = cyc_ws();
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        std::sync::Arc::clone(&ws.engine).meta(ws.engine.new_state(), &addr(a)),
    )
    .await
    .unwrap_or_else(|_| panic!("`inspect hashin {a}` hung on its own cycle"));
    let Err(e) = res else {
        panic!("`inspect hashin {a}` must fail on a cycle");
    };
    downcast_chain_ref::<CycleError>(&e)
        .unwrap_or_else(|| panic!("a CycleError, got: {e:#}"))
        .clone()
}

fn path_of(c: &CycleError) -> Vec<String> {
    c.path.iter().map(Addr::format).collect()
}

/// GRAPH-06, a 1-cycle.
#[tokio::test]
async fn hashin_on_1_cycle_errors() {
    let c = hashin_errors_with_cycle("//cyc:self").await;
    assert_eq!(path_of(&c), ["//cyc:self", "//cyc:self"]);
}

/// GRAPH-06, a 2-cycle.
#[tokio::test]
async fn hashin_on_2_cycle_errors() {
    let c = hashin_errors_with_cycle("//cyc:p").await;
    assert_eq!(path_of(&c), ["//cyc:p", "//cyc:q", "//cyc:p"]);
}

/// GRAPH-06, a 4-cycle.
#[tokio::test]
async fn hashin_on_4_cycle_errors() {
    let c = hashin_errors_with_cycle("//cyc:w").await;
    assert_eq!(
        path_of(&c),
        ["//cyc:w", "//cyc:x", "//cyc:y", "//cyc:z", "//cyc:w"]
    );
}

/// `heph run <a>` with keep-going: its error and every failure it recorded.
async fn run(ws: &Workspace, a: &str) -> (anyhow::Error, Vec<Arc<TargetFailure>>) {
    let rs = ws.engine.new_state_with_fail_fast(false);
    let res = Arc::clone(&ws.engine)
        .result(
            rs.clone(),
            &Matcher::Addr(addr(a)),
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    let err = match res {
        Ok(batch) => batch.errors.into_iter().next().map(|(_, e)| e),
        Err(e) => Some(e),
    }
    .unwrap_or_else(|| panic!("{a} must fail"));
    (err, rs.take_failures())
}

/// GRAPH-05/07: a cycle is one record, against the requested target, whose
/// text names the whole cycle once; a self-cycle is recorded the same way,
/// so it is counted as failed rather than shown as done.
#[tokio::test]
async fn cycle_reported_once_with_whole_path() {
    let ws = cyc_ws();
    for (a, path) in [
        ("//cyc:w", "//cyc:w → //cyc:x → //cyc:y → //cyc:z → //cyc:w"),
        ("//cyc:p", "//cyc:p → //cyc:q → //cyc:p"),
        ("//cyc:self", "//cyc:self → //cyc:self"),
    ] {
        let (err, failures) = run(&ws, a).await;
        let [f] = failures.as_slice() else {
            panic!("{a}: one record, got {failures:#?}");
        };
        assert_eq!(f.culprit().to_string(), a);
        let text = crate::common::failure_text(f);
        assert_eq!(text.matches(path).count(), 1, "{a}: the path once: {text}");
        assert_eq!(text.matches("cyclic").count(), 1, "{a}: one cause: {text}");
        let shown = format!("{err:#}");
        assert_eq!(shown.matches("cyclic").count(), 1, "{a}: {shown}");
    }
}
