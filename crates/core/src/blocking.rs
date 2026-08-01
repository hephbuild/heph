//! Bounded [`tokio::task::spawn_blocking`] for synchronous work that must not
//! run on a tokio runtime worker (tar/copy into the cache, gzip, borsh, sqlite
//! spool reads, Starlark evaluation, package walks).
//!
//! Running that work inline on a worker parks the worker with the runtime
//! unaware — it neither hands the worker's queue off nor spawns a replacement,
//! so enough concurrent jobs stop the reactor and the timer wheel and the
//! build looks hung while nothing is deadlocked (the failure #180 fixed).
//! [`run`] moves the job to tokio's blocking pool and awaits its
//! `JoinHandle`, bounded by a semaphore.
//!
//! ## History
//!
//! This module used to be a hand-rolled fixed pool of OS threads with a
//! `crossbeam_channel` queue, plus a process-wide waker registry re-woken by a
//! 250ms ticker thread ("the backstop"). Both existed because tokio's own
//! path was unusable or distrusted:
//!
//! - **No runtime context.** A cdylib plugin's futures used to be polled by
//!   host workers, where the plugin's statically-linked tokio saw no runtime
//!   at all — `spawn_blocking` there panics, and the panic aborts crossing
//!   the `extern "C"` seam. Since the spawn-at-the-seam change, every ABI
//!   entry point's body runs as a task on the cdylib's own runtime and every
//!   host callback body runs as a task on the host runtime, so every caller
//!   of [`run`] is polled with a runtime context, on both sides of the seam.
//! - **Distrusted wake-ups.** Tokio's cross-thread wake was believed to drop
//!   wake-ups on macOS under load (`docs/RCA_MACOS_WAKER.md`), so the result
//!   wait was insured by the ticker. A 2026-07-31 re-measurement could not
//!   reproduce the loss across ~40M wakes on the pinned tokio, and the
//!   `block_in_place` concurrency regression cited alongside it re-measured
//!   at parity within noise (`docs/CONCURRENCY_MEASUREMENTS.md`). The
//!   task-backed memoizer already trusts tokio's waker path for every build;
//!   so does this module.
//!
//! The registry's release coupling — GC flushing registered wakers because a
//! parked waker's `Arc` chain could pin a cache read guard — dissolved with
//! it: a `JoinHandle` await parks its waker in the task's join slot for
//! exactly the wait's lifetime, and an abandoned waiter is torn down by the
//! task-backed memoizer's abort cascade, which drops the chain and everything
//! it pins. See `Engine::run_trim_batch_with_delay` (`engine/gc.rs`) for the
//! consumer-side argument.
//!
//! ## Contract
//!
//! - **[`run`] requires a tokio runtime context** (it calls `spawn_blocking`).
//!   Every production caller has one post-seam; a sync caller that drives the
//!   future itself must enter a handle first (the buildfile LSP does).
//! - **Bounded.** Concurrency is capped at [`concurrency_limit`] (the old
//!   pool's size, `2 * cores`), not tokio's own `8 * cores + 64` blocking
//!   cap: an unbounded fan-out of gzip/Starlark jobs would thrash the CPU.
//!   The permit is acquired in async land — so a waiter dropped while
//!   queueing leaves cleanly, having spawned nothing — and then rides *into*
//!   the job, so a job whose caller was dropped still counts against the
//!   bound until it finishes. Right-sizing the bound against tokio's pool is
//!   a later, measured change.
//! - **Panics are transparent.** A panicking job resurfaces on the caller's
//!   task, exactly as the old pool's `catch_unwind` + `resume_unwind` did
//!   (tokio's task harness is the `catch_unwind` now).
//! - **Dropping the future detaches the job.** A `spawn_blocking` job cannot
//!   be aborted mid-run; dropping the `JoinHandle` lets it run to completion
//!   with the answer discarded — the same run-to-completion semantics the old
//!   pool had, and callers rely on it (a permit moved into a job is released
//!   even when nobody is left to await the answer).
//!
//! Jobs must be `'static`: a caller's future can be dropped (cancellation)
//! while its job is still running, so the job cannot borrow from the caller's
//! frame. Clone or `Arc` what it needs.

use std::cell::Cell;
use std::sync::LazyLock;
use tokio::sync::Semaphore;

/// Concurrency limit for [`run`] jobs.
///
/// The work is a mix of filesystem I/O (tar/copy into the cache, `stat`,
/// rmdir) and CPU (gzip, borsh, starlark evaluation), so it wants more slots
/// than cores — an I/O-bound job should not hold a slot a CPU-bound one could
/// use — but not so many that thousands of concurrent targets thrash the
/// disk. Callers that need a *tighter* bound impose their own (e.g. the
/// remote cache's `CODEC_SLOTS` caps concurrent gzip at the core count); this
/// is the ceiling underneath them. The value is the old dedicated pool's
/// thread count, preserved verbatim across the switch to `spawn_blocking`.
///
/// Public so callers that park inside a job to wait on *another* job can
/// assert their bound stays strictly below it (e.g. `pluginbuildfile`'s
/// `PKG_EVAL_SLOTS`, whose `LoadRegistry` condvar wait is deadlock-free only
/// while every claim holder can hold a slot of its own).
pub fn concurrency_limit() -> usize {
    /// Snapshotted once, not recomputed per call: [`SLOTS`] is sized from it
    /// at first touch, and `pluginbuildfile`'s `PKG_EVAL_SLOTS` asserts
    /// against it at its own first touch. Were `available_parallelism` ever to
    /// change under us (a cgroup resize), a recomputing function would let the
    /// reported limit, the asserted invariant, and the actual semaphore size
    /// disagree.
    static LIMIT: LazyLock<usize> = LazyLock::new(|| {
        let cores = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(8);
        (cores * 2).max(4)
    });
    *LIMIT
}

/// The bound. A static per linkage unit, not per runtime: the host binary and
/// each plugin cdylib statically link their own copy of this crate, each with
/// its own runtime and its own limit — exactly as each used to have its own
/// pool.
static SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(concurrency_limit()));

thread_local! {
    /// True while this thread is executing a [`run`] job.
    static IN_JOB: Cell<bool> = const { Cell::new(false) };
}

/// True while the calling thread is running a [`run`] job.
///
/// The witness for "this leaf was routed through [`run`]": tokio's blocking
/// threads carry no distinguishing name (a runtime names all its threads
/// alike), so tests that used to assert on the old pool's `heph-blocking-*`
/// thread names record this instead. Deliberately scoped to [`run`] jobs — a
/// bare `spawn_blocking` or `tokio::fs` op on the same pool reads `false`.
pub fn in_blocking_job() -> bool {
    IN_JOB.with(Cell::get)
}

/// Marks the job's thread for [`in_blocking_job`], reset on drop so a
/// panicking job cannot leave the flag set on a pool thread tokio will reuse.
struct JobMarker;

impl JobMarker {
    fn set() -> Self {
        IN_JOB.with(|c| c.set(true));
        Self
    }
}

impl Drop for JobMarker {
    fn drop(&mut self) {
        IN_JOB.with(|c| c.set(false));
    }
}

/// Run `f` on tokio's blocking pool, bounded by [`concurrency_limit`], and
/// await its result.
///
/// Requires a tokio runtime context — see the module docs for the full
/// contract, including panic transparency and drop-detaches semantics.
pub async fn run<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    // Queue in async land: a waiter dropped here (cancellation) leaves the
    // semaphore's queue cleanly, having spawned nothing — and the closure,
    // with whatever it owns (a caller's permit riding into the job), is
    // dropped on the cancelling thread.
    let permit = SLOTS
        .acquire()
        .await
        .expect("heph blocking slots semaphore is never closed");
    let handle = tokio::task::spawn_blocking(move || {
        // The permit rides into the job: once spawned, the job occupies a
        // slot until it *finishes*, even when the caller's future is dropped
        // mid-run — the same occupancy the old pool's threads enforced. Kept
        // in the caller's future instead, every detached job would run
        // outside the bound.
        let _permit = permit;
        let _marker = JobMarker::set();
        f()
    });
    match handle.await {
        Ok(value) => value,
        // Transparent panic propagation: resurface the job's panic on the
        // caller's task rather than wrapping it in a `JoinError`.
        Err(e) => match e.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            // Not a panic, and `run` never aborts the handle — so this is the
            // runtime refusing the job outright, i.e. submitting to a runtime
            // that is already gone (the LSP shape: an external thread holding
            // an entered handle whose server runtime shut down). A job merely
            // *queued* when a shutdown starts is not this case: tokio drains
            // its blocking queue on the way down and runs it (measured, see
            // `a_job_the_runtime_will_never_run_surfaces_as_a_panic`).
            //
            // Raised as a panic (via `resume_unwind`, the same shape a
            // panicking job produces) rather than folded into the return type
            // so `run` stays a drop-in for the synchronous call it replaced.
            Err(e) => std::panic::resume_unwind(Box::new(format!("heph blocking job lost: {e}"))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;

    /// [`SLOTS`] is process-wide and cargo runs these tests concurrently in
    /// one process, so a test that reasons about permit counts (holding them
    /// all, or parking jobs on a gate for its whole body) must not overlap
    /// another doing the same. Async-aware so holding it across `await` is
    /// sound.
    static EXCLUSIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Take every run slot, without ever parking in the semaphore's queue.
    ///
    /// A fair semaphore serves waiters in order, so a parked bulk acquire
    /// blocks every later acquire behind it — deadlocking against any job that
    /// holds a permit and needs another. Callers hold `EXCLUSIVE`.
    async fn drain_all_slots() -> tokio::sync::SemaphorePermit<'static> {
        let want = u32::try_from(concurrency_limit()).expect("limit fits u32");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(all) = SLOTS.try_acquire_many(want) {
                return all;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "every run slot must come free"
            );
            tokio::task::yield_now().await;
        }
    }

    /// A gate jobs park on, so a test controls exactly when its jobs finish.
    struct Gate {
        open: Mutex<bool>,
        cond: Condvar,
    }

    impl Gate {
        fn closed() -> Arc<Self> {
            Arc::new(Self {
                open: Mutex::new(false),
                cond: Condvar::new(),
            })
        }

        fn wait(&self) {
            let mut open = self.open.lock().expect("gate lock");
            while !*open {
                open = self.cond.wait(open).expect("gate wait");
            }
        }

        fn open(&self) {
            *self.open.lock().expect("gate lock") = true;
            self.cond.notify_all();
        }
    }

    #[tokio::test]
    async fn runs_off_the_calling_thread_and_returns_the_value() {
        let here = thread::current().id();
        let (value, ran_on, marked) =
            run(move || (41 + 1, thread::current().id(), in_blocking_job())).await;
        assert_eq!(value, 42);
        assert_ne!(ran_on, here, "job must not run on the caller's thread");
        assert!(marked, "a job must observe in_blocking_job()");
    }

    /// The whole point: a job that blocks its thread outright must not stop
    /// the runtime from making progress. If the work ran inline on the worker
    /// a single-worker runtime would never poll the timer, and this test
    /// would hang.
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
            "the runtime must keep polling while a blocking job runs",
        );
        ticker.await.expect("ticker");
    }

    /// A panicking job resurfaces on the caller's task, and later jobs are
    /// unaffected — under the old pool a leaked panic would have killed a
    /// pool thread and stranded every job scheduled onto it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panic_surfaces_on_the_caller_and_later_jobs_still_run() {
        let panicked = tokio::spawn(run(|| panic!("boom"))).await;
        assert!(panicked.is_err(), "panic must propagate to the caller");
        assert_eq!(run(|| 7).await, 7, "later jobs must still be served");
    }

    /// The witness is scoped to [`run`] jobs: a bare `spawn_blocking` on the
    /// same tokio pool must read `false`, or every test using the witness
    /// would pass for work that dodged this module (and its bound) entirely.
    #[tokio::test]
    async fn the_marker_identifies_run_jobs_not_the_shared_pool() {
        assert!(!in_blocking_job(), "an async caller is not a job");
        assert!(run(in_blocking_job).await, "inside a job it is set");
        let bare = tokio::task::spawn_blocking(in_blocking_job)
            .await
            .expect("bare spawn_blocking");
        assert!(!bare, "a bare spawn_blocking job is not a run job");
    }

    /// The bound: with every job parked on a gate, at most
    /// [`concurrency_limit`] of them may be running at once no matter how
    /// many are submitted. Mutation-verified: lifting the semaphore lets
    /// every submitted job start and `peak` overshoots.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrency_is_bounded_at_the_limit() {
        let _exclusive = EXCLUSIVE.lock().await;
        let limit = concurrency_limit();
        let n = limit + 4;
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let gate = Gate::closed();

        let jobs: Vec<_> = (0..n)
            .map(|_| {
                let running = Arc::clone(&running);
                let peak = Arc::clone(&peak);
                let gate = Arc::clone(&gate);
                tokio::spawn(run(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    gate.wait();
                    running.fetch_sub(1, Ordering::SeqCst);
                }))
            })
            .collect();

        // Eventual, bounded: the limit's worth of slots fill while the excess
        // queues on the semaphore.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while running.load(Ordering::SeqCst) < limit {
            assert!(
                std::time::Instant::now() < deadline,
                "the limit's worth of jobs must start"
            );
            tokio::task::yield_now().await;
        }

        gate.open();
        for job in jobs {
            job.await.expect("job");
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            limit,
            "no more than the limit may ever run at once, and the limit must be reachable",
        );
    }

    /// Dropping the awaiting future mid-job detaches the job: it still runs
    /// to completion (its side effect lands) and hands its slot back when it
    /// finishes. Callers rely on run-to-completion — a permit moved into a
    /// job must be released even when nobody is left to await the answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_caller_detaches_the_job_which_still_completes() {
        let _exclusive = EXCLUSIVE.lock().await;
        let gate = Gate::closed();
        let done = Arc::new(AtomicUsize::new(0));

        let mut fut = Box::pin(run({
            let gate = Arc::clone(&gate);
            let done = Arc::clone(&done);
            move || {
                gate.wait();
                done.fetch_add(1, Ordering::SeqCst);
            }
        }));
        // One poll acquires the (uncontended) permit and spawns the job.
        assert!(
            futures::poll!(&mut fut).is_pending(),
            "a gated job cannot answer before its first poll returns"
        );
        drop(fut);

        gate.open();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while done.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "a detached job must still run to completion"
            );
            tokio::task::yield_now().await;
        }
        // And its slot comes back once the job finishes (eventual: the permit
        // is dropped inside the job as it returns). Under EXCLUSIVE, the only
        // other holders are the transient quick-job tests, so the full limit
        // becoming acquirable is exactly the detached slot's return.
        let want = u32::try_from(concurrency_limit()).expect("limit fits u32");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(all) = SLOTS.try_acquire_many(want) {
                drop(all);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a detached job must release its slot when it finishes"
            );
            tokio::task::yield_now().await;
        }
    }

    /// The production wedge's leaf, driven through the real machinery: a
    /// memoized chain whose innermost computation holds a semaphore permit
    /// and parks inside [`run`] — then the chain's only awaiter is dropped,
    /// as fail-fast does. Everything the parked leaf holds must come back via
    /// the task-backed memoizer's abort cascade: the permit returns to the
    /// semaphore even though the blocking job is still running, detached.
    ///
    /// (This is the release edge that replaced the old backstop registry's
    /// `flush_backstop`: nothing retains the abandoned chain, so nothing
    /// needs a flush to let go of it.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_memoized_wait_releases_its_permit() {
        use crate::hmemoizer::Memoizer;

        let _exclusive = EXCLUSIVE.lock().await;

        let mem_result: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag_task(
            "bk-result",
            tokio::runtime::Handle::current(),
        ));
        let mem_execute: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag_task(
            "bk-execute_cache",
            tokio::runtime::Handle::current(),
        ));
        let permits = Arc::new(tokio::sync::Semaphore::new(1));

        // The job parks until released, so the wait is genuinely pending when
        // the abandonment happens.
        let gate = Gate::closed();

        let mut outer = Box::pin(mem_result.process("//pkg:tgt".to_string(), {
            let mem_execute = Arc::clone(&mem_execute);
            let permits = Arc::clone(&permits);
            let gate = Arc::clone(&gate);
            move || async move {
                mem_execute
                    .process("//pkg:tgt".to_string(), move || async move {
                        let _permit = permits
                            .acquire_owned()
                            .await
                            .expect("semaphore is never closed");
                        run(move || {
                            gate.wait();
                            7
                        })
                        .await
                    })
                    .await
            }
        }));
        assert!(
            futures::poll!(&mut outer).is_pending(),
            "the chain must park inside blocking::run"
        );
        // The chain's bodies run in spawned tasks: wait (bounded) for the
        // leaf to actually take the permit and park in the blocking wait.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while permits.available_permits() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the wait never captured the permit"
            );
            tokio::task::yield_now().await;
        }

        // Fail-fast: the only awaiter goes away while the job is still
        // running.
        drop(outer);

        // Task-cell teardown is asynchronous: the abort cascade lands when
        // the runtime processes it. Eventual, bounded.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while permits.available_permits() != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "abandoning the chain must release the permit held across the blocking wait"
            );
            tokio::task::yield_now().await;
        }

        // Let the detached job finish; its answer goes nowhere, and that is
        // fine.
        gate.open();
    }

    /// A job the runtime will never run surfaces on the caller as a panic —
    /// not as a hang, and not as a bogus value.
    ///
    /// The one non-panic `JoinError` [`run`] can see. Provoked by submitting
    /// to an already-shut-down runtime, which is deterministic; racing a
    /// shutdown against a *queued* job is not, because tokio drains its
    /// blocking queue on the way down (a queued job runs rather than being
    /// dropped — measured, not assumed).
    ///
    /// The reachable production shape is the LSP's: an external thread
    /// awaiting through an entered handle whose server runtime has gone away.
    #[test]
    fn a_job_the_runtime_will_never_run_surfaces_as_a_panic() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let handle = rt.handle().clone();
        rt.shutdown_background();

        let ran = Arc::new(AtomicUsize::new(0));
        let ran_in_job = Arc::clone(&ran);
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _entered = handle.enter();
            futures::executor::block_on(run(move || {
                ran_in_job.fetch_add(1, Ordering::SeqCst);
                42
            }))
        }));

        let payload = out.expect_err("a job that cannot run must not yield a value");
        let msg = payload
            .downcast_ref::<String>()
            .map_or("<not a string payload>", String::as_str);
        assert!(
            msg.contains("heph blocking job lost"),
            "a lost job must say so; got {msg:?}"
        );
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "the job must not have run at all"
        );
    }

    /// A job may itself call [`run`] and drive it to completion.
    ///
    /// Load-bearing, not hypothetical: a Starlark handler `block_on`s a
    /// provider function from inside a package-evaluation job, and anything
    /// below that may reach [`run`] again. The old pool needed no runtime
    /// context for this; `spawn_blocking` does, and it works only because
    /// tokio propagates the runtime context into a blocking closure. It is
    /// also the premise of `PKG_EVAL_SLOTS < concurrency_limit`: a claim
    /// holder inside a job must be able to take a slot of its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_can_run_a_nested_job() {
        // Serialized: this is the one test that holds a slot while acquiring a
        // second, so letting it interleave with the tests that park jobs on a
        // gate to fill every slot would deadlock both until their deadline.
        let _exclusive = EXCLUSIVE.lock().await;
        // Two permits, deadlock-free against the min-4 limit.
        let out = run(|| futures::executor::block_on(run(|| 7))).await;
        assert_eq!(out, 7);
    }

    /// A panicking job releases its slot — the permit rides into the closure,
    /// so the release is on the unwind path. Leaking one per panic would wedge
    /// the process after `limit` of them, which the panic-propagation test
    /// cannot see.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_job_releases_its_slot() {
        let _exclusive = EXCLUSIVE.lock().await;
        let panicked = tokio::spawn(run(|| panic!("boom"))).await;
        assert!(panicked.is_err(), "the job must have panicked");

        let want = u32::try_from(concurrency_limit()).expect("limit fits u32");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(all) = SLOTS.try_acquire_many(want) {
                drop(all);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a panicking job must hand its slot back"
            );
            tokio::task::yield_now().await;
        }
    }

    /// A panicking job must not leave [`IN_JOB`] set on the thread tokio will
    /// reuse — every `in_blocking_job` witness in the tree would then pass for
    /// work that never went through [`run`]. Pinned deterministically with a
    /// one-thread blocking pool, so the bare probe *must* land on the thread
    /// the panicking job used.
    #[test]
    fn a_panicking_job_clears_the_marker_for_the_next_user_of_its_thread() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let panicked = tokio::spawn(run(|| panic!("boom"))).await;
            assert!(panicked.is_err(), "the job must have panicked");
            let reused = tokio::task::spawn_blocking(in_blocking_job)
                .await
                .expect("probe");
            assert!(
                !reused,
                "a panicked job left the marker set on a reused blocking thread"
            );
        });
    }

    /// A waiter dropped *while still queued* spawns nothing and drops the
    /// closure — which is what releases anything the caller moved into it (a
    /// `PKG_EVAL_SLOTS` permit, in production).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_dropped_while_queued_spawns_nothing_and_drops_the_closure() {
        let _exclusive = EXCLUSIVE.lock().await;
        // Drained with try + yield rather than `acquire_many().await`: the
        // semaphore is fair, so a parked bulk acquire sits at the queue head
        // and blocks every later single acquire behind it — including one a
        // permit-holder is waiting on. `EXCLUSIVE` already excludes the tests
        // that could form that cycle; not parking means this cannot form one
        // with anything else either.
        let all = drain_all_slots().await;

        let ran = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        /// Stands in for a permit moved into the job.
        struct DropFlag(Arc<AtomicUsize>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let mut fut = Box::pin(run({
            let ran = Arc::clone(&ran);
            let carried = DropFlag(Arc::clone(&dropped));
            move || {
                let _carried = carried;
                ran.fetch_add(1, Ordering::SeqCst);
            }
        }));
        // Every slot is held, so this parks on the semaphore having spawned
        // nothing.
        assert!(futures::poll!(&mut fut).is_pending(), "must queue");
        drop(fut);
        drop(all);

        // Nothing to wait for: the drop is synchronous with the future's.
        assert_eq!(ran.load(Ordering::SeqCst), 0, "a queued job must not run");
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "dropping a queued caller must drop the job closure, releasing what it carried"
        );
    }

    /// Many jobs at once all complete, exercising queueing past the limit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queues_beyond_the_concurrency_limit() {
        let _exclusive = EXCLUSIVE.lock().await;
        let n = concurrency_limit() * 4;
        let jobs = (0..n).map(|i| run(move || i * 2));
        let out = futures::future::join_all(jobs).await;
        assert_eq!(out, (0..n).map(|i| i * 2).collect::<Vec<_>>());
    }
}
