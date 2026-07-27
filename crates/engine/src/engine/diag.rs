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
}

impl Limiter {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            saturated_since_ms: AtomicU64::new(0),
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
        }
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

    fn op_start(&self, op: Op, addr: &str, now_ms: u64) {
        let Some(st) = self.op(op) else { return };
        st.open.fetch_add(1, Ordering::Relaxed);
        self.touch(now_ms);
        if !op.tracks_oldest() {
            return;
        }
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

    fn op_end(&self, op: Op, addr: &str, now_ms: u64) {
        let Some(st) = self.op(op) else { return };
        st.open.fetch_sub(1, Ordering::Relaxed);
        self.touch(now_ms);
        if !op.tracks_oldest() {
            return;
        }
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
        let st = self.op(op)?;
        let oldest = st
            .slot_key
            .iter()
            .zip(st.slot_start.iter())
            .filter(|(k, _)| k.load(Ordering::Acquire) != FREE)
            .map(|(_, start)| start.load(Ordering::Acquire))
            .min();
        oldest.map(|s| Duration::from_millis(now_ms.saturating_sub(s)))
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
            .filter_map(|l| l.saturated_for(now_ms).map(|d| (l.name, d)))
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
        self.bytes
            .iter()
            .find(|(o, _)| *o == op)
            .is_some_and(|(_, b)| *b == 0)
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

fn secs(d: Duration) -> String {
    format!("{}s", d.as_secs())
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

/// Render a stall report as the paragraph printed to stderr.
///
/// The wording deliberately avoids the vocabulary of a *failure* — no "error",
/// no "failed". This lands in the CI log of every slow build, and a log processor
/// grepping stderr for failure signatures must not match a progress notice. It
/// says "no progress", never "hung": the run may well be fine.
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
        if r.open.iter().any(|(o, _, _)| o == op) {
            out.push_str(&format!(
                "  bytes        {} in the last {}s on {}\n",
                human_bytes(*b),
                BYTES_WINDOW.as_secs(),
                op.label()
            ));
        }
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

    // Only volunteer a cause when one op clearly dominates *and* something
    // corroborates it. A wrong automated hypothesis costs more credibility than
    // no hypothesis — better to show the table and let the reader conclude.
    if let Some((op, _)) = r.dominant()
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

    out.push_str("  (diagnostic text, not a stable interface)\n");
    out
}

/// Poll [`DiagState::evaluate`] and emit a paragraph when it reports a stall.
///
/// Runs on a plain OS thread, never a tokio timer: the whole point is to keep
/// working when the runtime is saturated or the reactor is not turning, which is
/// the condition being diagnosed. It is deliberately *not* merged into
/// `hcore::blocking`'s backstop thread — that one is a correctness mechanism for
/// dropped wake-ups, and this one ends in a blocking `write` to stderr. In CI
/// stderr is a pipe; a stalled consumer would fill the 64 KiB buffer and block
/// forever, freezing waker delivery for the entire blocking pool. The watchdog
/// would then hang the process it exists to diagnose.
pub struct Watchdog {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Watchdog {
    pub fn spawn(
        state: std::sync::Arc<DiagState>,
        threshold: Duration,
        emit: impl Fn(&str) + Send + 'static,
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
                while !stop_thread.load(Ordering::Relaxed) {
                    std::thread::sleep(tick);
                    let now = state.now_ms();
                    let Some(report) = state.evaluate(now, threshold) else {
                        last_fired = None;
                        continue;
                    };
                    // Escalate rather than repeat every tick: a stall that lasts
                    // an hour must not produce 3600 paragraphs.
                    let due = last_fired.is_none_or(|f| {
                        now.saturating_sub(f)
                            >= u64::try_from(threshold.as_millis()).unwrap_or(60_000) * 2
                    });
                    if due {
                        emit(&render_stall(&report));
                        last_fired = Some(now);
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
