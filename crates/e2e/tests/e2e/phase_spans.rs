#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! Phase spans end to end (session store PR 1): what `ResultEnd.spans` says
//! about a target that ran, hit, failed, timed out or was cancelled, through
//! the real engine and the real bash and template drivers. The recorder's own
//! rules (ordering, open marks, capacity, a wall-clock step) are unit tested
//! in `engine::phase_spans`.

use crate::common::{build_engine_with_remote, run_drain_events};
use hcore::events::{Phase, PhaseSpan};
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn write_build(root: &Path, content: &str) {
    let dir = root.join("p");
    std::fs::create_dir_all(&dir).expect("create pkg dir");
    std::fs::write(dir.join("BUILD"), content).expect("write BUILD");
}

fn target(addr: &str) -> Matcher {
    Matcher::Addr(parse_addr(addr).expect("parse addr"))
}

/// `addr`'s `ResultEnd` events: its spans, and the result's own window
/// (`ResultStart`..`ResultEnd`, in microseconds) for each.
fn result_ends(events: &[BuildEvent], addr: &str) -> Vec<(Vec<PhaseSpan>, u64, u64)> {
    let mut started = None;
    let mut out = Vec::new();
    for ev in events {
        match &ev.kind {
            BuildEventKind::ResultStart { addr: a } if a == addr => {
                started = Some(ev.at_unix_ms * 1_000);
            }
            BuildEventKind::ResultEnd { addr: a, spans, .. } if a == addr => {
                out.push((
                    spans.clone(),
                    started.take().expect("ResultStart before ResultEnd"),
                    ev.at_unix_ms * 1_000,
                ));
            }
            _ => {}
        }
    }
    out
}

/// The one `ResultEnd` of `addr`, and its window.
fn one_result(events: &[BuildEvent], addr: &str) -> (Vec<PhaseSpan>, u64, u64) {
    let mut ends = result_ends(events, addr);
    assert_eq!(ends.len(), 1, "one ResultEnd for {addr}: {ends:?}");
    ends.pop().expect("one")
}

fn names(spans: &[PhaseSpan]) -> Vec<&str> {
    spans.iter().map(|s| s.name.as_ref()).collect()
}

fn find(spans: &[PhaseSpan], phase: Phase) -> Option<&PhaseSpan> {
    spans.iter().find(|s| s.phase() == Some(phase))
}

/// Every span is closed and well formed: ordered by start, each ending at or
/// after it starts, and none overlapping the next. The phases of one result
/// are sequential.
fn assert_well_formed(spans: &[PhaseSpan]) {
    assert!(!spans.is_empty(), "no spans");
    for s in spans {
        assert!(s.end_unix_us >= s.start_unix_us, "{s:?}");
        assert!(s.phase().is_some(), "a name this build knows: {s:?}");
    }
    for (a, b) in spans.iter().zip(spans.iter().skip(1)) {
        assert!(
            b.start_unix_us >= a.end_unix_us,
            "{} overlaps {}: {spans:?}",
            a.name,
            b.name
        );
    }
}

/// Wall-clock slack between the event stamps (milliseconds, read off the
/// wall clock) and the spans (the request's anchor plus monotonic offsets).
const SLACK_US: u64 = 5_000;

/// An `exec` target that runs: its phases lie inside its result's window, do
/// not overlap, cover resolution, both cache sides, staging, the process and
/// collection — and `heph.process` brackets the command's known `sleep`.
#[tokio::test]
async fn result_spans_cover_the_result() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"
target(name = "dep", driver = "bash", run = "echo dep > $OUT", out = "dep.txt")
target(name = "t", driver = "bash", deps = [":dep"], run = "sleep 0.3; cat $SRC > $OUT", out = "t.txt")
"#,
    );
    let engine = build_engine_with_remote(ws.path(), None);
    let (res, events) = run_drain_events(&engine, &target("//p:t")).await;
    res?;

    let (spans, start_us, end_us) = one_result(&events, "//p:t");
    assert_well_formed(&spans);
    let first = spans.first().expect("spans");
    let last = spans.last().expect("spans");
    assert!(
        first.start_unix_us + SLACK_US >= start_us,
        "{spans:?} from {start_us}"
    );
    assert!(
        last.end_unix_us <= end_us + 1_000 + SLACK_US,
        "{spans:?} to {end_us}"
    );

    let got = names(&spans);
    for want in [
        Phase::Resolve,
        Phase::Deps,
        Phase::LockWait,
        Phase::CacheRead,
        Phase::Admission,
        Phase::Stage,
        Phase::Process,
        Phase::Collect,
        Phase::CacheWrite,
    ] {
        assert!(
            got.contains(&want.name()),
            "{} missing from {got:?}",
            want.name()
        );
    }
    assert!(!got.contains(&Phase::DriverRun.name()), "{got:?}");

    let process = find(&spans, Phase::Process).expect("process span");
    assert!(
        process.duration_us() >= 300_000,
        "heph.process covers the 0.3s sleep: {process:?}"
    );
    // Staging, collection and the cache write are heph's, not the sleep's.
    let stage = find(&spans, Phase::Stage).expect("stage span");
    let collect = find(&spans, Phase::Collect).expect("collect span");
    assert!(stage.end_unix_us <= process.start_unix_us);
    assert!(collect.start_unix_us >= process.end_unix_us);
    Ok(())
}

/// A cache hit reads the cache and runs nothing: `heph.cache.read`, and no
/// staging, process or cache write.
#[tokio::test]
async fn spans_on_a_hit() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt")"#,
    );
    let engine = build_engine_with_remote(ws.path(), None);
    let (res, _) = run_drain_events(&engine, &target("//p:t")).await;
    res?;

    let (res, events) = run_drain_events(&engine, &target("//p:t")).await;
    res?;
    let (spans, _, _) = one_result(&events, "//p:t");
    assert_well_formed(&spans);
    let got = names(&spans);
    assert!(got.contains(&Phase::CacheRead.name()), "{got:?}");
    assert!(got.contains(&Phase::Resolve.name()), "{got:?}");
    for absent in [
        Phase::Stage,
        Phase::Process,
        Phase::DriverRun,
        Phase::Collect,
        Phase::CacheWrite,
        Phase::Admission,
    ] {
        assert!(
            !got.contains(&absent.name()),
            "{} on a hit: {got:?}",
            absent.name()
        );
    }
    Ok(())
}

/// A driver that does not arm the run clock (`template`, in-process, no
/// process) is timed whole: `heph.driver.run`, never `heph.process`.
#[tokio::test]
async fn unarmed_driver_gets_driver_run() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"
target(name = "conf", driver = "template", src = file("conf.j2"), out = "conf",
       vars = {"name": "x"})
"#,
    );
    std::fs::write(ws.path().join("p/conf.j2"), "name={{ name }}\n")?;
    let mut e = Engine::new(Config::for_tests(ws.path()))?;
    e.register_provider(|init| {
        Box::new(heph::pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_driver(|_| Box::new(heph::plugintemplate::Driver))?;
    let engine = Arc::new(e);

    let (res, events) = run_drain_events(&engine, &target("//p:conf")).await;
    res?;
    let (spans, _, _) = one_result(&events, "//p:conf");
    assert_well_formed(&spans);
    let got = names(&spans);
    assert!(got.contains(&Phase::DriverRun.name()), "{got:?}");
    assert!(got.contains(&Phase::Stage.name()), "{got:?}");
    assert!(!got.contains(&Phase::Process.name()), "{got:?}");
    assert!(!got.contains(&Phase::Collect.name()), "{got:?}");
    Ok(())
}

/// I1: the spans reach no hash. A target's def hash and hashin are the same
/// from a request that measures spans (something listens) and from one that
/// measures none, and from a run whose spans differ from another's.
#[tokio::test]
async fn spans_reach_no_hash() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"
target(name = "dep", driver = "bash", run = "sleep 0.1; echo dep > $OUT", out = "dep.txt")
target(name = "t", driver = "bash", deps = [":dep"], run = "cat $SRC > $OUT", out = "t.txt")
"#,
    );
    let addr = parse_addr("//p:t")?;

    // Measured: an event-collecting request builds it, and spans arrive.
    let listened = build_engine_with_remote(ws.path(), None);
    let (res, events) = run_drain_events(&listened, &target("//p:t")).await;
    res?;
    let (spans, _, _) = one_result(&events, "//p:t");
    assert!(!spans.is_empty());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = listened.new_state_with_events(false, Some(tx));
    assert!(rs.has_listeners());
    let hashin_listened = Arc::clone(&listened)
        .meta(rs.clone(), &addr)
        .await?
        .hashin
        .clone();
    let def_listened = Arc::clone(&listened)
        .get_def(rs, &addr)
        .await?
        .target_def
        .hash
        .clone();

    // Unmeasured: a fresh engine over the same tree, nothing listening.
    let quiet = build_engine_with_remote(ws.path(), None);
    let rs = quiet.new_state();
    assert!(!rs.has_listeners());
    let built = Arc::clone(&quiet)
        .result_addr(
            rs.clone(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    drop(built);
    let hashin_quiet = Arc::clone(&quiet)
        .meta(rs.clone(), &addr)
        .await?
        .hashin
        .clone();
    let def_quiet = Arc::clone(&quiet)
        .get_def(rs, &addr)
        .await?
        .target_def
        .hash
        .clone();

    assert_eq!(hashin_listened, hashin_quiet);
    assert_eq!(def_listened, def_quiet);
    Ok(())
}

/// A result that ends badly still carries its spans, each closed: a failing
/// command, one timed out, one cancelled mid-process. And a target two
/// dependents share runs once, so one of its scopes carries the run.
#[tokio::test]
async fn spans_on_cancel_fail_timeout() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"
target(name = "boom", driver = "bash", run = "exit 3", out = "out.txt")
target(name = "slow", driver = "bash", run = "sleep 60; echo > $OUT", out = "out.txt", timeout = 1)
target(name = "parked", driver = "bash", run = "sleep 60; echo > $OUT", out = "out.txt")
target(name = "shared", driver = "bash", run = "echo s > $OUT", out = "s.txt")
target(name = "a", driver = "bash", deps = [":shared"], run = "cat $SRC > $OUT", out = "a.txt")
target(name = "b", driver = "bash", deps = [":shared"], run = "cat $SRC > $OUT", out = "b.txt")
target(name = "top", driver = "bash", deps = [":a", ":b"], run = "cat $SRC > $OUT", out = "top.txt")
"#,
    );
    let engine = build_engine_with_remote(ws.path(), None);

    // Failed: the process ran and exited non-zero.
    let (res, events) = run_drain_events(&engine, &target("//p:boom")).await;
    assert!(res.is_err() || res.is_ok_and(|b| !b.errors.is_empty()));
    let (spans, _, _) = one_result(&events, "//p:boom");
    assert_well_formed(&spans);
    assert!(find(&spans, Phase::Process).is_some(), "{spans:?}");
    assert!(
        find(&spans, Phase::CacheWrite).is_none(),
        "a failure caches nothing: {spans:?}"
    );

    // Timed out: the process span runs to the deadline.
    let (_res, events) = run_drain_events(&engine, &target("//p:slow")).await;
    let timed_out = events.iter().any(|ev| {
        matches!(&ev.kind, BuildEventKind::ResultEnd { addr, timed_out: Some(_), .. } if addr == "//p:slow")
    });
    assert!(timed_out, "//p:slow timed out");
    let (spans, _, _) = one_result(&events, "//p:slow");
    assert_well_formed(&spans);
    let process = find(&spans, Phase::Process).expect("process span");
    assert!(process.duration_us() >= 900_000, "{process:?}");
    assert!(process.duration_us() < 30_000_000, "{process:?}");

    // Cancelled mid-process: the open phase is closed at the stop.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let run = tokio::spawn({
        let (engine, rs) = (Arc::clone(&engine), Arc::clone(&rs));
        async move {
            engine
                .result(
                    rs,
                    &target("//p:parked"),
                    OutputMatcher::All,
                    &ResultOptions::default(),
                )
                .await
        }
    });
    let mut events = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await?
            .expect("events until the run started");
        let started =
            matches!(&ev.kind, BuildEventKind::ExecuteStart { addr, .. } if addr == "//p:parked");
        events.push(ev);
        if started {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    rs.ctoken().cancel();
    let _cancelled = tokio::time::timeout(Duration::from_secs(30), run).await??;
    drop(rs);
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        events.push(ev);
    }
    let (spans, _, _) = one_result(&events, "//p:parked");
    assert_well_formed(&spans);
    let process = find(&spans, Phase::Process).expect("process span");
    assert!(process.duration_us() >= 200_000, "{process:?}");
    assert!(process.duration_us() < 30_000_000, "{process:?}");

    // Two dependents, one shared target: it runs once. A dependent asks for
    // it twice, with different output matchers (`None` for its hashin, the
    // named outputs for its run), and each is its own result scope with its
    // own `ResultEnd` — that predates spans. Every scope's spans are well
    // formed, and exactly one of them carries the run.
    let (res, events) = run_drain_events(&engine, &target("//p:top")).await;
    res?;
    let ends = result_ends(&events, "//p:shared");
    assert!(!ends.is_empty(), "a ResultEnd for //p:shared");
    for (spans, _, _) in &ends {
        assert_well_formed(spans);
    }
    let ran = ends
        .iter()
        .filter(|(spans, _, _)| find(spans, Phase::Process).is_some())
        .count();
    assert_eq!(ran, 1, "one scope carries the run: {ends:?}");
    Ok(())
}
