//! The execute-concurrency pool: at most `capacity` targets running at once.
//!
//! A thin wrapper over `tokio::sync::Semaphore`. It was not always thin: under
//! the poll-cell memoizer, futures that were never polled again (parked in
//! abandoned cells) were routine, and the semaphore's fair queue would assign
//! a released permit to exactly such a waiter — spent, held by nobody,
//! unrecoverable. A wedged build measured 99 queued waiters, 21 of them in
//! abandoned chains, against a pool of 12. This module then queued nothing:
//! `try_acquire` + a `Notify` wake-all race.
//!
//! Task-backed memoizers dissolved the premise: every acquirer is a spawned
//! task the runtime always polls, and cancellation is an abort whose drop
//! leaves the queue (an assigned-but-unclaimed permit returns on
//! `Acquire::drop`). A future nobody polls and nobody drops no longer exists,
//! so the plain fair semaphore is sound — and O(1) per release where the
//! wake-all loop was O(waiters).
//!
//! `available()` stays as the diag gauge (`engine.rs` registers it).

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Bounds how many targets execute concurrently.
#[derive(Debug)]
pub struct WorkerPool {
    permits: Arc<Semaphore>,
    capacity: usize,
}

impl WorkerPool {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(capacity)),
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

    /// Take a permit, parking fairly until one is free.
    ///
    /// A waiter dropped mid-wait leaves the queue; a waiter dropped after
    /// being assigned a permit returns it (`Acquire::drop`). Every acquirer in
    /// the engine is a runtime-polled task, so neither case strands anything —
    /// see the module docs for the history that made this non-obvious.
    pub async fn acquire(self: &Arc<Self>) -> Result<WorkerPermit> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("the worker pool semaphore is never closed");
        Ok(WorkerPermit { _permit: permit })
    }
}

/// A held execute slot; returns to the pool on drop.
#[derive(Debug)]
pub struct WorkerPermit {
    _permit: OwnedSemaphorePermit,
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
        let mem_execute: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag_task(
            "wp-task-repro-execute_cache",
            handle,
        ));

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
