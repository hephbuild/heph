//! The per-request `PoolUsage` sampler.
//!
//! Nothing is sent from inside the pool: a release runs in a
//! [`Reservation`](super::Reservation)'s drop under the pool's lock, and the
//! pool is engine-wide while events belong to a request. Instead every change
//! moves [`ResourcePool::generation`], and a request that has admitted
//! something samples it on a [`TICK`]: when it moved since the last send, one
//! [`snapshot`](ResourcePool::snapshot) becomes one `PoolUsage` per bounded
//! pool. The request's last sample is taken when it ends
//! ([`PoolSampler::finish`]), so the last change after a quiet spell is never
//! lost, and a request that admitted nothing never starts a sampler at all.
//! A run's `sandbox` slot outlives the request until its sandbox is removed in
//! the background, so a request that ends with removals pending keeps its
//! stream open while they return their slots ([`PoolSampler::trail`]),
//! bounded by [`REMOVALS_WAIT`].
//!
//! What it costs the pool: one atomic increment per locked change, and a
//! snapshot (one lock, one pass over the pools; the waiting counts are kept
//! as waiters join and leave, so it does not grow with the queue) at
//! most once per tick per request. Admission never reads the generation.

use super::{Limit, ResourcePool};
use crate::engine::event::BuildEventKind;
use hcore::pending::Pending;
use hplugin::provider::PoolName;
use parking_lot::Mutex;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

/// How often a request looks at the pool.
pub(crate) const TICK: Duration = Duration::from_millis(250);

/// How long a request's last report waits, after the request ends, for its
/// sandbox removals to return their slots ([`PoolSampler::trail`]).
pub(crate) const REMOVALS_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(crate) struct PoolSampler {
    pool: Arc<ResourcePool>,
    /// The generation the last send showed; at first, the one found when the
    /// sampler was made, so the change that made it is the first one sent.
    sent: Mutex<u64>,
    task: Mutex<Option<tokio::task::AbortHandle>>,
}

impl PoolSampler {
    /// A sampler that has sent nothing, made before the change that warrants
    /// it (the request's first admission) so that change is seen as new.
    pub(crate) fn new(pool: Arc<ResourcePool>) -> Arc<Self> {
        let sent = pool.generation();
        Arc::new(Self {
            pool,
            sent: Mutex::new(sent),
            task: Mutex::new(None),
        })
    }

    /// One `PoolUsage` per bounded pool when the pool changed since the last
    /// call that returned any, else nothing. The pool's lock is taken only
    /// inside `snapshot()` and is released before this returns, so a caller
    /// sends what it gets with no pool lock held.
    pub(crate) fn sample(&self) -> Vec<BuildEventKind> {
        let mut sent = self.sent.lock();
        // Read before the snapshot: a change after this load is either in the
        // snapshot or moves the generation past what is recorded, and is sent
        // (again) on the next call. The `generation` field of `ResourcePool`
        // documents the ordering.
        let generation = self.pool.generation();
        if generation == *sent {
            return Vec::new();
        }
        *sent = generation;
        let snap = self.pool.snapshot().main;
        let removals = snap.removals_pending;
        snap.pools
            .into_iter()
            .filter_map(|p| {
                let Limit::Bounded(capacity) = p.capacity else {
                    return None;
                };
                let removals_pending = if p.pool.as_str() == PoolName::SANDBOX {
                    removals
                } else {
                    0
                };
                Some(BuildEventKind::PoolUsage {
                    pool: p.pool.as_str().to_owned(),
                    held: p.held,
                    capacity,
                    waiting: p.waiting,
                    removals_pending,
                })
            })
            .collect()
    }

    /// Call `tick` with this sampler every [`TICK`], the first one [`TICK`]
    /// from now, until it breaks or [`finish`](Self::finish) stops it.
    /// `tick` decides whether anyone still listens before it samples.
    pub(crate) fn spawn(
        self: &Arc<Self>,
        mut tick: impl FnMut(&Self) -> ControlFlow<()> + Send + 'static,
    ) {
        let this = Arc::clone(self);
        // Taken now, not when the task first runs.
        let start = tokio::time::Instant::now() + TICK;
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(start, TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if tick(&this).is_break() {
                    break;
                }
            }
        });
        *self.task.lock() = Some(task.abort_handle());
    }

    /// Stop the ticks and take the last sample: what changed since the last
    /// send, if anything did. Called once, when the request ends.
    pub(crate) fn finish(&self) -> Vec<BuildEventKind> {
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
        self.sample()
    }

    /// After [`finish`](Self::finish), while `removals` — the request's
    /// removals still holding a `sandbox` slot — is above zero: hand `send`
    /// what changed every [`TICK`], and once more when it drains, for at most
    /// `bound`. A removal still pending at the bound leaves its slot reported
    /// held, which is what the pool holds.
    pub(crate) async fn trail(
        &self,
        removals: &Pending,
        bound: Duration,
        mut send: impl FnMut(Vec<BuildEventKind>),
    ) {
        let deadline = tokio::time::Instant::now() + bound;
        loop {
            let wake = (tokio::time::Instant::now() + TICK).min(deadline);
            // A removal releases its slot before it leaves the count, so a
            // sample after the drain sees every slot back.
            let drained = tokio::time::timeout_at(wake, removals.wait_drained())
                .await
                .is_ok();
            let events = self.sample();
            if !events.is_empty() {
                send(events);
            }
            if drained || tokio::time::Instant::now() >= deadline {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::resource_pool::Reservation;
    use futures::FutureExt as _;
    use hplugin::driver::Demand;

    const CPU: u64 = 1_000_000;

    fn pool() -> Arc<ResourcePool> {
        ResourcePool::new([
            (PoolName::cpu(), Limit::Bounded(4 * CPU)),
            (PoolName::sandbox(), Limit::Bounded(8)),
            (
                PoolName::try_from("tokens").expect("name"),
                Limit::Unbounded,
            ),
        ])
    }

    fn cpus(n: u64) -> Demand {
        Demand::try_from_pairs([("cpu".to_string(), n * CPU)]).expect("demand")
    }

    fn now(pool: &Arc<ResourcePool>, d: &Demand, implicit: &[(PoolName, u64)]) -> Reservation {
        pool.acquire_with(d, implicit)
            .expect("acquire")
            .now_or_never()
            .expect("granted at once")
            .expect("granted")
    }

    fn held(events: &[BuildEventKind], name: &str) -> Option<(u64, u64, u64)> {
        events.iter().find_map(|e| match e {
            BuildEventKind::PoolUsage {
                pool,
                held,
                waiting,
                removals_pending,
                ..
            } if pool == name => Some((*held, *waiting, *removals_pending)),
            _ => None,
        })
    }

    /// Spec test `pool_usage_sampler_coalesces_and_trails`: a burst inside
    /// one tick is one event per bounded pool carrying the state after it; a
    /// last release with nothing after it arrives on the next tick; the
    /// request's end sends one more only if something changed since; and no
    /// event leaves while the pool's lock is held.
    #[tokio::test(start_paused = true)]
    async fn pool_usage_sampler_coalesces_and_trails() {
        let pool = pool();
        let sampler = PoolSampler::new(Arc::clone(&pool));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<BuildEventKind>>();
        let watched = Arc::clone(&pool);
        sampler.spawn(move |s| {
            let events = s.sample();
            // What `sample` returned is sent with the pool's lock free.
            assert!(!watched.state.is_locked(), "sent under the pool lock");
            if !events.is_empty() && tx.send(events).is_err() {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });

        // A burst within one tick: three grants, one release, one waiter.
        let a = now(&pool, &cpus(1), &[]);
        let b = now(&pool, &cpus(2), &[]);
        drop(a);
        let c = now(&pool, &cpus(1), &[]);
        let mut queued = pool.acquire(&cpus(4)).expect("d");
        assert!((&mut queued).now_or_never().is_none(), "d waits");
        tokio::time::advance(TICK).await;
        let burst = rx.recv().await.expect("one send for the burst");
        // One event per bounded pool, the unbounded one left out.
        assert_eq!(burst.len(), 2, "{burst:?}");
        assert_eq!(held(&burst, "cpu"), Some((3 * CPU, 1, 0)));
        assert_eq!(held(&burst, "sandbox"), Some((0, 0, 0)));
        assert_eq!(held(&burst, "tokens"), None);

        // A quiet tick sends nothing.
        tokio::time::advance(TICK).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "a quiet tick sends nothing");

        // A last change with nothing after it: the waiter gives up, then
        // both grants return. It still arrives on the next tick.
        drop(queued);
        drop((b, c));
        tokio::time::advance(TICK).await;
        let trail = rx.recv().await.expect("the trailing change");
        assert_eq!(held(&trail, "cpu"), Some((0, 0, 0)));

        // Nothing moved since: the end sends nothing.
        assert!(sampler.finish().is_empty(), "end with nothing new");

        // A change after the last tick is the end's to send.
        let sampler = PoolSampler::new(Arc::clone(&pool));
        let d = now(&pool, &cpus(1), &[]);
        let end = sampler.finish();
        assert_eq!(held(&end, "cpu"), Some((CPU, 0, 0)));
        drop(d);
    }

    /// The sandbox pool reports what it holds itself: a slot held for a
    /// pending removal stays held after the rest of the grant returns, with
    /// the removal counted, until the removal drops.
    #[test]
    fn sandbox_held_by_a_pending_removal() {
        let pool = pool();
        let sampler = PoolSampler::new(Arc::clone(&pool));
        let mut grant = now(&pool, &cpus(1), &[(PoolName::sandbox(), 1)]);
        let slot = grant.split_off(PoolName::SANDBOX).expect("a sandbox slot");
        drop(grant);
        let removal = slot.into_removal();
        let during = sampler.sample();
        assert_eq!(held(&during, "sandbox"), Some((1, 0, 1)));
        assert_eq!(held(&during, "cpu"), Some((0, 0, 0)));
        drop(removal);
        let after = sampler.sample();
        assert_eq!(held(&after, "sandbox"), Some((0, 0, 0)));
        assert!(sampler.sample().is_empty(), "nothing moved since");
    }

    /// A slot pending removal, counted in `removals` as the sandbox cleaner
    /// counts it.
    fn pending_removal(
        pool: &Arc<ResourcePool>,
        removals: &Pending,
    ) -> super::super::PendingRemoval {
        let mut grant = now(pool, &cpus(1), &[(PoolName::sandbox(), 1)]);
        let slot = grant.split_off(PoolName::SANDBOX).expect("a sandbox slot");
        drop(grant);
        removals.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        slot.into_removal()
    }

    /// The request ended with a removal pending: its last reports follow the
    /// removal, and the one after it returns shows the slot free; the wait
    /// ends with the drain, not at the bound.
    #[tokio::test(start_paused = true)]
    async fn trail_reports_the_slot_a_removal_returns() {
        let pool = pool();
        let removals = Pending::new_counter();
        let sampler = PoolSampler::new(Arc::clone(&pool));
        let removal = pending_removal(&pool, &removals);
        let end = sampler.finish();
        assert_eq!(held(&end, "sandbox"), Some((1, 0, 1)), "held at the end");

        let start = tokio::time::Instant::now();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let trail = tokio::spawn({
            let sampler = Arc::clone(&sampler);
            let removals = Arc::clone(&removals);
            async move {
                sampler
                    .trail(&removals, REMOVALS_WAIT, |e| drop(tx.send(e)))
                    .await;
            }
        });
        tokio::time::sleep(3 * TICK).await;
        // Slot first, then the count, as `CountedRemoval` drops.
        drop(removal);
        removals.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        trail.await.expect("trail");
        assert!(
            start.elapsed() < REMOVALS_WAIT,
            "ended with the drain: {:?}",
            start.elapsed()
        );
        let mut last = None;
        while let Ok(events) = rx.try_recv() {
            last = held(&events, "sandbox");
        }
        assert_eq!(last, Some((0, 0, 0)), "the last report shows it free");
    }

    /// A removal stuck past the bound: the wait ends at the bound, and what
    /// was last reported is the slot still held.
    #[tokio::test(start_paused = true)]
    async fn trail_ends_at_its_bound() {
        let pool = pool();
        let removals = Pending::new_counter();
        let sampler = PoolSampler::new(Arc::clone(&pool));
        let _stuck = pending_removal(&pool, &removals);
        let end = sampler.finish();
        assert_eq!(held(&end, "sandbox"), Some((1, 0, 1)));

        let start = tokio::time::Instant::now();
        let mut sent = Vec::new();
        sampler
            .trail(&removals, REMOVALS_WAIT, |e| sent.push(e))
            .await;
        assert_eq!(start.elapsed(), REMOVALS_WAIT, "ended at the bound");
        assert!(sent.is_empty(), "nothing moved, nothing re-sent: {sent:?}");
    }
}
