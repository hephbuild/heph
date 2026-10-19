//! A count of outstanding background jobs that a waiter can await reaching zero.
//!
//! The process may not exit while sandbox cleanups, remote-cache uploads or
//! history trims are still running, so the CLI backends wait for the count to
//! drain after the app finishes. They used to poll it every 10 ms, which put up
//! to one poll interval on the exit path of every run that had any background
//! work. [`Pending::wait_drained`] wakes on the decrement that reaches zero
//! instead.
//!
//! The counting methods mirror [`AtomicUsize`]'s (`fetch_add` / `fetch_sub` /
//! `load`) so the counter stays a drop-in for the bare atomic it replaced; the
//! only behavioural addition is the wake-up in [`Pending::fetch_sub`].

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Shared handle: the request state, every job and the waiting backend hold one.
pub type PendingCounter = Arc<Pending>;

/// The counter itself. See the module docs.
#[derive(Debug, Default)]
pub struct Pending {
    count: AtomicUsize,
    drained: tokio::sync::Notify,
}

impl Pending {
    /// A new counter at zero, behind an `Arc` so its holders can share it.
    /// Every call returns an independent counter: there is no process-wide
    /// instance, and two calls never see each other's jobs.
    pub fn new_counter() -> PendingCounter {
        Arc::new(Self::default())
    }

    /// Register `n` more outstanding jobs. Returns the previous count.
    pub fn fetch_add(&self, n: usize, order: Ordering) -> usize {
        self.count.fetch_add(n, order)
    }

    /// Retire `n` jobs. Returns the previous count. The decrement that brings
    /// the count to zero wakes every [`Self::wait_drained`] caller.
    pub fn fetch_sub(&self, n: usize, order: Ordering) -> usize {
        let prev = self.count.fetch_sub(n, order);
        if prev == n {
            self.drained.notify_waiters();
        }
        prev
    }

    /// The current count.
    pub fn load(&self, order: Ordering) -> usize {
        self.count.load(order)
    }

    /// Resolve once the count is zero — immediately if it already is.
    ///
    /// Lost-wakeup safe: the waiter registers with the [`Notify`] *before* it
    /// re-reads the count, so a decrement landing between the read and the
    /// await still wakes it. A count that goes back up after reaching zero
    /// (a job registering another) is re-checked, not trusted.
    ///
    /// [`Notify`]: tokio::sync::Notify
    pub async fn wait_drained(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Already drained: no wait at all.
    #[tokio::test]
    async fn resolves_at_once_when_nothing_is_pending() {
        let p = Pending::new_counter();
        tokio::time::timeout(Duration::from_secs(5), p.wait_drained())
            .await
            .expect("an empty counter is drained");
    }

    /// The waiter wakes on the decrement itself, not on a timer. Under paused
    /// time the clock only moves when a timer is the next thing to fire, so a
    /// polling wait would show up as elapsed virtual time; a notified one shows
    /// none.
    #[tokio::test(start_paused = true)]
    async fn wakes_on_the_last_decrement_without_any_timer() {
        let p = Pending::new_counter();
        p.fetch_add(2, Ordering::AcqRel);
        let started = tokio::time::Instant::now();
        let bg = Arc::clone(&p);
        let job = tokio::spawn(async move {
            tokio::task::yield_now().await;
            bg.fetch_sub(1, Ordering::AcqRel);
            tokio::task::yield_now().await;
            bg.fetch_sub(1, Ordering::AcqRel);
        });
        p.wait_drained().await;
        assert_eq!(p.load(Ordering::Acquire), 0);
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "the drain must not have slept on a poll interval"
        );
        job.await.expect("job");
    }

    /// A counter that drained and then rose again blocks a new waiter until it
    /// drains a second time, and that second drain wakes it.
    #[tokio::test(start_paused = true)]
    async fn a_count_rising_again_after_zero_is_waited_out_again() {
        let p = Pending::new_counter();
        p.fetch_add(1, Ordering::AcqRel);
        p.fetch_sub(1, Ordering::AcqRel);
        p.wait_drained().await;

        p.fetch_add(1, Ordering::AcqRel);
        let res = tokio::time::timeout(Duration::from_secs(1), p.wait_drained()).await;
        assert!(res.is_err(), "the count went back up: not drained");

        let started = tokio::time::Instant::now();
        let bg = Arc::clone(&p);
        let job = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            bg.fetch_sub(1, Ordering::AcqRel);
        });
        p.wait_drained().await;
        assert_eq!(started.elapsed(), Duration::from_millis(50));
        job.await.expect("job");
    }

    /// Every concurrent waiter is woken by the one decrement that drains the
    /// count (`notify_waiters`, not a single permit).
    #[tokio::test(start_paused = true)]
    async fn one_drain_wakes_two_concurrent_waiters() {
        let p = Pending::new_counter();
        p.fetch_add(1, Ordering::AcqRel);
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let p = Arc::clone(&p);
                tokio::spawn(async move { p.wait_drained().await })
            })
            .collect();
        // Let both waiters register before the drain.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        p.fetch_sub(1, Ordering::AcqRel);
        for w in waiters {
            tokio::time::timeout(Duration::from_secs(1), w)
                .await
                .expect("each waiter must wake on the drain")
                .expect("waiter task");
        }
    }

    /// A decrement that does not reach zero does not release the waiter.
    #[tokio::test(start_paused = true)]
    async fn a_partial_drain_keeps_waiting() {
        let p = Pending::new_counter();
        p.fetch_add(2, Ordering::AcqRel);
        p.fetch_sub(1, Ordering::AcqRel);
        let res = tokio::time::timeout(Duration::from_secs(1), p.wait_drained()).await;
        assert!(res.is_err(), "one job is still outstanding");
    }
}
