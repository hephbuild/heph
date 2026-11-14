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
//! a run can set it ([`AcquireError::ZeroCapacity`]). A pool with no capacity
//! at all ([`Limit::Unbounded`]: a custom pool declared without a value,
//! `memory` when the host could not be read) always fits: what is granted from
//! it is counted, never waited for.
//!
//! A request that needs nothing of a bounded pool — 0 of every pool (`cpu: 0`
//! and nothing else), or non-zero amounts of unbounded pools only — is granted
//! at once and never queues: it always fits, so making it wait behind the head
//! would only delay it. Nothing caps how many such grants are live; a
//! zero-cost target is limited by the runtime and the OS alone. Its unbounded
//! amounts are still counted as held, for the snapshot.
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
//! - A grant can be split ([`Reservation::split_off`]); each half returns its
//!   own amounts when it drops. A release is synchronous (the mutex and a
//!   `oneshot` send) and needs no runtime, so a half dropped on a plain OS
//!   thread — the reclaim lane's — wakes a queued tokio waiter like any other
//!   release.
//!
//! # The `sandbox` pool
//!
//! Admission bounds what *runs*; a finished run's sandbox is removed later,
//! fire-and-forget, on the reclaim lane (`sandbox_cleaner`). A build of many
//! short targets (thousands of sandboxed `go list` runs in a cold Go
//! `validate`) finishes sandboxes faster than they are removed, and nothing
//! bounded that backlog: it filled a disk. So the engine adds `sandbox: 1` to
//! every managed-driver run's request ([`ResourcePool::acquire_with`]),
//! admitted atomically with its cpu, and splits that part off the grant: the
//! cpu returns when the run ends, the sandbox only once the reclaim lane has
//! removed the directory. Live plus pending-removal sandboxes number at most
//! the pool's capacity ([`SANDBOXES_PER_CPU`] per CPU by default).
//!
//! It is the engine's ([`is_engine_owned`]): no target declares it or reads it
//! with `${resource:sandbox}`, and like every pool nothing about it reaches a
//! hash, argv, env or file. A `cpu: 0` managed run needs something of a
//! bounded pool now, so it queues like any other: that is the point (it is
//! exactly the run that piled sandboxes up). Outside the bound: cache hits,
//! non-managed drivers and invocation sandboxes take none, and a failed run's
//! sandbox kept for its diagnostic gives its slot back at once (one tree per
//! failed addr, removed by that addr's next run).

use hcore::capacity::{PoolSpec, Unit};
use hplugin::driver::{Demand, Resources};
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
        "acquires {pool}, but pool {pool} has capacity 0 and must be set: --resource {pool}=<n>, HEPH_RESOURCE_{env}=<n> or resources: {{{pool}: <n>}} in .hephconfig",
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

/// A pool's capacity in its unit's base (micro-CPUs, bytes, a count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// At most this much is held at once. `0`: the pool must be set per run,
    /// and a request naming it is an error ([`AcquireError::ZeroCapacity`]).
    Bounded(u64),
    /// No capacity: every request fits at once and nothing is clamped. A
    /// custom pool declared with no value, or `memory` when the host's could
    /// not be read.
    Unbounded,
}

impl Limit {
    /// The capacity, `None` when unbounded.
    pub fn bounded(self) -> Option<u64> {
        match self {
            Self::Bounded(n) => Some(n),
            Self::Unbounded => None,
        }
    }
}

/// Admits targets against per-pool capacities. See the module docs.
#[derive(Debug)]
pub struct ResourcePool {
    main: Arc<Lane>,
}

impl ResourcePool {
    /// A pool with these capacities (`cpu` in micro-CPUs, `memory` in bytes, a
    /// custom pool a count). A pool given twice keeps the last capacity.
    pub fn new(capacities: impl IntoIterator<Item = (PoolName, Limit)>) -> Arc<Self> {
        Arc::new(Self {
            main: Lane::new(capacities.into_iter().collect()),
        })
    }

    /// The capacity of a named pool, `None` when it is not one.
    pub fn capacity(&self, pool: &str) -> Option<Limit> {
        let idx = self.main.index(pool)?;
        self.main.capacity.get(idx).copied()
    }

    /// Every named pool and its capacity, in name order.
    pub fn capacities(&self) -> impl Iterator<Item = (&PoolName, Limit)> {
        self.main
            .names
            .iter()
            .zip(self.main.capacity.iter().copied())
    }

    /// What nobody holds in a named pool right now, `None` when it is not one
    /// or is unbounded.
    pub fn available(&self, pool: &str) -> Option<u64> {
        let idx = self.main.index(pool)?;
        let cap = self.main.capacity.get(idx)?.bounded()?;
        let held = self.main.state.lock().held.get(idx).copied()?;
        Some(cap.saturating_sub(held))
    }

    /// Ask for `request`'s amounts ([`Demand::amount`]: an undeclared `cpu`
    /// is one CPU). Validates and clamps now, then either grants at once or
    /// queues; await the returned [`Acquire`] for the grant.
    ///
    /// Dropping the `Acquire` leaves the queue, or returns a grant it was
    /// sent but never observed.
    pub fn acquire(self: &Arc<Self>, request: &Demand) -> Result<Acquire, AcquireError> {
        self.acquire_with(request, &[])
    }

    /// [`acquire`](Self::acquire) with `implicit` amounts on top of
    /// `request`'s, granted atomically with them: what the engine demands on
    /// the run's behalf (`sandbox: 1` for a managed run), which the target
    /// never declared. A pool in both is the target's declaration plus the
    /// implicit amount.
    pub fn acquire_with(
        self: &Arc<Self>,
        request: &Demand,
        implicit: &[(PoolName, u64)],
    ) -> Result<Acquire, AcquireError> {
        let (need, clamped) = self.resolve(request, implicit)?;
        let bounded = need
            .iter()
            .any(|&(idx, _)| matches!(self.main.capacity.get(idx), Some(Limit::Bounded(_))));
        let need = need.into_boxed_slice();
        if bounded {
            return Ok(self.main.enqueue(need, clamped));
        }
        // Nothing on a bounded pool: it always fits, so it is granted now,
        // past any queue, and nothing caps how many such grants are live.
        Ok(Acquire {
            state: AcquireState::Ready(Some(self.main.grant_now(need, clamped))),
        })
    }

    /// Per pool held/free/capacity, the queue length, and what the head is
    /// waiting for.
    pub fn snapshot(&self) -> ResourceSnapshot {
        ResourceSnapshot {
            main: self.main.snapshot(),
        }
    }

    /// The request plus `implicit` as `(pool index, granted amount)` in the
    /// main lane, empty when it reserves nothing anywhere; and whether any
    /// amount was clamped.
    fn resolve(
        &self,
        request: &Demand,
        implicit: &[(PoolName, u64)],
    ) -> Result<(Vec<(usize, u64)>, bool), AcquireError> {
        let lane = &self.main;
        let mut need = Vec::with_capacity(
            request
                .len()
                .saturating_add(1)
                .saturating_add(implicit.len()),
        );
        let mut clamped = false;
        // One entry per pool: `fits` checks each entry on its own, so a pool
        // listed twice could be granted past its capacity.
        let implicit_of = |pool: &str| {
            implicit
                .iter()
                .filter(|(p, _)| p.as_str() == pool)
                .fold(0u64, |acc, (_, n)| acc.saturating_add(*n))
        };
        for (pool, amount) in request.iter() {
            let amount = amount.saturating_add(implicit_of(pool.as_str()));
            lane.grant(lane.index(pool.as_str()), amount, &mut need, &mut clamped)
                .map_err(|e| e.naming(pool.clone()))?;
        }
        for (i, (pool, _)) in implicit.iter().enumerate() {
            let first = implicit.iter().position(|(p, _)| p == pool) == Some(i);
            if !first || request.declared(pool.as_str()).is_some() {
                continue;
            }
            let amount = implicit_of(pool.as_str());
            lane.grant(lane.index(pool.as_str()), amount, &mut need, &mut clamped)
                .map_err(|e| e.naming(pool.clone()))?;
        }
        if request.declared(PoolName::CPU).is_none() {
            lane.grant(
                lane.cpu_idx,
                Resources::DEFAULT_CPU_MICROS,
                &mut need,
                &mut clamped,
            )
            .map_err(|e| e.naming(PoolName::cpu()))?;
        }
        Ok((need, clamped))
    }
}

/// Why a pool could not grant a request, before the pool's name is attached.
#[derive(Debug, Clone, Copy)]
enum GrantError {
    Unknown,
    ZeroCapacity,
}

impl GrantError {
    fn naming(self, pool: PoolName) -> AcquireError {
        match self {
            Self::Unknown => AcquireError::UnknownPool { pool },
            Self::ZeroCapacity => AcquireError::ZeroCapacity { pool },
        }
    }
}

/// The `sandbox` pool's default capacity per CPU of the `cpu` capacity: room
/// for every running sandbox and three generations of finished ones queued
/// behind them on the reclaim lane before a new run waits for cleanup.
pub const SANDBOXES_PER_CPU: u64 = 4;

/// Every pool's capacity, from what the config and the run wrote (`specs`)
/// over the engine's defaults: `cpu` is `parallelism` CPUs, `memory` the
/// host's ([`hcore::sysres::host_memory_bytes`], read through `host_memory`
/// only when the memory capacity needs it; 0 is "could not read"), `sandbox`
/// [`SANDBOXES_PER_CPU`] per CPU of the resolved `cpu` capacity, rounded up.
/// A percentage is of the default; a custom pool has none, which the parser
/// already refused.
///
/// A pool with no value is unbounded, except `cpu` (always the parallelism),
/// `memory` (the host's) and `sandbox`, which keep their default; a `memory`
/// whose default is needed and could not be read is unbounded too.
pub fn resolve_capacities(
    specs: &BTreeMap<PoolName, PoolSpec>,
    parallelism: usize,
    host_memory: impl FnOnce() -> u64,
) -> Vec<(PoolName, Limit)> {
    let cpu_default = u64::try_from(parallelism)
        .unwrap_or(u64::MAX)
        .saturating_mul(Resources::MICROS_PER_CPU);
    let cpu = specs
        .get(PoolName::CPU)
        .and_then(|s| s.capacity)
        .map_or(cpu_default, |c| c.resolve(Unit::Cpu, || cpu_default));
    let sandbox_default = cpu
        .div_ceil(Resources::MICROS_PER_CPU)
        .saturating_mul(SANDBOXES_PER_CPU);
    let sandbox = specs
        .get(PoolName::SANDBOX)
        .and_then(|s| s.capacity)
        .map_or(sandbox_default, |c| {
            c.resolve(Unit::Count, || sandbox_default)
        });
    let memory = match specs.get(PoolName::MEMORY).and_then(|s| s.capacity) {
        Some(c @ hcore::capacity::Capacity::Amount(_)) => {
            Limit::Bounded(c.resolve(Unit::Bytes, || 0))
        }
        Some(c) => match host_memory() {
            0 => Limit::Unbounded,
            host => Limit::Bounded(c.resolve(Unit::Bytes, || host)),
        },
        None => match host_memory() {
            0 => Limit::Unbounded,
            host => Limit::Bounded(host),
        },
    };
    let custom = specs
        .iter()
        .filter(|(p, _)| Unit::of_builtin(p.as_str()).is_none())
        .map(|(p, s)| {
            let limit = s.capacity.map_or(Limit::Unbounded, |c| {
                Limit::Bounded(c.resolve(s.unit, || 0))
            });
            (p.clone(), limit)
        });
    [
        (PoolName::cpu(), Limit::Bounded(cpu)),
        (PoolName::memory(), memory),
        (PoolName::sandbox(), Limit::Bounded(sandbox)),
    ]
    .into_iter()
    .chain(custom)
    .collect()
}

/// Whether `pool` is the engine's own: a pool the engine demands on a run's
/// behalf, which no target may declare in `resources` nor read with
/// `${resource:…}`. Only `sandbox`: every managed run takes one, released
/// when its sandbox has been removed rather than when the run ends.
pub fn is_engine_owned(pool: &str) -> bool {
    pool == PoolName::SANDBOX
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
    specs: BTreeMap<PoolName, PoolSpec>,
    parallelism: usize,
    cpu: u64,
    pool: std::sync::OnceLock<Arc<ResourcePool>>,
}

impl EnginePools {
    pub fn new(specs: BTreeMap<PoolName, PoolSpec>, parallelism: usize) -> Self {
        let cpu = resolve_capacities(&specs, parallelism, || 0)
            .into_iter()
            .find_map(|(p, c)| (p.as_str() == PoolName::CPU).then_some(c))
            .and_then(Limit::bounded)
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
        self.unit(name).is_some()
    }

    /// The unit of pool `name` (a builtin's own, else what the config
    /// declared), `None` when it is not a pool.
    pub fn unit(&self, name: &str) -> Option<Unit> {
        self.specs
            .get(name)
            .map(|s| s.unit)
            .or_else(|| Unit::of_builtin(name))
    }

    /// Every pool a target may name, in name order: every pool but
    /// `sandbox`, which is the engine's (see [`is_engine_owned`]). These are
    /// what the "did you mean" and "known:" hints offer.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        let builtin = [PoolName::CPU, PoolName::MEMORY]
            .into_iter()
            .filter(|n| !self.specs.contains_key(*n));
        let mut all: Vec<&str> = builtin
            .chain(self.specs.keys().map(PoolName::as_str))
            .filter(|n| !is_engine_owned(n))
            .collect();
        all.sort_unstable();
        all.into_iter()
    }

    /// The pool whose name is nearest `name` by edit distance, when it is
    /// near enough to be a typo (at most 2 edits, and fewer than `name` has
    /// bytes), for a "did you mean" hint. Ties go to the first in name order.
    pub fn closest(&self, name: &str) -> Option<&str> {
        self.names()
            .map(|p| (strsim::levenshtein(name, p), p))
            .filter(|(d, _)| *d <= 2 && *d < name.len())
            .min_by_key(|(d, _)| *d)
            .map(|(_, p)| p)
    }

    /// The error tail for a pool that is not configured: a "did you mean"
    /// when one is near, then how to declare it (`declare`, the
    /// `.hephconfig` entry) and the one-off route, then the known pools.
    pub fn unknown_hint(&self, name: &str, declare: &str) -> String {
        let near = self
            .closest(name)
            .map(|p| format!(" (did you mean {p}?)"))
            .unwrap_or_default();
        format!(
            "{name} is not a pool{near}: add resources: {{{name}: {declare}}} to .hephconfig, or \
             --resource {name}=<n> for one run (known: {})",
            self.names().collect::<Vec<_>>().join(", ")
        )
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

    /// The built pool's snapshot, each named pool shown in its configured
    /// unit. `None` before the first admission has built the pool.
    pub fn snapshot(&self) -> Option<ResourceSnapshot> {
        let mut snap = self.built()?.snapshot();
        for s in &mut snap.main.pools {
            if let Some(unit) = self.unit(s.pool.as_str()) {
                s.unit = unit;
            }
        }
        Some(snap)
    }
}

/// One FIFO over every named pool.
#[derive(Debug)]
struct Lane {
    /// Sorted, so a pool is found by binary search; indices are stable.
    names: Box<[PoolName]>,
    capacity: Box<[Limit]>,
    /// Index of the `cpu` pool in `names`; `None` if there is none.
    cpu_idx: Option<usize>,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// What is granted and not yet released, indexed like `Lane::names`.
    /// Counted for an unbounded pool too, which only the snapshot reads.
    held: Box<[u64]>,
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
    fn new(capacities: BTreeMap<PoolName, Limit>) -> Arc<Self> {
        let (names, capacity): (Vec<PoolName>, Vec<Limit>) = capacities.into_iter().unzip();
        let cpu_idx = names
            .binary_search_by(|p| p.as_str().cmp(PoolName::CPU))
            .ok();
        Arc::new(Self {
            state: Mutex::new(State {
                held: vec![0; capacity.len()].into_boxed_slice(),
                waiters: BTreeMap::new(),
                next_seq: 0,
            }),
            names: names.into_boxed_slice(),
            capacity: capacity.into_boxed_slice(),
            cpu_idx,
        })
    }

    fn index(&self, pool: &str) -> Option<usize> {
        self.names.binary_search_by(|p| p.as_str().cmp(pool)).ok()
    }

    /// Adds `amount` of pool `idx` to `need`, clamped to a bounded capacity
    /// (setting `clamped`); a zero amount is checked but adds nothing.
    fn grant(
        &self,
        idx: Option<usize>,
        amount: u64,
        need: &mut Vec<(usize, u64)>,
        clamped: &mut bool,
    ) -> Result<(), GrantError> {
        let idx = idx.ok_or(GrantError::Unknown)?;
        let granted = match self.capacity.get(idx).copied() {
            Some(Limit::Unbounded) => amount,
            Some(Limit::Bounded(0)) | None => return Err(GrantError::ZeroCapacity),
            Some(Limit::Bounded(capacity)) => amount.min(capacity),
        };
        if amount != 0 {
            *clamped |= granted < amount;
            need.push((idx, granted));
        }
        Ok(())
    }

    /// Grant `need` now, past the queue. Only for a need that always fits
    /// (unbounded pools only, or nothing at all); an empty one takes no lock.
    fn grant_now(self: &Arc<Self>, need: Box<[(usize, u64)]>, clamped: bool) -> Reservation {
        if !need.is_empty() {
            take(&mut self.state.lock().held, &need);
        }
        Reservation {
            lane: Arc::clone(self),
            granted: need,
            clamped,
            armed: true,
        }
    }

    fn enqueue(self: &Arc<Self>, need: Box<[(usize, u64)]>, clamped: bool) -> Acquire {
        let mut st = self.state.lock();
        if st.waiters.is_empty() && fits(&self.capacity, &st.held, &need) {
            take(&mut st.held, &need);
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
            if !fits(&self.capacity, &st.held, &head.get().need) {
                break;
            }
            let waiter = head.remove();
            take(&mut st.held, &waiter.need);
            let reservation = Reservation {
                lane: Arc::clone(self),
                granted: waiter.need,
                clamped: waiter.clamped,
                armed: true,
            };
            if let Err(mut unobserved) = waiter.tx.send(reservation) {
                unobserved.armed = false;
                give(&self.capacity, &mut st.held, &unobserved.granted);
            }
        }
    }

    fn release(self: &Arc<Self>, granted: &[(usize, u64)]) {
        // An empty grant frees nothing, so it can admit nobody.
        if granted.is_empty() {
            return;
        }
        let mut st = self.state.lock();
        give(&self.capacity, &mut st.held, granted);
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
            pools: self.needs(&st.held, &waiter.need, false),
            since: waiter.since,
        })
    }

    fn snapshot(&self) -> LaneSnapshot {
        let st = self.state.lock();
        let pools = self
            .names
            .iter()
            .zip(self.capacity.iter().zip(st.held.iter()))
            .map(|(name, (&capacity, &held))| PoolSnapshot {
                pool: name.clone(),
                unit: Unit::default_for(name.as_str()),
                capacity,
                held,
            })
            .collect();
        let head = st.waiters.values().next().map(|w| HeadSnapshot {
            unmet: self.needs(&st.held, &w.need, true),
            since: w.since,
        });
        LaneSnapshot {
            pools,
            queued: st.waiters.len(),
            head,
        }
    }

    /// `need` as named amounts with each pool's free amount (`None`:
    /// unbounded); only the pools it does not fit in when `unmet_only`.
    fn needs(&self, held: &[u64], need: &[(usize, u64)], unmet_only: bool) -> Vec<PoolNeed> {
        need.iter()
            .filter_map(|&(idx, amount)| {
                let free = free_of(self.capacity.get(idx).copied()?, held.get(idx).copied()?);
                if unmet_only && free.is_none_or(|f| amount <= f) {
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

/// What a pool of capacity `limit` with `held` granted has free, `None` when
/// it is unbounded.
fn free_of(limit: Limit, held: u64) -> Option<u64> {
    limit.bounded().map(|cap| cap.saturating_sub(held))
}

/// Whether `need` fits beside `held`: an unbounded pool always does.
fn fits(capacity: &[Limit], held: &[u64], need: &[(usize, u64)]) -> bool {
    need.iter()
        .all(|&(idx, n)| match (capacity.get(idx), held.get(idx)) {
            (Some(Limit::Unbounded), Some(_)) => true,
            (Some(&Limit::Bounded(cap)), Some(&h)) => n <= cap.saturating_sub(h),
            _ => false,
        })
}

/// Count a need that [`fits`] as held. Saturates: an unbounded pool can be
/// granted more than a `u64` in total, which only the snapshot reads.
fn take(held: &mut [u64], need: &[(usize, u64)]) {
    for &(idx, n) in need {
        if let Some(h) = held.get_mut(idx) {
            *h = h.saturating_add(n);
        }
    }
}

/// Return a grant, never below nothing held. An unbounded pool's held count
/// saturates in [`take`], so releasing below 0 there is expected after a
/// saturation and not reported.
fn give(capacity: &[Limit], held: &mut [u64], granted: &[(usize, u64)]) {
    for &(idx, n) in granted {
        if let Some(h) = held.get_mut(idx) {
            *h = h.checked_sub(n).unwrap_or_else(|| {
                if !matches!(capacity.get(idx), Some(Limit::Unbounded)) {
                    tracing::error!(pool = idx, "resource pool released more than it granted");
                }
                0
            });
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

    /// The granted amount of a named pool; 0 for a pool not reserved.
    pub fn amount(&self, pool: &str) -> u64 {
        self.iter()
            .find_map(|(p, n)| (p.as_str() == pool).then_some(n))
            .unwrap_or(0)
    }

    /// The granted non-zero amounts, by pool; empty for a zero-cost grant.
    pub fn iter(&self) -> impl Iterator<Item = (&PoolName, u64)> {
        self.granted
            .iter()
            .filter_map(|&(idx, n)| Some((self.lane.names.get(idx)?, n)))
    }

    /// Move what this grant holds of `pool` into a reservation of its own,
    /// returned to the pool when *it* drops; `None` (and `self` unchanged)
    /// when nothing of `pool` was granted. The two halves are released
    /// independently, each running the grant chain: the `sandbox` part of a
    /// managed run outlives the run, until its sandbox has been removed.
    ///
    /// The split-off half is never `clamped`: whether the grant was clamped
    /// stays a fact about the original request.
    pub fn split_off(&mut self, pool: &str) -> Option<Reservation> {
        let idx = self.lane.index(pool)?;
        let pos = self.granted.iter().position(|&(i, _)| i == idx)?;
        let mut rest = std::mem::take(&mut self.granted).into_vec();
        let part = rest.remove(pos);
        self.granted = rest.into_boxed_slice();
        Some(Reservation {
            lane: Arc::clone(&self.lane),
            granted: Box::new([part]),
            clamped: false,
            armed: self.armed,
        })
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
        Poll::Ready(match &mut self.state {
            AcquireState::Ready(r) => r.take().ok_or(AcquireError::Closed),
            AcquireState::Waiting { guard, rx } => match Pin::new(rx).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(res) => {
                    // Granted (or the sender is gone): nothing left to remove.
                    guard.armed = false;
                    // Fused: a completed receiver must not be polled again (it
                    // panics), so a poll after this one answers `Closed` from
                    // `Ready(None)`.
                    self.state = AcquireState::Ready(None);
                    res.map_err(|_closed| AcquireError::Closed)
                }
            },
        })
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
    /// `None`: the pool is unbounded.
    pub free: Option<u64>,
}

/// The pool at one instant, for the in-flight report and the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSnapshot {
    /// Every named pool, in name order.
    pub main: LaneSnapshot,
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
    /// How the pool's amounts are shown. The pool itself does not know its
    /// unit: this is the name's default until
    /// [`Engine::resource_snapshot`](crate::engine::Engine::resource_snapshot)
    /// sets the configured one.
    pub unit: Unit,
    pub capacity: Limit,
    pub held: u64,
}

impl PoolSnapshot {
    /// What nobody holds, `None` when the pool is unbounded.
    pub fn free(&self) -> Option<u64> {
        free_of(self.capacity, self.held)
    }
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
    use std::time::Duration;

    const CPU: u64 = Resources::MICROS_PER_CPU;
    const GIB: u64 = 1 << 30;

    fn name(s: &str) -> PoolName {
        PoolName::try_from(s).unwrap()
    }

    fn pool(caps: &[(&str, u64)]) -> Arc<ResourcePool> {
        ResourcePool::new(caps.iter().map(|&(n, c)| (name(n), Limit::Bounded(c))))
    }

    fn req(pairs: &[(&str, u64)]) -> Demand {
        Demand::try_from_pairs(pairs.iter().map(|&(n, a)| (n.to_string(), a))).unwrap()
    }

    /// Acquire and expect an immediate grant.
    fn now(p: &Arc<ResourcePool>, r: &Demand) -> Reservation {
        p.acquire(r)
            .unwrap()
            .now_or_never()
            .expect("granted at once")
            .unwrap()
    }

    /// Acquire and expect to queue.
    fn queued(p: &Arc<ResourcePool>, r: &Demand) -> Acquire {
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

    /// An all-zero request is granted at once, past a full cpu pool and a cpu
    /// waiter at the head, and reserves nothing; the waiter keeps its place.
    #[test]
    fn an_all_zero_request_is_granted_past_a_full_queue() {
        let p = pool(&[("cpu", CPU), ("memory", 8 * GIB)]);
        let held = now(&p, &req(&[]));
        let mut waiting_for_cpu = queued(&p, &req(&[]));
        for zero in [req(&[("cpu", 0)]), req(&[("cpu", 0), ("memory", 0)])] {
            let r = now(&p, &zero);
            assert_eq!(r.iter().count(), 0, "reserves nothing");
            assert_eq!(p.snapshot().main.queued, 1, "never queued");
            drop(r);
            assert!(
                (&mut waiting_for_cpu).now_or_never().is_none(),
                "its release admits nobody"
            );
        }
        drop(held);
        drop(granted(&mut waiting_for_cpu).unwrap());

        // `cpu: 0` with memory is not zero-cost: it queues on memory.
        let mem = now(&p, &req(&[("cpu", 0), ("memory", 8 * GIB)]));
        let mut more = queued(&p, &req(&[("cpu", 0), ("memory", 8 * GIB)]));
        drop(mem);
        assert_eq!(granted(&mut more).unwrap().amount("memory"), 8 * GIB);
    }

    /// Nothing caps zero-cost grants: far more than any cpu-derived bound
    /// are live at once, none touching cpu.
    #[test]
    fn many_zero_cost_requests_are_all_granted_at_once() {
        let p = pool(&[("cpu", CPU)]);
        let zero = req(&[("cpu", 0)]);
        let held: Vec<_> = (0..10_000).map(|_| now(&p, &zero)).collect();
        assert_eq!(free(&p, "cpu"), CPU);
        assert_eq!(p.snapshot().main.queued, 0);
        drop(held);
        assert_eq!(free(&p, "cpu"), CPU);
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
                                let _r = p.acquire(&Demand::default()).unwrap().await.unwrap();
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
            p.acquire(&Demand::default()).unwrap(),
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
                let r = p.acquire(&Demand::default()).unwrap().await.unwrap();
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
                    unit: Unit::Cpu,
                    capacity: Limit::Bounded(4 * CPU),
                    held: 3 * CPU,
                },
                PoolSnapshot {
                    pool: name("memory"),
                    unit: Unit::Bytes,
                    capacity: Limit::Bounded(8 * GIB),
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
                free: Some(CPU),
            }],
            "only the pool it does not fit"
        );
        let info = head.wait_info().unwrap();
        assert_eq!(info.ahead, 0);
        assert_eq!(info.since, h.since);
        assert_eq!(info.pools.len(), 2);
        assert_eq!(follower.wait_info().unwrap().ahead, 1);
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
                 --resource gpu=<n>, HEPH_RESOURCE_GPU=<n> or resources: {gpu: <n>} in .hephconfig"
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
            none.acquire(&Demand::default()).unwrap_err(),
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
    /// (`exec`, `template`, `fs`, a relocating `group`), parsed by the real in-tree drivers
    /// with `resources` set to `declared`.
    async fn driver_defs(declared: Option<hcore::htvalue::Value>) -> Vec<(&'static str, Demand)> {
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
                parse_req(
                    "group",
                    &[("deps", Value::List(vec![s(":a")])), ("prefix", s("lib"))],
                    d,
                ),
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
            let demand = def
                .target_def
                .resources
                .parse(Unit::of_builtin)
                .unwrap_or_else(|e| panic!("demand {name}: {e:#}"));
            (name, demand)
        })
        .collect()
    }

    /// D3: a def that says nothing reserves 1 cpu (exec, template), except
    /// `fs` and `group`, which advertise cpu 0 and so reserve nothing.
    #[tokio::test]
    async fn a_def_without_resources_costs_one_cpu() {
        let p = pool(&[("cpu", 4 * CPU)]);
        for (driver, resources) in driver_defs(None).await {
            let want = match driver {
                "fs" | "group" => 0,
                _ => CPU,
            };
            assert_eq!(resources.amount("cpu"), want, "{driver}");
            let r = now(&p, &resources);
            assert_eq!(r.amount("cpu"), want, "{driver}");
            if want == 0 {
                assert_eq!(r.iter().count(), 0, "{driver}: reserves nothing");
            }
        }
    }

    /// An `fs` glob def is admitted at once while the cpu pool is saturated
    /// and a `cpu: 1` waiter is at the head.
    #[tokio::test]
    async fn an_fs_def_is_granted_past_a_saturated_cpu_pool() {
        let p = pool(&[("cpu", CPU)]);
        let held = now(&p, &req(&[("cpu", CPU)]));
        let mut head = queued(&p, &req(&[("cpu", CPU)]));
        let fs = driver_defs(None)
            .await
            .into_iter()
            .find_map(|(d, r)| (d == "fs").then_some(r))
            .unwrap();
        let r = now(&p, &fs);
        assert_eq!(r.iter().count(), 0);
        assert_eq!(p.snapshot().main.queued, 1, "the head still waits");
        drop((r, held));
        assert!(granted(&mut head).is_some());
    }

    /// D3, D5: a declared `cpu` replaces every driver's default, and `cpu: 0`
    /// is granted at once, reserving nothing from `cpu`.
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
            assert_eq!(r.iter().count(), 0, "{driver}: cpu 0 reserves nothing");
            assert_eq!(free(&p, "cpu"), 4 * CPU, "{driver}: holds no cpu");
        }
    }

    /// An unbounded pool admits every request at once, never clamps, and the
    /// grant carries the declared amount; it is counted as held.
    #[test]
    fn an_unbounded_pool_admits_immediately_with_the_declared_amount() {
        let p = ResourcePool::new([
            (name("cpu"), Limit::Bounded(4 * CPU)),
            (name("tokens"), Limit::Unbounded),
        ]);
        let big = req(&[("cpu", 0), ("tokens", u64::MAX)]);
        let a = now(&p, &big);
        let b = now(&p, &big);
        assert!(!a.clamped() && !b.clamped());
        assert_eq!(a.amount("tokens"), u64::MAX);
        assert_eq!(p.available("tokens"), None, "no free amount to report");
        let s = p.snapshot();
        let tokens = s
            .main
            .pools
            .iter()
            .find(|s| s.pool == name("tokens"))
            .unwrap();
        assert_eq!(tokens.capacity, Limit::Unbounded);
        assert_eq!(tokens.held, u64::MAX, "saturates rather than wrapping");
        assert_eq!(tokens.free(), None);
        drop((a, b));
        assert_eq!(p.snapshot().main.queued, 0);
    }

    /// A request that needs nothing of a bounded pool (`cpu: 0` beside an
    /// unbounded pool only) is granted at once, past a cpu waiter at the
    /// head, and its unbounded amounts are counted as held in their pool from
    /// the grant until the release.
    #[test]
    fn unbounded_only_requests_are_granted_at_once_and_keep_their_amounts() {
        let p = ResourcePool::new([
            (name("cpu"), Limit::Bounded(CPU)),
            (name("tokens"), Limit::Unbounded),
        ]);
        let tokens_held = |p: &ResourcePool| {
            p.snapshot()
                .main
                .pools
                .iter()
                .find(|s| s.pool == name("tokens"))
                .map(|s| s.held)
        };
        let cpu = now(&p, &req(&[]));
        let mut head = queued(&p, &req(&[]));
        let r = req(&[("cpu", 0), ("tokens", 5)]);
        let held: Vec<_> = (0..100).map(|_| now(&p, &r)).collect();
        assert_eq!(held[0].amount("tokens"), 5);
        assert_eq!(held[0].amount("cpu"), 0);
        assert_eq!(tokens_held(&p), Some(500));
        assert_eq!(p.snapshot().main.queued, 1, "only the cpu waiter");
        // An unobserved zero-cost grant returns its amounts too.
        drop(p.acquire(&r).unwrap());
        assert_eq!(tokens_held(&p), Some(500));
        drop(held);
        assert_eq!(tokens_held(&p), Some(0));
        assert!(
            (&mut head).now_or_never().is_none(),
            "releasing unbounded amounts admits nobody"
        );
        drop(cpu);
        assert!(granted(&mut head).is_some());
    }

    /// No value: a custom pool is unbounded, `cpu`/`memory` keep their
    /// defaults, and a host memory that could not be read (0) is unbounded;
    /// an explicit 0 stays a must-be-set pool.
    #[test]
    fn resolve_capacities_unbounded_and_zero() {
        use hcore::capacity::Capacity;
        let spec = |unit, capacity| PoolSpec { unit, capacity };
        let specs = BTreeMap::from([
            (name("cpu"), spec(Unit::Cpu, None)),
            (name("memory"), spec(Unit::Bytes, None)),
            (name("tokens"), spec(Unit::Count, None)),
            (name("gpu"), spec(Unit::Count, Some(Capacity::Amount(0)))),
        ]);
        let got: BTreeMap<_, _> = resolve_capacities(&specs, 2, || 0).into_iter().collect();
        assert_eq!(got[PoolName::CPU], Limit::Bounded(2 * CPU));
        assert_eq!(got[PoolName::MEMORY], Limit::Unbounded, "host read failed");
        assert_eq!(got["tokens"], Limit::Unbounded);
        assert_eq!(got["gpu"], Limit::Bounded(0));
        let got: BTreeMap<_, _> = resolve_capacities(&specs, 2, || GIB).into_iter().collect();
        assert_eq!(got[PoolName::MEMORY], Limit::Bounded(GIB));

        let p = ResourcePool::new(resolve_capacities(&specs, 2, || 0));
        assert_eq!(
            p.acquire(&req(&[("gpu", 1)])).unwrap_err(),
            AcquireError::ZeroCapacity { pool: name("gpu") },
            "value 0 still errors at admission"
        );
        assert!(!now(&p, &req(&[("memory", 64 * GIB)])).clamped());
    }

    /// The `sandbox` default is [`SANDBOXES_PER_CPU`] per CPU of the resolved
    /// `cpu` capacity, so it follows `-j` and a `cpu` override (a fractional
    /// cpu rounds up); an explicit amount wins, and a percentage is of that
    /// default.
    #[test]
    fn resolve_capacities_sandbox_default_override_and_percent() {
        use hcore::capacity::Capacity;
        let sandbox_of = |specs: &BTreeMap<PoolName, PoolSpec>, j: usize| {
            resolve_capacities(specs, j, || 0)
                .into_iter()
                .find(|(p, _)| p.as_str() == PoolName::SANDBOX)
                .map(|(_, l)| l)
                .expect("the sandbox pool always exists")
        };
        let spec = |unit, capacity| PoolSpec { unit, capacity };
        let none = BTreeMap::new();
        assert_eq!(sandbox_of(&none, 4), Limit::Bounded(16), "4 x cpu");
        assert_eq!(sandbox_of(&none, 1), Limit::Bounded(4), "scales with -j");
        assert_eq!(sandbox_of(&none, 10), Limit::Bounded(40), "scales with -j");

        let cpu = |c| BTreeMap::from([(name("cpu"), spec(Unit::Cpu, Some(c)))]);
        assert_eq!(
            sandbox_of(&cpu(Capacity::Amount(3 * CPU)), 8),
            Limit::Bounded(12),
            "a cpu override moves the default"
        );
        assert_eq!(
            sandbox_of(&cpu(Capacity::Amount(CPU / 2)), 8),
            Limit::Bounded(4),
            "half a cpu rounds up to one"
        );

        let sandbox = |c| BTreeMap::from([(name("sandbox"), spec(Unit::Count, Some(c)))]);
        assert_eq!(
            sandbox_of(&sandbox(Capacity::Amount(3)), 8),
            Limit::Bounded(3),
            "an override wins"
        );
        assert_eq!(
            sandbox_of(&sandbox(Capacity::Percent(50)), 4),
            Limit::Bounded(8),
            "50% of 4 x 4"
        );
        assert_eq!(
            sandbox_of(&sandbox(Capacity::Percent(200)), 2),
            Limit::Bounded(16),
            "200% of 4 x 2"
        );
    }

    /// The two halves of a split grant come back independently: the run's
    /// cpu when the run ends, its `sandbox` slot only when the slot drops,
    /// and a waiter for the slot is admitted then, not before.
    #[test]
    fn a_split_off_slot_is_released_independently() {
        let p = pool(&[("cpu", 2 * CPU), ("sandbox", 1)]);
        let slot = || [(PoolName::sandbox(), 1)];
        let mut run = p
            .acquire_with(&req(&[("cpu", CPU)]), &slot())
            .unwrap()
            .now_or_never()
            .expect("granted at once")
            .unwrap();
        assert!(
            run.split_off("memory").is_none(),
            "nothing of an unnamed pool"
        );
        let held = run.split_off(PoolName::SANDBOX).expect("the slot");
        assert!(!held.clamped());
        assert!(
            run.split_off(PoolName::SANDBOX).is_none(),
            "the slot left the run's grant"
        );
        assert_eq!(held.iter().collect::<Vec<_>>(), [(&name("sandbox"), 1)]);
        assert_eq!(run.iter().collect::<Vec<_>>(), [(&name("cpu"), CPU)]);

        let mut next = p.acquire_with(&req(&[("cpu", CPU)]), &slot()).unwrap();
        assert!(
            (&mut next).now_or_never().is_none(),
            "the only slot is held"
        );

        drop(run);
        assert_eq!(free(&p, "cpu"), 2 * CPU, "the run's cpu is back");
        assert_eq!(free(&p, "sandbox"), 0, "its slot is not");
        assert!(
            (&mut next).now_or_never().is_none(),
            "the run's end admits nobody waiting for a slot"
        );

        drop(held);
        let next = granted(&mut next).expect("the slot's release admits the waiter");
        drop(next);
        assert_eq!((free(&p, "cpu"), free(&p, "sandbox")), (2 * CPU, 1));
    }
}
