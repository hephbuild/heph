//! `admission_replay`: what the admission rule buys over strict FIFO, and what
//! an admission event costs under the lane mutex, measured on the real pool.
//!
//! `#[ignore]`d: this is an instrument, not a test. It asserts only enough to
//! know it measured the right thing (nothing is granted past a capacity, every
//! queue drains, each timed event was the event it is named after) and its
//! output is tables a human reads. Run it deliberately:
//!
//! ```text
//! cargo test --release -p engine --lib admission_replay -- --ignored --nocapture
//! ```
//!
//! Always `--release`: the lock-hold half times single calls in nanoseconds.
//! The two halves take a lock so they never overlap, whatever `--test-threads`
//! says. `HEPH_REPLAY_SEEDS=100` replays each workload with 100 seeds, not
//! 10: it takes seconds, and it is what tells a 2% change from noise.
//!
//! # Why it exists
//!
//! The rule's gains (hand-off, first fit, a head overtaken by at most
//! `PATIENCE` times its need and `PATIENCE_POOLS` times the pool) were
//! predicted by a Python discrete-event model, which is not the pool. This
//! replays that model's workloads through [`ResourcePool`] itself, so the
//! numbers are the rule as it is written.
//!
//! # The replay
//!
//! Under tokio's paused clock, on one thread. A run is a task: its arrival is
//! the `acquire` call, it sleeps its duration once granted, drops the grant,
//! and **only then** makes its dependents arrive, as the engine does (the
//! release is handed off and walked before a dependent exists). Each arrival
//! is judged alone, in the order the workload lists it.
//!
//! The baseline is strict FIFO, ported here from the pool as it was before
//! the rule ([`Fifo`]); the production pool has no switch. Both sides replay
//! the same runs: a workload is a function of its seed.
//!
//! Workloads: the model's six, its three whole-pool situations, and two
//! families the model could not express because it never had more than six
//! shapes:
//!
//! - **mixed widths, N shapes**: the first workload's runs, unchanged, where
//!   each run also takes a distinct amount of a pool that never fills. What
//!   competes for cpu is identical, so the difference from the first row is
//!   what distinct shapes cost: a hand-off needs a waiter of the same shape,
//!   and a walk tests 64 classes per event.
//! - **needle, N shapes**: half the runs are one narrow shape, the rest are
//!   wide runs spread over the other shapes, so the class that fits a gap is 1
//!   in N. The 3-shape row is its control.
//!
//! # Reading the tables
//!
//! - Wall time and utilization are a mean over the seeds with the range in
//!   brackets. `per seed` is the best and worst paired change; `slower` counts
//!   the seeds where the pool lost to FIFO. A mean change inside the per-seed
//!   range of a row that should be 0 is noise.
//! - Utilization is cpu-seconds held by grants a run task actually had, from
//!   the instant it observed the grant to the instant it dropped it, over
//!   capacity times wall. A head being waited for holds nothing and counts
//!   for nothing.
//! - The grant shares are [`Reservation::admitted`]. A slot handed to the
//!   oldest waiter reads `head`, not `hand-off`, so `hand-off` undercounts
//!   releases that needed no walk.
//!
//! # Lock hold
//!
//! In real time, one thread, nothing else touching the pool: the duration of a
//! call bounds the time it holds the lane mutex. Each event kind is set up so
//! it repeats on a queue of 10,000 waiters, and is timed alone.
//!
//! Traps:
//!
//! - The clock ticks every ~42 ns on Apple silicon, so a p50 is a multiple of
//!   that; the mean is not quantized. `the clock alone` is the floor.
//! - `max` is whatever preempted the thread. Read p99.
//! - Waiters are polled with a no-op waker. In the engine a grant also wakes a
//!   task under the lock, which this leaves out: it is one wake per grant
//!   under either rule.
//!
//! # What it does not tell you
//!
//! Admission only. No cache, no IO, no process spawn, no real durations (they
//! are exponential draws, as in the model), and no second thread contending
//! for the mutex. A gain here is a bound on what a build can gain when
//! admission is what limits it.

use super::{Acquire, Admitted, Limit, Reservation, ResourcePool};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use hplugin::driver::{Demand, Resources};
use hplugin::provider::PoolName;
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

const CPU: u64 = Resources::MICROS_PER_CPU;
const GIB: u64 = 1 << 30;

/// How many seeds each workload is replayed with: `HEPH_REPLAY_SEEDS`, 10 by
/// default, as in the model. The whole-pool waits are exponential draws, so
/// 10 seeds leave their mean off by tens of seconds.
fn seeds_wanted() -> u64 {
    std::env::var("HEPH_REPLAY_SEEDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(10)
}

/// The capacity of a pool that never fills: its amounts only tell shapes
/// apart.
const ROOMY: u64 = 1 << 40;
/// Virtual time after which a replay that has not finished is stuck.
const STUCK: Duration = Duration::from_secs(100_000_000);

/// The two halves never overlap: the lock-hold half times single calls.
static SERIAL: Mutex<()> = Mutex::new(());

fn name(s: &str) -> PoolName {
    PoolName::try_from(s).unwrap()
}

fn demand(pairs: &[(&str, u64)]) -> Demand {
    Demand::try_from_pairs(pairs.iter().map(|&(n, a)| (n.to_string(), a))).unwrap()
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

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// An exponential draw of this mean, in seconds.
    fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.unit()).ln()
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

/// What a run takes: cpu in micro-CPUs, and an amount of the workload's
/// second pool (0 when it has none or the run takes none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Need {
    cpu: u64,
    other: u64,
}

/// `tenths` of a cpu, and `other` of the second pool.
fn need(tenths: u64, other: u64) -> Need {
    Need {
        cpu: tenths * (CPU / 10),
        other,
    }
}

#[derive(Debug)]
struct Run {
    need: Need,
    millis: u64,
    /// The runs that wait for this one.
    then: Vec<usize>,
    deps: usize,
    /// The run whose wait the whole-pool table reports.
    marked: bool,
}

/// Runs in arrival order: those with no dependency arrive at the start, in
/// this order; a dependent arrives when its last dependency has released.
#[derive(Debug)]
struct Workload {
    /// The second pool and its capacity.
    other: Option<(&'static str, u64)>,
    runs: Vec<Run>,
}

impl Workload {
    fn new(other: Option<(&'static str, u64)>) -> Self {
        Self {
            other,
            runs: Vec::new(),
        }
    }

    fn add(&mut self, need: Need, secs: f64) -> usize {
        // Whole milliseconds, the paused clock's resolution, and never 0.
        let millis = u64::try_from(Duration::from_secs_f64(secs).as_millis())
            .unwrap()
            .max(1);
        self.runs.push(Run {
            need,
            millis,
            then: Vec::new(),
            deps: 0,
            marked: false,
        });
        self.runs.len() - 1
    }

    fn link(&mut self, first: usize, then: usize) {
        self.runs[first].then.push(then);
        self.runs[then].deps += 1;
    }

    /// Steps that each wait for the one before. Returns the first.
    fn chain(&mut self, steps: impl IntoIterator<Item = (Need, f64)>) -> usize {
        let mut first = None;
        let mut last: Option<usize> = None;
        for (need, secs) in steps {
            let id = self.add(need, secs);
            if let Some(prev) = last {
                self.link(prev, id);
            }
            first.get_or_insert(id);
            last = Some(id);
        }
        first.expect("a chain has a step")
    }

    fn shapes(&self) -> usize {
        self.runs
            .iter()
            .map(|r| r.need)
            .collect::<BTreeSet<_>>()
            .len()
    }
}

/// 85% `cpu: 1`, 10% `cpu: 4`, 5% `cpu: 8`, all ready at once.
fn wide(rng: &mut Rng) -> Workload {
    let mut w = Workload::new(None);
    for _ in 0..2000 {
        let r = rng.unit();
        if r < 0.85 {
            w.add(need(10, 0), rng.exp(2.0));
        } else if r < 0.95 {
            w.add(need(40, 0), rng.exp(4.0));
        } else {
            w.add(need(80, 0), rng.exp(3.0));
        }
    }
    w
}

/// A layered build: each run depends on up to 2 of the 60 before it.
fn dag(rng: &mut Rng) -> Workload {
    let mut w = Workload::new(None);
    for i in 0..2000 {
        let r = rng.unit();
        let id = if r < 0.75 {
            w.add(need(10, 0), rng.exp(2.0))
        } else if r < 0.87 {
            w.add(need(1, 0), rng.exp(1.0))
        } else if r < 0.96 {
            w.add(need(40, 0), rng.exp(4.0))
        } else {
            w.add(need(80, 0), rng.exp(3.0))
        };
        let wanted = [0, 1, 1, 2][rng.below(4)].min(i);
        let window = i.min(60);
        let mut deps: Vec<usize> = Vec::new();
        while deps.len() < wanted {
            let dep = i - 1 - rng.below(window);
            if !deps.contains(&dep) {
                deps.push(dep);
            }
        }
        for dep in deps {
            w.link(dep, id);
        }
    }
    w
}

/// A `cpu: 1` crowd, and `runs` runs of each of `shapes` (cpu tenths, each
/// with the one token) of mean `mean` seconds, shuffled.
fn tokens(rng: &mut Rng, shapes: &[u64], runs: usize, mean: f64) -> Workload {
    let mut steps: Vec<(Need, f64)> = (0..1900).map(|_| (need(10, 0), rng.exp(2.0))).collect();
    for &tenths in shapes {
        for _ in 0..runs {
            steps.push((need(tenths, 1), rng.exp(mean)));
        }
    }
    rng.shuffle(&mut steps);
    let mut w = Workload::new(Some(("token", 1)));
    for (need, secs) in steps {
        w.add(need, secs);
    }
    w
}

/// 16 streams of `cpu: 1` keep 8 cpus busy; after the eighth, `gated` arrives:
/// a chain whose steps each wait for the one before.
fn full_machine(rng: &mut Rng, gated: impl IntoIterator<Item = (Need, f64)>) -> Workload {
    let mut w = Workload::new(None);
    let mut gated = Some(gated);
    for i in 0..16 {
        let stream: Vec<_> = (0..125).map(|_| (need(10, 0), rng.exp(2.0))).collect();
        w.chain(stream);
        if i == 7
            && let Some(gated) = gated.take()
        {
            w.chain(gated);
        }
    }
    w
}

/// One `cpu: 8` run gates a 100-step chain.
fn gate(rng: &mut Rng) -> Workload {
    let chain = std::iter::once((need(80, 0), 5.0)).chain((0..100).map(|_| (need(10, 0), 3.0)));
    full_machine(rng, chain)
}

/// The critical path alternates a 0.1-cpu fetch and a `cpu: 1` step.
fn relay(rng: &mut Rng) -> Workload {
    let chain = (0..100).map(|k| {
        if k % 2 == 0 {
            (need(1, 0), 1.0)
        } else {
            (need(10, 0), 3.0)
        }
    });
    full_machine(rng, chain)
}

/// One stream of 40 runs (30 s mean) per entry of `kinds`, on 8 GiB of
/// memory; after the middle stream, a `cpu: 1` run that wants all the memory
/// arrives and gates a 20-step chain.
fn whole_pool(rng: &mut Rng, kinds: &[Need]) -> Workload {
    let mut w = Workload::new(Some(("memory", 8 * GIB)));
    for (i, &kind) in kinds.iter().enumerate() {
        let stream: Vec<_> = (0..40).map(|_| (kind, rng.exp(30.0))).collect();
        w.chain(stream);
        if i == kinds.len() / 2 {
            let chain = (0..20).map(|_| (need(10, 0), 30.0));
            let big = w.chain(std::iter::once((need(10, 8 * GIB), 30.0)).chain(chain));
            w.runs[big].marked = true;
        }
    }
    w
}

fn kinds(groups: &[(usize, Need)]) -> Vec<Need> {
    groups
        .iter()
        .flat_map(|&(n, need)| std::iter::repeat_n(need, n))
        .collect()
}

/// 1,000 `cpu: 1`, 500 `cpu: 3` and 500 `cpu: 6` runs, shuffled and all ready
/// at once: two wide runs leave a 2-cpu gap that only the narrow shape fits.
fn needle(rng: &mut Rng) -> Workload {
    let mut steps: Vec<(Need, f64)> = (0..1000).map(|_| (need(10, 0), rng.exp(2.0))).collect();
    for tenths in [30, 60] {
        for _ in 0..500 {
            steps.push((need(tenths, 0), rng.exp(3.0)));
        }
    }
    rng.shuffle(&mut steps);
    let mut w = Workload::new(None);
    for (need, secs) in steps {
        w.add(need, secs);
    }
    w
}

/// The same runs in more shapes: every run of a width in `groups` (cpu
/// tenths, how many shapes) also takes one of that many amounts of a pool
/// that never fills, in turn. What the runs compete for does not change.
fn reshape(mut w: Workload, groups: &[(u64, usize)]) -> Workload {
    assert!(w.other.is_none(), "the second pool is taken");
    w.other = Some(("memory", ROOMY));
    let mut seen = vec![0usize; groups.len()];
    for run in &mut w.runs {
        let group = groups
            .iter()
            .position(|&(tenths, _)| need(tenths, 0).cpu == run.need.cpu)
            .expect("every width has a group");
        run.need.other = (seen[group] % groups[group].1) as u64 + 1;
        seen[group] += 1;
    }
    w
}

/// How a grant came about, indexing [`Outcome::kinds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Arrival,
    Head,
    Handoff,
    Pass,
}

impl From<Admitted> for Kind {
    fn from(how: Admitted) -> Self {
        match how {
            Admitted::Arrival => Self::Arrival,
            Admitted::Head => Self::Head,
            Admitted::Handoff => Self::Handoff,
            Admitted::Pass => Self::Pass,
        }
    }
}

/// Strict FIFO, as the pool admitted before the rule (`enqueue`,
/// `grant_chain`, `release` and `cancel` of 60529218): the head waits until
/// everything it needs fits, and nothing behind it is admitted meanwhile. An
/// arrival is granted at once only when nobody waits.
#[derive(Debug)]
struct Fifo {
    capacity: [u64; 2],
    state: Mutex<FifoState>,
}

#[derive(Debug, Default)]
struct FifoState {
    held: [u64; 2],
    /// Arrival sequence → waiter. The first entry is the head.
    waiters: BTreeMap<u64, FifoWaiter>,
    next_seq: u64,
}

#[derive(Debug)]
struct FifoWaiter {
    need: Need,
    tx: oneshot::Sender<FifoGrant>,
}

impl Fifo {
    fn new(capacity: [u64; 2]) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            state: Mutex::new(FifoState::default()),
        })
    }

    fn fits(&self, held: [u64; 2], need: Need) -> bool {
        need.cpu <= self.capacity[0].saturating_sub(held[0])
            && need.other <= self.capacity[1].saturating_sub(held[1])
    }

    fn enqueue(self: &Arc<Self>, need: Need) -> FifoAcquire {
        let mut st = self.state.lock();
        if st.waiters.is_empty() && self.fits(st.held, need) {
            st.held[0] += need.cpu;
            st.held[1] += need.other;
            return FifoAcquire::Ready(FifoGrant {
                lane: Arc::clone(self),
                need,
                kind: Kind::Arrival,
                armed: true,
            });
        }
        let seq = st.next_seq;
        st.next_seq += 1;
        let (tx, rx) = oneshot::channel();
        st.waiters.insert(seq, FifoWaiter { need, tx });
        self.grant_chain(&mut st);
        FifoAcquire::Waiting {
            guard: FifoGuard {
                lane: Arc::clone(self),
                seq,
                armed: true,
            },
            rx,
        }
    }

    /// Grant from the head while the head fits. A send that fails hands the
    /// grant back, disarmed, so nothing armed is dropped under the lock.
    fn grant_chain(self: &Arc<Self>, st: &mut FifoState) {
        while let Some(head) = st.waiters.first_entry() {
            let need = head.get().need;
            if !self.fits(st.held, need) {
                break;
            }
            let waiter = head.remove();
            st.held[0] += need.cpu;
            st.held[1] += need.other;
            let grant = FifoGrant {
                lane: Arc::clone(self),
                need,
                kind: Kind::Head,
                armed: true,
            };
            if let Err(mut unobserved) = waiter.tx.send(grant) {
                unobserved.armed = false;
                st.held[0] -= need.cpu;
                st.held[1] -= need.other;
            }
        }
    }

    fn release(self: &Arc<Self>, need: Need) {
        let mut st = self.state.lock();
        st.held[0] -= need.cpu;
        st.held[1] -= need.other;
        self.grant_chain(&mut st);
    }

    /// Leave the queue (no-op once granted) and run the chain.
    fn cancel(self: &Arc<Self>, seq: u64) {
        let mut st = self.state.lock();
        if st.waiters.remove(&seq).is_some() {
            self.grant_chain(&mut st);
        }
    }

    fn idle(&self) -> bool {
        let st = self.state.lock();
        st.held == [0, 0] && st.waiters.is_empty()
    }
}

#[derive(Debug)]
struct FifoGrant {
    lane: Arc<Fifo>,
    need: Need,
    kind: Kind,
    armed: bool,
}

impl Drop for FifoGrant {
    fn drop(&mut self) {
        if self.armed {
            self.lane.release(self.need);
        }
    }
}

#[derive(Debug)]
struct FifoGuard {
    lane: Arc<Fifo>,
    seq: u64,
    armed: bool,
}

impl Drop for FifoGuard {
    fn drop(&mut self) {
        if self.armed {
            self.lane.cancel(self.seq);
        }
    }
}

#[derive(Debug)]
enum FifoAcquire {
    Ready(FifoGrant),
    Waiting {
        guard: FifoGuard,
        rx: oneshot::Receiver<FifoGrant>,
    },
}

impl FifoAcquire {
    async fn granted(self) -> FifoGrant {
        match self {
            Self::Ready(grant) => grant,
            Self::Waiting { mut guard, rx } => {
                let grant = rx.await.expect("a queued waiter is granted, never dropped");
                guard.armed = false;
                grant
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Fifo,
    Pool,
}

#[derive(Debug)]
enum Admission {
    Fifo(Arc<Fifo>),
    Pool(Arc<ResourcePool>),
}

/// An arrival: queued or granted by the time it exists.
#[derive(Debug)]
enum Pending {
    Fifo(FifoAcquire),
    Pool(Acquire),
}

impl Pending {
    async fn granted(self) -> Grant {
        match self {
            Self::Fifo(a) => Grant::Fifo(a.granted().await),
            Self::Pool(a) => Grant::Pool(a.await.expect("the pool outlives its waiters")),
        }
    }
}

/// Held while a run runs; dropping it is the release.
#[derive(Debug)]
enum Grant {
    Fifo(FifoGrant),
    Pool(Reservation),
}

impl Grant {
    fn kind(&self) -> Kind {
        match self {
            Self::Fifo(g) => g.kind,
            Self::Pool(r) => r.admitted().into(),
        }
    }
}

/// What one replay measured.
#[derive(Debug, Default, Clone)]
struct Outcome {
    wall: Duration,
    /// Micro-CPUs times nanoseconds, summed over every grant a run held.
    cpu_held: u128,
    /// Grants by [`Kind`].
    kinds: [u64; 4],
    /// How long the marked run waited.
    marked_wait: Option<Duration>,
    longest_wait: Duration,
}

#[derive(Debug)]
struct Live {
    deps: Vec<usize>,
    /// What the run tasks hold, counted here and not read from the pool.
    held: [u64; 2],
    left: usize,
    done: Option<oneshot::Sender<()>>,
    out: Outcome,
}

#[derive(Debug)]
struct Sim {
    label: String,
    admission: Admission,
    capacity: [u64; 2],
    runs: Vec<Run>,
    demands: Vec<Demand>,
    live: Mutex<Live>,
}

impl Sim {
    fn started(&self, id: usize, waited: Duration, kind: Kind) {
        let run = &self.runs[id];
        let mut live = self.live.lock();
        live.held[0] += run.need.cpu;
        live.held[1] += run.need.other;
        assert!(
            live.held[0] <= self.capacity[0] && live.held[1] <= self.capacity[1],
            "{}: run {id} ({:?}) was granted past a capacity: held {:?} of {:?}",
            self.label,
            run.need,
            live.held,
            self.capacity,
        );
        live.out.kinds[kind as usize] += 1;
        live.out.longest_wait = live.out.longest_wait.max(waited);
        if run.marked {
            live.out.marked_wait = Some(waited);
        }
    }

    fn ended(&self, id: usize, held_for: Duration) {
        let run = &self.runs[id];
        let mut live = self.live.lock();
        live.held[0] -= run.need.cpu;
        live.held[1] -= run.need.other;
        live.out.cpu_held += u128::from(run.need.cpu) * held_for.as_nanos();
    }
}

/// The arrival of run `id`: it is queued or granted here, before its task
/// first runs.
fn arrive(sim: &Arc<Sim>, id: usize) {
    let pending = match &sim.admission {
        Admission::Fifo(fifo) => Pending::Fifo(fifo.enqueue(sim.runs[id].need)),
        Admission::Pool(pool) => Pending::Pool(
            pool.acquire(&sim.demands[id])
                .expect("every pool is named and has capacity"),
        ),
    };
    tokio::spawn(run(Arc::clone(sim), id, Instant::now(), pending));
}

/// One run: wait for the grant, hold it for the run's duration, release it,
/// and only then let the dependents arrive.
fn run(sim: Arc<Sim>, id: usize, ready: Instant, pending: Pending) -> BoxFuture<'static, ()> {
    async move {
        let grant = pending.granted().await;
        let started = Instant::now();
        sim.started(id, started.duration_since(ready), grant.kind());
        tokio::time::sleep(Duration::from_millis(sim.runs[id].millis)).await;
        sim.ended(id, started.elapsed());
        drop(grant);
        for &next in &sim.runs[id].then {
            let ready = {
                let mut live = sim.live.lock();
                live.deps[next] -= 1;
                live.deps[next] == 0
            };
            if ready {
                arrive(&sim, next);
            }
        }
        let done = {
            let mut live = sim.live.lock();
            live.left -= 1;
            if live.left == 0 {
                live.done.take()
            } else {
                None
            }
        };
        if let Some(done) = done {
            done.send(()).expect("the replay waits for the last run");
        }
    }
    .boxed()
}

/// Replay `w` on `cpus` cpus under `rule`, in virtual time.
async fn replay(rule: Rule, cpus: u64, w: Workload, label: String) -> Outcome {
    let capacity = [cpus * CPU, w.other.map_or(0, |(_, cap)| cap)];
    let admission = match rule {
        Rule::Fifo => Admission::Fifo(Fifo::new(capacity)),
        Rule::Pool => Admission::Pool(ResourcePool::new(
            std::iter::once(("cpu", capacity[0]))
                .chain(w.other)
                .map(|(pool, cap)| (name(pool), Limit::Bounded(cap))),
        )),
    };
    let demands = w
        .runs
        .iter()
        .map(|r| match w.other {
            Some((pool, _)) if r.need.other > 0 => {
                demand(&[("cpu", r.need.cpu), (pool, r.need.other)])
            }
            _ => demand(&[("cpu", r.need.cpu)]),
        })
        .collect();
    let (done, finished) = oneshot::channel();
    let sim = Arc::new(Sim {
        label,
        admission,
        capacity,
        demands,
        live: Mutex::new(Live {
            deps: w.runs.iter().map(|r| r.deps).collect(),
            held: [0, 0],
            left: w.runs.len(),
            done: Some(done),
            out: Outcome::default(),
        }),
        runs: w.runs,
    });
    let start = Instant::now();
    for id in 0..sim.runs.len() {
        if sim.runs[id].deps == 0 {
            arrive(&sim, id);
        }
    }
    // With the clock paused, a queue that stops moving lets the timeout fire.
    if tokio::time::timeout(STUCK, finished).await.is_err() {
        let live = sim.live.lock();
        panic!(
            "{}: stuck with {} runs left, the run tasks holding {:?} of {:?}; admission: {:?}",
            sim.label, live.left, live.held, sim.capacity, sim.admission
        );
    }
    let wall = start.elapsed();
    match &sim.admission {
        Admission::Fifo(fifo) => assert!(fifo.idle(), "{}: FIFO did not drain", sim.label),
        Admission::Pool(pool) => {
            let snap = pool.snapshot().main;
            assert!(
                snap.queued == 0 && snap.pools.iter().all(|p| p.held == 0),
                "{}: the pool did not drain: {snap:?}",
                sim.label
            );
        }
    }
    let mut out = sim.live.lock().out.clone();
    out.wall = wall;
    out
}

/// A workload and what the spec's model predicted for it: the output of
/// `admission-backfill-sim.py` and `admission-backfill-mem-sim.py` for the
/// rule as built (sized patience 16, capped at 4 pools), mean of 10 seeds.
struct Case {
    name: &'static str,
    build: fn(&mut Rng) -> Workload,
    /// Wall-time change from strict FIFO on 8 and on 16 cpus, in percent.
    model: Option<(f64, f64)>,
    /// How long the marked run waits in the model, in seconds: under FIFO
    /// and under the rule over the spec's 10 seeds, then the same over 200
    /// seeds. The 10-seed means the spec quotes are 25 s above the 200-seed
    /// ones under FIFO, where the wait has a closed form that agrees with the
    /// 200.
    model_wait: Option<[f64; 4]>,
}

fn cases() -> Vec<Case> {
    let case = |name, build, model| Case {
        name,
        build,
        model,
        model_wait: None,
    };
    let whole = |name, build, model: f64, wait| Case {
        name,
        build,
        model: Some((model, f64::NAN)),
        model_wait: Some(wait),
    };
    vec![
        case("mixed widths, all ready", wide, Some((-19.3, -10.1))),
        case("dependency graph", dag, Some((-18.0, -9.1))),
        case(
            "crowd + 20 token runs",
            |rng| tokens(rng, &[40], 20, 20.0),
            Some((-6.4, -15.6)),
        ),
        case(
            "crowd + 3 shapes on the token",
            |rng| tokens(rng, &[40, 20, 10], 10, 12.0),
            Some((-10.8, -18.2)),
        ),
        case("cpu:8 gates a chain", gate, Some((-0.4, 0.0))),
        case("0.1/1-cpu critical path", relay, Some((0.0, 0.0))),
        whole(
            "whole memory: cpu and memory full",
            |rng| whole_pool(rng, &kinds(&[(8, need(10, 0)), (8, need(10, GIB))])),
            0.7,
            [58.3, 176.7, 33.0, 149.8],
        ),
        whole(
            "whole memory: memory full, cpu spare",
            |rng| whole_pool(rng, &kinds(&[(2, need(10, 0)), (6, need(10, 2 * GIB))])),
            -2.1,
            [83.5, 165.9, 58.3, 171.3],
        ),
        whole(
            "whole memory: nobody else uses memory",
            |rng| whole_pool(rng, &kinds(&[(16, need(10, 0))])),
            -0.6,
            [9.9, 73.9, 7.4, 67.2],
        ),
        case(
            "mixed widths, 100 shapes",
            |rng| reshape(wide(rng), &[(10, 85), (40, 10), (80, 5)]),
            None,
        ),
        case(
            "mixed widths, 1,000 shapes",
            |rng| reshape(wide(rng), &[(10, 850), (40, 100), (80, 50)]),
            None,
        ),
        case("needle, 3 shapes", needle, None),
        case(
            "needle, 100 shapes",
            |rng| reshape(needle(rng), &[(10, 1), (30, 49), (60, 50)]),
            None,
        ),
        case(
            "needle, 1,000 shapes",
            |rng| reshape(needle(rng), &[(10, 1), (30, 499), (60, 500)]),
            None,
        ),
    ]
}

/// A mean over the seeds, with the range.
struct Stat {
    mean: f64,
    min: f64,
    max: f64,
}

impl Stat {
    fn of(values: impl IntoIterator<Item = f64>) -> Self {
        let values: Vec<f64> = values.into_iter().collect();
        Self {
            mean: values.iter().sum::<f64>() / values.len() as f64,
            min: values.iter().copied().fold(f64::INFINITY, f64::min),
            max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        }
    }

    fn show(&self) -> String {
        format!("{:.1} ({:.1}-{:.1})", self.mean, self.min, self.max)
    }
}

/// Appends a table: the first column left-aligned, the rest right-aligned.
fn table(out: &mut String, title: &str, head: &[&str], rows: &[Vec<String>]) {
    let width = |c: usize| {
        rows.iter()
            .map(|r| r[c].chars().count())
            .chain(std::iter::once(head[c].chars().count()))
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..head.len()).map(width).collect();
    let line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut text = String::new();
        for (c, cell) in cells.enumerate() {
            if c == 0 {
                text.push_str(&format!("{cell:<w$}", w = widths[c]));
            } else {
                text.push_str(&format!("  {cell:>w$}", w = widths[c]));
            }
        }
        text.push('\n');
        text
    };
    out.push_str(&format!("\n== {title}\n\n"));
    out.push_str(&line(&mut head.iter().copied()));
    let rule: Vec<String> = widths.iter().map(|&w| "-".repeat(w)).collect();
    out.push_str(&line(&mut rule.iter().map(String::as_str)));
    for row in rows {
        out.push_str(&line(&mut row.iter().map(String::as_str)));
    }
}

/// Each seed's replay under strict FIFO and under the pool.
async fn measure(case: &Case, cpus: u64) -> Vec<[Outcome; 2]> {
    let mut seeds = Vec::new();
    for seed in 0..seeds_wanted() {
        let label = |rule: &str| format!("{} / {cpus} cpu / seed {seed} / {rule}", case.name);
        let fifo = (case.build)(&mut Rng(seed));
        let pool = (case.build)(&mut Rng(seed));
        seeds.push([
            replay(Rule::Fifo, cpus, fifo, label("FIFO")).await,
            replay(Rule::Pool, cpus, pool, label("pool")).await,
        ]);
    }
    seeds
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn percent(of: f64, base: f64) -> f64 {
    (of / base - 1.0) * 100.0
}

fn wall_row(case: &Case, seeds: &[[Outcome; 2]], model: Option<f64>) -> Vec<String> {
    let probe = (case.build)(&mut Rng(0));
    let fifo = Stat::of(seeds.iter().map(|s| secs(s[0].wall)));
    let pool = Stat::of(seeds.iter().map(|s| secs(s[1].wall)));
    let paired = Stat::of(
        seeds
            .iter()
            .map(|s| percent(secs(s[1].wall), secs(s[0].wall))),
    );
    let slower = seeds.iter().filter(|s| s[1].wall > s[0].wall).count();
    vec![
        case.name.to_string(),
        probe.shapes().to_string(),
        probe.runs.len().to_string(),
        fifo.show(),
        pool.show(),
        format!("{:+.1}%", percent(pool.mean, fifo.mean)),
        format!("{:+.1}..{:+.1}%", paired.min, paired.max),
        format!("{slower}/{}", seeds.len()),
        model.map_or_else(|| "-".to_string(), |m| format!("{m:+.1}%")),
    ]
}

const WALL_HEAD: [&str; 9] = [
    "workload",
    "shapes",
    "runs",
    "FIFO wall s",
    "pool wall s",
    "change",
    "per seed",
    "slower",
    "model",
];

fn utilization(o: &Outcome, cpus: u64) -> f64 {
    100.0 * o.cpu_held as f64 / ((cpus * CPU) as f64 * o.wall.as_nanos() as f64)
}

fn util_row(case: &Case, seeds: &[[Outcome; 2]], cpus: u64) -> Vec<String> {
    let util = |rule: usize| Stat::of(seeds.iter().map(|s| utilization(&s[rule], cpus))).show();
    let grants: u64 = seeds.iter().flat_map(|s| s[1].kinds).sum();
    let share = |kind: Kind| {
        let n: u64 = seeds.iter().map(|s| s[1].kinds[kind as usize]).sum();
        format!("{:.1}", 100.0 * n as f64 / grants as f64)
    };
    let wait = |rule: usize| Stat::of(seeds.iter().map(|s| secs(s[rule].longest_wait))).show();
    assert!(
        seeds
            .iter()
            .all(|s| s[0].kinds[Kind::Handoff as usize] + s[0].kinds[Kind::Pass as usize] == 0),
        "strict FIFO let a run pass"
    );
    vec![
        case.name.to_string(),
        util(0),
        util(1),
        share(Kind::Arrival),
        share(Kind::Head),
        share(Kind::Handoff),
        share(Kind::Pass),
        wait(0),
        wait(1),
    ]
}

fn whole_row(case: &Case, seeds: &[[Outcome; 2]]) -> Option<Vec<String>> {
    let [fifo10, rule10, fifo200, rule200] = case.model_wait?;
    let wait = |rule: usize| {
        Stat::of(
            seeds
                .iter()
                .map(|s| secs(s[rule].marked_wait.expect("the whole-pool run was granted"))),
        )
        .show()
    };
    Some(vec![
        case.name.to_string(),
        wait(0),
        wait(1),
        format!("{fifo10:.0} -> {rule10:.0}"),
        format!("{fifo200:.0} -> {rule200:.0}"),
    ])
}

async fn replay_report() -> String {
    let mut out = String::new();
    let cases = cases();
    let (mut wall, mut util, mut whole, mut wall16) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for case in &cases {
        let seeds = measure(case, 8).await;
        wall.push(wall_row(case, &seeds, case.model.map(|m| m.0)));
        util.push(util_row(case, &seeds, 8));
        whole.extend(whole_row(case, &seeds));
        // The whole-pool situations are defined on 8 cpus only.
        if case.model_wait.is_none() {
            let seeds = measure(case, 16).await;
            wall16.push(wall_row(case, &seeds, case.model.map(|m| m.1)));
        }
    }
    out.push_str(&format!(
        "\nadmission_replay: the real pool against strict FIFO, {} seeds, virtual time.\n\
         A cell is `mean (min-max)` over the seeds. `change` compares the means;\n\
         `per seed` is the best and worst paired change; `slower` counts the seeds\n\
         where the pool took longer than FIFO; `model` is the spec's prediction.\n",
        seeds_wanted()
    ));
    table(&mut out, "Wall time, 8 cpu", &WALL_HEAD, &wall);
    table(
        &mut out,
        "Cpu held by running runs (% of 8 cpu x wall), the pool's grants by kind (%), longest wait of any run (s)",
        &[
            "workload",
            "FIFO util",
            "pool util",
            "arrival",
            "head",
            "hand-off",
            "pass",
            "FIFO longest wait",
            "pool longest wait",
        ],
        &util,
    );
    table(
        &mut out,
        "How long the run that wants the whole memory pool waits (s), 8 cpu",
        &[
            "situation",
            "FIFO",
            "pool",
            "model, its 10 seeds",
            "model, 200 seeds",
        ],
        &whole,
    );
    table(&mut out, "Wall time, 16 cpu", &WALL_HEAD, &wall16);
    out
}

/// The rule against strict FIFO, on the model's workloads and on many shapes.
#[test]
#[ignore = "an instrument: it prints tables for a human, see the module docs"]
fn replay_against_strict_fifo() {
    let _serial = SERIAL.lock();
    // What `#[tokio::test(start_paused = true)]` builds, without holding the
    // lock above across an await.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap();
    let report = runtime.block_on(replay_report());
    print!("{report}");
}

const WAITERS: usize = 10_000;
const REPS: usize = 20_000;
const WARMUP: usize = 1_000;

/// Nanoseconds per call of one event kind.
#[derive(Debug, Default)]
struct Samples(Vec<u64>);

impl Samples {
    fn time<T>(&mut self, record: bool, event: impl FnOnce() -> T) -> T {
        let start = std::time::Instant::now();
        let out = event();
        let nanos = start.elapsed().as_nanos();
        if record {
            self.0.push(u64::try_from(nanos).unwrap_or(u64::MAX));
        }
        out
    }

    fn row(mut self, event: &str) -> Vec<String> {
        self.0.sort_unstable();
        let n = self.0.len();
        let at = |per_mille: usize| self.0[(n * per_mille / 1000).min(n - 1)];
        let mean = self.0.iter().sum::<u64>() as f64 / n as f64;
        vec![
            event.to_string(),
            n.to_string(),
            format!("{mean:.0}"),
            at(500).to_string(),
            at(990).to_string(),
            at(1000).to_string(),
        ]
    }
}

/// A full pool with [`WAITERS`] waiters behind it, set up so each kind of
/// event can be repeated without the queue changing shape.
///
/// 8 cpus are held by seven 1-cpu grants of waiting shapes and two half-cpu
/// grants of a shape nobody waits in. Releasing a 1-cpu grant hands its slot
/// to the oldest waiter of its shape; releasing a half frees less than any
/// waiter needs, so the walk refuses every class it tests.
struct Queue {
    pool: Arc<ResourcePool>,
    shapes: Vec<Demand>,
    half: Demand,
    /// Waiters per shape, oldest first.
    waiting: Vec<VecDeque<Acquire>>,
    /// The held 1-cpu grants, with the index of their shape.
    running: VecDeque<(usize, Reservation)>,
    halves: Vec<Reservation>,
    /// The holder of the one `docker` slot and the head, which waits for it.
    /// Nothing behind the head takes `docker`, so every grant leaves it room
    /// and it is never protected: the head stays the head.
    blocked: Option<(Reservation, Acquire)>,
}

impl Queue {
    /// `shapes` are the waiting shapes; the first `narrow` of them take 1 cpu.
    /// Without `blocked_head` the head is the oldest of those waiters.
    fn build(shapes: &[Demand], narrow: usize, blocked_head: bool) -> Self {
        let pool = ResourcePool::new(
            [("cpu", 8 * CPU), ("docker", 1), ("memory", ROOMY)]
                .map(|(pool, cap)| (name(pool), Limit::Bounded(cap))),
        );
        let now = |d: &Demand| {
            pool.acquire(d)
                .unwrap()
                .now_or_never()
                .expect("granted at once")
                .unwrap()
        };
        let half = demand(&[("cpu", CPU / 2)]);
        let running = (0..7)
            .map(|i| (i % narrow, now(&shapes[i % narrow])))
            .collect();
        let halves = vec![now(&half), now(&half)];
        let docker = demand(&[("cpu", 0), ("docker", 1)]);
        let holder = blocked_head.then(|| now(&docker));
        let mut q = Self {
            shapes: shapes.to_vec(),
            half,
            waiting: shapes.iter().map(|_| VecDeque::new()).collect(),
            running,
            halves,
            blocked: None,
            pool,
        };
        if let Some(holder) = holder {
            let mut head = q.pool.acquire(&docker).unwrap();
            assert!((&mut head).now_or_never().is_none(), "the head must wait");
            q.blocked = Some((holder, head));
        }
        for i in 0..WAITERS {
            q.arrive(i % shapes.len());
        }
        q
    }

    /// A waiter of shape `s` arrives and queues.
    fn arrive(&mut self, s: usize) {
        let arrival = self.pool.acquire(&self.shapes[s]).unwrap();
        self.park(s, arrival);
    }

    /// Keep an arrival of shape `s` that had to queue.
    fn park(&mut self, s: usize, mut arrival: Acquire) {
        assert!((&mut arrival).now_or_never().is_none(), "must queue");
        self.waiting[s].push_back(arrival);
    }

    /// The grant the oldest waiter of shape `s` was just sent.
    fn granted(&mut self, s: usize, how: Admitted) -> Reservation {
        let mut waiter = self.waiting[s].pop_front().expect("a waiter of the shape");
        let grant = (&mut waiter)
            .now_or_never()
            .expect("the oldest waiter of the shape was granted")
            .unwrap();
        assert_eq!(grant.admitted(), how);
        grant
    }

    /// Release a 1-cpu grant, timed: its slot is handed to the oldest waiter
    /// of its shape. Then, timed, a waiter of that shape arrives and queues.
    fn hand_off(&mut self, record: bool, handed: &mut Samples, queues: &mut Samples) {
        let (s, grant) = self.running.pop_front().expect("a running grant");
        handed.time(record, || drop(grant));
        let next = self.granted(s, Admitted::Handoff);
        self.running.push_back((s, next));
        let arrival = queues.time(record, || self.pool.acquire(&self.shapes[s]).unwrap());
        self.park(s, arrival);
    }

    /// The head is who it was, with everything else still queued behind it.
    fn settled(&self) {
        let waiting: usize = self.waiting.iter().map(VecDeque::len).sum();
        assert_eq!(waiting, WAITERS);
        let queued = waiting + usize::from(self.blocked.is_some());
        assert_eq!(self.pool.snapshot().main.queued, queued);
        if let Some((_, head)) = &self.blocked {
            assert_eq!(head.wait_info().map(|info| info.ahead), Some(0));
        }
    }
}

/// Time every kind of event on a queue of `shapes`, whose first `narrow`
/// take 1 cpu.
fn lock_hold_rows(shapes: &[Demand], narrow: usize) -> Vec<Vec<String>> {
    // More classes than one walk tests: the events after a walk resume it.
    let capped = shapes.len() > 64;
    let mut rows = Vec::new();

    // No walk is unfinished: a hand-off is one lookup and one grant.
    let mut q = Queue::build(shapes, narrow, true);
    let (mut handed, mut queues) = (Samples::default(), Samples::default());
    for i in 0..WARMUP + REPS {
        q.hand_off(i >= WARMUP, &mut handed, &mut queues);
    }
    q.settled();
    rows.push(handed.row("release, handed off"));
    rows.push(queues.row("arrival, queues"));

    // A release nobody of its shape waits for walks; here nothing fits what
    // it freed, so every class tested is refused. An arrival of that shape
    // then fits and passes the head.
    let mut q = Queue::build(shapes, narrow, true);
    let (mut walks, mut passes) = (Samples::default(), Samples::default());
    let (mut handed, mut queues) = (Samples::default(), Samples::default());
    for i in 0..WARMUP + REPS {
        let record = i >= WARMUP;
        let half = q.halves.pop().expect("a half-cpu grant");
        walks.time(record, || drop(half));
        q.hand_off(record, &mut handed, &mut queues);
        let mut arrival = passes.time(record, || q.pool.acquire(&q.half).unwrap());
        let grant = (&mut arrival)
            .now_or_never()
            .expect("half a cpu is free")
            .unwrap();
        assert_eq!(grant.admitted(), Admitted::Pass);
        q.halves.push(grant);
    }
    q.settled();
    rows.push(walks.row("release, walks, nothing fits"));
    if capped {
        rows.push(handed.row("release, handed off, resumes a walk"));
        rows.push(queues.row("arrival, queues, resumes a walk"));
        rows.push(passes.row("arrival, passes, resumes a walk"));
    } else {
        rows.push(passes.row("arrival, passes"));
    }

    let mut q = Queue::build(shapes, narrow, true);
    let mut cancels = Samples::default();
    for i in 0..WARMUP + REPS {
        let s = i % shapes.len();
        let middle = q.waiting[s].len() / 2;
        let waiter = q.waiting[s].remove(middle).expect("a waiter of the shape");
        cancels.time(i >= WARMUP, || drop(waiter));
        q.arrive(s);
    }
    q.settled();
    rows.push(cancels.row("cancellation behind the head"));

    // The head is the oldest waiter, and the queue is in shape order: when
    // it leaves, the next shape's oldest waiter is the head, and the walk
    // runs for it.
    let mut q = Queue::build(shapes, narrow, false);
    let mut heads = Samples::default();
    for i in 0..WARMUP + REPS {
        let s = i % shapes.len();
        let head = q.waiting[s].pop_front().expect("a waiter of the shape");
        assert_eq!(head.wait_info().map(|info| info.ahead), Some(0));
        heads.time(i >= WARMUP, || drop(head));
        q.arrive(s);
    }
    q.settled();
    rows.push(heads.row("cancellation of the head"));

    let q = Queue::build(shapes, narrow, true);
    let last = q.waiting[(WAITERS - 1) % shapes.len()]
        .back()
        .expect("the last waiter");
    let (mut infos, mut snapshots, mut clock) =
        (Samples::default(), Samples::default(), Samples::default());
    for _ in 0..REPS / 10 {
        let info = infos.time(true, || last.wait_info()).expect("it waits");
        assert_eq!(info.ahead, WAITERS);
    }
    for _ in 0..REPS {
        let snapshot = snapshots.time(true, || q.pool.snapshot());
        assert_eq!(snapshot.main.queued, WAITERS + 1);
        clock.time(true, || ());
    }
    rows.push(infos.row("wait_info() of the last waiter"));
    rows.push(snapshots.row("snapshot()"));
    rows.push(clock.row("the clock alone"));
    rows
}

/// What each admission event costs with 10,000 waiters queued.
#[test]
#[ignore = "an instrument: it prints tables for a human, see the module docs"]
fn lock_hold_with_ten_thousand_waiters() {
    let _serial = SERIAL.lock();
    let head = ["event", "calls", "mean ns", "p50 ns", "p99 ns", "max ns"];
    let mut out = format!(
        "\nadmission_replay: the duration of one call, real time, one thread, {WAITERS} waiters queued.\n\
         It bounds how long the call holds the lane mutex.\n"
    );
    let few: Vec<Demand> = [1, 2, 4, 8]
        .iter()
        .map(|&n| demand(&[("cpu", n * CPU)]))
        .collect();
    table(
        &mut out,
        "Lock hold, 5 shapes waiting (cpu 1, 2, 4, 8 and the head)",
        &head,
        &lock_hold_rows(&few, 1),
    );
    let many: Vec<Demand> = (1..=1000)
        .map(|n| demand(&[("cpu", CPU), ("memory", n)]))
        .collect();
    table(
        &mut out,
        "Lock hold, 1,001 shapes waiting (1,000 of cpu 1 and the head)",
        &head,
        &lock_hold_rows(&many, many.len()),
    );
    print!("{out}");
}
