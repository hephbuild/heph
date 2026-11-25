//! Declared host paths over the real engine and BUILD files (spec "nix store
//! liveness", PR 1).
//!
//! A driver names the host paths its output depends on (`host_paths` on its
//! run response); the engine stores them with the cache entry and `stat`s each
//! one on every local hit. A path that is gone makes the entry stale: a miss
//! with its reason, never a failure, and never a hash change. The write check,
//! the adversarial path table and the stat error are unit-tested in
//! `crates/engine/src/engine/result.rs`; this file is the end-to-end layer.
//!
//! [`HostPaths`] wraps the bash driver and declares whatever the test puts in
//! its [`Knobs`], creating each path first the way a nix rebuild brings a
//! collected store path back. Deleting a path from the test's `TempDir` stands
//! in for a wiped store, so nothing here needs nix.

#![expect(
    clippy::panic_in_result_fn,
    reason = "restriction/style lints scoped to production code; tests are exempt"
)]

use crate::common::{artifact_string, drain};
use hcore::events::CacheOutcomeKind;
use hcore::hmemoizer::downcast_chain_ref;
use heph::engine::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use heph::engine::event::BuildEventKind;
use heph::engine::{
    Config, EResult, Engine, HomeDir, OutputMatcher, RemoteCacheDef, ResultOptions,
};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use hplugin::error::HashUnknownError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What every run of the [`HostPaths`] driver does, shared with the test.
#[derive(Default)]
struct Knobs {
    /// The host paths each run declares.
    paths: Mutex<Vec<PathBuf>>,
    /// Each run creates every declared path before declaring it.
    create: AtomicBool,
    /// Each run fails before it starts.
    fail: AtomicBool,
    /// Overrides the parsed target's `cache.remote`. The def hash is the bash
    /// driver's and does not see it, so the target keeps its `hashin`.
    remote: Mutex<Option<bool>>,
    /// Runs started, per target name.
    runs: Mutex<HashMap<String, usize>>,
}

impl Knobs {
    fn new(paths: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            paths: Mutex::new(paths),
            create: AtomicBool::new(true),
            ..Self::default()
        })
    }

    fn set_paths(&self, paths: Vec<PathBuf>) {
        *self.paths.lock().expect("paths") = paths;
    }

    fn set_remote(&self, remote: Option<bool>) {
        *self.remote.lock().expect("remote") = remote;
    }

    fn runs(&self, name: &str) -> usize {
        self.runs
            .lock()
            .expect("runs")
            .get(name)
            .copied()
            .unwrap_or(0)
    }
}

/// The bash driver, registered as `hostpaths`, whose runs declare the
/// [`Knobs`]' paths.
struct HostPaths {
    bash: pluginexec::Driver,
    knobs: Arc<Knobs>,
}

#[async_trait::async_trait]
impl ManagedDriver for HostPaths {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "hostpaths".to_string(),
        })
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        ManagedDriver::schema(&self.bash)
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        let mut res = ManagedDriver::parse(&self.bash, req, ctoken).await?;
        if let Some(remote) = *self.knobs.remote.lock().expect("remote") {
            res.target_def.cache.remote_enabled = remote;
        }
        Ok(res)
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        ManagedDriver::apply_transitive(&self.bash, req, ctoken).await
    }
    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        let name = req.request.target.addr.name.to_string();
        *self
            .knobs
            .runs
            .lock()
            .expect("runs")
            .entry(name)
            .or_default() += 1;
        if self.knobs.fail.load(Ordering::SeqCst) {
            anyhow::bail!("hostpaths: this run was told to fail");
        }
        let paths = self.knobs.paths.lock().expect("paths").clone();
        if self.knobs.create.load(Ordering::SeqCst) {
            for p in &paths {
                std::fs::write(p, b"")
                    .map_err(|e| anyhow::anyhow!("creating host path {}: {e}", p.display()))?;
            }
        }
        let host_paths = paths
            .iter()
            .map(|p| hplugin::driver::HostPath::from_path(p))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut res = ManagedDriver::run(&self.bash, req, ctoken).await?;
        res.host_paths = host_paths;
        Ok(res)
    }
}

/// An engine over `root` with the buildfile provider, `bash`, and `hostpaths`
/// driven by `knobs`; with a read+write `file://` remote at `remote` if given.
/// Each call is a fresh `heph` invocation over the same on-disk cache.
fn engine(root: &Path, knobs: &Arc<Knobs>, remote: Option<&Path>) -> anyhow::Result<Arc<Engine>> {
    let mut e = Engine::new(Config {
        remote_caches: remote
            .map(|dir| RemoteCacheDef {
                name: "shared".to_string(),
                uri: format!("file://{}", dir.display()),
                read: true,
                write: true,
                concurrency: 10,
                endpoint: None,
                region: None,
            })
            .into_iter()
            .collect(),
        ..Config::for_tests(root)
    })?;
    e.register_provider(|init| {
        Box::new(pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
            Arc::clone(&init.functions),
        ))
    })?;
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash().with_host_path()))?;
    let knobs = Arc::clone(knobs);
    e.register_managed_driver(move |_| {
        Box::new(HostPaths {
            bash: pluginexec::Driver::new_bash().with_host_path(),
            knobs,
        })
    })?;
    Ok(Arc::new(e))
}

fn write_pkg(root: &Path, build: &str) {
    std::fs::create_dir_all(root.join("pkg")).expect("mkdir pkg");
    std::fs::write(root.join("pkg").join("BUILD"), build).expect("write BUILD");
}

/// `//pkg:hp` declares host paths and writes a constant; `//pkg:dep` copies it.
const DETERMINISTIC: &str = r#"
target(name = "hp", driver = "hostpaths", run = "echo fixed > $OUT", out = "o.txt",
       cache = {"remote": False})
target(name = "dep", driver = "bash", deps = {"in": ["//pkg:hp"]},
       run = "cat $SRC_IN > $OUT", out = "d.txt")
"#;

/// As [`DETERMINISTIC`], but each run of `//pkg:hp` writes a new value.
const VARYING: &str = r#"
target(name = "hp", driver = "hostpaths", run = "echo $RANDOM$RANDOM$RANDOM > $OUT",
       out = "o.txt", cache = {"remote": False})
target(name = "dep", driver = "bash", deps = {"in": ["//pkg:hp"]},
       run = "cat $SRC_IN > $OUT", out = "d.txt")
"#;

/// One request's answer and every event it emitted.
struct Ran {
    res: anyhow::Result<Arc<EResult>>,
    events: Vec<BuildEventKind>,
}

impl Ran {
    fn ok(&self) -> &EResult {
        self.res
            .as_ref()
            .map_err(|e| format!("{e:#}"))
            .expect("the request succeeds")
    }

    fn err(&self) -> String {
        self.res
            .as_ref()
            .err()
            .map(|e| format!("{e:#}"))
            .expect("the request fails")
    }

    /// Every `CacheOutcome` for `addr`, with its reason.
    fn outcomes(&self, addr: &str) -> Vec<(CacheOutcomeKind, Option<String>)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                BuildEventKind::CacheOutcome {
                    addr: a,
                    outcome,
                    reason,
                } if a == addr => Some((*outcome, reason.clone())),
                _ => None,
            })
            .collect()
    }

    fn count(&self, pred: impl Fn(&BuildEventKind) -> bool) -> usize {
        self.events.iter().filter(|e| pred(e)).count()
    }

    fn stale_output_changed(&self, addr: &str) -> usize {
        self.count(|e| matches!(e, BuildEventKind::StaleOutputChanged { addr: a } if a == addr))
    }
}

/// `heph run <addr>` on one event-collecting request, its background work
/// drained. A failure carries the root causes the request recorded, as the
/// CLI renders them.
async fn run(engine: &Arc<Engine>, addr: &str) -> Ran {
    let addr = parse_addr(addr).expect("parse addr");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rs = engine.new_state_with_events(false, Some(tx));
    let res = Arc::clone(engine)
        .result_addr(
            rs.clone(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .map_err(|err| {
            let causes: Vec<String> = rs
                .take_failures()
                .iter()
                .map(|f| format!("{}: {:#}", f.addr.format(), f.source))
                .collect();
            if causes.is_empty() {
                err
            } else {
                anyhow::anyhow!("{err:#}\n{}", causes.join("\n"))
            }
        });
    drain(rs).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev.kind);
    }
    Ran { res, events }
}

/// The single hashout of a result.
fn hashout(r: &EResult) -> String {
    let hashouts: Vec<&str> = r
        .artifacts_meta
        .iter()
        .map(|m| m.hashout.as_str())
        .collect();
    assert_eq!(hashouts.len(), 1, "one output: {hashouts:?}");
    hashouts.concat()
}

async fn hashin(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<String> {
    let addr = parse_addr(addr)?;
    Ok(Arc::clone(engine)
        .meta(engine.new_state(), &addr)
        .await?
        .hashin)
}

async fn def_hash(engine: &Arc<Engine>, addr: &str) -> anyhow::Result<Vec<u8>> {
    let addr = parse_addr(addr)?;
    Ok(Arc::clone(engine)
        .get_def(engine.new_state(), &addr)
        .await?
        .target_def
        .hash
        .clone())
}

fn stale_reason(path: &Path) -> Option<String> {
    Some(format!("stale: host path {} is gone", path.display()))
}

/// Count the objects (files) under a `file://` cache directory.
fn remote_object_count(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                remote_object_count(&path)
            } else {
                1
            }
        })
        .sum()
}

/// Run, delete the path, run again: the driver runs twice, and the second
/// outcome is a `Miss` that says which path is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_path_gone_reruns() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let path = store.path().join("env");
    write_pkg(root.path(), DETERMINISTIC);
    let knobs = Knobs::new(vec![path.clone()]);
    let engine = engine(root.path(), &knobs, None)?;

    let first = run(&engine, "//pkg:hp").await;
    first.ok();
    assert_eq!(
        first.outcomes("//pkg:hp"),
        vec![(CacheOutcomeKind::Miss, None)]
    );
    // A held result rides a read lock on its entry, which the stale re-run
    // must replace under the write lock: holding it here would deadlock.
    drop(first);

    std::fs::remove_file(&path)?;
    let second = run(&engine, "//pkg:hp").await;
    assert_eq!(artifact_string(second.ok()), "fixed\n");
    assert_eq!(knobs.runs("hp"), 2, "the stale entry re-ran");
    assert_eq!(
        second.outcomes("//pkg:hp"),
        vec![(CacheOutcomeKind::Miss, stale_reason(&path))],
        "{:?}",
        second.events
    );
    assert_eq!(
        second.count(|e| matches!(e, BuildEventKind::LocalCacheHit { .. })),
        0,
        "a stale entry is never announced as a hit: {:?}",
        second.events
    );

    // The re-run's entry replaced the stale one, and the rebuilt path is live.
    let third = run(&engine, "//pkg:hp").await;
    third.ok();
    assert_eq!(knobs.runs("hp"), 2);
    assert_eq!(
        third.outcomes("//pkg:hp"),
        vec![(CacheOutcomeKind::LocalHit, None)]
    );
    Ok(())
}

/// The path is kept: one run, then a local hit, from a fresh engine too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_path_present_hits() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    write_pkg(root.path(), DETERMINISTIC);
    let knobs = Knobs::new(vec![store.path().join("env")]);

    run(&engine(root.path(), &knobs, None)?, "//pkg:hp")
        .await
        .ok();
    for engine in [
        engine(root.path(), &knobs, None)?,
        engine(root.path(), &knobs, None)?,
    ] {
        let warm = run(&engine, "//pkg:hp").await;
        warm.ok();
        assert_eq!(
            warm.outcomes("//pkg:hp"),
            vec![(CacheOutcomeKind::LocalHit, None)]
        );
    }
    assert_eq!(knobs.runs("hp"), 1, "a live entry is served");
    Ok(())
}

/// The driver's output differs on each run. A dependent built in the run that
/// rebuilds the stale entry folds the new `hashout` (I3): it re-runs, and it
/// copies the new bytes, never the stale entry's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_path_gone_dependent_hashout() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let path = store.path().join("env");
    write_pkg(root.path(), VARYING);
    let knobs = Knobs::new(vec![path.clone()]);
    let engine = engine(root.path(), &knobs, None)?;

    // Results are dropped at once: each rides a read lock on its entry, which
    // the stale re-run must replace under the write lock.
    let dep_before = artifact_string(run(&engine, "//pkg:dep").await.ok());
    let hp_hashout_before = hashout(run(&engine, "//pkg:hp").await.ok());
    let dep_hashin_before = hashin(&engine, "//pkg:dep").await?;

    std::fs::remove_file(&path)?;
    let second = run(&engine, "//pkg:dep").await;
    let dep_after = artifact_string(second.ok());
    assert_eq!(knobs.runs("hp"), 2, "the stale dependency re-ran");
    assert_eq!(
        second.outcomes("//pkg:hp"),
        vec![(CacheOutcomeKind::Miss, stale_reason(&path))]
    );
    assert_eq!(
        second.outcomes("//pkg:dep"),
        vec![(CacheOutcomeKind::Miss, None)],
        "the dependent's key moved with the new hashout: {:?}",
        second.events
    );

    let hp_after = run(&engine, "//pkg:hp").await;
    let hp_hashout_after = hashout(hp_after.ok());
    assert_ne!(hp_hashout_after, hp_hashout_before, "the output changed");
    assert_eq!(
        dep_after,
        artifact_string(hp_after.ok()),
        "the dependent copied the rebuilt bytes"
    );
    assert_ne!(dep_after, dep_before);
    assert_ne!(
        hashin(&engine, "//pkg:dep").await?,
        dep_hashin_before,
        "the dependent's hashin folds the new hashout"
    );
    Ok(())
}

/// Spec case 5 (an ephemeral /nix): the path goes, the deterministic driver
/// re-runs with the same output, the `hashout` is unchanged, and the
/// dependent stays a local hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_path_gone_dependent_stays_hit() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let path = store.path().join("env");
    write_pkg(root.path(), DETERMINISTIC);
    let knobs = Knobs::new(vec![path.clone()]);
    let engine = engine(root.path(), &knobs, None)?;

    run(&engine, "//pkg:dep").await.ok();
    let hp_hashout_before = hashout(run(&engine, "//pkg:hp").await.ok());
    let dep_hashin_before = hashin(&engine, "//pkg:dep").await?;

    // The store restarts empty; the heph home survives.
    std::fs::remove_file(&path)?;
    let engine = self::engine(root.path(), &knobs, None)?;
    let second = run(&engine, "//pkg:dep").await;
    assert_eq!(artifact_string(second.ok()), "fixed\n");
    assert_eq!(knobs.runs("hp"), 2, "the stale dependency re-ran");
    assert_eq!(
        second.outcomes("//pkg:hp"),
        vec![(CacheOutcomeKind::Miss, stale_reason(&path))]
    );
    assert_eq!(
        second.outcomes("//pkg:dep"),
        vec![(CacheOutcomeKind::LocalHit, None)],
        "an unchanged hashout keeps the dependent's key: {:?}",
        second.events
    );
    assert_eq!(
        second.stale_output_changed("//pkg:hp"),
        0,
        "an unchanged output is not reported as changed"
    );
    assert_eq!(
        hashout(run(&engine, "//pkg:hp").await.ok()),
        hp_hashout_before
    );
    assert_eq!(hashin(&engine, "//pkg:dep").await?, dep_hashin_before);
    Ok(())
}

/// A `hash_only` request on a stale entry answers "unknown" rather than serve
/// the entry or build: it runs nothing and leaves the entry in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_path_gone_hash_only() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let path = store.path().join("env");
    write_pkg(root.path(), DETERMINISTIC);
    let knobs = Knobs::new(vec![path.clone()]);
    let engine = engine(root.path(), &knobs, None)?;
    let hp = parse_addr("//pkg:hp")?;
    let dep = parse_addr("//pkg:dep")?;

    run(&engine, "//pkg:hp").await.ok();
    std::fs::remove_file(&path)?;

    // A hash-only request is a nested one: its parent is the addr whose
    // resolution reads `hp` (here `dep`), never `hp` itself — that would read
    // as a cycle `hp → hp`.
    let err = Arc::clone(&engine)
        .result_addr(
            engine.new_hash_only_state(dep.clone(), Default::default()),
            &hp,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .err()
        .expect("a hash-only request must not serve a stale entry");
    assert!(
        downcast_chain_ref::<HashUnknownError>(&err).is_some(),
        "expected HashUnknownError, got: {err:#}"
    );
    // Through a dependent's hash too: `meta` hashes `dep` from its inputs.
    let err = Arc::clone(&engine)
        .meta(
            engine.new_hash_only_state(dep.clone(), Default::default()),
            &dep,
        )
        .await
        .err()
        .expect("a hash-only hash must not fold a stale hashout");
    assert!(
        downcast_chain_ref::<HashUnknownError>(&err).is_some(),
        "expected HashUnknownError, got: {err:#}"
    );
    assert_eq!(knobs.runs("hp"), 1, "a hash-only request runs nothing");

    // The entry is still there: with the path back, it is served.
    std::fs::write(&path, b"")?;
    let warm = run(&engine, "//pkg:hp").await;
    warm.ok();
    assert_eq!(
        warm.outcomes("//pkg:hp"),
        vec![(CacheOutcomeKind::LocalHit, None)]
    );
    assert_eq!(knobs.runs("hp"), 1);
    Ok(())
}

/// Two requests on one stale entry run the driver once: the one that loses
/// the write lock finds the rebuilt entry under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_path_gone_concurrent_requests_run_once() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let path = store.path().join("env");
    write_pkg(
        root.path(),
        r#"
target(name = "hp", driver = "hostpaths", run = "sleep 0.5; echo $RANDOM$RANDOM$RANDOM > $OUT",
       out = "o.txt", cache = {"remote": False})
"#,
    );
    let knobs = Knobs::new(vec![path.clone()]);
    let engine = engine(root.path(), &knobs, None)?;

    run(&engine, "//pkg:hp").await.ok();
    std::fs::remove_file(&path)?;

    let spawn = || {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { run(&engine, "//pkg:hp").await })
    };
    let (a, b) = (spawn(), spawn());
    let (a, b) = (a.await?, b.await?);
    assert_eq!(knobs.runs("hp"), 2, "one re-run for both requests");
    assert_eq!(
        artifact_string(a.ok()),
        artifact_string(b.ok()),
        "both requests see the one rebuilt output"
    );
    let mut outcomes = [a.outcomes("//pkg:hp"), b.outcomes("//pkg:hp")];
    outcomes.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(
        outcomes,
        [
            vec![(CacheOutcomeKind::LocalHit, None)],
            vec![(CacheOutcomeKind::Miss, stale_reason(&path))],
        ],
        "one rebuilt the stale entry, the other was served what it wrote"
    );
    Ok(())
}

/// The re-run fails: the error surfaces, the stale entry is never served (its
/// manifest is gone after the first failed re-run), and the next run retries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_path_gone_rerun_fails() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let path = store.path().join("env");
    write_pkg(root.path(), DETERMINISTIC);
    let knobs = Knobs::new(vec![path.clone()]);
    let engine = engine(root.path(), &knobs, None)?;

    run(&engine, "//pkg:hp").await.ok();
    std::fs::remove_file(&path)?;

    knobs.fail.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let failed = run(&engine, "//pkg:dep").await;
        let msg = failed.err();
        assert!(msg.contains("this run was told to fail"), "{msg}");
        assert_eq!(
            failed.count(|e| matches!(
                e,
                BuildEventKind::LocalCacheHit { .. } | BuildEventKind::RemoteCacheHit { .. }
            )),
            0,
            "the stale entry is never served: {:?}",
            failed.events
        );
        assert!(
            failed
                .outcomes("//pkg:hp")
                .iter()
                .all(|(kind, _)| *kind == CacheOutcomeKind::Miss),
            "{:?}",
            failed.events
        );
    }
    assert_eq!(knobs.runs("hp"), 3, "each request retried the stale entry");

    knobs.fail.store(false, Ordering::SeqCst);
    let fixed = run(&engine, "//pkg:dep").await;
    assert_eq!(artifact_string(fixed.ok()), "fixed\n");
    assert_eq!(knobs.runs("hp"), 4, "the next run retried and rebuilt it");
    // The first failed re-run removed the stale manifest under the write lock
    // (I5), so later runs find no entry: a plain miss, not a stale one.
    assert_eq!(
        fixed.outcomes("//pkg:hp"),
        vec![(CacheOutcomeKind::Miss, None)]
    );
    Ok(())
}

/// A re-run whose `hashout` differs from the stale entry's emits
/// `StaleOutputChanged`, once, for that target only. The `info` line is
/// asserted by the engine's `rerun_hashout_change_logs_info`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_path_gone_hashout_changed_logs() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let path = store.path().join("env");
    write_pkg(root.path(), VARYING);
    let knobs = Knobs::new(vec![path.clone()]);
    let engine = engine(root.path(), &knobs, None)?;

    let first = run(&engine, "//pkg:dep").await;
    first.ok();
    assert_eq!(first.stale_output_changed("//pkg:hp"), 0, "a cold run");

    std::fs::remove_file(&path)?;
    let second = run(&engine, "//pkg:dep").await;
    second.ok();
    assert_eq!(knobs.runs("hp"), 2);
    assert_eq!(
        second.stale_output_changed("//pkg:hp"),
        1,
        "{:?}",
        second.events
    );
    assert_eq!(
        second.count(|e| matches!(e, BuildEventKind::StaleOutputChanged { .. })),
        1,
        "only the rebuilt stale entry is reported: {:?}",
        second.events
    );
    Ok(())
}

/// I6: a target whose `cache.remote` is on and whose driver declares host
/// paths fails, naming the address and the paths; nothing is cached locally
/// or remotely. The same with and without a configured remote (Decision 14).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_paths_with_remote_upload_fails() -> anyhow::Result<()> {
    for configured in [true, false] {
        let root = tempfile::tempdir()?;
        let store = tempfile::tempdir()?;
        let remote = tempfile::tempdir()?;
        let path = store.path().join("env");
        // The default `cache`: on, remote on.
        write_pkg(
            root.path(),
            r#"target(name = "hp", driver = "hostpaths", run = "echo fixed > $OUT", out = "o.txt")"#,
        );
        let knobs = Knobs::new(vec![path.clone()]);
        let remote_dir = configured.then_some(remote.path());
        let engine = engine(root.path(), &knobs, remote_dir)?;

        let failed = run(&engine, "//pkg:hp").await;
        let msg = failed.err();
        assert!(msg.contains("//pkg:hp"), "configured={configured}: {msg}");
        assert!(
            msg.contains(&format!(
                "declares host paths ({}) with `cache.remote` on",
                path.display()
            )),
            "configured={configured}: {msg}"
        );
        assert_eq!(
            remote_object_count(remote.path()),
            0,
            "configured={configured}: the remote received nothing"
        );

        // Nothing was cached: the same target, now declaring nothing, runs.
        knobs.set_paths(vec![]);
        let engine = self::engine(root.path(), &knobs, remote_dir)?;
        let next = run(&engine, "//pkg:hp").await;
        next.ok();
        assert_eq!(
            knobs.runs("hp"),
            2,
            "configured={configured}: the refused run left no entry to hit"
        );
        assert_eq!(
            next.outcomes("//pkg:hp"),
            vec![(CacheOutcomeKind::Miss, None)],
            "configured={configured}"
        );
    }
    Ok(())
}

/// I6: with the remote on and a list-free entry for the same key on the
/// remote, a stale local entry re-runs the driver instead of pulling.
///
/// A target that declares paths cannot have `cache.remote` on (I6), so the
/// key is set up in three invocations of one `hashin`: the driver flips the
/// parsed `cache.remote` without touching the def hash.
///   1. remote on, no paths: the run uploads a list-free entry.
///   2. local cache wiped, remote off, one path: a local entry with the path.
///   3. the path deleted, remote on, no paths: the local entry is stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_entry_skips_remote_pull() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let remote_dir = tempfile::tempdir()?;
    let path = store.path().join("env");
    write_pkg(
        root.path(),
        r#"target(name = "hp", driver = "hostpaths", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "o.txt")"#,
    );
    let knobs = Knobs::new(vec![]);
    let remote = Some(remote_dir.path());

    knobs.set_remote(Some(true));
    let e1 = engine(root.path(), &knobs, remote)?;
    let uploaded = artifact_string(run(&e1, "//pkg:hp").await.ok());
    let key = hashin(&e1, "//pkg:hp").await?;
    drop(e1);
    assert!(
        remote_object_count(remote_dir.path()) > 0,
        "the list-free entry is on the remote"
    );

    std::fs::remove_dir_all(HomeDir::for_tests(root.path()).join("cache"))?;
    knobs.set_remote(Some(false));
    knobs.set_paths(vec![path.clone()]);
    let e2 = engine(root.path(), &knobs, remote)?;
    run(&e2, "//pkg:hp").await.ok();
    assert_eq!(hashin(&e2, "//pkg:hp").await?, key, "one key throughout");
    drop(e2);
    assert_eq!(knobs.runs("hp"), 2);

    std::fs::remove_file(&path)?;
    knobs.set_remote(Some(true));
    knobs.set_paths(vec![]);
    let e3 = engine(root.path(), &knobs, remote)?;
    assert_eq!(hashin(&e3, "//pkg:hp").await?, key, "one key throughout");
    let third = run(&e3, "//pkg:hp").await;
    let rebuilt = artifact_string(third.ok());
    assert_eq!(knobs.runs("hp"), 3, "the stale entry re-ran the driver");
    assert_eq!(
        third.outcomes("//pkg:hp"),
        vec![(CacheOutcomeKind::Miss, stale_reason(&path))],
        "{:?}",
        third.events
    );
    assert_eq!(
        third.count(|e| matches!(
            e,
            BuildEventKind::RemoteCacheHit { .. } | BuildEventKind::RemoteCacheMiss { .. }
        )),
        0,
        "the remote was not consulted: {:?}",
        third.events
    );
    assert_ne!(
        rebuilt, uploaded,
        "the output is a fresh run's, not the remote's"
    );
    Ok(())
}

/// I1: the def hash, `hashin` and `hashout` are the same across runs that
/// declare different lists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_paths_do_not_move_hashes() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    let [a, b, c] = ["a", "b", "c"].map(|n| store.path().join(n));
    write_pkg(root.path(), DETERMINISTIC);
    let knobs = Knobs::new(vec![a.clone()]);
    let engine = engine(root.path(), &knobs, None)?;

    let mut seen = Vec::new();
    // Each list after the first is declared by the re-run of a stale entry.
    for (gone, next) in [
        (None, None),
        (Some(&a), Some(vec![b.clone(), c.clone()])),
        (Some(&b), Some(vec![])),
    ] {
        if let Some(gone) = gone {
            std::fs::remove_file(gone)?;
        }
        if let Some(next) = next {
            knobs.set_paths(next);
        }
        let ran = run(&engine, "//pkg:hp").await;
        seen.push((
            def_hash(&engine, "//pkg:hp").await?,
            hashin(&engine, "//pkg:hp").await?,
            hashout(ran.ok()),
        ));
    }
    assert_eq!(knobs.runs("hp"), 3, "each list was declared by a run");
    let (first, rest) = seen.split_first().expect("three runs");
    for other in rest {
        assert_eq!(other, first, "host paths moved a hash");
    }
    Ok(())
}

/// A target with caching off that declares host paths ignores the list, even
/// one naming a path that does not exist, and succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_paths_on_uncacheable_target() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let store = tempfile::tempdir()?;
    write_pkg(
        root.path(),
        r#"target(name = "hp", driver = "hostpaths", run = "echo fixed > $OUT", out = "o.txt",
       cache = False)"#,
    );
    let knobs = Knobs::new(vec![store.path().join("missing")]);
    knobs.create.store(false, Ordering::SeqCst);
    let engine = engine(root.path(), &knobs, None)?;

    for n in 1..=2 {
        let ran = run(&engine, "//pkg:hp").await;
        assert_eq!(artifact_string(ran.ok()), "fixed\n");
        assert_eq!(knobs.runs("hp"), n, "an uncacheable target runs every time");
        assert!(ran.outcomes("//pkg:hp").is_empty(), "{:?}", ran.events);
    }
    Ok(())
}
