//! Every failure the run reports at its end is announced as it happens, by
//! a `ResultEnd` the live views (the TUI's rows and Log tab, the CI line's
//! block, a session, a hook) read — and that event says root or collateral
//! by the same rule that decides what the run records as a root.
#![expect(
    clippy::panic,
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::fault_provider::{FaultProvider, Faults};
use heph::engine::request_state::RequestState;
use heph::engine::{OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::pluginexec;
use heph::pluginstatictarget::Target;
use std::collections::HashMap;
use std::sync::Arc;

fn bash(addr: &str, run: &str, deps: &[&str]) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some(run.to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        deps: HashMap::from([(
            String::new(),
            deps.iter().map(|d| (*d).to_string()).collect(),
        )]),
        ..Default::default()
    }
}

fn workspace(targets: Vec<Target>) -> anyhow::Result<htestkit::Workspace> {
    let provider = FaultProvider::new(targets, Faults::default())?;
    htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .build()
}

/// What a request emitted, and the failures it recorded as roots.
struct Seen {
    events: Vec<BuildEvent>,
    recorded: Vec<String>,
}

/// A fresh request with fail-fast off, its events captured.
fn request(
    ws: &htestkit::Workspace,
) -> (Arc<RequestState>, impl FnOnce(Arc<RequestState>) -> Seen) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = ws.engine.new_state_with_events(false, Some(tx));
    let finish = move |rs: Arc<RequestState>| {
        let mut recorded: Vec<String> =
            rs.take_failures().iter().map(|f| f.addr.format()).collect();
        recorded.sort();
        drop(rs);
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        Seen { events, recorded }
    };
    (rs, finish)
}

/// `heph r <matcher>`.
async fn select(ws: &htestkit::Workspace, m: &Matcher) -> Seen {
    let (rs, finish) = request(ws);
    let res = Arc::clone(&ws.engine)
        .result(rs.clone(), m, OutputMatcher::All, &ResultOptions::default())
        .await;
    drop(res);
    finish(rs)
}

/// `heph r <addr>`.
async fn single(ws: &htestkit::Workspace, addr: &str) -> Seen {
    let (rs, finish) = request(ws);
    let res = Arc::clone(&ws.engine)
        .result_addr(
            rs.clone(),
            &parse_addr(addr).expect("addr"),
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    assert!(res.is_err(), "{addr} must fail");
    drop(res);
    finish(rs)
}

/// `addr`'s one failing `ResultEnd`: its error and the root it names.
fn failed_end(events: &[BuildEvent], addr: &str) -> (String, Option<String>) {
    let ends: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr: a,
                error: Some(error),
                upstream_of,
                ..
            } if a == addr => Some((error.clone(), upstream_of.clone())),
            _ => None,
        })
        .collect();
    match <[_; 1]>::try_from(ends) {
        Ok([end]) => end,
        Err(ends) => panic!("one failing ResultEnd for {addr}, got {ends:?}\n{events:#?}"),
    }
}

/// `addr` started before it ended: a view counts only a started target.
fn started_before_end(events: &[BuildEvent], addr: &str) -> bool {
    let at = |want_start: bool| {
        events.iter().position(|e| match &e.kind {
            BuildEventKind::ResultStart { addr: a } => want_start && a == addr,
            BuildEventKind::ResultEnd { addr: a, .. } => !want_start && a == addr,
            _ => false,
        })
    };
    matches!((at(true), at(false)), (Some(s), Some(e)) if s < e)
}

fn owned(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

/// A target whose def does not resolve fails before its result scope opens.
/// In a selection it never even reached `result_addr`: the entrypoint check
/// resolved the def first and handed its error straight to the batch. Both
/// ways, the failure reached only the end-of-run report.
#[tokio::test]
async fn a_def_that_does_not_resolve_is_announced() -> anyhow::Result<()> {
    let targets = || {
        vec![
            bash("//a:broken", "echo ok > $OUT", &["//a:nope"]),
            bash("//a:ok", "echo ok > $OUT", &[]),
        ]
    };

    let ws = workspace(targets())?;
    let seen = select(&ws, &Matcher::Package("a".into())).await;
    assert!(
        started_before_end(&seen.events, "//a:broken"),
        "{:#?}",
        seen.events
    );
    let (error, root) = failed_end(&seen.events, "//a:broken");
    assert!(error.contains("target not found: //a:nope"), "{error}");
    assert_eq!(root, None, "its own failure");
    assert_eq!(seen.recorded, owned(&["//a:broken"]));

    let ws = workspace(targets())?;
    let seen = single(&ws, "//a:broken").await;
    assert!(
        started_before_end(&seen.events, "//a:broken"),
        "{:#?}",
        seen.events
    );
    let (error, root) = failed_end(&seen.events, "//a:broken");
    assert!(error.contains("target not found: //a:nope"), "{error}");
    assert_eq!(root, None);
    assert_eq!(seen.recorded, owned(&["//a:broken"]));
    Ok(())
}

/// A dependent that failed only because its dependency did names that root,
/// though its error reaches the end as `meta: …` around the memoizer's shared
/// copy of the marker. Read off the chain, the end stopped at that copy and
/// every view showed the dependent as a failure of its own — one the
/// end-of-run report, which records only roots, never listed.
#[tokio::test]
async fn a_meta_failure_names_its_root() -> anyhow::Result<()> {
    let ws = workspace(vec![
        bash("//a:root", "exit 1", &[]),
        bash("//a:top", "echo ok > $OUT", &["//a:root"]),
    ])?;
    let seen = single(&ws, "//a:top").await;
    let (error, root) = failed_end(&seen.events, "//a:top");
    assert!(error.starts_with("meta"), "the case this pins: {error}");
    assert_eq!(root.as_deref(), Some("//a:root"));
    let (_, root) = failed_end(&seen.events, "//a:root");
    assert_eq!(root, None);
    assert_eq!(seen.recorded, owned(&["//a:root"]));
    Ok(())
}

/// Two failing dependencies (fail-fast off) reach the dependent as one
/// aggregate the chain never looks inside: it is still collateral, naming
/// one of its roots.
#[tokio::test]
async fn an_aggregate_of_dependency_failures_names_a_root() -> anyhow::Result<()> {
    let ws = workspace(vec![
        bash("//a:r1", "exit 1", &[]),
        bash("//a:r2", "exit 2", &[]),
        bash("//a:top", "echo ok > $OUT", &["//a:r1", "//a:r2"]),
    ])?;
    let seen = single(&ws, "//a:top").await;
    let (_, root) = failed_end(&seen.events, "//a:top");
    let root = root.unwrap_or_else(|| panic!("collateral: {:#?}", seen.events));
    assert!(root == "//a:r1" || root == "//a:r2", "{root}");
    assert_eq!(seen.recorded, owned(&["//a:r1", "//a:r2"]));
    Ok(())
}
