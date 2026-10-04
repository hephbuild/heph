//! Label selection from listed labels: a provider that reports a target's
//! labels from `list` lets a label selector decide membership without
//! resolving the candidates. The selection trusts the listing; `heph validate`
//! is what holds it to the specs.
#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

mod common;

use futures::TryStreamExt;
use heph::engine::discovery::Stage;
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::fault_provider::{FaultProvider, Faults, GetLog};
use heph::engine::{Discovery, Gaps, OutputMatcher, ResultOptions};
use heph::htaddr::{Addr, parse_addr};
use heph::htmatcher::Matcher;
use heph::pluginexec;
use heph::pluginstatictarget::Target;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn bash(addr: &str, labels: &[&str]) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "bash".to_string(),
        run: Some("echo ok > $OUT".to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        ..Default::default()
    }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|l| (*l).to_string()).collect()
}

fn workspace(
    targets: Vec<Target>,
    faults: Faults,
) -> anyhow::Result<(htestkit::Workspace, Arc<GetLog>)> {
    let provider = FaultProvider::new(targets, faults)?;
    let gets = provider.gets();
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .build()?;
    Ok((ws, gets))
}

fn label(l: &str) -> Matcher {
    Matcher::Label(l.to_string())
}

/// `heph r <matcher>`, returning the batch and every event it emitted.
async fn run(
    ws: &htestkit::Workspace,
    m: &Matcher,
    opts: &ResultOptions,
) -> (anyhow::Result<heph::engine::BatchResult>, Vec<BuildEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = ws.engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(&ws.engine)
        .result(rs.clone(), m, OutputMatcher::All, opts)
        .await;
    drop(rs);
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (res, events)
}

fn matched(events: &[BuildEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::Matched { addrs, .. } => Some(addrs.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// Every target that resolved successfully, from its `ResultEnd`.
fn built(events: &[BuildEvent]) -> Vec<String> {
    let mut addrs: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ResultEnd {
                addr, error: None, ..
            } => Some(addr.clone()),
            _ => None,
        })
        .collect();
    addrs.sort();
    addrs
}

fn got(gets: &GetLog) -> Vec<String> {
    gets.addrs().iter().map(Addr::format).collect()
}

/// The listing decides: `query` resolves no candidate at all, and `result`
/// resolves only the match it is about to build.
#[tokio::test]
async fn listed_labels_decide_without_resolving() -> anyhow::Result<()> {
    let (ws, gets) = workspace(
        vec![
            bash("//a:yes", &["x"]),
            bash("//a:no", &["y"]),
            bash("//b:none", &[]),
            // A near miss: a label that contains the selected one.
            bash("//b:near", &["xx"]),
        ],
        Faults::default(),
    )?;

    let addrs: Vec<Addr> = Arc::clone(&ws.engine)
        .query(ws.engine.new_state(), &label("x"), Discovery::Complete)
        .try_collect()
        .await?;
    assert_eq!(addrs, vec![parse_addr("//a:yes")?]);
    assert!(got(&gets).is_empty(), "query resolved {:?}", got(&gets));

    let (res, events) = run(&ws, &label("x"), &ResultOptions::default()).await;
    res?;
    assert_eq!(built(&events), vec!["//a:yes"]);
    assert!(
        got(&gets).iter().all(|a| a == "//a:yes"),
        "a non-match reached `get`: {:?}",
        got(&gets)
    );
    Ok(())
}

async fn mismatches(ws: &htestkit::Workspace) -> anyhow::Result<Vec<String>> {
    Ok(Arc::clone(&ws.engine)
        .listing_mismatches(
            ws.engine.new_state(),
            &Matcher::PackagePrefix(heph::htpkg::PkgBuf::from("")),
            Discovery::Complete,
        )
        .await?
        .iter()
        .map(ToString::to_string)
        .collect())
}

/// The run trusts a provider's listing, wrong or not; `heph validate`'s check
/// is what reports a listing that differs from its spec, naming the provider,
/// the target and the labels that differ.
#[tokio::test]
async fn label_lie_is_trusted_by_the_run_and_reported_by_validate() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        vec![bash("//a:t", &["y"]), bash("//a:honest", &["x"])],
        Faults {
            name: Some("liar"),
            listed_labels: vec![(parse_addr("//a:t")?, strings(&["x"]))],
            ..Default::default()
        },
    )?;

    let (res, events) = run(&ws, &label("x"), &ResultOptions::default()).await;
    res?;
    assert_eq!(built(&events), vec!["//a:honest", "//a:t"]);

    let found = mismatches(&ws).await?;
    assert_eq!(found.len(), 1, "{found:?}");
    for want in [
        "provider `liar` lists //a:t with labels [x], but its spec has [y]",
        "listed but not in the spec: x",
        "in the spec but not listed: y",
        "Provider `liar` must list exactly the labels its `get` returns",
    ] {
        assert!(found[0].contains(want), "{want} missing from: {}", found[0]);
    }
    Ok(())
}

/// A walk for one driver's targets trusts a listed driver the way a label
/// selector trusts listed labels, so `heph validate` reports a provider whose
/// listed driver differs from its spec's, naming both.
#[tokio::test]
async fn driver_lie_is_reported_by_validate() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        vec![bash("//a:t", &[]), bash("//a:honest", &[])],
        Faults {
            name: Some("liar"),
            listed_drivers: vec![(parse_addr("//a:t")?, "credential".to_string())],
            ..Default::default()
        },
    )?;

    let found = mismatches(&ws).await?;
    assert_eq!(
        found,
        vec![
            "provider `liar` lists //a:t with driver `credential`, but its spec has `bash`. \
             Provider `liar` must list exactly the driver its `get` returns"
                .to_string()
        ]
    );
    Ok(())
}

/// Honest listings, phantoms included, report nothing: a candidate that does
/// not resolve has no labels to get wrong.
#[tokio::test]
async fn honest_listings_report_no_mismatch() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        vec![bash("//a:x", &["x"]), bash("//a:none", &[])],
        Faults {
            vanished: vec![(parse_addr("//a:ghost")?, strings(&["x"]))],
            ..Default::default()
        },
    )?;
    assert_eq!(mismatches(&ws).await?, Vec::<String>::new());
    Ok(())
}

/// A provider may list a target it cannot resolve. `query` trusts the listing
/// and yields it, like any candidate; `result` drops it before announcing it:
/// not built, not an error, and not in the matched denominator.
#[tokio::test]
async fn listed_match_not_found_is_dropped() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        vec![bash("//a:real", &["x"])],
        Faults {
            vanished: vec![(parse_addr("//a:ghost")?, strings(&["x"]))],
            ..Default::default()
        },
    )?;

    let (res, events) = run(&ws, &label("x"), &ResultOptions::default()).await;
    let batch = res?;
    assert_eq!(built(&events), vec!["//a:real"]);
    assert!(batch.errors.is_empty(), "{:?}", batch.errors);
    assert_eq!(matched(&events), vec!["//a:real"]);

    let addrs: Vec<Addr> = Arc::clone(&ws.engine)
        .query(ws.engine.new_state(), &label("x"), Discovery::Complete)
        .try_collect()
        .await?;
    assert_eq!(
        addrs,
        vec![parse_addr("//a:ghost")?, parse_addr("//a:real")?]
    );
    Ok(())
}

/// Only a match the address alone does not decide is dropped when it does not
/// resolve. Asked for by name, the same phantom is an error.
#[tokio::test]
async fn explicit_addr_not_found_still_errors() -> anyhow::Result<()> {
    let ghost = parse_addr("//a:ghost")?;
    let (ws, _) = workspace(
        vec![bash("//a:real", &["x"])],
        Faults {
            vanished: vec![(ghost.clone(), strings(&["x"]))],
            ..Default::default()
        },
    )?;

    let (res, _) = run(
        &ws,
        &Matcher::Addr(ghost.clone()),
        &ResultOptions::default(),
    )
    .await;
    let batch = res?;
    assert!(batch.ok.is_empty());
    assert_eq!(batch.errors.len(), 1, "{:?}", batch.errors);
    assert_eq!(batch.errors[0].0, ghost);
    assert!(
        format!("{:#}", batch.errors[0].1).contains("target not found"),
        "{:#}",
        batch.errors[0].1
    );
    Ok(())
}

/// `result` confirms label matches concurrently before admitting them, not one
/// at a time.
#[tokio::test]
async fn label_query_is_parallel() -> anyhow::Result<()> {
    let targets: Vec<Target> = (0..8).map(|i| bash(&format!("//p{i}:t"), &["x"])).collect();
    let (ws, gets) = workspace(
        targets,
        Faults {
            slow_get: Some(Duration::from_millis(100)),
            ..Default::default()
        },
    )?;

    let (res, _) = run(&ws, &label("x"), &ResultOptions::default()).await;
    assert_eq!(res?.ok.len(), 8);
    assert!(
        gets.max_in_flight() > 1,
        "every `get` ran alone: the listed matches were resolved serially"
    );
    Ok(())
}

fn keep_going(selector: &str) -> (Arc<Gaps>, ResultOptions) {
    let gaps = Gaps::new(selector);
    let opts = ResultOptions {
        discovery: Discovery::KeepGoing(gaps.clone()),
        ..Default::default()
    };
    (gaps, opts)
}

/// A listed match whose `get` fails is confirmed on the parallel path, and
/// that path keeps the walk's rules: a recorded skip under keep-going, with
/// the healthy match still built; a failed run without it. A phantom is never
/// a skip.
#[tokio::test]
async fn listed_match_failures_follow_the_walk_rules() -> anyhow::Result<()> {
    let faults = || -> anyhow::Result<Faults> {
        Ok(Faults {
            fail_get: vec![parse_addr("//a:bad")?],
            vanished: vec![(parse_addr("//a:ghost")?, strings(&["x"]))],
            ..Default::default()
        })
    };
    let targets = || vec![bash("//a:ok", &["x"]), bash("//a:bad", &["x"])];

    let (ws, _) = workspace(targets(), faults()?)?;
    let (gaps, opts) = keep_going("label(x)");
    let (res, events) = run(&ws, &label("x"), &opts).await;
    let batch = res?;
    assert!(batch.errors.is_empty(), "{:?}", batch.errors);
    assert_eq!(built(&events), vec!["//a:ok"]);
    let report = gaps.report();
    assert_eq!(report.skipped, 1, "the ghost is not a skip: {report:?}");
    assert_eq!(report.groups[0].stage, Stage::Spec);
    assert_eq!(report.groups[0].examples[0].scope, "//a:bad");

    let (ws, _) = workspace(targets(), faults()?)?;
    let (res, _) = run(&ws, &label("x"), &ResultOptions::default()).await;
    let err = res
        .err()
        .expect("without keep-going a failing match fails the run");
    assert!(format!("{err:#}").contains("go list"), "{err:#}");
    Ok(())
}

/// `Not(label)` selects on the listing too, so a phantom it matches is
/// dropped and never announced, like one `label` matches.
#[tokio::test]
async fn negated_label_drops_phantoms() -> anyhow::Result<()> {
    let (ws, _) = workspace(
        vec![bash("//a:real", &[]), bash("//a:excluded", &["y"])],
        Faults {
            vanished: vec![(parse_addr("//a:ghost")?, strings(&[]))],
            ..Default::default()
        },
    )?;
    let not_y = Matcher::Not(Box::new(label("y")));
    let (res, events) = run(&ws, &not_y, &ResultOptions::default()).await;
    res?;
    let mut selected = matched(&events);
    selected.retain(|a| a.starts_with("//a:"));
    assert_eq!(selected, vec!["//a:real"]);
    Ok(())
}

/// Two providers list one addr, and only the second resolves it. Their sets
/// differ, so the walk does not trust either and the spec decides: the target
/// is selected by its real labels, and validate has no listing to report.
#[tokio::test]
async fn providers_listing_one_addr_differently_let_the_spec_decide() -> anyhow::Result<()> {
    let x = parse_addr("//p:x")?;
    let first = FaultProvider::new(
        vec![],
        Faults {
            name: Some("first"),
            vanished: vec![(x.clone(), strings(&["a"]))],
            ..Default::default()
        },
    )?;
    let second = FaultProvider::new(
        vec![bash("//p:x", &["b"])],
        Faults {
            name: Some("second"),
            ..Default::default()
        },
    )?;
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(first))
        .with_provider(move |_| Box::new(second))
        .with_managed_driver(Box::new(pluginexec::Driver::new_bash()))
        .build()?;

    let (res, events) = run(&ws, &label("b"), &ResultOptions::default()).await;
    res?;
    assert_eq!(built(&events), vec!["//p:x"]);
    let (res, events) = run(&ws, &label("a"), &ResultOptions::default()).await;
    res?;
    assert!(built(&events).is_empty(), "{events:?}");
    assert_eq!(mismatches(&ws).await?, Vec::<String>::new());
    Ok(())
}
