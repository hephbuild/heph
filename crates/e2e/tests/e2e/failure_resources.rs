//! I8: what a run holds per failure, read from this process around a
//! failure load (`common::failure_load`). The memory slope is
//! `tests/failure_memory.rs`, a binary of its own: `ru_maxrss` is a process
//! high-water mark, so each point needs a process.
//!
//! The fd count is of the whole process, so `failure_fd_count_flat` reads
//! true only with nothing else running in it: nextest's process per test
//! (CI), or a filter that selects it alone.

use crate::common::failure_load::{
    FailureLoad, FdSampler, cache_dir, fd_paths_added, open_fd_paths_outside, open_fds,
    peak_rss_bytes,
};

/// I8: what the open-fd maximum during a failing run may exceed the green
/// baseline's by.
const FD_SLACK: usize = 16;

/// I8: a failure holds no open file. The most fds open while 200 failures
/// (two of them under 50-deep chains) run and are recorded is within
/// [`FD_SLACK`] of the same load passing, and what the request holds once
/// they are recorded is within it of what the green run held.
///
/// Neither count includes the local cache's files (`<home>/cache/`). Deciding
/// a failure's `log:` row (`Engine::log_readable`) lists the target's cache
/// entries, so a failing run reads `cache.db` where a green run of
/// `cache = False` targets never does. Its read pool opens a connection per
/// concurrent read (a `cache.db` and a `cache.db-wal` fd each, plus one
/// `-shm`) and keeps it for the engine's life, up to the pool's fixed size:
/// how many of those reads overlapped, not how many failed. Measured on this
/// load: 9 to 31 cache fds after 200 failures (4 to 15 connections, from one
/// run to the next), one connection after a single failure; outside the
/// cache, the same 12 fds as the green run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failure_fd_count_flat() {
    let load = FailureLoad {
        targets: 400,
        failures: 200,
        log_bytes: 128 * 1024,
        chains: 2,
        depth: 50,
        fail: true,
    };
    let green = FailureLoad {
        fail: false,
        ..load
    };

    let ws = green.workspace();
    let cache = cache_dir(&ws);
    let root = cache
        .parent()
        .and_then(|p| p.parent())
        .expect("<ws>/.heph/cache");
    let sampler = FdSampler::start_outside(cache.clone());
    let (rs, _) = green.run(&ws.engine).await;
    let green_max = sampler.max();
    // The engine and the request still alive, as below: what an engine
    // keeps open after a run (its locks) is not a failure's.
    let green_held = open_fd_paths_outside(&cache, root);
    drop((rs, ws));

    let ws = load.workspace();
    let cache = cache_dir(&ws);
    let root = cache
        .parent()
        .and_then(|p| p.parent())
        .expect("<ws>/.heph/cache");
    let sampler = FdSampler::start_outside(cache.clone());
    let (rs, failures) = load.run(&ws.engine).await;
    let failing_max = sampler.max();
    let held = open_fd_paths_outside(&cache, root);
    let in_cache = open_fds().saturating_sub(held.len());
    let (green_n, held_n) = (green_held.len(), held.len());
    eprintln!(
        "failure_fd_count_flat: during the run: green max {green_max}, failing max {failing_max}; \
         after it: green {green_n}, holding {} failures {held_n} (and {in_cache} in the cache)",
        failures.len()
    );
    assert!(
        failing_max <= green_max + FD_SLACK,
        "fds open during the failing run: {failing_max}, green baseline {green_max}"
    );
    assert!(
        held_n <= green_n + FD_SLACK,
        "{} recorded failures hold {held_n} fds, the green run {green_n}; held besides: {:#?}",
        failures.len(),
        fd_paths_added(&green_held, &held),
    );
    drop((rs, failures));
}

/// The I8 instrument at full scale, for perf-measurement: 20k targets, 5k
/// failures, 10 MB logs, 200-deep chains. Prints peak RSS and the most fds
/// open; asserts nothing. Each figure can be scaled down for a local run
/// (`HEPH_FL_TARGETS`, `HEPH_FL_FAILURES`, `HEPH_FL_LOG_MB`, `HEPH_FL_DEPTH`,
/// `HEPH_FL_CHAINS`): at full scale it writes ~50 GB of logs.
///
/// ```text
/// cargo test --release -p e2e --test e2e failure_resources::failure_load_20k -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "an instrument: tens of GB of logs, numbers a human reads"]
async fn failure_load_20k() {
    let env = |name: &str, default: usize| -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let load = FailureLoad {
        targets: env("HEPH_FL_TARGETS", 20_000),
        failures: env("HEPH_FL_FAILURES", 5_000),
        log_bytes: env("HEPH_FL_LOG_MB", 10) * 1024 * 1024,
        chains: env("HEPH_FL_CHAINS", 10),
        depth: env("HEPH_FL_DEPTH", 200),
        fail: true,
    };
    let ws = load.workspace();
    let before_rss = peak_rss_bytes();
    let before_fds = open_fds();
    let started = std::time::Instant::now();
    let sampler = FdSampler::start();
    let (rs, failures) = load.run(&ws.engine).await;
    let took = started.elapsed();
    // The claim writes land in the background; their drain is its own
    // figure (FQ-1: their slot exclusions were once O(n²) to look up).
    let drain_started = std::time::Instant::now();
    ws.engine.failure_slots().writes_drained().await;
    let drain = drain_started.elapsed();
    let max_fds = sampler.max();
    let peak = peak_rss_bytes();
    eprintln!(
        "failure_load_20k: {load:?}\n  took {took:?}\n  slot writes drained in {drain:?} more\n  failures {}\n  peak rss {} MiB (before the run {} MiB)\n  \
         fds: before {before_fds}, max during {max_fds}, held after {}",
        failures.len(),
        peak / (1024 * 1024),
        before_rss / (1024 * 1024),
        open_fds(),
    );
    drop((rs, failures));
}
