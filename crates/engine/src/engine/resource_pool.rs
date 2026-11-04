//! The execute-admission pool: a target runs only once what it reserves fits
//! in every pool it names, so the sum of what is running fits the machine.
//!
//! # Premise
//!
//! Every acquirer is a spawned task the runtime always polls, and cancellation
//! is an abort whose drop leaves the queue. A future nobody polls and nobody
//! drops does not exist in the engine (task-backed memoizers dissolved it; see
//! the history below), so a grant is never assigned to a waiter that will
//! neither take it nor give it back.
//!
//! History: under the poll-cell memoizer, futures parked in abandoned cells
//! were routine, and a fair queue would hand a released permit to exactly such
//! a waiter — spent, held by nobody, unrecoverable. A wedged build measured 99
//! queued waiters, 21 of them in abandoned chains, against a pool of 12.
//!
//! # Shape
//!
//! Pools are named (`cpu` in micro-CPUs, `memory` in bytes, custom pools a
//! count) and a request may name several; a grant is **atomic** across them,
//! so a target never holds its cpu while it waits for memory. tokio's
//! `acquire_many` cannot grant across semaphores atomically, which is why this
//! is not a set of semaphores.
//!
//! Admission is strict FIFO: the head waits until everything it needs fits,
//! and nothing behind it is admitted meanwhile. A request for more than a
//! pool's capacity is clamped to the capacity (it then runs alone in that
//! pool) and the grant says so ([`Reservation::clamped`]). A request naming a
//! pool whose capacity is 0 is an error, never a wait: such a pool exists so
//! a run can set it ([`AcquireError::ZeroCapacity`]).
//!
//! A request that reserves nothing anywhere (`cpu: 0`) takes 1 of the builtin
//! **light** lane instead, a separate FIFO with capacity
//! `max(8 × cpu capacity in CPUs, 64)`: the number of concurrent executes stays
//! bounded, and a zero-cost target never waits behind a cpu request.
//!
//! # The grant and Drop contract
//!
//! - A grant is a [`Reservation`] built on the release side and sent through
//!   a `oneshot`, so a waiter dropped after its grant was sent drops the
//!   unobserved grant, which returns itself.
//! - **Every** mutation of a lane's free amounts or queue ends by running the
//!   grant chain from the head: a release, an arrival, and a waiter's drop (a
//!   cancelled head admits its follower without waiting for the next release).
//! - The mutex is never held across an await, and an armed `Reservation` is
//!   never dropped under it: a `send` that fails hands the grant back, which
//!   is disarmed and re-added under the same lock.
//! - The arrival fast path (grant without queueing) is taken only when the
//!   queue is empty, or an arrival racing a release would overtake.
//! - Removal from the queue is by key, not a scan.

use hplugin::driver::Resources;
use hplugin::provider::PoolName;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::sync::oneshot;

/// Why a request cannot be admitted. Shown after the target's address:
/// `format!("{addr} {err}")`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AcquireError {
    /// The request names a pool whose capacity is 0: it exists so a run can
    /// set it, and this run did not.
    #[error(
        "acquires {pool}, but pool {pool} has capacity 0 and must be set: --resource {pool}=<n>, HEPH_RESOURCE_{env}=<n> or resources.{pool} in .hephconfig",
        env = pool.as_str().to_ascii_uppercase()
    )]
    ZeroCapacity { pool: PoolName },
    /// The request names a pool the engine does not have. Validation at def
    /// link time normally refuses this first.
    #[error("acquires {pool}, but {pool} is not a pool")]
    UnknownPool { pool: PoolName },
    /// A queued grant was never sent. Cannot happen while the pool is alive;
    /// reported rather than panicking.
    #[error("the resource pool dropped a waiter without granting it")]
    Closed,
}

/// The name of the light lane's one pool. A user may name a pool `light` too:
/// the lane is a separate [`Lane`], so the two never share a grant, and the
/// in-flight report shows this one as `zero-cost`/`slots`.
const LIGHT: &str = "light";

/// Admits targets against per-pool capacities. See the module docs.
#[derive(Debug)]
pub struct ResourcePool {
    main: Arc<Lane>,
    light: Arc<Lane>,
}

impl ResourcePool {
    /// A pool with these capacities (`cpu` in micro-CPUs, `memory` in bytes, a
    /// custom pool a count). A pool given twice keeps the last capacity. The
    /// light lane is sized from the `cpu` capacity (absent counts as 0).
    pub fn new(capacities: impl IntoIterator<Item = (PoolName, u64)>) -> Arc<Self> {
        let capacities: BTreeMap<PoolName, u64> = capacities.into_iter().collect();
        let cpu = capacities.get(PoolName::CPU).copied().unwrap_or(0);
        let light_capacity = cpu
            .div_ceil(Resources::MICROS_PER_CPU)
            .saturating_mul(8)
            .max(64);
        // `light` is a well-formed pool name; the fallback only names the
        // lane's one pool in a snapshot and never decides a grant.
        let light_name = PoolName::try_from(LIGHT).unwrap_or_else(|_| PoolName::cpu());
        Arc::new(Self {
            main: Lane::new(capacities, false),
            light: Lane::new(BTreeMap::from([(light_name, light_capacity)]), true),
        })
    }

    /// The capacity of a named pool, `None` when it is not one.
    pub fn capacity(&self, pool: &str) -> Option<u64> {
        let idx = self.main.index(pool)?;
        self.main.capacity.get(idx).copied()
    }

    /// Every named pool and its capacity, in name order (the light lane is not
    /// a named pool).
    pub fn capacities(&self) -> impl Iterator<Item = (&PoolName, u64)> {
        self.main
            .names
            .iter()
            .zip(self.main.capacity.iter().copied())
    }

    /// The light lane's capacity.
    pub fn light_capacity(&self) -> u64 {
        self.light.capacity.first().copied().unwrap_or(0)
    }

    /// What nobody holds in a named pool right now, `None` when it is not one.
    pub fn available(&self, pool: &str) -> Option<u64> {
        let idx = self.main.index(pool)?;
        self.main.state.lock().free.get(idx).copied()
    }

    /// Ask for `request`'s amounts ([`Resources::amount`]: an undeclared `cpu`
    /// is one CPU). Validates and clamps now, then either grants at once or
    /// queues; await the returned [`Acquire`] for the grant.
    ///
    /// Dropping the `Acquire` leaves the queue, or returns a grant it was
    /// sent but never observed.
    pub fn acquire(self: &Arc<Self>, request: &Resources) -> Result<Acquire, AcquireError> {
        let (need, clamped) = self.resolve(request)?;
        let lane = if need.is_empty() {
            &self.light
        } else {
            &self.main
        };
        let need = if need.is_empty() { vec![(0, 1)] } else { need };
        Ok(lane.enqueue(need.into_boxed_slice(), clamped))
    }

    /// Per pool held/free/capacity, queue lengths, and what the head of each
    /// lane is waiting for.
    pub fn snapshot(&self) -> ResourceSnapshot {
        ResourceSnapshot {
            main: self.main.snapshot(),
            light: self.light.snapshot(),
        }
    }

    /// The request as `(pool index, granted amount)` in the main lane, empty
    /// when it reserves nothing anywhere; and whether any amount was clamped.
    fn resolve(&self, request: &Resources) -> Result<(Vec<(usize, u64)>, bool), AcquireError> {
        let lane = &self.main;
        let default_cpu = request
            .declared(PoolName::CPU)
            .is_none()
            .then(|| (&lane.cpu_name, request.default_cpu()));
        let mut need = Vec::with_capacity(request.len().saturating_add(1));
        let mut clamped = false;
        for (pool, amount) in request.iter().chain(default_cpu) {
            let Some(idx) = lane.index(pool.as_str()) else {
                return Err(AcquireError::UnknownPool { pool: pool.clone() });
            };
            let capacity = lane.capacity.get(idx).copied().unwrap_or(0);
            if capacity == 0 {
                return Err(AcquireError::ZeroCapacity { pool: pool.clone() });
            }
            if amount == 0 {
                continue;
            }
            let granted = amount.min(capacity);
            clamped |= granted < amount;
            need.push((idx, granted));
        }
        Ok((need, clamped))
    }
}

/// Every pool's capacity, from what the config and the run wrote (`specs`)
/// over the engine's defaults: `cpu` is `parallelism` CPUs, `memory` the
/// host's ([`hcore::sysres::host_memory_bytes`], read through `host_memory`
/// only when the memory capacity needs it), a custom pool only what was
/// written. A percentage is of the default; a custom pool has none, which the
/// parser already refused.
pub fn resolve_capacities(
    specs: &BTreeMap<PoolName, hcore::capacity::Capacity>,
    parallelism: usize,
    host_memory: impl FnOnce() -> u64,
) -> Vec<(PoolName, u64)> {
    let cpu_default = u64::try_from(parallelism)
        .unwrap_or(u64::MAX)
        .saturating_mul(Resources::MICROS_PER_CPU);
    let cpu = specs
        .get(PoolName::CPU)
        .map_or(cpu_default, |c| c.resolve(PoolName::CPU, || cpu_default));
    let memory = match specs.get(PoolName::MEMORY) {
        Some(c) => c.resolve(PoolName::MEMORY, host_memory),
        None => host_memory(),
    };
    let custom = specs
        .iter()
        .filter(|(p, _)| p.as_str() != PoolName::CPU && p.as_str() != PoolName::MEMORY)
        .map(|(p, c)| (p.clone(), c.resolve(p.as_str(), || 0)));
    [(PoolName::cpu(), cpu), (PoolName::memory(), memory)]
        .into_iter()
        .chain(custom)
        .collect()
}

/// The engine's [`ResourcePool`], built on first use.
///
/// The pool needs every capacity at construction, and the `memory` default is
/// a host read (cgroup files on Linux, a sysctl on macOS) that a `heph query`
/// should never pay for. So everything that needs no host read — the `cpu`
/// capacity, the set of pool names — is resolved at `Engine::new`, and the pool
/// itself on the first admission.
#[derive(Debug)]
pub struct EnginePools {
    specs: BTreeMap<PoolName, hcore::capacity::Capacity>,
    parallelism: usize,
    cpu: u64,
    pool: std::sync::OnceLock<Arc<ResourcePool>>,
}

impl EnginePools {
    pub fn new(specs: BTreeMap<PoolName, hcore::capacity::Capacity>, parallelism: usize) -> Self {
        let cpu = resolve_capacities(&specs, parallelism, || 0)
            .into_iter()
            .find_map(|(p, c)| (p.as_str() == PoolName::CPU).then_some(c))
            .unwrap_or(0);
        Self {
            specs,
            parallelism,
            cpu,
            pool: std::sync::OnceLock::new(),
        }
    }

    /// The `cpu` capacity, in micro-CPUs.
    pub fn cpu_capacity(&self) -> u64 {
        self.cpu
    }

    /// Whether `name` is a pool: `cpu`, `memory`, or one the config or the run
    /// named (even at capacity 0).
    pub fn is_pool(&self, name: &str) -> bool {
        name == PoolName::CPU || name == PoolName::MEMORY || self.specs.contains_key(name)
    }

    /// Every pool name, in name order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        let builtin = [PoolName::CPU, PoolName::MEMORY]
            .into_iter()
            .filter(|n| !self.specs.contains_key(*n));
        let mut all: Vec<&str> = builtin
            .chain(self.specs.keys().map(PoolName::as_str))
            .collect();
        all.sort_unstable();
        all.into_iter()
    }

    /// The pool, built (and the host memory read) on the first call.
    pub fn get(&self) -> &Arc<ResourcePool> {
        self.pool.get_or_init(|| {
            ResourcePool::new(resolve_capacities(
                &self.specs,
                self.parallelism,
                hcore::sysres::host_memory_bytes,
            ))
        })
    }

    /// The pool if an admission has built it.
    pub fn built(&self) -> Option<&Arc<ResourcePool>> {
        self.pool.get()
    }
}

/// One FIFO over a fixed set of pools. The main lane holds every named pool;
/// the light lane holds one.
#[derive(Debug)]
struct Lane {
    /// Sorted, so a pool is found by binary search; indices are stable.
    names: Box<[PoolName]>,
    capacity: Box<[u64]>,
    cpu_name: PoolName,
    /// The light lane, whose grants reserve no named pool.
    light: bool,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// Indexed like `Lane::names`.
    free: Box<[u64]>,
    /// The queue: arrival sequence → waiter. The first entry is the head;
    /// removal is by key.
    waiters: BTreeMap<u64, Waiter>,
    next_seq: u64,
}

#[derive(Debug)]
struct Waiter {
    need: Box<[(usize, u64)]>,
    clamped: bool,
    since: Instant,
    tx: oneshot::Sender<Reservation>,
}

impl Lane {
    fn new(capacities: BTreeMap<PoolName, u64>, light: bool) -> Arc<Self> {
        let (names, capacity): (Vec<PoolName>, Vec<u64>) = capacities.into_iter().unzip();
        Arc::new(Self {
            state: Mutex::new(State {
                free: capacity.clone().into_boxed_slice(),
                waiters: BTreeMap::new(),
                next_seq: 0,
            }),
            names: names.into_boxed_slice(),
            capacity: capacity.into_boxed_slice(),
            cpu_name: PoolName::cpu(),
            light,
        })
    }

    fn index(&self, pool: &str) -> Option<usize> {
        self.names.binary_search_by(|p| p.as_str().cmp(pool)).ok()
    }

    fn enqueue(self: &Arc<Self>, need: Box<[(usize, u64)]>, clamped: bool) -> Acquire {
        let mut st = self.state.lock();
        if st.waiters.is_empty() && fits(&st.free, &need) {
            take(&mut st.free, &need);
            drop(st);
            return Acquire {
                state: AcquireState::Ready(Some(Reservation {
                    lane: Arc::clone(self),
                    granted: need,
                    clamped,
                    armed: true,
                })),
            };
        }
        let seq = st.next_seq;
        st.next_seq = st.next_seq.wrapping_add(1);
        let (tx, rx) = oneshot::channel();
        st.waiters.insert(
            seq,
            Waiter {
                need,
                clamped,
                since: Instant::now(),
                tx,
            },
        );
        // An arrival is a mutation too. The head did not fit before it, so this
        // grants nothing unless this waiter is the head — kept for the rule.
        self.grant_chain(&mut st);
        drop(st);
        Acquire {
            state: AcquireState::Waiting {
                guard: WaitGuard {
                    lane: Arc::clone(self),
                    seq,
                    armed: true,
                },
                rx,
            },
        }
    }

    /// Grant from the head while the head fits. Runs under the lock; a send
    /// that fails hands the grant back, disarmed, and its amounts are re-added
    /// here, so no armed `Reservation` is dropped under the lock.
    fn grant_chain(self: &Arc<Self>, st: &mut State) {
        while let Some(head) = st.waiters.first_entry() {
            if !fits(&st.free, &head.get().need) {
                break;
            }
            let waiter = head.remove();
            take(&mut st.free, &waiter.need);
            let reservation = Reservation {
                lane: Arc::clone(self),
                granted: waiter.need,
                clamped: waiter.clamped,
                armed: true,
            };
            if let Err(mut unobserved) = waiter.tx.send(reservation) {
                unobserved.armed = false;
                give(&mut st.free, &self.capacity, &unobserved.granted);
            }
        }
    }

    fn release(self: &Arc<Self>, granted: &[(usize, u64)]) {
        let mut st = self.state.lock();
        give(&mut st.free, &self.capacity, granted);
        self.grant_chain(&mut st);
    }

    /// Leave the queue (no-op once granted) and run the chain.
    fn cancel(self: &Arc<Self>, seq: u64) {
        let mut st = self.state.lock();
        if st.waiters.remove(&seq).is_some() {
            self.grant_chain(&mut st);
        }
    }

    fn wait_info(&self, seq: u64) -> Option<WaitInfo> {
        let st = self.state.lock();
        let waiter = st.waiters.get(&seq)?;
        Some(WaitInfo {
            ahead: st.waiters.range(..seq).count(),
            pools: self.needs(&st.free, &waiter.need, false),
            since: waiter.since,
        })
    }

    fn snapshot(&self) -> LaneSnapshot {
        let st = self.state.lock();
        let pools = self
            .names
            .iter()
            .zip(self.capacity.iter().zip(st.free.iter()))
            .map(|(name, (&capacity, &free))| PoolSnapshot {
                pool: name.clone(),
                capacity,
                free,
                held: capacity.saturating_sub(free),
            })
            .collect();
        let head = st.waiters.values().next().map(|w| HeadSnapshot {
            unmet: self.needs(&st.free, &w.need, true),
            since: w.since,
        });
        LaneSnapshot {
            pools,
            queued: st.waiters.len(),
            head,
        }
    }

    /// `need` as named amounts with each pool's free amount; only the pools it
    /// does not fit in when `unmet_only`.
    fn needs(&self, free: &[u64], need: &[(usize, u64)], unmet_only: bool) -> Vec<PoolNeed> {
        need.iter()
            .filter_map(|&(idx, amount)| {
                let free = free.get(idx).copied().unwrap_or(0);
                if unmet_only && amount <= free {
                    return None;
                }
                Some(PoolNeed {
                    pool: self.names.get(idx)?.clone(),
                    need: amount,
                    free,
                })
            })
            .collect()
    }
}

fn fits(free: &[u64], need: &[(usize, u64)]) -> bool {
    need.iter()
        .all(|&(idx, n)| free.get(idx).is_some_and(|&f| n <= f))
}

/// Subtract a need that [`fits`].
fn take(free: &mut [u64], need: &[(usize, u64)]) {
    for &(idx, n) in need {
        if let Some(f) = free.get_mut(idx) {
            *f = f.checked_sub(n).unwrap_or_else(|| {
                tracing::error!(pool = idx, "resource pool took more than was free");
                0
            });
        }
    }
}

/// Return a grant, never past capacity.
fn give(free: &mut [u64], capacity: &[u64], granted: &[(usize, u64)]) {
    for &(idx, n) in granted {
        let cap = capacity.get(idx).copied().unwrap_or(0);
        if let Some(f) = free.get_mut(idx) {
            *f = match f.checked_add(n) {
                Some(v) if v <= cap => v,
                _ => {
                    tracing::error!(pool = idx, "resource pool released more than it granted");
                    cap
                }
            };
        }
    }
}

/// What a target holds while it runs; returns itself to the pool on drop.
#[derive(Debug)]
pub struct Reservation {
    lane: Arc<Lane>,
    granted: Box<[(usize, u64)]>,
    clamped: bool,
    /// False only for a grant handed back by a failed send, whose amounts the
    /// chain re-added itself.
    armed: bool,
}

impl Reservation {
    /// Whether some amount was clamped to its pool's capacity.
    pub fn clamped(&self) -> bool {
        self.clamped
    }

    /// Whether this is a light-lane grant (the request reserved nothing).
    pub fn is_light(&self) -> bool {
        self.lane.light
    }

    /// The granted amount of a named pool; 0 for a pool not reserved (and for
    /// every named pool on a light-lane grant).
    pub fn amount(&self, pool: &str) -> u64 {
        if self.is_light() {
            return 0;
        }
        self.iter()
            .find_map(|(p, n)| (p.as_str() == pool).then_some(n))
            .unwrap_or(0)
    }

    /// The granted amounts of named pools (nothing on a light-lane grant).
    pub fn iter(&self) -> impl Iterator<Item = (&PoolName, u64)> {
        let light = self.is_light();
        self.granted
            .iter()
            .filter(move |_| !light)
            .filter_map(|&(idx, n)| Some((self.lane.names.get(idx)?, n)))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.armed {
            self.lane.release(&self.granted);
        }
    }
}

/// A pending [`ResourcePool::acquire`]. Resolves to the grant; dropping it
/// leaves the queue or returns an unobserved grant.
#[derive(Debug)]
pub struct Acquire {
    state: AcquireState,
}

#[derive(Debug)]
enum AcquireState {
    Ready(Option<Reservation>),
    Waiting {
        // Dropped before `rx`; either order is sound (see `WaitGuard`).
        guard: WaitGuard,
        rx: oneshot::Receiver<Reservation>,
    },
}

impl Acquire {
    /// Where this request stands in its lane, `None` once granted (or when it
    /// was granted on arrival).
    pub fn wait_info(&self) -> Option<WaitInfo> {
        match &self.state {
            AcquireState::Ready(_) => None,
            AcquireState::Waiting { guard, .. } => guard.lane.wait_info(guard.seq),
        }
    }
}

impl Future for Acquire {
    type Output = Result<Reservation, AcquireError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let res = match &mut self.state {
            AcquireState::Ready(r) => return Poll::Ready(r.take().ok_or(AcquireError::Closed)),
            AcquireState::Waiting { guard, rx } => match Pin::new(rx).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(res) => {
                    // Granted (or the sender is gone): nothing left to remove.
                    guard.armed = false;
                    res.map_err(|_closed| AcquireError::Closed)
                }
            },
        };
        // Fused: a completed receiver must not be polled again (it panics), so
        // a poll after this one answers `Closed` from `Ready(None)`.
        self.state = AcquireState::Ready(None);
        Poll::Ready(res)
    }
}

/// Removes a waiter that is dropped before its grant, then runs the chain.
/// If the grant was already sent, the entry is gone and this does nothing; the
/// grant inside the receiver returns itself when the receiver drops.
#[derive(Debug)]
struct WaitGuard {
    lane: Arc<Lane>,
    seq: u64,
    armed: bool,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        if self.armed {
            self.lane.cancel(self.seq);
        }
    }
}

/// A queued request's position: how many wait ahead of it, and per pool what
/// it needs against what is free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitInfo {
    pub ahead: usize,
    pub pools: Vec<PoolNeed>,
    pub since: Instant,
}

/// One pool of a request: what it needs and what is free right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolNeed {
    pub pool: PoolName,
    pub need: u64,
    pub free: u64,
}

/// The pool at one instant, for the in-flight report and the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSnapshot {
    /// Every named pool, in name order.
    pub main: LaneSnapshot,
    /// The light lane: one pool, `light`.
    pub light: LaneSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSnapshot {
    pub pools: Vec<PoolSnapshot>,
    pub queued: usize,
    /// What the head of the queue waits for: only the pools it does not fit.
    pub head: Option<HeadSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolSnapshot {
    pub pool: PoolName,
    pub capacity: u64,
    pub free: u64,
    pub held: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadSnapshot {
    pub unmet: Vec<PoolNeed>,
    pub since: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const CPU: u64 = Resources::MICROS_PER_CPU;
    const GIB: u64 = 1 << 30;

    fn name(s: &str) -> PoolName {
        PoolName::try_from(s).unwrap()
    }

    fn pool(caps: &[(&str, u64)]) -> Arc<ResourcePool> {
        ResourcePool::new(caps.iter().map(|&(n, c)| (name(n), c)))
    }

    fn req(pairs: &[(&str, u64)]) -> Resources {
        Resources::try_from_pairs(pairs.iter().map(|&(n, a)| (n.to_string(), a))).unwrap()
    }

    /// Acquire and expect an immediate grant.
    fn now(p: &Arc<ResourcePool>, r: &Resources) -> Reservation {
        p.acquire(r)
            .unwrap()
            .now_or_never()
            .expect("granted at once")
            .unwrap()
    }

    /// Acquire and expect to queue.
    fn queued(p: &Arc<ResourcePool>, r: &Resources) -> Acquire {
        let mut a = p.acquire(r).unwrap();
        assert!((&mut a).now_or_never().is_none(), "must wait");
        a
    }

    fn granted(a: &mut Acquire) -> Option<Reservation> {
        a.now_or_never().map(Result::unwrap)
    }

    fn free(p: &ResourcePool, pool: &str) -> u64 {
        p.available(pool).unwrap()
    }

    #[test]
    fn grants_in_arrival_order() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let held = now(&p, &req(&[("cpu", 3 * CPU)]));
        let mut big = queued(&p, &req(&[("cpu", 2 * CPU)]));
        // One CPU is free and this asks for one, but it arrived behind `big`.
        let mut small = queued(&p, &req(&[("cpu", CPU)]));
        assert_eq!(free(&p, "cpu"), CPU, "a waiter holds nothing");

        drop(held);
        let big = granted(&mut big).expect("the head first");
        let small = granted(&mut small).expect("then its follower");
        assert_eq!(free(&p, "cpu"), CPU);
        drop((big, small));
        assert_eq!(free(&p, "cpu"), 4 * CPU);
    }

    #[test]
    fn multi_pool_grant_is_atomic() {
        let p = pool(&[("cpu", 2 * CPU), ("memory", 10 * GIB)]);
        let mem = now(&p, &req(&[("cpu", 0), ("memory", 8 * GIB)]));
        let mut both = queued(&p, &req(&[("cpu", CPU), ("memory", 4 * GIB)]));
        assert_eq!(free(&p, "cpu"), 2 * CPU, "cpu fits but is not taken alone");

        drop(mem);
        let both = granted(&mut both).expect("both pools at once");
        assert_eq!(both.amount("cpu"), CPU);
        assert_eq!(both.amount("memory"), 4 * GIB);
        assert_eq!(free(&p, "cpu"), CPU);
        assert_eq!(free(&p, "memory"), 6 * GIB);
    }

    #[test]
    fn oversize_request_is_clamped_and_runs_alone() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let held = now(&p, &req(&[]));
        let mut big = queued(&p, &req(&[("cpu", 16 * CPU)]));
        drop(held);
        let big = granted(&mut big).expect("an empty pool fits the clamped request");
        assert!(big.clamped());
        assert_eq!(big.amount("cpu"), 4 * CPU);
        let mut next = queued(&p, &req(&[]));
        drop(big);
        assert!(
            !granted(&mut next)
                .expect("after the oversize one")
                .clamped()
        );
    }

    #[test]
    fn oversize_memory_request_is_clamped_and_runs_alone() {
        let p = pool(&[("cpu", 4 * CPU), ("memory", GIB)]);
        let held = now(&p, &req(&[("cpu", 0), ("memory", 1)]));
        let mut big = queued(&p, &req(&[("cpu", 0), ("memory", 2 * GIB)]));
        drop(held);
        let big = granted(&mut big).unwrap();
        assert!(big.clamped());
        assert_eq!(big.amount("memory"), GIB);
        assert_eq!(free(&p, "memory"), 0);
    }

    #[test]
    fn oversize_mixed_request_is_clamped_in_the_oversize_pool_only() {
        let p = pool(&[("cpu", 4 * CPU), ("memory", GIB)]);
        let r = now(&p, &req(&[("cpu", 16 * CPU), ("memory", GIB / 2)]));
        assert!(r.clamped());
        assert_eq!(r.amount("cpu"), 4 * CPU);
        assert_eq!(r.amount("memory"), GIB / 2);
        let fits = now(&p, &req(&[("cpu", 0), ("memory", GIB / 2)]));
        assert!(!fits.clamped());
    }

    #[test]
    fn fractional_requests_pack_into_the_pool() {
        let p = pool(&[("cpu", CPU)]);
        let quarter = req(&[("cpu", CPU / 4)]);
        let mut four: Vec<_> = (0..4).map(|_| now(&p, &quarter)).collect();
        let mut fifth = queued(&p, &quarter);
        drop(four.pop());
        assert_eq!(four.len(), 3, "exactly one released");
        let _fifth = granted(&mut fifth).expect("one release admits the fifth");
        assert_eq!(free(&p, "cpu"), 0, "three held plus the fifth");
    }

    /// A queued `Acquire` polled again after its grant answers `Closed`
    /// rather than polling a spent receiver (which panics).
    #[test]
    fn a_resolved_acquire_is_fused() {
        let p = pool(&[("cpu", CPU)]);
        let held = now(&p, &req(&[]));
        let mut waiting = queued(&p, &req(&[]));
        drop(held);
        let grant = granted(&mut waiting).expect("granted on release");
        assert!(matches!(
            (&mut waiting).now_or_never(),
            Some(Err(AcquireError::Closed))
        ));
        drop(grant);
        assert_eq!(free(&p, "cpu"), CPU);
    }

    /// The light lane is bounded at its capacity: `cap` grants are taken at
    /// once, the next waits, and one release admits it.
    #[test]
    fn the_light_lane_queues_past_its_capacity() {
        let p = pool(&[("cpu", CPU)]);
        let cap = p.light_capacity();
        assert_eq!(cap, 64, "max(8 × 1, 64)");
        let zero = req(&[("cpu", 0)]);
        let mut held: Vec<_> = (0..cap).map(|_| now(&p, &zero)).collect();
        assert!(held.iter().all(Reservation::is_light));
        let mut next = queued(&p, &zero);
        assert_eq!(p.snapshot().light.queued, 1);
        drop(held.pop());
        assert!(granted(&mut next).unwrap().is_light());
        assert_eq!(free(&p, "cpu"), CPU, "the light lane never touches cpu");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zero_cost_requests_use_the_light_pool_and_are_bounded() {
        let p = pool(&[("cpu", CPU)]);
        let cap = p.light_capacity();
        assert_eq!(cap, 64, "max(8 × 1, 64)");
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..10 * cap {
            let (p, in_flight, peak) = (Arc::clone(&p), Arc::clone(&in_flight), Arc::clone(&peak));
            tasks.spawn(async move {
                let r = p.acquire(&req(&[("cpu", 0)])).unwrap().await.unwrap();
                assert!(r.is_light());
                assert_eq!(r.amount("cpu"), 0);
                let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                // Hold the grant until the lane fills, so the bound is
                // reached rather than merely not exceeded (bounded wait).
                for _ in 0..1_000 {
                    if peak.load(Ordering::SeqCst) >= usize::try_from(cap).unwrap() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                in_flight.fetch_sub(1, Ordering::SeqCst);
                drop(r);
            });
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(r) = tasks.join_next().await {
                r.unwrap();
            }
        })
        .await
        .expect("light waiters starved");
        let peak = u64::try_from(peak.load(Ordering::SeqCst)).unwrap();
        assert_eq!(peak, cap, "the lane filled exactly to its capacity");
        assert_eq!(free(&p, "cpu"), CPU, "the light lane never touches cpu");
        assert_eq!(p.snapshot().light.pools[0].free, cap);
    }

    #[test]
    fn zero_cost_requests_never_wait_for_cpu() {
        let p = pool(&[("cpu", CPU), ("memory", 8 * GIB)]);
        let held = now(&p, &req(&[]));
        let mut waiting_for_cpu = queued(&p, &req(&[]));
        // The cpu lane is blocked at its head; a zero-cost request is not in it.
        let light = now(&p, &req(&[("cpu", 0)]));
        assert!(light.is_light());
        drop(held);
        drop(granted(&mut waiting_for_cpu).unwrap());

        // `cpu: 0` with memory is not zero-cost: it queues on memory.
        let mem = now(&p, &req(&[("cpu", 0), ("memory", 8 * GIB)]));
        assert!(!mem.is_light());
        let mut more = queued(&p, &req(&[("cpu", 0), ("memory", 8 * GIB)]));
        drop(mem);
        assert!(!granted(&mut more).unwrap().is_light());
    }

    #[test]
    fn cancelling_the_head_admits_the_follower_without_a_release() {
        let p = pool(&[("cpu", 2 * CPU)]);
        let _held = now(&p, &req(&[]));
        let head = queued(&p, &req(&[("cpu", 2 * CPU)]));
        let mut follower = queued(&p, &req(&[]));
        drop(head);
        assert!(
            granted(&mut follower).is_some(),
            "the head's drop runs the chain"
        );
    }

    #[test]
    fn a_dropped_unobserved_grant_returns_itself() {
        let p = pool(&[("cpu", CPU)]);
        let held = now(&p, &req(&[]));
        let a = queued(&p, &req(&[]));
        let mut b = queued(&p, &req(&[]));
        drop(held);
        // `a` was sent its grant but never polled again: it holds the CPU.
        assert_eq!(free(&p, "cpu"), 0);
        assert!(a.wait_info().is_none(), "no longer queued");
        drop(a);
        assert!(granted(&mut b).is_some(), "the unobserved grant came back");
    }

    /// Dropping a waiter mid-wait leaves the pool untouched.
    #[test]
    fn a_cancelled_waiter_leaves_no_trace() {
        let p = pool(&[("cpu", CPU)]);
        let held = now(&p, &req(&[]));
        let cancelled = queued(&p, &req(&[]));
        drop(cancelled);
        assert_eq!(p.snapshot().main.queued, 0);
        drop(held);
        assert_eq!(free(&p, "cpu"), CPU);
    }

    /// With task-backed memoizers, cancellation is abort-based and teardown is
    /// asynchronous: the reservation comes back when the runtime drops the
    /// aborted chain. So the assertions are eventual (bounded), plus the
    /// property that matters to a user — a live caller is served while the
    /// abandoned chain drains.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_task_memoized_chain_returns_its_reservation() {
        use hcore::hmemoizer::Memoizer;

        let handle = tokio::runtime::Handle::current();
        let p = pool(&[("cpu", CPU)]);
        let mem_result: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag_task(
            "rp-task-repro-result",
            handle.clone(),
        ));
        let mem_locked: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag_task(
            "rp-task-repro-locked_result",
            handle.clone(),
        ));
        let mem_execute: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag_task(
            "rp-task-repro-execute_cache",
            handle,
        ));

        let mut outer = Box::pin(mem_result.process("//pkg:tgt".to_string(), {
            let (mem_locked, mem_execute) = (Arc::clone(&mem_locked), Arc::clone(&mem_execute));
            let p = Arc::clone(&p);
            move || async move {
                mem_locked
                    .process("//pkg:tgt".to_string(), move || async move {
                        mem_execute
                            .process("//pkg:tgt".to_string(), move || async move {
                                let _r = p.acquire(&Resources::new()).unwrap().await.unwrap();
                                futures::future::pending::<u32>().await
                            })
                            .await
                    })
                    .await
            }
        }));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while free(&p, "cpu") != 0 {
            assert!(futures::poll!(&mut outer).is_pending());
            assert!(
                tokio::time::Instant::now() < deadline,
                "the leaf never acquired"
            );
            tokio::task::yield_now().await;
        }

        drop(outer);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while free(&p, "cpu") != CPU {
            assert!(
                tokio::time::Instant::now() < deadline,
                "abandoning the chain must (eventually) return the reservation"
            );
            tokio::task::yield_now().await;
        }

        let live = tokio::time::timeout(
            Duration::from_secs(5),
            p.acquire(&Resources::new()).unwrap(),
        )
        .await
        .expect("a live caller must not be blocked by the abandoned chain")
        .unwrap();
        drop(live);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_waiters_all_make_progress() {
        let p = pool(&[("cpu", 2 * CPU)]);
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let p = Arc::clone(&p);
            tasks.spawn(async move {
                let r = p.acquire(&Resources::new()).unwrap().await.unwrap();
                tokio::task::yield_now().await;
                drop(r);
            });
        }
        let done = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(r) = tasks.join_next().await {
                r.unwrap();
            }
        })
        .await;
        assert!(done.is_ok(), "waiters starved");
        assert_eq!(free(&p, "cpu"), 2 * CPU);
    }

    #[test]
    fn cancelling_n_waiters_leaves_the_rest_in_order() {
        let p = pool(&[("cpu", CPU)]);
        let held = now(&p, &req(&[]));
        let mut waiters: Vec<(usize, Acquire)> =
            (0..10).map(|i| (i, queued(&p, &req(&[])))).collect();
        // Cancel every odd one, from the middle of the queue.
        waiters.retain(|(i, _)| i % 2 == 0);
        assert_eq!(p.snapshot().main.queued, 5);
        for (pos, (_, a)) in waiters.iter().enumerate() {
            assert_eq!(a.wait_info().unwrap().ahead, pos);
        }

        let mut current = held;
        let mut order = Vec::new();
        for (i, mut a) in waiters {
            drop(current);
            current = granted(&mut a).expect("the next survivor, in order");
            order.push(i);
        }
        assert_eq!(order, [0, 2, 4, 6, 8]);
    }

    #[test]
    fn amounts_at_u64_max_do_not_wrap() {
        let p = pool(&[("cpu", u64::MAX), ("tokens", u64::MAX)]);
        assert_eq!(p.light_capacity(), (u64::MAX / CPU + 1) * 8);
        let all = now(&p, &req(&[("cpu", u64::MAX), ("tokens", u64::MAX)]));
        assert!(!all.clamped());
        let mut one = queued(&p, &req(&[("tokens", 1)]));
        drop(all);
        let one = granted(&mut one).unwrap();
        assert_eq!(free(&p, "tokens"), u64::MAX - 1);
        drop(one);
        assert_eq!(free(&p, "tokens"), u64::MAX);
        assert_eq!(free(&p, "cpu"), u64::MAX);

        // Clamped from u64::MAX to a small capacity.
        let small = pool(&[("cpu", 4 * CPU)]);
        let r = now(&small, &req(&[("cpu", u64::MAX)]));
        assert!(r.clamped());
        assert_eq!(r.amount("cpu"), 4 * CPU);
    }

    #[test]
    fn snapshot_reports_held_free_and_head() {
        let p = pool(&[("cpu", 4 * CPU), ("memory", 8 * GIB)]);
        let idle = p.snapshot();
        assert_eq!(idle.main.queued, 0);
        assert!(idle.main.head.is_none());

        let _held = now(&p, &req(&[("cpu", 3 * CPU), ("memory", GIB)]));
        let head = queued(&p, &req(&[("cpu", 2 * CPU), ("memory", GIB)]));
        let follower = queued(&p, &req(&[]));
        let s = p.snapshot();
        assert_eq!(
            s.main.pools,
            [
                PoolSnapshot {
                    pool: name("cpu"),
                    capacity: 4 * CPU,
                    free: CPU,
                    held: 3 * CPU,
                },
                PoolSnapshot {
                    pool: name("memory"),
                    capacity: 8 * GIB,
                    free: 7 * GIB,
                    held: GIB,
                },
            ]
        );
        assert_eq!(s.main.queued, 2);
        let h = s.main.head.unwrap();
        assert_eq!(
            h.unmet,
            [PoolNeed {
                pool: name("cpu"),
                need: 2 * CPU,
                free: CPU,
            }],
            "only the pool it does not fit"
        );
        let info = head.wait_info().unwrap();
        assert_eq!(info.ahead, 0);
        assert_eq!(info.since, h.since);
        assert_eq!(info.pools.len(), 2);
        assert_eq!(follower.wait_info().unwrap().ahead, 1);
        assert_eq!(s.light.queued, 0);
        assert_eq!(s.light.pools[0].pool, name("light"));
    }

    #[test]
    fn a_zero_capacity_pool_is_an_error_not_a_wait() {
        let p = pool(&[("cpu", 4 * CPU), ("gpu", 0)]);
        for amount in [1, 0] {
            let err = p.acquire(&req(&[("gpu", amount)])).unwrap_err();
            assert_eq!(err, AcquireError::ZeroCapacity { pool: name("gpu") });
            assert_eq!(
                format!("//p:t {err}"),
                "//p:t acquires gpu, but pool gpu has capacity 0 and must be set: \
                 --resource gpu=<n>, HEPH_RESOURCE_GPU=<n> or resources.gpu in .hephconfig"
            );
        }
        assert_eq!(p.snapshot().main.queued, 0, "refused before queueing");

        let err = p.acquire(&req(&[("docker", 1)])).unwrap_err();
        assert_eq!(
            err,
            AcquireError::UnknownPool {
                pool: name("docker")
            }
        );

        // A cpu capacity of 0 refuses even the default one-CPU request.
        let none = pool(&[("cpu", 0)]);
        assert_eq!(
            none.acquire(&Resources::new()).unwrap_err(),
            AcquireError::ZeroCapacity { pool: name("cpu") }
        );
    }

    /// A parse request for `//p:<driver>` on `driver`: `config`, plus
    /// `resources` when declared.
    fn parse_req(
        driver: &str,
        config: &[(&str, hcore::htvalue::Value)],
        resources: Option<&hcore::htvalue::Value>,
    ) -> hplugin::driver::ParseRequest {
        let mut config: std::collections::HashMap<_, _> = config
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        if let Some(r) = resources {
            config.insert(Resources::CONFIG_KEY.to_string(), r.clone());
        }
        hplugin::driver::ParseRequest {
            request_id: "t".to_string(),
            target_spec: Arc::new(hplugin::provider::TargetSpec {
                addr: hmodel::htaddr::parse_addr(&format!("//p:{driver}")).unwrap(),
                driver: driver.to_string(),
                config,
                ..Default::default()
            }),
        }
    }

    /// The resources on the def of a target of each driver the spec names
    /// (`exec`, `template`, `fs`, `group`), parsed by the real in-tree drivers
    /// with `resources` set to `declared`.
    async fn driver_defs(
        declared: Option<hcore::htvalue::Value>,
    ) -> Vec<(&'static str, Resources)> {
        use hcore::htvalue::Value;
        use hdriver_support::driver_managed::ManagedDriver as _;
        use hplugin::driver::Driver as _;
        let s = |v: &str| Value::String(v.to_string());
        let d = declared.as_ref();
        let ct = hcore::hasync::StdCancellationToken::new();
        let exec = hplugin_exec::pluginexec::Driver::new_exec()
            .parse(parse_req("exec", &[("run", s("true"))], d), &ct)
            .await;
        let template = hbuiltins::plugintemplate::Driver
            .parse(parse_req("template", &[("src", s(":t.j2"))], d), &ct)
            .await;
        let fs = hbuiltins::pluginfs::Driver::default()
            .parse(parse_req("fs", &[("f", s("a.txt"))], d), &ct)
            .await;
        let group = hbuiltins::plugingroup::Driver
            .parse(
                parse_req("group", &[("deps", Value::List(vec![s(":a")]))], d),
                &ct,
            )
            .await;
        [
            ("exec", exec),
            ("template", template),
            ("fs", fs),
            ("group", group),
        ]
        .into_iter()
        .map(|(name, res)| {
            let def = res.unwrap_or_else(|e| panic!("parse {name}: {e:#}"));
            (name, def.target_def.resources)
        })
        .collect()
    }

    /// D3: a def that says nothing reserves 1 cpu (exec, template), except
    /// `fs` and `group`, which lower their default to 10 micro-CPUs.
    #[tokio::test]
    async fn a_def_without_resources_costs_one_cpu() {
        let p = pool(&[("cpu", 4 * CPU)]);
        for (driver, resources) in driver_defs(None).await {
            let want = match driver {
                "fs" | "group" => Resources::VIEW_CPU_MICROS,
                _ => CPU,
            };
            assert_eq!(resources.amount("cpu"), want, "{driver}");
            let r = now(&p, &resources);
            assert!(!r.is_light(), "{driver}");
            assert_eq!(r.amount("cpu"), want, "{driver}");
        }
    }

    /// D3, D5: a declared `cpu` replaces every driver's default, and `cpu: 0`
    /// takes the light lane, reserving nothing from `cpu`.
    #[tokio::test]
    async fn declared_resources_override_the_default() {
        use hcore::htvalue::Value;
        let cpu = |v: Value| Some(Value::Map([("cpu".to_string(), v)].into()));
        let p = pool(&[("cpu", 4 * CPU)]);
        for (driver, resources) in driver_defs(cpu(Value::Int(2))).await {
            let r = now(&p, &resources);
            assert_eq!(r.amount("cpu"), 2 * CPU, "{driver}");
        }
        for (driver, resources) in driver_defs(cpu(Value::Int(0))).await {
            assert_eq!(resources.declared("cpu"), Some(0), "{driver}");
            let r = now(&p, &resources);
            assert!(r.is_light(), "{driver}: cpu 0 is the light lane");
            assert_eq!(free(&p, "cpu"), 4 * CPU, "{driver}: holds no cpu");
        }
    }
}
