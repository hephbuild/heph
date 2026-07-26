//! Fixed pool of long-lived OS threads for synchronous blocking work that must
//! not run on a tokio runtime worker.
//!
//! Every other way of running blocking work is unusable here for a different
//! reason:
//!
//! - **Inline on the worker** — what `hproc::process_supervisor::block_or_inline`
//!   does on Linux. The runtime is never told, so it neither hands the worker's
//!   queue off nor spawns a replacement: that thread simply stops polling. With
//!   `worker_threads = ncpu` (2–4 on a CI runner) a handful of concurrent cache
//!   writes park *every* worker, and then the reactor and the timer wheel stop
//!   running too — in-flight HTTP transfers make no progress, their deadlines
//!   never fire, the TUI freezes. Nothing is deadlocked and the build looks hung.
//!   This is the failure this module exists to remove.
//! - **`tokio::task::block_in_place`** — correct in principle (the runtime hands
//!   off), but measured a concurrency regression on this workload (0.94 → 0.74,
//!   see `PERFORMANCE.md`), because every call burns a worker handoff and pulls a
//!   fresh thread out of the blocking pool.
//! - **`tokio::task::spawn_blocking`** — its `JoinHandle` wake-up rides tokio's
//!   cross-thread waker, observed to drop wake-ups on macOS under heavy load (see
//!   `RCA_MACOS_WAKER.md`), which strands the awaiting task.
//!
//! So: a fixed set of named threads, a `crossbeam_channel` queue, and a
//! `oneshot` for the result. The threads are created once and live for the
//! process, so a job costs a channel send rather than a thread spawn.
//!
//! **Dropped-wake-up backstop.** The result still crosses threads, so [`run`]
//! does not simply `await` the `oneshot` — it re-polls on a timer
//! ([`WAKE_BACKSTOP`]). A lost wake-up then costs latency instead of stranding
//! the caller forever. This is the same defence the macOS child watcher uses for
//! its own dropped kernel events (`kqueue_macos.rs`), and it is cheap: the timer
//! only fires for jobs that outlive it.
//!
//! Jobs must be `'static`: a caller's future can be dropped (cancellation) while
//! its job is still running, so the job cannot borrow from the caller's frame.
//! Clone or `Arc` what it needs.

use crossbeam_channel::{Sender, unbounded};
use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

/// One unit of blocking work. Erased to `()` because the result travels back
/// over a `oneshot` the closure already owns.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// A panicking job's payload, forwarded so it resurfaces on the caller's task
/// rather than silently killing a pool thread.
type Panic = Box<dyn Any + Send + 'static>;

/// How often [`run`] re-polls a pending result.
///
/// Purely a backstop against a dropped cross-thread wake-up: on a healthy wake-up
/// the result arrives immediately and the timer is never reached. Short enough
/// that a lost wake-up is a hiccup, long enough that a pool of thousands of
/// queued jobs isn't paying for a busy poll.
const WAKE_BACKSTOP: Duration = Duration::from_millis(250);

/// Pool size.
///
/// The work is a mix of filesystem I/O (tar/copy into the cache, `stat`, rmdir)
/// and CPU (gzip, borsh, starlark evaluation), so it wants more threads than
/// cores — an I/O-bound job should not hold a slot a CPU-bound one could use —
/// but not so many that thousands of concurrent targets thrash the disk. Callers
/// that need a *tighter* bound impose their own (e.g. the remote cache's
/// `CODEC_SLOTS` caps concurrent gzip at the core count); this is the ceiling
/// underneath them.
fn pool_size() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(8);
    (cores * 2).max(4)
}

static POOL: OnceLock<Sender<Job>> = OnceLock::new();

fn sender() -> &'static Sender<Job> {
    POOL.get_or_init(|| {
        let (tx, rx) = unbounded::<Job>();
        for i in 0..pool_size() {
            let rx = rx.clone();
            thread::Builder::new()
                .name(format!("heph-blocking-{i}"))
                .spawn(move || {
                    // Ends only when every sender is dropped, which for a
                    // process-lifetime static means at exit.
                    for job in rx.iter() {
                        job();
                    }
                })
                // Same stance as the sandbox cleaner: a process that cannot spawn
                // its worker threads at startup has nothing to fall back to.
                .expect("spawn heph blocking-io thread");
        }
        tx
    })
}

/// Fail a [`run`] the same way a panicking job does, for the two states that
/// cannot happen unless the pool itself is broken (its queue closed, or a thread
/// dropping a job without answering — `catch_unwind` means even a panic answers).
/// Raised as a panic rather than folded into the return type so [`run`] stays a
/// drop-in for the synchronous call it replaced.
fn pool_broken(what: &'static str) -> ! {
    std::panic::resume_unwind(Box::new(format!("heph blocking pool {what}")))
}

/// Run `f` on the blocking pool and await its result.
///
/// Panics are transparent: a panicking job resurfaces on the caller's task
/// instead of taking down a pool thread and stranding every later job.
pub async fn run<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, mut rx) = tokio::sync::oneshot::channel::<Result<R, Panic>>();
    let job: Job = Box::new(move || {
        let out = catch_unwind(AssertUnwindSafe(f));
        // The receiver is gone when the caller's future was dropped — the job
        // still had to run to completion, but nobody wants the answer.
        drop(tx.send(out));
    });
    if sender().send(job).is_err() {
        pool_broken("queue closed");
    }

    // Re-poll rather than awaiting once: see the dropped-wake-up backstop note
    // in the module docs.
    loop {
        match tokio::time::timeout(WAKE_BACKSTOP, &mut rx).await {
            Ok(Ok(Ok(value))) => return value,
            Ok(Ok(Err(panic))) => std::panic::resume_unwind(panic),
            Ok(Err(_closed)) => pool_broken("dropped a job unanswered"),
            Err(_elapsed) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn runs_off_the_calling_thread_and_returns_the_value() {
        let here = thread::current().id();
        let (value, ran_on) = run(move || (41 + 1, thread::current().id())).await;
        assert_eq!(value, 42);
        assert_ne!(ran_on, here, "job must not run on the caller's thread");
    }

    /// The whole point: a job that blocks its thread outright must not stop the
    /// runtime from making progress. If the work ran inline on the worker (what
    /// `block_or_inline` does on Linux) a single-worker runtime would never poll
    /// the timer, and this test would hang.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_work_does_not_stall_the_runtime() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticker = tokio::spawn({
            let ticks = Arc::clone(&ticks);
            async move {
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        run(|| thread::sleep(Duration::from_millis(120))).await;
        assert!(
            ticks.load(Ordering::SeqCst) > 0,
            "the runtime must keep polling while a pool job blocks",
        );
        ticker.await.expect("ticker");
    }

    /// A panicking job must not kill its pool thread — that would silently strand
    /// every job scheduled onto it for the rest of the process.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panic_surfaces_on_the_caller_and_the_pool_survives() {
        let panicked = tokio::spawn(run(|| panic!("boom"))).await;
        assert!(panicked.is_err(), "panic must propagate to the caller");
        assert_eq!(run(|| 7).await, 7, "the pool must still serve jobs");
    }

    /// Many jobs at once all complete, exercising the queue past the pool size.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queues_beyond_the_pool_size() {
        let n = pool_size() * 4;
        let jobs = (0..n).map(|i| run(move || i * 2));
        let out = futures::future::join_all(jobs).await;
        assert_eq!(out, (0..n).map(|i| i * 2).collect::<Vec<_>>());
    }
}
