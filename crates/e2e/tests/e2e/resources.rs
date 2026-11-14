#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! Target resources end to end: a BUILD file declares `resources`, the driver
//! puts them on the def, the engine refuses an unknown pool at link time and
//! admits each execute through the resource pool, whose capacities come from
//! the engine's `Config` (what `.hephconfig`'s `resources:` and the per-run
//! overrides resolve to; the CLI spellings are tested in `global.rs`).
//!
//! The overlap probe: two targets each block on a gate file in their own
//! `TempDir`. The test waits — with a deadline — until the pool snapshot shows
//! one held and the other queued, checks the queued one has no `ExecuteStart`,
//! opens the gate and asserts the two `ExecuteStart`/`ExecuteEnd` pairs never
//! interleave in stream order. `two_small_targets_do_overlap` is the positive
//! control: the same probe sees the overlap when the pool has room for both.

use hcore::capacity::{Capacity, PoolSpec, Unit};
use heph::engine::event::{BuildEvent, BuildEventKind, EventReceiver};
use heph::engine::resource_pool::ResourceSnapshot;
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::{pluginbuildfile, pluginexec};
use hplugin::provider::PoolName;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MICROS: u64 = 1_000_000;
const DEADLINE: Duration = Duration::from_secs(60);

/// `(pool, capacity)` as `Config.resources` holds it, each pool in its
/// default unit: micro-CPUs for `cpu`, bytes for `memory`, a count for a
/// custom pool.
fn caps(caps: &[(&str, u64)]) -> BTreeMap<PoolName, PoolSpec> {
    caps.iter()
        .map(|&(p, c)| {
            (
                PoolName::try_from(p.to_string()).expect("pool name"),
                PoolSpec {
                    unit: Unit::default_for(p),
                    capacity: Some(Capacity::Amount(c)),
                },
            )
        })
        .collect()
}

/// An engine over `root` with the buildfile provider and the bash driver, and
/// the given pools. Called again over the same root, it is the next `heph`
/// invocation: same on-disk cache, fresh in-memory state.
fn engine(root: &Path, resources: BTreeMap<PoolName, PoolSpec>) -> anyhow::Result<Arc<Engine>> {
    engine_with_notice(root, resources, None)
}

/// [`engine`] with the admission-wait notice threshold set to `notice`
/// (`None` keeps the production one). Set before the engine is shared.
fn engine_with_notice(
    root: &Path,
    resources: BTreeMap<PoolName, PoolSpec>,
    notice: Option<Duration>,
) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config {
        resources,
        ..Config::for_tests(root.to_path_buf())
    })?;
    if let Some(notice) = notice {
        e.set_admission_notice(notice);
    }
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    let e = Arc::new(e);
    e.install_exec_runner_host();
    Ok(e)
}

fn write_build(root: &Path, pkg: &str, content: &str) {
    let dir = root.join(pkg);
    std::fs::create_dir_all(&dir).expect("create pkg dir");
    std::fs::write(dir.join("BUILD"), content).expect("write BUILD");
}

/// A bash target that waits for `gate` to exist, then writes its name and the
/// `${resource:cpu}` it was told.
fn gated(name: &str, gate: &Path, resources: &str) -> String {
    format!(
        r#"
target(
    name = "{name}",
    driver = "bash",
    run = "while [ ! -e '{gate}' ]; do sleep 0.02; done; echo {name} $N > $OUT",
    out = "{name}.txt",
    env = {{"N": "${{resource:cpu}}"}},
    resources = {resources},
)
"#,
        gate = gate.display(),
    )
}

/// Build `addrs` as one batch; the failure registry's root causes are folded
/// into the error, as the CLI renders them.
async fn build(
    engine: Arc<Engine>,
    addrs: Vec<String>,
    tx: Option<heph::engine::event::EventSender>,
) -> anyhow::Result<()> {
    let rs = engine.new_state_with_events(false, tx);
    let m = Matcher::Or(
        addrs
            .iter()
            .map(|a| parse_addr(a).map(Matcher::Addr))
            .collect::<Result<_, _>>()?,
    );
    let batch = Arc::clone(&engine)
        .result(
            rs.clone(),
            &m,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await;
    let mut msg = String::new();
    for f in rs.take_failures() {
        msg.push_str(&format!("{}: {:#}\n", f.addr.format(), f.source));
    }
    match batch {
        Err(e) => msg.push_str(&format!("{e:#}\n")),
        Ok(b) => {
            for (a, e) in &b.errors {
                msg.push_str(&format!("{}: {e:#}\n", a.format()));
            }
        }
    }
    if msg.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{}", msg.trim_end()))
    }
}

/// [`build`] collecting every event the request emitted.
async fn build_events(
    engine: &Arc<Engine>,
    addrs: &[&str],
) -> (anyhow::Result<()>, Vec<BuildEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let res = build(
        Arc::clone(engine),
        addrs.iter().map(|s| s.to_string()).collect(),
        Some(tx),
    )
    .await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    (res, events)
}

fn starts(events: &[BuildEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ExecuteStart { addr, .. } => Some(addr.as_str()),
            _ => None,
        })
        .collect()
}

/// `("start"|"end", addr)` for every execute event, in stream order.
fn execute_trace(events: &[BuildEvent]) -> Vec<(&'static str, String)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::ExecuteStart { addr, .. } => Some(("start", addr.clone())),
            BuildEventKind::ExecuteEnd { addr, .. } => Some(("end", addr.clone())),
            _ => None,
        })
        .collect()
}

fn held(snap: &ResourceSnapshot, pool: &str) -> u64 {
    snap.main
        .pools
        .iter()
        .find(|p| p.pool.as_str() == pool)
        .map_or(0, |p| p.held)
}

/// What the probe saw while the gate was closed, and the whole stream after.
struct Probe {
    /// Snapshot at the moment the probe's condition held.
    snapshot: ResourceSnapshot,
    /// `ExecuteStart`s seen while the gate was closed.
    started_while_gated: Vec<String>,
    events: Vec<BuildEvent>,
}

/// Run `addrs` (all gated on `gate`) as one batch. Wait until `ready` holds
/// over the pool snapshot and the number of `ExecuteStart`s seen so far, then
/// open the gate and let the batch finish.
async fn probe(
    engine: &Arc<Engine>,
    addrs: &[&str],
    gate: &Path,
    ready: impl Fn(&ResourceSnapshot, usize) -> bool,
) -> anyhow::Result<Probe> {
    let (tx, mut rx): (_, EventReceiver) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(build(
        Arc::clone(engine),
        addrs.iter().map(|s| s.to_string()).collect(),
        Some(tx),
    ));
    let mut events = Vec::new();
    let start = Instant::now();
    let snapshot = loop {
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        if let Some(snap) = engine.resource_snapshot()
            && ready(&snap, starts(&events).len())
        {
            break snap;
        }
        if task.is_finished() || start.elapsed() > DEADLINE {
            std::fs::write(gate, "")?;
            let res = task.await?;
            anyhow::bail!(
                "the probe's condition never held (snapshot {:?}, starts {:?}, run {res:?})",
                engine.resource_snapshot(),
                starts(&events),
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let started_while_gated = starts(&events).into_iter().map(str::to_string).collect();
    std::fs::write(gate, "")?;
    tokio::time::timeout(DEADLINE, task).await???;
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    Ok(Probe {
        snapshot,
        started_while_gated,
        events,
    })
}

/// The serialising probe over `//p:a` and `//p:b`, both declaring
/// `resources`: one runs, the other waits in the main lane with no
/// `ExecuteStart`, and the two never interleave.
async fn assert_serialised(
    capacities: &[(&str, u64)],
    resources: &str,
    pool: &str,
    held_each: u64,
) -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let gates = tempfile::tempdir()?;
    let gate = gates.path().join("open");
    write_build(
        ws.path(),
        "p",
        &format!(
            "{}{}",
            gated("a", &gate, resources),
            gated("b", &gate, resources)
        ),
    );
    let engine = engine(ws.path(), caps(capacities))?;
    let p = probe(&engine, &["//p:a", "//p:b"], &gate, |snap, started| {
        started == 1 && snap.main.queued == 1
    })
    .await?;

    assert_eq!(held(&p.snapshot, pool), held_each, "{:?}", p.snapshot);
    assert_eq!(
        p.started_while_gated.len(),
        1,
        "the queued target must not have started: {:?}",
        p.started_while_gated
    );
    let trace = execute_trace(&p.events);
    let first = p.started_while_gated.first().cloned().unwrap_or_default();
    let second = if first == "//p:a" { "//p:b" } else { "//p:a" };
    assert_eq!(
        trace,
        vec![
            ("start", first.clone()),
            ("end", first),
            ("start", second.to_string()),
            ("end", second.to_string()),
        ],
        "the two executes must never interleave"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_cpu_heavy_targets_never_overlap() -> anyhow::Result<()> {
    assert_serialised(&[("cpu", 4 * MICROS)], r#"{"cpu": 3}"#, "cpu", 3 * MICROS).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_memory_heavy_targets_never_overlap() -> anyhow::Result<()> {
    assert_serialised(
        &[("cpu", 8 * MICROS), ("memory", 1 << 30)],
        r#"{"memory": "768M"}"#,
        "memory",
        768 << 20,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_pool_of_one_serializes() -> anyhow::Result<()> {
    assert_serialised(
        &[("cpu", 8 * MICROS), ("docker", 1)],
        r#"{"docker": 1}"#,
        "docker",
        1,
    )
    .await
}

/// An old plugin's def carries no `resources`: it reserves the default 1 cpu,
/// so two of them on a 1-cpu pool serialise exactly like declared ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_resources_count_as_cpu_one() -> anyhow::Result<()> {
    assert_serialised(&[("cpu", MICROS)], "{}", "cpu", MICROS).await
}

/// The positive control: the same probe at `cpu: 1` on a 4-cpu pool sees both
/// running at once, so the probe can see an overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_small_targets_do_overlap() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let gates = tempfile::tempdir()?;
    let gate = gates.path().join("open");
    let res = r#"{"cpu": 1}"#;
    write_build(
        ws.path(),
        "p",
        &format!("{}{}", gated("a", &gate, res), gated("b", &gate, res)),
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    let p = probe(&engine, &["//p:a", "//p:b"], &gate, |snap, started| {
        started == 2 && snap.main.queued == 0
    })
    .await?;

    assert_eq!(held(&p.snapshot, "cpu"), 2 * MICROS, "{:?}", p.snapshot);
    let trace = execute_trace(&p.events);
    assert_eq!(trace.len(), 4, "{trace:?}");
    assert!(
        trace
            .get(..2)
            .is_some_and(|t| t.iter().all(|(k, _)| *k == "start")),
        "both must start before either ends: {trace:?}"
    );
    Ok(())
}

/// A custom pool declared with no value is unbounded: two targets each
/// reserving far more of it than any bounded pool would hold run at once (the
/// probe and its positive control above), each holding its whole, unclamped
/// declaration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unbounded_custom_pool_admits_concurrently() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let gates = tempfile::tempdir()?;
    let gate = gates.path().join("open");
    let res = r#"{"cpu": 1, "gpu": 1000}"#;
    write_build(
        ws.path(),
        "p",
        &format!("{}{}", gated("a", &gate, res), gated("b", &gate, res)),
    );
    let mut pools = caps(&[("cpu", 4 * MICROS)]);
    pools.insert(
        PoolName::try_from("gpu".to_string())?,
        PoolSpec {
            unit: Unit::Count,
            capacity: None,
        },
    );
    let engine = engine(ws.path(), pools)?;
    let p = probe(&engine, &["//p:a", "//p:b"], &gate, |snap, started| {
        started == 2 && snap.main.queued == 0
    })
    .await?;
    assert_eq!(held(&p.snapshot, "gpu"), 2000, "{:?}", p.snapshot);
    let trace = execute_trace(&p.events);
    assert_eq!(trace.len(), 4, "{trace:?}");
    assert!(
        trace
            .get(..2)
            .is_some_and(|t| t.iter().all(|(k, _)| *k == "start")),
        "both must start before either ends: {trace:?}"
    );
    Ok(())
}

/// `cpu: 16` on a 4-cpu pool is clamped to the whole pool: it runs, runs
/// alone (a `cpu: 1` target queues behind it), and its `${resource:cpu}` is 4.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_declaration_runs_alone() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let gates = tempfile::tempdir()?;
    let gate = gates.path().join("open");
    write_build(
        ws.path(),
        "p",
        &format!(
            "{}{}",
            gated("big", &gate, r#"{"cpu": 16}"#),
            gated("small", &gate, r#"{"cpu": 1}"#)
        ),
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    let trace = first_then_second(&engine, "//p:big", 4 * MICROS, "//p:small", &gate).await?;
    assert_eq!(
        trace,
        vec![
            ("start", "//p:big".to_string()),
            ("end", "//p:big".to_string()),
            ("start", "//p:small".to_string()),
            ("end", "//p:small".to_string()),
        ],
        "the oversize target runs alone"
    );
    assert_eq!(output(&engine, "//p:big").await?, "big 4", "told the clamp");
    assert_eq!(output(&engine, "//p:small").await?, "small 1");
    Ok(())
}

/// The single output of `addr`, trimmed (a cache hit if it already ran).
async fn output(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    let r = Arc::clone(engine)
        .result_addr(
            engine.new_state(),
            &parse_addr(addr)?,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    Ok(crate::common::artifact_string(&r).trim().to_string())
}

/// The `hashin` of `addr` on `engine`.
async fn hashin(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    Ok(Arc::clone(engine)
        .meta(engine.new_state(), &parse_addr(addr)?)
        .await?
        .hashin
        .clone())
}

/// D7: a `cpu: 3` target's `${resource:cpu}` is 3, and a target that
/// declares nothing is told the 1 cpu it was granted.
#[tokio::test]
async fn target_sees_its_allotment() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "t", driver = "bash", run = "echo $N > $OUT", out = "t.txt", env = {"N": "${resource:cpu}"}, resources = {"cpu": 3})
target(name = "plain", driver = "bash", run = "echo $N > $OUT", out = "p.txt", env = {"N": "${resource:cpu}"})
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    assert_eq!(output(&engine, "//p:t").await?, "3");
    assert_eq!(output(&engine, "//p:plain").await?, "1");
    Ok(())
}

/// `${resource:memory}` is the declared memory, in bytes.
#[tokio::test]
async fn target_sees_its_memory_allotment() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"target(name = "mem", driver = "bash", run = "echo $M > $OUT", out = "mem.txt", env = {"M": "${resource:memory}"}, resources = {"memory": "1G"})"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS), ("memory", 2 << 30)]))?;
    assert_eq!(output(&engine, "//p:mem").await?, (1u64 << 30).to_string());
    Ok(())
}

/// `${resource:gpu}` on a custom count pool is the declared count, in any
/// deferrable field (here `env` and the `run` text itself).
#[tokio::test]
async fn custom_pool_allotment_renders() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"target(name = "t", driver = "bash", run = "echo $G ${resource:gpu} > $OUT", out = "t.txt", env = {"G": "${resource:gpu}"}, resources = {"gpu": 2})"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS), ("gpu", 3)]))?;
    assert_eq!(output(&engine, "//p:t").await?, "2 2");
    Ok(())
}

/// The link error `get_def` of `addr` fails with, rendered as the CLI does.
async fn link_error(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    Arc::clone(engine)
        .get_def(engine.new_state(), &parse_addr(addr)?)
        .await
        .err()
        .map(|e| format!("{e:#}"))
        .ok_or_else(|| anyhow::anyhow!("{addr}: must be refused at link"))
}

fn assert_has(err: &str, needles: &[&str]) {
    for needle in needles {
        assert!(err.contains(needle), "missing {needle:?} in {err}");
    }
}

/// A `${resource:<pool>}` naming a pool the engine has but the target does
/// not reserve is refused when the def is linked, naming the field and the
/// fix; reserving it lets it through. Declaring 0 of it says so, rather than
/// telling the author to add what they already wrote.
#[tokio::test]
async fn reference_to_an_unreserved_pool_is_refused_at_link() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "bad", driver = "bash", run = "echo $G > $OUT", out = "b.txt", env = {"G": "${resource:gpu}"})
target(name = "zero", driver = "bash", run = "echo $G > $OUT", out = "z.txt", env = {"G": "${resource:gpu}"}, resources = {"gpu": 0})
target(name = "ok", driver = "bash", run = "echo $G > $OUT", out = "o.txt", env = {"G": "${resource:gpu}"}, resources = {"gpu": 1})
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", MICROS), ("gpu", 1)]))?;
    assert_has(
        &link_error(&engine, "//p:bad").await?,
        &[
            "//p:bad",
            "env.G",
            "reserves no gpu",
            r#"add resources = {"gpu": <amount>}"#,
        ],
    );
    let zero = link_error(&engine, "//p:zero").await?;
    assert_has(
        &zero,
        &["//p:zero", "declares gpu: 0", "declare an amount above 0"],
    );
    assert!(!zero.contains("add resources"), "{zero}");
    assert_eq!(output(&engine, "//p:ok").await?, "1");
    Ok(())
}

/// A pool-shaped name that is no configured pool is a link error with a
/// "did you mean" when one is near, the two ways to declare it, and the
/// known pools; a far name gets no guess.
#[tokio::test]
async fn reference_to_an_unconfigured_pool_is_refused_with_a_hint() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "typo", driver = "bash", run = "echo $X > $OUT", out = "t.txt", env = {"X": "${resource:cpus}"})
target(name = "gpus", driver = "bash", run = "echo ${resource:gpus} > $OUT", out = "g.txt", resources = {"gpu": 1})
target(name = "far", driver = "bash", run = "echo $X > $OUT", out = "f.txt", env = {"X": "-j${resource:licence_server}"})
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", MICROS), ("gpu", 1)]))?;
    assert_has(
        &link_error(&engine, "//p:typo").await?,
        &[
            "//p:typo",
            "env.X",
            "${resource:cpus}",
            "cpus is not a pool (did you mean cpu?)",
            "add resources: {cpus: <n>} to .hephconfig",
            "--resource cpus=<n>",
            "(known: cpu, gpu, memory)",
        ],
    );
    assert_has(
        &link_error(&engine, "//p:gpus").await?,
        &["//p:gpus", "run", "did you mean gpu?"],
    );
    let far = link_error(&engine, "//p:far").await?;
    assert_has(&far, &["licence_server is not a pool:"]);
    assert!(!far.contains("did you mean"), "{far}");
    Ok(())
}

/// `sandbox` is the engine's pool: it always exists, but a target may neither
/// declare it (the engine demands one per managed run on its own) nor read
/// it (no part of the allotment). Both are link errors naming the pool and
/// how to size it instead; neither is mistaken for an unknown pool.
#[tokio::test]
async fn the_sandbox_pool_is_neither_declared_nor_read_by_a_target() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "declares", driver = "bash", run = "echo t > $OUT", out = "d.txt", resources = {"sandbox": 1})
target(name = "reads", driver = "bash", run = "echo $S > $OUT", out = "r.txt", env = {"S": "${resource:sandbox}"})
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", MICROS)]))?;
    let declares = link_error(&engine, "//p:declares").await?;
    assert_has(
        &declares,
        &[
            "//p:declares declares resources.sandbox",
            "sandbox is the engine's",
            "--resource sandbox=<n>",
        ],
    );
    assert!(!declares.contains("is not a pool"), "{declares}");
    let reads = link_error(&engine, "//p:reads").await?;
    assert_has(
        &reads,
        &[
            "//p:reads",
            "env.S",
            "${resource:sandbox}",
            "sandbox is the engine's",
        ],
    );
    assert!(!reads.contains("is not a pool"), "{reads}");
    Ok(())
}

/// U5: the allotment never reaches the key. A `cpu: 4` target built under
/// capacity 1 (told 1), then under capacity 4: the same `hashin`, and the
/// second run is served the first's output.
#[tokio::test]
async fn jobs_flag_does_not_move_the_cache_key() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"target(name = "t", driver = "bash", run = "echo $N > $OUT", out = "t.txt", env = {"N": "${resource:cpu}"}, resources = {"cpu": 4})"#,
    );
    let one = engine(ws.path(), caps(&[("cpu", MICROS)]))?;
    let (res, events) = build_events(&one, &["//p:t"]).await;
    res?;
    assert_eq!(starts(&events), vec!["//p:t"]);
    let before = hashin(&one, "//p:t").await?;

    let four = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    let (res, events) = build_events(&four, &["//p:t"]).await;
    res?;
    assert!(starts(&events).is_empty(), "{:?}", starts(&events));
    assert_eq!(before, hashin(&four, "//p:t").await?);
    assert_eq!(output(&four, "//p:t").await?, "1", "run 1's output");
    Ok(())
}

/// Build `first` alone until it holds `first_cpu` of the cpu pool, then
/// `second`, until the pool shows it queued; open `gate`, let both finish, and
/// return the execute trace of the two.
async fn first_then_second(
    engine: &Arc<Engine>,
    first: &str,
    first_cpu: u64,
    second: &str,
    gate: &Path,
) -> anyhow::Result<Vec<(&'static str, String)>> {
    let (tx, mut rx): (_, EventReceiver) = tokio::sync::mpsc::unbounded_channel();
    let a = tokio::spawn(build(
        Arc::clone(engine),
        vec![first.to_string()],
        Some(tx.clone()),
    ));
    let start = Instant::now();
    let until = async |done: &dyn Fn(&ResourceSnapshot) -> bool| {
        while !engine.resource_snapshot().is_some_and(|s| done(&s)) {
            if start.elapsed() > DEADLINE {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        true
    };
    let admitted = until(&|s| held(s, "cpu") == first_cpu).await;
    let b = tokio::spawn(build(
        Arc::clone(engine),
        vec![second.to_string()],
        Some(tx),
    ));
    let queued = admitted && until(&|s| s.main.queued == 1).await;
    let snapshot = engine.resource_snapshot();
    // Open the gate whatever happened, so a failing probe never leaves a
    // target blocked behind it.
    std::fs::write(gate, "")?;
    let a = tokio::time::timeout(DEADLINE, a).await;
    let b = tokio::time::timeout(DEADLINE, b).await;
    anyhow::ensure!(
        admitted,
        "{first} was never admitted (snapshot {snapshot:?})"
    );
    anyhow::ensure!(queued, "{second} never queued (snapshot {snapshot:?})");
    a???;
    b???;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    Ok(execute_trace(&events))
}

const UNKNOWN_POOL: &str = "declares resources.docker, but docker is not a pool: add resources: {docker: <n>} to .hephconfig, or --resource docker=<n> for one run (known: cpu, memory)";

#[tokio::test]
async fn unknown_pool_is_a_def_time_error() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt", resources = {"docker": 1})"#,
    );
    let engine = engine(ws.path(), BTreeMap::new())?;

    let rs = engine.new_state();
    let err = Arc::clone(&engine)
        .get_def(rs, &parse_addr("//p:t")?)
        .await
        .err()
        .expect("an unknown pool fails the def");
    assert!(
        format!("{err:#}").contains(&format!("//p:t {UNKNOWN_POOL}")),
        "{err:#}"
    );

    let (res, events) = build_events(&engine, &["//p:t"]).await;
    let err = res.expect_err("the run fails");
    assert!(format!("{err:#}").contains(UNKNOWN_POOL), "{err:#}");
    assert!(
        starts(&events).is_empty(),
        "nothing ran: {:?}",
        starts(&events)
    );
    Ok(())
}

/// An amount that is not a count (`"64G"`) on an undeclared pool: the hint
/// names the unit the new pool needs, since a bare `<n>` would declare a
/// count and then refuse the same amount.
#[tokio::test]
async fn unknown_pool_hint_names_the_unit_of_the_amount() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt", resources = {"scratch": "64G"})"#,
    );
    let engine = engine(ws.path(), BTreeMap::new())?;

    let rs = engine.new_state();
    let err = Arc::clone(&engine)
        .get_def(rs, &parse_addr("//p:t")?)
        .await
        .err()
        .expect("an unknown pool fails the def");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("declares resources.scratch") && msg.contains("{unit: bytes, value: <n>}"),
        "{msg}"
    );
    assert!(msg.contains("--resource"), "{msg}");
    Ok(())
}

/// The dep's error survives the memoizer and the consumer's wrapping: the
/// message still names `//p:lib`, the pool and the key.
#[tokio::test]
async fn unknown_pool_on_a_dep_still_names_the_dep() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "lib", driver = "bash", run = "echo lib > $OUT", out = "lib.txt", resources = {"docker": 1})
target(name = "app", driver = "bash", run = "cat $SRC > $OUT", out = "app.txt", deps = ["//p:lib"])
"#,
    );
    let engine = engine(ws.path(), BTreeMap::new())?;
    let (res, events) = build_events(&engine, &["//p:app"]).await;
    let err = format!("{:#}", res.expect_err("the run fails"));
    for needle in ["//p:lib", "docker", "resources.docker", UNKNOWN_POOL] {
        assert!(err.contains(needle), "missing {needle:?} in {err}");
    }
    assert!(
        starts(&events).is_empty(),
        "no dep is built: {:?}",
        starts(&events)
    );
    Ok(())
}

const GPU_TARGET: &str = r#"target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt", resources = {"gpu": 1})"#;

#[tokio::test]
async fn zero_capacity_pool_errors_at_admission_naming_the_override() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(ws.path(), "p", GPU_TARGET);
    let engine = engine(ws.path(), caps(&[("gpu", 0)]))?;
    let (res, events) = build_events(&engine, &["//p:t"]).await;
    let err = format!("{:#}", res.expect_err("a pool at capacity 0 fails the run"));
    for needle in [
        "//p:t acquires gpu, but pool gpu has capacity 0",
        "--resource gpu=<n>",
        "HEPH_RESOURCE_GPU=<n>",
        "resources: {gpu: <n>} in .hephconfig",
    ] {
        assert!(err.contains(needle), "missing {needle:?} in {err}");
    }
    assert!(
        starts(&events).is_empty(),
        "fails before the target runs: {:?}",
        starts(&events)
    );
    Ok(())
}

/// `heph validate`'s link walk (get_def, then link) is green on a pool at
/// capacity 0: the pool exists, the run sets it.
#[tokio::test]
async fn validate_passes_with_a_zero_capacity_pool() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(ws.path(), "p", GPU_TARGET);
    let engine = engine(ws.path(), caps(&[("gpu", 0)]))?;
    let rs = engine.new_state();
    let def = Arc::clone(&engine)
        .get_def(rs.clone(), &parse_addr("//p:t")?)
        .await?;
    Arc::clone(&engine)
        .link(rs, Arc::clone(&def.target_def))
        .await?;
    Ok(())
}

/// A per-run override (what `--resource gpu=1` / `HEPH_RESOURCE_GPU=1` resolve
/// to in `Config.resources`) over the config's 0 lets it run.
#[tokio::test]
async fn override_lifts_a_zero_capacity_pool() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(ws.path(), "p", GPU_TARGET);
    let engine = engine(ws.path(), caps(&[("gpu", 1)]))?;
    let (res, events) = build_events(&engine, &["//p:t"]).await;
    res?;
    assert_eq!(starts(&events), vec!["//p:t"]);
    Ok(())
}

/// U5: the declaration reaches no hash. Rebuilding after editing `resources`
/// is a cache hit with the same `hashin`.
#[tokio::test]
async fn changing_resources_keeps_the_cache_hit() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let t = |cpu: u32| {
        format!(
            r#"target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt", resources = {{"cpu": {cpu}, "docker": 1}})"#
        )
    };
    let capacities = &[("cpu", 4 * MICROS), ("docker", 2)];
    let addr = parse_addr("//p:t")?;

    write_build(ws.path(), "p", &t(1));
    let first = engine(ws.path(), caps(capacities))?;
    let (res, events) = build_events(&first, &["//p:t"]).await;
    res?;
    assert_eq!(starts(&events), vec!["//p:t"], "the first build runs");
    let before = Arc::clone(&first)
        .meta(first.new_state(), &addr)
        .await?
        .hashin;

    write_build(ws.path(), "p", &t(2));
    let second = engine(ws.path(), caps(capacities))?;
    let def = Arc::clone(&second)
        .get_def(second.new_state(), &addr)
        .await?;
    assert_eq!(
        def.target_def.demand.declared("cpu"),
        Some(2 * MICROS),
        "the edit reached the def"
    );
    let (res, events) = build_events(&second, &["//p:t"]).await;
    res?;
    assert!(
        starts(&events).is_empty(),
        "an edited declaration must not rebuild: {:?}",
        starts(&events)
    );
    let after = Arc::clone(&second)
        .meta(second.new_state(), &addr)
        .await?
        .hashin;
    assert_eq!(before, after);
    Ok(())
}

/// A `group` (an in-process driver) declaring `cpu: 2` takes the pool: on a
/// 2-cpu pool it queues behind a running `cpu: 1` target, then runs.
///
/// A relocating group (`prefix`), because an aggregate group is transparent:
/// it is spliced into its consumers and never executes, so it never reaches
/// admission and its `resources` reserve nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_target_with_resources_reserves_and_runs() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let gates = tempfile::tempdir()?;
    let gate = gates.path().join("open");
    write_build(
        ws.path(),
        "p",
        &format!(
            r#"{}
target(name = "leaf", driver = "bash", run = "echo leaf > $OUT", out = "leaf.txt", resources = {{"cpu": 0}})
target(name = "g", driver = "group", deps = ["//p:leaf"], prefix = "moved", resources = {{"cpu": 2}})
"#,
            gated("a", &gate, r#"{"cpu": 1}"#)
        ),
    );
    let engine = engine(ws.path(), caps(&[("cpu", 2 * MICROS)]))?;
    let def = Arc::clone(&engine)
        .get_def(engine.new_state(), &parse_addr("//p:g")?)
        .await?;
    assert_eq!(def.target_def.demand.declared("cpu"), Some(2 * MICROS));

    // `a` first, so `g` is the one that waits.
    let trace: Vec<_> = first_then_second(&engine, "//p:a", MICROS, "//p:g", &gate)
        .await?
        .into_iter()
        .filter(|(_, addr)| addr != "//p:leaf")
        .collect();
    assert_eq!(
        trace,
        vec![
            ("start", "//p:a".to_string()),
            ("end", "//p:a".to_string()),
            ("start", "//p:g".to_string()),
            ("end", "//p:g".to_string()),
        ],
        "g waited for a's cpu"
    );
    // Told nothing: a group spawns nothing, so its allotment reaches no env
    // and its output is the dep's bytes, unchanged.
    assert_eq!(output(&engine, "//p:g").await?, "leaf");
    Ok(())
}

/// An aggregate group (no `prefix`) is spliced into its consumers and never
/// executes, so its `resources` reserve nothing: one that names a pool at
/// capacity 0 — which admission would refuse — still builds, and never starts.
#[tokio::test]
async fn aggregate_group_resources_reserve_nothing() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "leaf", driver = "bash", run = "echo leaf > $OUT", out = "leaf.txt")
target(name = "g", driver = "group", deps = ["//p:leaf"], resources = {"docker": 1})
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", MICROS), ("docker", 0)]))?;
    let def = Arc::clone(&engine)
        .get_def(engine.new_state(), &parse_addr("//p:g")?)
        .await?;
    assert_eq!(def.target_def.demand.declared("docker"), Some(1));
    let (res, events) = build_events(&engine, &["//p:g"]).await;
    res?;
    assert_eq!(starts(&events), vec!["//p:leaf"], "g never executes");
    assert_eq!(output(&engine, "//p:g").await?, "leaf");
    Ok(())
}

/// `${resource:memory}` with no memory declared is refused when the def is
/// linked — so after a declaration is dropped, a cache hit cannot hide it —
/// and a declaration lets it through.
#[tokio::test]
async fn memory_reference_without_memory_is_a_def_time_error() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "bad", driver = "bash", run = "echo $M > $OUT", out = "b.txt", env = {"M": "${resource:memory}"})
target(name = "zero", driver = "bash", run = "echo $M > $OUT", out = "z.txt", env = {"M": "${resource:memory}"}, resources = {"memory": 0})
target(name = "cpu_only", driver = "bash", run = "echo $M > $OUT", out = "c.txt", env = {"M": "${resource:cpu}"})
target(name = "ok", driver = "bash", run = "echo $M > $OUT", out = "o.txt", env = {"M": "${resource:memory}"}, resources = {"memory": "1K"})
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", MICROS), ("memory", 1 << 20)]))?;
    for name in ["bad", "zero"] {
        let err = Arc::clone(&engine)
            .get_def(engine.new_state(), &parse_addr(&format!("//p:{name}"))?)
            .await
            .err()
            .unwrap_or_else(|| panic!("{name}: refused at link"));
        let msg = format!("{err:#}");
        assert!(msg.contains(&format!("//p:{name}")), "{msg}");
        let why = if name == "zero" {
            "declares memory: 0"
        } else {
            "reserves no memory"
        };
        assert!(msg.contains(why), "{msg}");
        assert!(msg.contains("env.M"), "{msg}");
    }
    assert_eq!(output(&engine, "//p:cpu_only").await?, "1");
    assert_eq!(output(&engine, "//p:ok").await?, "1024");
    Ok(())
}

/// D11: `heph inspect def` serialises the def, and `resources` is on it as
/// written; a target that declares nothing prints no key.
#[tokio::test]
async fn inspect_def_prints_resources() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt", resources = {"cpu": 0.5, "memory": "1G", "docker": 1})
target(name = "plain", driver = "bash", run = "echo t > $OUT", out = "t.txt")
"#,
    );
    let engine = engine(ws.path(), caps(&[("docker", 1)]))?;
    let def = Arc::clone(&engine)
        .get_def(engine.new_state(), &parse_addr("//p:t")?)
        .await?;
    let json = serde_json::to_value(&*def.target_def)?;
    assert_eq!(
        json.get("resources"),
        Some(&serde_json::json!({"cpu": "0.5", "docker": "1", "memory": "1G"})),
        "{json}"
    );
    let plain = Arc::clone(&engine)
        .get_def(engine.new_state(), &parse_addr("//p:plain")?)
        .await?;
    let json = serde_json::to_value(&*plain.target_def)?;
    assert!(json.get("resources").is_none(), "{json}");
    Ok(())
}

/// D10: `heph validate`'s link walk reports an unknown pool with the message
/// that names the fix.
#[tokio::test]
async fn validate_reports_an_unknown_pool() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"target(name = "t", driver = "bash", run = "echo t > $OUT", out = "t.txt", resources = {"docker": 1})"#,
    );
    let engine = engine(ws.path(), caps(&[("gpu", 0)]))?;
    let rs = engine.new_state();
    let res = async {
        let def = Arc::clone(&engine)
            .get_def(rs.clone(), &parse_addr("//p:t")?)
            .await?;
        Arc::clone(&engine)
            .link(rs.clone(), Arc::clone(&def.target_def))
            .await?;
        anyhow::Ok(())
    }
    .await;
    let err = format!("{:#}", res.expect_err("validate reports it"));
    assert!(
        err.contains(
            "//p:t declares resources.docker, but docker is not a pool: add resources: {docker: <n>} to .hephconfig, or --resource docker=<n> for one run (known: cpu, gpu, memory)"
        ),
        "{err}"
    );
    Ok(())
}

/// `(declared, reserved)` of `addr`'s `ExecuteStart`.
type Announced = (Option<BTreeMap<String, u64>>, Option<BTreeMap<String, u64>>);

fn announced(events: &[BuildEvent], addr: &str) -> Option<Announced> {
    events.iter().find_map(|e| match &e.kind {
        BuildEventKind::ExecuteStart {
            addr: a,
            declared,
            reserved,
            ..
        } if a == addr => Some((declared.clone(), reserved.clone())),
        _ => None,
    })
}

/// D8: `RequestConfig` announces every pool's capacity (and `max_workers`,
/// the cpu capacity in CPUs); `ExecuteStart` carries what the target declared
/// and what it was granted — clamped to the pool, and `cpu: 1` for a target
/// that declares nothing. The `sandbox` slot each run also holds is the
/// engine's, never part of `reserved`.
#[tokio::test]
async fn request_config_announces_pools_and_execute_start_reserved() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "big", driver = "bash", run = "echo big > $OUT", out = "big.txt", resources = {"cpu": 16})
target(name = "plain", driver = "bash", run = "echo plain > $OUT", out = "plain.txt")
"#,
    );
    let engine = engine(
        ws.path(),
        caps(&[("cpu", 4 * MICROS), ("memory", 8 << 30), ("docker", 2)]),
    )?;
    let (res, events) = build_events(&engine, &["//p:big", "//p:plain"]).await;
    res?;

    let configs: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            BuildEventKind::RequestConfig {
                max_workers,
                resources,
                ..
            } => Some((*max_workers, resources.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        configs,
        vec![(
            4,
            BTreeMap::from([
                ("cpu".to_string(), Some(4 * MICROS)),
                ("docker".to_string(), Some(2)),
                ("memory".to_string(), Some(8 << 30)),
                // The engine's own, at its default of 4 per cpu.
                ("sandbox".to_string(), Some(16)),
            ])
        )],
        "one RequestConfig, every pool's capacity"
    );

    assert_eq!(
        announced(&events, "//p:big"),
        Some((
            Some(BTreeMap::from([("cpu".to_string(), 16 * MICROS)])),
            Some(BTreeMap::from([("cpu".to_string(), 4 * MICROS)])),
        )),
        "declared as written, reserved clamped to the pool"
    );
    assert_eq!(
        announced(&events, "//p:plain"),
        Some((
            Some(BTreeMap::new()),
            Some(BTreeMap::from([("cpu".to_string(), MICROS)])),
        )),
        "nothing declared, one cpu reserved"
    );
    Ok(())
}

/// An `AdmissionWaitStart`: `(addr, pool, need, free, ahead)`.
type WaitStart = (String, String, u64, u64, u64);

/// The admission-wait events of a stream: each Start, and the addrs of the
/// Ends.
fn admission_waits(events: &[BuildEvent]) -> (Vec<WaitStart>, Vec<String>) {
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    for e in events {
        match &e.kind {
            BuildEventKind::AdmissionWaitStart {
                addr,
                pool,
                need,
                free,
                ahead,
            } => starts.push((addr.clone(), pool.clone(), *need, *free, *ahead)),
            BuildEventKind::AdmissionWaitEnd { addr } => ends.push(addr.clone()),
            _ => {}
        }
    }
    (starts, ends)
}

/// D8: a wait past the notice threshold is announced as
/// `AdmissionWaitStart { pool, need, free, ahead }`, paired with an
/// `AdmissionWaitEnd` before the target's `ExecuteStart`. Three `cpu: 1`
/// targets on a 1-cpu pool: one runs, the other two wait, the second of them
/// behind the first. An uncontended run announces nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_wait_is_announced_past_the_notice_threshold() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    let gates = tempfile::tempdir()?;
    let gate = gates.path().join("open");
    write_build(
        ws.path(),
        "p",
        &format!(
            "{}{}{}",
            gated("a", &gate, r#"{"cpu": 1}"#),
            gated("b", &gate, r#"{"cpu": 1}"#),
            gated("c", &gate, r#"{"cpu": 1}"#)
        ),
    );
    let engine = engine_with_notice(
        ws.path(),
        caps(&[("cpu", MICROS)]),
        Some(Duration::from_millis(50)),
    )?;

    let (tx, mut rx): (_, EventReceiver) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(build(
        Arc::clone(&engine),
        ["//p:a", "//p:b", "//p:c"].map(String::from).to_vec(),
        Some(tx),
    ));
    let mut events = Vec::new();
    let start = Instant::now();
    // Both waiters announced while the gate is closed.
    while admission_waits(&events).0.len() < 2 && !task.is_finished() && start.elapsed() < DEADLINE
    {
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (gated_starts, _) = admission_waits(&events);
    let started_while_gated = starts(&events).len();
    std::fs::write(&gate, "")?;
    tokio::time::timeout(DEADLINE, task).await???;
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    assert_eq!(
        started_while_gated, 1,
        "one target ran while the gate was shut"
    );
    let mut seen: Vec<(String, u64, u64, u64)> = gated_starts
        .iter()
        .map(|(_, pool, need, free, ahead)| (pool.clone(), *need, *free, *ahead))
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("cpu".to_string(), MICROS, 0, 0),
            ("cpu".to_string(), MICROS, 0, 1),
        ],
        "the head waits with nothing ahead, the next behind it: {gated_starts:?}"
    );

    let (all_starts, mut ends) = admission_waits(&events);
    let mut waited: Vec<String> = all_starts.into_iter().map(|s| s.0).collect();
    waited.sort();
    ends.sort();
    assert_eq!(waited, ends, "every Start has its End");
    for addr in &waited {
        let pos =
            |want: &dyn Fn(&BuildEventKind) -> bool| events.iter().position(|e| want(&e.kind));
        let end = pos(&|k| matches!(k, BuildEventKind::AdmissionWaitEnd { addr: a } if a == addr));
        let exec = pos(&|k| matches!(k, BuildEventKind::ExecuteStart { addr: a, .. } if a == addr));
        assert!(
            end.is_some() && end < exec,
            "{addr}: the wait ends before the execute starts ({end:?}, {exec:?})"
        );
    }

    // Uncontended: room for everything, nothing announced.
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "x", driver = "bash", run = "echo x > $OUT", out = "x.txt", resources = {"cpu": 1})
target(name = "y", driver = "bash", run = "echo y > $OUT", out = "y.txt", resources = {"cpu": 1})
"#,
    );
    let engine = engine_with_notice(
        ws.path(),
        caps(&[("cpu", 4 * MICROS)]),
        Some(Duration::from_millis(50)),
    )?;
    let (res, events) = build_events(&engine, &["//p:x", "//p:y"]).await;
    res?;
    assert_eq!(
        admission_waits(&events),
        (vec![], vec![]),
        "an uncontended run announces no wait"
    );
    Ok(())
}

/// A `MakeWriter` into a shared buffer, for capturing log lines in-test.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// D8: the clamp is logged at `info` once per addr per request, however many
/// dependents reach the clamped target. A current-thread runtime, so the
/// thread-local subscriber sees every engine task.
#[tokio::test]
async fn clamp_is_logged_once_per_addr_per_request() -> anyhow::Result<()> {
    use tracing_subscriber::util::SubscriberInitExt as _;
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "big", driver = "bash", run = "echo big > $OUT", out = "big.txt", resources = {"cpu": 16})
target(name = "a", driver = "bash", run = "echo a > $OUT", out = "a.txt", deps = ["//p:big"])
target(name = "b", driver = "bash", run = "echo b > $OUT", out = "b.txt", deps = ["//p:big"])
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    let log = Captured::default();
    let writer = log.clone();
    let res = {
        // A second live dispatcher, held for the test. With only one,
        // tracing-core caches a callsite's interest from whichever thread
        // registers it first, and under `cargo test` that is often another
        // test's thread with no subscriber: the clamp line would be cached as
        // "never" and lost here. With two, interest comes from every live
        // dispatcher. (nextest runs each test alone in a process.)
        let _second = tracing_subscriber::registry().set_default();
        let _guard = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
            .set_default();
        build_events(&engine, &["//p:a", "//p:b"]).await.0
    };
    res?;
    let text = String::from_utf8(log.0.lock().expect("log buffer").clone())?;
    let clamps: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("reservation clamped"))
        .collect();
    assert_eq!(clamps.len(), 1, "one clamp line: {text}");
    let line = clamps.first().copied().unwrap_or_default();
    assert!(line.contains("INFO") && line.contains("//p:big"), "{line}");
    Ok(())
}

/// D7: `${resource:cpu}` in an `env` value is substituted at run with the
/// run's whole CPUs, while the def hash covers the reference text — so the
/// key does not move with the capacity the run was granted.
#[tokio::test]
async fn reference_in_env_is_hashed_as_text_and_expanded_at_run() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"target(name = "t", driver = "bash", run = "echo $MAKEFLAGS/$RT > $OUT", out = "t.txt", env = {"MAKEFLAGS": "-j${resource:cpu}"}, runtime_env = {"RT": "rt${resource:cpu}"}, resources = {"cpu": 3})"#,
    );
    let four = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    assert_eq!(output(&four, "//p:t").await?, "-j3/rt3");
    let one = engine(ws.path(), caps(&[("cpu", MICROS)]))?;
    assert_eq!(hashin(&four, "//p:t").await?, hashin(&one, "//p:t").await?);
    Ok(())
}

/// A dependency's transitive `env` takes no deferred reference of any kind —
/// `${resource:}` included: it is the dependent's link error, naming the
/// variable, never text that reaches the process.
#[tokio::test]
async fn deferred_reference_in_transitive_env_is_refused() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "src", driver = "bash", run = "echo x > $OUT", out = "s.txt")
target(name = "res", driver = "bash", run = "echo x > $OUT", out = "r.txt",
       transitive = {"env": {"MAKEFLAGS": "-j${resource:cpu}"}})
target(name = "rd", driver = "bash", run = "echo x > $OUT", out = "rd.txt",
       transitive = {"env": {"R": "${read://p:src}"}})
target(name = "sr", driver = "bash", run = "echo x > $OUT", out = "sr.txt",
       transitive = {"env": {"S": "${src://p:src}"}})
target(name = "c_res", driver = "bash", run = "echo x > $OUT", out = "c1.txt",
       deps = ["//p:res"], resources = {"cpu": 2})
target(name = "c_rd", driver = "bash", run = "echo x > $OUT", out = "c2.txt", deps = ["//p:rd"])
target(name = "c_sr", driver = "bash", run = "echo x > $OUT", out = "c3.txt", deps = ["//p:sr"])
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    for (addr, name, reference) in [
        ("//p:c_res", "MAKEFLAGS", "${resource:cpu}"),
        ("//p:c_rd", "R", "${read://p:src}"),
        ("//p:c_sr", "S", "${src://p:src}"),
    ] {
        assert_has(
            &link_error(&engine, addr).await?,
            &[
                &format!("transitive `env.{name}`"),
                reference,
                "does not accept one",
            ],
        );
    }
    Ok(())
}

/// An argument that is not pool-shaped (`[a-z][a-z0-9_]{0,63}`) is not
/// heph's: the text reaches the target's environment untouched, like any
/// other `${…}` heph does not own — beside a real reference too.
#[tokio::test]
async fn non_pool_shaped_resource_reference_passes_through() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"target(name = "t", driver = "bash", run = 'echo "$X" > $OUT', out = "t.txt", env = {"X": "${resource:CPU} ${resource:-x} ${resource:1gpu} ${resource:cpu}"}, resources = {"cpu": 2})"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    assert_eq!(
        output(&engine, "//p:t").await?,
        "${resource:CPU} ${resource:-x} ${resource:1gpu} 2"
    );
    Ok(())
}

/// Custom pools in other units: a `bytes` pool's amount is read as a size
/// and rendered in bytes, a `cpu`-unit pool's fraction renders rounded up,
/// and an amount its pool's unit refuses is a link error naming the target,
/// the pool and the unit.
#[tokio::test]
async fn custom_pools_read_and_render_in_their_unit() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "vram", driver = "bash", run = "echo $V > $OUT", out = "v.txt",
       env = {"V": "${resource:vram}"}, resources = {"vram": "8G"})
target(name = "slots", driver = "bash", run = "echo $S > $OUT", out = "s.txt",
       env = {"S": "${resource:slots}"}, resources = {"slots": 0.5})
target(name = "half_gpu", driver = "bash", run = "echo x > $OUT", out = "g.txt",
       resources = {"gpu": 0.5})
"#,
    );
    let mut pools = caps(&[("cpu", 4 * MICROS), ("gpu", 2)]);
    for (name, unit, capacity) in [
        ("vram", Unit::Bytes, 16u64 << 30),
        ("slots", Unit::Cpu, 2 * MICROS),
    ] {
        pools.insert(
            PoolName::try_from(name.to_string())?,
            PoolSpec {
                unit,
                capacity: Some(Capacity::Amount(capacity)),
            },
        );
    }
    let engine = engine(ws.path(), pools)?;
    assert_eq!(output(&engine, "//p:vram").await?, "8589934592");
    assert_eq!(output(&engine, "//p:slots").await?, "1");
    assert_has(
        &link_error(&engine, "//p:half_gpu").await?,
        &["//p:half_gpu", "resources.gpu", "count", "\"0.5\""],
    );
    Ok(())
}

/// What a target reserves is its parse's: a driver whose
/// `apply_transitive` rebuilds the def without `resources` (as a cdylib
/// across the ABI may) does not drop the declaration.
#[tokio::test]
async fn resources_survive_apply_transitive() -> anyhow::Result<()> {
    struct DropsResources(pluginexec::Driver);
    #[async_trait::async_trait]
    impl heph::engine::driver_managed::ManagedDriver for DropsResources {
        fn config(
            &self,
            _req: hplugin::driver::ConfigRequest,
        ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
            Ok(hplugin::driver::ConfigResponse {
                name: "drops".to_string(),
            })
        }
        fn schema(&self) -> hplugin::driver::DriverSchema {
            heph::engine::driver_managed::ManagedDriver::schema(&self.0)
        }
        async fn parse(
            &self,
            req: hplugin::driver::ParseRequest,
            ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<hplugin::driver::ParseResponse> {
            heph::engine::driver_managed::ManagedDriver::parse(&self.0, req, ctoken).await
        }
        async fn apply_transitive(
            &self,
            req: hplugin::driver::ApplyTransitiveRequest,
            ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
            let mut res =
                heph::engine::driver_managed::ManagedDriver::apply_transitive(&self.0, req, ctoken)
                    .await?;
            res.target_def.resources = Default::default();
            Ok(res)
        }
        async fn run<'a, 'io>(
            &self,
            req: heph::engine::driver_managed::ManagedRunRequest<'a, 'io>,
            ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
        ) -> anyhow::Result<heph::engine::driver_managed::ManagedRunResponse> {
            heph::engine::driver_managed::ManagedDriver::run(&self.0, req, ctoken).await
        }
    }

    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "tool", driver = "drops", run = "echo x > $OUT", out = "tool.txt",
       transitive = {"env": {"T": "t"}})
target(name = "c", driver = "drops", run = "echo $G > $OUT", out = "c.txt", deps = ["//p:tool"],
       env = {"G": "${resource:gpu}"}, resources = {"gpu": 2})
"#,
    );
    let mut e = Engine::new(Config {
        resources: caps(&[("cpu", 4 * MICROS), ("gpu", 2)]),
        ..Config::for_tests(ws.path().to_path_buf())
    })?;
    let r = ws.path().to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| {
        Box::new(DropsResources(
            pluginexec::Driver::new_bash().with_host_path(),
        ))
    })?;
    let engine = Arc::new(e);
    engine.install_exec_runner_host();
    let def = Arc::clone(&engine)
        .get_def(engine.new_state(), &parse_addr("//p:c")?)
        .await?;
    assert_eq!(def.target_def.resources.declared("gpu"), Some("2"));
    assert_eq!(def.target_def.demand.declared("gpu"), Some(2));
    assert_eq!(output(&engine, "//p:c").await?, "2");
    Ok(())
}

/// A value naming a `${src://…}` is finished by the managed layer once the
/// artifact is staged; a `${resource:…}` beside it is substituted in that same
/// pass, from the same allotment.
#[tokio::test]
async fn reference_beside_a_src_reference_is_substituted_in_one_pass() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(name = "cfg", driver = "bash", run = "echo c > $OUT", out = "cfg.txt")
target(name = "t", driver = "bash", out = "t.txt", resources = {"cpu": 2},
       env = {"X": "${src://p:cfg}:${resource:cpu}"},
       run = 'test -f "${X%:*}" && echo "${X##*:}" > $OUT')
"#,
    );
    let engine = engine(ws.path(), caps(&[("cpu", 4 * MICROS)]))?;
    assert_eq!(output(&engine, "//p:t").await?, "2");
    Ok(())
}
