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

/// Waiters to re-wake on every tick, keyed by registration.
///
/// **Retained**, not drained. The list used to be emptied by each tick, on the
/// reasoning that waking a waiter provokes a poll and the poll re-registers it.
/// That holds only if the wake actually reaches the future — and in this engine
/// it need not. `run` is awaited inside a `hmemoizer` cell, so the waker handed
/// to it is the *cell's* waker, and `Cell::wake_by_ref` deliberately drops a wake
/// when the cell already has a driver that owes it a re-poll. One swallowed wake
/// then meant the waiter was never polled, never re-registered, and never woken
/// again — with the blocking pool sitting idle on an empty queue because its job
/// had long since finished.
///
/// That stranded twelve targets inside `execute`'s sandbox cleanup while they
/// held every worker permit, with ninety more queued behind them on the
/// semaphore and the whole build wedged. Retaining the registration until the
/// waiter is done makes the backstop's guarantee hold no matter what the waker
/// does with a wake.
static PENDING: Mutex<Vec<(u64, Waker)>> = Mutex::new(Vec::new());

static NEXT_REGISTRATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

static BACKSTOP_THREAD: Once = Once::new();

/// One pass of the backstop: wake every live registration, keeping them armed.
///
/// Split out of the thread's loop so the retention contract can be asserted by
/// calling this directly. Timing a real tick means sleeping for a multiple of
/// [`WAKE_BACKSTOP`] and counting wakes, which on a loaded CI runner measures the
/// scheduler rather than the invariant.
///
/// Returns how many registrations were woken.
fn tick() -> usize {
    // Cloned under the lock and woken outside it: a waker may re-enter this
    // module from inside `wake`.
    let due: Vec<Waker> = lock_pending().iter().map(|(_, w)| w.clone()).collect();
    let n = due.len();
    for waker in due {
        waker.wake();
    }
    n
}

fn start_backstop_thread() {
    BACKSTOP_THREAD.call_once(|| {
        thread::Builder::new()
            .name("heph-blocking-wake".to_string())
            .spawn(|| {
                loop {
                    thread::sleep(WAKE_BACKSTOP);
                    tick();
                }
            })
            // Same stance as the pool itself: no fallback worth having.
            .expect("spawn heph blocking-wake thread");
    });
}

/// A live backstop registration. The waker stays armed until this is dropped.
///
/// Held by the awaiting future, so the registration's lifetime is exactly the
/// wait's: it goes away when the result arrives *or* when the future is
/// cancelled, and never outlives either. That is what lets the tick retain
/// entries instead of draining them.
pub struct Backstop {
    id: u64,
}

impl Backstop {
    /// Reserve a registration, starting the backstop thread on first use (a
    /// process that never blocks never pays for it). Nothing is armed until
    /// [`arm`](Self::arm) is called with a waker.
    pub fn new() -> Self {
        start_backstop_thread();
        Self {
            id: NEXT_REGISTRATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Arm (or refresh) this registration with the polling task's waker. Call
    /// from every pending poll; re-arming with the same waker is free.
    pub fn arm(&self, waker: &Waker) {
        let mut pending = lock_pending();
        match pending.iter_mut().find(|(id, _)| *id == self.id) {
            Some((_, armed)) => {
                if !armed.will_wake(waker) {
                    *armed = waker.clone();
                }
            }
            None => pending.push((self.id, waker.clone())),
        }
    }
}

impl Default for Backstop {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Backstop {
    fn drop(&mut self) {
        let mut pending = lock_pending();
        if let Some(i) = pending.iter().position(|(id, _)| *id == self.id) {
            pending.swap_remove(i);
        }
    }
}

/// Wake every registered backstop waiter now rather than on the next tick, and
/// hand back the `Waker`s it was holding.
///
/// Waking early is always sound: a spurious wake costs one poll, and a waiter
/// that is genuinely still pending re-arms from the poll that wake provokes.
///
/// **Releasing is half the point, and it is not only about *finished* waits.** A
/// `Waker` owns whatever it can reach — here an `Arc<hmemoizer::Cell>`, whose
/// memoized value for `mem_locked_result` is the addr's riding cache read. The
/// post-run cache trim has to take a write lock, so every one of those reads must
/// be gone before it runs, or the trim finds its target contended and silently
/// skips.
///
/// [`Backstop`] fixes the *finished* half at the source: a registration is
/// dropped when its wait ends, so a completed waiter no longer pins anything
/// until a tick sweeps it. But a request can be torn down while background work
/// is still in flight, and those registrations are live, not stale — the guard
/// will not release them because the wait genuinely has not ended. Taking them
/// here is what unpins the read guards, and the still-pending waiter re-arms on
/// its next poll.
///
/// (That re-arm is the same assumption the tick deliberately no longer makes: see
/// [`PENDING`]. It is sound here because this is a teardown path — the wedge the
/// tick's retention guards against happens mid-run, under a memoizer cell that
/// swallows a wake it thinks is redundant.)
///
/// Returns how many registrations were taken, so a caller can tell its own flush
/// from a tick that happened to land first.
pub fn flush_backstop() -> usize {
    // Taken, not cloned: releasing the `Waker`s is half the point (see above).
    // Taken before waking, and the lock released first, so a waker that re-arms
    // from inside `wake` cannot deadlock against us — its `arm` simply pushes a
    // fresh entry under the same id.
    let due: Vec<Waker> = std::mem::take(&mut *lock_pending())
        .into_iter()
        .map(|(_, w)| w)
        .collect();
    let n = due.len();
    for waker in due {
        // Callers reach this from `Drop` on a teardown path. A panicking waker
        // there would unwind out of a destructor — and abort outright if that
        // drop is itself already unwinding — so one bad waker is contained
        // rather than allowed to take the process with it. The panic is still
        // reported by the default hook before this returns, and this crate has
        // no `tracing` (it is linked into cdylib plugins, where no subscriber is
        // ever installed), so there is nothing further to say about it here.
        // `lock_pending` already contemplates this case for the tick thread.
        drop(catch_unwind(AssertUnwindSafe(|| waker.wake())));
    }
    n
}

/// A waker panicking mid-`wake` would poison the list and strand every later
/// waiter, so poisoning is ignored — the `Vec` is still consistent.
fn lock_pending() -> std::sync::MutexGuard<'static, Vec<(u64, Waker)>> {
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
    // wake-up: see the dropped-wake-up note in the module docs. The registration
    // is held for the whole wait and released here on the way out — including on
    // cancellation, when this future is dropped mid-`await`.
    let armed = Backstop::new();
    let received = poll_fn(|cx| match Pin::new(&mut rx).poll(cx) {
        Poll::Ready(out) => Poll::Ready(out),
        Poll::Pending => {
            armed.arm(cx.waker());
            Poll::Pending
        }
    })
    .await;
    drop(armed);

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

    /// [`PENDING`] is process-wide, and `flush_backstop` takes *everything* in
    /// it. Cargo runs these tests in threads of one process, so a test that
    /// flushes will happily disarm a registration another test is still watching.
    /// Every test that touches the shared list holds this first.
    static EXCLUSIVE: Mutex<()> = Mutex::new(());

    /// Poisoning is ignored: the guard protects no invariant of its own, and a
    /// failing test must not cascade into every later one.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner())
    }

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
    /// finished task still holds would stay reachable from `PENDING` for as long
    /// as the registration does. It must therefore end with the wait, not with a
    /// tick or a flush, which is what the post-run cache trim depends on.
    #[test]
    fn a_registration_is_released_when_its_wait_ends() {
        let _exclusive = exclusive();
        struct Owning(#[expect(dead_code, reason = "held to observe the refcount")] Arc<()>);
        impl futures::task::ArcWake for Owning {
            fn wake_by_ref(_: &Arc<Self>) {}
        }

        let owned = Arc::new(());
        let armed = Backstop::new();

        // Cloning a `Waker` clones the `Arc<Owning>`, so every clone shares the
        // one inner `Arc<()>`: the count moves only when the last `Waker` goes.
        // Drop the local one here so what remains is the registration's alone.
        {
            let waker = futures::task::waker(Arc::new(Owning(Arc::clone(&owned))));
            armed.arm(&waker);
        }
        assert_eq!(
            Arc::strong_count(&owned),
            2,
            "armed: the registration is the only thing still holding the waker"
        );

        drop(armed);
        assert_eq!(
            Arc::strong_count(&owned),
            1,
            "ending the wait must release the waker the registration held"
        );
    }

    /// A flush must hand back the wakers of waits that are *still pending*, not
    /// only of finished ones.
    ///
    /// A `Waker` owns whatever it can reach — an `Arc<hmemoizer::Cell>`, whose
    /// memoized `mem_locked_result` value is the addr's riding cache read. A
    /// request can be torn down while background uploads are still in flight, and
    /// those registrations are live rather than stale, so [`Backstop`]'s
    /// end-of-wait release does not cover them. Leaving them armed pins the read
    /// guards, and the post-run cache-history trim then finds every target
    /// contended and silently skips
    /// (`e2e::cache_history_is_enforced_by_the_end_of_the_run`).
    #[test]
    fn a_flush_releases_a_still_pending_registration() {
        let _exclusive = exclusive();
        struct Owning(#[expect(dead_code, reason = "held to observe the refcount")] Arc<()>);
        impl futures::task::ArcWake for Owning {
            fn wake_by_ref(_: &Arc<Self>) {}
        }

        let owned = Arc::new(());
        // Deliberately kept alive for the whole test: this stands for a wait that
        // has not ended, so nothing but the flush can release its waker.
        let armed = Backstop::new();
        {
            let waker = futures::task::waker(Arc::new(Owning(Arc::clone(&owned))));
            armed.arm(&waker);
        }
        assert_eq!(Arc::strong_count(&owned), 2, "armed");

        assert!(
            flush_backstop() >= 1,
            "the flush must take our registration"
        );
        assert_eq!(
            Arc::strong_count(&owned),
            1,
            "a flush must release the waker even though the wait is still live"
        );

        drop(armed);
    }

    /// The bug this module's retention exists to prevent.
    ///
    /// The list used to be drained by each tick, on the reasoning that waking a
    /// waiter provokes a poll that re-registers it. A waker that swallows the
    /// wake — `hmemoizer`'s cell does, when it already has a driver owing it a
    /// re-poll — broke that: one dropped wake and the waiter was never polled,
    /// never re-registered, and never woken again. That stranded twelve targets
    /// in `execute`'s sandbox cleanup holding every worker permit, with the
    /// blocking pool idle because their jobs had already finished.
    #[test]
    fn a_swallowed_wake_does_not_disarm_the_backstop() {
        let _exclusive = exclusive();
        /// Counts wakes and drops every one, like a cell that already has a
        /// driver on the hook.
        struct Swallowing(Arc<AtomicUsize>);
        impl futures::task::ArcWake for Swallowing {
            fn wake_by_ref(me: &Arc<Self>) {
                me.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let wakes = Arc::new(AtomicUsize::new(0));
        let waker = futures::task::waker(Arc::new(Swallowing(Arc::clone(&wakes))));

        let armed = Backstop::new();
        armed.arm(&waker);

        // Driven, not timed. Under the old drain-once list the first tick would
        // consume the registration and the second would find nothing, because a
        // swallowed wake never produces the re-registering poll. Calling `tick`
        // directly asserts exactly that and nothing about the scheduler.
        for round in 1..=3 {
            tick();
            assert_eq!(
                wakes.load(Ordering::SeqCst),
                round,
                "a waiter whose wakes are swallowed must be re-woken on every tick"
            );
        }

        drop(armed);
        let before = wakes.load(Ordering::SeqCst);
        tick();
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            before,
            "and must stop being woken once its wait has ended"
        );
    }

    /// A waiter that is genuinely still pending must survive a flush: it is
    /// woken, re-polls, finds its job unfinished and re-registers. This is the
    /// invariant that makes `flush_backstop` safe to call from anywhere.
    #[test]
    fn flush_backstop_does_not_strand_a_still_pending_waiter() {
        let _exclusive = exclusive();
        let out = futures::executor::block_on(async {
            let job = run(|| {
                thread::sleep(WAKE_BACKSTOP / 2);
                "done"
            });
            futures::pin_mut!(job);
            // Flush repeatedly while the job is still running; each one takes the
            // waiter's registration out from under it.
            loop {
                let mut flushed = 0;
                let polled = futures::poll!(&mut job);
                if let Poll::Ready(v) = polled {
                    break v;
                }
                while flushed < 3 {
                    flush_backstop();
                    flushed += 1;
                    thread::sleep(Duration::from_millis(20));
                }
            }
        });
        assert_eq!(
            out, "done",
            "a flushed-but-pending waiter must still finish"
        );
    }

    /// A waiter must be re-woken while its job is still running — that spare
    /// wake-up is the whole defence against a dropped one. Counted with a waker
    /// nothing else can wake: the job is still asleep, so any wake seen here came
    /// from the backstop and not from the job answering.
    #[test]
    fn backstop_re_wakes_a_waiter_while_its_job_is_still_running() {
        let _exclusive = exclusive();
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
