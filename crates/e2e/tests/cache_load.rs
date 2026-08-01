//! A load harness for the cache paths, measuring **runtime liveness** rather
//! than throughput.
//!
//! `#[ignore]`d: this is an instrument, not a test. It asserts only enough to
//! know it measured the right thing, it writes gigabytes, and its output is
//! numbers a human reads — none of which belongs in `tst` on every push. Run it
//! deliberately:
//!
//! ```text
//! HEPH_LT_TARGETS=100 HEPH_LT_SIZE_KB=1024 HEPH_LT_WORKERS=10 \
//!   cargo test --release -p e2e --test cache_load -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Always `--release`; a debug build measures rustc, not heph.
//!
//! # Why a heartbeat and not a profiler
//!
//! The failure this exists to catch is a tokio worker blocking on a sync cache
//! call — parking on the write-behind queue's condvar, waiting out a sqlite
//! checkout, or doing byte-proportional work inline. A parked worker burns no
//! CPU, so `samply` and friends show nothing at all; the symptom is that the
//! *runtime* stops making progress. Enough workers park and the reactor, the
//! timer wheel, in-flight transfers and the TUI stall together, and the build
//! looks hung with nothing actually deadlocked.
//!
//! So the instrument is a heartbeat: a spawned task that sleeps 1ms in a loop
//! and records the gap between consecutive wake-ups. A gap far above 1ms means
//! no worker was free to drive the timer. That is a direct measurement of the
//! thing that matters and it needs no profiler.
//!
//! # Reading the output
//!
//! One `METRICS {...}` JSON line per run, with a summary per phase.
//!
//! **Use `over_10ms` and `max_us`, not `stall_ms`.** The empty-workload floor is
//! a ~2.4ms median gap (tokio's timer granularity for a 1ms sleep) and *zero*
//! gaps over 10ms, so anything above 10ms is signal. `stall_ms` sums the excess
//! over a 2ms allowance across the whole phase, which is easy to misread: a
//! handful of long freezes in a short phase drives it close to the phase's own
//! wall-clock and looks like continuous starvation when the median gap is in
//! fact perfectly healthy. It is there for magnitude, not for diagnosis.
//!
//! # Comparing two commits
//!
//! **Interleave the trees — never run all of A then all of B.** On a normal box
//! the page cache and disk state move enough between runs to invent a
//! difference that is not there: measuring this harness across #201 produced a
//! confident "the second tree is 40% slower" that vanished entirely once the
//! runs alternated, in both directions depending on which tree went second. The
//! same tree with identical parameters varied 3x between sessions. Alternate
//! A/B/A/B, discard the first rep of each as page-cache warm-up, compare
//! medians, and treat anything smaller than the observed spread as noise.
//!
//! # What it does not tell you
//!
//! A heartbeat measures worker *availability*, never the reason for it. A long
//! gap says the runtime could not run a ready task; it does not say which call
//! site was responsible, and it cannot separate "parked on a lock" from "busy
//! doing CPU work". Use it to detect and to size a regression — then reach for
//! the diag facilities (stall reports, the parked-futures dump) to name the
//! call site. Inferring the culprit from these numbers alone is guesswork.
#![expect(
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_sign_loss,
    clippy::let_underscore_must_use,
    reason = "restriction/style lints scoped to production code; this harness is \
              entirely test code, and its plain helper fns sit outside the \
              `allow-*-in-tests` clippy.toml exemptions"
)]

use heph::engine::{Config, Engine, OutputMatcher, RemoteCacheDef, ResultOptions};
use heph::htaddr::parse_addr;
use heph::{pluginbuildfile, pluginexec};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Gaps at or under this are a healthy runtime — it is roughly tokio's timer
/// granularity for a 1ms sleep, and the empty-workload floor sits here.
const HEALTHY_GAP_US: u64 = 2_000;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Dropping the remote layer isolates the local write path: the cold phase then
/// executes and writes the local cache but never uploads. The warm phase has no
/// meaning without a remote and is skipped.
fn no_remote() -> bool {
    std::env::var_os("HEPH_LT_NO_REMOTE").is_some()
}

fn build_engine(root: &Path, remote_uri: &str, parallelism: Option<usize>) -> Arc<Engine> {
    let remote_caches = if no_remote() {
        Vec::new()
    } else {
        vec![RemoteCacheDef {
            name: "shared".to_string(),
            uri: remote_uri.to_string(),
            read: true,
            write: true,
            concurrency: 10,
        }]
    };
    let mut e = Engine::new(Config {
        root: root.to_path_buf(),
        home_dir: PathBuf::new(),
        parallelism,
        remote_caches,
        ..Default::default()
    })
    .expect("engine");
    e.register_provider(|init| {
        Box::new(pluginbuildfile::Provider::new(
            init.root.to_path_buf(),
            init.runtime.clone(),
        ))
    })
    .expect("register buildfile provider");
    e.register_managed_driver(|_| Box::new(pluginexec::Driver::new_bash()))
        .expect("register bash driver");
    Arc::new(e)
}

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

/// Records the gap between consecutive 1ms wake-ups, for the phase it brackets.
struct Heartbeat {
    stop: Arc<AtomicBool>,
    gaps_us: Arc<Mutex<Vec<u64>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Heartbeat {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let gaps_us = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::spawn({
            let stop = stop.clone();
            let gaps_us = gaps_us.clone();
            async move {
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    let now = Instant::now();
                    gaps_us
                        .lock()
                        .expect("heartbeat gaps mutex poisoned")
                        .push(now.duration_since(last).as_micros() as u64);
                    last = now;
                }
            }
        });
        Self {
            stop,
            gaps_us,
            handle,
        }
    }

    async fn stop(self) -> Vec<u64> {
        self.stop.store(true, Ordering::Relaxed);
        // The heartbeat is the very thing a stalled runtime cannot poll, so this
        // join is also the last chance for a pending wake-up to land.
        let _ = self.handle.await;
        let gaps = self.gaps_us.lock().expect("heartbeat gaps mutex poisoned");
        gaps.clone()
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

/// Summarize one phase's gaps as a JSON object. See the module docs on which
/// fields to trust.
fn summarize(gaps: &[u64]) -> String {
    let mut sorted = gaps.to_vec();
    sorted.sort_unstable();
    let over = |t: u64| gaps.iter().filter(|g| **g > t).count();
    let stall_ms: u64 = gaps
        .iter()
        .filter(|g| **g > HEALTHY_GAP_US)
        .map(|g| g - HEALTHY_GAP_US)
        .sum::<u64>()
        / 1000;
    format!(
        "{{\"samples\":{},\"p50_us\":{},\"p99_us\":{},\"max_us\":{},\
\"over_10ms\":{},\"over_100ms\":{},\"over_1s\":{},\"stall_ms\":{stall_ms}}}",
        gaps.len(),
        pct(&sorted, 0.50),
        pct(&sorted, 0.99),
        sorted.last().copied().unwrap_or(0),
        over(10_000),
        over(100_000),
        over(1_000_000),
    )
}

/// Read just the `stamp` output. Never the blob — that would pull
/// `targets * size_kb` into memory to compare a dozen bytes.
fn stamp_of(result: &heph::engine::EResult) -> String {
    use heph::hartifactcontent::WalkEntryKind;
    use std::io::Read as _;
    for artifact in &result.artifacts {
        for entry in artifact.walk().expect("walk artifacts") {
            let entry = entry.expect("artifact entry");
            if entry.path.file_name().and_then(|n| n.to_str()) != Some("stamp.txt") {
                continue;
            }
            if let WalkEntryKind::File { mut data, .. } = entry.kind {
                let mut s = String::new();
                data.read_to_string(&mut s).expect("read stamp");
                return s;
            }
        }
    }
    panic!("result has no stamp.txt");
}

/// Resolve every target concurrently, then drain background uploads the way the
/// CLI does on exit.
async fn run_all(engine: &Arc<Engine>, n: usize) -> Vec<String> {
    let rs = engine.new_state();
    let tasks: Vec<_> = (0..n)
        .map(|i| {
            let engine = engine.clone();
            let rs = rs.clone();
            tokio::spawn(async move {
                let addr = parse_addr(&format!("//pkg{i}:t")).expect("parse addr");
                let result = engine
                    .result_addr(rs, &addr, OutputMatcher::All, &ResultOptions::default())
                    .await
                    .expect("run target");
                stamp_of(&result)
            })
        })
        .collect();

    let mut stamps = Vec::with_capacity(n);
    for t in tasks {
        stamps.push(t.await.expect("join target task"));
    }

    // Release the request before waiting on its counter: some background work is
    // only submitted when the request state drops, so a waiter holding `rs` alive
    // waits on a counter that cannot reach zero.
    let bg = rs.bg_pending();
    drop(rs);
    let deadline = Instant::now() + Duration::from_secs(300);
    while bg.load(Ordering::Acquire) > 0 {
        assert!(Instant::now() < deadline, "background upload never drained");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stamps
}

/// Write `n` single-target packages, each producing a `bytes`-sized blob and a
/// small random stamp.
///
/// The stamp is `$RANDOM`: a re-execution produces a different one, so stamp
/// equality across phases is what proves the warm phase was served from cache.
/// The blob is sized by the caller to sit under the 8 MiB spill threshold (so it
/// stays in sqlite rather than becoming a plain file) and over the 16 KiB
/// mem-tier cap (so it is not served out of memory) — the band where the durable
/// cache actually does the work.
fn write_workspace(root: &Path, n: usize, bytes: usize) {
    for i in 0..n {
        let dir = root.join(format!("pkg{i}"));
        std::fs::create_dir_all(&dir).expect("mkdir package");
        std::fs::write(
            dir.join("BUILD"),
            format!(
                r#"target(
    name = "t",
    driver = "bash",
    run = "echo $RANDOM$RANDOM$RANDOM > $OUT_STAMP; head -c {bytes} /dev/urandom > $OUT_BLOB",
    out = {{"stamp": ["stamp.txt"], "blob": ["blob.bin"]}},
)
"#
            ),
        )
        .expect("write BUILD");
    }
}

/// Cold (execute + local write + upload) then warm (pull everything back from
/// the remote), heartbeating both.
///
/// Cold is heartbeated as an in-run companion, not a clean baseline: it moves
/// the same bytes through the same write-behind queue, but it is also running
/// every target's subprocess, so a stall there is not attributable to the cache
/// on its own. `HEPH_LT_NO_REMOTE=1` plus a low target count is the way to pull
/// those apart.
#[test]
#[ignore = "load harness: writes GBs and reports numbers; run it deliberately, see module docs"]
fn cache_runtime_liveness() {
    let targets = env_usize("HEPH_LT_TARGETS", 100);
    let size_kb = env_usize("HEPH_LT_SIZE_KB", 1024);
    let workers = env_usize("HEPH_LT_WORKERS", 0);
    let parallelism = env_usize("HEPH_LT_PARALLELISM", 0);
    let label = std::env::var("HEPH_LT_LABEL").unwrap_or_else(|_| "unlabelled".to_string());

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if workers > 0 {
        builder.worker_threads(workers);
    }
    let rt = builder.build().expect("build runtime");

    rt.block_on(async move {
        let root = tempfile::tempdir().expect("root tempdir");
        let remote = tempfile::tempdir().expect("remote tempdir");
        let remote_uri = format!("file://{}", remote.path().display());
        let parallelism = (parallelism > 0).then_some(parallelism);

        write_workspace(root.path(), targets, size_kb * 1024);

        // ---- COLD: execute everything, populate local (+ remote). ----
        let engine = build_engine(root.path(), &remote_uri, parallelism);
        let hb = Heartbeat::start();
        // Let the heartbeat settle before the load starts.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let t0 = Instant::now();
        let cold_stamps = run_all(&engine, targets).await;
        let cold_ms = t0.elapsed().as_millis();
        let cold_gaps = hb.stop().await;
        drop(engine);

        let emit = |warm_ms: u128, warm_gaps: &[u64]| {
            println!(
                "METRICS {{\"label\":\"{label}\",\"targets\":{targets},\"size_kb\":{size_kb},\
\"tokio_workers\":{workers},\"parallelism\":{},\"no_remote\":{},\
\"cold_ms\":{cold_ms},\"warm_ms\":{warm_ms},\"cold\":{},\"warm\":{}}}",
                parallelism.unwrap_or(0),
                no_remote(),
                summarize(&cold_gaps),
                summarize(warm_gaps),
            );
        };

        if no_remote() {
            emit(0, &[]);
            return;
        }

        let remote_after_cold = remote_object_count(remote.path());
        assert!(
            remote_after_cold > 0,
            "cold phase populated no remote objects"
        );

        // A fresh engine over an erased local cache: every target must come back
        // over the remote.
        std::fs::remove_dir_all(root.path().join(".heph3").join("cache"))
            .expect("delete local cache");

        // ---- WARM: pull everything back from the remote. ----
        let engine = build_engine(root.path(), &remote_uri, parallelism);
        let hb = Heartbeat::start();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let t0 = Instant::now();
        let warm_stamps = run_all(&engine, targets).await;
        let warm_ms = t0.elapsed().as_millis();
        let warm_gaps = hb.stop().await;
        drop(engine);

        // Both checks, or the numbers describe a rebuild rather than a cache
        // read — which would make every measurement above meaningless.
        assert_eq!(
            warm_stamps, cold_stamps,
            "warm phase re-executed instead of reading the cache"
        );
        assert_eq!(
            remote_object_count(remote.path()),
            remote_after_cold,
            "warm phase uploaded, so it re-executed"
        );

        emit(warm_ms, &warm_gaps);
    });
}
