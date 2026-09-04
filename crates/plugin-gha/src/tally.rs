//! Build-status aggregation for the GitHub Actions hook.
//!
//! Folds the engine's [`BuildEvent`] stream into the handful of facts the two
//! renderers need. The shape is driven by one constraint: **a CI run has ~20k
//! targets**, so nothing retained here may be proportional to the graph and
//! nothing rendered from it may be a per-target list.
//!
//! The rules that follow from that, and the reasons they are not negotiable:
//!
//! - **One record per *in-flight* target**, dropped at `ResultEnd`. The previous
//!   design kept `finished` and `cache_hit` as graph-wide `BTreeSet<String>`s but
//!   only ever read them as `matched ∩ …`, retaining ~16 MB at 100k targets of
//!   which ~90% was never read. The TUI reached the same conclusion after
//!   measuring (`crates/tui/src/tui/progress.rs:668`): fold at both edges, keep
//!   no history.
//! - **Counters, not sets**, for everything whose only use is a number.
//! - **Failures are tracked by *root*.** One broken leaf under 20k dependents
//!   produces 20k `ResultEnd`s carrying `dependency failed (root: …)`; counting
//!   them individually renders `failed: 20001` and buries the actual cause. Each
//!   collateral failure increments its root's `blocked` count and a per-package
//!   tally instead of allocating a row.
//! - **Slowest-completed is a bounded top-N heap**, never a map. Retaining a
//!   duration per completed target costs ~118 B each — ~2.4 MB at 20k, ~12 MB at
//!   100k — for a list that only ever shows 20 rows.
//! - **The addr `String` is allocated once**, into a `Box<str>` key on first
//!   sight; every later event for that target is a lookup and a field write.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use hcore::events::{BuildEvent, BuildEventKind, LogTailData};
use rustc_hash::{FxHashMap, FxHashSet};

/// How many completed targets to retain for the "slowest" table. The heap is
/// capped at this, so the memory is constant regardless of graph size.
pub(crate) const SLOWEST_KEPT: usize = 20;

/// Root failures retained with full detail. Beyond this only the count is kept —
/// a build with 200 independent breakages is a catastrophe whose report is the
/// count, not the list.
pub(crate) const ROOTS_KEPT: usize = 10;

/// One long-running phase a target can be in. A target is slow because of a
/// *single* active phase, so the report names which one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Execute,
    /// Warming a scratch cache before the target can run — seeding a lineage or
    /// pulling a snapshot from the remote. On the critical path (it holds the
    /// slot guard and precedes the worker permit), so unlike a background upload
    /// it belongs in "running longest".
    ScratchPrepare,
    CachePull,
    LocalCacheWrite,
    RemoteCacheWrite,
}

impl Phase {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Phase::Execute => "execute",
            Phase::ScratchPrepare => "scratch prepare",
            Phase::CachePull => "cache pull",
            Phase::LocalCacheWrite => "cache write",
            Phase::RemoteCacheWrite => "remote cache write",
        }
    }

    /// Runs on a background task *after* the build's critical path, so it is
    /// never the reason a build feels stuck and must not appear in the live
    /// "running longest" table.
    ///
    /// This matters more than it looks: `RemoteCacheWriteStart` is emitted
    /// *before* the upload semaphore is acquired (deliberately — a queued push
    /// should still render as in-flight, see
    /// `crates/engine/src/engine/remote_cache.rs:1643`) and there are only
    /// [16 slots](`MAX_CONCURRENT_UPLOADS`). At 20k targets that means ~20k
    /// entries in flight with 16 actually uploading, so per-target rows would
    /// flood the table with targets that are queued, not slow.
    fn is_background(self) -> bool {
        matches!(self, Phase::RemoteCacheWrite)
    }
}

/// Which cache answered. Kept per in-flight target so a hit can be *un-counted*
/// off the right counter when the target turns out to rebuild.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheHitKind {
    Local,
    Remote,
}

/// State for a target that is currently in flight. Dropped at `ResultEnd`.
#[derive(Debug, Default)]
struct TargetRec {
    /// First event seen for this target, used as its duration origin.
    started_ms: u64,
    /// The active phase and when it began, or `None` between phases.
    phase: Option<(Phase, u64)>,
    driver: Option<Box<str>>,
    /// The cache that answered, if any — retracted when the target executes.
    cache: Option<CacheHitKind>,
    /// Whether this target actually ran. A later cache hit must not count a
    /// target that already executed.
    executed: bool,
}

/// A completed target retained for the "slowest" table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completed {
    pub duration_ms: u64,
    pub addr: String,
    pub driver: Option<String>,
}

// Ordered by duration, then addr, so the heap is deterministic and two runs
// produce diffable output.
impl Ord for Completed {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.duration_ms
            .cmp(&other.duration_ms)
            .then_with(|| other.addr.cmp(&self.addr))
    }
}
impl PartialOrd for Completed {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A failure that is *not* downstream of another failure — the thing a human has
/// to fix. Collateral failures are folded into `blocked` / `blocked_by_pkg`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RootFailure {
    pub addr: String,
    pub message: String,
    /// The subprocess's exit status, when the failure was a subprocess.
    pub exit_status: Option<String>,
    /// The last lines of the target's log — the part that actually says what
    /// went wrong. This is what makes a red build diagnosable from the report
    /// rather than only from the job log.
    pub log_tail: Option<LogTailData>,
    pub driver: Option<String>,
    pub duration_ms: Option<u64>,
    /// Targets that failed solely because this one did.
    pub blocked: usize,
    /// Where those blocked targets live, for the rollup line. Bounded by package
    /// count (~1–3k), not target count.
    pub blocked_by_pkg: FxHashMap<Box<str>, usize>,
}

/// A target waiting on the per-addr result lock — the "is it stuck?" signal.
#[derive(Debug, Clone)]
pub(crate) struct LockWait {
    pub addr: String,
    pub since_ms: u64,
    pub holder_pid: Option<u32>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Counters {
    pub executed: usize,
    pub cached_local: usize,
    pub cached_remote: usize,
    pub miss_local: usize,
    pub miss_remote: usize,
    /// Collateral failures across all roots, including those beyond `ROOTS_KEPT`.
    pub blocked: usize,
    /// Root failures seen, including those beyond `ROOTS_KEPT`.
    pub roots_total: usize,
}

impl Counters {
    pub(crate) fn cached(&self) -> usize {
        self.cached_local.saturating_add(self.cached_remote)
    }

    pub(crate) fn misses(&self) -> usize {
        self.miss_local.saturating_add(self.miss_remote)
    }

    /// Hit rate over targets the cache was actually consulted for. `None` when it
    /// was never consulted — rendering `0%` there would be a lie.
    pub(crate) fn hit_rate(&self) -> Option<f64> {
        let considered = self.cached().saturating_add(self.misses());
        (considered > 0).then(|| self.cached() as f64 / considered as f64)
    }
}

/// The folded build status.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    /// Top-level matched targets → whether they have finished. Bounded by what
    /// the user selected, not by the graph, and it is the only exact membership
    /// set retained.
    matched: FxHashMap<Box<str>, bool>,
    matched_done: usize,
    matched_complete: bool,

    /// In-flight targets only. Dropped at `ResultEnd`.
    live: FxHashMap<Box<str>, TargetRec>,

    /// Targets currently waiting on the result lock. Bounded by concurrency.
    lock_waits: FxHashMap<Box<str>, LockWait>,

    /// In-flight background cache uploads, rendered as one aggregate row rather
    /// than thousands of per-target ones. See [`Phase::is_background`].
    background_ops: usize,

    counters: Counters,

    /// Roots in first-seen order — never re-sorted, so consecutive renders differ
    /// by appends only and an agent diffing two fetches sees "one new root"
    /// rather than a reshuffled list.
    roots: Vec<RootFailure>,
    root_index: FxHashMap<Box<str>, usize>,

    /// Min-heap of the [`SLOWEST_KEPT`] longest completed targets.
    slowest: BinaryHeap<Reverse<Completed>>,

    /// Cache misses per package, for the rollup that replaces a per-target list.
    misses_by_pkg: FxHashMap<Box<str>, usize>,

    max_workers: usize,
    /// Whether this invocation stops at the first failure. Reported by the
    /// engine, never inferred here — see `BuildEventKind::RequestConfig`.
    fail_fast: bool,
    scratch_disabled: bool,
    /// Open scratch waits: consumer -> (cache, started at). Drained into
    /// `scratch_waits` when the wait ends.
    scratch_wait_since: FxHashMap<Box<str>, (Box<str>, u64)>,
    /// Finished scratch waits per cache: `(waiters, total ms)`. The total is the
    /// number worth reporting — one target blocked briefly is noise, dozens
    /// blocked between them is why the job was slow.
    scratch_waits: FxHashMap<Box<str>, (u64, u64)>,
    /// Caches restored at a path they were not produced at, so they are present
    /// and inert. Named because nothing else distinguishes this from a hit.
    scratch_inert: FxHashSet<Box<str>>,
    /// Caches dropped for exceeding `max_size` — the targets using them ran cold.
    scratch_dropped: FxHashSet<Box<str>>,
    first_event_ms: Option<u64>,
    last_event_ms: u64,

    /// Set once the event stream closes. The build is over, so the status must
    /// settle: progress counts are not a reliable "finished" signal because a
    /// matched transparent group emits no `ResultEnd`.
    closed: bool,
}

/// A failing `ResultEnd`, borrowed from the event.
///
/// The engine extracts this structure once at the emit site, so the reporter
/// never has to re-derive it from prose. It previously told collateral damage
/// from a root failure by searching the flattened message for
/// `"dependency failed (root: …)"` — a sniff of a `Display` impl that would
/// break silently the moment that wording changed, taking the root/collateral
/// split with it.
#[derive(Clone, Copy)]
pub(crate) struct Failure<'a> {
    pub message: &'a str,
    pub upstream_of: Option<&'a str>,
    pub exit_status: Option<&'a str>,
    pub log_tail: Option<&'a LogTailData>,
}

/// The package part of `//pkg/sub:name`, or the whole addr if it has no `:`.
fn package_of(addr: &str) -> &str {
    match addr.rfind(':') {
        Some(i) => addr.get(..i).unwrap_or(addr),
        None => addr,
    }
}

impl Tally {
    /// Fold one event.
    ///
    /// This runs on the plugin's event-drain thread for every event — ~160k times
    /// on a 20k-target build — so it must stay allocation-light. In particular it
    /// must never `format!`, and must never allocate a package `String` per
    /// event (hence `get_mut`-then-`insert` on the package maps).
    pub(crate) fn apply(&mut self, ev: &BuildEvent) {
        self.first_event_ms.get_or_insert(ev.at_unix_ms);
        self.last_event_ms = self.last_event_ms.max(ev.at_unix_ms);

        // Exhaustive on purpose: a new `BuildEventKind` must be a compile error
        // here, not a silent drop. The previous `_ => {}` swallowed six kinds,
        // four of which this renderer wants.
        match &ev.kind {
            BuildEventKind::RequestConfig {
                max_workers,
                fail_fast,
                scratch_disabled,
            } => {
                self.max_workers = *max_workers;
                self.fail_fast = *fail_fast;
                self.scratch_disabled = *scratch_disabled;
            }

            BuildEventKind::Matched { addrs, complete } => {
                for a in addrs {
                    // Allocate the key only for genuinely new addrs — `Matched`
                    // arrives incrementally and can repeat an addr.
                    if !self.matched.contains_key(a.as_str()) {
                        self.matched.insert(a.as_str().into(), false);
                    }
                }
                if *complete {
                    self.matched_complete = true;
                }
            }

            BuildEventKind::ResultStart { addr } => {
                self.ensure(addr, ev.at_unix_ms);
            }

            BuildEventKind::ResultEnd {
                addr,
                error,
                upstream_of,
                exit_status,
                log_tail,
            } => {
                self.finish(
                    addr,
                    error.as_deref().map(|message| Failure {
                        message,
                        upstream_of: upstream_of.as_deref(),
                        exit_status: exit_status.as_deref(),
                        log_tail: log_tail.as_ref(),
                    }),
                    ev.at_unix_ms,
                );
            }

            BuildEventKind::ExecuteStart { addr, driver, .. } => {
                let at = ev.at_unix_ms;
                self.ensure(addr, at);
                let mut retract = None;
                if let Some(rec) = self.live.get_mut(addr.as_str()) {
                    rec.phase = Some((Phase::Execute, at));
                    rec.executed = true;
                    if rec.driver.is_none() {
                        rec.driver = Some(driver.as_str().into());
                    }
                    // A target that executes is not a cached target, even when a
                    // hit was announced first: the engine decides a hit from the
                    // revision's manifest, and a manifest can outlive its blobs
                    // (GC, a lifecycle rule, or blobs never pulled on a run that
                    // is now offline), in which case it rebuilds. Retract the hit
                    // off the counter it was put on, or "executed + cached" can
                    // sum past the target count.
                    retract = rec.cache.take();
                }
                match retract {
                    Some(CacheHitKind::Local) => {
                        self.counters.cached_local = self.counters.cached_local.saturating_sub(1);
                    }
                    Some(CacheHitKind::Remote) => {
                        self.counters.cached_remote = self.counters.cached_remote.saturating_sub(1);
                    }
                    None => {}
                }
            }

            BuildEventKind::ExecuteEnd { addr, error } => {
                self.clear_phase(addr);
                if error.is_none() {
                    self.counters.executed = self.counters.executed.saturating_add(1);
                }
            }

            BuildEventKind::ScratchPrepareStart { addr, .. } => {
                self.start_phase(addr, Phase::ScratchPrepare, ev.at_unix_ms);
            }
            BuildEventKind::ScratchPrepareEnd {
                addr,
                scratch,
                outcome,
                path_mismatch,
                ..
            } => {
                if *path_mismatch {
                    self.scratch_inert.insert(scratch.as_str().into());
                }
                if outcome == "dropped_over_max" {
                    self.scratch_dropped.insert(scratch.as_str().into());
                }
                self.clear_phase(addr);
            }
            BuildEventKind::RemoteCacheReadStart { addr } => {
                self.start_phase(addr, Phase::CachePull, ev.at_unix_ms);
            }
            BuildEventKind::LocalCacheWriteStart { addr } => {
                self.start_phase(addr, Phase::LocalCacheWrite, ev.at_unix_ms);
            }
            BuildEventKind::RemoteCacheWriteStart { addr } => {
                self.start_phase(addr, Phase::RemoteCacheWrite, ev.at_unix_ms);
            }
            BuildEventKind::RemoteCacheReadEnd { addr, .. }
            | BuildEventKind::LocalCacheWriteEnd { addr, .. }
            | BuildEventKind::RemoteCacheWriteEnd { addr, .. } => {
                self.clear_phase(addr);
            }

            // A grant is an audit record, not build progress: it belongs on the
            // stream for an incident to read, and nothing in a job summary
            // changes because of it.
            BuildEventKind::SecretGranted { .. } => {}
            BuildEventKind::LocalCacheHit { addr } => {
                self.hit(addr, ev.at_unix_ms, CacheHitKind::Local)
            }
            BuildEventKind::RemoteCacheHit { addr } => {
                self.hit(addr, ev.at_unix_ms, CacheHitKind::Remote)
            }

            BuildEventKind::LocalCacheMiss { addr } => {
                self.counters.miss_local = self.counters.miss_local.saturating_add(1);
                self.bump_pkg_miss(addr);
            }
            BuildEventKind::RemoteCacheMiss { .. } => {
                // Counted, but not rolled up by package: a remote miss almost
                // always accompanies the local miss already counted above, and
                // double-counting the package would overstate the rollup.
                self.counters.miss_remote = self.counters.miss_remote.saturating_add(1);
            }

            BuildEventKind::ResultLockWaitStart { addr, holder_pid } => {
                self.lock_waits.insert(
                    addr.as_str().into(),
                    LockWait {
                        addr: addr.clone(),
                        since_ms: ev.at_unix_ms,
                        holder_pid: *holder_pid,
                    },
                );
            }
            BuildEventKind::ResultLockWaitEnd { addr } => {
                self.lock_waits.remove(addr.as_str());
            }

            BuildEventKind::ScratchLockWaitStart { addr, scratch, .. } => {
                self.scratch_wait_since.insert(
                    addr.as_str().into(),
                    (scratch.as_str().into(), ev.at_unix_ms),
                );
            }
            BuildEventKind::ScratchLockWaitEnd { addr, .. } => {
                if let Some((scratch, since)) = self.scratch_wait_since.remove(addr.as_str()) {
                    let e = self.scratch_waits.entry(scratch).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += ev.at_unix_ms.saturating_sub(since);
                }
            }

            // Emitted only by `heph tool gc` / `clean`, which this hook does not
            // report on — those commands have their own output.
            BuildEventKind::GcTargetSwept { .. } => {}

            // An event kind from a host newer than this plugin. Skipping it keeps
            // the rest of the stream flowing; the alternative is a decode failure,
            // which the SDK treats as end-of-stream and would silently truncate
            // everything after it.
            BuildEventKind::Unknown => {}
        }
    }

    /// Ensure a record exists for `addr`, allocating the `Box<str>` key only on
    /// first sight.
    ///
    /// Deliberately does *not* return `&mut TargetRec`: doing so needs either a
    /// conditional-return borrow that current borrowck rejects, or `entry()`,
    /// which takes an owned key and would therefore allocate on every one of
    /// ~160k events. Callers pair this with `live.get_mut`. Two hashes on the hit
    /// path is far cheaper than one allocation.
    fn ensure(&mut self, addr: &str, at: u64) {
        if !self.live.contains_key(addr) {
            self.live.insert(
                addr.into(),
                TargetRec {
                    started_ms: at,
                    ..TargetRec::default()
                },
            );
        }
    }

    fn start_phase(&mut self, addr: &str, phase: Phase, at: u64) {
        if phase.is_background() {
            self.background_ops = self.background_ops.saturating_add(1);
        }
        self.ensure(addr, at);
        if let Some(rec) = self.live.get_mut(addr) {
            rec.phase = Some((phase, at));
        }
    }

    fn clear_phase(&mut self, addr: &str) {
        if let Some(rec) = self.live.get_mut(addr)
            && let Some((phase, _)) = rec.phase.take()
            && phase.is_background()
        {
            self.background_ops = self.background_ops.saturating_sub(1);
        }
    }

    fn hit(&mut self, addr: &str, at: u64, kind: CacheHitKind) {
        self.ensure(addr, at);
        // A hit arriving *after* the target executed (possible: the result
        // memoizer keys on `(addr, outputs, is_top)`, so a second variant can hit
        // the local cache the first variant just populated) must not count the
        // same addr as both executed and cached.
        match self.live.get_mut(addr) {
            Some(rec) if rec.executed || rec.cache.is_some() => return,
            Some(rec) => rec.cache = Some(kind),
            None => return,
        }
        match kind {
            CacheHitKind::Local => {
                self.counters.cached_local = self.counters.cached_local.saturating_add(1)
            }
            CacheHitKind::Remote => {
                self.counters.cached_remote = self.counters.cached_remote.saturating_add(1)
            }
        }
    }

    fn bump_pkg_miss(&mut self, addr: &str) {
        let pkg = package_of(addr);
        // `get_mut` first so the package key is allocated only on first sight —
        // otherwise this allocates a `String` on every one of ~160k events.
        if let Some(n) = self.misses_by_pkg.get_mut(pkg) {
            *n = n.saturating_add(1);
            return;
        }
        self.misses_by_pkg.insert(pkg.into(), 1);
    }

    fn finish(&mut self, addr: &str, error: Option<Failure<'_>>, at: u64) {
        let rec = self.live.remove(addr);

        // Progress is over the matched top-level set, deduped: the memoizer can
        // emit several `ResultEnd`s for one addr, and `done` must never exceed
        // `total`.
        if let Some(done) = self.matched.get_mut(addr)
            && !*done
        {
            *done = true;
            self.matched_done = self.matched_done.saturating_add(1);
        }

        if let Some(rec) = &rec
            && error.is_none()
            && rec.executed
        {
            let duration_ms = at.saturating_sub(rec.started_ms);
            self.push_slowest(Completed {
                duration_ms,
                addr: addr.to_string(),
                driver: rec.driver.as_deref().map(str::to_string),
            });
        }

        let Some(failure) = error else { return };

        if let Some(root) = failure.upstream_of {
            self.record_collateral(root, addr);
            return;
        }
        self.record_root(addr, failure, rec.as_ref(), at);
    }

    fn push_slowest(&mut self, c: Completed) {
        if self.slowest.len() < SLOWEST_KEPT {
            self.slowest.push(Reverse(c));
            return;
        }
        // Replace the smallest if this one is longer. `peek` on a min-heap of
        // `Reverse` yields the shortest retained duration.
        if let Some(Reverse(min)) = self.slowest.peek()
            && c.duration_ms > min.duration_ms
        {
            self.slowest.pop();
            self.slowest.push(Reverse(c));
        }
    }

    fn record_root(&mut self, addr: &str, failure: Failure<'_>, rec: Option<&TargetRec>, at: u64) {
        self.counters.roots_total = self.counters.roots_total.saturating_add(1);
        if self.root_index.contains_key(addr) {
            // Deduped: one addr can produce several `ResultEnd`s.
            self.counters.roots_total = self.counters.roots_total.saturating_sub(1);
            return;
        }
        if self.roots.len() >= ROOTS_KEPT {
            // Beyond the cap only the count is kept.
            return;
        }
        self.root_index.insert(addr.into(), self.roots.len());
        self.roots.push(RootFailure {
            addr: addr.to_string(),
            message: failure.message.to_string(),
            exit_status: failure.exit_status.map(str::to_string),
            log_tail: failure.log_tail.cloned(),
            driver: rec.and_then(|r| r.driver.as_deref().map(str::to_string)),
            duration_ms: rec.map(|r| at.saturating_sub(r.started_ms)),
            blocked: 0,
            blocked_by_pkg: FxHashMap::default(),
        });
    }

    fn record_collateral(&mut self, root: &str, blocked_addr: &str) {
        self.counters.blocked = self.counters.blocked.saturating_add(1);
        let Some(&idx) = self.root_index.get(root) else {
            // The root is beyond `ROOTS_KEPT`, or its own `ResultEnd` has not
            // arrived yet. The global `blocked` counter above still reflects it.
            return;
        };
        let Some(rf) = self.roots.get_mut(idx) else {
            return;
        };
        rf.blocked = rf.blocked.saturating_add(1);
        let pkg = package_of(blocked_addr);
        if let Some(n) = rf.blocked_by_pkg.get_mut(pkg) {
            *n = n.saturating_add(1);
            return;
        }
        rf.blocked_by_pkg.insert(pkg.into(), 1);
    }

    pub(crate) fn set_closed(&mut self) {
        self.closed = true;
    }

    pub(crate) fn counters(&self) -> Counters {
        self.counters
    }

    pub(crate) fn roots(&self) -> &[RootFailure] {
        &self.roots
    }

    pub(crate) fn max_workers(&self) -> usize {
        self.max_workers
    }

    pub(crate) fn fail_fast(&self) -> bool {
        self.fail_fast
    }

    pub(crate) fn scratch_disabled(&self) -> bool {
        self.scratch_disabled
    }

    /// Caches that were restored but are inert — present, unused, and
    /// indistinguishable from a hit anywhere else in the report.
    pub(crate) fn scratch_inert(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.scratch_inert.iter().map(|s| &**s).collect();
        v.sort_unstable();
        v
    }

    /// Caches dropped for exceeding their cap. Every target using one ran cold,
    /// which is otherwise indistinguishable from a first build.
    pub(crate) fn scratch_dropped(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.scratch_dropped.iter().map(|s| &**s).collect();
        v.sort_unstable();
        v
    }

    /// Caches that serialized targets this run, as `(cache, waiters, total ms)`,
    /// worst total first.
    pub(crate) fn scratch_waits(&self) -> Vec<(&str, u64, u64)> {
        let mut v: Vec<(&str, u64, u64)> = self
            .scratch_waits
            .iter()
            .map(|(k, (n, ms))| (&**k, *n, *ms))
            .collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        v
    }

    /// Targets currently inside a *foreground* phase — the denominator for
    /// "workers busy". Background cache uploads are excluded; they are not
    /// occupying a worker slot the build is waiting on.
    pub(crate) fn active_foreground(&self) -> usize {
        self.live
            .values()
            .filter(|r| r.phase.is_some_and(|(p, _)| !p.is_background()))
            .count()
    }

    pub(crate) fn background_ops(&self) -> usize {
        self.background_ops
    }

    /// `(done, total, total_is_final)` over the matched top-level set.
    pub(crate) fn progress(&self) -> (usize, usize, bool) {
        (self.matched_done, self.matched.len(), self.matched_complete)
    }

    /// Elapsed across the observed event stream. Named "build time" rather than
    /// wall time: the first event is not process start, so this excludes startup
    /// and matcher resolution before the first emit.
    pub(crate) fn elapsed_ms(&self, now_ms: u64) -> u64 {
        let Some(first) = self.first_event_ms else {
            return 0;
        };
        let end = if self.closed {
            self.last_event_ms
        } else {
            now_ms.max(self.last_event_ms)
        };
        end.saturating_sub(first)
    }

    /// Foreground targets past `threshold_ms` in their current phase, longest
    /// first, capped at `limit`. Returns `(rows, total_over_threshold)` so the
    /// renderer can show a sample and an honest count.
    ///
    /// Scans only the in-flight map and never sorts more than `limit` rows: at
    /// 20k targets the previous implementation cloned and sorted every entry past
    /// the threshold on every 30-second render.
    pub(crate) fn running_longest(
        &self,
        now_ms: u64,
        threshold_ms: u64,
        limit: usize,
    ) -> (Vec<(String, &'static str, u64)>, usize) {
        let mut total = 0usize;
        let mut heap: BinaryHeap<Reverse<(u64, &str, &'static str)>> = BinaryHeap::new();
        for (addr, rec) in &self.live {
            let Some((phase, since)) = rec.phase else {
                continue;
            };
            if phase.is_background() {
                continue;
            }
            let elapsed = now_ms.saturating_sub(since);
            if elapsed < threshold_ms {
                continue;
            }
            total = total.saturating_add(1);
            if heap.len() < limit {
                heap.push(Reverse((elapsed, addr, phase.label())));
            } else if let Some(Reverse((min, _, _))) = heap.peek()
                && elapsed > *min
            {
                heap.pop();
                heap.push(Reverse((elapsed, addr, phase.label())));
            }
        }
        let mut rows: Vec<(String, &'static str, u64)> = heap
            .into_iter()
            .map(|Reverse((elapsed, addr, phase))| (addr.to_string(), phase, elapsed))
            .collect();
        rows.sort_by_key(|(addr, _, elapsed)| (Reverse(*elapsed), addr.clone()));
        (rows, total)
    }

    /// Lock waits past `threshold_ms`, longest first, capped at `limit`.
    pub(crate) fn lock_waits(
        &self,
        now_ms: u64,
        threshold_ms: u64,
        limit: usize,
    ) -> (Vec<LockWait>, usize) {
        let mut all: Vec<LockWait> = self
            .lock_waits
            .values()
            .filter(|w| now_ms.saturating_sub(w.since_ms) >= threshold_ms)
            .cloned()
            .collect();
        let total = all.len();
        all.sort_by_key(|w| (w.since_ms, w.addr.clone()));
        all.truncate(limit);
        (all, total)
    }

    /// The retained slowest completed targets, longest first.
    pub(crate) fn slowest(&self) -> Vec<Completed> {
        let mut v: Vec<Completed> = self.slowest.iter().map(|Reverse(c)| c.clone()).collect();
        v.sort_by(|a, b| b.cmp(a));
        v
    }

    /// Cache misses per package, most first, capped at `limit`. Returns the rows
    /// and the total package count.
    pub(crate) fn misses_by_package(&self, limit: usize) -> (Vec<(String, usize)>, usize) {
        let total = self.misses_by_pkg.len();
        let mut v: Vec<(String, usize)> = self
            .misses_by_pkg
            .iter()
            .map(|(k, n)| (k.to_string(), *n))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(limit);
        (v, total)
    }

    /// ⏳ until the stream closes, then ✅ or ❌. Progress counts do not drive
    /// this: a matched transparent group emits no `ResultEnd`, so `done == total`
    /// is unreliable as a "finished" signal — the stream closing is the
    /// authoritative one.
    pub(crate) fn status_emoji(&self) -> &'static str {
        if !self.closed {
            if self.counters.roots_total > 0 {
                "❌"
            } else {
                "⏳"
            }
        } else if self.counters.roots_total == 0 {
            "✅"
        } else {
            "❌"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent {
            at_unix_ms: at,
            kind,
        }
    }

    fn matched(t: &mut Tally, addrs: &[&str], complete: bool) {
        t.apply(&ev(
            0,
            BuildEventKind::Matched {
                addrs: addrs.iter().map(|s| (*s).to_string()).collect(),
                complete,
            },
        ));
    }

    fn exec(t: &mut Tally, addr: &str, start: u64, end: u64) {
        t.apply(&ev(
            start,
            BuildEventKind::ExecuteStart {
                addr: addr.into(),
                driver: "exec".into(),
                cache: false,
            },
        ));
        t.apply(&ev(
            end,
            BuildEventKind::ExecuteEnd {
                addr: addr.into(),
                error: None,
            },
        ));
        t.apply(&ev(
            end,
            BuildEventKind::ResultEnd {
                addr: addr.into(),
                error: None,
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
    }

    #[test]
    fn collateral_failures_fold_into_their_root() {
        // One broken leaf under 4k dependents must render as ONE failure with a
        // blocked count — not 4001 failures. This is the 20k-scale case.
        let mut t = Tally::default();
        t.apply(&ev(
            10,
            BuildEventKind::ResultEnd {
                addr: "//base:proto".into(),
                error: Some("execute //base:proto: process exited with status 1".into()),
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
        for i in 0..4_000 {
            t.apply(&ev(
                20,
                BuildEventKind::ResultEnd {
                    addr: format!("//services/api:t{i}"),
                    error: Some("dependency failed".into()),
                    upstream_of: Some("//base:proto".into()),
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }

        let c = t.counters();
        assert_eq!(c.roots_total, 1, "one root failure, not 4001");
        assert_eq!(c.blocked, 4_000, "collateral counted, not listed");
        assert_eq!(t.roots().len(), 1);
        let root = t.roots().first().expect("root");
        assert_eq!(root.addr, "//base:proto");
        assert_eq!(root.blocked, 4_000);
        assert_eq!(
            root.blocked_by_pkg.get("//services/api").copied(),
            Some(4_000),
            "blocked targets rolled up by package"
        );
    }

    #[test]
    fn duplicate_result_end_is_not_double_counted() {
        // The result memoizer keys on `(addr, outputs, is_top)`, so one addr can
        // emit several `ResultEnd`s.
        let mut t = Tally::default();
        matched(&mut t, &["//a:x"], true);
        for _ in 0..3 {
            t.apply(&ev(
                5,
                BuildEventKind::ResultEnd {
                    addr: "//a:x".into(),
                    error: Some("boom".into()),
                    upstream_of: None,
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }
        assert_eq!(t.counters().roots_total, 1, "root deduped by addr");
        assert_eq!(t.roots().len(), 1);
        let (done, total, _) = t.progress();
        assert_eq!((done, total), (1, 1), "done must never exceed total");
    }

    #[test]
    fn cache_hit_is_retracted_when_the_target_rebuilds() {
        // A manifest can outlive its blobs, so an announced hit can still rebuild.
        let mut t = Tally::default();
        t.apply(&ev(
            1,
            BuildEventKind::LocalCacheHit {
                addr: "//a:x".into(),
            },
        ));
        assert_eq!(t.counters().cached(), 1);
        t.apply(&ev(
            2,
            BuildEventKind::ExecuteStart {
                addr: "//a:x".into(),
                driver: "exec".into(),
                cache: false,
            },
        ));
        assert_eq!(t.counters().cached(), 0, "hit retracted off its counter");
        t.apply(&ev(
            3,
            BuildEventKind::ExecuteEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        let c = t.counters();
        assert_eq!(c.executed, 1);
        assert_eq!(
            c.executed + c.cached(),
            1,
            "executed + cached must not exceed the target count"
        );
    }

    #[test]
    fn cache_hit_after_execute_does_not_double_count() {
        // The reverse order: a second memoizer variant hits the cache the first
        // just populated.
        let mut t = Tally::default();
        t.apply(&ev(
            1,
            BuildEventKind::ExecuteStart {
                addr: "//a:x".into(),
                driver: "exec".into(),
                cache: false,
            },
        ));
        t.apply(&ev(
            2,
            BuildEventKind::ExecuteEnd {
                addr: "//a:x".into(),
                error: None,
            },
        ));
        t.apply(&ev(
            3,
            BuildEventKind::LocalCacheHit {
                addr: "//a:x".into(),
            },
        ));
        let c = t.counters();
        assert_eq!(c.cached(), 0, "an executed target is not also cached");
        assert_eq!(c.executed, 1);
    }

    #[test]
    fn local_and_remote_hits_are_counted_separately() {
        let mut t = Tally::default();
        t.apply(&ev(
            1,
            BuildEventKind::LocalCacheHit {
                addr: "//a:l".into(),
            },
        ));
        t.apply(&ev(
            2,
            BuildEventKind::RemoteCacheHit {
                addr: "//a:r".into(),
            },
        ));
        let c = t.counters();
        assert_eq!((c.cached_local, c.cached_remote), (1, 1));
        assert_eq!(c.cached(), 2);
    }

    #[test]
    fn queued_uploads_never_appear_as_slow_targets() {
        // `RemoteCacheWriteStart` fires before the upload semaphore is acquired,
        // so at 20k targets ~20k are "in flight" with 16 uploading. They must be
        // one aggregate number, never per-target rows.
        let mut t = Tally::default();
        for i in 0..5_000 {
            t.apply(&ev(
                0,
                BuildEventKind::RemoteCacheWriteStart {
                    addr: format!("//a:t{i}"),
                },
            ));
        }
        let (rows, total) = t.running_longest(600_000, 30_000, 6);
        assert!(rows.is_empty(), "background uploads are not slow targets");
        assert_eq!(total, 0);
        assert_eq!(t.background_ops(), 5_000, "surfaced as one aggregate count");
        assert_eq!(t.active_foreground(), 0, "not occupying worker slots");
    }

    #[test]
    fn running_longest_samples_without_sorting_everything() {
        let mut t = Tally::default();
        // 500 concurrently-executing targets, all past the threshold; the ones
        // that started earliest are the slowest.
        for i in 0..500u64 {
            t.apply(&ev(
                i,
                BuildEventKind::ExecuteStart {
                    addr: format!("//a:t{i}"),
                    driver: "exec".into(),
                    cache: false,
                },
            ));
        }
        let (rows, total) = t.running_longest(100_000, 30_000, 6);
        assert_eq!(rows.len(), 6, "sample capped");
        assert_eq!(total, 500, "but the true count is reported");
        // Longest first, and the longest is the earliest-started.
        assert_eq!(rows.first().map(|r| r.0.as_str()), Some("//a:t0"));
        assert!(
            rows.windows(2).all(|w| w[0].2 >= w[1].2),
            "rows ordered longest-first"
        );
    }

    #[test]
    fn slowest_completed_is_bounded_and_keeps_the_longest() {
        let mut t = Tally::default();
        // 1000 completions of increasing duration; only the top 20 are retained.
        for i in 0..1_000u64 {
            exec(&mut t, &format!("//a:t{i}"), 0, i);
        }
        let slowest = t.slowest();
        assert_eq!(slowest.len(), SLOWEST_KEPT, "heap bounded");
        assert_eq!(
            slowest.first().map(|c| c.duration_ms),
            Some(999),
            "longest first"
        );
        assert_eq!(
            slowest.last().map(|c| c.duration_ms),
            Some(980),
            "keeps the longest, not the first seen"
        );
        assert!(
            slowest
                .windows(2)
                .all(|w| w[0].duration_ms >= w[1].duration_ms),
            "ordered"
        );
    }

    #[test]
    fn progress_counts_only_matched_targets() {
        let mut t = Tally::default();
        matched(&mut t, &["//a:x", "//a:y"], true);
        // A dependency finishes — not matched, so it moves no progress.
        exec(&mut t, "//dep:d", 0, 5);
        let (done, total, complete) = t.progress();
        assert_eq!((done, total, complete), (0, 2, true));
        exec(&mut t, "//a:x", 0, 5);
        assert_eq!(t.progress().0, 1);
    }

    fn wait(t: &mut Tally, addr: &str, scratch: &str, start: u64, end: u64) {
        t.apply(&ev(
            start,
            BuildEventKind::ScratchLockWaitStart {
                addr: addr.into(),
                scratch: scratch.into(),
                access: "exclusive".into(),
                holder_pid: None,
            },
        ));
        t.apply(&ev(
            end,
            BuildEventKind::ScratchLockWaitEnd {
                addr: addr.into(),
                scratch: scratch.into(),
            },
        ));
    }

    /// Waits aggregate per cache, and the report orders by what the contention
    /// actually cost rather than by who waited most recently.
    #[test]
    fn scratch_waits_aggregate_per_cache_worst_first() {
        let mut t = Tally::default();
        wait(&mut t, "//a:x", "//build:gocache", 0, 1_000);
        wait(&mut t, "//a:y", "//build:gocache", 0, 2_000);
        wait(&mut t, "//a:z", "//build:gomodcache", 0, 10_000);

        assert_eq!(
            t.scratch_waits(),
            vec![
                ("//build:gomodcache", 1, 10_000),
                ("//build:gocache", 2, 3_000),
            ],
        );
    }

    /// An `End` with no `Start` is ignored rather than counted as a zero-length
    /// wait. Reachable two ways: a host older than these events, and a budgeted
    /// stream that dropped the `Start`.
    #[test]
    fn an_unmatched_scratch_wait_end_is_ignored() {
        let mut t = Tally::default();
        t.apply(&ev(
            5_000,
            BuildEventKind::ScratchLockWaitEnd {
                addr: "//a:x".into(),
                scratch: "//build:gocache".into(),
            },
        ));
        assert!(t.scratch_waits().is_empty(), "no wait was ever announced");
    }

    fn prepared(t: &mut Tally, addr: &str, scratch: &str, outcome: &str, mismatch: bool) {
        t.apply(&ev(
            0,
            BuildEventKind::ScratchPrepareStart {
                addr: addr.into(),
                scratch: scratch.into(),
            },
        ));
        t.apply(&ev(
            1,
            BuildEventKind::ScratchPrepareEnd {
                addr: addr.into(),
                scratch: scratch.into(),
                outcome: outcome.into(),
                bytes: 0,
                path_mismatch: mismatch,
                error: None,
            },
        ));
    }

    /// An inert restore is collected and deduped by cache — 50 consumers of one
    /// inert cache is one problem, not 50 identical warnings.
    #[test]
    fn an_inert_restore_is_reported_once_per_cache() {
        let mut t = Tally::default();
        for i in 0..50 {
            prepared(
                &mut t,
                &format!("//a:x{i}"),
                "//build:gocache",
                "pulled",
                true,
            );
        }
        assert_eq!(t.scratch_inert(), vec!["//build:gocache"]);
    }

    /// A dropped cache is reported once per cache, however many targets used it.
    #[test]
    fn a_dropped_cache_is_reported_once_per_cache() {
        let mut t = Tally::default();
        for i in 0..50 {
            prepared(
                &mut t,
                &format!("//a:y{i}"),
                "//build:gomodcache",
                "dropped_over_max",
                false,
            );
        }
        assert_eq!(t.scratch_dropped(), vec!["//build:gomodcache"]);
        assert!(
            t.scratch_inert().is_empty(),
            "a drop is not an inert restore"
        );
    }

    /// An ordinary outcome is not a problem — otherwise every cold CI run would
    /// raise a warning.
    #[test]
    fn a_normal_prepare_reports_no_problem() {
        let mut t = Tally::default();
        prepared(&mut t, "//a:x", "//build:gocache", "warm", false);
        prepared(&mut t, "//a:y", "//build:gocache", "pulled", false);
        assert!(t.scratch_inert().is_empty() && t.scratch_dropped().is_empty());
    }

    /// A target warming its cache shows as in-flight under its own phase label.
    /// It is on the critical path — it holds the slot guard and runs before the
    /// worker permit — so unlike a background upload it belongs in "running
    /// longest".
    #[test]
    fn a_scratch_prepare_is_a_foreground_phase() {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::ScratchPrepareStart {
                addr: "//a:x".into(),
                scratch: "//build:gocache".into(),
            },
        ));
        let (running, _) = t.running_longest(60_000, 1_000, 5);
        assert_eq!(running.len(), 1, "the target is in flight");
        assert_eq!(running[0].1, "scratch prepare", "under its own phase label");

        t.apply(&ev(
            1_000,
            BuildEventKind::ScratchPrepareEnd {
                addr: "//a:x".into(),
                scratch: "//build:gocache".into(),
                outcome: "warm".into(),
                bytes: 0,
                path_mismatch: false,
                error: None,
            },
        ));
        assert!(
            t.running_longest(60_000, 1_000, 5).0.is_empty(),
            "the phase cleared",
        );
    }

    #[test]
    fn counters_are_graph_wide() {
        // Decision recorded in docs/GHA_REPORTING.md §13.4: `executed` and
        // `cached` both count the whole graph, so the two are reconcilable
        // against each other even though progress is matched-only.
        let mut t = Tally::default();
        matched(&mut t, &["//a:top"], true);
        exec(&mut t, "//dep:one", 0, 1);
        exec(&mut t, "//dep:two", 0, 1);
        exec(&mut t, "//a:top", 0, 2);
        assert_eq!(t.counters().executed, 3, "dependencies included");
        assert_eq!(t.progress(), (1, 1, true), "progress stays matched-only");
    }

    #[test]
    fn misses_roll_up_by_package() {
        let mut t = Tally::default();
        for i in 0..30 {
            t.apply(&ev(
                0,
                BuildEventKind::LocalCacheMiss {
                    addr: format!("//services/api:t{i}"),
                },
            ));
        }
        for i in 0..10 {
            t.apply(&ev(
                0,
                BuildEventKind::LocalCacheMiss {
                    addr: format!("//web:t{i}"),
                },
            ));
        }
        let (rows, total_pkgs) = t.misses_by_package(10);
        assert_eq!(total_pkgs, 2);
        assert_eq!(
            rows.first().map(|(p, n)| (p.as_str(), *n)),
            Some(("//services/api", 30)),
            "most misses first"
        );
        assert_eq!(t.counters().misses(), 40);
    }

    #[test]
    fn hit_rate_is_none_when_the_cache_was_never_consulted() {
        let t = Tally::default();
        assert!(t.counters().hit_rate().is_none(), "0% would be a lie");
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::LocalCacheHit {
                addr: "//a:x".into(),
            },
        ));
        t.apply(&ev(
            0,
            BuildEventKind::LocalCacheMiss {
                addr: "//a:y".into(),
            },
        ));
        assert_eq!(t.counters().hit_rate(), Some(0.5));
    }

    #[test]
    fn lock_waits_surface_with_their_holder() {
        let mut t = Tally::default();
        t.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitStart {
                addr: "//a:x".into(),
                holder_pid: Some(4412),
            },
        ));
        let (waits, total) = t.lock_waits(60_000, 30_000, 5);
        assert_eq!(total, 1);
        assert_eq!(waits.first().and_then(|w| w.holder_pid), Some(4412));
        t.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitEnd {
                addr: "//a:x".into(),
            },
        ));
        assert_eq!(t.lock_waits(60_000, 30_000, 5).1, 0, "cleared on end");
    }

    #[test]
    fn status_settles_when_the_stream_closes() {
        let mut t = Tally::default();
        // A matched transparent group emits no `ResultEnd`, so `done == total`
        // can never be reached; the stream closing is the authoritative signal.
        matched(&mut t, &["//a:grp"], false);
        assert_eq!(t.status_emoji(), "⏳");
        t.set_closed();
        assert_eq!(t.status_emoji(), "✅");
    }

    #[test]
    fn a_failure_shows_as_failed_while_still_running() {
        // Diagnosing before the end is the point: the status must go red the
        // moment a root failure lands, not at close.
        let mut t = Tally::default();
        matched(&mut t, &["//a:x", "//a:y"], true);
        assert_eq!(t.status_emoji(), "⏳");
        t.apply(&ev(
            5,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: Some("boom".into()),
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
        assert_eq!(t.status_emoji(), "❌", "red before the build finishes");
    }

    #[test]
    fn roots_are_capped_but_still_counted() {
        let mut t = Tally::default();
        for i in 0..200 {
            t.apply(&ev(
                0,
                BuildEventKind::ResultEnd {
                    addr: format!("//a:t{i}"),
                    error: Some("boom".into()),
                    upstream_of: None,
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }
        assert_eq!(t.roots().len(), ROOTS_KEPT, "detail capped");
        assert_eq!(t.counters().roots_total, 200, "count is exact");
    }

    #[test]
    fn roots_keep_first_seen_order() {
        // Consecutive renders must differ by appends only, so an agent diffing
        // two fetches sees "one new root" rather than a reshuffle.
        let mut t = Tally::default();
        for addr in ["//c:z", "//a:x", "//b:y"] {
            t.apply(&ev(
                0,
                BuildEventKind::ResultEnd {
                    addr: addr.into(),
                    error: Some("boom".into()),
                    upstream_of: None,
                    exit_status: None,
                    log_tail: None,
                },
            ));
        }
        let order: Vec<&str> = t.roots().iter().map(|r| r.addr.as_str()).collect();
        assert_eq!(order, vec!["//c:z", "//a:x", "//b:y"], "not re-sorted");
    }

    #[test]
    fn a_root_failure_carries_its_exit_status_and_log_tail() {
        // The whole point of the structured event: the report can show what
        // actually went wrong instead of one line of a flattened cause chain.
        let mut t = Tally::default();
        t.apply(&ev(
            10,
            BuildEventKind::ResultEnd {
                addr: "//services/api:test".into(),
                error: Some("target failed: //services/api:test".into()),
                upstream_of: None,
                exit_status: Some("exit status: 1".into()),
                log_tail: Some(LogTailData {
                    text: "--- FAIL: TestCreateUser (0.03s)\n    want 201, got 500".into(),
                    start_line: 88,
                }),
            },
        ));
        let root = t.roots().first().expect("root recorded");
        assert_eq!(root.exit_status.as_deref(), Some("exit status: 1"));
        assert!(
            root.log_tail
                .as_ref()
                .is_some_and(|l| l.text.contains("TestCreateUser")),
            "log tail reaches the reporter"
        );
    }

    #[test]
    fn package_derivation() {
        assert_eq!(package_of("//services/api:test"), "//services/api");
        assert_eq!(package_of("//:root"), "//");
        // No colon at all — the whole thing is the package rather than a panic.
        assert_eq!(package_of("//services/api"), "//services/api");
    }

    #[test]
    fn elapsed_is_bounded_by_the_stream_once_closed() {
        let mut t = Tally::default();
        t.apply(&ev(
            1_000,
            BuildEventKind::RequestConfig {
                max_workers: 16,
                fail_fast: false,
                scratch_disabled: false,
            },
        ));
        t.apply(&ev(
            5_000,
            BuildEventKind::ResultEnd {
                addr: "//a:x".into(),
                error: None,
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
        ));
        // Still running: elapsed tracks now.
        assert_eq!(t.elapsed_ms(9_000), 8_000);
        t.set_closed();
        // Closed: elapsed freezes at the last event, not at render time.
        assert_eq!(t.elapsed_ms(999_000), 4_000);
    }
}
