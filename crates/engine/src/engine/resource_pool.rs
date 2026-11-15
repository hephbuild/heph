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
//! A request for more than a
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
//! at once and never queues: it always fits, so making it wait would only
//! delay it. Nothing caps how many such grants are live; a
//! zero-cost target is limited by the runtime and the OS alone. Its unbounded
//! amounts are still counted as held, for the snapshot.
//!
//! # Who is admitted
//!
//! Strict FIFO stopped every run behind the oldest waiter, so capacity sat
//! idle whenever that waiter did not fit: a cpu-only run behind one waiting
//! for a `sandbox` slot, three free cpu behind one that needs four. Admission
//! keeps the machine busy instead, at a cost per event that does not grow
//! with the queue.
//!
//! The amounts a request takes, after clamping, are its *shape*; the waiters
//! of one shape are a *class*, in arrival order. The oldest waiter is the
//! *head*. A grant *leaves room* for the head when, on every bounded pool the
//! grant takes from, the head's whole need still fits beside it. A grant
//! behind the head that does not leave room is *charged*: its amount on each
//! such pool is added to the head's debt there. The head is *protected* once
//! its debt on a pool it needs reaches its limit: [`PATIENCE`] times its own
//! need, and never more than [`PATIENCE_POOLS`] times the pool.
//!
//! When a grant admitted as shape X is released:
//!
//! 1. **Hand-off.** The oldest waiter of X is offered the slot. A wide run
//!    starts only in a drained pool, and this pays for the drain once per
//!    batch of them, not once per run.
//! 2. **Head.** While the head's whole need fits, it is granted; the next
//!    oldest waiter becomes the head, with no debt.
//! 3. **Pass.** Each other class, the one with the oldest waiter first, has
//!    its oldest waiter granted while it fits and either the head is not
//!    protected or the grant leaves room. A class that is refused is done for
//!    the walk: what is free only shrinks during it.
//!
//! A waiter that is not the head always meets step 3's condition, in step 1
//! too. An arrival queues behind a waiter of its own shape and is otherwise
//! judged alone, by the same condition. So:
//!
//! - **Overtaking is bounded.** What is granted behind a head without leaving
//!   it room adds up, on any pool it needs, to at most its limit plus one
//!   grant. After that nothing admitted takes what it needs, and it starts
//!   as soon as the runs holding its pools release enough.
//! - **No starvation.** Waiters become the head in arrival order.
//! - **One shape starts in arrival order.** Across shapes, start order does
//!   not follow arrival.
//! - **Idle capacity beside a protected head is deliberate**: it is draining
//!   for a run that needs more than is free.
//! - **Deterministic.** No clock, no randomness and no hash-map order decides
//!   a grant.
//! - **Nothing persistent.** Grant order reaches no hash, no run and no
//!   format: it only changes which runs execute together.
//!
//! The walk runs only when something could let a waiter fit: not after a
//! release whose slot went to its own shape with the head unchanged (that
//! waiter takes at least what came back, so nothing is freer than before),
//! not after a release that frees nothing bounded, not when a waiter behind
//! the head leaves. It tests one waiter per class, never one per waiter, and
//! at most [`WALK_CAP`] classes per event; classes it did not reach are
//! tested by the events that follow, of any kind, until each waiting class
//! was tested once. With more classes than that waiting, one that fits can
//! wait a few events.
//!
//! A grant that was split ([`Reservation::split_off`]: a managed run's cpu
//! and its `sandbox` slot) is released in parts. Each part is offered to the
//! shape the whole was admitted as. That waiter needs the other part too, so
//! unlike an unsplit hand-off it is tested for fit; it still takes at least
//! what came back.
//!
//! # The grant and Drop contract
//!
//! - A grant is a [`Reservation`] built on the release side and sent through
//!   a `oneshot`, so a waiter dropped after its grant was sent drops the
//!   unobserved grant, which returns itself.
//! - **Every** mutation of a lane's free amounts or queue ends with whatever
//!   it made grantable granted: a release, an arrival, and a waiter's drop (a
//!   cancelled head admits whoever its protection held back, without waiting
//!   for the next release).
//! - The mutex is never held across an await, and an armed `Reservation` is
//!   never dropped under it: a grant is counted as held only once its `send`
//!   succeeded, and a `send` that fails hands it back, which is disarmed.
//! - The release path runs inside `Drop`, so it does not panic: checked
//!   lookups and saturating arithmetic only.
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
//! sandbox kept by `--keep-failed-sandboxes` gives its slot back at once (one
//! tree per failed addr, removed by that addr's next run). A failed run's
//! sandbox that is not kept holds its slot until its removal returns, as a
//! passing run's does.

use hcore::capacity::{PoolSpec, Unit};
use hplugin::driver::{Demand, Resources};
use hplugin::provider::PoolName;
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::collections::BTreeSet;
use std::collections::btree_map::{self, BTreeMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
        let (mut need, clamped) = self.resolve(request, implicit)?;
        // One spelling per shape: `resolve` lists pools in declaration order.
        need.sort_unstable_by_key(|&(idx, _)| idx);
        if self.main.frees_bounded(&need) {
            return Ok(self.main.enqueue(need, clamped));
        }
        // Nothing on a bounded pool: it always fits, so it is granted now,
        // past any queue, and nothing caps how many such grants are live.
        Ok(Acquire {
            state: AcquireState::Ready(Some(self.main.grant_now(need.into(), clamped))),
        })
    }

    /// Per pool held/free/capacity, the queue length, and what the head is
    /// waiting for.
    pub fn snapshot(&self) -> ResourceSnapshot {
        ResourceSnapshot {
            main: self.main.snapshot(),
        }
    }

    /// How many finished runs' sandbox removals have not returned yet
    /// ([`Reservation::into_removal`]), each holding its `sandbox` slot.
    pub fn removals_pending(&self) -> u64 {
        self.main.removals.load(Ordering::Acquire)
    }

    /// A counter that moves on every change to what the pool holds, who
    /// waits, or how many removals are pending; nothing else about it means
    /// anything. A [`snapshot`](Self::snapshot) taken after reading a value
    /// shows every change that moved the counter up to that value (see
    /// `Lane::generation`), so a sampler that remembers the last value it
    /// sent knows whether there is anything new without taking the lock.
    pub fn generation(&self) -> u64 {
        self.main.generation.load(Ordering::Acquire)
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
        // listed twice could be granted past its capacity. So every pool's
        // entry is its whole amount: the declaration (for an undeclared
        // `cpu`, the default one CPU) plus every implicit entry naming it.
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
        if request.declared(PoolName::CPU).is_none() {
            let amount = Resources::DEFAULT_CPU_MICROS.saturating_add(implicit_of(PoolName::CPU));
            lane.grant(lane.cpu_idx, amount, &mut need, &mut clamped)
                .map_err(|e| e.naming(PoolName::cpu()))?;
        }
        for (i, (pool, _)) in implicit.iter().enumerate() {
            // Already granted: declared (`cpu` always is, by default), or an
            // earlier entry of `implicit` named it and took them all.
            let earlier = implicit.iter().take(i).any(|(p, _)| p == pool);
            if earlier
                || pool.as_str() == PoolName::CPU
                || request.declared(pool.as_str()).is_some()
            {
                continue;
            }
            let amount = implicit_of(pool.as_str());
            lane.grant(lane.index(pool.as_str()), amount, &mut need, &mut clamped)
                .map_err(|e| e.naming(pool.clone()))?;
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
    // The default is never empty, even taken of a `cpu: 0`, and a
    // percentage floors at 1 (`Capacity::resolve`). An explicit 0 stays 0,
    // as for any pool: the run sets it, and admission refuses a managed run
    // until it does.
    let sandbox_default = cpu
        .div_ceil(Resources::MICROS_PER_CPU)
        .saturating_mul(SANDBOXES_PER_CPU)
        .max(1);
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

/// A need: `(pool index, amount)`, one entry per pool, sorted by pool index.
/// Amounts on unbounded pools are part of it: two runs that differ only there
/// are told different amounts, so they are two shapes.
type Shape = Arc<[(usize, u64)]>;

/// A head is overtaken by this many times its own need of a pool before it is
/// protected.
const PATIENCE: u64 = 16;

/// And never by more than this many times the pool's capacity, so a request
/// for a whole pool waits about four pool-lengths, not sixteen.
const PATIENCE_POOLS: u64 = 4;

/// How many classes one event's walk tests at most. What it leaves untested,
/// the following events test.
const WALK_CAP: usize = 64;

/// How a grant came about. Diagnostic: it answers "why did this start before
/// that", and decides nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admitted {
    /// On arrival, with nobody waiting (a zero-cost request always is).
    Arrival,
    /// As the oldest waiter.
    Head,
    /// Ahead of an older waiter, in the slot a run of its own shape just
    /// released.
    Handoff,
    /// Ahead of an older waiter, in capacity that was free: on arrival or
    /// from the queue.
    Pass,
}

/// One queue over every named pool.
#[derive(Debug)]
struct Lane {
    /// Sorted, so a pool is found by binary search; indices are stable.
    names: Box<[PoolName]>,
    capacity: Box<[Limit]>,
    /// Index of the `cpu` pool in `names`; `None` if there is none.
    cpu_idx: Option<usize>,
    state: Mutex<State>,
    /// Live [`PendingRemoval`]s: grants a finished run still holds until its
    /// sandbox is removed. Diagnostic only; nothing is admitted by it.
    removals: AtomicU64,
    /// [`ResourcePool::generation`]. Bumped (`Release`) while the state lock
    /// is held, before the change it announces ([`Lane::change`]), or after
    /// `removals` moves. A reader that loads it (`Acquire`) and then takes
    /// the lock is ordered after the changer's lock acquisition, so it waits
    /// for that change to finish and sees it: no change is ever announced
    /// before it can be observed, on a weakly ordered CPU (aarch64) as on
    /// x86_64. A change landing between the load and the lock is seen early
    /// and announced again on the next read: at worst one redundant sample.
    /// Never read by admission.
    generation: AtomicU64,
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
    /// Shape → the id of its class. Looked up, never iterated: a hash map's
    /// order must not decide a grant.
    shapes: FxHashMap<Shape, u64>,
    /// The waiting classes by id, which is creation order. A class is here
    /// exactly while it has a waiter.
    classes: BTreeMap<u64, Class>,
    next_class: u64,
    head: Head,
    /// The class id the next batch of the walk starts from.
    cursor: u64,
    untested: Untested,
    /// The walk's batch, kept for its allocation.
    batch: Vec<(u64, u64)>,
    /// How many classes the walk has tested, over the lane's life.
    #[cfg(test)]
    visited: u64,
}

#[derive(Debug)]
struct Waiter {
    class: u64,
    clamped: bool,
    since: Instant,
    tx: oneshot::Sender<Reservation>,
}

/// The waiters of one shape, in arrival order.
#[derive(Debug)]
struct Class {
    need: Shape,
    waiters: BTreeSet<u64>,
}

/// What the head has been overtaken by. It belongs to one waiter: any other
/// head starts from nothing ([`State::sync_head`]).
#[derive(Debug, Default)]
struct Head {
    /// The oldest waiter's sequence and its need; `None` when nobody waits.
    who: Option<(u64, Shape)>,
    /// Per entry of the need: what was granted behind this head, on that
    /// pool, without leaving its need there. Empty when nobody waits.
    debt: Vec<u64>,
}

/// What the admission rule says of a request, given what is free and the
/// head ([`Head::judge`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// It does not fit what is free.
    NoFit,
    /// It fits, and would take what a protected head needs.
    Yields,
    /// It is granted. `charged`: the grant does not leave the head's need,
    /// so it counts against the head ([`Head::charge`]).
    Grant { charged: bool },
}

impl Head {
    fn seq(&self) -> Option<u64> {
        self.who.as_ref().map(|&(seq, _)| seq)
    }

    fn need(&self) -> Option<&[(usize, u64)]> {
        self.who.as_ref().map(|(_, need)| &**need)
    }

    /// Whether its debt reached its limit on a pool it needs. From then on
    /// nothing is granted that takes from what it needs.
    fn protected(&self, capacity: &[Limit]) -> bool {
        self.need().is_some_and(|need| {
            need.iter().zip(&self.debt).any(|(&(idx, n), &debt)| {
                patience(capacity.get(idx).copied(), n).is_some_and(|limit| debt >= limit)
            })
        })
    }

    /// The admission rule, for a request of `need`: the waiter `seq`, or an
    /// arrival that is not queued (`None`). The one place it is written:
    /// an arrival, an offer and a waiter's report all ask here.
    ///
    /// It must fit. The head is then granted; anything else is granted when
    /// it leaves room for the head, charged when it does not and the head is
    /// not protected, and refused when the head is. `held` is before the
    /// grant. Decides only: the caller charges and takes.
    fn judge(
        &self,
        capacity: &[Limit],
        held: &[u64],
        need: &[(usize, u64)],
        seq: Option<u64>,
    ) -> Verdict {
        if !fits(capacity, held, need) {
            return Verdict::NoFit;
        }
        let room = match &self.who {
            None => true,
            Some((head, _)) if Some(*head) == seq => true,
            Some((_, head)) => leaves_room(capacity, held, need, head),
        };
        if room {
            Verdict::Grant { charged: false }
        } else if self.protected(capacity) {
            Verdict::Yields
        } else {
            Verdict::Grant { charged: true }
        }
    }

    /// Count a grant of `grant` against the head: its amount on each pool
    /// where it does not leave the head's need. `held` is before the grant.
    fn charge(&mut self, capacity: &[Limit], held: &[u64], grant: &[(usize, u64)]) {
        let Some((_, need)) = &self.who else {
            return;
        };
        for (&(idx, n), debt) in need.iter().zip(&mut self.debt) {
            let taken = amount_in(grant, idx);
            if taken != 0 && !leaves(capacity, held, idx, taken, n) {
                *debt = debt.saturating_add(taken);
            }
        }
    }
}

/// What of the current pass over the waiting classes is still to test. A pass
/// starts when something changed that could let a waiter fit, and covers
/// every class once, [`WALK_CAP`] per event.
#[derive(Debug, Clone, Copy)]
enum Untested {
    /// Nothing: no waiter can be granted until a release or a head change.
    Nothing,
    /// The class ids from the cursor up, then those below `stop`.
    Wrap { stop: u64 },
    /// The class ids from the cursor up to `stop`, exclusive.
    Until { stop: u64 },
}

/// What came of offering a class's oldest waiter what is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offer {
    Granted,
    /// It does not fit, or it would take what a protected head needs, or the
    /// class has no waiter left.
    Refused,
    /// Its receiver was dropped: it left the queue with nothing granted, and
    /// the class's next waiter can be offered the same.
    Gone,
}

impl State {
    /// Point `head` at the oldest waiter. A waiter that was not the head the
    /// last time this ran starts with no debt.
    fn sync_head(&mut self) {
        let first = self
            .waiters
            .first_key_value()
            .map(|(&seq, w)| (seq, w.class));
        if self.head.seq() == first.map(|(seq, _)| seq) {
            return;
        }
        self.head.who = first.and_then(|(seq, class)| {
            let class = self.classes.get(&class)?;
            Some((seq, Arc::clone(&class.need)))
        });
        let entries = self.head.need().map_or(0, <[_]>::len);
        self.head.debt.clear();
        self.head.debt.resize(entries, 0);
    }

    /// The class of the oldest waiter.
    fn head_class(&self) -> Option<u64> {
        self.waiters.first_key_value().map(|(_, w)| w.class)
    }

    fn head_seq(&self) -> Option<u64> {
        self.waiters.first_key_value().map(|(&seq, _)| seq)
    }

    /// Take a waiter out of the queue and of its class; its last waiter
    /// takes the class with it.
    fn remove(&mut self, seq: u64) -> Option<Waiter> {
        let waiter = self.waiters.remove(&seq)?;
        if let btree_map::Entry::Occupied(mut class) = self.classes.entry(waiter.class) {
            class.get_mut().waiters.remove(&seq);
            if class.get().waiters.is_empty() {
                let class = class.remove();
                self.shapes.remove(&class.need);
            }
        }
        Some(waiter)
    }

    /// Queue a waiter of shape `need`, whose class is `class`: the id
    /// `shapes` holds for `need`, or a fresh one. A fresh class is entered
    /// here with its first waiter, in `classes` and in `shapes` together as
    /// [`State::remove`] takes it out of both, so no waiter is without a
    /// class and no class without the entry that finds it by shape.
    fn push(
        &mut self,
        class: u64,
        need: Shape,
        clamped: bool,
    ) -> (u64, oneshot::Receiver<Reservation>) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let (tx, rx) = oneshot::channel();
        match self.classes.entry(class) {
            btree_map::Entry::Occupied(mut occupied) => {
                occupied.get_mut().waiters.insert(seq);
            }
            btree_map::Entry::Vacant(vacant) => {
                self.shapes.insert(Arc::clone(&need), class);
                vacant.insert(Class {
                    need,
                    waiters: BTreeSet::from([seq]),
                });
            }
        }
        self.waiters.insert(
            seq,
            Waiter {
                class,
                clamped,
                since: Instant::now(),
                tx,
            },
        );
        (seq, rx)
    }

    /// Move up to [`WALK_CAP`] untested classes into `batch`, as `(oldest
    /// waiter, class id)`, in id order from the cursor. The head's class is
    /// left out: its oldest waiter is the head, which the walk tests first.
    fn next_batch(&mut self, head_class: u64) {
        let (classes, batch) = (&self.classes, &mut self.batch);
        batch.clear();
        // The id of the first class left out once the batch is full.
        let mut fill = |range: btree_map::Range<'_, u64, Class>| {
            for (&id, class) in range {
                if id == head_class {
                    continue;
                }
                if batch.len() >= WALK_CAP {
                    return Some(id);
                }
                if let Some(&oldest) = class.waiters.first() {
                    batch.push((oldest, id));
                }
            }
            None
        };
        let (cursor, untested) = match self.untested {
            Untested::Nothing => return,
            Untested::Wrap { stop } => match fill(classes.range(self.cursor..)) {
                Some(next) => (next, Untested::Wrap { stop }),
                None => match fill(classes.range(..stop)) {
                    Some(next) => (next, Untested::Until { stop }),
                    None => (stop, Untested::Nothing),
                },
            },
            // `range` panics on a start past its end.
            Untested::Until { stop } if self.cursor >= stop => (stop, Untested::Nothing),
            Untested::Until { stop } => match fill(classes.range(self.cursor..stop)) {
                Some(next) => (next, Untested::Until { stop }),
                None => (stop, Untested::Nothing),
            },
        };
        self.cursor = cursor;
        self.untested = untested;
    }
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
                shapes: FxHashMap::default(),
                classes: BTreeMap::new(),
                next_class: 0,
                head: Head::default(),
                cursor: 0,
                untested: Untested::Nothing,
                batch: Vec::new(),
                #[cfg(test)]
                visited: 0,
            }),
            names: names.into_boxed_slice(),
            capacity: capacity.into_boxed_slice(),
            cpu_idx,
            removals: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        })
    }

    /// The state lock, for a call that may change what is held or who
    /// waits: announces the change on `generation` while the lock is held.
    /// Reads take `state.lock()` directly.
    fn change(&self) -> parking_lot::MutexGuard<'_, State> {
        let guard = self.state.lock();
        self.generation.fetch_add(1, Ordering::Release);
        guard
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
    fn grant_now(self: &Arc<Self>, need: Shape, clamped: bool) -> Reservation {
        if !need.is_empty() {
            take(&mut self.change().held, &need);
        }
        Reservation {
            lane: Arc::clone(self),
            granted: need,
            origin: None,
            clamped,
            admitted: Admitted::Arrival,
            armed: true,
        }
    }

    /// Whether `amounts` holds something of a bounded pool: the only thing a
    /// request waits for, and the only thing whose release lets a waiter fit.
    fn frees_bounded(&self, amounts: &[(usize, u64)]) -> bool {
        amounts
            .iter()
            .any(|&(idx, _)| matches!(self.capacity.get(idx), Some(Limit::Bounded(_))))
    }

    /// An arrival. Behind a waiter of its own shape it queues, so one shape
    /// starts in arrival order. Otherwise it is judged alone, as the walk
    /// would judge it: granted when it fits and nobody waits, or when it fits
    /// and either the head is not protected or the grant leaves the head's
    /// need.
    fn enqueue(self: &Arc<Self>, need: Vec<(usize, u64)>, clamped: bool) -> Acquire {
        let mut guard = self.change();
        let st = &mut *guard;
        // Older classes an unfinished pass has not reached go first.
        self.pass(st);
        let (class, need) = match st.shapes.get_key_value(need.as_slice()) {
            Some((shape, &class)) => (class, Arc::clone(shape)),
            None => {
                let need: Shape = need.into();
                st.sync_head();
                if let Verdict::Grant { charged } =
                    st.head.judge(&self.capacity, &st.held, &need, None)
                {
                    if charged {
                        st.head.charge(&self.capacity, &st.held, &need);
                    }
                    let admitted = match st.head.who {
                        Some(_) => Admitted::Pass,
                        None => Admitted::Arrival,
                    };
                    take(&mut st.held, &need);
                    drop(guard);
                    return Acquire {
                        state: AcquireState::Ready(Some(Reservation {
                            lane: Arc::clone(self),
                            granted: need,
                            origin: None,
                            clamped,
                            admitted,
                            armed: true,
                        })),
                    };
                }
                let class = st.next_class;
                st.next_class = st.next_class.wrapping_add(1);
                (class, need)
            }
        };
        let (seq, rx) = st.push(class, need, clamped);
        drop(guard);
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

    /// Offer `class`'s oldest waiter what is free. The head is granted when
    /// it fits. Any other waiter must also not take what a protected head
    /// needs; a grant that does not leave an unprotected head's need is
    /// charged to it.
    ///
    /// Runs under the lock. The grant is built here and counted as held only
    /// once it is sent: a send that fails hands it back, which is disarmed,
    /// so no armed `Reservation` is dropped under the lock and nothing is
    /// charged for a waiter that was not there.
    fn offer(self: &Arc<Self>, st: &mut State, class: u64, how: Admitted) -> Offer {
        st.sync_head();
        let (seq, need, charged) = {
            let Some(c) = st.classes.get(&class) else {
                return Offer::Refused;
            };
            let Some(&seq) = c.waiters.first() else {
                return Offer::Refused;
            };
            let Verdict::Grant { charged } =
                st.head.judge(&self.capacity, &st.held, &c.need, Some(seq))
            else {
                return Offer::Refused;
            };
            (seq, Arc::clone(&c.need), charged)
        };
        let is_head = st.head.seq() == Some(seq);
        let Some(waiter) = st.remove(seq) else {
            return Offer::Refused;
        };
        let reservation = Reservation {
            lane: Arc::clone(self),
            granted: Arc::clone(&need),
            origin: None,
            clamped: waiter.clamped,
            admitted: if is_head { Admitted::Head } else { how },
            armed: true,
        };
        if let Err(mut unobserved) = waiter.tx.send(reservation) {
            unobserved.armed = false;
            return Offer::Gone;
        }
        if charged {
            st.head.charge(&self.capacity, &st.held, &need);
        }
        take(&mut st.held, &need);
        Offer::Granted
    }

    /// Steps 2 and 3 of the rule, after something changed that could let a
    /// waiter fit: capacity came back, or the head left. Grants the head
    /// while it fits, then starts a pass over the other classes.
    fn walk(self: &Arc<Self>, st: &mut State) {
        while let Some(class) = st.head_class() {
            if self.offer(st, class, Admitted::Head) == Offer::Refused {
                break;
            }
        }
        st.untested = Untested::Wrap { stop: st.cursor };
        self.pass(st);
    }

    /// One batch of the pass over the waiting classes, if one is unfinished:
    /// each class's oldest waiter is offered what is free, the class with the
    /// older waiter first. A class that is granted is offered again; a class
    /// that is refused is done for this pass, because what is free only
    /// shrinks during it.
    fn pass(self: &Arc<Self>, st: &mut State) {
        if matches!(st.untested, Untested::Nothing) {
            return;
        }
        let Some(head_class) = st.head_class() else {
            st.untested = Untested::Nothing;
            return;
        };
        st.next_batch(head_class);
        let mut batch = std::mem::take(&mut st.batch);
        batch.sort_unstable();
        for &(_, class) in &batch {
            #[cfg(test)]
            {
                st.visited = st.visited.saturating_add(1);
            }
            while self.offer(st, class, Admitted::Pass) != Offer::Refused {}
        }
        st.batch = batch;
    }

    /// Return `granted`, part or all of a grant admitted as `shape`. The
    /// oldest waiter of that shape is offered the slot first; unless it took
    /// it and the head is who it was, the walk runs.
    fn release(self: &Arc<Self>, granted: &[(usize, u64)], shape: &[(usize, u64)]) {
        // An empty grant frees nothing, so it can admit nobody.
        if granted.is_empty() {
            return;
        }
        let mut guard = self.change();
        let st = &mut *guard;
        give(&self.capacity, &mut st.held, granted);
        if !self.frees_bounded(granted) {
            // Nothing a waiter waits for came back.
            self.pass(st);
            return;
        }
        let head = st.head_seq();
        let mut handed = false;
        if let Some(class) = st.shapes.get(shape).copied() {
            loop {
                match self.offer(st, class, Admitted::Handoff) {
                    Offer::Gone => {}
                    Offer::Granted => {
                        handed = true;
                        break;
                    }
                    Offer::Refused => break,
                }
            }
        }
        if handed && st.head_seq() == head {
            // The slot went to a run of the shape that released it, which
            // takes at least what came back: nothing is freer than before
            // the release, so nobody else can newly fit.
            self.pass(st);
        } else {
            self.walk(st);
        }
    }

    /// Leave the queue (no-op once granted). Only the head leaving can let
    /// another waiter start: the next head has no debt, so it protects
    /// nothing yet.
    fn cancel(self: &Arc<Self>, seq: u64) {
        let mut guard = self.change();
        let st = &mut *guard;
        let was_head = st.head_seq() == Some(seq);
        if st.remove(seq).is_none() {
            return;
        }
        if was_head {
            self.walk(st);
        } else {
            self.pass(st);
        }
    }

    fn wait_info(&self, seq: u64) -> Option<WaitInfo> {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        st.sync_head();
        let waiter = st.waiters.get(&seq)?;
        let need = &st.classes.get(&waiter.class)?.need;
        // It fits and still waits: only a protected head holds it back.
        let verdict = st.head.judge(&self.capacity, &st.held, need, Some(seq));
        let yielding_to = st
            .head
            .need()
            .filter(|_| verdict == Verdict::Yields)
            .and_then(|head| {
                need.iter().find_map(|&(idx, n)| {
                    let wanted = amount_in(head, idx);
                    if leaves(&self.capacity, &st.held, idx, n, wanted) {
                        return None;
                    }
                    Some(YieldingTo {
                        pool: self.names.get(idx)?.clone(),
                        need: wanted,
                    })
                })
            });
        Some(WaitInfo {
            ahead: st.waiters.range(..seq).count(),
            pools: self.needs(&st.held, need, false),
            since: waiter.since,
            yielding_to,
        })
    }

    fn snapshot(&self) -> LaneSnapshot {
        let mut guard = self.state.lock();
        let st = &mut *guard;
        st.sync_head();
        // Per pool, the waiters whose need takes some of it: one pass over
        // the classes, not the waiters.
        let mut waiting = vec![0u64; self.names.len()];
        for class in st.classes.values() {
            let n = class.waiters.len() as u64;
            for &(idx, _) in class.need.iter() {
                if let Some(w) = waiting.get_mut(idx) {
                    *w = w.saturating_add(n);
                }
            }
        }
        let pools = self
            .names
            .iter()
            .zip(self.capacity.iter().zip(st.held.iter()))
            .zip(waiting)
            .map(|((name, (&capacity, &held)), waiting)| PoolSnapshot {
                pool: name.clone(),
                unit: Unit::default_for(name.as_str()),
                capacity,
                held,
                waiting,
            })
            .collect();
        let head = st
            .waiters
            .values()
            .next()
            .zip(st.head.need())
            .map(|(w, need)| HeadSnapshot {
                unmet: self.needs(&st.held, need, true),
                since: w.since,
                overtaken: need
                    .iter()
                    .zip(&st.head.debt)
                    .filter_map(|(&(idx, n), &by)| {
                        Some(Overtaken {
                            pool: self.names.get(idx)?.clone(),
                            by,
                            limit: patience(self.capacity.get(idx).copied(), n)?,
                        })
                    })
                    .collect(),
            });
        LaneSnapshot {
            pools,
            queued: st.waiters.len(),
            head,
            removals_pending: self.removals.load(Ordering::Acquire),
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

/// What `shape` takes of pool `idx`; 0 when it does not name it.
fn amount_in(shape: &[(usize, u64)], idx: usize) -> u64 {
    shape
        .iter()
        .find_map(|&(i, n)| (i == idx).then_some(n))
        .unwrap_or(0)
}

/// Whether granting `grant` of pool `idx`, which fits, leaves `head` of it
/// free. An unbounded pool always does. Written as subtractions from what is
/// free, like [`fits`], so no sum can overflow.
fn leaves(capacity: &[Limit], held: &[u64], idx: usize, grant: u64, head: u64) -> bool {
    match (capacity.get(idx), held.get(idx)) {
        (Some(&Limit::Bounded(cap)), Some(&h)) => {
            head <= cap.saturating_sub(h).saturating_sub(grant)
        }
        _ => true,
    }
}

/// Whether a grant of `grant`, which fits, leaves room for the head: on every
/// bounded pool the grant takes from, the head's whole need still fits beside
/// it.
fn leaves_room(
    capacity: &[Limit],
    held: &[u64],
    grant: &[(usize, u64)],
    head: &[(usize, u64)],
) -> bool {
    grant
        .iter()
        .all(|&(idx, n)| leaves(capacity, held, idx, n, amount_in(head, idx)))
}

/// By how much a head needing `need` of a pool is overtaken there before it
/// is protected: [`PATIENCE`] times its need, at most [`PATIENCE_POOLS`]
/// times the pool. `None` for an unbounded pool, where every grant leaves
/// room.
fn patience(limit: Option<Limit>, need: u64) -> Option<u64> {
    let capacity = limit?.bounded()?;
    Some(
        need.saturating_mul(PATIENCE)
            .min(capacity.saturating_mul(PATIENCE_POOLS)),
    )
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
    /// Shared with the class it was granted from.
    granted: Shape,
    /// The shape this was admitted as, once `granted` is only a part of it
    /// ([`split_off`](Self::split_off)). Each part, when it is released, is
    /// offered to that shape's oldest waiter first.
    origin: Option<Shape>,
    clamped: bool,
    admitted: Admitted,
    /// False only for a grant handed back by a failed send, which was never
    /// counted as held.
    armed: bool,
}

impl Reservation {
    /// Whether some amount was clamped to its pool's capacity.
    pub fn clamped(&self) -> bool {
        self.clamped
    }

    /// How this grant came about.
    pub fn admitted(&self) -> Admitted {
        self.admitted
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
        let part = self.granted.iter().copied().find(|&(i, _)| i == idx)?;
        let origin = Arc::clone(self.origin.get_or_insert_with(|| Arc::clone(&self.granted)));
        self.granted = self
            .granted
            .iter()
            .copied()
            .filter(|&(i, _)| i != idx)
            .collect();
        Some(Reservation {
            lane: Arc::clone(&self.lane),
            granted: Arc::from([part]),
            origin: Some(origin),
            clamped: false,
            admitted: self.admitted,
            armed: self.armed,
        })
    }

    /// Hold this grant for a sandbox removal: counted in
    /// [`ResourcePool::removals_pending`] from now until the returned guard
    /// drops, which uncounts it and then releases the grant, so a waiter the
    /// release admits never sees its own wait's cause still counted.
    pub fn into_removal(self) -> PendingRemoval {
        self.lane.removals.fetch_add(1, Ordering::AcqRel);
        self.lane.generation.fetch_add(1, Ordering::Release);
        PendingRemoval { reservation: self }
    }
}

/// A [`Reservation`] held until a finished run's sandbox has been removed
/// ([`Reservation::into_removal`]). Dropping it ends the removal's count and
/// returns the grant.
#[derive(Debug)]
pub struct PendingRemoval {
    reservation: Reservation,
}

impl Drop for PendingRemoval {
    fn drop(&mut self) {
        // Before the field drops, which is the release (and moves the
        // generation again, under the lock).
        let lane = &self.reservation.lane;
        lane.removals.fetch_sub(1, Ordering::AcqRel);
        lane.generation.fetch_add(1, Ordering::Release);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.armed {
            let shape = self.origin.as_deref().unwrap_or(&self.granted);
            self.lane.release(&self.granted, shape);
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

/// A queued request at one instant: how many were queued before it, and per
/// pool what it needs against what is free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitInfo {
    /// How many waiters arrived before this one and still wait. A snapshot,
    /// and not a promise that they start first: a waiter that fits what is
    /// free can start ahead of an older one (see the module docs).
    pub ahead: usize,
    pub pools: Vec<PoolNeed>,
    pub since: Instant,
    /// Set when this request fits what is free and still waits: the oldest
    /// waiter has been overtaken as far as it may be, and this grant would
    /// take what it needs.
    pub yielding_to: Option<YieldingTo>,
}

/// What a waiter that fits is held back for: the oldest waiter's need of the
/// first pool where both cannot fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YieldingTo {
    pub pool: PoolName,
    /// What the oldest waiter needs of `pool`.
    pub need: u64,
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
    /// Finished runs whose sandbox removal has not returned yet, each still
    /// holding its `sandbox` slot ([`Reservation::into_removal`]).
    pub removals_pending: u64,
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
    /// How many queued requests need some of this pool.
    pub waiting: u64,
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
    /// Per bounded pool the head needs: what has started ahead of it there,
    /// against how much may.
    pub overtaken: Vec<Overtaken>,
}

impl HeadSnapshot {
    /// Whether the head has been overtaken as far as it may be: nothing more
    /// is admitted that takes what it needs, so its pools drain until it
    /// fits.
    pub fn draining(&self) -> bool {
        self.overtaken.iter().any(|o| o.by >= o.limit)
    }
}

/// How far the oldest waiter has been overtaken on one pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overtaken {
    pub pool: PoolName,
    /// The amounts granted behind it that did not leave its need of `pool`.
    pub by: u64,
    /// Once `by` reaches this, it is overtaken no further.
    pub limit: u64,
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

    fn cpus(n: u64) -> Demand {
        req(&[("cpu", n * CPU)])
    }

    /// How many classes the walk has tested so far.
    fn visited(p: &ResourcePool) -> u64 {
        p.main.state.lock().visited
    }

    /// Whether no pass is unfinished.
    fn pass_done(p: &ResourcePool) -> bool {
        matches!(p.main.state.lock().untested, Untested::Nothing)
    }

    /// How many classes wait. Each is findable by its shape, and no other
    /// shape is.
    fn classes(p: &ResourcePool) -> usize {
        let st = p.main.state.lock();
        assert_eq!(st.classes.len(), st.shapes.len());
        st.classes.len()
    }

    /// The head's debt and limit on `pool`.
    fn overtaken(p: &ResourcePool, pool: &str) -> (u64, u64) {
        let head = p.snapshot().main.head.expect("a head");
        let o = head
            .overtaken
            .iter()
            .find(|o| o.pool.as_str() == pool)
            .expect("the head needs the pool");
        (o.by, o.limit)
    }

    fn draining(p: &ResourcePool) -> bool {
        p.snapshot().main.head.is_some_and(|h| h.draining())
    }

    /// Arrivals of `r` that start at once, each released before the next,
    /// until one has to queue (and leaves).
    fn passes(p: &Arc<ResourcePool>, r: &Demand) -> u64 {
        let mut n = 0;
        loop {
            let mut a = p.acquire(r).unwrap();
            match (&mut a).now_or_never() {
                Some(g) => drop(g.unwrap()),
                None => return n,
            }
            n += 1;
            assert!(n < 100_000, "never refused");
        }
    }

    /// Whether a full walk would grant nothing: no pass is unfinished, the
    /// head does not fit, and every other class's oldest waiter either does
    /// not fit or would take what a protected head needs.
    fn settled(p: &ResourcePool) -> bool {
        let lane = &p.main;
        let mut guard = lane.state.lock();
        let st = &mut *guard;
        st.sync_head();
        matches!(st.untested, Untested::Nothing)
            && st.classes.values().all(|c| {
                let Some(&oldest) = c.waiters.first() else {
                    return false;
                };
                if !fits(&lane.capacity, &st.held, &c.need) {
                    return true;
                }
                let Some(head) = st.head.need() else {
                    return false;
                };
                st.head.seq() != Some(oldest)
                    && !leaves_room(&lane.capacity, &st.held, &c.need, head)
                    && st.head.protected(&lane.capacity)
            })
    }

    /// A waiter whose receiver is gone and which never left the queue: what
    /// a grant finds when it races the waiter's drop.
    fn dead(a: Acquire) -> WaitGuard {
        match a.state {
            AcquireState::Waiting { guard, rx } => {
                drop(rx);
                guard
            }
            AcquireState::Ready(_) => panic!("must wait"),
        }
    }

    /// splitmix64: a seeded sequence, the same on every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    #[test]
    fn grants_in_arrival_order() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let held = now(&p, &req(&[("cpu", 4 * CPU)]));
        let mut big = queued(&p, &req(&[("cpu", 3 * CPU)]));
        // Nothing is free, so neither fits: a follower that did would start.
        let mut small = queued(&p, &req(&[("cpu", CPU)]));
        assert_eq!(free(&p, "cpu"), 0, "a waiter holds nothing");

        drop(held);
        let big = granted(&mut big).expect("the head first");
        assert_eq!(big.admitted(), Admitted::Head);
        let small = granted(&mut small).expect("then its follower");
        assert_eq!(small.admitted(), Admitted::Head, "the oldest once big left");
        assert_eq!(free(&p, "cpu"), 0);
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
        let p = pool(&[("cpu", 2 * CPU), ("docker", 1)]);
        let _docker = now(&p, &req(&[("cpu", 0), ("docker", 1)]));
        let passes = |n| (0..n).map(|_| drop(now(&p, &req(&[])))).count();
        // Waits for `docker`, and is overtaken by cpu runs until its cpu
        // debt reaches its limit: min(16 × 2, 4 × 2) cpu.
        let head = queued(&p, &req(&[("cpu", 2 * CPU), ("docker", 1)]));
        assert_eq!(passes(8), 8);
        // Fits, but would take the cpu the head is now owed.
        let mut follower = queued(&p, &req(&[]));
        drop(head);
        assert!(
            granted(&mut follower).is_some(),
            "the head's drop runs the walk"
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

        // The head's limit (16 times its need, 4 times the pool), its debt
        // and the leaves-room test all saturate: a head needing the whole
        // pool is overtaken twice by runs one short of it, then protected.
        let p = pool(&[("cpu", u64::MAX)]);
        let wide = req(&[("cpu", u64::MAX - 1)]);
        let first = now(&p, &wide);
        let mut head = queued(&p, &req(&[("cpu", u64::MAX)]));
        let mut followers = [queued(&p, &wide), queued(&p, &wide), queued(&p, &wide)];
        drop(first);
        let [a, b, c] = &mut followers;
        let a = granted(a).expect("handed off");
        assert_eq!(overtaken(&p, "cpu"), (u64::MAX - 1, u64::MAX));
        drop(a);
        let b = granted(b).expect("handed off: the debt is one short");
        assert_eq!(overtaken(&p, "cpu"), (u64::MAX, u64::MAX), "saturated");
        assert!(draining(&p));
        drop(b);
        assert!(granted(c).is_none(), "the head is protected");
        let head = granted(&mut head).expect("it fits once the pool is empty");
        assert_eq!(head.admitted(), Admitted::Head);
        drop(head);
        drop(granted(c).expect("then the follower"));
        assert_eq!(free(&p, "cpu"), u64::MAX);
    }

    #[test]
    fn snapshot_reports_held_free_and_head() {
        let p = pool(&[("cpu", 4 * CPU), ("memory", 8 * GIB)]);
        let idle = p.snapshot();
        assert_eq!(idle.main.queued, 0);
        assert!(idle.main.head.is_none());

        let _held = now(&p, &req(&[("cpu", 3 * CPU), ("memory", GIB)]));
        let head = queued(&p, &req(&[("cpu", 2 * CPU), ("memory", GIB)]));
        // Two cpu with one free: a follower that fit would not queue.
        let follower = queued(&p, &req(&[("cpu", 2 * CPU)]));
        let s = p.snapshot();
        assert_eq!(
            s.main.pools,
            [
                PoolSnapshot {
                    pool: name("cpu"),
                    unit: Unit::Cpu,
                    capacity: Limit::Bounded(4 * CPU),
                    held: 3 * CPU,
                    waiting: 2,
                },
                PoolSnapshot {
                    pool: name("memory"),
                    unit: Unit::Bytes,
                    capacity: Limit::Bounded(8 * GIB),
                    held: GIB,
                    // The head needs memory too; the follower does not.
                    waiting: 1,
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
        // What the engine computes is never empty: a percentage that floors
        // to 0, and a default taken of `cpu: 0`. What the user wrote is kept:
        // an explicit 0 is "the run sets it", as for any pool.
        assert_eq!(
            sandbox_of(&sandbox(Capacity::Percent(10)), 1),
            Limit::Bounded(1),
            "10% of 4 x 1 floors to 0, raised to 1"
        );
        assert_eq!(
            sandbox_of(&cpu(Capacity::Amount(0)), 1),
            Limit::Bounded(1),
            "a default of cpu: 0 is still one sandbox"
        );
        assert_eq!(
            sandbox_of(&sandbox(Capacity::Amount(0)), 1),
            Limit::Bounded(0),
            "an explicit 0 stays 0"
        );
        let p = ResourcePool::new(resolve_capacities(&sandbox(Capacity::Amount(0)), 1, || 0));
        assert_eq!(
            p.acquire_with(&Default::default(), &[(name("sandbox"), 1)])
                .unwrap_err(),
            AcquireError::ZeroCapacity {
                pool: name("sandbox")
            },
            "a managed run fails at admission, like any pool's 0"
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

    /// Every pool is one entry of the grant, its whole amount: a pool both
    /// declared and implicit, a pool named twice in `implicit`, and an
    /// implicit `cpu` beside the undeclared default each sum into one entry
    /// (two entries would each be checked against the capacity alone, and be
    /// granted past it together).
    #[test]
    fn implicit_amounts_sum_into_one_entry_per_pool() {
        let p = pool(&[("cpu", 4 * CPU), ("gpu", 4), ("sandbox", 4)]);
        let acquire = |r: &Demand, implicit: &[(&str, u64)]| {
            let implicit: Vec<(PoolName, u64)> =
                implicit.iter().map(|&(n, a)| (name(n), a)).collect();
            p.acquire_with(r, &implicit)
                .unwrap()
                .now_or_never()
                .expect("granted at once")
                .unwrap()
        };
        let amounts = |g: &Reservation| {
            let mut v: Vec<(String, u64)> =
                g.iter().map(|(n, a)| (n.as_str().to_owned(), a)).collect();
            v.sort();
            v
        };
        let both = acquire(&req(&[("gpu", 1)]), &[("gpu", 2)]);
        assert_eq!(
            amounts(&both),
            [("cpu".to_owned(), CPU), ("gpu".to_owned(), 3)],
            "declared plus implicit"
        );
        drop(both);
        let twice = acquire(&req(&[]), &[("sandbox", 1), ("sandbox", 2)]);
        assert_eq!(
            amounts(&twice),
            [("cpu".to_owned(), CPU), ("sandbox".to_owned(), 3)],
            "an implicit pool named twice"
        );
        drop(twice);
        let cpu = acquire(&req(&[]), &[("cpu", CPU)]);
        assert_eq!(
            amounts(&cpu),
            [("cpu".to_owned(), 2 * CPU)],
            "an implicit cpu on top of the undeclared default"
        );
        drop(cpu);
        let declared_cpu = acquire(&req(&[("cpu", 2 * CPU)]), &[("cpu", CPU)]);
        assert_eq!(amounts(&declared_cpu), [("cpu".to_owned(), 3 * CPU)]);
        drop(declared_cpu);
        assert_eq!(
            (free(&p, "cpu"), free(&p, "gpu"), free(&p, "sandbox")),
            (4 * CPU, 4, 4)
        );
    }

    // ---- Step 3: a waiter that fits passes the head -----------------------

    #[test]
    fn a_run_needing_another_pool_passes_a_waiting_head() {
        let p = pool(&[("cpu", 4 * CPU), ("docker", 1)]);
        let cpu = now(&p, &cpus(4));
        let _docker = now(&p, &req(&[("cpu", 0), ("docker", 1)]));
        let mut head = queued(&p, &req(&[("cpu", CPU), ("docker", 1)]));
        let mut waiter = queued(&p, &cpus(1));

        drop(cpu);
        assert!(granted(&mut head).is_none(), "docker is still held");
        let run = granted(&mut waiter).expect("cpu is free and the head keeps its own");
        assert_eq!(run.admitted(), Admitted::Pass);
        assert_eq!(overtaken(&p, "cpu").0, 0, "it left the head's cpu");
        assert!(settled(&p));
    }

    #[test]
    fn a_run_that_fits_passes_an_unprotected_head() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let _stays = now(&p, &cpus(1));
        let leaves = now(&p, &cpus(3));
        let mut head = queued(&p, &cpus(4));
        let mut waiter = queued(&p, &cpus(1));

        drop(leaves);
        assert!(granted(&mut head).is_none(), "3 of the 4 it needs are free");
        let run = granted(&mut waiter).expect("it fits the 3 free");
        assert_eq!(run.admitted(), Admitted::Pass);
        assert_eq!(overtaken(&p, "cpu"), (CPU, 16 * CPU), "min(16 × 4, 4 × 4)");
        assert!(settled(&p));
    }

    #[test]
    fn the_walk_serves_the_older_class_first() {
        let p = pool(&[("cpu", 4 * CPU), ("tokens", 4)]);
        let _stays = now(&p, &cpus(3));
        // A shape nobody waits in, so its release is not handed off.
        let leaves = now(&p, &req(&[("cpu", CPU), ("tokens", 2)]));
        let _head = queued(&p, &cpus(4));
        let plain = cpus(1);
        let token = req(&[("cpu", CPU), ("tokens", 1)]);
        // The `plain` class is created first, then loses its oldest waiter:
        // by class id it comes first, by oldest waiter second.
        let first = queued(&p, &plain);
        let mut older = queued(&p, &token);
        let mut younger = queued(&p, &plain);
        drop(first);

        drop(leaves);
        let _run = granted(&mut older).expect("one cpu, to the older waiter");
        assert!(granted(&mut younger).is_none());
        assert!(settled(&p));
    }

    #[test]
    fn a_granted_class_is_tried_again() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let _stays = now(&p, &cpus(4));
        let leaves = now(&p, &cpus(4));
        let mut head = queued(&p, &cpus(8));
        let mut waiters: Vec<Acquire> = (0..3).map(|_| queued(&p, &cpus(1))).collect();

        drop(leaves);
        assert!(granted(&mut head).is_none());
        let runs: Vec<Reservation> = waiters.iter_mut().filter_map(granted).collect();
        assert_eq!(runs.len(), 3, "one release admits the class three times");
        assert_eq!(overtaken(&p, "cpu").0, 3 * CPU);
        assert_eq!(free(&p, "cpu"), CPU);
    }

    // ---- Step 1: hand-off -------------------------------------------------

    #[test]
    fn a_release_goes_to_the_next_waiter_of_its_shape() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let wide = now(&p, &cpus(8));
        let mut narrow: Vec<Acquire> = (0..3).map(|_| queued(&p, &cpus(1))).collect();
        let mut next_wide = queued(&p, &cpus(8));
        let before = visited(&p);

        drop(wide);
        let run = granted(&mut next_wide).expect("the slot stays with its shape");
        assert_eq!(run.admitted(), Admitted::Handoff);
        assert!(
            narrow.iter_mut().all(|w| granted(w).is_none()),
            "the older narrow runs wait for the next drain"
        );
        assert_eq!(overtaken(&p, "cpu"), (8 * CPU, 16 * CPU));
        assert_eq!(visited(&p), before, "a hand-off walks nothing");
        assert!(settled(&p));
    }

    #[test]
    fn a_hand_off_to_the_head_changes_the_head_and_walks() {
        let p = pool(&[("cpu", 2 * CPU), ("docker", 1)]);
        let shape = req(&[("cpu", CPU), ("docker", 1)]);
        let first = now(&p, &shape);
        let mut head = queued(&p, &shape);
        let _next = queued(&p, &cpus(2));
        // The head waits for `docker` and is overtaken on cpu up to its
        // limit, min(16 × 1, 4 × 2); the next cpu run is then held back.
        assert_eq!(passes(&p, &cpus(1)), 8);
        let mut held_back = queued(&p, &cpus(1));
        let before = visited(&p);

        drop(first);
        let run = granted(&mut head).expect("its shape's slot came back");
        assert_eq!(run.admitted(), Admitted::Head, "the oldest: not charged");
        let passer = granted(&mut held_back).expect("the new head protects nothing yet");
        assert_eq!(passer.admitted(), Admitted::Pass);
        assert!(visited(&p) > before, "a head change walks");
        assert_eq!(overtaken(&p, "cpu"), (CPU, 8 * CPU), "from zero");
        assert!(settled(&p));
    }

    #[test]
    fn a_hand_off_stops_when_the_head_is_protected() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let wide = now(&p, &cpus(8));
        let mut head = queued(&p, &cpus(1));
        let mut wides: Vec<Acquire> = (0..3).map(|_| queued(&p, &cpus(8))).collect();
        let [a, b, c] = wides.as_mut_slice() else {
            panic!("three");
        };

        drop(wide);
        let a = granted(a).expect("first hand-off");
        assert!(!draining(&p), "8 of 16");
        drop(a);
        let b = granted(b).expect("second hand-off");
        assert!(draining(&p), "16 of 16");
        assert!(granted(&mut head).is_none());
        drop(b);
        assert!(granted(c).is_none(), "the third would take the head's cpu");
        let head = granted(&mut head).expect("the head starts in the drained pool");
        assert_eq!(head.admitted(), Admitted::Head);
        drop(head);
        assert!(granted(c).is_some(), "then the third");
    }

    #[test]
    fn a_clamped_release_hands_off_to_an_unclamped_waiter() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let oversize = now(&p, &cpus(16));
        assert!(oversize.clamped());
        let _head = queued(&p, &cpus(1));
        let mut exact = queued(&p, &cpus(8));
        let mut oversize_too = queued(&p, &cpus(16));
        assert_eq!(classes(&p), 2, "clamped to 8, it is the shape of an 8");

        drop(oversize);
        let run = granted(&mut exact).expect("the clamped run's slot");
        assert_eq!(run.admitted(), Admitted::Handoff);
        assert!(!run.clamped(), "it asked for what it got");
        drop(run);
        let run = granted(&mut oversize_too).expect("and on to the next of the shape");
        assert!(run.clamped(), "each grant keeps its own flag");
    }

    /// A managed run's grant is released in two halves, its cpu when the run
    /// ends and its `sandbox` slot once the sandbox is removed. Each half is
    /// offered to the shape the run was admitted as, not to the shape of the
    /// half.
    #[test]
    fn each_half_of_a_split_grant_hands_off_to_its_shape() {
        let p = pool(&[("cpu", 2 * CPU), ("sandbox", 2)]);
        let slot = [(PoolName::sandbox(), 1)];
        let mut run = p
            .acquire_with(&cpus(1), &slot)
            .unwrap()
            .now_or_never()
            .expect("granted at once")
            .unwrap();
        let _other = now(&p, &cpus(1));
        let _head = queued(&p, &cpus(2));
        let mut next = p.acquire_with(&cpus(1), &slot).unwrap();
        assert!((&mut next).now_or_never().is_none(), "cpu is full");
        let removal = run.split_off(PoolName::SANDBOX).expect("the slot");
        assert_eq!(removal.admitted(), run.admitted());
        let before = visited(&p);

        drop(run);
        let next = granted(&mut next).expect("the cpu half goes to the managed waiter");
        assert_eq!(next.admitted(), Admitted::Handoff);
        assert_eq!(visited(&p), before);
        drop(removal);
        assert_eq!(free(&p, "sandbox"), 1);
        assert!(settled(&p));
    }

    /// A managed run: `n` cpu and the engine's `sandbox: 1`.
    fn managed(p: &Arc<ResourcePool>, n: u64) -> Acquire {
        p.acquire_with(&cpus(n), &[(PoolName::sandbox(), 1)])
            .unwrap()
    }

    /// The `sandbox` half is offered to the run's shape too: a managed
    /// waiter short of a slot only, behind an older waiter, takes the slot
    /// as a hand-off, with no walk.
    #[test]
    fn the_sandbox_half_of_a_split_grant_hands_off_to_its_shape() {
        let p = pool(&[("cpu", 3 * CPU), ("sandbox", 1)]);
        let mut run = managed(&p, 1).now_or_never().unwrap().unwrap();
        let _other = now(&p, &cpus(1));
        let _head = queued(&p, &cpus(2));
        let mut next = managed(&p, 1);
        assert!(
            (&mut next).now_or_never().is_none(),
            "a cpu is free, no slot is"
        );
        let removal = run.split_off(PoolName::SANDBOX).unwrap();
        let before = visited(&p);

        drop(removal);
        let next = granted(&mut next).expect("the slot goes to the managed waiter");
        assert_eq!(next.admitted(), Admitted::Handoff);
        assert_eq!(visited(&p), before, "a hand-off walks nothing");
        assert_eq!((free(&p, "cpu"), free(&p, "sandbox")), (0, 0));
        drop(run);
        assert!(settled(&p));
    }

    /// The cpu half's hand-off is judged like any other: a waiter of the
    /// run's shape that would take a protected head's cpu is refused, and
    /// the head starts in what came back.
    #[test]
    fn a_split_hand_off_yields_to_a_protected_head() {
        let p = pool(&[("cpu", 8 * CPU), ("sandbox", 8)]);
        let _stays = now(&p, &cpus(6));
        let mut run = managed(&p, 1).now_or_never().unwrap().unwrap();
        let mut head = queued(&p, &cpus(2));
        // Runs of the managed shape pass it, one cpu each, until 16 × 2.
        for _ in 0..32 {
            let mut pass = managed(&p, 1).now_or_never().unwrap().unwrap();
            let slot = pass.split_off(PoolName::SANDBOX).unwrap();
            drop((pass, slot));
        }
        assert!(draining(&p));
        let mut next = managed(&p, 1);
        assert!(
            (&mut next).now_or_never().is_none(),
            "the head is protected"
        );
        let removal = run.split_off(PoolName::SANDBOX).unwrap();

        drop(run);
        assert!(
            granted(&mut next).is_none(),
            "it fits, and would take the head's cpu"
        );
        let head = granted(&mut head).expect("the head starts on the release");
        assert_eq!(head.admitted(), Admitted::Head);
        drop(removal);
        assert!(granted(&mut next).is_none(), "the head holds the cpu");
        drop(head);
        assert_eq!(granted(&mut next).unwrap().admitted(), Admitted::Head);
    }

    /// The cpu half comes back while the run's `sandbox` slot is still held:
    /// the waiter of its shape does not fit, so the walk runs and grants who
    /// does.
    #[test]
    fn a_split_hand_off_that_does_not_fit_walks() {
        let p = pool(&[("cpu", 2 * CPU), ("sandbox", 1)]);
        let mut run = managed(&p, 1).now_or_never().unwrap().unwrap();
        let _other = now(&p, &cpus(1));
        let _head = queued(&p, &cpus(2));
        let mut next = managed(&p, 1);
        assert!((&mut next).now_or_never().is_none(), "no slot");
        let mut half = queued(&p, &req(&[("cpu", CPU / 2)]));
        let removal = run.split_off(PoolName::SANDBOX).unwrap();
        let before = visited(&p);

        drop(run);
        assert!(granted(&mut next).is_none(), "the slot is still held");
        assert_eq!(visited(&p) - before, 2, "the walk tested both classes");
        let half = granted(&mut half).expect("the walk grants who fits");
        assert_eq!(half.admitted(), Admitted::Pass);
        drop(removal);
        assert!(granted(&mut next).is_none(), "its cpu went to the pass");
        assert!(settled(&p));
    }

    /// A gone waiter of the run's shape is skipped, uncharged, and the cpu
    /// half goes to the next of the shape.
    #[test]
    fn a_split_hand_off_skips_a_dead_receiver() {
        let p = pool(&[("cpu", 2 * CPU), ("sandbox", 3)]);
        let mut run = managed(&p, 1).now_or_never().unwrap().unwrap();
        let _other = now(&p, &cpus(1));
        let _head = queued(&p, &cpus(2));
        let _gone = dead(managed(&p, 1));
        let mut next = managed(&p, 1);
        assert!((&mut next).now_or_never().is_none(), "cpu is full");
        let removal = run.split_off(PoolName::SANDBOX).unwrap();
        let before = visited(&p);

        drop(run);
        let next = granted(&mut next).expect("the next of the shape");
        assert_eq!(next.admitted(), Admitted::Handoff);
        assert_eq!(visited(&p), before, "a hand-off walks nothing");
        assert_eq!(overtaken(&p, "cpu").0, CPU, "charged once");
        assert_eq!((free(&p, "cpu"), free(&p, "sandbox")), (0, 1));
        assert_eq!(classes(&p), 1, "the head's");
        drop((removal, next));
        assert!(settled(&p));
    }

    // ---- Sized patience ---------------------------------------------------

    #[test]
    fn the_head_is_protected_at_sixteen_times_its_need() {
        let p = pool(&[("cpu", 16 * CPU)]);
        let _stays = now(&p, &cpus(15));
        let _head = queued(&p, &cpus(2));
        assert_eq!(passes(&p, &cpus(1)), 32, "16 × 2, under 4 × 16");
        assert!(draining(&p));
        assert_eq!(overtaken(&p, "cpu"), (32 * CPU, 32 * CPU));
        assert_eq!(free(&p, "cpu"), CPU, "the freed cpu stays free");
        assert!(settled(&p));
    }

    #[test]
    fn a_small_head_is_overtaken_less_than_a_wide_one() {
        // How many runs are handed a slot ahead of a head needing `head`.
        let overtakes = |head: Demand| {
            let p = pool(&[("cpu", 4 * CPU), ("tokens", 100)]);
            let shape = req(&[("cpu", CPU), ("tokens", 1)]);
            let _stays = now(&p, &cpus(3));
            let mut running = now(&p, &shape);
            let mut head = queued(&p, &head);
            let waiters: Vec<Acquire> = (0..20).map(|_| queued(&p, &shape)).collect();
            let mut waiters = waiters.into_iter();
            let mut passed = 0;
            loop {
                drop(running);
                if granted(&mut head).is_some() {
                    return passed;
                }
                let mut next = waiters.next().expect("the head starts before 20 passed");
                running = granted(&mut next).expect("a hand-off");
                passed += 1;
            }
        };
        assert_eq!(overtakes(req(&[("cpu", CPU / 10)])), 2, "2 ≥ 16 × 0.1");
        assert_eq!(overtakes(cpus(1)), 16);
    }

    #[test]
    fn a_wide_head_is_capped_at_four_pools() {
        for head in [cpus(8), cpus(16)] {
            let p = pool(&[("cpu", 8 * CPU)]);
            let _stays = now(&p, &cpus(7));
            let _head = queued(&p, &head);
            assert_eq!(passes(&p, &cpus(1)), 32, "4 × 8, not 16 × 8");
            assert_eq!(overtaken(&p, "cpu"), (32 * CPU, 32 * CPU));
        }
    }

    #[test]
    fn the_last_pass_may_overshoot_the_limit() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let _stays = now(&p, &cpus(4));
        let _head = queued(&p, &cpus(8));
        for _ in 0..31 {
            drop(now(&p, &cpus(1)));
        }
        assert_eq!(overtaken(&p, "cpu"), (31 * CPU, 32 * CPU));
        drop(now(&p, &cpus(4)));
        assert_eq!(overtaken(&p, "cpu"), (35 * CPU, 32 * CPU), "plus one grant");
        let _refused = queued(&p, &cpus(1));
        assert_eq!(overtaken(&p, "cpu").0, 35 * CPU);
    }

    #[test]
    fn a_grant_that_leaves_room_is_never_charged() {
        let p = pool(&[("cpu", 8 * CPU), ("docker", 1)]);
        let _docker = now(&p, &req(&[("cpu", 0), ("docker", 1)]));
        let _head = queued(&p, &req(&[("cpu", CPU), ("docker", 1)]));
        for _ in 0..200 {
            // Seven cpu at most: the head's one stays free.
            let runs: Vec<Reservation> = (0..7).map(|_| now(&p, &cpus(1))).collect();
            drop(runs);
        }
        assert_eq!(overtaken(&p, "cpu").0, 0);
        assert!(!draining(&p));
        let mut eighth: Vec<Reservation> = (0..7).map(|_| now(&p, &cpus(1))).collect();
        eighth.push(now(&p, &cpus(1)));
        assert_eq!(overtaken(&p, "cpu").0, CPU, "the eighth takes the head's");
    }

    #[test]
    fn debt_is_per_pool_and_the_first_limit_protects() {
        let cpu_run = cpus(1);
        let mem_run = req(&[("cpu", 0), ("memory", GIB)]);
        let lane = || {
            let p = pool(&[("cpu", 2 * CPU), ("memory", 8 * GIB)]);
            let stays = now(&p, &req(&[("cpu", CPU), ("memory", 6 * GIB)]));
            let head = queued(&p, &req(&[("cpu", CPU), ("memory", 4 * GIB)]));
            (p, stays, head)
        };

        let (p, _stays, _head) = lane();
        for _ in 0..3 {
            drop(now(&p, &cpu_run));
        }
        for _ in 0..2 {
            drop(now(&p, &mem_run));
        }
        assert_eq!(overtaken(&p, "cpu"), (3 * CPU, 8 * CPU), "cpu runs only");
        assert_eq!(
            overtaken(&p, "memory"),
            (2 * GIB, 32 * GIB),
            "memory runs only"
        );
        assert_eq!(passes(&p, &cpu_run), 5, "cpu reaches its limit first");
        assert!(draining(&p));
        assert_eq!(overtaken(&p, "memory").0, 2 * GIB);
        assert_eq!(passes(&p, &mem_run), 0, "and protects memory too");

        let (p, _stays, _head) = lane();
        assert_eq!(passes(&p, &mem_run), 32, "here memory does");
        assert_eq!(overtaken(&p, "cpu").0, 0);
        assert_eq!(passes(&p, &cpu_run), 0);
    }

    #[test]
    fn a_pool_the_head_does_not_need_never_protects_it() {
        let p = pool(&[("cpu", 8 * CPU), ("docker", 1)]);
        let docker = req(&[("cpu", 0), ("docker", 1)]);
        let _held = now(&p, &docker);
        let _head = queued(&p, &docker);
        for _ in 0..100 {
            drop(now(&p, &cpus(8)));
        }
        let head = p.snapshot().main.head.unwrap();
        assert_eq!(
            head.overtaken,
            [Overtaken {
                pool: name("docker"),
                by: 0,
                limit: 4,
            }],
            "no debt and no limit on cpu, which it does not need"
        );
        assert!(!head.draining());
    }

    #[test]
    fn a_micro_cpu_head_is_protected_after_one_charged_pass() {
        let p = pool(&[("cpu", CPU)]);
        let first = now(&p, &cpus(1));
        let mut head = queued(&p, &req(&[("cpu", 1)]));
        let mut second = queued(&p, &cpus(1));
        let mut third = queued(&p, &cpus(1));

        drop(first);
        let run = granted(&mut second).expect("handed off");
        assert_eq!(overtaken(&p, "cpu"), (CPU, 16));
        assert!(draining(&p));
        drop(run);
        assert!(granted(&mut third).is_none(), "refused its shape's slot");
        let _head = granted(&mut head).expect("which went to the head");
        assert!(granted(&mut third).is_none(), "one micro-cpu short");
    }

    // ---- A protected head -------------------------------------------------

    #[test]
    fn a_protected_head_starts_on_the_release_it_waited_for() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let one = now(&p, &cpus(1));
        let _stays = now(&p, &cpus(6));
        let mut head = queued(&p, &cpus(2));
        assert_eq!(passes(&p, &cpus(1)), 32);
        let mut held_back = queued(&p, &cpus(1));

        drop(one);
        let run = granted(&mut head).expect("the second cpu it waited for");
        assert_eq!(run.admitted(), Admitted::Head);
        assert!(
            granted(&mut held_back).is_none(),
            "the slot was its own shape's, and still went to the head"
        );
    }

    #[test]
    fn nothing_takes_from_a_protected_heads_pools() {
        let p = pool(&[("cpu", 8 * CPU), ("docker", 1)]);
        let _docker = now(&p, &req(&[("cpu", 0), ("docker", 1)]));
        let _stays = now(&p, &cpus(4));
        // Short of `docker` only: its 4 cpu are free.
        let _head = queued(&p, &req(&[("cpu", 4 * CPU), ("docker", 1)]));
        assert_eq!(passes(&p, &cpus(1)), 32);

        let waiter = queued(&p, &cpus(1));
        assert_eq!(free(&p, "cpu"), 4 * CPU, "it would leave 3 of the head's 4");
        assert!(settled(&p));
        drop(waiter);
    }

    #[test]
    fn the_next_head_starts_with_no_debt() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let one = now(&p, &cpus(1));
        let _stays = now(&p, &cpus(6));
        let mut first = queued(&p, &cpus(2));
        let _second = queued(&p, &cpus(3));
        assert_eq!(passes(&p, &cpus(1)), 32);

        drop(one);
        let run = granted(&mut first).expect("the protected head");
        assert_eq!(overtaken(&p, "cpu"), (0, 32 * CPU), "min(16 × 3, 4 × 8)");
        drop(run);
        drop(now(&p, &cpus(1)));
        assert_eq!(overtaken(&p, "cpu").0, CPU, "overtaken again");
    }

    #[test]
    fn cancelling_the_head_moves_the_protection() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let _stays = now(&p, &cpus(7));
        let first = queued(&p, &cpus(2));
        let _second = queued(&p, &cpus(3));
        assert_eq!(passes(&p, &cpus(1)), 32);
        let mut held_back = queued(&p, &cpus(1));
        let behind = queued(&p, &cpus(4));
        let before = visited(&p);

        drop(behind);
        assert_eq!(
            visited(&p),
            before,
            "a waiter behind the head tests nothing"
        );
        assert!(granted(&mut held_back).is_none());

        drop(first);
        let run = granted(&mut held_back).expect("the new head protects nothing yet");
        assert_eq!(run.admitted(), Admitted::Pass);
        assert!(visited(&p) > before, "the head leaving walks");
        assert_eq!(overtaken(&p, "cpu"), (CPU, 32 * CPU), "from zero");
        assert!(settled(&p));
    }

    // ---- Classes ----------------------------------------------------------

    #[test]
    fn one_shape_is_granted_in_arrival_order() {
        use std::collections::VecDeque;
        let p = pool(&[("cpu", 4 * CPU)]);
        let mut running: VecDeque<Reservation> = (0..4).map(|_| now(&p, &cpus(1))).collect();
        let mut ones = VecDeque::new();
        let mut twos = VecDeque::new();
        for wide in [false, true, false, false, true, false, false, true, false] {
            if wide {
                twos.push_back(queued(&p, &cpus(2)));
            } else {
                ones.push_back(queued(&p, &cpus(1)));
            }
        }
        let mut rounds = 0;
        while !ones.is_empty() || !twos.is_empty() {
            drop(
                running
                    .pop_front()
                    .expect("something runs while something waits"),
            );
            for class in [&mut ones, &mut twos] {
                // The granted ones are the oldest of the class.
                while let Some(run) = class.front_mut().and_then(granted) {
                    running.push_back(run);
                    class.pop_front();
                }
                assert!(
                    class.iter_mut().all(|w| granted(w).is_none()),
                    "a younger waiter started first"
                );
            }
            assert!(settled(&p));
            rounds += 1;
            assert!(rounds < 100, "the queue does not drain");
        }
    }

    #[test]
    fn a_need_is_one_shape_whatever_its_pool_order() {
        let p = pool(&[("cpu", 4 * CPU), ("docker", 1)]);
        // `cpu` is appended after `docker` in the first, declared before it
        // in the second.
        let implicit_cpu = req(&[("docker", 1)]);
        let declared_cpu = req(&[("cpu", CPU), ("docker", 1)]);
        let running = now(&p, &declared_cpu);
        let _stays = now(&p, &cpus(3));
        let _head = queued(&p, &cpus(4));
        let mut older = queued(&p, &implicit_cpu);
        let mut younger = queued(&p, &declared_cpu);
        assert_eq!(classes(&p), 2, "the head's and one for both spellings");

        drop(running);
        let run = granted(&mut older).expect("the slot of the other spelling");
        assert_eq!(run.admitted(), Admitted::Handoff);
        assert!(granted(&mut younger).is_none(), "never ahead of the older");
        drop(run);
        let run = granted(&mut younger).expect("and back");
        assert_eq!(run.admitted(), Admitted::Handoff);
    }

    #[test]
    fn shapes_differing_on_an_unbounded_pool_are_two_classes() {
        let p = ResourcePool::new([
            (name("cpu"), Limit::Bounded(CPU)),
            (name("tokens"), Limit::Unbounded),
        ]);
        let running = now(&p, &cpus(1));
        let mut three = queued(&p, &req(&[("tokens", 3)]));
        let mut five = queued(&p, &req(&[("tokens", 5)]));
        assert_eq!(classes(&p), 2);

        drop(running);
        let run = granted(&mut three).unwrap();
        assert_eq!(run.amount("tokens"), 3);
        drop(run);
        let run = granted(&mut five).unwrap();
        assert_eq!(run.amount("tokens"), 5, "told its own amount");
    }

    #[test]
    fn a_waiter_of_the_heads_shape_never_passes_it() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let running = now(&p, &cpus(2));
        let _stays = now(&p, &cpus(2));
        let mut head = queued(&p, &cpus(2));
        let mut same = queued(&p, &cpus(2));
        assert_eq!(classes(&p), 1);

        drop(running);
        assert!(granted(&mut same).is_none());
        let run = granted(&mut head).expect("its shape's slot goes to the oldest");
        assert_eq!(run.admitted(), Admitted::Head);
        drop(run);
        assert!(granted(&mut same).is_some());
    }

    #[test]
    fn cancelling_within_a_class_leaves_no_entry() {
        let p = pool(&[("cpu", 3 * CPU)]);
        let _stays = now(&p, &cpus(2));
        let one = now(&p, &cpus(1));
        let head = queued(&p, &cpus(3));
        let [oldest, mut second, middle, mut last] = [(); 4].map(|()| queued(&p, &cpus(1)));
        assert_eq!(classes(&p), 2);

        drop(oldest);
        drop(middle);
        drop(one);
        let run = granted(&mut second).expect("the oldest that remains");
        assert_eq!(run.admitted(), Admitted::Handoff);
        assert!(granted(&mut last).is_none());

        drop(last);
        assert_eq!(classes(&p), 1, "its last waiter took the class with it");
        drop(run);
        let fresh = now(&p, &cpus(1));
        assert_eq!(fresh.admitted(), Admitted::Pass, "judged alone, afresh");

        drop(head);
        assert_eq!(classes(&p), 0);
        assert_eq!(p.snapshot().main.queued, 0);
    }

    // ---- Arrivals ---------------------------------------------------------

    #[test]
    fn an_arrival_passes_like_a_queued_run() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let _stays = now(&p, &cpus(3));
        let _head = queued(&p, &cpus(2));
        let run = now(&p, &cpus(1));
        assert_eq!(run.admitted(), Admitted::Pass);
        assert_eq!(
            overtaken(&p, "cpu"),
            (CPU, 16 * CPU),
            "charged like any pass"
        );
        assert_eq!(classes(&p), 1, "it never queued");
    }

    #[test]
    fn an_arrival_queues_behind_its_own_shape() {
        let p = pool(&[("cpu", 8 * CPU), ("docker", 1)]);
        let _docker = now(&p, &req(&[("cpu", 0), ("docker", 1)]));
        let _stays = now(&p, &cpus(4));
        let head = queued(&p, &req(&[("cpu", 4 * CPU), ("docker", 1)]));
        assert_eq!(passes(&p, &cpus(1)), 32);
        let mut older = queued(&p, &cpus(1));
        let before = visited(&p);

        // Four cpu are free. Its shape waits, so it queues behind it.
        let mut arrival = queued(&p, &cpus(1));
        assert_eq!(classes(&p), 2);
        assert_eq!(arrival.wait_info().unwrap().ahead, 2);
        assert_eq!(visited(&p), before);

        drop(head);
        assert!(granted(&mut older).is_some());
        assert!(granted(&mut arrival).is_some());
    }

    // ---- When the walk runs, and how far ----------------------------------

    #[test]
    fn a_full_hand_off_does_not_walk() {
        let p = pool(&[("cpu", 8 * CPU), ("docker", 1)]);
        let _docker = now(&p, &req(&[("cpu", 0), ("docker", 1)]));
        let running = now(&p, &cpus(4));
        let _stays = now(&p, &cpus(4));
        let _head = queued(&p, &req(&[("cpu", CPU), ("docker", 1)]));
        let mut same = queued(&p, &cpus(4));
        let _same_too = queued(&p, &cpus(4));
        let _others = [queued(&p, &cpus(2)), queued(&p, &cpus(3))];
        let before = visited(&p);

        drop(running);
        assert!(granted(&mut same).is_some());
        assert_eq!(visited(&p), before, "no other class was tested");
        assert!(settled(&p), "and none had to be");
    }

    #[test]
    fn an_unbounded_release_does_not_walk() {
        let p = ResourcePool::new([
            (name("cpu"), Limit::Bounded(2 * CPU)),
            (name("tokens"), Limit::Unbounded),
        ]);
        let tokens = now(&p, &req(&[("cpu", 0), ("tokens", 5)]));
        let _stays = now(&p, &cpus(2));
        let mut waiters = [queued(&p, &cpus(2)), queued(&p, &cpus(1))];
        let before = visited(&p);

        drop(tokens);
        assert_eq!(visited(&p), before);
        assert!(waiters.iter_mut().all(|w| granted(w).is_none()));
    }

    #[test]
    fn a_refused_shape_is_tested_once_per_walk() {
        let p = pool(&[("cpu", 8 * CPU)]);
        let _stays = now(&p, &cpus(7));
        let one = now(&p, &cpus(1));
        let mut waiters: Vec<Acquire> = (0..1000).map(|i| queued(&p, &cpus(2 + i % 3))).collect();
        assert_eq!(classes(&p), 3);
        let before = visited(&p);

        drop(one);
        assert_eq!(
            visited(&p) - before,
            2,
            "the two shapes that are not the head's"
        );
        assert!(waiters.iter_mut().all(|w| granted(w).is_none()));
    }

    /// Each event tests at most 64 classes, and exactly as many as the pass
    /// has left, up to 64. Class `i` is shape `i`; class 0 is the head's.
    #[test]
    fn a_walk_refuses_at_most_64_shapes() {
        // Per event below. With 100 classes: the release tests 1..=64, the
        // arrival the 35 left (65..=99), which ends the pass; the head change
        // starts one (2..=65), the cancellation of class 99 ends it (66..=98
        // and the arrival's class 100), the passing arrival finds no pass to
        // continue, and its release starts another. With 1000, every event
        // has 64 left. The counts are exact on purpose: they pin the cap and
        // where a pass ends, so a change to batching that moves them changes
        // what an event may cost.
        for (shapes, expected) in [(100, [64, 35, 64, 34, 0, 64]), (1000, [64; 6])] {
            let p = pool(&[("cpu", 8 * CPU)]);
            let _stays = now(&p, &cpus(7));
            let mut one = Some(now(&p, &cpus(1)));
            // Every waiter its own shape, none of which fits one cpu.
            let shape = |i: u64| req(&[("cpu", 2 * CPU + i)]);
            let mut waiters: std::collections::VecDeque<Acquire> =
                (0..shapes).map(|i| queued(&p, &shape(i))).collect();
            assert_eq!(classes(&p), shapes as usize);
            let tested = |event: &mut dyn FnMut()| {
                let before = visited(&p);
                event();
                visited(&p) - before
            };

            let mut counts = Vec::new();
            counts.push(tested(&mut || drop(one.take())));
            let mut arrival = None;
            counts.push(tested(&mut || arrival = Some(queued(&p, &shape(shapes)))));
            counts.push(tested(&mut || drop(waiters.pop_front())));
            counts.push(tested(&mut || drop(waiters.pop_back())));
            let mut passer = None;
            counts.push(tested(&mut || {
                passer = Some(now(&p, &req(&[("cpu", CPU / 2)])));
            }));
            counts.push(tested(&mut || drop(passer.take())));
            assert_eq!(
                counts, expected,
                "{shapes} classes: a release, an arrival, a head change, a \
                 cancellation behind the head, an arrival that passes, its release"
            );
        }
    }

    #[test]
    fn an_unfinished_walk_resumes_on_the_next_event() {
        let p = pool(&[("cpu", 8 * CPU), ("tokens", 2)]);
        let tokens = now(&p, &req(&[("cpu", 0), ("tokens", 2)]));
        let running = now(&p, &cpus(4));
        let _stays = now(&p, &cpus(4));
        let _head = queued(&p, &cpus(8));
        // 64 classes that never fit, then the one that will.
        let _never: Vec<Acquire> = (1..=64)
            .map(|i| queued(&p, &req(&[("cpu", 2 * CPU + i)])))
            .collect();
        let mut fits_later = queued(&p, &req(&[("cpu", 0), ("tokens", 1)]));
        let mut same = queued(&p, &cpus(4));
        let before = visited(&p);

        drop(tokens);
        assert_eq!(visited(&p) - before, 64, "the walk stopped at its cap");
        assert!(
            granted(&mut fits_later).is_none(),
            "the 65th class was not reached"
        );

        drop(running);
        assert!(
            granted(&mut same).is_some(),
            "a hand-off, which walks nothing"
        );
        let run = granted(&mut fits_later).expect("but continues an unfinished walk");
        assert_eq!(run.admitted(), Admitted::Pass);
        assert!(!draining(&p), "long before the head is protected");
    }

    /// A lane of 202 waiting classes, and a pass left unfinished at class
    /// 193: the head's class 0 (two `cpus(8)` waiters), 200 classes that
    /// never fit, and class 151, which fits once `tokens` is released.
    struct Ring {
        p: Arc<ResourcePool>,
        tokens: Option<Reservation>,
        head: Option<Acquire>,
        fits: Acquire,
        /// What `fits` was granted.
        started: Option<Reservation>,
        /// Keeps every other waiter queued.
        rest: Vec<Acquire>,
        _stays: Reservation,
    }

    impl Ring {
        /// 201 classes besides the head's.
        const OTHERS: u64 = 201;

        fn new() -> Self {
            let p = pool(&[("cpu", 8 * CPU), ("tokens", 2)]);
            let tokens = now(&p, &req(&[("cpu", 0), ("tokens", 2)]));
            let stays = now(&p, &cpus(7));
            let one = now(&p, &cpus(1));
            let head = queued(&p, &cpus(8));
            let mut rest = vec![queued(&p, &cpus(8))];
            let never = |i: u64| req(&[("cpu", 2 * CPU + i)]);
            rest.extend((1..=150).map(|i| queued(&p, &never(i))));
            let fits = queued(&p, &req(&[("cpu", 0), ("tokens", 1)]));
            rest.extend((151..=200).map(|i| queued(&p, &never(i))));
            assert_eq!(classes(&p), 202);

            // A release starts a pass from class 1; two arrivals of the
            // head's shape carry it to class 193. Class 151 is tested on the
            // way and refused: `tokens` is still held.
            let before = visited(&p);
            drop(one);
            rest.push(queued(&p, &cpus(8)));
            rest.push(queued(&p, &cpus(8)));
            assert_eq!(visited(&p) - before, 3 * 64);
            assert!(!pass_done(&p));
            Self {
                p,
                tokens: Some(tokens),
                head: Some(head),
                fits,
                started: None,
                rest,
                _stays: stays,
            }
        }

        /// Run `event`; how many classes it tested.
        fn tested(&mut self, event: impl FnOnce(&mut Self)) -> u64 {
            let before = visited(&self.p);
            event(self);
            visited(&self.p) - before
        }

        /// An event that frees nothing and leaves the head as it is: an
        /// arrival behind the head's own shape.
        fn idle(&mut self) {
            let waiter = queued(&self.p, &cpus(8));
            self.rest.push(waiter);
        }

        /// After the first event of a pass, which tested `first`: idle events
        /// until the pass could have covered every class, each one's count,
        /// and the event (1 is the first) after which class 151 had started.
        fn finish(&mut self, first: u64) -> (Vec<u64>, Option<u64>) {
            let mut counts = vec![first];
            let mut started_after = None;
            for event in 2..=Self::OTHERS.div_ceil(64) {
                counts.push(self.tested(Self::idle));
                if self.started.is_none() {
                    // Held: its release would be an event of its own.
                    self.started = granted(&mut self.fits);
                    started_after = self.started.as_ref().map(|run| {
                        assert_eq!(run.admitted(), Admitted::Pass);
                        event
                    });
                }
            }
            (counts, started_after)
        }
    }

    /// A pass started mid-ring runs to the top, wraps to the bottom and on
    /// up to where it started, then ends; a class that fits is reached
    /// within ceil(classes / 64) events, wherever the cursor was.
    #[test]
    fn a_pass_over_more_than_64_classes_wraps_and_ends() {
        let mut ring = Ring::new();
        let first = ring.tested(|r| drop(r.tokens.take()));
        assert_eq!(first, 64, "193..=201, then 1..=55");
        assert!(
            granted(&mut ring.fits).is_none(),
            "class 151 is not reached"
        );
        assert!(!pass_done(&ring.p));

        let (counts, started_after) = ring.finish(first);
        assert_eq!(
            counts,
            [64, 64, 64, 9],
            "then 56..=119, 120..=183, 184..=192"
        );
        assert_eq!(started_after, Some(3), "in 120..=183");
        assert!(pass_done(&ring.p), "every class was tested once");
        assert_eq!(counts.iter().sum::<u64>(), Ring::OTHERS);
        assert!(settled(&ring.p));
    }

    /// The head leaving mid-pass starts the pass over from where it was, so
    /// the classes the old pass had not reached are tested by the new one,
    /// and so is every other class, once.
    #[test]
    fn a_head_change_mid_pass_leaves_no_class_untested() {
        let mut ring = Ring::new();
        // Released: the pass wraps and stops at class 56 of the bottom leg.
        assert_eq!(ring.tested(|r| drop(r.tokens.take())), 64);
        assert!(granted(&mut ring.fits).is_none());
        assert!(!pass_done(&ring.p));

        // The head leaves; the next of its shape is the head.
        let first = ring.tested(|r| drop(r.head.take()));
        let (counts, started_after) = ring.finish(first);
        assert_eq!(
            counts,
            [64, 64, 64, 9],
            "56..=119, 120..=183, 184..=201 and 1..=46, 47..=55"
        );
        assert_eq!(started_after, Some(2));
        assert!(pass_done(&ring.p));
        assert_eq!(
            counts.iter().sum::<u64>(),
            Ring::OTHERS,
            "each class but the head's, once"
        );
        assert!(settled(&ring.p));
    }

    /// A pass is bounded by class ids, not by classes that must still wait:
    /// the class it stops at and the class it resumes from can both lose
    /// their last waiter mid-pass, and every class left is still tested
    /// once.
    #[test]
    fn a_pass_outlives_the_classes_that_bound_it() {
        let mut ring = Ring::new();
        let mut counts = vec![ring.tested(|r| drop(r.tokens.take()))];
        assert!(!pass_done(&ring.p), "stopped at class 56, to end at 193");

        // Class 193, where the pass ends: `rest[i]` is class `i` up to 150,
        // and class `i + 1` after.
        counts.push(ring.tested(|r| drop(r.rest.remove(192))));
        // Class 120, where it resumes.
        counts.push(ring.tested(|r| drop(r.rest.remove(120))));
        let run = granted(&mut ring.fits).expect("class 151 is reached");
        assert_eq!(run.admitted(), Admitted::Pass);
        counts.push(ring.tested(Ring::idle));
        assert!(pass_done(&ring.p));
        counts.push(ring.tested(Ring::idle));

        assert_eq!(
            counts,
            [64, 64, 64, 8, 0],
            "193..=201 and 1..=55, 56..=119, 121..=184, 185..=192"
        );
        assert_eq!(
            counts.iter().sum::<u64>(),
            Ring::OTHERS - 1,
            "class 120 left before its turn"
        );
        assert!(settled(&ring.p));
    }

    #[test]
    fn the_zero_cost_fast_path_creates_no_class() {
        let p = ResourcePool::new([
            (name("cpu"), Limit::Bounded(CPU)),
            (name("tokens"), Limit::Unbounded),
        ]);
        let _stays = now(&p, &cpus(1));
        let _head = queued(&p, &cpus(1));

        // Nothing at all: granted and returned without the lane's lock,
        // which this thread holds.
        let lock = p.main.state.lock();
        let nothing = p
            .acquire(&req(&[("cpu", 0)]))
            .unwrap()
            .now_or_never()
            .expect("granted at once")
            .unwrap();
        assert_eq!(nothing.admitted(), Admitted::Arrival);
        drop(nothing);
        drop(lock);

        let unbounded = now(&p, &req(&[("cpu", 0), ("tokens", 5)]));
        assert_eq!(classes(&p), 1, "only the head's");
        drop(unbounded);
        assert_eq!(classes(&p), 1);
    }

    /// A protected head holds back what takes from its pools, never a run
    /// that reserves nothing of a bounded pool.
    #[test]
    fn a_zero_cost_arrival_starts_past_a_protected_head() {
        let p = ResourcePool::new([
            (name("cpu"), Limit::Bounded(16 * CPU)),
            (name("tokens"), Limit::Unbounded),
        ]);
        let _stays = now(&p, &cpus(15));
        let _head = queued(&p, &cpus(2));
        assert_eq!(passes(&p, &cpus(1)), 32);
        assert!(draining(&p));
        let debt = overtaken(&p, "cpu");

        for zero in [req(&[("cpu", 0)]), req(&[("cpu", 0), ("tokens", 5)])] {
            let run = now(&p, &zero);
            assert_eq!(run.admitted(), Admitted::Arrival);
            assert_eq!(overtaken(&p, "cpu"), debt, "and is not charged");
        }
        assert_eq!(classes(&p), 1, "only the head's");
    }

    #[test]
    fn a_dropped_receiver_is_not_charged() {
        // A hand-off: the slot's shape has a waiter that is gone, then one
        // that is there.
        let p = pool(&[("cpu", 4 * CPU)]);
        let _stays = now(&p, &cpus(3));
        let one = now(&p, &cpus(1));
        let _head = queued(&p, &cpus(4));
        let _gone = dead(queued(&p, &cpus(1)));
        let mut there = queued(&p, &cpus(1));
        drop(one);
        let run = granted(&mut there).expect("the next of the class");
        assert_eq!(run.admitted(), Admitted::Handoff);
        assert_eq!(overtaken(&p, "cpu").0, CPU, "charged once");
        assert_eq!(free(&p, "cpu"), 0, "and held once");
        assert_eq!(classes(&p), 1);

        // A pass, from the walk.
        let p = pool(&[("cpu", 4 * CPU)]);
        let _stays = now(&p, &cpus(2));
        let two = now(&p, &cpus(2));
        let _head = queued(&p, &cpus(4));
        let _gone = dead(queued(&p, &cpus(1)));
        let mut there = queued(&p, &cpus(1));
        drop(two);
        let run = granted(&mut there).expect("the next of the class");
        assert_eq!(run.admitted(), Admitted::Pass);
        assert_eq!(overtaken(&p, "cpu").0, CPU);
        assert_eq!(free(&p, "cpu"), CPU);

        // The head itself: the next oldest becomes the head, and is granted.
        let p = pool(&[("cpu", CPU)]);
        let one = now(&p, &cpus(1));
        let _gone = dead(queued(&p, &cpus(1)));
        let mut there = queued(&p, &cpus(1));
        drop(one);
        assert_eq!(granted(&mut there).unwrap().admitted(), Admitted::Head);
    }

    /// Every waiter of the released shape is gone: the class leaves the lane
    /// during the hand-off, and the release falls through to the walk, which
    /// grants who fits.
    #[test]
    fn a_hand_off_to_a_class_of_dead_waiters_walks() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let _stays = now(&p, &cpus(3));
        let one = now(&p, &cpus(1));
        let _head = queued(&p, &cpus(4));
        let _gone = [dead(queued(&p, &cpus(1))), dead(queued(&p, &cpus(1)))];
        let mut half = queued(&p, &req(&[("cpu", CPU / 2)]));
        assert_eq!(classes(&p), 3);
        let before = visited(&p);

        drop(one);
        let run = granted(&mut half).expect("the walk grants who fits");
        assert_eq!(run.admitted(), Admitted::Pass);
        assert_eq!(visited(&p) - before, 1, "the only class but the head's");
        assert_eq!(classes(&p), 1, "the head's");
        assert_eq!(overtaken(&p, "cpu").0, CPU / 2, "only the pass is charged");
        assert_eq!(free(&p, "cpu"), CPU / 2);
    }

    // ---- What a wait and a start say --------------------------------------

    #[test]
    fn a_reservation_says_how_it_was_admitted() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let first = now(&p, &cpus(4));
        assert_eq!(first.admitted(), Admitted::Arrival);
        assert_eq!(now(&p, &req(&[("cpu", 0)])).admitted(), Admitted::Arrival);

        let mut head = queued(&p, &cpus(3));
        let mut same = queued(&p, &cpus(4));
        drop(first);
        let handed = granted(&mut same).unwrap();
        assert_eq!(handed.admitted(), Admitted::Handoff);
        drop(handed);
        let mut oldest = granted(&mut head).unwrap();
        assert_eq!(oldest.admitted(), Admitted::Head);
        assert_eq!(
            oldest.split_off("cpu").unwrap().admitted(),
            Admitted::Head,
            "a half says what the whole did"
        );
        drop(oldest);

        let _stays = now(&p, &cpus(3));
        let _head = queued(&p, &cpus(2));
        assert_eq!(now(&p, &cpus(1)).admitted(), Admitted::Pass);
    }

    #[test]
    fn the_head_snapshot_reports_debt_and_limit() {
        let p = ResourcePool::new([
            (name("cpu"), Limit::Bounded(4 * CPU)),
            (name("tokens"), Limit::Unbounded),
        ]);
        let _stays = now(&p, &cpus(3));
        let _head = queued(&p, &req(&[("cpu", 2 * CPU), ("tokens", 1)]));
        for passed in 1..=16 {
            let head = p.snapshot().main.head.unwrap();
            assert!(!head.draining());
            drop(now(&p, &cpus(1)));
            assert_eq!(
                p.snapshot().main.head.unwrap().overtaken,
                [Overtaken {
                    pool: name("cpu"),
                    by: passed * CPU,
                    limit: 16 * CPU,
                }],
                "nothing for the unbounded pool"
            );
        }
        assert!(draining(&p));
    }

    #[test]
    fn a_waiter_held_back_for_the_head_says_what_it_yields_to() {
        let p = pool(&[("cpu", 8 * CPU), ("docker", 1)]);
        let _docker = now(&p, &req(&[("cpu", 0), ("docker", 1)]));
        let _stays = now(&p, &cpus(4));
        let head = queued(&p, &req(&[("cpu", 4 * CPU), ("docker", 1)]));
        let too_wide = queued(&p, &cpus(6));
        assert_eq!(passes(&p, &cpus(1)), 32);
        let fits = queued(&p, &cpus(1));

        assert_eq!(
            fits.wait_info().unwrap().yielding_to,
            Some(YieldingTo {
                pool: name("cpu"),
                need: 4 * CPU,
            })
        );
        assert_eq!(head.wait_info().unwrap().yielding_to, None, "the head");
        assert_eq!(
            too_wide.wait_info().unwrap().yielding_to,
            None,
            "it waits for cpu, not for the head"
        );
    }

    #[test]
    fn ahead_counts_earlier_waiters_after_passes() {
        let p = pool(&[("cpu", 4 * CPU)]);
        let _stays = now(&p, &cpus(3));
        let head = queued(&p, &cpus(2));
        let second = queued(&p, &cpus(3));
        let passer = now(&p, &cpus(1));
        let third = queued(&p, &cpus(4));
        let mut fourth = queued(&p, &cpus(1));
        let ahead = |a: &Acquire| a.wait_info().unwrap().ahead;
        assert_eq!([&head, &second, &third, &fourth].map(ahead), [0, 1, 2, 3]);

        drop(passer);
        let _run = granted(&mut fourth).expect("the youngest starts first");
        assert_eq!([&head, &second, &third].map(ahead), [0, 1, 2]);
        assert_eq!(ahead(&queued(&p, &cpus(1))), 3, "the three older waiters");
    }

    // ---- Scripts ----------------------------------------------------------

    /// Where one request of a [`Script`] stands.
    enum Slot {
        Waiting(Acquire),
        Held(Reservation),
        Over,
    }

    /// What the engine adds to a request: `sandbox: 1` for a managed run.
    fn implicit(managed: bool) -> &'static [(PoolName, u64)] {
        static SLOT: std::sync::LazyLock<[(PoolName, u64); 1]> =
            std::sync::LazyLock::new(|| [(PoolName::sandbox(), 1)]);
        if managed { &*SLOT } else { &[] }
    }

    /// A lane driven by a seeded sequence of arrivals, releases and
    /// cancellations, with what the test knows of every request.
    struct Script {
        pool: Arc<ResourcePool>,
        shapes: Vec<Demand>,
        /// The shapes from this index on are managed runs': acquired with the
        /// engine's `sandbox: 1`, whose slot is split off the grant and
        /// released by an event of its own.
        managed_from: usize,
        /// Each of `shapes` as the lane resolves it: two that resolve alike
        /// are one shape.
        resolved: Vec<Vec<(usize, u64)>>,
        /// Whether one walk tests every class there can be: with no more
        /// shapes than [`WALK_CAP`], no pass is ever left unfinished.
        within_cap: bool,
        /// Every request in arrival order: its shape and where it stands.
        slots: Vec<(usize, Slot)>,
        /// The `sandbox` halves split off managed runs' grants.
        removals: Vec<Reservation>,
        /// How many of them were released.
        removed: usize,
        /// How many managed runs started in a slot handed off to their shape.
        managed_handoffs: usize,
        /// Every grant: the request and how it was admitted.
        log: Vec<(usize, Admitted)>,
        /// The head after the last event, when it was draining: its request
        /// and its debts.
        drained: Option<(usize, Vec<Overtaken>)>,
        /// The most classes that waited at once.
        peak_classes: usize,
        /// After how many events a pass was left unfinished.
        unfinished: usize,
    }

    impl Script {
        fn new(pool: Arc<ResourcePool>, shapes: Vec<Demand>) -> Self {
            Self::with_managed(pool, shapes, Vec::new())
        }

        /// `shapes`, then `managed` as managed runs' shapes.
        fn with_managed(
            pool: Arc<ResourcePool>,
            mut shapes: Vec<Demand>,
            managed: Vec<Demand>,
        ) -> Self {
            let managed_from = shapes.len();
            shapes.extend(managed);
            let resolved: Vec<_> = shapes
                .iter()
                .enumerate()
                .map(|(s, shape)| {
                    let (mut need, _) = pool.resolve(shape, implicit(s >= managed_from)).unwrap();
                    need.sort_unstable();
                    need
                })
                .collect();
            let mut distinct = resolved.clone();
            distinct.sort_unstable();
            distinct.dedup();
            Self {
                pool,
                shapes,
                managed_from,
                resolved,
                within_cap: distinct.len() <= WALK_CAP,
                slots: Vec::new(),
                removals: Vec::new(),
                removed: 0,
                managed_handoffs: 0,
                log: Vec::new(),
                drained: None,
                peak_classes: 0,
                unfinished: 0,
            }
        }

        fn count(&self, held: bool) -> usize {
            self.slots
                .iter()
                .filter(|(_, s)| match s {
                    Slot::Waiting(_) => !held,
                    Slot::Held(_) => held,
                    Slot::Over => false,
                })
                .count()
        }

        fn arrive(&mut self, shape: usize) {
            let acquire = self
                .pool
                .acquire_with(&self.shapes[shape], implicit(shape >= self.managed_from))
                .unwrap();
            self.slots.push((shape, Slot::Waiting(acquire)));
            self.observe();
        }

        /// Release the `nth` split-off `sandbox` slot: the reclaim lane
        /// removed its sandbox.
        fn end_removal(&mut self, nth: usize) {
            drop(self.removals.remove(nth));
            self.removed += 1;
            self.observe();
        }

        /// Events that change nothing a waiter waits for, until no pass is
        /// unfinished: the head's own shape arrives behind it and leaves. The
        /// head stays, so each event continues the pass, which then ends
        /// within one event per [`WALK_CAP`] classes. Then a full walk would
        /// grant nothing.
        fn settle(&mut self) {
            let bound = self.shapes.len().div_ceil(WALK_CAP);
            for _ in 0..bound {
                if pass_done(&self.pool) {
                    break;
                }
                let Some(&(shape, _)) = self
                    .slots
                    .iter()
                    .find(|(_, s)| matches!(s, Slot::Waiting(_)))
                else {
                    break;
                };
                self.arrive(shape);
                self.check();
                let last = self.count(false) - 1;
                self.end(last, false);
                self.check();
            }
            assert!(
                settled(&self.pool),
                "a pass did not end within {bound} pairs of idle events"
            );
        }

        /// Release the `nth` live grant, or cancel the `nth` waiter.
        fn end(&mut self, nth: usize, held: bool) {
            let slot = self
                .slots
                .iter_mut()
                .filter(|(_, s)| match s {
                    Slot::Waiting(_) => !held,
                    Slot::Held(_) => held,
                    Slot::Over => false,
                })
                .nth(nth)
                .expect("a live request");
            slot.1 = Slot::Over;
            self.observe();
        }

        /// Note every waiter that was granted, oldest first, and check that
        /// none started ahead of an older waiter of its shape (I4).
        fn observe(&mut self) {
            for i in 0..self.slots.len() {
                let (older, rest) = self.slots.split_at_mut(i);
                let (shape, slot) = &mut rest[0];
                let Slot::Waiting(acquire) = slot else {
                    continue;
                };
                let Some(mut run) = granted(acquire) else {
                    continue;
                };
                let alike = &self.resolved[*shape];
                assert!(
                    !older.iter().any(|(s, slot)| {
                        matches!(slot, Slot::Waiting(_)) && &self.resolved[*s] == alike
                    }),
                    "request {i} started ahead of an older one of its shape"
                );
                self.log.push((i, run.admitted()));
                if *shape >= self.managed_from {
                    self.managed_handoffs += usize::from(run.admitted() == Admitted::Handoff);
                    let removal = run
                        .split_off(PoolName::SANDBOX)
                        .expect("a managed grant holds a slot");
                    self.removals.push(removal);
                }
                *slot = Slot::Held(run);
            }
        }

        /// The invariants that hold after every event.
        fn check(&mut self) {
            let snap = self.pool.snapshot().main;
            assert_eq!(snap.queued, self.count(false));
            for pool in &snap.pools {
                let live: u64 = self
                    .slots
                    .iter()
                    .filter_map(|(_, s)| match s {
                        Slot::Held(run) => Some(run),
                        _ => None,
                    })
                    .chain(&self.removals)
                    .map(|run| run.amount(pool.pool.as_str()))
                    .sum();
                assert_eq!(pool.held, live, "{}: held is the live grants", pool.pool);
                if let Limit::Bounded(capacity) = pool.capacity {
                    assert!(pool.held <= capacity, "{} is over capacity", pool.pool);
                }
            }
            let head = self
                .slots
                .iter()
                .position(|(_, s)| matches!(s, Slot::Waiting(_)));
            let was = self.drained.take();
            if let Some((head, snap)) = head.zip(snap.head) {
                for o in &snap.overtaken {
                    // I1: its limit plus one grant, the widest there is.
                    let idx = self.pool.main.index(o.pool.as_str()).unwrap();
                    let widest = self
                        .resolved
                        .iter()
                        .map(|shape| amount_in(shape, idx))
                        .max()
                        .unwrap_or(0);
                    assert!(o.by < o.limit + widest, "{o:?} is past its limit");
                }
                if let Some((_, debts)) = was.filter(|&(request, _)| request == head) {
                    assert_eq!(debts, snap.overtaken, "I2: a protected head is passed");
                }
                if snap.draining() {
                    self.drained = Some((head, snap.overtaken));
                }
            }
            self.peak_classes = self
                .peak_classes
                .max(self.pool.main.state.lock().classes.len());
            if !pass_done(&self.pool) {
                self.unfinished += 1;
            }
            // Past the cap a pass can be unfinished between events, and
            // `settle` checks this once it ends.
            if self.within_cap {
                assert!(settled(&self.pool), "a full walk would grant something");
            }
        }

        /// `events` seeded events, the invariants checked after each, then
        /// releases until the queue is empty. Past the walk cap, every tenth
        /// event or so is followed by idle events until its pass ended.
        fn run(&mut self, seed: u64, events: usize) {
            let mut rng = Rng(seed);
            for _ in 0..events {
                let (held, waiting) = (self.count(true), self.count(false));
                match rng.below(20) {
                    0..=6 if held > 0 => self.end(rng.below(held), true),
                    7..=9 if waiting > 0 => self.end(rng.below(waiting), false),
                    10..=11 if !self.removals.is_empty() => {
                        self.end_removal(rng.below(self.removals.len()));
                    }
                    _ => self.arrive(rng.below(self.shapes.len())),
                }
                self.check();
                if !self.within_cap && rng.below(10) == 0 {
                    self.settle();
                }
            }
            while self.count(false) > 0 {
                if self.count(true) > 0 {
                    self.end(0, true);
                } else {
                    assert!(!self.removals.is_empty(), "waiters, and nothing to release");
                    self.end_removal(0);
                }
                self.check();
            }
            while !self.removals.is_empty() {
                self.end_removal(0);
                self.check();
            }
            self.slots.clear();
            for pool in self.pool.snapshot().main.pools {
                assert_eq!(pool.held, 0, "{} came back", pool.pool);
            }
            assert_eq!(classes(&self.pool), 0);
        }
    }

    #[test]
    fn random_events_keep_the_invariants() {
        let mut managed_handoffs = 0;
        for seed in 0..40 {
            let pool = ResourcePool::new([
                (name("cpu"), Limit::Bounded(8 * CPU)),
                (name("docker"), Limit::Bounded(2)),
                (name("memory"), Limit::Bounded(16 * GIB)),
                (name("sandbox"), Limit::Bounded(4)),
                (name("tokens"), Limit::Unbounded),
            ]);
            let shapes = vec![
                req(&[("cpu", CPU / 10)]),
                req(&[("cpu", CPU / 2)]),
                cpus(1),
                cpus(2),
                cpus(4),
                cpus(8),
                cpus(16),
                req(&[("memory", 4 * GIB)]),
                req(&[("memory", 16 * GIB)]),
                req(&[("cpu", 0), ("docker", 1)]),
                req(&[("cpu", 2 * CPU), ("docker", 1)]),
                req(&[("docker", 1), ("cpu", 2 * CPU)]),
                req(&[("tokens", 3)]),
                req(&[("tokens", 5)]),
                req(&[("cpu", 4 * CPU), ("memory", 8 * GIB), ("docker", 2)]),
            ];
            let managed = vec![
                cpus(1),
                req(&[("cpu", 0)]),
                req(&[("cpu", 2 * CPU), ("docker", 1)]),
            ];
            let mut script = Script::with_managed(pool, shapes, managed);
            script.run(seed, 600);
            assert!(script.removed > 0, "seed {seed} released no split slot");
            managed_handoffs += script.managed_handoffs;
            let how = |a| script.log.iter().filter(|&&(_, x)| x == a).count();
            assert!(
                [
                    Admitted::Arrival,
                    Admitted::Head,
                    Admitted::Handoff,
                    Admitted::Pass
                ]
                .into_iter()
                .all(|a| how(a) > 0),
                "seed {seed} never took one of the four ways in"
            );
        }
        // Over the seeds, not per seed: a managed waiter is seldom the next
        // of its shape when a half comes back, and some seeds have none.
        assert!(managed_handoffs > 0, "no seed handed a split grant off");
    }

    /// More shapes than one walk tests: a pass is left unfinished between
    /// events, and the invariants still hold after every one. Once the pass
    /// ends, a full walk would grant nothing.
    #[test]
    fn random_events_past_the_walk_cap_keep_the_invariants() {
        for seed in 0..6 {
            let pool = ResourcePool::new([
                (name("cpu"), Limit::Bounded(8 * CPU)),
                (name("docker"), Limit::Bounded(2)),
                (name("memory"), Limit::Bounded(16 * GIB)),
                (name("sandbox"), Limit::Bounded(8)),
            ]);
            let mut shapes: Vec<Demand> = (1..=96).map(|i| req(&[("cpu", i * CPU / 12)])).collect();
            shapes.extend([
                req(&[("memory", 4 * GIB)]),
                req(&[("memory", 16 * GIB)]),
                req(&[("cpu", 0), ("docker", 1)]),
                req(&[("cpu", 2 * CPU), ("docker", 1)]),
            ]);
            let managed = vec![cpus(1), req(&[("cpu", CPU / 12)]), req(&[("cpu", 0)])];
            let mut script = Script::with_managed(pool, shapes, managed);
            assert!(!script.within_cap);
            // The whole cpu pool first, so nearly every shape waits.
            script.arrive(95);
            for shape in 0..script.shapes.len() {
                script.arrive(shape);
                script.check();
            }
            assert!(
                script.peak_classes > WALK_CAP + 1,
                "{} classes",
                script.peak_classes
            );
            script.run(seed, 1500);
            assert!(
                script.unfinished > 0,
                "seed {seed} never left a pass unfinished"
            );
            assert!(script.removed > 0, "seed {seed} released no split slot");
        }
    }

    /// With no more classes than one walk tests ([`WALK_CAP`]), grants
    /// depend on the requests alone: not on class ids, nor on what the shape
    /// map held before. The same script, on a fresh lane and on one where
    /// other shapes (and its own, in reverse) queued and left first, grants
    /// the same requests the same way. Past the cap a walk takes classes by
    /// id from its cursor, so which class it reaches first follows the
    /// lane's history: the same arrivals and completions still give the same
    /// starts, from the same lane.
    #[test]
    fn grants_are_deterministic() {
        let run = |history: bool| {
            let pool = pool(&[("cpu", 8 * CPU), ("docker", 2)]);
            let mut shapes: Vec<Demand> = (1..=20).map(|i| req(&[("cpu", i * CPU / 4)])).collect();
            shapes.extend((1..=4).map(|i| req(&[("cpu", i * CPU), ("docker", 1)])));
            if history {
                let full = [
                    now(&pool, &cpus(8)),
                    now(&pool, &req(&[("cpu", 0), ("docker", 2)])),
                ];
                // A few micro-CPUs each: no shape of the script's.
                let others = (1..=200).map(|i| req(&[("cpu", i)]));
                let mut waiters: Vec<Acquire> = shapes
                    .iter()
                    .rev()
                    .cloned()
                    .chain(others)
                    .map(|shape| queued(&pool, &shape))
                    .collect();
                assert_eq!(classes(&pool), 224);
                // Leave from both ends, so classes go in no single order.
                while !waiters.is_empty() {
                    drop(waiters.swap_remove(waiters.len() / 3));
                }
                drop(full);
                assert_eq!(classes(&pool), 0);
                assert_eq!((free(&pool, "cpu"), free(&pool, "docker")), (8 * CPU, 2));
            }
            let mut script = Script::new(pool, shapes);
            script.run(11, 2000);
            script.log
        };
        let (first, second, after_history) = (run(false), run(false), run(true));
        assert!(first.len() > 500, "{} grants", first.len());
        assert_eq!(first, second);
        assert_eq!(first, after_history, "by request index");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mixed_shapes_with_aborts_return_everything() {
        let p = pool(&[("cpu", 4 * CPU), ("docker", 1), ("memory", 8 * GIB)]);
        let shapes = [
            cpus(1),
            cpus(2),
            cpus(4),
            req(&[("cpu", CPU / 10)]),
            req(&[("docker", 1)]),
            req(&[("memory", 6 * GIB)]),
            req(&[("cpu", 0), ("memory", 4 * GIB)]),
        ];
        let mut rng = Rng(7);
        let mut tasks = tokio::task::JoinSet::new();
        let mut handles = Vec::new();
        for shape in shapes.iter().cycle().take(600) {
            let p = Arc::clone(&p);
            let shape = shape.clone();
            let yields = rng.below(4);
            handles.push(tasks.spawn(async move {
                let run = p.acquire(&shape).unwrap().await.unwrap();
                for _ in 0..yields {
                    tokio::task::yield_now().await;
                }
                drop(run);
            }));
        }
        for handle in &handles {
            if rng.below(3) == 0 {
                handle.abort();
                tokio::task::yield_now().await;
            }
        }
        let done = tokio::time::timeout(Duration::from_secs(60), async {
            while let Some(joined) = tasks.join_next().await {
                if let Err(e) = joined {
                    assert!(e.is_cancelled(), "{e}");
                }
            }
        })
        .await;
        assert!(done.is_ok(), "waiters starved");
        assert_eq!(
            (free(&p, "cpu"), free(&p, "docker"), free(&p, "memory")),
            (4 * CPU, 1, 8 * GIB)
        );
        assert_eq!(p.snapshot().main.queued, 0);
        assert_eq!(classes(&p), 0);
    }
}

#[cfg(test)]
mod admission_replay;
pub(crate) mod sampler;
