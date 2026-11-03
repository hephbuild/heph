#![expect(
    clippy::panic_in_result_fn,
    clippy::panic,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

//! `heph.core.resource(name)` end to end (D14): the value is the host's capacity,
//! whatever this run's pools are, and a target that declares the whole of it
//! is clamped to the run's `cpu` pool like any oversize declaration.

use hcore::capacity::Capacity;
use hcore::htvalue::Value;
use heph::engine::event::{BuildEvent, BuildEventKind};
use heph::engine::{Config, Engine, OutputMatcher, ResultOptions};
use heph::htaddr::parse_addr;
use heph::htmatcher::Matcher;
use heph::{pluginbuildfile, pluginexec};
use hplugin::provider::PoolName;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const MICROS: u64 = 1_000_000;

/// An engine over `root` with the buildfile provider and the bash driver.
/// `jobs` is what `-j N` resolves to (`--resource cpu=N`), and also sizes the
/// engine's own `parallelism`, so neither knob can leak into the value.
fn engine(root: &Path, jobs: Option<u64>) -> anyhow::Result<Arc<Engine>> {
    let resources: BTreeMap<PoolName, Capacity> = jobs
        .map(|n| (PoolName::cpu(), Capacity::Amount(n * MICROS)))
        .into_iter()
        .collect();
    let mut e = Engine::new(Config {
        resources,
        parallelism: jobs.map(|n| usize::try_from(n).expect("small")),
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

/// The host's CPUs as `heph.core.resource("cpu")` reports them, in whole cores.
fn host_cpus() -> u64 {
    u64::try_from(hcore::sysres::host_cpus()).expect("core count fits u64")
}

/// `config[key]` as a whole number (the BUILD bridge hands an int over 32 bits
/// across as a float).
fn whole(config: &std::collections::HashMap<String, Value>, key: &str) -> u64 {
    match config.get(key) {
        Some(Value::Int(i)) => u64::try_from(*i).expect("non-negative"),
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test"
        )]
        Some(Value::Float(f)) if f.fract() == 0.0 && *f >= 0.0 => *f as u64,
        other => panic!("{key}: expected a whole number, got {other:?}"),
    }
}

/// `-j 2` (and a 2-wide engine) leaves both values at the host's numbers:
/// they are the machine, not the run, so a BUILD file reading them evaluates
/// the same whatever the run's flags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_resource_is_unchanged_by_jobs() -> anyhow::Result<()> {
    let build = r#"
target(
    name = "t",
    driver = "bash",
    run = "true",
    cpu = heph.core.resource("cpu"),
    memory = heph.core.resource("memory"),
)
"#;
    for jobs in [None, Some(2)] {
        let ws = tempfile::tempdir()?;
        write_build(ws.path(), "p", build);
        let engine = engine(ws.path(), jobs)?;
        let spec = Arc::clone(&engine)
            .get_spec(engine.new_state(), &parse_addr("//p:t")?)
            .await?;
        assert_eq!(
            whole(&spec.config, "cpu"),
            host_cpus(),
            "cpu under {jobs:?}"
        );
        assert_eq!(
            whole(&spec.config, "memory"),
            hcore::sysres::host_memory_bytes(),
            "memory under {jobs:?}"
        );
    }
    Ok(())
}

/// `resources = {"cpu": heph.core.resource("cpu")}` declares the host's
/// width and, under `-j 2`, reserves the whole 2-cpu pool: the declaration is
/// the machine's, the grant is the run's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_from_core_resource_clamps_under_j() -> anyhow::Result<()> {
    let ws = tempfile::tempdir()?;
    write_build(
        ws.path(),
        "p",
        r#"
target(
    name = "wide",
    driver = "bash",
    run = "echo ok > $OUT",
    out = "wide.txt",
    resources = {"cpu": heph.core.resource("cpu")},
)
"#,
    );
    let engine = engine(ws.path(), Some(2))?;
    let events = build_events(&engine, "//p:wide").await?;
    let (declared, reserved) = events
        .iter()
        .find_map(|e| match &e.kind {
            BuildEventKind::ExecuteStart {
                declared, reserved, ..
            } => Some((declared.clone(), reserved.clone())),
            _ => None,
        })
        .expect("the target ran");
    let cpu = |m: Option<BTreeMap<String, u64>>| m.and_then(|m| m.get("cpu").copied());
    assert_eq!(
        cpu(declared),
        Some(host_cpus() * MICROS),
        "declares the host"
    );
    assert_eq!(
        cpu(reserved),
        Some(host_cpus().min(2) * MICROS),
        "reserves the run's pool"
    );
    Ok(())
}

/// Build `addr`, collecting every event the request emitted.
async fn build_events(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<Vec<BuildEvent>> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let batch = Arc::clone(engine)
        .result(
            rs.clone(),
            &Matcher::Addr(parse_addr(addr)?),
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await?;
    if let Some((a, e)) = batch.errors.first() {
        anyhow::bail!("{}: {e:#}", a.format());
    }
    if let Some(f) = rs.take_failures().into_iter().next() {
        anyhow::bail!("{}: {:#}", f.addr.format(), f.source);
    }
    drop(rs);
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    Ok(events)
}
