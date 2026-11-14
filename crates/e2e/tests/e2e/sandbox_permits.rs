#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! The sandbox permit end to end: a managed run takes a permit before its
//! sandbox exists and gives it back only once the reclaim lane has removed the
//! sandbox, so the sandboxes on disk — live plus pending removal — never
//! outnumber the permits (`Config::sandbox_permits`).
//!
//! A slow disk is stood in by `Engine::set_sandbox_removal_delay`, which makes
//! every removal that releases a permit sleep first: removal falls behind
//! creation, which is the condition the permit exists for.

use heph::engine::request_state::RequestState;
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::{pluginbuildfile, pluginexec};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Bound on every build here: a leaked permit parks the next run forever, and
/// a hang must fail the test rather than the CI job.
const DEADLINE: Duration = Duration::from_secs(120);

/// An engine over `root` with the buildfile provider, the managed bash driver,
/// `permits` sandbox permits and 4 cpus.
fn engine(root: &Path, permits: usize) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config {
        parallelism: Some(4),
        sandbox_permits: Some(permits),
        ..Config::for_tests(root.to_path_buf())
    })?;
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

/// Wait until every permit is back, or fail naming what is still out.
async fn all_permits_back(engine: &Engine) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        let g = engine.sandbox_permit_gauge();
        if g.held == 0 && g.pending_removal == 0 && g.waiting == 0 {
            return;
        }
        assert!(Instant::now() < deadline, "a permit leaked: {g:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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
/// lane slowed to 300ms a removal: without the permit, finished sandboxes pile
/// up faster than they are removed (the disk-filling `validate`). With two
/// permits, a watcher sampling the sandbox root every millisecond never sees
/// more than two.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandboxes_on_disk_never_exceed_the_permit_count() -> anyhow::Result<()> {
    const PERMITS: usize = 2;
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
    let engine = engine(root, PERMITS)?;
    engine.set_sandbox_removal_delay(Duration::from_millis(300));

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
        peak <= PERMITS,
        "{peak} sandboxes on disk at once, with {PERMITS} permits"
    );
    assert_eq!(sandboxes(&engine, "p"), 0, "every sandbox is removed");
    all_permits_back(&engine).await;
    Ok(())
}

/// One permit and a ten-deep chain: each target's permit is back (its sandbox
/// removed) before the next can run, so the chain completes only if no step
/// keeps one. A dep resolved while its dependant holds nothing is what makes
/// this finish at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_permit_pool_drains() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    write_build(root, "p", &chain(10, |i| format!("echo {i} > $OUT")));
    let engine = engine(root, 1)?;
    engine.set_sandbox_removal_delay(Duration::from_millis(20));

    build(&engine, &["//p:t9".to_owned()]).await?;
    all_permits_back(&engine).await;
    assert_eq!(sandboxes(&engine, "p"), 0);
    Ok(())
}

/// The failure leg: the fifth link fails. Its sandbox stays for the
/// diagnostic, outside the bound, and its permit comes back at once: a target
/// built afterwards with the single permit still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_permit_pool_drains_after_a_failure() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    let mut build_file = chain(10, |i| {
        if i == 5 {
            "echo failing; exit 1".to_owned()
        } else {
            format!("echo {i} > $OUT")
        }
    });
    build_file.push_str(&target("after", "echo after > $OUT", None));
    write_build(root, "p", &build_file);
    let engine = engine(root, 1)?;

    let err = build(&engine, &["//p:t9".to_owned()])
        .await
        .expect_err("t5 fails");
    assert!(format!("{err:#}").contains("//p:t5"), "{err:#}");
    all_permits_back(&engine).await;
    // The failed sandbox is kept, and only it.
    assert_eq!(sandboxes(&engine, "p"), 1);

    build(&engine, &["//p:after".to_owned()]).await?;
    all_permits_back(&engine).await;
    Ok(())
}

/// The cancellation leg: the request is cancelled while the fifth link runs
/// (it waits on a gate nobody opens). The dropped teardown queues the reclaim
/// with the permit in it, so it comes back, and a later build still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_permit_pool_drains_after_a_cancellation() -> anyhow::Result<()> {
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
    let engine = engine(root, 1)?;

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
    // t5 holds the only permit while it runs.
    assert_eq!(engine.sandbox_permit_gauge().held, 1);
    rs.ctoken().cancel();
    let ended = tokio::time::timeout(DEADLINE, task)
        .await
        .map_err(|_elapsed| anyhow::anyhow!("a cancelled build hung"))?;
    assert!(
        matches!(ended, Ok(Err(_))),
        "a cancelled build does not succeed"
    );
    drain(rs).await;
    all_permits_back(&engine).await;

    build(&engine, &["//p:after".to_owned()]).await?;
    all_permits_back(&engine).await;
    Ok(())
}
