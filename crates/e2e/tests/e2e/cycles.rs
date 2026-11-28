//! Dependency cycles (error-reporting D8, GRAPH-05/06/07): every entry point
//! that walks the graph detects a cycle instead of waiting on itself, and the
//! error names the whole cycle.
#![expect(
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::Workspace;
use hcore::hmemoizer::downcast_chain_ref;
use heph::htaddr::{Addr, parse_addr};
use hplugin::error::CycleError;
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

/// GRAPH-06, a 1-cycle.
#[tokio::test]
async fn hashin_on_1_cycle_errors() {
    hashin_errors_with_cycle("//cyc:self").await;
}

/// GRAPH-06, a 2-cycle.
#[tokio::test]
async fn hashin_on_2_cycle_errors() {
    hashin_errors_with_cycle("//cyc:p").await;
}

/// GRAPH-06, a 4-cycle.
#[tokio::test]
async fn hashin_on_4_cycle_errors() {
    hashin_errors_with_cycle("//cyc:w").await;
}
