//! I8: memory grows with the number of failures, never with log size or
//! graph depth. `failure_memory_slope_bounded` reads peak RSS (`ru_maxrss`)
//! at N and 4N failures, with small and large logs, and bounds the slope.
//!
//! A separate binary, not a module of `tests/e2e`: `ru_maxrss` is a process
//! high-water mark, so each point runs in a process of its own — this binary,
//! re-executed on `failure_memory_point` with the point in
//! [`POINT_ENV`] — and nothing else may run in that process.

mod common;

use common::failure_load::{FailureLoad, peak_rss_bytes};

/// Names the point a re-executed child measures: `<failures>,<log bytes>`.
const POINT_ENV: &str = "HEPH_FAILURE_MEMORY_POINT";
/// What a child prints its reading after, on stdout.
const READING: &str = "heph-failure-memory-peak-rss=";

/// Targets in every point: the failures replace passing targets, so the
/// slope is per failure, not per target.
const TARGETS: usize = 1000;
/// N; the second point is 4N.
const N: usize = 64;
/// The small and the large log, 4x apart.
const SMALL_LOG: usize = 1024 * 1024;
const LARGE_LOG: usize = 4 * 1024 * 1024;
/// I8's bound: peak RSS per failure.
const SLOPE_MAX: f64 = 64.0 * 1024.0;
/// How far apart the two logs' slopes may be, as a share of [`SLOPE_MAX`].
const SLOPE_SPREAD: f64 = 0.10;

fn load(failures: usize, log_bytes: usize) -> FailureLoad {
    FailureLoad {
        targets: TARGETS,
        failures,
        log_bytes,
        chains: 4,
        depth: 50,
        fail: true,
    }
}

/// One point, in a child process: the peak RSS of a run of
/// `load(failures, log_bytes)` that still holds its failures.
fn measure(failures: usize, log_bytes: usize) -> u64 {
    let exe = std::env::current_exe().expect("this test binary");
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "failure_memory_point",
            "--ignored",
            "--nocapture",
        ])
        .env(POINT_ENV, format!("{failures},{log_bytes}"))
        .output()
        .expect("re-exec this binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the point ({failures} failures, {log_bytes} B logs) failed: {}\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let reading = stdout
        .lines()
        .find_map(|l| l.strip_prefix(READING))
        .and_then(|v| v.trim().parse().ok());
    assert!(reading.is_some(), "no reading from the point:\n{stdout}");
    reading.unwrap_or_default()
}

/// The peak RSS each extra failure costs, from N to 4N, in bytes.
fn slope(log_bytes: usize) -> (f64, u64, u64) {
    let at_n = measure(N, log_bytes);
    let at_4n = measure(4 * N, log_bytes);
    #[expect(
        clippy::cast_precision_loss,
        reason = "a few GiB fit an f64 exactly enough"
    )]
    let per = (at_4n as f64 - at_n as f64) / (3 * N) as f64;
    (per, at_n, at_4n)
}

/// I8: peak RSS grows by at most 64 KiB per failure from N to 4N failures,
/// and by the same with 4x the log (within a tenth of the bound: two slopes
/// near zero are too close to noise for a ratio).
#[test]
fn failure_memory_slope_bounded() {
    let (small, small_n, small_4n) = slope(SMALL_LOG);
    let (large, large_n, large_4n) = slope(LARGE_LOG);
    let mib = |b: u64| b / (1024 * 1024);
    eprintln!(
        "failure_memory_slope_bounded: {TARGETS} targets, {N} -> {} failures\n  \
         {} MiB logs: {} -> {} MiB, {:.1} KiB per failure\n  \
         {} MiB logs: {} -> {} MiB, {:.1} KiB per failure",
        4 * N,
        SMALL_LOG / (1024 * 1024),
        mib(small_n),
        mib(small_4n),
        small / 1024.0,
        LARGE_LOG / (1024 * 1024),
        mib(large_n),
        mib(large_4n),
        large / 1024.0,
    );
    assert!(
        small <= SLOPE_MAX,
        "{:.1} KiB per failure with 1 MiB logs",
        small / 1024.0
    );
    assert!(
        large <= SLOPE_MAX,
        "{:.1} KiB per failure with 4 MiB logs",
        large / 1024.0
    );
    assert!(
        (large - small).abs() <= SLOPE_MAX * SLOPE_SPREAD,
        "the slope moved with the log: {:.1} KiB (1 MiB) vs {:.1} KiB (4 MiB) per failure",
        small / 1024.0,
        large / 1024.0
    );
}

/// The child: one point of [`failure_memory_slope_bounded`]. Ignored, so a
/// plain run of this binary skips it; only the parent's re-exec runs it.
#[test]
#[ignore = "a re-executed child of failure_memory_slope_bounded"]
fn failure_memory_point() {
    let Ok(point) = std::env::var(POINT_ENV) else {
        return;
    };
    let (failures, log_bytes) = point
        .split_once(',')
        .and_then(|(f, l)| Some((f.parse().ok()?, l.parse().ok()?)))
        .unwrap_or_else(|| panic!("{POINT_ENV}={point}"));
    let load = load(failures, log_bytes);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let ws = load.workspace();
    let (rs, held) = rt.block_on(load.run(&ws.engine));
    // Read while the request and its failures are still held.
    let peak = peak_rss_bytes();
    println!("{READING}{peak}");
    drop((rs, held));
}
