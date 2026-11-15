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
//!
//! What it costs the pool: one atomic increment per locked change, and a
//! snapshot (one lock, one pass over the pools and the waiting classes) at
//! most once per tick per request. Admission never reads the generation.

use super::{Limit, ResourcePool};
use crate::engine::event::BuildEventKind;
use hplugin::provider::PoolName;
use parking_lot::Mutex;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

/// How often a request looks at the pool.
pub(crate) const TICK: Duration = Duration::from_millis(250);

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
        // (again) on the next call. See `Lane::generation` for the ordering.
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
    /// send, if anything did. Called once, before the request's stream ends.
    pub(crate) fn finish(&self) -> Vec<BuildEventKind> {
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
        self.sample()
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
            assert!(!watched.main.state.is_locked(), "sent under the pool lock");
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
}
