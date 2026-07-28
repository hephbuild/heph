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
//! does not simply `await` the `oneshot` — a pending waiter is re-woken on a timer
//! ([`WAKE_BACKSTOP`]). A lost wake-up then costs latency instead of stranding
//! the caller forever. This is the same defence the macOS child watcher uses for
//! its own dropped kernel events (`kqueue_macos.rs`).
//!
//! The backstop is a plain thread, *not* `tokio::time::timeout`, because [`run`]
//! is also awaited inside a loaded cdylib plugin: the plugin's statically-linked
//! tokio is a separate instance from the host's, and the future is polled by a
//! host worker, so the plugin's tokio sees no runtime context at all. A tokio
//! timer there panics with "there is no reactor running", and that panic crosses
//! the plugin's `extern "C"` ABI seam, where it aborts the process. Nothing in
//! this module may touch the reactor; a `oneshot` is plain waker traffic and is
//! fine.
//!
//! Jobs must be `'static`: a caller's future can be dropped (cancellation) while
//! its job is still running, so the job cannot borrow from the caller's frame.
//! Clone or `Arc` what it needs.

use crossbeam_channel::{Sender, unbounded};
use std::any::Any;
use std::future::{Future, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Mutex, Once, OnceLock};
use std::task::{Poll, Waker};
use std::thread;
use std::time::Duration;

/// One unit of blocking work. Erased to `()` because the result travels back
/// over a `oneshot` the closure already owns.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// A panicking job's payload, forwarded so it resurfaces on the caller's task
/// rather than silently killing a pool thread.
type Panic = Box<dyn Any + Send + 'static>;

/// How often a pending [`run`] waiter is re-woken.
///
/// Purely a backstop against a dropped cross-thread wake-up: on a healthy wake-up
/// the result arrives immediately and the tick is never reached. Short enough
/// that a lost wake-up is a hiccup, long enough that a pool of thousands of
/// queued jobs isn't paying for a busy poll.
const WAKE_BACKSTOP: Duration = Duration::from_millis(250);

/// Waiters to re-wake on the next tick. Drained by the backstop thread, so a
/// registration is consumed by one tick and a still-pending waiter re-registers
/// from the poll that tick provokes.
static PENDING: Mutex<Vec<Waker>> = Mutex::new(Vec::new());

static BACKSTOP_THREAD: Once = Once::new();

/// Re-wake `waker` within [`WAKE_BACKSTOP`], starting the backstop thread on the
/// first pending waiter (a process that never blocks never pays for it).
///
/// Public because [`run`] is not the only future in the tree woken from a
/// non-tokio thread: the sqlite cache's write-behind queue signals its commit
/// from a dedicated writer thread, which is the same dropped-wake-up exposure
/// for the same reason. One backstop thread serves every such waiter.
pub fn backstop(waker: Waker) {
    BACKSTOP_THREAD.call_once(|| {
        thread::Builder::new()
            .name("heph-blocking-wake".to_string())
            .spawn(|| {
                loop {
                    thread::sleep(WAKE_BACKSTOP);
                    let due = std::mem::take(&mut *lock_pending());
                    for waker in due {
                        waker.wake();
                    }
                }
            })
            // Same stance as the pool itself: no fallback worth having.
            .expect("spawn heph blocking-wake thread");
    });
    lock_pending().push(waker);
}

/// Wake every registered backstop waiter now rather than on the next tick.
///
/// A registration outlives the future that made it. [`backstop`] is a
/// fire-and-forget push: a waiter re-registers on every pending poll and never
/// unregisters, so once its result arrives its last registration simply sits in
/// [`PENDING`] until a tick sweeps it — and a `Waker` owns its task, which owns
/// everything that task's state holds. For up to [`WAKE_BACKSTOP`] after a piece
/// of work is completely finished, its state is still reachable from this list.
///
/// Usually that is invisible: it is bounded, and it costs a little memory. It is
/// *not* invisible to a caller that must observe those values actually released
/// — the post-run cache trim needs the request's cache read guards gone before
/// it can take a write lock, and a stale registration here is enough to keep one
/// alive past the request that made it.
///
/// Waking early is always sound: a spurious wake costs one poll, and a waiter
/// still genuinely pending re-registers from that poll.
pub fn flush_backstop() {
    // Taken before waking, and the lock released first, so a waker that
    // re-registers from inside `wake` cannot deadlock against us.
    let due = std::mem::take(&mut *lock_pending());
    for waker in due {
        waker.wake();
    }
}

/// A waker panicking mid-`wake` would poison the list and strand every later
/// waiter, so poisoning is ignored — the `Vec` is still consistent.
fn lock_pending() -> std::sync::MutexGuard<'static, Vec<Waker>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

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

    // Arm the backstop on every pending poll rather than trusting a single
    // wake-up: see the dropped-wake-up note in the module docs.
    let received = poll_fn(|cx| match Pin::new(&mut rx).poll(cx) {
        Poll::Ready(out) => Poll::Ready(out),
        Poll::Pending => {
            backstop(cx.waker().clone());
            Poll::Pending
        }
    })
    .await;

    match received {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_closed) => pool_broken("dropped a job unanswered"),
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

    /// Inside a loaded cdylib plugin the plugin's own tokio has no runtime
    /// context — the host worker polls the plugin's future through the stable ABI
    /// seam. Anything here that touched the reactor panicked there, and that panic
    /// aborts the process on its way back across the `extern "C"` boundary.
    /// `block_on` with no runtime installed reproduces exactly that context.
    #[test]
    fn works_with_no_tokio_runtime_installed() {
        assert_eq!(futures::executor::block_on(run(|| 7)), 7);
    }

    /// Same, for a job slow enough that the waiter actually parks and arms the
    /// backstop — the reactor-free path has to carry the pending case too.
    #[test]
    fn pending_job_completes_with_no_tokio_runtime_installed() {
        let out = futures::executor::block_on(run(|| {
            thread::sleep(WAKE_BACKSTOP * 2);
            "done"
        }));
        assert_eq!(out, "done");
    }

    /// A registration owns its waker, and a waker owns its task — so anything a
    /// finished task still holds stays reachable from `PENDING` until a tick
    /// sweeps it. `flush_backstop` is how a caller that needs those values
    /// *actually* released gets them back without waiting out the tick, which is
    /// what the post-run cache trim depends on.
    #[test]
    fn flush_backstop_releases_registered_wakers_without_waiting_for_a_tick() {
        struct Owning(#[expect(dead_code, reason = "held to observe the refcount")] Arc<()>);
        impl futures::task::ArcWake for Owning {
            fn wake_by_ref(_: &Arc<Self>) {}
        }

        let owned = Arc::new(());
        let started = std::time::Instant::now();
        backstop(futures::task::waker(Arc::new(Owning(Arc::clone(&owned)))));
        flush_backstop();

        assert_eq!(
            Arc::strong_count(&owned),
            1,
            "flushing must drop the registration, not just wake it",
        );
        assert!(
            started.elapsed() < WAKE_BACKSTOP,
            "the release must not have come from a backstop tick",
        );
    }

    /// A waiter must be re-woken while its job is still running — that spare
    /// wake-up is the whole defence against a dropped one. Counted with a waker
    /// nothing else can wake: the job is still asleep, so any wake seen here came
    /// from the backstop and not from the job answering.
    #[test]
    fn backstop_re_wakes_a_waiter_while_its_job_is_still_running() {
        use std::task::{Context, RawWaker, RawWakerVTable, Waker};

        static WAKES: AtomicUsize = AtomicUsize::new(0);
        unsafe fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        unsafe fn wake(_: *const ()) {
            WAKES.fetch_add(1, Ordering::SeqCst);
        }
        unsafe fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, noop);
        let counting = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };

        let mut fut = Box::pin(run(|| {
            thread::sleep(WAKE_BACKSTOP * 4);
            11
        }));
        assert!(
            fut.as_mut()
                .poll(&mut Context::from_waker(&counting))
                .is_pending(),
            "a job that sleeps cannot answer before its first poll returns",
        );

        thread::sleep(WAKE_BACKSTOP * 2);
        assert!(
            WAKES.load(Ordering::SeqCst) > 0,
            "the backstop must re-wake a waiter whose job is still running",
        );

        assert_eq!(futures::executor::block_on(fut), 11);
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
