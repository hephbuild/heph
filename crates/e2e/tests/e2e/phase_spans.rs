#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! Phase spans end to end (session store PR 1): what `ResultEnd.spans` says
//! about a target that ran, hit, failed, timed out or was cancelled, waited
//! on a full pool or a held lock, and what reaches a hook, through the real
//! engine and the real bash and template drivers. The recorder's own
//! rules (ordering, open marks, capacity, a wall-clock step) are unit tested
//! in `engine::phase_spans`.

use crate::common::{build_engine_with_remote, run_drain_events};
use hcore::events::{Phase, PhaseSpan};
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::{Config, Engine, HomeDir, OutputMatcher, ResultOptions};
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

/// What one side of [`spans_reach_no_hash`] built: the hashouts of `dep` and
/// `t`, `t`'s hashin and def hash, and the bytes `//p:env` wrote.
#[derive(Debug, PartialEq, Eq)]
struct Built {
    dep: Vec<String>,
    t: Vec<String>,
    hashin: String,
    def: Vec<u8>,
    env: Vec<u8>,
}

async fn build_side(
    engine: &Arc<Engine>,
    rs: Arc<heph::engine::request_state::RequestState>,
) -> anyhow::Result<Built> {
    // A result holds a read on its entry: take what is needed and drop it.
    let result = |addr: &'static str| {
        let (engine, rs) = (Arc::clone(engine), Arc::clone(&rs));
        async move {
            engine
                .result_addr(
                    rs,
                    &parse_addr(addr)?,
                    OutputMatcher::All,
                    &ResultOptions::default(),
                )
                .await
        }
    };
    let hashouts = |r: &hplugin::eresult::EResult| -> Vec<String> {
        r.artifacts_meta.iter().map(|m| m.hashout.clone()).collect()
    };
    let dep = hashouts(&*result("//p:dep").await?);
    let t = hashouts(&*result("//p:t").await?);
    let env = crate::common::artifact_bytes(&*result("//p:env").await?);
    let addr = parse_addr("//p:t")?;
    let hashin = Arc::clone(engine).meta(rs.clone(), &addr).await?.hashin;
    let def = Arc::clone(engine)
        .get_def(rs.clone(), &addr)
        .await?
        .target_def
        .hash
        .clone();
    crate::common::drain(rs).await;
    Ok(Built {
        dep,
        t,
        hashin,
        def,
        env,
    })
}

/// I1: the spans reach no hash, and nothing about them reaches a run. The
/// same targets built by a request that measures spans (something listens)
/// and, from an empty cache, by one that measures none — so both sides
/// really run, one with the run clock armed and one without — give the same
/// hashouts, hashin and def hash, and a target that writes its environment
/// and argv writes the same bytes.
#[tokio::test]
async fn spans_reach_no_hash() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"
target(name = "dep", driver = "bash", run = "sleep 0.1; echo dep > $OUT", out = "dep.txt")
target(name = "t", driver = "bash", deps = [":dep"], run = "cat $SRC > $OUT", out = "t.txt")
target(name = "env", driver = "bash", run = 'env | sort > $OUT; echo "$0" "$@" >> $OUT', out = "env.txt")
"#,
    );

    // Measured: an event-collecting request builds it, and spans arrive.
    let listened = build_engine_with_remote(ws.path(), None);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = listened.new_state_with_events(false, Some(tx));
    assert!(rs.has_listeners());
    let measured = build_side(&listened, rs).await?;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    for addr in ["//p:dep", "//p:t", "//p:env"] {
        let ran = result_ends(&events, addr)
            .into_iter()
            .any(|(spans, _, _)| find(&spans, Phase::Process).is_some());
        assert!(ran, "{addr} ran with its clock armed");
    }
    drop(listened);

    // Unmeasured: the cache emptied, a fresh engine, nothing listening, so
    // every target runs again with no recorder and an inert clock.
    std::fs::remove_dir_all(HomeDir::for_tests(ws.path()).join("cache"))?;
    let quiet = build_engine_with_remote(ws.path(), None);
    let rs = quiet.new_state();
    assert!(!rs.has_listeners());
    let unmeasured = build_side(&quiet, rs).await?;

    assert!(!measured.env.is_empty());
    assert_eq!(
        measured,
        unmeasured,
        "env listened:\n{}\nenv quiet:\n{}",
        String::from_utf8_lossy(&measured.env),
        String::from_utf8_lossy(&unmeasured.env)
    );
    Ok(())
}

/// A target cancelled mid-process, with no timeout: the run clock is armed
/// by the listener alone (the spec's `run_clock_armed_without_timeout`), so
/// the process has its span, closed at the stop. The result reads as
/// cancelled, not timed out, and the stop takes nothing like the 10s grace.
#[tokio::test]
async fn spans_on_cancel() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let marker = ws.path().join("started");
    write_build(
        ws.path(),
        &format!(
            r#"target(name = "parked", driver = "bash", run = "echo up > {}; sleep 60; echo > $OUT", out = "out.txt")"#,
            marker.display()
        ),
    );
    let engine = build_engine_with_remote(ws.path(), None);
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
    // The process itself says it is running.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped_at = std::time::Instant::now();
    rs.ctoken().cancel();
    let _cancelled = tokio::time::timeout(Duration::from_secs(30), run).await??;
    let stop_took = stopped_at.elapsed();
    drop(rs);
    let mut events = Vec::new();
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        events.push(ev);
    }
    assert!(
        stop_took < Duration::from_secs(5),
        "a cancel does not wait out the grace: {stop_took:?}"
    );
    let (cancelled, timed_out) = events
        .iter()
        .find_map(|ev| match &ev.kind {
            BuildEventKind::ResultEnd {
                addr,
                cancelled,
                timed_out,
                ..
            } if addr == "//p:parked" => Some((*cancelled, timed_out.clone())),
            _ => None,
        })
        .expect("a ResultEnd for //p:parked");
    assert!(cancelled, "a cancel reads as cancelled");
    assert!(timed_out.is_none(), "not a timeout: {timed_out:?}");
    let (spans, _, _) = one_result(&events, "//p:parked");
    assert_well_formed(&spans);
    let process = find(&spans, Phase::Process).expect("process span");
    assert!(process.duration_us() >= 100_000, "{process:?}");
    assert!(process.duration_us() < 30_000_000, "{process:?}");
    Ok(())
}

/// An engine over `root` with the bash driver, `config` applied on top of
/// the test config, and `hook` registered if given.
fn bash_engine(
    root: &Path,
    config: impl FnOnce(&mut Config),
    hook: Option<Arc<dyn hplugin::hook::Hook>>,
) -> anyhow::Result<Arc<Engine>> {
    let mut cfg = Config::for_tests(root);
    config(&mut cfg);
    let mut e = Engine::new(cfg)?;
    e.register_provider(|init| {
        Box::new(heph::pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(heph::pluginexec::Driver::new_bash().with_host_path()))?;
    if let Some(hook) = hook {
        e.register_hook(hook)?;
    }
    Ok(Arc::new(e))
}

/// A full pool: two targets that each need the one cpu there is. The one
/// admitted second waits in `heph.admission` until the first has run, so its
/// admission ends no earlier than the first's process, and lasts about as
/// long.
#[tokio::test]
async fn admission_waits_out_a_full_pool() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"
target(name = "x", driver = "bash", run = "sleep 0.4; echo x > $OUT", out = "x.txt")
target(name = "y", driver = "bash", run = "sleep 0.4; echo y > $OUT", out = "y.txt")
target(name = "top", driver = "bash", deps = [":x", ":y"], run = "cat $SRC > $OUT", out = "top.txt")
"#,
    );
    let engine = bash_engine(ws.path(), |c| c.parallelism = Some(1), None)?;
    let (res, events) = run_drain_events(&engine, &target("//p:top")).await;
    res?;

    // Each target's run-carrying scope; both in one request, on one clock.
    let ran = |addr: &str| {
        result_ends(&events, addr)
            .into_iter()
            .map(|(spans, _, _)| spans)
            .find(|spans| find(spans, Phase::Process).is_some())
            .unwrap_or_else(|| panic!("{addr} ran"))
    };
    let (x, y) = (ran("//p:x"), ran("//p:y"));
    let process_start = |s: &[PhaseSpan]| find(s, Phase::Process).map(|p| p.start_unix_us);
    let (first, second) = if process_start(&x) <= process_start(&y) {
        (x, y)
    } else {
        (y, x)
    };
    assert_well_formed(&second);
    let first_process = find(&first, Phase::Process).expect("process");
    let waited = second
        .iter()
        .filter(|s| s.phase() == Some(Phase::Admission))
        .max_by_key(|s| s.duration_us())
        .expect("an admission span");
    assert!(
        waited.end_unix_us >= first_process.end_unix_us,
        "admitted only once the first had run: {waited:?} vs {first_process:?}"
    );
    assert!(
        waited.duration_us() >= first_process.duration_us() / 2,
        "waited about as long as the first ran: {waited:?} vs {first_process:?}"
    );
    Ok(())
}

/// A held result lock: another holder has the target's revision, so the
/// result waits in `heph.lock_wait` for as long as it is held.
#[tokio::test]
async fn lock_wait_lasts_while_the_lock_is_held() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt")"#,
    );
    let addr = parse_addr("//p:t")?;
    let holder = build_engine_with_remote(ws.path(), None);
    let hrs = holder.new_state();
    let hashin = Arc::clone(&holder).meta(hrs.clone(), &addr).await?.hashin;
    let write = holder
        .result_lock()
        .write(&addr, &hashin, hrs.ctoken())
        .await?;

    let engine = build_engine_with_remote(ws.path(), None);
    let run = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { run_drain_events(&engine, &target("//p:t")).await }
    });
    let held = Duration::from_millis(600);
    tokio::time::sleep(held).await;
    drop(write);
    let (res, events) = tokio::time::timeout(Duration::from_secs(30), run).await??;
    res?;

    let (spans, _, _) = one_result(&events, "//p:t");
    assert_well_formed(&spans);
    let waited = spans
        .iter()
        .filter(|s| s.phase() == Some(Phase::LockWait))
        .map(PhaseSpan::duration_us)
        .max()
        .expect("a lock_wait span");
    assert!(
        waited >= 400_000,
        "waited most of the {held:?} the lock was held: {waited}us in {spans:?}"
    );
    Ok(())
}

/// Spans reach a hook, not only the event channel: a request whose only
/// listener is a registered hook measures them, and the hook's `ResultEnd`
/// carries them.
#[tokio::test]
async fn spans_reach_a_hook() -> anyhow::Result<()> {
    #[derive(Default)]
    struct Ends(std::sync::Mutex<Vec<BuildEvent>>);
    impl hplugin::hook::Hook for Ends {
        fn name(&self) -> String {
            "ends".to_string()
        }
        fn on_event(&self, ev: &BuildEvent) {
            if matches!(ev.kind, BuildEventKind::ResultEnd { .. }) {
                self.0.lock().expect("ends").push(ev.clone());
            }
        }
        fn on_close(&self) {}
    }

    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt")"#,
    );
    let ends = Arc::new(Ends::default());
    let engine = bash_engine(
        ws.path(),
        |_| {},
        Some(Arc::clone(&ends) as Arc<dyn hplugin::hook::Hook>),
    )?;
    let rs = engine.new_state();
    assert!(rs.has_listeners(), "a hook is a listener");
    let built = Arc::clone(&engine)
        .result_addr(
            rs.clone(),
            &parse_addr("//p:t")?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    drop(built);
    crate::common::drain(rs).await;

    let events = ends.0.lock().expect("ends").clone();
    let carried: Vec<Vec<PhaseSpan>> = events
        .iter()
        .filter_map(|ev| match &ev.kind {
            BuildEventKind::ResultEnd { addr, spans, .. } if addr == "//p:t" => Some(spans.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(carried.len(), 1, "{events:?}");
    let spans = &carried[0];
    assert_well_formed(spans);
    assert!(find(spans, Phase::Process).is_some(), "{spans:?}");
    Ok(())
}

/// A result that ends badly still carries its spans, each closed: a failing
/// command, one timed out. And a target two dependents share runs once, so
/// one of its scopes carries the run.
#[tokio::test]
async fn spans_on_fail_and_timeout() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        r#"
target(name = "boom", driver = "bash", run = "exit 3", out = "out.txt")
target(name = "slow", driver = "bash", run = "sleep 60; echo > $OUT", out = "out.txt", timeout = 1)
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
