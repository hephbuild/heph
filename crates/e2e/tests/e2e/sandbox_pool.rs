#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The `sandbox` resource pool end to end: a managed run is admitted with one
//! `sandbox` slot alongside its cpu, and the slot comes back only once the
//! reclaim lane has removed the sandbox, so the sandboxes on disk — live plus
//! pending removal — never outnumber the pool's capacity (`--resource
//! sandbox=N`, here `Config.resources`).
//!
//! A slow disk is stood in by `Engine::set_sandbox_removal_delay`, which makes
//! every removal that releases a slot sleep first: removal falls behind
//! creation, which is the condition the pool's bound exists for. A test that
//! needs a slot to stay pending removal while something else happens holds
//! removals with `Engine::set_sandbox_removal_gate` instead, so no assertion
//! rests on a delay being long enough.

use hcore::capacity::PoolSpec;
use heph::engine::request_state::RequestState;
use heph::engine::resource_pool::{PoolSnapshot, ResourceSnapshot};
use heph::engine::sandbox_cleaner::RemovalGate;
use heph::engine::{Config, Engine, InvokeOutcome, InvokeRequest, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::{pluginbuildfile, pluginexec};
use hplugin::provider::PoolName;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Bound on every build here: a leaked slot parks the next run forever, and a
/// hang must fail the test rather than the CI job.
const DEADLINE: Duration = Duration::from_secs(120);

/// An engine over `root` with the buildfile provider, the managed bash driver,
/// the non-managed template driver, 4 cpus and a `sandbox` pool of `slots`;
/// every removal that releases a slot first waits `removal_delay`.
fn engine(root: &Path, slots: usize, removal_delay: Duration) -> anyhow::Result<Arc<Engine>> {
    engine_with(root, slots, |e| e.set_sandbox_removal_delay(removal_delay))
}

/// [`engine`] whose removals that release a slot are held by the returned
/// gate, closed, until the test opens it: a slot stays pending removal for
/// exactly as long as the test needs. Dropping the gate opens it.
fn gated_engine(root: &Path, slots: usize) -> anyhow::Result<(Arc<Engine>, RemovalGate)> {
    let gate = RemovalGate::closed();
    let engine = engine_with(root, slots, |e| e.set_sandbox_removal_gate(&gate))?;
    Ok((engine, gate))
}

/// The engine of [`engine`], with `stall` setting how its removals wait.
fn engine_with(
    root: &Path,
    slots: usize,
    stall: impl FnOnce(&mut Engine),
) -> anyhow::Result<Arc<Engine>> {
    engine_configured(root, slots, |_| {}, stall)
}

/// [`engine_with`], with `configure` adjusting the config first.
fn engine_configured(
    root: &Path,
    slots: usize,
    configure: impl FnOnce(&mut Config),
    stall: impl FnOnce(&mut Engine),
) -> anyhow::Result<Arc<Engine>> {
    let mut cfg = Config {
        parallelism: Some(4),
        ..Config::for_tests(root.to_path_buf())
    };
    configure(&mut cfg);
    cfg.resources.insert(
        PoolName::sandbox(),
        PoolSpec::from_written(PoolName::SANDBOX, None, Some(&slots.to_string()))?,
    );
    let mut e = Engine::new(cfg)?;
    stall(&mut e);
    let r = root.to_path_buf();
    e.register_provider(move |init| {
        Box::new(pluginbuildfile::Provider::new(
            r,
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    // Not managed: it renders in-process and creates no sandbox.
    e.register_driver(|_| Box::new(heph::plugintemplate::Driver))?;
    let e = Arc::new(e);
    e.install_exec_runner_host();
    Ok(e)
}

fn write_build(root: &Path, pkg: &str, content: &str) {
    let dir = root.join(pkg);
    std::fs::create_dir_all(&dir).expect("create pkg dir");
    std::fs::write(dir.join("BUILD"), content).expect("write BUILD");
}

/// A bash target `name` running `run`, depending on `dep` when given.
fn target(name: &str, run: &str, dep: Option<&str>) -> String {
    let deps = dep.map_or_else(String::new, |d| format!("deps = {{\"d\": [\"{d}\"]}},"));
    format!(
        r#"
target(
    name = "{name}",
    driver = "bash",
    run = "{run}",
    out = "{name}.txt",
    {deps}
)
"#
    )
}

/// `//p:t0` … `//p:t{n-1}`, each depending on the one before, every one
/// running `run(i)`.
fn chain(n: usize, run: impl Fn(usize) -> String) -> String {
    (0..n)
        .map(|i| {
            let dep = (i > 0).then(|| format!("//p:t{}", i - 1));
            target(&format!("t{i}"), &run(i), dep.as_deref())
        })
        .collect()
}

/// Build `addrs` in one request, bounded by [`DEADLINE`]. `Err` names every
/// failed target.
async fn build(engine: &Arc<Engine>, addrs: &[String]) -> anyhow::Result<()> {
    let rs = engine.new_state();
    let res = build_in(engine, &rs, addrs).await;
    drain(rs).await;
    res
}

async fn build_in(
    engine: &Arc<Engine>,
    rs: &Arc<RequestState>,
    addrs: &[String],
) -> anyhow::Result<()> {
    let m = Matcher::Or(
        addrs
            .iter()
            .map(|a| parse_addr(a).map(Matcher::Addr))
            .collect::<Result<_, _>>()?,
    );
    let batch = tokio::time::timeout(
        DEADLINE,
        Arc::clone(engine).result(
            Arc::clone(rs),
            &m,
            OutputMatcher::All,
            &ResultOptions::default(),
        ),
    )
    .await
    .map_err(|_elapsed| anyhow::anyhow!("the build hung past {DEADLINE:?}"))?;
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

/// Wait for the request's background removals, bounded: drop the request,
/// then wait for its `bg_pending` to reach zero.
async fn drain(rs: Arc<RequestState>) {
    let bg = rs.bg_pending();
    drop(rs);
    let deadline = Instant::now() + DEADLINE;
    while bg.load(Ordering::Acquire) > 0 {
        assert!(Instant::now() < deadline, "the reclaim lane never drained");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The pool at this instant; `None` before the first admission built it.
fn snapshot(engine: &Engine) -> Option<ResourceSnapshot> {
    engine.resource_snapshot()
}

fn pool<'a>(snap: &'a ResourceSnapshot, name: &str) -> Option<&'a PoolSnapshot> {
    snap.main.pools.iter().find(|p| p.pool.as_str() == name)
}

/// What `pool` holds in `snap`, 0 when it is absent.
fn held(snap: &ResourceSnapshot, name: &str) -> u64 {
    pool(snap, name).map_or(0, |p| p.held)
}

/// Whether the head of the queue waits on a `sandbox` slot.
fn head_waits_on_sandbox(snap: &ResourceSnapshot) -> bool {
    snap.main
        .head
        .as_ref()
        .is_some_and(|h| h.unmet.iter().any(|n| n.pool.as_str() == PoolName::SANDBOX))
}

/// Wait until every cpu and sandbox slot is back and nothing is queued, or
/// fail naming what is still out.
async fn all_capacity_back(engine: &Engine) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let Some(s) = snapshot(engine) else {
            // Never built: nothing was ever admitted, so nothing is out.
            return;
        };
        if held(&s, PoolName::SANDBOX) == 0 && held(&s, PoolName::CPU) == 0 && s.main.queued == 0 {
            return;
        }
        assert!(Instant::now() < deadline, "capacity leaked: {s:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until the snapshot satisfies `cond`, or fail naming `what`.
async fn snapshot_until(engine: &Engine, what: &str, cond: impl Fn(&ResourceSnapshot) -> bool) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let s = snapshot(engine);
        if s.as_ref().is_some_and(&cond) {
            return;
        }
        assert!(Instant::now() < deadline, "never saw {what}: {s:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Build `//p:busy` in its own request and return once its run has ended and
/// its sandbox is queued for removal: no cpu held, the only `sandbox` slot
/// held by the pending removal. On a [`gated_engine`] of one slot, the pool
/// is exhausted until the gate opens. The request is returned so the caller
/// drains it, after opening the gate.
async fn exhaust_the_pool(engine: &Arc<Engine>) -> anyhow::Result<Arc<RequestState>> {
    let rs = engine.new_state();
    build_in(engine, &rs, &["//p:busy".to_owned()]).await?;
    snapshot_until(engine, "busy's sandbox pending removal", |s| {
        held(s, PoolName::SANDBOX) == 1 && held(s, PoolName::CPU) == 0
    })
    .await;
    Ok(rs)
}

/// Assert busy's slot is still out (its removal still owed) and nothing else
/// is held or queued.
fn assert_only_busy_pending(engine: &Engine, what: &str) {
    let s = snapshot(engine).expect("the pool is built");
    assert_eq!(
        (
            held(&s, PoolName::SANDBOX),
            held(&s, PoolName::CPU),
            s.main.queued
        ),
        (1, 0, 0),
        "{what}: only busy's removal is owed: {s:?}"
    );
}

/// The sandbox directories under `pkg`'s sandbox root right now.
fn sandboxes(engine: &Engine, pkg: &str) -> usize {
    let dir: PathBuf = engine.home.join("sandbox").join(pkg);
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("__target_"))
            .count()
    })
}

/// The ceiling itself. Twenty independent targets at `-j 4` behind a reclaim
/// lane slowed to 300ms a removal: without the pool, finished sandboxes pile
/// up faster than they are removed (the disk-filling `validate`). With two
/// slots, a watcher sampling the sandbox root every millisecond never sees
/// more than two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandboxes_on_disk_never_exceed_the_sandbox_pool() -> anyhow::Result<()> {
    const SLOTS: usize = 2;
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let names: Vec<String> = (0..20).map(|i| format!("s{i}")).collect();
    write_build(
        root,
        "p",
        &names
            .iter()
            .map(|n| target(n, &format!("echo {n} > $OUT"), None))
            .collect::<String>(),
    );
    let engine = engine(root, SLOTS, Duration::from_millis(300))?;

    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(0));
    let watcher = std::thread::spawn({
        let (engine, stop, peak) = (Arc::clone(&engine), Arc::clone(&stop), Arc::clone(&peak));
        move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(sandboxes(&engine, "p"), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });

    let addrs: Vec<String> = names.iter().map(|n| format!("//p:{n}")).collect();
    let built = build(&engine, &addrs).await;
    stop.store(true, Ordering::Relaxed);
    watcher.join().expect("watcher");
    built?;

    let peak = peak.load(Ordering::Relaxed);
    assert!(
        peak >= 1,
        "the watcher never saw a sandbox: the probe is blind"
    );
    assert!(
        peak <= SLOTS,
        "{peak} sandboxes on disk at once, with a sandbox pool of {SLOTS}"
    );
    assert_eq!(sandboxes(&engine, "p"), 0, "every sandbox is removed");
    all_capacity_back(&engine).await;
    Ok(())
}

/// One slot and a ten-deep chain: each target's slot is back (its sandbox
/// removed) before the next can run, so the chain completes only if no step
/// keeps one. A dep resolved while its dependant holds nothing is what makes
/// this finish at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_sandbox_pool_drains() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    write_build(root, "p", &chain(10, |i| format!("echo {i} > $OUT")));
    let engine = engine(root, 1, Duration::from_millis(20))?;

    build(&engine, &["//p:t9".to_owned()]).await?;
    all_capacity_back(&engine).await;
    assert_eq!(sandboxes(&engine, "p"), 0);
    Ok(())
}

/// The failure leg: the fifth link fails. Its sandbox is removed like any
/// other, its slot coming back with the removal: a target built afterwards
/// with the single slot still runs, and no sandbox is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_sandbox_pool_drains_after_a_failure() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    write_build(root, "p", &failing_chain());
    let engine = engine(root, 1, Duration::ZERO)?;

    let err = build(&engine, &["//p:t9".to_owned()])
        .await
        .expect_err("t5 fails");
    assert!(format!("{err:#}").contains("//p:t5"), "{err:#}");
    all_capacity_back(&engine).await;
    assert_eq!(sandboxes(&engine, "p"), 0, "the failed sandbox is removed");

    build(&engine, &["//p:after".to_owned()]).await?;
    all_capacity_back(&engine).await;
    Ok(())
}

/// The failure leg under `--keep-failed-sandboxes`: the failed sandbox stays,
/// outside the bound, and its slot comes back at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_sandbox_pool_drains_after_a_kept_failure() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    write_build(root, "p", &failing_chain());
    let engine = engine_configured(
        root,
        1,
        |cfg| cfg.keep_failed_sandboxes = true,
        |e| e.set_sandbox_removal_delay(Duration::ZERO),
    )?;

    let err = build(&engine, &["//p:t9".to_owned()])
        .await
        .expect_err("t5 fails");
    assert!(format!("{err:#}").contains("//p:t5"), "{err:#}");
    all_capacity_back(&engine).await;
    // The failed sandbox is kept, and only it.
    assert_eq!(sandboxes(&engine, "p"), 1);

    build(&engine, &["//p:after".to_owned()]).await?;
    all_capacity_back(&engine).await;
    Ok(())
}

/// A ten-deep chain whose fifth link fails, plus an independent `//p:after`.
fn failing_chain() -> String {
    let mut build_file = chain(10, |i| {
        if i == 5 {
            "echo failing; exit 1".to_owned()
        } else {
            format!("echo {i} > $OUT")
        }
    });
    build_file.push_str(&target("after", "echo after > $OUT", None));
    build_file
}

/// A failed run's sandbox counts toward the cap: with removals held, its slot
/// stays out after the run, pending the removal, and the next run waits on
/// it; once the removal runs, the slot is back and the sandbox gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_sandbox_holds_its_slot_until_removed() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let mut build_file = target("fail", "echo failing; exit 1", None);
    build_file.push_str(&target("after", "echo after > $OUT", None));
    write_build(root, "p", &build_file);
    let (engine, gate) = gated_engine(root, 1)?;

    let rs = engine.new_state();
    let err = build_in(&engine, &rs, &["//p:fail".to_owned()])
        .await
        .expect_err("fail fails");
    assert!(format!("{err:#}").contains("//p:fail"), "{err:#}");
    // The removal is queued before the execute returns, and held by the gate.
    let s = snapshot(&engine).expect("the pool is built");
    assert_eq!(
        (held(&s, PoolName::SANDBOX), held(&s, PoolName::CPU)),
        (1, 0),
        "the failed sandbox's slot is out until it is removed: {s:?}"
    );
    assert_eq!(sandboxes(&engine, "p"), 1, "not removed yet");

    // The next run waits on that slot.
    let after = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { build(&engine, &["//p:after".to_owned()]).await }
    });
    snapshot_until(
        &engine,
        "after waiting on the sandbox pool",
        head_waits_on_sandbox,
    )
    .await;

    gate.open();
    after.await??;
    drain(rs).await;
    all_capacity_back(&engine).await;
    assert_eq!(sandboxes(&engine, "p"), 0, "both sandboxes are removed");
    Ok(())
}

/// The cancellation leg: the request is cancelled while the fifth link runs
/// (it waits on a gate nobody opens). The dropped teardown queues the reclaim
/// with the slot in it, so it comes back, and a later build still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_sandbox_pool_drains_after_a_cancellation() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let gates = tempfile::tempdir()?;
    let started = gates.path().join("started");
    let gate = gates.path().join("never");
    let mut build_file = chain(10, |i| {
        if i == 5 {
            format!(
                "touch '{started}'; while [ ! -e '{gate}' ]; do sleep 0.02; done; echo 5 > $OUT",
                started = started.display(),
                gate = gate.display(),
            )
        } else {
            format!("echo {i} > $OUT")
        }
    });
    build_file.push_str(&target("after", "echo after > $OUT", None));
    write_build(root, "p", &build_file);
    let engine = engine(root, 1, Duration::ZERO)?;

    let rs = engine.new_state();
    let task = tokio::spawn({
        let (engine, rs) = (Arc::clone(&engine), Arc::clone(&rs));
        async move { build_in(&engine, &rs, &["//p:t9".to_owned()]).await }
    });
    let deadline = Instant::now() + DEADLINE;
    while !started.exists() {
        assert!(!task.is_finished(), "the chain ended before t5 ran");
        assert!(Instant::now() < deadline, "t5 never started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // t5 holds the only slot while it runs.
    let s = snapshot(&engine).expect("the pool is built");
    assert_eq!(held(&s, PoolName::SANDBOX), 1, "{s:?}");
    rs.ctoken().cancel();
    let ended = tokio::time::timeout(DEADLINE, task)
        .await
        .map_err(|_elapsed| anyhow::anyhow!("a cancelled build hung"))?;
    assert!(
        matches!(ended, Ok(Err(_))),
        "a cancelled build does not succeed"
    );
    drain(rs).await;
    all_capacity_back(&engine).await;
    // Whether the cancellation dropped t5's execute or exec reported it as an
    // error of its own, its sandbox is removed, not kept as a failure's.
    assert_eq!(
        sandboxes(&engine, "p"),
        0,
        "the cancelled sandbox is removed"
    );

    build(&engine, &["//p:after".to_owned()]).await?;
    all_capacity_back(&engine).await;
    Ok(())
}

/// A cache hit creates no sandbox, so it takes no slot: with the only one
/// pending removal behind a slow disk, a target already in the cache still
/// comes back at once. The target's run counts itself into a file outside
/// the sandbox, so "it was a hit" is observed, not assumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cache_hit_takes_no_sandbox_slot() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let runs = tmp.path().join("runs");
    write_build(
        root,
        "p",
        &format!(
            "{}{}",
            target(
                "cached",
                &format!("echo run >> '{}'; echo cached > $OUT", runs.display()),
                None
            ),
            target("busy", "echo busy > $OUT", None),
        ),
    );
    let (engine, gate) = gated_engine(root, 1)?;
    gate.open();
    build(&engine, &["//p:cached".to_owned()]).await?;
    all_capacity_back(&engine).await;
    gate.close();

    // The removal is held until the gate opens, so the hit finishing at all
    // proves it took no slot.
    let busy = exhaust_the_pool(&engine).await?;
    let rs = engine.new_state();
    build_in(&engine, &rs, &["//p:cached".to_owned()]).await?;
    assert_only_busy_pending(&engine, "after the hit");
    assert_eq!(
        std::fs::read_to_string(&runs)?.lines().count(),
        1,
        "the second build was a cache hit"
    );

    gate.open();
    drain(rs).await;
    drain(busy).await;
    all_capacity_back(&engine).await;
    Ok(())
}

/// A non-managed driver (`template`, rendered in-process) creates no
/// sandbox, so it takes no slot either: its first run completes while the
/// only slot is pending removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_managed_driver_takes_no_sandbox_slot() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("p"))?;
    std::fs::write(root.join("p/conf.j2"), "name: {{ name }}\n")?;
    write_build(
        root,
        "p",
        &format!(
            "{}{}",
            target("busy", "echo busy > $OUT", None),
            r#"
target(name = "conf", driver = "template", src = file("conf.j2"), out = "conf",
       vars = {"name": "x"})
"#,
        ),
    );
    let (engine, gate) = gated_engine(root, 1)?;

    // busy's removal is held until the gate opens: the template finishing at
    // all proves it took no slot.
    let busy = exhaust_the_pool(&engine).await?;
    let rs = engine.new_state();
    build_in(&engine, &rs, &["//p:conf".to_owned()]).await?;
    assert_only_busy_pending(&engine, "after the template");

    gate.open();
    drain(rs).await;
    drain(busy).await;
    all_capacity_back(&engine).await;
    Ok(())
}

/// An invocation (`heph run` of an entrypoint) runs in its own sandbox but
/// takes no admission, so no slot: it completes while the only slot is still
/// pending removal behind a slow disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invocation_takes_no_sandbox_slot() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let out = tmp.path().join("invoked");
    write_build(
        root,
        "p",
        &format!(
            "{}{}",
            target("busy", "echo busy > $OUT", None),
            r#"
target(name = "ep", driver = "bash", entrypoint = True,
       run = "echo invoked > \"$HEPH_TEST_OUT\"")
"#,
        ),
    );
    let (engine, gate) = gated_engine(root, 1)?;

    // busy's removal is held until the gate opens: the invocation finishing
    // at all proves it took no slot.
    let busy = exhaust_the_pool(&engine).await?;
    let req = InvokeRequest::new(
        parse_addr("//p:ep")?,
        root,
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HEPH_TEST_OUT".into(), out.as_os_str().to_owned()),
        ],
    );
    let outcome = tokio::time::timeout(
        DEADLINE,
        Arc::clone(&engine).invoke(engine.new_state(), req),
    )
    .await
    .map_err(|_elapsed| anyhow::anyhow!("the invocation hung"))??;
    match outcome {
        InvokeOutcome::Exited(exit) => assert_eq!(exit, hcore::process::Exit::Code(0)),
        other => panic!("the invocation did not exit cleanly: {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&out)?, "invoked\n");
    assert_only_busy_pending(&engine, "after the invocation");

    gate.open();
    drain(busy).await;
    all_capacity_back(&engine).await;
    Ok(())
}

/// Cancelled while queued for a slot: the run leaves the queue as a
/// cancellation — never a failure — without ever taking a slot, and the pool
/// is whole afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_cancelled_while_queued_on_the_sandbox_pool_is_a_cancellation() -> anyhow::Result<()>
{
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    write_build(
        root,
        "p",
        &format!(
            "{}{}",
            target("busy", "echo busy > $OUT", None),
            target("parked", "echo parked > $OUT", None),
        ),
    );
    let (engine, gate) = gated_engine(root, 1)?;

    let busy = exhaust_the_pool(&engine).await?;
    let rs = engine.new_state();
    let task = tokio::spawn({
        let (engine, rs) = (Arc::clone(&engine), Arc::clone(&rs));
        async move {
            let m = Matcher::Addr(parse_addr("//p:parked")?);
            Arc::clone(&engine)
                .result(rs, &m, OutputMatcher::All, &ResultOptions::default())
                .await
                .map(|batch| batch.errors.len())
        }
    });
    snapshot_until(&engine, "the run queued on the sandbox pool", |s| {
        s.main.queued == 1 && head_waits_on_sandbox(s)
    })
    .await;
    rs.ctoken().cancel();
    let ended = tokio::time::timeout(DEADLINE, task)
        .await
        .map_err(|_elapsed| anyhow::anyhow!("a cancelled wait hung"))??;

    // Out of the queue, holding nothing, while busy's removal is still held
    // by the gate: the run left without its slot ever coming back.
    assert_only_busy_pending(&engine, "after the cancelled wait");
    match ended {
        Err(e) => assert!(hplugin::error::is_cancelled(&e), "{e:#}"),
        Ok(errors) => assert!(errors > 0, "a cancelled run did not succeed"),
    }
    let failures = rs.take_failures();
    for f in &failures {
        assert!(
            hplugin::error::is_cancelled(&f.source),
            "{} recorded a failure, not a cancellation: {:#}",
            f.addr.format(),
            f.source
        );
    }

    gate.open();
    drain(rs).await;
    drain(busy).await;
    all_capacity_back(&engine).await;
    Ok(())
}

/// Strict FIFO, accepted on purpose (`docs/RESOURCES.md`): a managed run at
/// the head of the queue waiting for a `sandbox` slot holds back every run
/// behind it, even one that needs no slot and fits the free cpu. Here the
/// template target `conf` (one cpu, of four free) waits behind `parked`
/// until busy's removal gives the slot back. Pinned so that changing it is a
/// decision rather than an accident.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_waiting_on_sandbox_holds_back_the_runs_behind_it() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("p"))?;
    std::fs::write(root.join("p/conf.j2"), "name: {{ name }}\n")?;
    write_build(
        root,
        "p",
        &format!(
            "{}{}{}",
            target("busy", "echo busy > $OUT", None),
            target("parked", "echo parked > $OUT", None),
            r#"
target(name = "conf", driver = "template", src = file("conf.j2"), out = "conf",
       vars = {"name": "x"})
"#,
        ),
    );
    let (engine, gate) = gated_engine(root, 1)?;

    let busy = exhaust_the_pool(&engine).await?;
    let spawn_build = |addr: &str| {
        let (engine, addrs) = (Arc::clone(&engine), vec![addr.to_owned()]);
        tokio::spawn(async move { build(&engine, &addrs).await })
    };
    let parked = spawn_build("//p:parked");
    snapshot_until(&engine, "parked at the head, waiting on sandbox", |s| {
        s.main.queued == 1 && head_waits_on_sandbox(s)
    })
    .await;
    let conf = spawn_build("//p:conf");
    snapshot_until(&engine, "conf queued behind parked", |s| s.main.queued == 2).await;

    let s = snapshot(&engine).expect("the pool is built");
    assert_eq!(held(&s, PoolName::CPU), 0, "every cpu is free: {s:?}");
    assert!(
        head_waits_on_sandbox(&s),
        "the head waits on sandbox: {s:?}"
    );
    assert!(
        !conf.is_finished(),
        "conf needs no slot and fits the free cpu, and still waits"
    );

    gate.open();
    tokio::time::timeout(DEADLINE, parked).await???;
    tokio::time::timeout(DEADLINE, conf).await???;
    drain(busy).await;
    all_capacity_back(&engine).await;
    Ok(())
}

/// `sandbox: 0` in the config is "the run sets it", as for any pool: a
/// managed run that reaches admission fails at once with the generic
/// zero-capacity error rather than waiting for a slot that never comes, and
/// a non-managed target, which takes no slot, still builds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_sandbox_pool_fails_a_managed_run_at_once() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    std::fs::create_dir_all(root.join("p"))?;
    std::fs::write(root.join("p/conf.j2"), "name: {{ name }}\n")?;
    write_build(
        root,
        "p",
        &format!(
            "{}{}",
            target("managed", "echo managed > $OUT", None),
            r#"
target(name = "conf", driver = "template", src = file("conf.j2"), out = "conf",
       vars = {"name": "x"})
"#,
        ),
    );
    let engine = engine(root, 0, Duration::ZERO)?;

    let started = Instant::now();
    let err = build(&engine, &["//p:managed".to_owned()])
        .await
        .expect_err("a managed run with no sandbox capacity fails");
    let err = format!("{err:#}");
    assert!(
        err.contains(
            "//p:managed acquires sandbox, but pool sandbox has capacity 0 and must be set: \
             --resource sandbox=<n>"
        ),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "failed at admission, not after a wait: {:?}",
        started.elapsed()
    );
    build(&engine, &["//p:conf".to_owned()]).await?;
    Ok(())
}

/// A run waiting on `sandbox` names its cause: the `AdmissionWaitStart` it
/// emits carries `removals_pending`, here busy's removal held by the gate,
/// and the count is back to 0 once that removal returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandbox_wait_reports_the_removals_pending() -> anyhow::Result<()> {
    use heph::engine::event::{BuildEventKind, EventReceiver};
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    write_build(
        root,
        "p",
        &format!(
            "{}{}",
            target("busy", "echo busy > $OUT", None),
            target("parked", "echo parked > $OUT", None),
        ),
    );
    let gate = RemovalGate::closed();
    let engine = engine_with(root, 1, |e| {
        e.set_sandbox_removal_gate(&gate);
        e.set_admission_notice(Duration::from_millis(50));
    })?;

    let busy = exhaust_the_pool(&engine).await?;
    let pending = |e: &Engine| snapshot(e).map(|s| s.main.removals_pending);
    assert_eq!(pending(&engine), Some(1), "busy's removal is held");

    let (tx, mut rx): (_, EventReceiver) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let parked = tokio::spawn({
        let (engine, rs) = (Arc::clone(&engine), Arc::clone(&rs));
        async move { build_in(&engine, &rs, &["//p:parked".to_owned()]).await }
    });
    let start = loop {
        let ev = tokio::time::timeout(DEADLINE, rx.recv())
            .await
            .map_err(|_elapsed| anyhow::anyhow!("no AdmissionWaitStart for parked"))?
            .ok_or_else(|| anyhow::anyhow!("the event stream closed first"))?;
        if let BuildEventKind::AdmissionWaitStart {
            addr,
            pool,
            removals_pending,
            ..
        } = ev.kind
        {
            break (addr, pool, removals_pending);
        }
    };
    assert_eq!(
        start,
        ("//p:parked".to_owned(), "sandbox".to_owned(), Some(1)),
        "the wait names sandbox and the one removal owed"
    );

    gate.open();
    tokio::time::timeout(DEADLINE, parked).await???;
    drain(rs).await;
    drain(busy).await;
    all_capacity_back(&engine).await;
    assert_eq!(pending(&engine), Some(0), "every removal returned");
    Ok(())
}
