//! What every sqlite store in heph shares. Each store keeps its own schema and
//! pragmas — they differ on purpose (the walker cache runs
//! `synchronous = OFF`, the local cache `NORMAL`) — so this holds only what
//! must not differ.
//!
//! # An r2d2 pool whose helper thread leaves with it
//!
//! Build an r2d2 pool with [`pool_builder`], never `r2d2::Pool::builder()`.
//! `r2d2::Pool::builder()` gives every pool its own `ScheduledThreadPool` of
//! three `r2d2-worker-N` threads, and that pool's default drop behaviour runs
//! its pending scheduled jobs first. The connection reaper is one of them, due
//! every 30 s, so a dropped pool's threads outlive it by up to 30 s. A process
//! that opens and drops caches faster than that piles them up: the engine's
//! unit tests reached 1,620 `r2d2-worker` threads 7 s into a run, past the
//! per-process thread limit of a CI macOS runner, and tests failed with
//! `WouldBlock` (EAGAIN) from `pthread_create`.
//!
//! The pool here discards its pending jobs on drop, so its thread exits as the
//! r2d2 pool does. One thread is enough: it only reaps idle connections and
//! tops the pool up to `min_idle`.

use r2d2::ManageConnection;
use scheduled_thread_pool::{OnPoolDropBehavior, ScheduledThreadPool};
use std::sync::Arc;

/// `r2d2::Pool::builder()`, with a helper thread that exits when the pool is dropped.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place it is called: everywhere else goes through here"
)]
pub fn pool_builder<M: ManageConnection>() -> r2d2::Builder<M> {
    r2d2::Pool::builder().thread_pool(helper_pool())
}

fn helper_pool() -> Arc<ScheduledThreadPool> {
    Arc::new(
        ScheduledThreadPool::builder()
            .num_threads(1)
            .thread_name_pattern("r2d2-worker-{}")
            .on_drop_behavior(OnPoolDropBehavior::DiscardPendingScheduled)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A worker holds the pool's shared state, queue included, until it exits;
    /// so a job queued far in the future is dropped exactly when the last
    /// worker has gone. With the default drop behaviour it sits there until
    /// it is due — the reaper's 30 s.
    #[test]
    fn a_dropped_helper_pool_lets_its_thread_go_at_once() {
        let pool = helper_pool();
        let queued = Arc::new(());
        let job = Arc::clone(&queued);
        pool.execute_after(Duration::from_secs(600), move || drop(job));
        drop(pool);

        let deadline = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&queued) > 1 {
            assert!(
                Instant::now() < deadline,
                "the helper thread outlived its pool: a job due in 600s is still queued"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
