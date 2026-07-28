//! Live self-diagnosis: what is this process doing *right now*, and is it stuck?
//!
//! heph already knew why it was hanging. Resolving a 27,569-target build, it held
//! "98 targets open in remote-cache-read, oldest 512s, zero completions in 480s"
//! in memory for forty minutes and never said so, because the only thing that
//! rendered it was the TUI — which degrades under exactly the load that raises the
//! question. Diagnosing that took a day of `/proc` forensics and three wrong
//! hypotheses. This module exists so the next one takes one paragraph.
//!
//! # One table, several renderers
//!
//! [`DiagState`] is the single source: open-span counts per op kind, the oldest
//! open span of each, bytes moved in a rolling window, limiter saturation, and the
//! timestamp of the last state transition of any kind. The stall watchdog, the
//! published status file, and the end-of-run summary are all views over it.
//! Deriving it at a *consumer* instead would make the view only as fresh as that
//! consumer's drain loop, and a wedged process's table would go stale precisely
//! when it matters.
//!
//! # Why the hook body is atomics only
//!
//! [`DiagHook::on_event`] runs synchronously on the emitting thread, from every
//! worker, roughly eight times per target. At 27k targets that is ~220k calls
//! through one shared structure. A `Mutex`, a `HashMap`, an allocation or a
//! `format!` here would make this another `hmemoizer::set_phase` — which takes a
//! global lock per await point and is therefore permanently gated off behind an
//! env var. A diagnostic that has to be switched on cannot help with the hang you
//! did not anticipate, so this one is always on, and everything it does is a
//! relaxed atomic. All string work happens on the watchdog thread, after a stall
//! has already been detected.
//!
//! # Why the trigger is "no transition", not "no completion"
//!
//! `heph r //some:link_step` on a single thirty-minute target emits one
//! `ResultStart` and then nothing until it finishes. "No target completed in 60s"
//! would print a stall paragraph on a perfectly healthy build — and a diagnostic
//! that cries wolf is worse than no diagnostic, because people learn to skip it.
//! A build that is progressing *opens* spans even when it closes none, so
//! [`DiagState::last_transition`] advances on starts, ends, cache hits and misses,
//! and on bytes moving.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hcore::events::{BuildEvent, BuildEventKind};

/// Op kinds tracked separately. Deliberately coarse: the paragraph needs to name
/// *which subsystem* is stuck, not reproduce the whole event taxonomy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Result,
    Execute,
    RemoteCacheRead,
    RemoteCacheWrite,
    LocalCacheWrite,
}

impl Op {
    pub const ALL: [Op; 5] = [
        Op::Result,
        Op::Execute,
        Op::RemoteCacheRead,
        Op::RemoteCacheWrite,
        Op::LocalCacheWrite,
    ];

    const fn idx(self) -> usize {
        match self {
            Op::Result => 0,
            Op::Execute => 1,
            Op::RemoteCacheRead => 2,
            Op::RemoteCacheWrite => 3,
            Op::LocalCacheWrite => 4,
        }
    }

    /// Human label used in the stall paragraph.
    pub const fn label(self) -> &'static str {
        match self {
            Op::Result => "result",
            Op::Execute => "execute",
            Op::RemoteCacheRead => "remote-cache-read",
            Op::RemoteCacheWrite => "remote-cache-write",
            Op::LocalCacheWrite => "local-cache-write",
        }
    }

    /// Whether an "oldest open" age can be reported for this op.
    ///
    /// Every op except [`Op::Result`] is bounded by a semaphore (workers, the
    /// remote-cache request budget, the blocking pool), so a fixed slot array
    /// tracks all of them exactly. `Result` spans are *not* bounded — every
    /// ancestor holds one open while awaiting its children, so thousands are live
    /// at once on a deep graph. Rather than report an age from a slot array that
    /// silently evicted the span we care about, report count only and say so.
    pub const fn tracks_oldest(self) -> bool {
        !matches!(self, Op::Result)
    }

    /// Whether [`DiagState::add_bytes`] is ever called for this op.
    ///
    /// Only the remote-cache read path moves bytes through a counter. For every
    /// other op the window is structurally zero, which makes "0 B in the last
    /// 60s" both meaningless as a line and *actively misleading* as evidence:
    /// [`StallReport::dominant_is_starved`] would read that hard zero as proof
    /// of starvation and print a confident hypothesis derived from a counter
    /// that cannot move. A wrong automated hypothesis costs more credibility
    /// than a missing one, so ops that do not report bytes say nothing.
    pub const fn reports_bytes(self) -> bool {
        matches!(self, Op::RemoteCacheRead)
    }
}

/// Per-op slots for oldest-open tracking.
///
/// Sized to the *concurrency bound* of the ops that use it, not to the target
/// count. A ring buffer indexed by a sequence number would be wrong in the one
/// case that matters: it evicts by insertion order, so the span that opened at
/// t=0 and is still open gets overwritten by later spans, and the watchdog reports
/// "oldest 2s" in the middle of a 512s stall.
const SLOTS: usize = 512;

/// Sentinel for a free slot: no span can legitimately hash to it.
const FREE: u64 = 0;

/// At or below this many open `Execute` spans and nothing else, silence is
/// treated as "subprocesses are working" rather than a stall — until
/// [`QUIET_EXEC_FACTOR`] times the threshold has passed.
const QUIET_EXEC_MAX: u64 = 4;

/// How much longer a silent-subprocess build is given before it is reported.
const QUIET_EXEC_FACTOR: u64 = 10;

/// Rolling window over which bytes-moved is reported.
pub const BYTES_WINDOW: Duration = Duration::from_secs(60);

struct OpState {
    /// Open spans.
    ///
    /// Signed, and clamped at read. `progress.rs` documents a real same-addr
    /// double-fire that it defends against with set semantics; a counter has no
    /// set to dedup against, so a duplicated `*End` decrements twice. Unsigned,
    /// that prints `18446744073709551615 open` — the diagnostic lying during the
    /// incident it exists to explain.
    open: AtomicI64,
    /// `(span key, start millis)` pairs. Key `FREE` means the slot is free.
    slot_key: [AtomicU64; SLOTS],
    slot_start: [AtomicU64; SLOTS],
    /// Cumulative bytes moved by this op.
    bytes_total: AtomicU64,
    /// Bytes at the start of the current window, and when that window opened.
    bytes_window_base: AtomicU64,
    window_opened_ms: AtomicU64,
}

/// Age of the oldest span still holding a slot on `st`.
fn oldest_of(st: &OpState, now_ms: u64) -> Option<Duration> {
    st.slot_key
        .iter()
        .zip(st.slot_start.iter())
        .filter(|(k, _)| k.load(Ordering::Acquire) != FREE)
        .map(|(_, start)| start.load(Ordering::Acquire))
        .min()
        .map(|s| Duration::from_millis(now_ms.saturating_sub(s)))
}

impl OpState {
    fn new() -> Self {
        Self {
            open: AtomicI64::new(0),
            slot_key: [const { AtomicU64::new(FREE) }; SLOTS],
            slot_start: [const { AtomicU64::new(0) }; SLOTS],
            bytes_total: AtomicU64::new(0),
            bytes_window_base: AtomicU64::new(0),
            window_opened_ms: AtomicU64::new(0),
        }
    }
}

/// A limiter worth naming in the stall paragraph.
///
/// Reports saturation, not queue depth. Depth needs a wrapper around every
/// acquire — affordable at these sites, but it measures the wrong thing at the
/// one that matters: `meta_op` takes `shared_slots` with `try_acquire` and only
/// ever *awaits* on `meta_reserve`, so metadata never queues on `shared_slots` by
/// construction and a depth gauge there reads zero during a metadata starvation.
/// "Saturated for 47s" is both cheaper and more actionable than "depth 213".
pub struct Limiter {
    pub name: &'static str,
    /// Millis at which this limiter last went from "has permits" to "none", or 0.
    saturated_since_ms: AtomicU64,
    /// Reads the limiter's *current* free permits, when one has been attached.
    ///
    /// Without it a limiter is only ever sampled from its own acquire path, so
    /// the reading freezes the moment nothing acquires any more — which is
    /// exactly when the stall watchdog fires. A wedged build then reported
    /// "workers saturated for 90s" from a stamp left by the last acquire
    /// attempt, indistinguishable from a pool that is still fully held.
    /// Telling those two apart is the whole question at a stall: permits held
    /// means deadlocked on the pool, permits free means wedged elsewhere.
    gauge: std::sync::OnceLock<Box<dyn Fn() -> usize + Send + Sync>>,
}

impl Limiter {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            saturated_since_ms: AtomicU64::new(0),
            gauge: std::sync::OnceLock::new(),
        }
    }

    /// Attach a live reader of this limiter's free permits.
    ///
    /// Idempotent: the first attachment wins and later ones are dropped, so a
    /// call site on a hot path can arm it without a separate one-time hook.
    /// Only read when a report is being rendered, never on the acquire path.
    pub fn attach_gauge(&self, gauge: impl Fn() -> usize + Send + Sync + 'static) {
        drop(self.gauge.set(Box::new(gauge)));
    }

    /// Re-read the live gauge, if there is one, so a report reflects the present
    /// rather than the last acquire.
    fn refresh(&self, now_ms: u64) {
        if let Some(gauge) = self.gauge.get() {
            self.observe(gauge(), now_ms);
        }
    }

    /// Record the limiter's current availability. One relaxed load plus, on a
    /// transition, one store — cheap enough for an acquire path.
    pub fn observe(&self, available: usize, now_ms: u64) {
        if available == 0 {
            // Only stamp the *transition*, so the age keeps growing.
            let _prev = self.saturated_since_ms.compare_exchange(
                0,
                now_ms.max(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        } else {
            self.saturated_since_ms.store(0, Ordering::Relaxed);
        }
    }

    fn saturated_for(&self, now_ms: u64) -> Option<Duration> {
        let since = self.saturated_since_ms.load(Ordering::Relaxed);
        (since != 0).then(|| Duration::from_millis(now_ms.saturating_sub(since)))
    }
}

/// The live table. One per process.
pub struct DiagState {
    /// Monotonic base. Everything else is millis since this, so timestamps are
    /// lock-free `u64`s and immune to the wall clock stepping — `at_unix_ms` on
    /// the events is `SystemTime`, and an NTP correction there makes an elapsed
    /// go negative.
    base: Instant,
    /// Millis of the last state transition of *any* kind. The stall trigger.
    last_transition_ms: AtomicU64,
    ops: [OpState; 5],
    limiters: Vec<Limiter>,
    /// Completed / failed targets, for the progress line.
    done: AtomicU64,
    failed: AtomicU64,
    /// Blocked acquisitions of the per-addr result lock.
    ///
    /// Not an [`Op`]: `Op` is the vocabulary of the TUI's per-target operation
    /// timeline, and a lock wait is not an operation the target is performing —
    /// it is the target being prevented from performing one. Tracked separately
    /// so the stall paragraph can name it without perturbing what `Op` means.
    ///
    /// The wait is what makes cross-*process* contention diagnosable at all: the
    /// filesystem lock backend is the default, so the holder can be another heph
    /// on the same machine, and no amount of introspection into this process
    /// would ever find it.
    lock_wait: OpState,
    /// Pid believed to hold the lock at the most recent blocked acquisition;
    /// `0` for none/unknown. Last-writer-wins — a diagnostic hint, not a census.
    lock_holder_pid: AtomicU64,
    /// Worker permits currently held by a target that is actually running —
    /// incremented once `acquire()` has *returned*, decremented when the permit
    /// drops.
    ///
    /// The point is the arithmetic against the pool's own free count. A tokio
    /// `Semaphore` hands a released permit to the first queued waiter and wakes
    /// it, so a permit can be *granted* to an `Acquire` future that is never
    /// polled again — spent, but held by nobody. That permit is invisible from
    /// both ends: the pool reports it as taken, and no target reports running.
    ///
    /// `capacity - free - running` names exactly that population, which is the
    /// difference between "the pool is busy" and "the pool has been leaked away"
    /// — and those two want opposite investigations.
    workers_running: AtomicI64,
    /// The pool's permit count, or 0 before the engine has registered it.
    workers_capacity: AtomicU64,
    /// Reads the pool's *current* free permits. Sampled while a report is being
    /// rendered, never on the acquire path.
    workers_free: std::sync::OnceLock<Box<dyn Fn() -> usize + Send + Sync>>,
}

/// Marks a worker permit as held by a *running* target for as long as it lives.
///
/// Tied to the permit's own scope, so cancellation releases it on the same path
/// the permit itself is released on — a count that could drift would be worse
/// than no count, since the whole value here is the arithmetic.
pub struct RunningPermit(std::sync::Arc<DiagState>);

impl RunningPermit {
    #[expect(clippy::new_without_default, reason = "a guard, not a value")]
    pub fn new() -> Self {
        Self::on(std::sync::Arc::clone(global()))
    }

    /// Count against a specific table rather than the process-wide one. Lets the
    /// accounting be asserted without every test sharing one counter.
    pub fn on(state: std::sync::Arc<DiagState>) -> Self {
        state.worker_permit_acquired();
        Self(state)
    }
}

impl Drop for RunningPermit {
    fn drop(&mut self) {
        self.0.worker_permit_released();
    }
}

/// Worker-permit accounting, as of one report.
#[derive(Debug, Clone, Copy)]
pub struct WorkerPermits {
    pub capacity: u64,
    pub free: u64,
    /// Held by a target that is actually running.
    pub running: u64,
}

impl WorkerPermits {
    /// Permits the pool considers taken that no running target holds — granted
    /// to a waiter that is not being polled, and therefore never coming back.
    pub fn unaccounted(&self) -> u64 {
        self.capacity
            .saturating_sub(self.free)
            .saturating_sub(self.running)
    }
}

impl DiagState {
    pub fn new(limiters: Vec<Limiter>) -> Self {
        Self {
            base: Instant::now(),
            last_transition_ms: AtomicU64::new(0),
            ops: [
                OpState::new(),
                OpState::new(),
                OpState::new(),
                OpState::new(),
                OpState::new(),
            ],
            limiters,
            done: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            lock_wait: OpState::new(),
            lock_holder_pid: AtomicU64::new(0),
            workers_running: AtomicI64::new(0),
            workers_capacity: AtomicU64::new(0),
            workers_free: std::sync::OnceLock::new(),
        }
    }

    /// Register the worker pool: its permit count, and a live reader of its free
    /// permits. Idempotent; the first registration wins.
    pub fn register_worker_pool(
        &self,
        capacity: usize,
        free: impl Fn() -> usize + Send + Sync + 'static,
    ) {
        self.workers_capacity
            .store(capacity as u64, Ordering::Relaxed);
        drop(self.workers_free.set(Box::new(free)));
    }

    /// A worker permit has been acquired and the target is now running. Paired
    /// with [`worker_permit_released`](Self::worker_permit_released).
    pub fn worker_permit_acquired(&self) {
        self.workers_running.fetch_add(1, Ordering::Relaxed);
    }

    pub fn worker_permit_released(&self) {
        self.workers_running.fetch_sub(1, Ordering::Relaxed);
    }

    /// Worker-permit accounting, or `None` before the pool has been registered.
    pub fn worker_permits(&self) -> Option<WorkerPermits> {
        let free = self.workers_free.get()?;
        let capacity = self.workers_capacity.load(Ordering::Relaxed);
        if capacity == 0 {
            return None;
        }
        Some(WorkerPermits {
            capacity,
            free: free() as u64,
            running: u64::try_from(self.workers_running.load(Ordering::Relaxed).max(0))
                .unwrap_or(0),
        })
    }

    /// Record the start of a blocked result-lock acquisition.
    pub fn lock_wait_start(&self, addr: &str, holder_pid: Option<u32>, now_ms: u64) {
        if let Some(pid) = holder_pid {
            self.lock_holder_pid
                .store(u64::from(pid), Ordering::Relaxed);
        }
        self.span_start(&self.lock_wait, addr, now_ms);
    }

    /// Record the end of a blocked result-lock acquisition (acquired or
    /// cancelled — `ResultLockWaitEnd` fires for both).
    pub fn lock_wait_end(&self, addr: &str, now_ms: u64) {
        self.span_end(&self.lock_wait, addr, now_ms);
    }

    /// `(open waits, oldest age, holder pid)` — `None` when nothing is blocked.
    pub fn lock_waits(&self, now_ms: u64) -> Option<(u64, Option<Duration>, Option<u32>)> {
        let open = u64::try_from(self.lock_wait.open.load(Ordering::Relaxed).max(0)).unwrap_or(0);
        if open == 0 {
            return None;
        }
        let pid = self.lock_holder_pid.load(Ordering::Relaxed);
        Some((
            open,
            oldest_of(&self.lock_wait, now_ms),
            (pid != 0).then(|| u32::try_from(pid).unwrap_or(0)),
        ))
    }

    /// Infallible by construction (`ops` is sized to `Op::ALL`), but resolved
    /// through `get` so a future op kind added without extending the array
    /// degrades to "not tracked" rather than panicking inside a diagnostic.
    fn op(&self, op: Op) -> Option<&OpState> {
        self.ops.get(op.idx())
    }

    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Millis since anything at all happened.
    fn quiet_for_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_transition_ms.load(Ordering::Relaxed))
    }

    fn touch(&self, now_ms: u64) {
        self.last_transition_ms.store(now_ms, Ordering::Relaxed);
    }

    /// Hash an addr into a non-[`FREE`] slot key. A collision costs at worst
    /// freeing the wrong one of two same-hash spans, which perturbs a reported
    /// age — acceptable for a diagnostic, and never a correctness issue.
    fn key_of(addr: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = rustc_hash::FxHasher::default();
        addr.hash(&mut h);
        h.finish() | 1
    }

    /// Open a span on `st`, claiming an oldest-tracking slot for `addr`.
    fn span_start(&self, st: &OpState, addr: &str, now_ms: u64) {
        st.open.fetch_add(1, Ordering::Relaxed);
        self.touch(now_ms);
        let key = Self::key_of(addr);
        for (slot, start) in st.slot_key.iter().zip(st.slot_start.iter()) {
            if slot
                .compare_exchange(FREE, key, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                start.store(now_ms, Ordering::Release);
                return;
            }
        }
        // All slots busy: the count is still exact, only the age is unknown.
    }

    /// Close a span on `st`, releasing `addr`'s slot.
    fn span_end(&self, st: &OpState, addr: &str, now_ms: u64) {
        st.open.fetch_sub(1, Ordering::Relaxed);
        self.touch(now_ms);
        let key = Self::key_of(addr);
        for slot in st.slot_key.iter() {
            if slot.load(Ordering::Acquire) == key
                && slot
                    .compare_exchange(key, FREE, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                return;
            }
        }
    }

    fn op_start(&self, op: Op, addr: &str, now_ms: u64) {
        let Some(st) = self.op(op) else { return };
        if op.tracks_oldest() {
            self.span_start(st, addr, now_ms);
        } else {
            // Unbounded op: count only. A slot array would silently evict the
            // span whose age we care about.
            st.open.fetch_add(1, Ordering::Relaxed);
            self.touch(now_ms);
        }
    }

    fn op_end(&self, op: Op, addr: &str, now_ms: u64) {
        let Some(st) = self.op(op) else { return };
        if op.tracks_oldest() {
            self.span_end(st, addr, now_ms);
        } else {
            st.open.fetch_sub(1, Ordering::Relaxed);
            self.touch(now_ms);
        }
    }

    /// Record bytes moved. Callers accumulate locally and flush periodically —
    /// incrementing per 8 KiB chunk would put ~100k atomic RMWs on one cache line
    /// during a cold multi-GB pull, contended across every transfer.
    pub fn add_bytes(&self, op: Op, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let Some(st) = self.op(op) else { return };
        st.bytes_total.fetch_add(bytes, Ordering::Relaxed);
        // Bytes moving *is* progress, even with no span transition — this is what
        // stops a long, healthy transfer from reading as a stall.
        self.touch(self.now_ms());
    }

    pub fn open_count(&self, op: Op) -> u64 {
        self.op(op)
            .map(|st| u64::try_from(st.open.load(Ordering::Relaxed).max(0)).unwrap_or(0))
            .unwrap_or(0)
    }

    /// Age of the oldest open span of `op`, when trackable.
    pub fn oldest_open(&self, op: Op, now_ms: u64) -> Option<Duration> {
        if !op.tracks_oldest() {
            return None;
        }
        oldest_of(self.op(op)?, now_ms)
    }

    /// Bytes moved by `op` within the rolling window, and the cumulative total.
    pub fn bytes(&self, op: Op, now_ms: u64) -> (u64, u64) {
        let Some(st) = self.op(op) else { return (0, 0) };
        let total = st.bytes_total.load(Ordering::Relaxed);
        let opened = st.window_opened_ms.load(Ordering::Relaxed);
        let window_ms = u64::try_from(BYTES_WINDOW.as_millis()).unwrap_or(60_000);
        if now_ms.saturating_sub(opened) > window_ms {
            // Roll the window forward. Racy under concurrent readers, and that is
            // fine: the worst outcome is one window reported short.
            st.window_opened_ms.store(now_ms, Ordering::Relaxed);
            st.bytes_window_base.store(total, Ordering::Relaxed);
            return (0, total);
        }
        let base = st.bytes_window_base.load(Ordering::Relaxed);
        (total.saturating_sub(base), total)
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Look a limiter up by name, falling back to an inert one so a caller that
    /// names a limiter this build does not track degrades to "not reported"
    /// rather than panicking inside a diagnostic.
    pub fn limiter(&self, name: &'static str) -> &Limiter {
        static INERT: Limiter = Limiter::new("inert");
        self.limiters
            .iter()
            .find(|l| l.name == name)
            .unwrap_or(&INERT)
    }

    pub fn limiters(&self) -> &[Limiter] {
        &self.limiters
    }

    /// Saturated limiters, longest-saturated first.
    pub fn saturated(&self, now_ms: u64) -> Vec<(&'static str, Duration)> {
        let mut v: Vec<_> = self
            .limiters
            .iter()
            .filter_map(|l| {
                // Sample now, not at the last acquire. A stall report is rendered
                // precisely when nothing is acquiring, so an acquire-only reading
                // is frozen at whatever it was when work stopped.
                l.refresh(now_ms);
                l.saturated_for(now_ms).map(|d| (l.name, d))
            })
            .collect();
        v.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
        v
    }

    /// Decide whether this looks stalled, as of `now_ms`.
    ///
    /// Pure: it holds no clock and does no I/O, so every scenario is testable by
    /// passing a time rather than sleeping. The watchdog thread is then a two-line
    /// loop with one wiring test instead of a suite of flaky sleepy ones.
    pub fn evaluate(&self, now_ms: u64, threshold: Duration) -> Option<StallReport> {
        let threshold_ms = u64::try_from(threshold.as_millis()).unwrap_or(60_000);
        let quiet = self.quiet_for_ms(now_ms);
        if quiet < threshold_ms {
            return None;
        }

        let mut open: Vec<(Op, u64, Option<Duration>)> = Op::ALL
            .iter()
            .map(|&op| (op, self.open_count(op), self.oldest_open(op, now_ms)))
            .filter(|(_, n, _)| *n > 0)
            .collect();
        open.sort_by_key(|(_, n, _)| std::cmp::Reverse(*n));

        // A handful of `Execute` spans and nothing else is a normal build running
        // normal subprocesses. heph cannot see inside one: a compiler thinking
        // quietly for ten minutes emits exactly what a wedged one emits, which is
        // nothing. Reporting that at the ordinary threshold would fire on every
        // narrow invocation — `heph r //some:slow_target` — and a notice that
        // cries wolf is worse than none, because people stop reading it.
        //
        // So hold silent subprocesses to a much longer clock. A genuinely stuck
        // one is still reported, just late, and `dominant` refuses to volunteer a
        // theory about it (see `StallReport::dominant`).
        let only_subprocesses = !open.is_empty()
            && open
                .iter()
                .all(|(op, n, _)| matches!(op, Op::Execute) && *n <= QUIET_EXEC_MAX);
        if only_subprocesses && quiet < threshold_ms.saturating_mul(QUIET_EXEC_FACTOR) {
            return None;
        }

        // Nothing open and nothing moving: either the run is over (the watchdog is
        // stopped then) or we are wedged *before* any span opened — matching, or
        // walking packages on a 27k-target repo. That phase is real and must not
        // be silently uninstrumented, so report it rather than staying quiet.
        Some(StallReport {
            quiet_for: Duration::from_millis(self.quiet_for_ms(now_ms)),
            open,
            bytes: Op::ALL
                .iter()
                .map(|&op| (op, self.bytes(op, now_ms).0))
                .collect(),
            saturated: self.saturated(now_ms),
            done: self.done(),
            failed: self.failed(),
            lock_waits: self.lock_waits(now_ms),
            workers: self.worker_permits(),
            delta: None,
            stuck: Vec::new(),
        })
    }
}

/// The process-wide table.
///
/// A `static` rather than a field threaded through the engine: there is exactly
/// one per process, the hook needs it before the engine is wrapped in an `Arc`,
/// and both the watchdog thread and any future reader live outside the engine's
/// ownership graph. Same shape as the other diagnostic modules.
pub fn global() -> &'static std::sync::Arc<DiagState> {
    static STATE: std::sync::OnceLock<std::sync::Arc<DiagState>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| {
        std::sync::Arc::new(DiagState::new(vec![
            Limiter::new("workers"),
            Limiter::new("remote-cache-metadata"),
            Limiter::new("remote-cache-transfer"),
            Limiter::new("codec"),
        ]))
    })
}

/// What the watchdog found. Rendered by the caller; holds no formatting itself.
#[derive(Debug)]
pub struct StallReport {
    pub quiet_for: Duration,
    /// `(op, open count, oldest age)`, most-open first.
    pub open: Vec<(Op, u64, Option<Duration>)>,
    pub bytes: Vec<(Op, u64)>,
    pub saturated: Vec<(&'static str, Duration)>,
    pub done: u64,
    pub failed: u64,
    /// `(open waits, oldest age, holder pid)` when the result lock is blocking.
    pub lock_waits: Option<(u64, Option<Duration>, Option<u32>)>,
    /// Change since the previous fire; `None` on the first.
    pub delta: Option<StallDelta>,
    /// Worker-permit accounting, when the pool has been registered.
    pub workers: Option<WorkerPermits>,
    /// Incomplete memoizer cells. Filled in by [`Watchdog`] after `evaluate`,
    /// which stays pure — see its docs.
    pub stuck: Vec<hcore::hmemoizer::StuckCell>,
}

impl StallReport {
    /// The dominant op, when one clearly dominates.
    ///
    /// Gated at 80% because a wrong automated hypothesis costs more credibility
    /// than a missing one. Below that the caller prints the table and offers no
    /// theory.
    pub fn dominant(&self) -> Option<(Op, u64)> {
        let total: u64 = self.open.iter().map(|(_, n, _)| *n).sum();
        let (op, n, _) = self.open.first()?;
        // Never volunteer a theory about a subprocess: heph has no view inside
        // one, so "stuck execute" would be a guess dressed as a finding.
        if matches!(op, Op::Execute) {
            return None;
        }
        (total > 0 && n * 100 >= total * 80).then_some((*op, *n))
    }

    /// Whether the dominant op has moved no bytes in the window — the signal that
    /// separates "stalled" from "slow". Counts and ages alone cannot: a slow link
    /// and a wedged socket look identical by both.
    pub fn dominant_is_starved(&self) -> bool {
        let Some((op, _)) = self.dominant() else {
            return false;
        };
        // An op with no byte counter is not "starved", it is unmeasured. See
        // [`Op::reports_bytes`].
        if !op.reports_bytes() {
            return false;
        }
        self.bytes
            .iter()
            .find(|(o, _)| *o == op)
            .is_some_and(|(_, b)| *b == 0)
    }

    /// Nothing is actually *doing* anything: no subprocess, no cache transfer,
    /// no cache write, no blocked lock.
    ///
    /// [`Op::Result`] is excluded deliberately — a result span is bookkeeping for
    /// "this addr is being resolved", and every ancestor holds one open while
    /// awaiting its children. Thousands of them open says nothing about whether
    /// work is happening. Every *other* op is a real operation in flight, so
    /// "only result spans are open" means the process has nothing to do.
    pub fn no_work_in_flight(&self) -> bool {
        self.lock_waits.is_none()
            && self
                .open
                .iter()
                .all(|(op, n, _)| matches!(op, Op::Result) || *n == 0)
    }
}

/// How the run changed between two consecutive fires of the watchdog.
///
/// Two stall paragraphs that are byte-identical are the strongest signal the
/// log can carry — "wedged", not "slow" — and until this existed the only way
/// to see it was to diff them by eye. Filled in by [`Watchdog`], which is the
/// only thing that knows what the previous fire looked like.
#[derive(Debug, Clone, Copy)]
pub struct StallDelta {
    pub since: Duration,
    pub done: i64,
    pub open: i64,
}

impl StallDelta {
    /// Nothing at all moved since the previous report.
    pub fn is_flat(&self) -> bool {
        self.done == 0 && self.open == 0
    }
}

/// Folds the engine's event stream into [`DiagState`]. Atomics only — see the
/// module docs for why.
pub struct DiagHook {
    state: std::sync::Arc<DiagState>,
}

impl DiagHook {
    pub fn new(state: std::sync::Arc<DiagState>) -> Self {
        Self { state }
    }
}

impl hplugin::hook::Hook for DiagHook {
    fn name(&self) -> String {
        "diag".to_string()
    }

    fn on_event(&self, ev: &BuildEvent) {
        let s = &self.state;
        let now = s.now_ms();
        match &ev.kind {
            BuildEventKind::ResultStart { addr } => s.op_start(Op::Result, addr, now),
            BuildEventKind::ResultEnd { addr, error } => {
                s.op_end(Op::Result, addr, now);
                s.done.fetch_add(1, Ordering::Relaxed);
                if error.is_some() {
                    s.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            BuildEventKind::ExecuteStart { addr, .. } => s.op_start(Op::Execute, addr, now),
            BuildEventKind::ExecuteEnd { addr, .. } => s.op_end(Op::Execute, addr, now),
            BuildEventKind::RemoteCacheReadStart { addr } => {
                s.op_start(Op::RemoteCacheRead, addr, now);
            }
            BuildEventKind::RemoteCacheReadEnd { addr, .. } => {
                s.op_end(Op::RemoteCacheRead, addr, now);
            }
            // The cache-write spans. Unwired until now, which cut both ways: a
            // build wedged in a cache write reported "0 open" for it, and — worse
            // — a build *progressing* through the write-heavy tail of a cached
            // run never touched the quiet clock and so read as stalled.
            BuildEventKind::LocalCacheWriteStart { addr } => {
                s.op_start(Op::LocalCacheWrite, addr, now);
            }
            BuildEventKind::LocalCacheWriteEnd { addr, .. } => {
                s.op_end(Op::LocalCacheWrite, addr, now);
            }
            BuildEventKind::RemoteCacheWriteStart { addr } => {
                s.op_start(Op::RemoteCacheWrite, addr, now);
            }
            BuildEventKind::RemoteCacheWriteEnd { addr, .. } => {
                s.op_end(Op::RemoteCacheWrite, addr, now);
            }
            BuildEventKind::ResultLockWaitStart { addr, holder_pid } => {
                s.lock_wait_start(addr, *holder_pid, now);
            }
            BuildEventKind::ResultLockWaitEnd { addr } => s.lock_wait_end(addr, now),
            // Cache hit/miss carry no span but are unambiguous progress.
            BuildEventKind::LocalCacheHit { .. }
            | BuildEventKind::LocalCacheMiss { .. }
            | BuildEventKind::RemoteCacheHit { .. }
            | BuildEventKind::RemoteCacheMiss { .. }
            | BuildEventKind::Matched { .. } => s.touch(now),
            _ => {}
        }
    }

    fn on_close(&self) {}
}

/// Cells listed individually in a stall paragraph. The paragraph repeats on
/// every escalation, so the full inventory (thousands of cells on a big graph)
/// would bury the run's own output; the `SIGQUIT` dump prints all of them.
const INVENTORY_LINES: usize = 20;

fn secs(d: Duration) -> String {
    format!("{}s", d.as_secs())
}

/// Render a delta with an explicit sign, so `+0` reads as "measured, no change"
/// rather than as a missing value.
fn signed(n: i64) -> String {
    format!("{n:+}")
}

fn human_bytes(b: u64) -> String {
    const U: [(u64, &str); 4] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB"), (1, "B")];
    for (unit, label) in U {
        if b >= unit {
            return format!("{:.1} {label}", b as f64 / unit as f64);
        }
    }
    "0 B".to_string()
}

/// Render a stall report as the paragraph appended to the [`StallLog`].
///
/// The wording deliberately avoids the vocabulary of a *failure* — no "error",
/// no "failed". The file is read (and often catted into a CI log) on every slow
/// build, and a log processor grepping for failure signatures must not match a
/// progress notice. It says "no progress", never "hung": the run may well be
/// fine.
///
/// The text is a diagnostic, not an interface. Nothing should parse it — that is
/// what the machine-readable surface is for.
pub fn render_stall(r: &StallReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("\nheph: no progress for {}\n", secs(r.quiet_for)));

    if r.open.is_empty() {
        // Wedged before any span opened — matching, or walking packages. A real
        // phase on a large repo, and one that would otherwise be uninstrumented
        // and silent.
        out.push_str("  open ops     none — still resolving targets\n");
    } else {
        let parts: Vec<String> = r
            .open
            .iter()
            .map(|(op, n, age)| match age {
                Some(a) => format!("{n} {} (oldest {})", op.label(), secs(*a)),
                // Unbounded op: an age from a fixed slot array would be a guess.
                None => format!("{n} {}", op.label()),
            })
            .collect();
        out.push_str(&format!("  open ops     {}\n", parts.join(", ")));
    }

    for (op, b) in &r.bytes {
        // Only ops that actually move bytes through a counter. Printing a hard
        // zero for the rest reads as evidence of starvation when it is evidence
        // of nothing — see [`Op::reports_bytes`].
        if op.reports_bytes() && r.open.iter().any(|(o, _, _)| o == op) {
            out.push_str(&format!(
                "  bytes        {} in the last {}s on {}\n",
                human_bytes(*b),
                BYTES_WINDOW.as_secs(),
                op.label()
            ));
        }
    }

    // Cross-process contention: the holder may be another heph on this machine,
    // which nothing else in this report could ever surface.
    // The arithmetic, not just the age: "saturated" says the pool is full, and
    // this says whether anything is actually using it.
    if let Some(w) = r.workers {
        out.push_str(&format!(
            "  workers      {} max, {} free, {} running",
            w.capacity, w.free, w.running
        ));
        if w.unaccounted() > 0 {
            out.push_str(&format!(", {} unaccounted", w.unaccounted()));
        }
        out.push('\n');
    }

    if let Some((n, oldest, pid)) = r.lock_waits {
        let age = match oldest {
            Some(a) => format!(", oldest {}", secs(a)),
            None => String::new(),
        };
        let holder = match pid {
            Some(p) => format!(", holder pid {p}"),
            None => String::new(),
        };
        out.push_str(&format!(
            "  lock waits   {n} on the result lock{age}{holder}\n"
        ));
    }

    if !r.saturated.is_empty() {
        let parts: Vec<String> = r
            .saturated
            .iter()
            .map(|(n, d)| format!("{n} saturated for {}", secs(*d)))
            .collect();
        out.push_str(&format!("  limits       {}\n", parts.join(", ")));
    }

    // "unsuccessful", not "failed": this line lands in the stderr of every slow
    // CI build, and a log scanner grepping for failure words must not match a
    // progress notice that is merely reporting a zero.
    out.push_str(&format!(
        "  progress     {} done, {} unsuccessful\n",
        r.done, r.failed
    ));

    // The escalation *is* the diagnostic: identical consecutive paragraphs mean
    // wedged where either alone means only slow. Stating the delta saves the
    // reader diffing two tables by eye — and is the difference between a build
    // that is crawling and one that has stopped.
    if let Some(d) = r.delta {
        out.push_str(&format!(
            "  since last   {} done, {} open, over {}\n",
            signed(d.done),
            signed(d.open),
            secs(d.since)
        ));
    }

    // What a thread dump structurally cannot show: the parked futures. On a
    // wedged build every thread is idle and this is the only place the stuck
    // work is visible at all.
    out.push_str(&hcore::hmemoizer::render_inventory(
        &r.stuck,
        INVENTORY_LINES,
    ));

    // Only volunteer a cause when one op clearly dominates *and* something
    // corroborates it. A wrong automated hypothesis costs more credibility than
    // no hypothesis — better to show the table and let the reader conclude.
    let stranded = r.stuck.iter().filter(|c| c.is_stranded()).count();
    if r.delta.is_some_and(|d| d.is_flat()) && !r.stuck.is_empty() && r.no_work_in_flight() {
        // Three independent observations: nothing changed between two fires, no
        // operation of any kind is in flight, and cells are still incomplete. A
        // build with work to do would have *something* open.
        //
        // Keyed on "no work in flight" rather than on the driver bit, because a
        // parked driver still occupies the driver slot: a cell whose awaiter was
        // woken and never re-polled reads `driver=true` forever. Keying on that
        // alone reported nothing on a build wedged with 578 result cells and all
        // drivers populated.
        out.push_str(&format!(
            "\n  Nothing is executing, transferring, or waiting on a lock, {} cell(s) are\n  \
             still incomplete, and nothing moved since the last report. The process is\n  \
             idle with work outstanding — a lost wake-up or a dependency cycle, not\n  \
             slow work.\n",
            r.stuck.len()
        ));
        if stranded > 0 {
            // Sharper still when present: these have waiters and no driver at
            // all, so not even a re-poll is pending.
            out.push_str(&format!(
                "  {stranded} of them have waiters with no driver elected.\n"
            ));
        }
        // The full path, not a bare filename: this is read later, often by
        // someone who did not start the build and has no idea what its cwd was.
        let inflight = INFLIGHT_PATH.get().map_or_else(
            || format!("inflight-{}.log beside this file", std::process::id()),
            |p| p.display().to_string(),
        );
        out.push_str(&format!(
            "  The full list, the wait-for graph and each invocation's next await are in\n  \
             {inflight}, refreshed on every report. Re-run with\n  \
             `HEPH_DEBUG_MEMOIZER_CYCLE=1 HEPH_PHASE_TRACE=1` to populate the last two,\n  \
             and to fail a real cycle instead of hanging on it.\n"
        ));
    } else if let Some((op, _)) = r.dominant()
        && r.dominant_is_starved()
    {
        out.push_str(&format!(
            "\n  Nothing has been read on {} in the last {}s, so this looks like a\n  \
             stuck {} rather than slow work.\n",
            op.label(),
            BYTES_WINDOW.as_secs(),
            op.label()
        ));
    }

    // Ctrl-C is what a human reaches for on a frozen build, and on a wedged run
    // it is exactly what cannot help — the TUI clears ISIG, so the terminal
    // never raises SIGINT. Name the escalation that does work, here, where it is
    // read, rather than leaving it to be rediscovered per incident.
    out.push_str(&format!(
        "\n  Still stuck? `kill -QUIT {}` writes every thread's backtrace plus the\n  \
         full in-flight inventory next to this file; it does not kill the process.\n",
        std::process::id()
    ));

    out.push_str("  (diagnostic text, not a stable interface)\n");
    out
}

/// Where stall paragraphs are appended.
///
/// A file rather than the terminal, because the two readers want opposite
/// things. The paragraph is six lines of table that repeats as the stall
/// escalates; inlined into a TUI frame or a CI log it buries the build output it
/// is meant to annotate, and in CI it lands in the middle of whatever the
/// compiler was printing. A file keeps the full history, keeps it in one place
/// across every fire of the run, and leaves the terminal with a single line
/// naming the path.
///
/// It sits next to the `SIGQUIT` thread dumps (`<home>/diag/`) — the two are read
/// together when diagnosing the same hang, and `heph tool gc` already sweeps that
/// directory. Per-pid, so concurrent heph processes in one workspace do not
/// interleave their reports into one unreadable file.
/// `<home>/diag/<name>`, made absolute.
///
/// Absolute because these paths are *reported* — the stall paragraph names the
/// in-flight file, a `warn!` names the stall log, and both are read later, often
/// by someone who was not the one who started the build and has no idea what its
/// cwd was. `home` is normally absolute already; this makes it so when a caller
/// passes a relative root (tests, `--home` with a relative path) rather than
/// emitting a path that resolves differently depending on where you stand.
///
/// `std::path::absolute` rather than `canonicalize`: the directory does not exist
/// until the first write, and `canonicalize` fails on a path that is not already
/// there.
fn diag_path(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = home.join("diag").join(name);
    std::path::absolute(&path).unwrap_or(path)
}

/// The full in-flight state, rewritten on every stall fire.
///
/// Separate from [`StallLog`] because the two want opposite things. The stall
/// log is a short, readable paragraph that *appends*, so the escalation history
/// survives; this is the complete uncapped dump — every incomplete cell, the
/// wait-for graph, every invocation's next-await label — which is thousands of
/// lines on a large graph and would bury that history if appended fourteen
/// times.
///
/// Truncated on each write, so it always holds the *current* state rather than a
/// stack of stale ones. The stall log's own history says how the run got here.
///
/// Written by the watchdog, without anyone having to catch the process alive:
/// the first incident this machinery was built for ended with the process dying
/// before a `SIGQUIT` could be sent, and every byte of in-flight state went with
/// it. `SIGQUIT` still produces the same report on demand — this one just does
/// not require someone to be watching.
pub struct InflightLog {
    path: std::path::PathBuf,
}

/// Where the in-flight report is being written, for the stall paragraph to name.
///
/// A static rather than a field threaded through `StallReport`: `render_stall`
/// is called from the watchdog thread, which builds the report from
/// [`DiagState`] alone and has no handle on the log. Same shape as
/// [`global`].
static INFLIGHT_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

impl InflightLog {
    pub fn new(home: &std::path::Path) -> Self {
        let path = diag_path(home, &format!("inflight-{}.log", std::process::id()));
        drop(INFLIGHT_PATH.set(path.clone()));
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Replace the file with the current in-flight report.
    pub fn write(&self, text: &str) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, text)
    }
}

pub struct StallLog {
    path: std::path::PathBuf,
}

impl StallLog {
    pub fn new(home: &std::path::Path) -> Self {
        Self {
            path: diag_path(home, &format!("stall-{}.log", std::process::id())),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Append one rendered report, creating the directory on first write.
    ///
    /// Appends rather than truncates: the escalation sequence *is* the
    /// diagnostic — "98 open, oldest 60s" followed by "98 open, oldest 512s" says
    /// wedged, where either line alone says only slow.
    pub fn append(&self, text: &str) -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(text.as_bytes())
    }
}

/// Poll [`DiagState::evaluate`] and emit a report when it detects a stall.
///
/// Runs on a plain OS thread, never a tokio timer: the whole point is to keep
/// working when the runtime is saturated or the reactor is not turning, which is
/// the condition being diagnosed. It is deliberately *not* merged into
/// `hcore::blocking`'s backstop thread — that one is a correctness mechanism for
/// dropped wake-ups, and this one ends in blocking I/O. In CI stderr is a pipe;
/// a stalled consumer would fill the 64 KiB buffer and block forever, freezing
/// waker delivery for the entire blocking pool. The watchdog would then hang the
/// process it exists to diagnose.
///
/// `emit` takes the report, not the rendered text: the caller decides what goes
/// to the file and what goes to the terminal, and rendering inside the watchdog
/// would fix that policy here.
pub struct Watchdog {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Watchdog {
    pub fn spawn(
        state: std::sync::Arc<DiagState>,
        threshold: Duration,
        emit: impl Fn(&StallReport) + Send + 'static,
    ) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        // A 30-60s threshold wants a 1s tick, not the 250ms other tickers use;
        // copying that cadence out of symmetry would be 4x the wakeups for no
        // extra resolution.
        let tick = Duration::from_secs(1);
        std::thread::Builder::new()
            .name("heph-stall-watchdog".to_string())
            .spawn(move || {
                let mut last_fired: Option<u64> = None;
                let mut last_seen: Option<(u64, i64, u64)> = None;
                while !stop_thread.load(Ordering::Relaxed) {
                    std::thread::sleep(tick);
                    let now = state.now_ms();
                    let Some(mut report) = state.evaluate(now, threshold) else {
                        last_fired = None;
                        last_seen = None;
                        continue;
                    };
                    // Escalate rather than repeat every tick: a stall that lasts
                    // an hour must not produce 3600 paragraphs.
                    let due = last_fired.is_none_or(|f| {
                        now.saturating_sub(f)
                            >= u64::try_from(threshold.as_millis()).unwrap_or(60_000) * 2
                    });
                    if due {
                        let open: i64 = report
                            .open
                            .iter()
                            .map(|(_, n, _)| i64::try_from(*n).unwrap_or(i64::MAX))
                            .sum();
                        let done = report.done;
                        if let Some((prev_done, prev_open, at)) = last_seen {
                            report.delta = Some(StallDelta {
                                since: Duration::from_millis(now.saturating_sub(at)),
                                done: i64::try_from(done.saturating_sub(prev_done))
                                    .unwrap_or(i64::MAX),
                                open: open - prev_open,
                            });
                        }
                        // Collected here rather than in `evaluate` so that stays
                        // pure and testable by passing a time: this walks live
                        // maps and formats keys, which is affordable once a stall
                        // is already confirmed and not on every tick.
                        report.stuck = hcore::hmemoizer::inventory();
                        emit(&report);
                        last_fired = Some(now);
                        last_seen = Some((done, open, now));
                    }
                }
            })
            // Same stance as the other diagnostic threads: a process that cannot
            // spawn this has nothing to fall back to.
            .expect("spawn heph stall-watchdog thread");
        Self { stop }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::events::BuildEvent;
    use hplugin::hook::Hook as _;

    const T: Duration = Duration::from_secs(60);

    fn state() -> std::sync::Arc<DiagState> {
        std::sync::Arc::new(DiagState::new(vec![Limiter::new("workers")]))
    }

    fn ev(kind: BuildEventKind) -> BuildEvent {
        BuildEvent {
            kind,
            at_unix_ms: 0,
        }
    }

    /// A lone quiet subprocess is not a stall at the ordinary threshold.
    ///
    /// `heph r //some:slow_target` emits one `ExecuteStart` and then nothing for
    /// as long as the compiler thinks. heph cannot see inside it — a healthy
    /// subprocess and a wedged one emit identically, which is to say nothing — so
    /// firing here would put a stall notice on every narrow invocation, and a
    /// notice that cries wolf stops being read.
    #[test]
    fn does_not_fire_on_one_quiet_slow_target() {
        let s = state();
        s.op_start(Op::Execute, "//a:b", 0);
        for minutes in 1..10 {
            let now = minutes * 60_000;
            assert!(
                s.evaluate(now, T).is_none(),
                "fired at {now}ms on a single quiet subprocess"
            );
        }
    }

    /// It is held to a longer clock, not exempted: a genuinely stuck subprocess
    /// is still reported, just late.
    #[test]
    fn a_quiet_subprocess_is_reported_eventually() {
        let s = state();
        s.op_start(Op::Execute, "//a:b", 0);
        let r = s
            .evaluate(T.as_millis() as u64 * QUIET_EXEC_FACTOR + 1, T)
            .expect("reported once the longer clock elapses");
        assert_eq!(
            r.open.first().map(|(op, n, _)| (*op, *n)),
            Some((Op::Execute, 1))
        );
        assert_eq!(
            r.dominant(),
            None,
            "heph cannot see inside a subprocess, so it must not blame one"
        );
    }

    /// The cache-write spans reach the table at all.
    ///
    /// They are emitted by the engine and consumed by the TUI, but the diag hook
    /// dropped them on the floor, so a build wedged in a cache write reported
    /// "0 open" for the subsystem it was stuck in.
    #[test]
    fn cache_write_spans_are_counted() {
        let s = state();
        let hook = DiagHook::new(std::sync::Arc::clone(&s));
        hook.on_event(&ev(BuildEventKind::LocalCacheWriteStart {
            addr: "//a:b".into(),
        }));
        hook.on_event(&ev(BuildEventKind::RemoteCacheWriteStart {
            addr: "//c:d".into(),
        }));
        assert_eq!(s.open_count(Op::LocalCacheWrite), 1);
        assert_eq!(s.open_count(Op::RemoteCacheWrite), 1);

        hook.on_event(&ev(BuildEventKind::LocalCacheWriteEnd {
            addr: "//a:b".into(),
            error: None,
        }));
        assert_eq!(s.open_count(Op::LocalCacheWrite), 0);
    }

    /// The sharper half of the same bug: because the write spans never touched
    /// the quiet clock, a run *progressing* through the write-heavy tail of a
    /// mostly-cached build looked silent and got reported as stalled. A stall
    /// notice that fires on a healthy build stops being read.
    #[test]
    fn a_build_progressing_through_cache_writes_is_not_a_stall() {
        let s = state();
        let hook = DiagHook::new(std::sync::Arc::clone(&s));
        s.op_start(Op::Result, "//root:all", 0);

        for i in 0..20 {
            // Only cache-write activity, spaced under the threshold.
            std::thread::sleep(std::time::Duration::from_millis(1));
            hook.on_event(&ev(BuildEventKind::LocalCacheWriteStart {
                addr: format!("//t:{i}"),
            }));
            hook.on_event(&ev(BuildEventKind::LocalCacheWriteEnd {
                addr: format!("//t:{i}"),
                error: None,
            }));
            assert!(
                s.evaluate(s.now_ms(), T).is_none(),
                "cache writes are progress; iteration {i} read as a stall"
            );
        }
    }

    /// A blocked result lock is named, with the holder — which for the default
    /// filesystem backend can be a different heph process entirely, something no
    /// amount of introspection into this one would ever find.
    #[test]
    fn a_blocked_result_lock_is_reported_with_its_holder() {
        let s = state();
        let hook = DiagHook::new(std::sync::Arc::clone(&s));
        hook.on_event(&ev(BuildEventKind::ResultLockWaitStart {
            addr: "//a:b".into(),
            holder_pid: Some(4242),
        }));
        s.op_start(Op::Result, "//a:b", 0);

        let r = s.evaluate(61_000, T).expect("stalled");
        let (n, _, pid) = r.lock_waits.expect("the wait must be reported");
        assert_eq!(n, 1);
        assert_eq!(pid, Some(4242));
        let text = render_stall(&r);
        assert!(text.contains("lock waits   1"), "{text}");
        assert!(text.contains("holder pid 4242"), "{text}");

        hook.on_event(&ev(BuildEventKind::ResultLockWaitEnd {
            addr: "//a:b".into(),
        }));
        assert!(
            s.evaluate(200_000, T)
                .expect("still stalled")
                .lock_waits
                .is_none(),
            "an acquired (or cancelled) wait must stop being reported"
        );
    }

    /// `add_bytes` is only ever called for remote-cache reads, so for every other
    /// op the window is a structural zero. Printing it reads as evidence of
    /// starvation when it is evidence of nothing.
    #[test]
    fn no_byte_line_or_starvation_claim_for_ops_that_never_report_bytes() {
        let s = state();
        for i in 0..98 {
            s.op_start(Op::Result, &format!("//pkg:{i}"), 0);
        }
        let r = s.evaluate(61_000, T).expect("stalled");
        assert_eq!(
            r.dominant().map(|(op, _)| op),
            Some(Op::Result),
            "result dominates this report"
        );
        assert!(
            !r.dominant_is_starved(),
            "an op with no byte counter is unmeasured, not starved"
        );

        let text = render_stall(&r);
        assert!(
            !text.contains("bytes"),
            "no byte line for an op that cannot move the counter: {text}"
        );
        assert!(
            !text.contains("Nothing has been read"),
            "no hypothesis derived from a counter that cannot move: {text}"
        );
    }

    /// The escalation is the diagnostic: two identical paragraphs mean wedged,
    /// where either alone means only slow. Say it rather than making the reader
    /// diff two tables by eye.
    #[test]
    fn the_delta_since_the_last_report_is_stated() {
        let s = state();
        s.op_start(Op::Result, "//a:b", 0);
        let mut r = s.evaluate(61_000, T).expect("stalled");
        r.delta = Some(StallDelta {
            since: Duration::from_secs(120),
            done: 0,
            open: 0,
        });
        let text = render_stall(&r);
        assert!(
            text.contains("since last   +0 done, +0 open, over 120s"),
            "{text}"
        );
        assert!(r.delta.expect("delta").is_flat());
    }

    /// Waiters parked on a cell with nobody elected to poll it, plus a flat
    /// delta, is a lost wake-up — and unlike the byte-starvation claim it is
    /// backed by two independent observations rather than one structural zero.
    #[test]
    fn a_stranded_cell_with_a_flat_delta_is_called_a_lost_wakeup() {
        let s = state();
        s.op_start(Op::Result, "//a:b", 0);
        let mut r = s.evaluate(61_000, T).expect("stalled");
        r.delta = Some(StallDelta {
            since: Duration::from_secs(120),
            done: 0,
            open: 0,
        });
        r.stuck = vec![hcore::hmemoizer::StuckCell {
            tag: "result",
            key: "//a:b".to_string(),
            waiters: Some(4),
            has_driver: false,
        }];
        let text = render_stall(&r);
        assert!(text.contains("The process is"), "{text}");
        assert!(text.contains("idle with work outstanding"), "{text}");
        assert!(
            text.contains("1 of them have waiters with no driver elected"),
            "the sharper sub-case is called out when present: {text}"
        );
        assert!(text.contains("[result] //a:b waiters=4"), "{text}");
    }

    /// The shape the driver-keyed check missed.
    ///
    /// A real wedge had 578 result cells, 455 meta, 123 locked_result — and
    /// `driver=true` on every one of them, because an awaiter that is woken and
    /// never re-polled keeps occupying the driver slot forever. Keying the
    /// headline on "no driver" reported nothing at all on 25 minutes of a
    /// completely idle process.
    #[test]
    fn a_wedge_is_reported_even_when_every_cell_still_has_a_driver() {
        let s = state();
        for i in 0..578 {
            s.op_start(Op::Result, &format!("//pkg:{i}"), 0);
        }
        let mut r = s.evaluate(61_000, T).expect("stalled");
        r.delta = Some(StallDelta {
            since: Duration::from_secs(120),
            done: 0,
            open: 0,
        });
        r.stuck = (0..3)
            .map(|i| hcore::hmemoizer::StuckCell {
                tag: "locked_result",
                key: format!("@heph/fs:file {i}"),
                waiters: Some(1),
                has_driver: true,
            })
            .collect();

        assert_eq!(
            r.stuck.iter().filter(|c| c.is_stranded()).count(),
            0,
            "none are stranded by the narrow definition — that is the point"
        );
        assert!(r.no_work_in_flight(), "only result spans are open");

        let text = render_stall(&r);
        assert!(text.contains("idle with work outstanding"), "{text}");
        assert!(
            !text.contains("have waiters with no driver elected"),
            "the sub-case line must not appear when nothing is stranded: {text}"
        );
        assert!(
            text.contains("HEPH_DEBUG_MEMOIZER_CYCLE=1"),
            "the paragraph must name the two vars that resolve it: {text}"
        );
    }

    /// The claim is "nothing is happening", so any real operation in flight
    /// must retract it — otherwise it fires on a build that is merely slow.
    #[test]
    fn work_in_flight_retracts_the_idle_claim() {
        let s = state();
        s.op_start(Op::Result, "//a:b", 0);
        s.op_start(Op::Execute, "//a:b", 0);
        let mut r = s
            .evaluate(T.as_millis() as u64 * QUIET_EXEC_FACTOR + 1, T)
            .expect("stalled");
        r.delta = Some(StallDelta {
            since: Duration::from_secs(120),
            done: 0,
            open: 0,
        });
        r.stuck = vec![hcore::hmemoizer::StuckCell {
            tag: "result",
            key: "//a:b".to_string(),
            waiters: Some(1),
            has_driver: true,
        }];
        assert!(!r.no_work_in_flight(), "an execute span is real work");
        let text = render_stall(&r);
        assert!(!text.contains("idle with work outstanding"), "{text}");
    }

    /// A blocked lock is work in flight too — the process is waiting on
    /// something real, quite possibly another process, and calling that "idle"
    /// would point the reader at the wrong subsystem entirely.
    #[test]
    fn a_blocked_lock_retracts_the_idle_claim() {
        let s = state();
        let hook = DiagHook::new(std::sync::Arc::clone(&s));
        s.op_start(Op::Result, "//a:b", 0);
        hook.on_event(&ev(BuildEventKind::ResultLockWaitStart {
            addr: "//a:b".into(),
            holder_pid: Some(99),
        }));
        let mut r = s.evaluate(61_000, T).expect("stalled");
        r.delta = Some(StallDelta {
            since: Duration::from_secs(120),
            done: 0,
            open: 0,
        });
        r.stuck = vec![hcore::hmemoizer::StuckCell {
            tag: "locked_result",
            key: "//a:b".to_string(),
            waiters: Some(1),
            has_driver: true,
        }];
        assert!(!r.no_work_in_flight());
        let text = render_stall(&r);
        assert!(!text.contains("idle with work outstanding"), "{text}");
    }

    /// Both diagnostic paths are absolute, whatever root they were given.
    ///
    /// These paths get *reported* — the paragraph names the in-flight file, a
    /// `warn!` names the stall log — and are read later, often by someone who did
    /// not start the build. A path relative to the process's cwd resolves
    /// differently depending on where the reader stands, which is worth nothing
    /// when the process it described is already gone.
    #[test]
    fn diagnostic_paths_are_absolute_even_from_a_relative_home() {
        let stall = StallLog::new(std::path::Path::new("rel/home"));
        assert!(
            stall.path().is_absolute(),
            "stall log path must be absolute: {:?}",
            stall.path()
        );
        assert!(
            stall
                .path()
                .ends_with(format!("stall-{}.log", std::process::id()))
        );

        let inflight = InflightLog::new(std::path::Path::new("rel/home"));
        assert!(
            inflight.path().is_absolute(),
            "in-flight log path must be absolute: {:?}",
            inflight.path()
        );
    }

    /// The paragraph points at the companion file by name, so the reader does
    /// not have to already know it exists.
    #[test]
    fn the_paragraph_names_the_inflight_companion_file() {
        let s = state();
        s.op_start(Op::Result, "//a:b", 0);
        let mut r = s.evaluate(61_000, T).expect("stalled");
        r.delta = Some(StallDelta {
            since: Duration::from_secs(120),
            done: 0,
            open: 0,
        });
        r.stuck = vec![hcore::hmemoizer::StuckCell {
            tag: "result",
            key: "//a:b".to_string(),
            waiters: Some(1),
            has_driver: true,
        }];
        let text = render_stall(&r);
        assert!(
            text.contains(&format!("inflight-{}.log", std::process::id())),
            "{text}"
        );
    }

    /// The companion file holds the *current* state, not a pile of stale ones.
    ///
    /// It is the uncapped dump — thousands of lines on a large graph — so
    /// appending it on all fourteen fires of a 25-minute wedge would bury the
    /// stall log's own escalation history, which is the thing that says "wedged"
    /// rather than "slow". The stall log appends; this one replaces.
    #[test]
    fn the_inflight_log_is_replaced_not_appended() {
        let home = tempfile::tempdir().expect("tempdir");
        let log = InflightLog::new(home.path());

        log.write("first report").expect("write");
        log.write("second report").expect("write");

        let body = std::fs::read_to_string(log.path()).expect("read");
        assert_eq!(body, "second report");
        assert!(
            log.path().to_string_lossy().contains("diag"),
            "it sits beside the stall log and the SIGQUIT dumps: {:?}",
            log.path()
        );
    }

    /// It carries all three sections, and the gated ones announce themselves
    /// rather than being silently absent — "no wait-for graph" and "wait-for
    /// graph not recorded" are very different messages to someone reading this
    /// during an incident.
    #[test]
    fn the_inflight_report_carries_every_section() {
        let text = hcore::hmemoizer::render_full_report();
        assert!(text.contains("in-flight inventory"), "{text}");
        assert!(text.contains("memoizer wait-for graph"), "{text}");
        assert!(text.contains("memoizer phases"), "{text}");
        assert!(text.contains("HEPH_DEBUG_MEMOIZER_CYCLE"), "{text}");
        assert!(text.contains("HEPH_PHASE_TRACE"), "{text}");
    }

    /// The measurement this exists for: permits the pool considers taken that no
    /// running target holds.
    ///
    /// A wedged build showed every worker permit gone, 107 targets queued on the
    /// semaphore, and *nothing executing* — no open execute span, no subprocess
    /// on any thread. Those two readings are only reconcilable one way: tokio
    /// hands a released permit to the first queued waiter and wakes it, so a
    /// permit granted to a future that is never polled again is spent and held
    /// by nobody. "Busy" and "leaked away" look identical without this line, and
    /// they want opposite investigations.
    #[test]
    fn unaccounted_permits_are_named() {
        let free = std::sync::Arc::new(AtomicU64::new(0));
        let s = state();
        s.register_worker_pool(12, {
            let free = std::sync::Arc::clone(&free);
            move || usize::try_from(free.load(Ordering::Relaxed)).unwrap_or(0)
        });
        s.op_start(Op::Result, "//a:b", 0);

        // Three targets running, nine permits gone with nobody holding them.
        let _running: Vec<RunningPermit> = (0..3)
            .map(|_| RunningPermit::on(std::sync::Arc::clone(&s)))
            .collect();

        let r = s.evaluate(61_000, T).expect("stalled");
        let w = r.workers.expect("the pool is registered");
        assert_eq!((w.capacity, w.free, w.running), (12, 0, 3));
        assert_eq!(w.unaccounted(), 9);

        let text = render_stall(&r);
        assert!(
            text.contains("workers      12 max, 0 free, 3 running, 9 unaccounted"),
            "{text}"
        );
    }

    /// A healthy busy pool must not be described as leaking: every taken permit
    /// is accounted for by a running target, so the line stays quiet about it.
    #[test]
    fn a_fully_busy_pool_reports_no_unaccounted_permits() {
        let s = state();
        s.register_worker_pool(4, || 0);
        s.op_start(Op::Result, "//a:b", 0);
        let _running: Vec<RunningPermit> = (0..4)
            .map(|_| RunningPermit::on(std::sync::Arc::clone(&s)))
            .collect();

        let r = s.evaluate(61_000, T).expect("stalled");
        assert_eq!(r.workers.expect("registered").unaccounted(), 0);
        let text = render_stall(&r);
        assert!(text.contains("4 max, 0 free, 4 running"), "{text}");
        assert!(!text.contains("unaccounted"), "{text}");
    }

    /// The guard is tied to the permit's scope, so a released permit stops being
    /// counted — a counter that drifted would make the arithmetic worthless.
    #[test]
    fn a_released_permit_stops_being_counted() {
        let s = state();
        s.register_worker_pool(2, || 2);
        {
            let _running = RunningPermit::on(std::sync::Arc::clone(&s));
            assert_eq!(s.worker_permits().expect("registered").running, 1);
        }
        assert_eq!(s.worker_permits().expect("registered").running, 0);
    }

    /// Before the engine registers its pool there is nothing to report, and the
    /// paragraph must not invent a line about it.
    #[test]
    fn an_unregistered_pool_reports_nothing() {
        let s = state();
        s.op_start(Op::Result, "//a:b", 0);
        let r = s.evaluate(61_000, T).expect("stalled");
        assert!(r.workers.is_none());
        assert!(
            !render_stall(&r).contains("workers  "),
            "{}",
            render_stall(&r)
        );
    }

    /// The `workers` limiter is declared by `global()` and, until now, fed by
    /// nothing — so `saturated_for` was permanently `None` and an exhausted
    /// worker pool could not appear in the paragraph at any threshold. The
    /// permit is taken after dep resolution but before `ExecuteStart`, so a
    /// target queued there shows up as neither an open `execute` span nor a
    /// limits line: completely invisible.
    #[test]
    fn a_saturated_worker_pool_reaches_the_paragraph() {
        let s = state();
        s.op_start(Op::Result, "//a:b", 0);
        let d = s.limiter("workers");
        d.observe(0, 1_000);

        let r = s.evaluate(61_000, T).expect("stalled");
        assert!(
            r.saturated.iter().any(|(name, _)| *name == "workers"),
            "workers must be reportable: {:?}",
            r.saturated
        );
        let text = render_stall(&r);
        assert!(text.contains("workers saturated for"), "{text}");
    }

    /// A frozen TUI swallows Ctrl-C (raw mode clears ISIG), so the paragraph must
    /// name the escalation that does work instead of leaving it to be
    /// rediscovered per incident.
    #[test]
    fn the_paragraph_names_the_next_step() {
        let s = state();
        s.op_start(Op::Result, "//a:b", 0);
        let text = render_stall(&s.evaluate(61_000, T).expect("stalled"));
        assert!(text.contains("kill -QUIT"), "{text}");
        assert!(
            text.contains(&std::process::id().to_string()),
            "the pid must be filled in, not left as a placeholder: {text}"
        );
    }

    /// The suppression is scoped to subprocesses. Many open remote-cache reads
    /// with nothing moving is the case this whole feature exists for and must
    /// fire at the ordinary threshold.
    #[test]
    fn suppression_does_not_hide_a_stalled_subsystem() {
        let s = state();
        for i in 0..98 {
            s.op_start(Op::RemoteCacheRead, &format!("//pkg:{i}"), 0);
        }
        assert!(s.evaluate(61_000, T).is_some());
    }

    /// A wide fan-out of subprocesses is a real signal — that is the worker pool
    /// wedged, not one slow compile — so the suppression must not swallow it.
    #[test]
    fn many_open_subprocesses_still_fire() {
        let s = state();
        for i in 0..(QUIET_EXEC_MAX + 1) {
            s.op_start(Op::Execute, &format!("//e:{i}"), 0);
        }
        assert!(s.evaluate(61_000, T).is_some());
    }

    /// Work that keeps closing spans never trips the threshold, however long any
    /// single span stays open.
    #[test]
    fn does_not_fire_while_work_keeps_completing() {
        let s = state();
        s.op_start(Op::Execute, "//slow:one", 0);
        let mut now = 0u64;
        for i in 0..20 {
            now += 30_000; // half the threshold
            s.op_start(Op::Result, &format!("//t:{i}"), now);
            s.op_end(Op::Result, &format!("//t:{i}"), now);
            assert!(
                s.evaluate(now, T).is_none(),
                "fired at {now}ms despite steady completions"
            );
        }
    }

    /// Bytes moving is progress even when no span opens or closes — a single
    /// large transfer must not read as a stall.
    #[test]
    fn does_not_fire_while_bytes_are_moving() {
        let s = state();
        s.op_start(Op::RemoteCacheRead, "//a:b", 0);
        for _ in 0..10 {
            s.add_bytes(Op::RemoteCacheRead, 1 << 20);
            assert!(s.evaluate(s.now_ms(), T).is_none());
        }
    }

    /// Fires when spans are open and nothing has moved.
    #[test]
    fn fires_when_open_spans_go_quiet() {
        let s = state();
        for i in 0..98 {
            s.op_start(Op::RemoteCacheRead, &format!("//pkg:{i}"), 0);
        }
        assert!(
            s.evaluate(30_000, T).is_none(),
            "not yet past the threshold"
        );

        let r = s.evaluate(512_000, T).expect("stalled");
        assert_eq!(r.dominant(), Some((Op::RemoteCacheRead, 98)));
        assert!(
            r.dominant_is_starved(),
            "no bytes moved: this is what separates stalled from slow"
        );
        assert_eq!(
            r.open[0].2,
            Some(Duration::from_millis(512_000)),
            "oldest open must be the span that opened at t=0, not a recent one"
        );
    }

    /// The oldest-open slot array must survive far more spans than it has slots
    /// without losing the long-lived one. A ring buffer would have evicted it.
    #[test]
    fn oldest_open_survives_slot_pressure() {
        let s = state();
        s.op_start(Op::RemoteCacheRead, "//stuck:one", 0);
        // Churn many short spans through the array.
        for i in 0..(SLOTS * 4) {
            let a = format!("//churn:{i}");
            s.op_start(Op::RemoteCacheRead, &a, 100_000);
            s.op_end(Op::RemoteCacheRead, &a, 100_000);
        }
        assert_eq!(
            s.oldest_open(Op::RemoteCacheRead, 512_000),
            Some(Duration::from_millis(512_000)),
            "the long-lived span was evicted; the reported age is now meaningless"
        );
    }

    /// A duplicated `*End` must not wrap the counter. `progress.rs` documents a
    /// real same-addr double-fire; unsigned arithmetic would print
    /// `18446744073709551615 open` in the middle of the incident.
    #[test]
    fn open_count_never_wraps_on_unpaired_ends() {
        let s = state();
        s.op_start(Op::Execute, "//a:b", 0);
        s.op_end(Op::Execute, "//a:b", 1);
        s.op_end(Op::Execute, "//a:b", 2);
        assert_eq!(s.open_count(Op::Execute), 0);

        s.op_end(Op::Execute, "//never:started", 3);
        assert_eq!(s.open_count(Op::Execute), 0);
    }

    /// `Result` spans are unbounded — thousands are open at once on a deep graph —
    /// so an age from a fixed slot array would be a guess. Report count only.
    #[test]
    fn result_spans_report_count_but_not_age() {
        let s = state();
        s.op_start(Op::Result, "//a:b", 0);
        assert_eq!(s.open_count(Op::Result), 1);
        assert_eq!(s.oldest_open(Op::Result, 500_000), None);
    }

    /// A hypothesis is only offered when one op clearly dominates.
    #[test]
    fn no_dominant_op_when_the_mix_is_even() {
        let s = state();
        for i in 0..10 {
            s.op_start(Op::RemoteCacheRead, &format!("//r:{i}"), 0);
            s.op_start(Op::Execute, &format!("//e:{i}"), 0);
        }
        let r = s.evaluate(120_000, T).expect("stalled");
        assert_eq!(
            r.dominant(),
            None,
            "an even split must not be blamed on one op"
        );
    }

    /// Limiter saturation ages from the transition, and clears when permits free.
    #[test]
    fn limiter_reports_how_long_it_has_been_saturated() {
        let s = state();
        let l = &s.limiters()[0];
        l.observe(0, 1_000);
        l.observe(0, 5_000);
        assert_eq!(
            s.saturated(48_000),
            vec![("workers", Duration::from_millis(47_000))]
        );
        l.observe(3, 50_000);
        assert!(s.saturated(60_000).is_empty());
    }

    /// A limiter sampled only from its own acquire path freezes exactly when it
    /// matters.
    ///
    /// A stall report is rendered *because* nothing is acquiring any more, so
    /// the last acquire-time reading is by definition stale. A real wedge
    /// reported "workers saturated for 90s" from a stamp left behind when work
    /// stopped, while zero `execute` spans were open — the pool was in fact
    /// free, and the paragraph could not say so. Permits held means deadlocked
    /// on the pool; permits free means wedged elsewhere. That is the whole
    /// question at a stall, and the two must not look identical.
    #[test]
    fn a_live_gauge_reports_the_present_not_the_last_acquire() {
        let s = state();
        let l = &s.limiters()[0];
        let free = std::sync::Arc::new(AtomicU64::new(0));

        l.attach_gauge({
            let free = std::sync::Arc::clone(&free);
            move || usize::try_from(free.load(Ordering::Relaxed)).unwrap_or(0)
        });

        // Exhausted, and nothing has acquired since.
        l.observe(0, 1_000);
        assert_eq!(
            s.saturated(48_000),
            vec![("workers", Duration::from_millis(47_000))]
        );

        // Permits come back with no acquire to notice — the frozen reading would
        // still claim saturation here.
        free.store(4, Ordering::Relaxed);
        assert!(
            s.saturated(60_000).is_empty(),
            "a live gauge must retract a stale saturation claim"
        );

        // And the reverse: it can go saturated between reports, without an
        // acquire, and still be reported.
        free.store(0, Ordering::Relaxed);
        assert_eq!(
            s.saturated(61_000)
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>(),
            vec!["workers"]
        );
    }

    /// Without a gauge the old acquire-only behaviour is untouched, so a limiter
    /// nobody has wired up cannot start reporting phantom saturation.
    #[test]
    fn a_limiter_without_a_gauge_keeps_its_acquire_only_reading() {
        let s = state();
        let l = &s.limiters()[0];
        l.observe(0, 1_000);
        assert_eq!(
            s.saturated(5_000),
            vec![("workers", Duration::from_millis(4_000))]
        );
    }

    /// First attachment wins, so arming it from a hot path is safe.
    #[test]
    fn attaching_a_gauge_twice_keeps_the_first() {
        let s = state();
        let l = &s.limiters()[0];
        l.attach_gauge(|| 0);
        l.attach_gauge(|| 7);
        l.observe(9, 1_000);
        assert_eq!(
            s.saturated(2_000)
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>(),
            vec!["workers"],
            "the first gauge (0 permits) must still be the one consulted"
        );
    }

    /// The paragraph must name the subsystem, the count and the age, and must not
    /// use failure vocabulary — it lands in the CI log of every slow build, and a
    /// processor grepping stderr for "error"/"failed" must not match a progress
    /// notice.
    #[test]
    fn stall_paragraph_names_the_subsystem_without_failure_vocabulary() {
        let s = state();
        for i in 0..98 {
            s.op_start(Op::RemoteCacheRead, &format!("//pkg:{i}"), 0);
        }
        let r = s.evaluate(512_000, T).expect("stalled");
        let text = render_stall(&r);

        assert!(text.contains("no progress for 512s"), "{text}");
        assert!(text.contains("98 remote-cache-read"), "{text}");
        assert!(text.contains("oldest 512s"), "{text}");
        assert!(text.contains("not a stable interface"), "{text}");
        let lower = text.to_lowercase();
        for banned in ["error", "failed", "panic"] {
            assert!(
                !lower.contains(banned),
                "paragraph contains failure vocabulary {banned:?}: {text}"
            );
        }
    }

    /// With no dominant op, the paragraph offers the table and no theory.
    #[test]
    fn stall_paragraph_offers_no_theory_without_a_dominant_op() {
        let s = state();
        for i in 0..10 {
            s.op_start(Op::RemoteCacheRead, &format!("//r:{i}"), 0);
            s.op_start(Op::Execute, &format!("//e:{i}"), 0);
        }
        let text = render_stall(&s.evaluate(120_000, T).expect("stalled"));
        assert!(!text.contains("looks like"), "{text}");
    }

    /// Wedged before any span opens — matching, or walking packages on a large
    /// repo. Real phase, and silent if left uninstrumented.
    #[test]
    fn stall_paragraph_covers_the_no_open_spans_phase() {
        let s = state();
        let text = render_stall(&s.evaluate(120_000, T).expect("stalled"));
        assert!(text.contains("still resolving targets"), "{text}");
    }

    /// The log lands under the home dir's `diag/` — beside the `SIGQUIT` thread
    /// dumps, which is where a reader chasing a hang already looks — and creates
    /// that directory itself, since nothing else does before the first stall.
    #[test]
    fn stall_log_writes_under_the_home_diag_dir() {
        let home = tempfile::tempdir().expect("tempdir");
        let log = StallLog::new(home.path());
        assert_eq!(
            log.path().parent(),
            Some(home.path().join("diag").as_path())
        );

        log.append("first\n").expect("append into a missing dir");
        assert_eq!(
            std::fs::read_to_string(log.path()).expect("read back"),
            "first\n"
        );
    }

    /// Successive fires accumulate. The escalation sequence is the diagnostic —
    /// "oldest 60s" then "oldest 512s" says wedged, where either alone says only
    /// slow — so a truncating write would destroy the signal.
    #[test]
    fn stall_log_appends_rather_than_truncating() {
        let home = tempfile::tempdir().expect("tempdir");
        let log = StallLog::new(home.path());
        log.append("oldest 60s\n").expect("append");
        log.append("oldest 512s\n").expect("append");
        assert_eq!(
            std::fs::read_to_string(log.path()).expect("read back"),
            "oldest 60s\noldest 512s\n"
        );
    }

    /// The hook folds the real event stream, and `ResultEnd` counts failures.
    #[test]
    fn hook_folds_events_into_the_table() {
        let s = state();
        let h = DiagHook::new(std::sync::Arc::clone(&s));
        h.on_event(&ev(BuildEventKind::ResultStart {
            addr: "//a:b".into(),
        }));
        assert_eq!(s.open_count(Op::Result), 1);
        h.on_event(&ev(BuildEventKind::ResultEnd {
            addr: "//a:b".into(),
            error: Some("boom".into()),
        }));
        assert_eq!(s.open_count(Op::Result), 0);
        assert_eq!((s.done(), s.failed()), (1, 1));
    }
}
