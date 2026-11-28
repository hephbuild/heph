//! I8: memory grows with the number of failures, never with log size or
//! graph depth. `failure_memory_slope_bounded` reads the heap a run holds,
//! and the most it held at once, at N and 4N failures, with small and large
//! logs, and bounds both slopes.
//!
//! The heap is counted by this binary's own allocator ([`Counting`]), not
//! read from `ru_maxrss`. A peak RSS is a high-water mark of pages, not of
//! what is kept: darwin's malloc keeps freed large blocks resident, so on
//! the macOS runners the 4 MiB-log slope read 68 KiB per failure on one run
//! and below zero on the next, from the same tree. The allocator counts
//! what is allocated and not yet freed, exactly, on every platform.
//!
//! A separate binary, not a module of `tests/e2e`: the counting allocator is
//! the whole binary's, and each point runs in a process of its own — this
//! binary, re-executed on `failure_memory_point` with the point in
//! [`POINT_ENV`] — so nothing else runs while it counts.

mod common;

use common::failure_load::{FailureLoad, peak_rss_bytes};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Heap bytes allocated and not yet freed, process-wide.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The most [`LIVE`] has been since it was last reset.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// `System`, counting [`LIVE`] and [`PEAK`].
struct Counting;

impl Counting {
    fn grew(by: usize) {
        let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }

    fn shrank(by: usize) {
        LIVE.fetch_sub(by, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to `System`, which upholds the `GlobalAlloc`
// contract; the wrapper only bumps two atomics, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded unchanged from the caller.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            Self::grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is forwarded unchanged from the caller.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            Self::grew(layout.size());
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr`, `layout` and `new_size` are forwarded unchanged.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                Self::grew(new_size - layout.size());
            } else {
                Self::shrank(layout.size() - new_size);
            }
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` are forwarded unchanged from the caller.
        unsafe { System.dealloc(ptr, layout) };
        Self::shrank(layout.size());
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Names the point a re-executed child measures: `<failures>,<log bytes>`.
const POINT_ENV: &str = "HEPH_FAILURE_MEMORY_POINT";
/// What a child prints its reading after, on stdout:
/// `<held>,<peak heap>,<peak rss>`, in bytes.
const READING: &str = "heph-failure-memory=";

/// Targets in every point: the failures replace passing targets, so the
/// slope is per failure, not per target.
const TARGETS: usize = 1000;
/// N; the second point is 4N.
const N: usize = 64;
/// The small and the large log, 4x apart.
const SMALL_LOG: usize = 1024 * 1024;
const LARGE_LOG: usize = 4 * 1024 * 1024;
/// I8's bound: heap per failure, held after the run and at the peak.
const SLOPE_MAX: f64 = 64.0 * 1024.0;
/// How far apart the two logs' slopes may be, as a share of [`SLOPE_MAX`]
/// (I8's 10%): 6.4 KiB, which a run that kept even 0.25% of each log
/// would cross (1 MiB vs 4 MiB logs).
const SLOPE_SPREAD: f64 = 0.1;
/// Targets each point runs at once. The peak heap is the held heap plus
/// what the running targets have in flight, so it is only comparable
/// between points at one fixed parallelism.
const PARALLELISM: usize = 2;

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

/// One point's reading, in bytes.
#[derive(Debug, Clone, Copy)]
struct Reading {
    /// The heap the run holds once it is over, with its request and its
    /// failures still alive.
    held: u64,
    /// The most heap the process held at once, from the workspace's start.
    peak: u64,
    /// `ru_maxrss`, printed for comparison and not bounded.
    rss: u64,
}

/// One point, in a child process.
fn measure(failures: usize, log_bytes: usize) -> Reading {
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
        .and_then(|v| {
            let mut v = v.trim().split(',').map(|n| n.parse().ok());
            Some(Reading {
                held: v.next()??,
                peak: v.next()??,
                rss: v.next()??,
            })
        });
    assert!(reading.is_some(), "no reading from the point:\n{stdout}");
    reading.unwrap_or(Reading {
        held: 0,
        peak: 0,
        rss: 0,
    })
}

/// What each extra failure costs from N to 4N, in bytes.
#[expect(
    clippy::cast_precision_loss,
    reason = "a few GiB fit an f64 exactly enough"
)]
fn per_failure(at_n: u64, at_4n: u64) -> f64 {
    (at_4n as f64 - at_n as f64) / (3 * N) as f64
}

/// I8: the heap a run holds, and the most it holds at once, grow by at most
/// 64 KiB per failure from N to 4N failures, and by the same with 4x the
/// log (within [`SLOPE_SPREAD`] of the bound: two slopes near zero are too
/// close for a ratio).
#[test]
fn failure_memory_slope_bounded() {
    let points = [SMALL_LOG, LARGE_LOG].map(|log| (log, measure(N, log), measure(4 * N, log)));
    let kib = |b: f64| b / 1024.0;
    let mib = |b: u64| b / (1024 * 1024);
    let mut report = format!(
        "failure_memory_slope_bounded: {TARGETS} targets, {N} -> {} failures",
        4 * N
    );
    for (log, n, n4) in &points {
        report.push_str(&format!(
            "\n  {} MiB logs: held {:.1} KiB per failure ({} -> {} MiB), \
             peak heap {:.1} KiB per failure ({} -> {} MiB), peak rss {} -> {} MiB",
            log / (1024 * 1024),
            kib(per_failure(n.held, n4.held)),
            mib(n.held),
            mib(n4.held),
            kib(per_failure(n.peak, n4.peak)),
            mib(n.peak),
            mib(n4.peak),
            mib(n.rss),
            mib(n4.rss),
        ));
    }
    eprintln!("{report}");
    for (what, of) in [
        ("held", (|r: &Reading| r.held) as fn(&Reading) -> u64),
        ("peak heap", |r: &Reading| r.peak),
    ] {
        let [small, large] = points.map(|(_, n, n4)| per_failure(of(&n), of(&n4)));
        for (slope, log) in [(small, "1 MiB"), (large, "4 MiB")] {
            assert!(
                slope <= SLOPE_MAX,
                "{what}: {:.1} KiB per failure with {log} logs\n{report}",
                kib(slope)
            );
        }
        assert!(
            (large - small).abs() <= SLOPE_MAX * SLOPE_SPREAD,
            "{what}: the slope moved with the log: {:.1} KiB (1 MiB) vs {:.1} KiB (4 MiB) per failure\n{report}",
            kib(small),
            kib(large)
        );
    }
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
    // The engine spawns on the runtime from its constructor to its drop.
    let _in_rt = rt.enter();
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let ws = load.workspace_with_parallelism(Some(PARALLELISM));
    let (rs, held) = rt.block_on(async {
        let run = load.run(&ws.engine).await;
        // The slot writes land in the background: what is held is read
        // once they have.
        ws.engine.failure_slots().writes_drained().await;
        run
    });
    // Read while the request and its failures are still alive.
    let (live, peak) = (LIVE.load(Ordering::Relaxed), PEAK.load(Ordering::Relaxed));
    println!("{READING}{live},{peak},{}", peak_rss_bytes());
    drop((rs, held, ws));
}
