//! Isolated stress reproducer for docs/RCA_MACOS_WAKER.md.
//!
//! Hypothesis under test: tokio's cross-thread waker (mio::Waker =
//! kqueue EVFILT_USER on macOS) can drop wake-ups under heavy load, stranding
//! any task awaiting a wake that originates off-runtime.
//!
//! Exercises the RCA's confirmed-affected primitives concurrently:
//!   A. `spawn_blocking` JoinHandle awaits (storm of short jobs)
//!   B. `oneshot::Receiver::await` with the sender fired from std threads
//!   C. `tokio::process::Child` waits (SIGCHLD reaper path, "possibly affected")
//!   D. task-spawn churn (many short-lived tasks), mirroring the field load
//!
//! Every loop stamps a per-slot watermark after each completed await. A std
//! watchdog thread (kernel wakes only — no tokio involvement) flags any slot
//! whose watermark stalls past STALL_SECS while the process is otherwise live,
//! then exits 2. Clean completion of the full run exits 0.
//!
//! A stall here = repro. No stall proves nothing (the RCA never had an isolated
//! reproducer either) but is evidence the current tokio + current macOS does
//! not exhibit the failure under this shape and duration.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STALL_SECS: u64 = 20;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

struct Slot {
    name: &'static str,
    idx: usize,
    last: AtomicU64,
    iters: AtomicU64,
}

impl Slot {
    fn new(name: &'static str, idx: usize) -> Arc<Self> {
        Arc::new(Self {
            name,
            idx,
            last: AtomicU64::new(now_ms()),
            iters: AtomicU64::new(0),
        })
    }
    fn stamp(&self) {
        self.last.store(now_ms(), Ordering::Relaxed);
        self.iters.fetch_add(1, Ordering::Relaxed);
    }
}

fn main() {
    let run_secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(540);
    let ncpu = std::thread::available_parallelism().unwrap().get();
    // Mirror the field shape: workers = ncpu, and also run a second config in
    // the writeup with few workers (CI shape). Controlled via env.
    let workers: usize = std::env::var("WORKERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ncpu);

    eprintln!(
        "waker-repro: tokio 1.52.3, {ncpu} cpus, {workers} workers, {run_secs}s, stall threshold {STALL_SECS}s"
    );

    let mut slots: Vec<Arc<Slot>> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(run_secs);
    let done = Arc::new(AtomicBool::new(false));

    // B's responder threads: receive a oneshot sender, fire it after ~100us.
    // Dedicated std threads so the wake is always cross-thread and off-runtime.
    let (btx, brx) = std::sync::mpsc::channel::<tokio::sync::oneshot::Sender<u64>>();
    let brx = Arc::new(std::sync::Mutex::new(brx));
    for _ in 0..4 {
        let brx = Arc::clone(&brx);
        std::thread::spawn(move || loop {
            let req = { brx.lock().unwrap().recv() };
            match req {
                Ok(tx) => {
                    std::thread::sleep(Duration::from_micros(100));
                    let _ = tx.send(now_ms());
                }
                Err(_) => return,
            }
        });
    }

    // Watchdog: pure std. Prints throughput every 30s; on stall dumps and exits.
    let watchdog = {
        let done = Arc::clone(&done);
        let slots_ref: Arc<std::sync::Mutex<Vec<Arc<Slot>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let slots_for_watchdog = Arc::clone(&slots_ref);
        let handle = std::thread::spawn(move || {
            let mut last_report = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if done.load(Ordering::Relaxed) {
                    return;
                }
                let slots = slots_for_watchdog.lock().unwrap();
                let now = now_ms();
                let mut stalled: Vec<String> = Vec::new();
                for s in slots.iter() {
                    let age_ms = now.saturating_sub(s.last.load(Ordering::Relaxed));
                    if age_ms > STALL_SECS * 1000 {
                        stalled.push(format!(
                            "{}[{}] stalled {}s (iters={})",
                            s.name,
                            s.idx,
                            age_ms / 1000,
                            s.iters.load(Ordering::Relaxed)
                        ));
                    }
                }
                if !stalled.is_empty() {
                    eprintln!("=== STALL DETECTED (repro!) ===");
                    for line in &stalled {
                        eprintln!("  {line}");
                    }
                    eprintln!("=== run `sample {} 3` for thread states ===", std::process::id());
                    // Leave time for an external sample before dying.
                    std::thread::sleep(Duration::from_secs(15));
                    std::process::exit(2);
                }
                if last_report.elapsed() >= Duration::from_secs(30) {
                    last_report = Instant::now();
                    let total: u64 = slots.iter().map(|s| s.iters.load(Ordering::Relaxed)).sum();
                    eprintln!("[t+{}s] total iters {}", 0, total);
                }
            }
        });
        (slots_ref, handle)
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let mut handles = Vec::new();

        // A: spawn_blocking storm — 4x workers loops of short blocking jobs.
        for i in 0..workers * 4 {
            let s = Slot::new("spawn_blocking", i);
            slots.push(Arc::clone(&s));
            handles.push(tokio::spawn(async move {
                while Instant::now() < deadline {
                    let t = tokio::task::spawn_blocking(|| {
                        std::thread::sleep(Duration::from_micros(500));
                        7u64
                    })
                    .await
                    .unwrap();
                    assert_eq!(t, 7);
                    s.stamp();
                }
            }));
        }

        // B: cross-thread oneshot — sender fired from dedicated std threads.
        for i in 0..workers * 2 {
            let s = Slot::new("std_oneshot", i);
            slots.push(Arc::clone(&s));
            let btx = btx.clone();
            handles.push(tokio::spawn(async move {
                while Instant::now() < deadline {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    btx.send(tx).unwrap();
                    rx.await.unwrap();
                    s.stamp();
                }
            }));
        }

        // C: subprocess churn — SIGCHLD reaper path.
        for i in 0..8 {
            let s = Slot::new("child_wait", i);
            slots.push(Arc::clone(&s));
            handles.push(tokio::spawn(async move {
                while Instant::now() < deadline {
                    let st = tokio::process::Command::new("/usr/bin/true")
                        .status()
                        .await
                        .unwrap();
                    assert!(st.success());
                    s.stamp();
                }
            }));
        }

        // D: task churn — constant spawn of short-lived tasks, plus tokio::fs
        // traffic (routes through spawn_blocking internally).
        for i in 0..workers {
            let s = Slot::new("churn", i);
            slots.push(Arc::clone(&s));
            handles.push(tokio::spawn(async move {
                let dir = std::env::temp_dir().join(format!("waker-repro-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("f{i}"));
                while Instant::now() < deadline {
                    let inner = tokio::spawn(async { 1u64 });
                    let v = inner.await.unwrap();
                    tokio::fs::write(&path, b"x").await.unwrap();
                    let read = tokio::fs::read(&path).await.unwrap();
                    assert_eq!(read, b"x");
                    assert_eq!(v, 1);
                    s.stamp();
                }
            }));
        }

        // Hand the slots to the watchdog only once they exist.
        *watchdog.0.lock().unwrap() = slots.clone();

        for h in handles {
            h.await.unwrap();
        }
    });

    done.store(true, Ordering::Relaxed);
    let _ = watchdog.1.join();
    let total: u64 = slots.iter().map(|s| s.iters.load(Ordering::Relaxed)).sum();
    eprintln!("completed with no stall: {total} total iterations");
}
