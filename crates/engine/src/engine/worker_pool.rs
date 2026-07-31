//! The execute-concurrency pool: at most `capacity` targets running at once.
//!
//! A plain `tokio::sync::Semaphore` is the obvious fit and is the wrong one
//! here, for a reason that only shows up in this engine.
//!
//! `Semaphore::acquire` is fair and queueing: on release, tokio takes the permit
//! and **assigns** it to the first waiter in its queue, then wakes that waiter.
//! The permit belongs to that waiter's `Acquire` future from that moment on,
//! before the waiter has run — so if the waiter is never polled again, the
//! permit is spent and held by nobody. It cannot be re-granted, and dropping it
//! requires dropping a future nobody owns.
//!
//! In this engine, futures that are never polled again are routine. Every level
//! from the CLI down to `execute` runs inside a `hmemoizer` cell, and a cell
//! keeps its in-flight future when its last awaiter goes away — which fail-fast
//! (drop every sibling on the first error) and Ctrl-C both do wholesale. That
//! parked future keeps its place in the semaphore's queue.
//!
//! A wedged build measured exactly that: 99 targets queued for a permit, 21 of
//! them in chains reachable only from abandoned cells, against a pool of 12.
//! Every permit had been handed to a waiter that would never run, so the pool
//! read as fully busy while nothing at all was executing.
//!
//! So this pool never queues. A permit is taken with `try_acquire` by a task
//! that is *running the poll*, and a task that finds none parks on a `Notify`
//! holding nothing. An abandoned future parks there forever and costs the pool
//! nothing, because there is no permit to strand.
//!
//! # Why every waiter is woken
//!
//! Release wakes **all** parked waiters and they race for the permit. Waking one
//! would be cheaper and is not safe: an abandoned future is a registered waiter
//! like any other, so `notify_one` can hand the only notification to a task that
//! will never retry — and the permit then sits *free* while live waiters park
//! forever. That is the same failure with a different shape.
//!
//! The cost is a wake per waiter per release. That is real, and it is bounded by
//! work that is already this coarse (one release per executed target), where the
//! alternative is a build that stops.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// Bounds how many targets execute concurrently.
#[derive(Debug)]
pub struct WorkerPool {
    permits: Arc<Semaphore>,
    /// Signalled after a permit is returned. Never `notify_one` — see module
    /// docs.
    released: Notify,
    capacity: usize,
}

impl WorkerPool {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(capacity)),
            released: Notify::new(),
            capacity,
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Permits nobody is holding, right now.
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }

    /// Take a permit, parking until one is free.
    ///
    /// Holds nothing while parked: a caller dropped or abandoned mid-wait leaves
    /// the pool exactly as it found it.
    pub async fn acquire(self: &Arc<Self>) -> Result<WorkerPermit> {
        loop {
            // Registered *before* the attempt, so a release landing between the
            // failed attempt and the park is not missed. `notify_waiters` only
            // reaches waiters already registered when it runs.
            let parked = self.released.notified();
            tokio::pin!(parked);
            parked.as_mut().enable();

            if let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() {
                return Ok(WorkerPermit {
                    permit: Some(permit),
                    pool: Arc::clone(self),
                });
            }

            parked.await;
        }
    }
}

/// A held execute slot. Returns it to the pool on drop and wakes the waiters.
#[derive(Debug)]
pub struct WorkerPermit {
    /// `Option` so [`Drop`] can return the permit *before* announcing it.
    permit: Option<OwnedSemaphorePermit>,
    pool: Arc<WorkerPool>,
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        // Order matters: a waiter woken before the permit is back finds nothing,
        // parks again, and never hears about it — `notify_waiters` leaves no
        // notification behind for anyone who was not already registered.
        drop(self.permit.take());
        self.pool.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn bounds_concurrency_to_its_capacity() {
        let pool = WorkerPool::new(2);
        let a = pool.acquire().await.expect("first");
        let b = pool.acquire().await.expect("second");
        assert_eq!(pool.available(), 0);

        // A third caller must wait, and must not be holding anything while it
        // does — the whole point of the type.
        let mut third = Box::pin(pool.acquire());
        assert!(futures::poll!(&mut third).is_pending());
        assert_eq!(pool.available(), 0, "a waiter must hold no permit");

        drop(a);
        let c = third.await.expect("third");
        assert_eq!(pool.available(), 0);
        drop((b, c));
        assert_eq!(pool.available(), 2);
    }

    /// **The regression this type exists for.**
    ///
    /// A waiter that is dropped, or simply never polled again, must not consume
    /// a permit. `Semaphore::acquire` fails this: on release it assigns the
    /// permit to the queued waiter, so abandoning that waiter strands it.
    #[tokio::test]
    async fn an_abandoned_waiter_strands_no_permit() {
        let pool = WorkerPool::new(1);
        let held = pool.acquire().await.expect("held");

        // Three waiters that are polled once and then never again — the shape a
        // memoizer cell leaves behind when its last awaiter goes away.
        let mut abandoned: Vec<_> = (0..3).map(|_| Box::pin(pool.acquire())).collect();
        for w in &mut abandoned {
            assert!(futures::poll!(w).is_pending());
        }

        drop(held);

        // The permit is back and claimable, even though three futures are still
        // parked on the pool and none of them will ever run again.
        assert_eq!(
            pool.available(),
            1,
            "a released permit was handed to a waiter that will never use it"
        );
        let live = tokio::time::timeout(Duration::from_secs(5), pool.acquire())
            .await
            .expect("a live caller must not be blocked by abandoned waiters")
            .expect("acquire");
        drop(live);
        drop(abandoned);
        assert_eq!(pool.available(), 1);
    }

    /// The production wedge at this pool's level: a memoized
    /// `result → locked_result → execute_cache` chain whose leaf holds a
    /// [`WorkerPermit`] across a park on a waker-storing await, abandoned by
    /// fail-fast. The permit — this pool's, not a bare tokio semaphore's —
    /// must come back, and a live caller must be served.
    ///
    /// Deliberately not asserted here: `diag`'s `RunningPermit` / unaccounted
    /// bookkeeping. `diag::global()` is process-wide and the engine suite runs
    /// in parallel, so those numbers are not this test's to observe; the
    /// permit accounting itself is what wedged production and is what this
    /// asserts.
    #[tokio::test]
    async fn an_abandoned_memoized_chain_returns_its_worker_permit() {
        use hcore::hmemoizer::Memoizer;
        use std::sync::Mutex;

        let pool = WorkerPool::new(1);
        let mem_result: Arc<Memoizer<String, u32>> =
            Arc::new(Memoizer::with_tag("wp-repro-result"));
        let mem_locked: Arc<Memoizer<String, u32>> =
            Arc::new(Memoizer::with_tag("wp-repro-locked_result"));
        let mem_execute: Arc<Memoizer<String, u32>> =
            Arc::new(Memoizer::with_tag("wp-repro-execute_cache"));
        let stash: Arc<Mutex<Option<std::task::Waker>>> = Arc::new(Mutex::new(None));

        let mut outer = Box::pin(mem_result.process("//pkg:tgt".to_string(), {
            let (mem_locked, mem_execute) = (Arc::clone(&mem_locked), Arc::clone(&mem_execute));
            let (pool, stash) = (Arc::clone(&pool), Arc::clone(&stash));
            move || async move {
                mem_locked
                    .process("//pkg:tgt".to_string(), move || async move {
                        mem_execute
                            .process("//pkg:tgt".to_string(), move || async move {
                                let _permit = pool.acquire().await.expect("pool is never closed");
                                futures::future::poll_fn(move |cx| {
                                    *stash.lock().expect("stash") = Some(cx.waker().clone());
                                    std::task::Poll::<u32>::Pending
                                })
                                .await
                            })
                            .await
                    })
                    .await
            }
        }));

        assert!(futures::poll!(&mut outer).is_pending());
        assert_eq!(pool.available(), 0, "the parked leaf must hold the permit");
        assert!(
            stash.lock().expect("stash").is_some(),
            "leaf must have stored a waker"
        );

        drop(outer);

        assert_eq!(
            pool.available(),
            1,
            "abandoning the chain must return the worker permit to the pool"
        );
        let live = tokio::time::timeout(Duration::from_secs(5), pool.acquire())
            .await
            .expect("a live caller must not be blocked by the abandoned chain")
            .expect("acquire");
        drop(live);
    }

    /// Task-mode variant of the wedge above. With task-backed memoizers,
    /// cancellation is abort-based and teardown is *asynchronous*: the permit
    /// comes back when the runtime drops the aborted chain, not synchronously
    /// on the awaiter's drop. So the assertions are eventual (bounded), plus
    /// the property that actually matters to a user — a live caller is served
    /// while the abandoned chain drains.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_task_memoized_chain_returns_its_worker_permit() {
        use hcore::hmemoizer::Memoizer;

        let handle = tokio::runtime::Handle::current();
        let pool = WorkerPool::new(1);
        let mem_result: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag_task(
            "wp-task-repro-result",
            handle.clone(),
        ));
        let mem_locked: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag_task(
            "wp-task-repro-locked_result",
            handle.clone(),
        ));
        let mem_execute: Arc<Memoizer<String, u32>> =
            Arc::new(Memoizer::with_tag_task("wp-task-repro-execute_cache", handle));

        let mut outer = Box::pin(mem_result.process("//pkg:tgt".to_string(), {
            let (mem_locked, mem_execute) = (Arc::clone(&mem_locked), Arc::clone(&mem_execute));
            let pool = Arc::clone(&pool);
            move || async move {
                mem_locked
                    .process("//pkg:tgt".to_string(), move || async move {
                        mem_execute
                            .process("//pkg:tgt".to_string(), move || async move {
                                let _permit = pool.acquire().await.expect("pool is never closed");
                                futures::future::pending::<u32>().await
                            })
                            .await
                    })
                    .await
            }
        }));

        // The chain's bodies run in spawned tasks: drive the joiner and wait
        // (bounded) for the leaf to actually take the permit.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while pool.available() != 0 {
            assert!(futures::poll!(&mut outer).is_pending());
            assert!(
                tokio::time::Instant::now() < deadline,
                "the leaf never acquired the permit"
            );
            tokio::task::yield_now().await;
        }

        drop(outer);

        // Teardown is asynchronous under abort: the permit returns when the
        // runtime drops the chain. Eventual, but bounded.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while pool.available() != 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "abandoning the chain must (eventually) return the worker permit"
            );
            tokio::task::yield_now().await;
        }

        let live = tokio::time::timeout(Duration::from_secs(5), pool.acquire())
            .await
            .expect("a live caller must not be blocked by the abandoned chain")
            .expect("acquire");
        drop(live);
    }

    /// Dropping a waiter mid-wait leaves the pool untouched.
    #[tokio::test]
    async fn a_cancelled_waiter_leaves_no_trace() {
        let pool = WorkerPool::new(1);
        let held = pool.acquire().await.expect("held");
        let mut cancelled = Box::pin(pool.acquire());
        assert!(futures::poll!(&mut cancelled).is_pending());
        drop(cancelled);
        drop(held);
        assert_eq!(pool.available(), 1);
    }

    /// Every parked waiter is woken on release, so the one that gets the permit
    /// is never decided by a notification that a dead waiter swallowed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_waiters_all_make_progress() {
        let pool = WorkerPool::new(2);
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let pool = Arc::clone(&pool);
            tasks.spawn(async move {
                let permit = pool.acquire().await.expect("acquire");
                tokio::task::yield_now().await;
                drop(permit);
            });
        }
        let done = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(r) = tasks.join_next().await {
                r.expect("task");
            }
        })
        .await;
        assert!(done.is_ok(), "waiters starved");
        assert_eq!(pool.available(), 2);
    }
}
