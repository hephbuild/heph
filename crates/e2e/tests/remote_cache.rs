//! End-to-end remote cache test over a `file://` backend, exercising the full
//! cold → warm → hot lifecycle with *separate engines over the same root*:
//!
//! - **cold**: empty everything. The target executes; its artifacts are written
//!   to the local cache and pushed to the remote (background task).
//! - **warm**: a fresh engine (empty in-memory cache) with the on-disk local
//!   cache deleted — a genuine local miss. The revision is pulled from the
//!   remote into the local cache instead of re-executing.
//! - **hot**: the same warm engine again — now a fully local cache hit.
//!
//! The target's output is a random value, so a re-execution would produce a
//! *different* output. An output that stays identical across runs therefore
//! proves the result came from a cache, not a fresh execution.

use heph::engine::{Config, Engine, OutputMatcher, RemoteCacheDef, ResultOptions};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use htestkit::artifact_string;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Build a fresh engine rooted at `root`, wired to a single read+write remote
/// cache at `remote_uri`. Each call gets its own empty in-memory cache tier.
fn build_engine(root: &Path, remote_uri: &str) -> Arc<Engine> {
    let mut e = Engine::new(Config {
        root: root.to_path_buf(),
        home_dir: std::path::PathBuf::new(),
        remote_caches: vec![RemoteCacheDef {
            name: "shared".to_string(),
            uri: remote_uri.to_string(),
            read: true,
            write: true,
            concurrency: 10,
        }],
        ..Default::default()
    })
    .expect("engine");
    e.register_provider(|init| Box::new(pluginbuildfile::Provider::new(init.root.to_path_buf())))
        .expect("register buildfile provider");
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash()))
        .expect("register bash driver");
    Arc::new(e)
}

/// Run a target and return its artifact string, after draining any background
/// remote upload (mirrors the CLI/TUI, which blocks exit until `bg_pending`
/// reaches zero). Without this the upload could still be in flight when the
/// caller inspects the remote.
async fn run_drain(engine: &Arc<Engine>, addr: &str) -> String {
    let addr = parse_addr(addr).expect("parse addr");
    let rs = engine.new_state();
    let result = engine
        .clone()
        .result_addr(
            rs.clone(),
            &addr,
            OutputMatcher::All,
            &ResultOptions::default(),
        )
        .await
        .expect("run target");

    let bg = rs.bg_pending();
    let deadline = Instant::now() + Duration::from_secs(10);
    while bg.load(Ordering::Acquire) > 0 {
        assert!(Instant::now() < deadline, "background upload never drained");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    artifact_string(&result)
}

/// Count the objects (files) under a `file://` cache directory.
fn remote_object_count(dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += remote_object_count(&path);
        } else {
            count += 1;
        }
    }
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_cache_cold_warm_hot() {
    let root = tempfile::tempdir().expect("root tempdir");
    let remote = tempfile::tempdir().expect("remote tempdir");
    let remote_uri = format!("file://{}", remote.path().display());

    // The output is a random value, so re-executing the target yields a
    // *different* output. An unchanged output across runs ⇒ it came from a cache.
    std::fs::create_dir_all(root.path().join("pkg")).expect("mkdir pkg");
    std::fs::write(
        root.path().join("pkg").join("BUILD"),
        r#"target(name = "t", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")"#,
    )
    .expect("write BUILD");

    // ---- COLD: empty everything → execute, write local + remote. ----
    let cold = build_engine(root.path(), &remote_uri);
    let out_cold = run_drain(&cold, "//pkg:t").await;
    assert!(!out_cold.trim().is_empty(), "cold run produced no output");
    assert!(
        remote_object_count(remote.path()) > 0,
        "cold run must populate the remote cache"
    );
    // Drop the cold engine (and its in-memory cache tier) before the warm run.
    drop(cold);

    // Delete the on-disk local cache so the warm run cannot hit it locally.
    std::fs::remove_dir_all(root.path().join(".heph3").join("cache")).expect("rm local cache");

    // ---- WARM: fresh engine (empty mem) + no local cache → pull from remote. ----
    // A re-execution would produce a new random value, so equality with the cold
    // output proves the artifacts were served from the remote cache.
    let warm = build_engine(root.path(), &remote_uri);
    let out_warm = run_drain(&warm, "//pkg:t").await;
    assert_eq!(
        out_warm, out_cold,
        "warm run must serve the remote-cached output, not re-execute"
    );

    // ---- HOT: same engine again → fully local cache hit. ----
    let out_hot = run_drain(&warm, "//pkg:t").await;
    assert_eq!(
        out_hot, out_cold,
        "hot run must serve the local-cached output"
    );
}

/// Companion check: with **no** remote configured, deleting the local cache and
/// re-running *does* re-execute (different output). This guards against the
/// cold→warh→hot test passing for the wrong reason (e.g. a stray reproducible
/// output that would match even without a cache).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_is_nondeterministic_without_cache() {
    let root = tempfile::tempdir().expect("root tempdir");
    std::fs::create_dir_all(root.path().join("pkg")).expect("mkdir pkg");
    std::fs::write(
        root.path().join("pkg").join("BUILD"),
        r#"target(name = "t", driver = "bash", run = "echo $RANDOM$RANDOM$RANDOM > $OUT", out = "out.txt")"#,
    )
    .expect("write BUILD");

    // No `caches:` → no remote.
    let build_plain = |root: &Path| -> Arc<Engine> {
        let mut e = Engine::new(Config {
            root: root.to_path_buf(),
            home_dir: std::path::PathBuf::new(),
            ..Default::default()
        })
        .expect("engine");
        e.register_provider(|init| {
            Box::new(pluginbuildfile::Provider::new(init.root.to_path_buf()))
        })
        .expect("provider");
        e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash()))
            .expect("driver");
        Arc::new(e)
    };

    let e1 = build_plain(root.path());
    let first = run_drain(&e1, "//pkg:t").await;
    drop(e1);
    std::fs::remove_dir_all(root.path().join(".heph3").join("cache")).expect("rm local cache");

    let e2 = build_plain(root.path());
    let second = run_drain(&e2, "//pkg:t").await;

    assert_ne!(
        first, second,
        "without a cache, a fresh run must re-execute and produce a different output"
    );
}
