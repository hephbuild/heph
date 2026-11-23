//! Client-side aggregator for the engine's build-progress event stream.
//!
//! [`BuildState`] folds [`BuildEvent`]s into running/completed/errored counts,
//! cache hit/miss statistics, and a list of targets that are currently executing.
//! It uses only the server-stamped `at_unix_ms` off each event — never a local
//! receipt clock — so elapsed times stay correct across a future client/server
//! process split. Rendering callers pass their own `now_ms` wall clock.

use std::collections::{BTreeMap, HashMap, HashSet};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::app::{CIAppView, TUIAppView};
use crate::tui::session::{HolderNames, SessionId};
use hcore::capacity::Unit;
use hcore::events::{BuildEvent, BuildEventKind, CacheCounts};

#[cfg(test)]
mod approval_tests;
mod line;
mod spotlight;
#[cfg(test)]
pub(crate) mod test_events;

pub use spotlight::{HOLD_MS, Spot};

/// The dashboard's key hints once the build has ended and the dashboard
/// holds it on screen: nothing is left to stop, and `q` leaves.
/// The most list rows `heph session show` prints.
pub const SNAPSHOT_ROWS: u16 = 15;

const FINISHED_DASHBOARD_HINTS: &str = "↑/↓ select · enter details · l log · tab/⇧tab list · / filter · a all targets · q or esc leave";

/// A target is considered "taking long" once its execute span exceeds this.
pub const LONG_RUNNING_THRESHOLD_MS: u64 = 5_000;

/// Display width the elapsed clock pads to once it leaves the seconds band.
/// The seconds band renders natural (2–3 chars) so a fresh run starts compact;
/// from a minute on it pads to this so the field only ever grows, never shrinks
/// (e.g. `59m59s` → `1h00m` would otherwise lose a column and jitter the counts).
const ELAPSED_MIN_WIDTH: usize = 6;

/// Compact, human-friendly elapsed time. Precision is indicative only — the
/// coarsest two units are shown: `12s`, `1m05s`, `1h05m`, `2d03h`. The seconds
/// band is natural width (compact start); everything past a minute is padded to
/// [`ELAPSED_MIN_WIDTH`] so the field grows monotonically and never flickers.
fn human_elapsed(ms: u64) -> String {
    let s = hcore::units::human_duration(std::time::Duration::from_millis(ms));
    if ms / 1000 < 60 {
        return s;
    }
    format!("{s:>ELAPSED_MIN_WIDTH$}")
}

/// Like [`human_elapsed`] but the sub-minute band keeps millisecond precision
/// (`12.345s`). Used by the final report where the live flicker concern that
/// drives [`human_elapsed`]'s coarse seconds band does not apply.
fn human_elapsed_ms(ms: u64) -> String {
    if ms / 1000 < 60 {
        return format!("{}.{:03}s", ms / 1000, ms % 1000);
    }
    human_elapsed(ms)
}

/// Case-insensitive substring test for the `/` filter on the `Done`/`Failed`
/// bodies. An empty filter matches everything (the unfiltered list).
///
/// Compares in place. Lower-casing both sides allocated two `String`s per row,
/// and the filter has to be tried against *every* row to know how many matched —
/// so at 100k targets that was 200k allocations on every frame.
pub(crate) fn addr_matches(addr: &str, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    // ASCII-only folding, matching what the two `to_ascii_lowercase` calls did.
    // `windows` yields nothing when the filter is longer than the addr, and the
    // empty filter is handled above, so the width is always non-zero.
    addr.as_bytes()
        .windows(filter.len())
        .any(|w| w.eq_ignore_ascii_case(filter.as_bytes()))
}

/// Which target set the header counters are scoped to. `Matched` (the default)
/// counts only the matched top-level targets; `All` counts every target the view
/// has observed, including transitive deps. The `a` key toggles between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CountScope {
    /// Counters scoped to the matched top-level target set.
    #[default]
    Matched,
    /// Counters across every target the view has seen (incl. transitive deps).
    All,
}

/// A target-scoped operation, as the client groups it for display. This is a
/// purely client-side (rendering) concept: the engine emits individually typed
/// events (`ExecuteStart/End`, `LocalCacheWriteStart/End`, …) and
/// [`event_op_boundary`] collapses those into this timeline. Add a variant here
/// (plus a mapping arm) when a new typed span event should appear in the
/// breakdown — e.g. remote read/write once their events land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Op {
    RemoteCacheRead,
    /// Warming a scratch directory — seeding a lineage from a fallback scope, or
    /// pulling a snapshot from the remote.
    ScratchPrepare,
    Execute,
    LocalCacheWrite,
    RemoteCacheWrite,
}

impl Op {
    /// Single glyph shown before the op's elapsed time. Must stay **BMP
    /// single-width** (plain-text rows; a double-width glyph would clip). Remote
    /// ops use `↓`/`↑` (U+2193/2191), never the emoji `⬇`/`⬆`.
    pub(crate) fn icon(self) -> char {
        match self {
            Op::RemoteCacheRead => '↓',
            Op::ScratchPrepare => '↺',
            Op::Execute => '▶',
            Op::LocalCacheWrite => '⊕',
            Op::RemoteCacheWrite => '↑',
        }
    }

    /// The op's name in a [`crate::tui::session::SessionSnapshot`]. Part of the
    /// snapshot's JSON: renaming one is a format change.
    fn name(self) -> &'static str {
        match self {
            Op::RemoteCacheRead => "remote_cache_read",
            Op::ScratchPrepare => "scratch_prepare",
            Op::Execute => "execute",
            Op::LocalCacheWrite => "local_cache_write",
            Op::RemoteCacheWrite => "remote_cache_write",
        }
    }

    /// Pipeline ordinal for stable left-to-right ordering of the breakdown:
    /// remote download → scratch warm → execute → local-cache write → remote
    /// upload. This is the order the engine runs them in, so a row reads
    /// left-to-right as the target's own history.
    pub(crate) fn order(self) -> u8 {
        match self {
            Op::RemoteCacheRead => 0,
            Op::ScratchPrepare => 1,
            Op::Execute => 2,
            Op::LocalCacheWrite => 3,
            Op::RemoteCacheWrite => 4,
        }
    }

    /// Every op, indexed by [`Op::order`].
    pub(crate) const BY_ORDER: [Op; 5] = [
        Op::RemoteCacheRead,
        Op::ScratchPrepare,
        Op::Execute,
        Op::LocalCacheWrite,
        Op::RemoteCacheWrite,
    ];

    /// The dashboard's PHASE words for a target in this op.
    pub(crate) fn phase(self) -> &'static str {
        match self {
            Op::RemoteCacheRead => "download",
            Op::ScratchPrepare => "scratch",
            Op::Execute => "execute",
            Op::LocalCacheWrite => "cache write",
            Op::RemoteCacheWrite => "upload",
        }
    }
}

/// Which edge of an [`Op`] span an event represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boundary {
    Start,
    End,
}

/// Map an individually-typed engine event to the op-timeline boundary it
/// represents, if any. This is where the well-defined engine events are collapsed
/// into the shared per-target timeline; a new op needs one arm here.
pub(crate) fn event_op_boundary(kind: &BuildEventKind) -> Option<(&str, Op, Boundary)> {
    match kind {
        BuildEventKind::ExecuteStart { addr, .. } => Some((addr, Op::Execute, Boundary::Start)),
        BuildEventKind::ExecuteEnd { addr, .. } => Some((addr, Op::Execute, Boundary::End)),
        BuildEventKind::LocalCacheWriteStart { addr } => {
            Some((addr, Op::LocalCacheWrite, Boundary::Start))
        }
        BuildEventKind::LocalCacheWriteEnd { addr, .. } => {
            Some((addr, Op::LocalCacheWrite, Boundary::End))
        }
        BuildEventKind::RemoteCacheWriteStart { addr } => {
            Some((addr, Op::RemoteCacheWrite, Boundary::Start))
        }
        BuildEventKind::RemoteCacheWriteEnd { addr, .. } => {
            Some((addr, Op::RemoteCacheWrite, Boundary::End))
        }
        BuildEventKind::ScratchPrepareStart { addr, .. } => {
            Some((addr, Op::ScratchPrepare, Boundary::Start))
        }
        BuildEventKind::ScratchPrepareEnd { addr, .. } => {
            Some((addr, Op::ScratchPrepare, Boundary::End))
        }
        BuildEventKind::RemoteCacheReadStart { addr } => {
            Some((addr, Op::RemoteCacheRead, Boundary::Start))
        }
        BuildEventKind::RemoteCacheReadEnd { addr, .. } => {
            Some((addr, Op::RemoteCacheRead, Boundary::End))
        }
        _ => None,
    }
}

/// Folds the engine's build-progress event stream into renderable state.
#[derive(Debug, Default)]
///
/// Per target it keeps nothing of its own: every target's state, op times,
/// matched and cached bits live in [`Self::dash`]'s fixed-size records,
/// named by an interned id. What is keyed by address here is bounded by
/// what is waiting or running right now (the wait maps, the clamps, the
/// spotlight's orderings), or by the caches a build declares.
pub struct BuildState {
    /// `ResultEnd`s of targets that had started, by outcome: no error,
    /// an error, a stop. Counted once each, as the model's in-flight bit
    /// says.
    completed: usize,
    errored: usize,
    /// Targets a stop ended (`ResultEnd.cancelled`): not `errored`, not
    /// `completed`.
    cancelled: usize,
    /// Targets whose driver actually ran to success (`ExecuteEnd` with no error).
    /// Distinct from `completed`, which includes cache hits that never executed.
    executed: usize,
    /// Per-target cache accounting, folded from `CacheOutcome` and
    /// `CacheHitRebuilt` — the same fold the GHA report and telemetry use, so
    /// the summary line agrees with them. The tier events are not counted.
    cache: CacheCounts,
    /// Server timestamp of the first event seen — the run's start anchor for the
    /// header's elapsed clock. `None` until any event lands.
    started_at_ms: Option<u64>,
    /// Server timestamp of the latest event seen — how long the run has been
    /// quiet, for a session viewer. `None` until any event lands.
    last_event_ms: Option<u64>,
    /// Addrs blocked on the result lock past the notice threshold, mapped to
    /// what is known of the holder and when the wait was announced. Added on
    /// `ResultLockWaitStart`, removed on `ResultLockWaitEnd`, so it reflects
    /// only currently-blocked waits.
    lock_waits: HashMap<String, (LockHolder, u64)>,
    /// The session behind each holder pid a wait named, resolved once per pid
    /// when its wait starts and only read when rendering. Given no root (the
    /// default: a test, the recorder's own fold), it names none.
    holders: HolderNames,
    /// Consumers blocked on a scratch slot, keyed by the *consumer* addr so a
    /// wait can be removed when its own `ScratchLockWaitEnd` lands.
    ///
    /// Rendered collapsed by scratch declaration — see [`scratch_wait_lines`].
    /// One `exclusive` cache shared by hundreds of targets produces that many
    /// simultaneous waiters, and that many identical rows is a flooded terminal,
    /// not a diagnostic.
    ///
    /// [`scratch_wait_lines`]: BuildState::scratch_wait_lines
    scratch_waits: HashMap<String, ScratchWait>,
    /// Finished scratch waits, aggregated per cache as `(waiters, total ms)`.
    ///
    /// Kept after the wait ends because the *total* is the number worth
    /// reporting: one target blocked for two seconds is noise, but 47 targets
    /// blocked for three minutes between them is the whole reason the build was
    /// slow, and by the time anyone reads the summary every wait has ended.
    scratch_wait_totals: HashMap<String, (u64, u64)>,
    /// Every pool's unit as `RequestConfig` announced it, for the words of a
    /// clamp and an admission wait. What a pool holds is its own report
    /// (`PoolUsage`, in [`Self::dash`]), never a sum of reservations.
    units: BTreeMap<String, Unit>,
    /// Targets whose admission has waited past the notice threshold, by
    /// addr. Added on `AdmissionWaitStart`, removed on `AdmissionWaitEnd`.
    admission_waits: HashMap<String, AdmissionWait>,
    /// The line's spotlight: the waits and runs ordered by start, the root
    /// failure on show, and the hold. See [`spotlight`].
    spot: spotlight::SpotIndex,
    /// Failures with no `upstream_of`: the roots. `errored` less this is
    /// what they took down, as the finished line splits it.
    root_failed: usize,
    /// heph's home, redacted from every error text this state keeps (I9).
    redact: crate::tui::text::Redact,
    /// The dashboard's fold: one record per target in per-state buckets,
    /// and each bounded pool's latest `PoolUsage`. The one source of the
    /// line's and the box's counts and gauges.
    dash: crate::tui::dashboard::Model,
}

/// A root failure's reason, as the line's spotlight and the dashboard's
/// PHASE column show it: `exit 101`, `timed out 10m`, or the error's first
/// line, cleaned and redacted.
pub(crate) fn failure_reason(
    error: &str,
    exit_status: Option<&str>,
    timed_out: Option<&hcore::events::TimedOutData>,
    redact: &crate::tui::text::Redact,
) -> String {
    spotlight::RootFailure::reason(error, exit_status, timed_out, redact)
}

/// How a finished run ended: the final line's mark and the dashboard title
/// row's verdict both say this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    /// A user stop, whatever finished before it.
    Stopped,
}

/// The finished line's split of the failed count, from
/// [`BuildState::failure_split`]: ` (1 root, 2 because of it)`, ` (2 roots)`;
/// empty when nothing failed.
fn failure_split_text((roots, took_down): (usize, usize)) -> String {
    let named = match roots {
        0 => None,
        1 => Some("1 root".to_owned()),
        n => Some(format!("{n} roots")),
    };
    let blame = match roots {
        0 => "a dependency",
        1 => "it",
        _ => "them",
    };
    match (named, took_down) {
        (None, 0) => String::new(),
        (Some(named), 0) => format!(" ({named})"),
        (Some(named), n) => format!(" ({named}, {n} because of {blame})"),
        (None, n) => format!(" ({n} because of {blame})"),
    }
}

/// One target's announced admission wait.
#[derive(Debug, Clone)]
struct AdmissionWait {
    pool: String,
    need: u64,
    free: u64,
    ahead: u64,
    removals_pending: Option<u64>,
    /// The oldest waiter's need this one is held back for, when it is.
    yielding_to: Option<hcore::events::YieldingToData>,
    since_ms: u64,
}

/// One consumer's blocked wait on a scratch slot.
#[derive(Debug, Clone)]
struct ScratchWait {
    /// The scratch declaration's addr — the subject of the rendered row, since
    /// the cache is what the user would change, not the target that wanted it.
    scratch: String,
    /// A process holding the slot, when one could be named.
    holder_pid: Option<u32>,
    /// When this consumer started waiting, for the row's elapsed clock.
    since_ms: u64,
}

/// The header's count fields for one frame, as rendered strings.
///
/// Every consumer — the split header, the plain-text summary — goes through
/// [`BuildState::counts`]. The done count is kept split from its denominator
/// because the header binds each half to a different view tab, while the
/// plain-text summary wants them joined; carrying both shapes in one value is
/// what lets the two renderings share a single computation and stay in step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    /// The finished count, bound to the [`ViewMode::Done`] tab.
    pub done: String,
    /// The denominator, bound to the [`ViewMode::Matched`] tab and `~`-prefixed
    /// while the matcher is still streaming. `None` before any `Matched` event,
    /// where the segment reads `{done} done` with no denominator (and `done` is
    /// the executed count, not a matched count).
    pub total: Option<String>,
    /// Cache hits in scope, e.g. `"3 cached"`.
    pub cached: String,
    /// Targets held at admission right now, e.g. `"2 waiting"`. Live-only: a
    /// finished run has nothing waiting, so the final summary leaves it out.
    pub waiting: String,
    /// Failures, e.g. `"0 failed"`.
    pub failed: String,
}

impl Counts {
    /// The done segment as one string: `"{done} / {total} done"`, or
    /// `"{done} done"` when there is no denominator.
    pub fn done_segment(&self) -> String {
        match &self.total {
            Some(total) => format!("{} / {total} done", self.done),
            None => format!("{} done", self.done),
        }
    }
}

impl BuildState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Redact `redact`'s paths from every error text kept from here on.
    pub fn set_redact(&mut self, redact: crate::tui::text::Redact) {
        self.redact = redact;
    }

    /// `(roots, took down)`: the failures that are their own, and those
    /// that failed only because a dependency did.
    pub fn failure_split(&self) -> (usize, usize) {
        (
            self.root_failed,
            self.errored.saturating_sub(self.root_failed),
        )
    }

    /// Something failed, or a stop ended a target: the finished line's `✗`.
    pub fn unsuccessful(&self) -> bool {
        self.errored > 0 || self.cancelled > 0
    }

    /// How to name the session behind a lock holder's pid. See
    /// [`HolderNames`].
    pub fn name_lock_holders(&mut self, names: HolderNames) {
        self.holders = names;
    }

    /// A lock holder as a notice names it: `session <id> (pid N)`, or `pid N`
    /// when no live session is recorded for it. Reads what the backend's
    /// receiver resolved before the fold; never does IO, so it is safe on
    /// every frame.
    fn holder(&self, pid: u32) -> String {
        holder_text(pid, self.holders.get(pid))
    }

    /// Fold a single event. Reads `ev.at_unix_ms` + `ev.kind`; no local clock.
    ///
    /// Idempotent against the rare same-addr/different-`OutputMatcher`
    /// double-fire: `ResultStart` inserts only if absent, and a second
    /// `ResultEnd` for an already-removed addr is a no-op.
    pub fn apply(&mut self, ev: &BuildEvent) {
        // Anchor the elapsed clock to the earliest event observed.
        self.started_at_ms = Some(match self.started_at_ms {
            Some(t) => t.min(ev.at_unix_ms),
            None => ev.at_unix_ms,
        });
        self.last_event_ms = Some(
            self.last_event_ms
                .map_or(ev.at_unix_ms, |t| t.max(ev.at_unix_ms)),
        );
        // The op timeline is the model's (one record per target); what this
        // state keeps of it is the spotlight's run order, which follows the
        // record's open op across the fold. A `ResultEnd` closes it too.
        let op_addr = match &ev.kind {
            BuildEventKind::ResultEnd { addr, .. } => Some(addr.as_str()),
            kind => event_op_boundary(kind).map(|(addr, ..)| addr),
        };
        let open_before = op_addr.and_then(|addr| self.dash.open_since(addr));
        // Counted once per started target: the model clears the bit below.
        let was_in_flight = match &ev.kind {
            BuildEventKind::ResultEnd { addr, .. } => self.dash.in_flight(addr),
            _ => false,
        };
        self.dash.apply(ev, &self.redact);
        if let Some(addr) = op_addr {
            let open_after = self.dash.open_since(addr);
            if open_before != open_after {
                if let Some(at) = open_before {
                    self.spot.running.remove(&(at, addr.to_owned()));
                }
                if let Some(at) = open_after {
                    self.spot.running.insert((at, addr.to_owned()));
                }
            }
        }
        match &ev.kind {
            BuildEventKind::RequestConfig {
                resources, units, ..
            } => {
                for pool in resources.keys() {
                    self.units
                        .insert(pool.clone(), hcore::events::unit_of(units, pool));
                }
            }
            // Folded by `dash`: the matched bit and the in-flight bit.
            BuildEventKind::Matched { .. } | BuildEventKind::ResultStart { .. } => {}
            BuildEventKind::ResultEnd {
                addr,
                error,
                upstream_of,
                exit_status,
                timed_out,
                cancelled,
                ..
            } => {
                // A root failure — one no dependency caused — takes the
                // spotlight until a newer one does. Collateral ones never do,
                // nor a stop: Ctrl-C is not a failure to explain.
                if let Some(err) = error
                    && upstream_of.is_none()
                    && !cancelled
                    && self
                        .spot
                        .root_failure
                        .as_ref()
                        .is_none_or(|f| f.is_replaced_by(ev.at_unix_ms, addr))
                {
                    self.spot.root_failure = Some(spotlight::RootFailure {
                        at_ms: ev.at_unix_ms,
                        addr: addr.clone(),
                        reason: spotlight::RootFailure::reason(
                            err,
                            exit_status.as_deref(),
                            timed_out.as_ref(),
                            &self.redact,
                        ),
                    });
                }
                if was_in_flight {
                    if error.is_some() && *cancelled {
                        // Stopped, not failed: in neither count, and not
                        // a row of the Failed tab.
                        self.cancelled += 1;
                    } else if error.is_some() {
                        self.errored += 1;
                        if upstream_of.is_none() {
                            self.root_failed += 1;
                        }
                    } else {
                        self.completed += 1;
                    }
                }
            }
            // The op timeline (folded above) tracks Execute's duration; here we
            // keep only the `executed` counter side effect on a successful end.
            // A hit that rebuilds is retracted by its `CacheHitRebuilt`.
            BuildEventKind::ExecuteStart { addr, .. } => {
                // A target that runs is no longer waiting. The engine sends
                // `AdmissionWaitEnd` first; this keeps a lost End from leaving
                // the row in the Waiting tab for the rest of the run.
                self.end_admission_wait(addr);
            }
            BuildEventKind::ExecuteEnd { error, .. } => {
                if error.is_none() {
                    self.executed += 1;
                }
            }
            BuildEventKind::AdmissionWaitStart {
                addr,
                pool,
                need,
                free,
                ahead,
                removals_pending,
                yielding_to,
            } => {
                // A repeated Start replaces the wait, so it counts once.
                self.end_admission_wait(addr);
                self.admission_waits.insert(
                    addr.clone(),
                    AdmissionWait {
                        pool: pool.clone(),
                        need: *need,
                        free: *free,
                        ahead: *ahead,
                        removals_pending: *removals_pending,
                        yielding_to: yielding_to.clone(),
                        since_ms: ev.at_unix_ms,
                    },
                );
            }
            BuildEventKind::AdmissionWaitEnd { addr } => {
                self.end_admission_wait(addr);
            }
            // Tier events: per tier, not per target, so never a count here (a
            // remote hit is a local miss first). See `hcore::events`.
            BuildEventKind::LocalCacheHit { .. }
            | BuildEventKind::LocalCacheMiss { .. }
            | BuildEventKind::RemoteCacheHit { .. }
            | BuildEventKind::RemoteCacheMiss { .. } => {}
            // The per-target hit bit is the model's; a retracted hit clears it.
            BuildEventKind::CacheOutcome { .. } => {
                self.cache.apply(&ev.kind);
            }
            BuildEventKind::CacheHitRebuilt { .. } => {
                // A rebuild whose `was` this build does not know moves nothing
                // in the fold.
                if CacheCounts::delta(&ev.kind).is_some() {
                    self.cache.apply(&ev.kind);
                }
            }
            BuildEventKind::ResultLockWaitStart {
                addr,
                holder_pid,
                in_use_by_readers,
            } => {
                if let Some((_, since)) = self.lock_waits.insert(
                    addr.clone(),
                    (
                        LockHolder::of(*holder_pid, *in_use_by_readers),
                        ev.at_unix_ms,
                    ),
                ) {
                    self.spot.lock.remove(&(since, addr.clone()));
                }
                self.spot.lock.insert((ev.at_unix_ms, addr.clone()));
            }
            BuildEventKind::ResultLockWaitEnd { addr } => {
                if let Some((_, since)) = self.lock_waits.remove(addr) {
                    self.spot.lock.remove(&(since, addr.clone()));
                }
            }
            BuildEventKind::ScratchLockWaitStart {
                addr,
                scratch,
                holder_pid,
                ..
            } => {
                if let Some(prev) = self.scratch_waits.insert(
                    addr.clone(),
                    ScratchWait {
                        scratch: scratch.clone(),
                        holder_pid: *holder_pid,
                        since_ms: ev.at_unix_ms,
                    },
                ) {
                    self.unindex_scratch_wait(addr, &prev);
                }
                self.spot.scratch.insert((ev.at_unix_ms, addr.clone()));
                self.spot
                    .by_cache
                    .entry(scratch.clone())
                    .or_default()
                    .insert((ev.at_unix_ms, addr.clone()));
            }
            // The prepare span is folded into the op timeline above, and its
            // outcome is reported by the CI view rather than counted here — a
            // seeded or pulled cache is not a cache *hit*, and putting it on the
            // hit counters would overstate reuse.
            BuildEventKind::ScratchPrepareStart { .. }
            | BuildEventKind::ScratchPrepareEnd { .. } => {}
            BuildEventKind::ScratchLockWaitEnd { addr, .. } => {
                if let Some(w) = self.scratch_waits.remove(addr) {
                    self.unindex_scratch_wait(addr, &w);
                    let e = self.scratch_wait_totals.entry(w.scratch).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += ev.at_unix_ms.saturating_sub(w.since_ms);
                }
            }
            // Read/Write markers are not aggregated into counters. The local
            // cache-write span is folded into the op timeline above, so the
            // counter match ignores it here.
            BuildEventKind::RemoteCacheReadStart { .. }
            | BuildEventKind::RemoteCacheReadEnd { .. }
            | BuildEventKind::RemoteCacheWriteStart { .. }
            | BuildEventKind::RemoteCacheWriteEnd { .. }
            | BuildEventKind::LocalCacheWriteStart { .. }
            | BuildEventKind::LocalCacheWriteEnd { .. } => {}
            // GC progress is tracked by GcHeader, not the build counters. The
            // elapsed-clock anchor at the top of `apply` still runs, so the
            // clock works during a gc sweep.
            BuildEventKind::GcTargetSwept { .. }
            | BuildEventKind::GcFailureLogsSwept { .. }
            | BuildEventKind::GcCacheDb { .. } => {}
            // Emitted once at the end, after the live view; the command prints
            // its own "incomplete selection" block.
            BuildEventKind::SelectionIncomplete { .. } => {}
            // Codegen index diagnostics are log lines, not build counters.
            BuildEventKind::CodegenExcluded { .. } | BuildEventKind::CodegenImported { .. } => {}
            // Folded by `dash`: the gauges are the pool's own report.
            BuildEventKind::PoolUsage { .. } => {}
            // The Outputs tab's groups; nothing draws the tab yet.
            BuildEventKind::ResultOutputs { .. } => {}
            // An event kind newer than this build knows about. Skipped rather
            // than fatal — see `BuildEventKind::Unknown`.
            BuildEventKind::Unknown => {}
        }
        self.fold_spotlight(ev.at_unix_ms);
    }

    /// Number of matched top-level targets seen so far (provisional until the
    /// matcher fully resolves). `None` if no `Matched` event has arrived.
    pub fn matched_total(&self) -> Option<usize> {
        self.dash.matched_total()
    }

    /// Matched-target progress as `(done, total, complete)`: how many matched
    /// top-level targets have finished, the total matched so far, and whether the
    /// matcher fully resolved (`false` ⇒ the total is provisional). `None` until
    /// a `Matched` event arrives (e.g. a single-target `result_addr` entry).
    ///
    /// `O(1)`: bucket lengths in the model.
    pub fn matched_progress(&self) -> Option<(usize, usize, bool)> {
        if !self.dash.matched_seen() {
            return None;
        }
        let tabs = self.dash.counts(CountScope::Matched);
        Some((tabs.ended(), tabs.all(), self.dash.matched_complete()))
    }

    /// The count fields for one frame — see [`Counts`]. `O(1)`: every field
    /// is a bucket length in the dashboard's fold, the one source of the
    /// line's, the box's and the tabs' counts.
    pub fn counts(&self, scope: CountScope) -> Counts {
        self.counts_of(&self.dash.counts(scope), scope)
    }

    /// [`Counts`] from the tabs' counts. Done is Done + Cached (a failure is
    /// not done); the total is every target in scope, `~`-prefixed while
    /// the matcher streams. Before any `Matched` event the matched scope has
    /// no total: `{done} done`.
    fn counts_of(&self, tabs: &crate::tui::dashboard::TabCounts, scope: CountScope) -> Counts {
        let total = match scope {
            // Every observed target: it grows as deps stream rather than
            // resolving to a fixed set, so it is never provisional.
            CountScope::All => Some(tabs.all().to_string()),
            CountScope::Matched if self.dash.matched_seen() => {
                let tilde = if self.dash.matched_complete() {
                    ""
                } else {
                    "~"
                };
                Some(format!("{tilde}{}", tabs.all()))
            }
            CountScope::Matched => None,
        };
        Counts {
            done: tabs.finished_ok().to_string(),
            total,
            cached: format!("{} cached", tabs.cached),
            waiting: format!("{} waiting", tabs.blocked),
            failed: format!("{} failed", tabs.failed_total()),
        }
    }

    /// The three freestanding count fields — `(done, cached, failed)` — each
    /// without separators. The plain-text rendering of [`BuildState::counts`],
    /// used by [`BuildState::counts_segment`] for the final summary; the live
    /// header takes [`Counts`] directly so it can split the done segment into
    /// per-view tabs.
    pub fn count_fields(&self, scope: CountScope) -> (String, String, String) {
        let counts = self.counts(scope);
        (counts.done_segment(), counts.cached, counts.failed)
    }

    /// The final summary's count segment: `D / ~N done · C cached · F
    /// failed`, the failures split into roots and what they took down
    /// (`3 failed (1 root, 2 because of it)`). No elapsed clock, no worker
    /// braille — callers prepend the elapsed field themselves.
    pub fn counts_segment(&self, scope: CountScope) -> String {
        let (done, cached, failed) = self.count_fields(scope);
        // Split in the same scope as the count it splits.
        let tabs = self.dash.counts(scope);
        let split = failure_split_text((tabs.failed, tabs.dep_failed));
        format!("{done} · {cached} · {failed}{split}")
    }

    /// Drop `addr`'s announced admission wait, if any.
    fn end_admission_wait(&mut self, addr: &str) {
        self.admission_waits.remove(addr);
    }

    /// Take a scratch wait that ended or was replaced out of the spotlight's
    /// order, dropping its cache's group once the last waiter leaves it.
    fn unindex_scratch_wait(&mut self, addr: &str, w: &ScratchWait) {
        let key = (w.since_ms, addr.to_owned());
        self.spot.scratch.remove(&key);
        if let Some(group) = self.spot.by_cache.get_mut(&w.scratch) {
            group.remove(&key);
            if group.is_empty() {
                self.spot.by_cache.remove(&w.scratch);
            }
        }
    }

    /// Pool `pool`'s unit: as announced, else its default.
    fn unit_of(&self, pool: &str) -> Unit {
        self.units
            .get(pool)
            .copied()
            .unwrap_or_else(|| Unit::default_for(pool))
    }

    /// The dashboard's fold: records, buckets and gauges.
    pub fn dashboard(&self) -> &crate::tui::dashboard::Model {
        &self.dash
    }

    /// Milliseconds since the first observed event, given the caller's wall
    /// clock. `0` before any event has been seen.
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        self.started_at_ms
            .map(|start| now_ms.saturating_sub(start))
            .unwrap_or(0)
    }

    /// Whether any build activity was observed. Used to suppress an all-zero
    /// final summary for commands that emit no build events (inspect/query).
    pub fn has_activity(&self) -> bool {
        self.dash.matched_seen()
            || self.completed > 0
            || self.errored > 0
            || self.cancelled > 0
            || self.dash.in_flight_count() > 0
            || self.cache.consulted() > 0
    }

    /// One-line summary. Prefixed with matched progress once a `Matched` event
    /// has arrived: `matched D / N` (or `D / ~N` while the matcher is still
    /// streaming), then `done M, err K, running R, cache H hit / M miss`.
    pub fn summary(&self) -> String {
        // Per target: a remote hit is a hit, a full miss one miss.
        let hits = self.cache.hits();
        let misses = self.cache.misses();
        let matched = match self.matched_progress() {
            // `~` marks a provisional total while the matcher is still streaming.
            Some((done, total, complete)) => {
                let tilde = if complete { "" } else { "~" };
                format!("matched {done} / {tilde}{total}, ")
            }
            None => String::new(),
        };
        format!(
            "{matched}done {}, err {}, running {}, cache {} hit / {} miss",
            self.completed,
            self.errored,
            self.dash.in_flight_count(),
            hits,
            misses,
        )
    }

    /// Every cache that serialized somebody this run, as
    /// `(scratch, waiters, total wait ms)`, ordered by total wait descending so
    /// the worst offender is first. Empty when nothing ever blocked.
    pub fn scratch_wait_totals(&self) -> Vec<(&str, u64, u64)> {
        let mut v: Vec<(&str, u64, u64)> = self
            .scratch_wait_totals
            .iter()
            .map(|(k, (n, ms))| (k.as_str(), *n, *ms))
            .collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
        v
    }

    /// What this state says a session is doing, as `show --json` carries it:
    /// every list in full, each sorted so two folds of the same events produce
    /// the same bytes. `end` and `truncated` are the caller's to set.
    pub fn snapshot(&self) -> crate::tui::session::SessionSnapshot {
        self.snapshot_capped(usize::MAX)
    }

    /// [`Self::snapshot`] with each list cut to its first `cap` entries, as
    /// `status.json` carries it ([`crate::tui::session::STATUS_LIST_CAP`]);
    /// `totals` and the counts are over the full lists.
    ///
    /// Walks the in-flight sets (open ops, waits) and the failure list, never
    /// every target, and clones only the entries it keeps; called at most once
    /// a second by the recorder, on its own thread.
    pub fn snapshot_capped(&self, cap: usize) -> crate::tui::session::SessionSnapshot {
        use crate::tui::session::format::{
            AdmissionWaitEntry, ListTotal, ListTotals, LockWaitEntry, RunningTarget,
            SessionSnapshot, SnapshotCounts,
        };
        let count = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        let mut running: Vec<(&str, Op, u64)> = self
            .dash
            .open_ops()
            .filter_map(|(id, r)| Some((self.dash.addr(id), r.op()?, r.op_since_ms)))
            .collect();
        running.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(b.0)));
        let running_unblocked = running
            .iter()
            .filter(|(addr, ..)| {
                !self.lock_waits.contains_key(*addr) && !self.admission_waits.contains_key(*addr)
            })
            .count();
        let all = self.dash.counts(CountScope::All);
        let matched = self
            .dash
            .matched_seen()
            .then(|| self.dash.counts(CountScope::Matched));
        let mut lock_waits: Vec<(&String, &(LockHolder, u64))> = self.lock_waits.iter().collect();
        lock_waits.sort_by(|a, b| a.0.cmp(b.0));
        let mut admission_waits: Vec<(&String, &AdmissionWait)> =
            self.admission_waits.iter().collect();
        admission_waits.sort_by(|a, b| a.0.cmp(b.0));
        let mut waiting: Vec<&String> = lock_waits
            .iter()
            .map(|(addr, _)| *addr)
            .chain(admission_waits.iter().map(|(addr, _)| *addr))
            .collect();
        waiting.sort();
        waiting.dedup();
        let totals = ListTotals {
            running: ListTotal::of(running.len(), cap),
            waiting: ListTotal::of(waiting.len(), cap),
            failed: ListTotal::of(all.failed_total(), cap),
            lock_waits: ListTotal::of(lock_waits.len(), cap),
            admission_waits: ListTotal::of(admission_waits.len(), cap),
        };
        let running: Vec<RunningTarget> = running
            .into_iter()
            .take(cap)
            .map(|(addr, op, since)| RunningTarget {
                addr: addr.to_owned(),
                op: op.name().to_owned(),
                since_unix_ms: since,
            })
            .collect();
        let lock_waits: Vec<LockWaitEntry> = lock_waits
            .into_iter()
            .take(cap)
            .map(|(addr, (holder, since))| LockWaitEntry {
                addr: addr.clone(),
                holder_pid: match holder {
                    LockHolder::Pid(pid) => Some(*pid),
                    LockHolder::Readers | LockHolder::Unknown => None,
                },
                in_use_by_readers: *holder == LockHolder::Readers,
                since_unix_ms: *since,
            })
            .collect();
        let admission_waits: Vec<AdmissionWaitEntry> = admission_waits
            .into_iter()
            .take(cap)
            .map(|(addr, w)| AdmissionWaitEntry {
                addr: addr.clone(),
                pool: w.pool.clone(),
                need: w.need,
                free: w.free,
                ahead: w.ahead,
                since_unix_ms: w.since_ms,
            })
            .collect();
        let waiting: Vec<String> = waiting.into_iter().take(cap).cloned().collect();
        SessionSnapshot {
            first_event_unix_ms: self.started_at_ms,
            last_event_unix_ms: self.last_event_ms,
            counts: SnapshotCounts {
                matched: matched.map(|m| count(m.all())),
                matched_complete: self.dash.matched_complete(),
                matched_done: count(matched.map_or(0, |m| m.ended())),
                matched_cached: count(matched.map_or(0, |m| m.cached)),
                finished: count(all.ended()),
                in_flight: count(self.dash.in_flight_count()),
                executed: count(self.executed),
                failed: count(self.errored),
                running_unblocked: Some(count(running_unblocked)),
            },
            running,
            waiting,
            failed: self
                .dash
                .failures()
                .take(cap)
                .map(|id| self.dash.addr(id).to_owned())
                .collect(),
            lock_waits,
            admission_waits,
            totals,
            ..SessionSnapshot::default()
        }
    }
}

/// What a `ResultLockWaitStart` could say about who is in the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockHolder {
    /// A stamped, live holder of the target's gateway.
    Pid(u32),
    /// Readers of the revision — unstamped, so detectable but not nameable.
    Readers,
    Unknown,
}

/// `session <id> (pid N)`, or `pid N` when no session is known for it.
fn holder_text(pid: u32, session: Option<SessionId>) -> String {
    match session {
        Some(id) => format!("session {id} (pid {pid})"),
        None => format!("pid {pid}"),
    }
}

impl LockHolder {
    fn of(holder_pid: Option<u32>, in_use_by_readers: bool) -> Self {
        match holder_pid {
            Some(pid) => Self::Pid(pid),
            None if in_use_by_readers => Self::Readers,
            None => Self::Unknown,
        }
    }
}

/// Shared aggregation backing both paved-road views: a label plus the folded
/// [`BuildState`] and the list of errored targets. Both `TuiProgressView` and
/// `CiProgressView` wrap one of these so the fold logic lives in one place.
struct ProgressCore {
    label: String,
    state: BuildState,
}

impl ProgressCore {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: BuildState::new(),
        }
    }

    /// Fold one event into the aggregate counters. Per-target failures are not
    /// collected here — they're rendered richly from the request's failure
    /// registry (see `commands::errors::render_failures`).
    fn fold(&mut self, ev: &BuildEvent) {
        self.state.apply(ev);
    }
}

pub use hcore::units::human_bytes;

/// Per-command header content for [`TuiProgressView`]. The view owns the
/// line, the elapsed clock and the dashboard; the header supplies the label
/// and the final summary, and a command with no dashboard its line's status
/// segment. `run`/`query`/`inspect` use [`BuildHeader`]; `gc` uses
/// [`GcHeader`].
pub trait ProgressHeader: Send {
    /// Fold an event into header-private state. The build header reads the
    /// view's shared [`BuildState`] instead, so its impl is a no-op.
    fn apply(&mut self, _ev: &BuildEvent) {}
    /// The line's status segment, after the clock, for a header with no
    /// dashboard ([`has_dashboard`](Self::has_dashboard) `false`). A header
    /// with one never has it read: the line draws the build counts itself.
    fn segment(&self) -> String {
        String::new()
    }
    /// The bottom-border label.
    fn label(&self) -> String;
    /// Whether the line keeps [`label`](Self::label) up once the matcher has
    /// completed, not only while it streams: an entrypoint's "Invoking …",
    /// which names what is about to take the terminal.
    fn label_stands(&self) -> bool {
        false
    }
    /// Final summary segment printed after the run (after the elapsed clock).
    /// Empty ⇒ nothing is printed. `scope` is the count scope the view was left
    /// on (the `a` toggle), so the printed summary matches the header the user
    /// was reading; state-private headers ignore it.
    fn last_render(&self, core: &BuildState, scope: CountScope) -> String;
    /// Which header a session viewer should rebuild for this run. Defaults to
    /// the build counts.
    fn session_kind(&self) -> crate::tui::session::HeaderKind {
        crate::tui::session::HeaderKind::Build
    }
    /// Whether `tab` opens a dashboard over the line. `false` for a command
    /// with no per-target progress (`heph gc`): its line is its own segment,
    /// with no hint, and `tab` does nothing.
    fn has_dashboard(&self) -> bool {
        true
    }
}

/// Build/query/inspect header: the elapsed-clock counts segment plus worker
/// braille, all read from the shared [`BuildState`].
pub struct BuildHeader {
    label: String,
}

impl BuildHeader {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

impl ProgressHeader for BuildHeader {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn last_render(&self, core: &BuildState, scope: CountScope) -> String {
        if !core.has_activity() {
            return String::new();
        }
        // Report the scope the view was left on: switching to `All` mid-run
        // carries through to the printed exit summary.
        match scope {
            CountScope::Matched => core.counts_segment(scope),
            // The printed line outlives the header (scrollback, copy-paste),
            // where nothing else says which population the counts cover — so
            // the non-default scope labels itself. Matched output stays
            // byte-identical to what it always was.
            CountScope::All => format!("{} · all targets", core.counts_segment(scope)),
        }
    }
}

/// GC sweep header: targets explored, revisions dropped, and bytes freed,
/// folded from [`BuildEventKind::GcTargetSwept`].
pub struct GcHeader {
    label: String,
    targets: usize,
    revisions_removed: usize,
    bytes_removed: u64,
    /// Failure logs removed and their bytes, once a
    /// [`BuildEventKind::GcFailureLogsSwept`] arrives. `None` for a sweep that
    /// has none (`heph tool clean`), whose summary keeps its old shape.
    failure_logs: Option<(usize, u64)>,
    /// The `cache.db` pass, rendered, once a [`BuildEventKind::GcCacheDb`]
    /// arrives. `None` for a sweep that has none (`heph tool clean`).
    cache_db: Option<String>,
}

impl GcHeader {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            targets: 0,
            revisions_removed: 0,
            bytes_removed: 0,
            failure_logs: None,
            cache_db: None,
        }
    }
}

/// One `GcCacheDb` event as a header segment: the file as the pass left it,
/// then what the pass did, e.g.
/// `cache.db 12.1 GiB (live 11.6 GiB, free 0 B, incremental): converted, freed 8.9 GiB`.
#[expect(clippy::too_many_arguments, reason = "the event's fields, one for one")]
fn cache_db_segment(
    file_bytes: u64,
    live_bytes: u64,
    free_bytes: u64,
    mode: &str,
    action: &str,
    freed_bytes: u64,
    needed_bytes: u64,
    available_bytes: u64,
    detail: Option<&str>,
) -> String {
    if action == "failed" {
        return format!("cache.db: vacuum failed: {}", detail.unwrap_or("unknown"));
    }
    let mode = if mode.is_empty() { "?" } else { mode };
    let state = format!(
        "cache.db {} (live {}, free {}, {mode})",
        human_bytes(file_bytes),
        human_bytes(live_bytes),
        human_bytes(free_bytes),
    );
    let outcome = match action {
        "converted" | "vacuumed" => {
            let mut o = format!("{action}, freed {}", human_bytes(freed_bytes));
            if let Some(d) = detail {
                o.push_str(&format!(" ({d})"));
            }
            o
        }
        "skipped" if needed_bytes > 0 => format!(
            "not converted: needs {} free, {} available",
            human_bytes(needed_bytes),
            human_bytes(available_bytes),
        ),
        "skipped" => format!("skipped: {}", detail.unwrap_or("unknown")),
        "unchanged" | "" => "unchanged".to_string(),
        other => other.to_string(),
    };
    format!("{state}: {outcome}")
}

impl ProgressHeader for GcHeader {
    fn apply(&mut self, ev: &BuildEvent) {
        match &ev.kind {
            BuildEventKind::GcTargetSwept {
                revisions_removed,
                bytes_removed,
            } => {
                self.targets += 1;
                self.revisions_removed += revisions_removed;
                self.bytes_removed = self.bytes_removed.saturating_add(*bytes_removed);
            }
            BuildEventKind::GcFailureLogsSwept {
                removed,
                bytes_removed,
            } => {
                let (n, bytes) = self.failure_logs.get_or_insert((0, 0));
                *n += removed;
                *bytes = bytes.saturating_add(*bytes_removed);
            }
            BuildEventKind::GcCacheDb {
                file_bytes,
                wal_bytes: _,
                live_bytes,
                free_bytes,
                mode,
                action,
                freed_bytes,
                needed_bytes,
                available_bytes,
                detail,
            } => {
                self.cache_db = Some(cache_db_segment(
                    *file_bytes,
                    *live_bytes,
                    *free_bytes,
                    mode,
                    action,
                    *freed_bytes,
                    *needed_bytes,
                    *available_bytes,
                    detail.as_deref(),
                ));
            }
            _ => {}
        }
    }

    fn segment(&self) -> String {
        let mut s = format!(
            "{} targets · {} freed",
            self.targets,
            human_bytes(self.bytes_removed),
        );
        if let Some((n, bytes)) = self.failure_logs {
            s.push_str(&format!(
                " · {n} failure logs, {} freed",
                human_bytes(bytes)
            ));
        }
        if let Some(db) = &self.cache_db {
            s.push_str(" · ");
            s.push_str(db);
        }
        s
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn session_kind(&self) -> crate::tui::session::HeaderKind {
        crate::tui::session::HeaderKind::Gc
    }

    fn has_dashboard(&self) -> bool {
        false
    }

    fn last_render(&self, _core: &BuildState, _scope: CountScope) -> String {
        if self.targets > 0
            || self.failure_logs.is_some_and(|(n, _)| n > 0)
            || self.cache_db.is_some()
        {
            self.segment()
        } else {
            String::new()
        }
    }
}

/// The paved-road [`TUIAppView`]: an elapsed-clock header (its status segment
/// supplied by a [`ProgressHeader`]) over a shared [`BuildState`] that drives the
/// long-running/lock-wait body, plus a persistent final summary.
pub struct TuiProgressView {
    /// Shared build state: elapsed-clock anchor + body rows. Folds every event.
    state: BuildState,
    /// Per-command header content (build counts vs gc sweep stats).
    model: Box<dyn ProgressHeader>,
    /// Server-wall timestamp captured when the run finished and the viewport was
    /// held open (the dashboard was up). `Some` freezes the elapsed clock at
    /// this instant.
    finished_at_ms: Option<u64>,
    /// The user stopped the run ([`TUIAppView::set_stopped`]): the final line
    /// reads `✗ stopped · …`.
    stopped: bool,
    /// Whether the dashboard is up (full screen, on the alternate screen)
    /// rather than the line. A run starts on the line; `Tab` opens the
    /// dashboard and `Esc` with no filter closes it.
    dashboard_open: bool,
    /// The dashboard list's tab, selection, scroll, filter and scope. Kept
    /// while the dashboard is closed, so `Tab` reopens it where it was.
    list: crate::tui::dashboard::ListState,
    /// The LAST LINE column's text per visible row, as `tails` last found it.
    last_lines: crate::tui::dashboard::LastLines,
    /// The background reader that fills `last_lines`: the run's own logs
    /// for a live view ([`Self::with_log_reader`]). `None`: the column shows
    /// wait and failure text only (an attached view, until it has a reader).
    tails: Option<crate::tui::dashboard::TailReader>,
    /// The reader [`Self::tails`] starts on, held until the first dashboard
    /// frame: a run nobody presses Tab on starts no thread.
    tail_source: Option<std::sync::Arc<dyn crate::tui::dashboard::LogReader>>,
    /// The target screen open over the dashboard (Enter / `l` on a row);
    /// Esc closes it back to the list, its selection kept.
    target: Option<crate::tui::dashboard::TargetScreen>,
    /// Where a target screen reads its log and outputs
    /// ([`Self::with_target_reader`]). `None`: Overview only.
    target_reader: Option<std::sync::Arc<dyn crate::tui::dashboard::TargetReader>>,
    /// Whose view this is: the run's own, or a session viewer's. The one seam
    /// between the two; everything not behind it renders identically.
    role: Role,
    /// The first of two ctrl-c on a full screen.
    stop_arm: crate::tui::stop_arm::StopArm,
    /// `--dashboard`: the finished run holds until `q` on any screen.
    pinned: bool,
    /// The approval view, over whichever screen opened it (the line, the
    /// dashboard or a target screen); closing it returns there.
    approval: Option<crate::tui::dashboard::approval::ApprovalScreen>,
    /// Where the approval view reads notices: the run's store
    /// ([`Self::with_notices`]).
    notices: Option<crate::tui::dashboard::approval::NoticeStore>,
    /// Whether the approval glyph flashes (`✦`/`❖`). Off under reduced
    /// motion, which is `NO_COLOR` alone: there is no config key for it.
    motion: bool,
}

/// Whose terminal a [`TuiProgressView`] draws on.
///
/// `heph session attach` draws the run's own view, so the only things that
/// differ are here: what acts on the run (cancel, the approval view's
/// decisions) exists only in [`Role::Live`], and an attached view reads what the run's
/// terminal showed besides build events (its label changes, the approval
/// prompt, the instant its summary was measured) from the recorded
/// [`SessionFeed`].
///
/// [`SessionFeed`]: crate::tui::session::SessionFeed
enum Role {
    /// The run of this process.
    Live {
        /// Pending approval prompts shared with the engine's approval handler.
        /// `None` for commands without an approval gate. While one is
        /// pending the line shows its prompt row, and the approval view
        /// decides it.
        approval: Option<crate::tui::approval::ApprovalCenter>,
    },
    /// A viewer of the recorded session `id`.
    Attached {
        id: SessionId,
        feed: crate::tui::session::SessionFeed,
        /// `q` on the main view while the run goes on: end this viewer. Never
        /// a cancel: the view has no way to reach the run it shows.
        detach: Box<dyn Fn() + Send>,
    },
}

/// The prompt row above the line while a request is pending:
/// `✦ approval required: //x · 2 pending · enter to review`. Both roles draw
/// through this; a viewer (`live` false) says where the decision is made
/// instead of offering a key (I11). Never a notice's path.
fn approval_prompt_row(glyph: &'static str, addr: &str, pending: u64, live: bool) -> Line<'static> {
    let queued = if pending > 1 {
        format!(" · {pending} pending")
    } else {
        String::new()
    };
    let tail = if live {
        APPROVAL_REVIEW
    } else {
        APPROVAL_ELSEWHERE
    };
    // As the spec's mockup: the request yellow bold, the rest yellow.
    let yellow = Style::default().fg(Color::Yellow);
    let style = yellow.add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(glyph, style),
        Span::styled(
            format!(
                " approval required: {}",
                crate::tui::text::clean_inline(addr)
            ),
            style,
        ),
        Span::styled(format!("{queued}{tail}"), yellow),
    ])
}

/// The pending requests a session recorded, as an attached viewer's queue,
/// their waits measured at `now_unix_ms`.
fn recorded_queue(
    prompt: crate::tui::session::ApprovalPrompt,
    now_unix_ms: u64,
) -> Vec<crate::tui::approval::QueuedApproval> {
    use crate::tui::approval::{NoticeRef, QueuedApproval};
    let notices = |ns: Vec<crate::tui::session::ApprovalPromptNotice>| {
        ns.into_iter()
            .map(|n| NoticeRef {
                name: n.name,
                sources: n.sources,
            })
            .collect()
    };
    prompt
        .queue
        .into_iter()
        .map(|q| QueuedApproval {
            seq: q.seq,
            addr: q.addr,
            notices: notices(q.notices),
            waited: std::time::Duration::from_millis(
                now_unix_ms.saturating_sub(q.queued_at_unix_ms),
            ),
        })
        .collect()
}

/// The prompt row's key, in the run's own view: Enter opens the approval
/// view, the only place that decides.
const APPROVAL_REVIEW: &str = " · enter to review";

/// The dashboard's hint for the approval view while a request is pending.
const APPROVAL_OPEN_HINT: &str = "v review approval";

/// What an attached viewer's prompt row says in place of the key (I11).
const APPROVAL_ELSEWHERE: &str = " · decide in the terminal that runs it";

impl TuiProgressView {
    /// Build-counts header (run/query/inspect). The label rides the bottom border.
    pub fn new(label: impl Into<String>) -> Self {
        Self::with_header(Box::new(BuildHeader::new(label)))
    }

    /// A view with a custom header model (e.g. [`GcHeader`] for `heph gc`).
    pub fn with_header(model: Box<dyn ProgressHeader>) -> Self {
        Self {
            state: BuildState::new(),
            model,
            finished_at_ms: None,
            stopped: false,
            dashboard_open: false,
            list: crate::tui::dashboard::ListState::default(),
            last_lines: crate::tui::dashboard::LastLines::default(),
            tails: None,
            tail_source: None,
            target: None,
            target_reader: None,
            role: Role::Live { approval: None },
            stop_arm: crate::tui::stop_arm::StopArm::default(),
            pinned: false,
            approval: None,
            notices: None,
            motion: std::env::var_os("NO_COLOR").is_none(),
        }
    }

    /// Read the approval view's notices from the store under `roots` (heph's
    /// home and the workspace root) through `read`, the engine's capped
    /// `read_notice_from_store`. Without it every notice reads "notice
    /// unavailable".
    pub fn with_notices(
        mut self,
        roots: hcore::approval_notice::NoticeRoots,
        read: hcore::approval_notice::ReadNotice,
    ) -> Self {
        self.notices = Some(crate::tui::dashboard::approval::NoticeStore::new(
            roots, read,
        ));
        self
    }

    /// The approval glyph at `now_ms`, flashing unless motion is reduced.
    fn glyph(&self, now_ms: u64) -> &'static str {
        crate::tui::dashboard::model::approval_glyph(now_ms, self.motion)
    }

    /// The pending requests, oldest first: a live view's queue; a viewer's
    /// as the run last recorded it, by number and notice sources (it reads, and
    /// decides nothing).
    fn approval_queue(&self) -> Vec<crate::tui::approval::QueuedApproval> {
        match &self.role {
            Role::Live { approval } => approval
                .as_ref()
                .map(crate::tui::approval::ApprovalCenter::queue)
                .unwrap_or_default(),
            Role::Attached { feed, .. } => feed
                .approval()
                .map(|p| recorded_queue(p, hcore::events::now_unix_ms()))
                .unwrap_or_default(),
        }
    }

    /// Hand the list this frame's (or key's) pending requests, so its
    /// Approvals tab and ✦ rows match the queue. Before every use of the
    /// list. `glyph`: this frame's, or `None` to keep the last frame's.
    fn sync_approvals(&mut self, glyph: Option<&'static str>) {
        let queue = self.approval_queue();
        self.sync_approvals_with(&queue, glyph);
    }

    /// [`Self::sync_approvals`] from a `queue` the caller already read.
    fn sync_approvals_with(
        &mut self,
        queue: &[crate::tui::approval::QueuedApproval],
        glyph: Option<&'static str>,
    ) {
        use crate::tui::dashboard::list::ApprovalRow;
        let glyph = glyph.unwrap_or_else(|| self.list.approval_glyph());
        let rows = queue
            .iter()
            .filter_map(|q| {
                let id = self.state.dash.id(&q.addr)?;
                let names: Vec<&str> = q.notices.iter().map(|n| n.name.as_str()).collect();
                Some(ApprovalRow {
                    id,
                    seq: q.seq,
                    waited_ms: u64::try_from(q.waited.as_millis()).unwrap_or(u64::MAX),
                    notices: if names.is_empty() {
                        String::new()
                    } else {
                        format!("notices: {}", names.join(", "))
                    },
                })
            })
            .collect();
        self.list.set_approvals(&self.state.dash, rows, glyph);
    }

    /// Open the approval view on request `seq`, when it is still pending
    /// (or on the oldest, for `None`). `false`: nothing to open.
    fn open_approval(&mut self, seq: Option<u64>) -> bool {
        let queue = self.approval_queue();
        let req = match seq {
            Some(seq) => queue.into_iter().find(|q| q.seq == seq),
            None => queue.into_iter().next(),
        };
        let Some(req) = req else {
            return false;
        };
        let screen = crate::tui::dashboard::approval::ApprovalScreen::open(
            req,
            self.notices.clone(),
            &self.state.redact,
        );
        // A viewer reads the notices; the terminal that runs it decides (I11).
        self.approval = Some(match self.role {
            Role::Live { .. } => screen,
            Role::Attached { .. } => screen.read_only(),
        });
        true
    }

    /// This view, as `heph session attach` draws session `id`: the same view
    /// in its attached role. It reads the run's label, approval prompt and
    /// finish instant from `feed`, offers no key that acts on the run, and
    /// calls `detach` on `q` while the run goes on (Ctrl-C detaches through
    /// the viewer's shutdown). Drops any approval queue it was given.
    pub fn attached(
        mut self,
        id: SessionId,
        feed: crate::tui::session::SessionFeed,
        detach: impl Fn() + Send + 'static,
    ) -> Self {
        self.role = Role::Attached {
            id,
            feed,
            detach: Box::new(detach),
        };
        self
    }

    /// The label on the bottom border: the header model's, or in an attached
    /// view the run's latest recorded one.
    fn label(&self) -> String {
        match &self.role {
            Role::Attached { feed, .. } => feed.label().unwrap_or_else(|| self.model.label()),
            Role::Live { .. } => self.model.label(),
        }
    }

    /// The approval queue, in a live view that has one.
    fn approval_center(&self) -> Option<&crate::tui::approval::ApprovalCenter> {
        match &self.role {
            Role::Live { approval } => approval.as_ref(),
            Role::Attached { .. } => None,
        }
    }

    /// The folded state, for a caller that renders more than the view does
    /// (`heph session show`'s stuck-on block reads its snapshot).
    pub fn state(&self) -> &BuildState {
        &self.state
    }

    /// One frame as plain lines, for printing once (`heph session show`): the
    /// line, then the dashboard's All list, at most [`SNAPSHOT_ROWS`] rows,
    /// with its column titles and position. No key hints. `ratatui`'s `Line`
    /// prints its text with `Display`, styles dropped.
    ///
    /// `now_ms` is the clock the elapsed time and wait ages are measured
    /// against: the viewer's now for a live session, the `End` record's time
    /// for an ended one ([`TUIAppView::set_finished`] it first, so the line
    /// reads as finished).
    pub fn snapshot_lines(&self, now_ms: u64, width: u16) -> Vec<Line<'static>> {
        use crate::tui::dashboard::{LastLines, ListInputs, ListState, list::list_lines};
        let mut lines = vec![self.line("▶", now_ms, width, false)];
        if self.model.has_dashboard() {
            let mut list = ListState::default();
            list.scope = self.list.scope;
            let inputs = ListInputs {
                model: &self.state.dash,
                waits: &self.state,
                lines: &LastLines::default(),
                now_ms,
                redact: &self.state.redact,
            };
            // The titles, the rows and the position; not the rows the list
            // pads its window with, which a print has no window for.
            lines.extend(
                list_lines(&inputs, &mut list, width, SNAPSHOT_ROWS + 3)
                    .into_iter()
                    .filter(|l| l.spans.iter().any(|s| !s.content.trim().is_empty())),
            );
        }
        lines
    }

    /// The final summary segment, scoped to whatever the `a` toggle was left on
    /// — a user who switched the header to `All` reads the same scope after
    /// teardown. Split out from [`TUIAppView::last_render`], which writes it
    /// straight to stderr and so cannot be asserted on.
    fn final_summary_segment(&self) -> String {
        self.model.last_render(&self.state, self.list.scope)
    }

    /// The final summary line [`TUIAppView::last_render`] prints, `None` when
    /// there was nothing to report. The clock is the finish instant
    /// ([`TUIAppView::set_finished`]), matching the frozen header, so a viewer
    /// prints the line the run printed.
    pub fn final_line(&self) -> Option<String> {
        self.final_line_in(false)
    }

    /// [`final_line`](Self::final_line), painted as the spec's mockup when
    /// `color`: the ✓ green bold or the ✗ (and `stopped`) red bold, the clock
    /// bold, a non-zero failed count red bold and its split dim. Without
    /// colour it is `final_line`, byte for byte; only the terminal gets the
    /// paint, never a session viewer.
    fn final_line_in(&self, color: bool) -> Option<String> {
        use crossterm::style::Stylize;
        let paint = |s: &str, f: fn(String) -> crossterm::style::StyledContent<String>| {
            if color {
                f(s.to_owned()).to_string()
            } else {
                s.to_owned()
            }
        };
        let red_bold = |s: String| s.red().bold();
        let segment = self.final_summary_segment();
        // The failed count and its split, as `counts_segment` wrote them.
        let segment = {
            let (_, _, failed) = self.state.count_fields(self.list.scope);
            let tabs = self.state.dash.counts(self.list.scope);
            let split = failure_split_text((tabs.failed, tabs.dep_failed));
            let whole = format!("{failed}{split}");
            match segment.split_once(&whole) {
                Some((before, after)) if color && !failed.starts_with("0 ") => {
                    format!(
                        "{before}{}{}{after}",
                        paint(&failed, red_bold),
                        paint(&split, |s| s.dim())
                    )
                }
                _ => segment,
            }
        };
        // A stop always says so, even before anything matched.
        if segment.is_empty() && !self.stopped {
            return None;
        }
        let clock = self
            .finished_at_ms
            .unwrap_or_else(hcore::events::now_unix_ms);
        let elapsed = human_elapsed_ms(self.state.elapsed_ms(clock));
        let elapsed = paint(elapsed.trim_start(), |s| s.bold());
        let mark = match self.outcome() {
            // A user stop is never a success, whatever finished before it;
            // the word says why the counts stop short.
            Outcome::Stopped => {
                let stopped = paint("✗ stopped", red_bold);
                if segment.is_empty() {
                    return Some(format!("{stopped} · {elapsed}"));
                }
                return Some(format!("{stopped} · {elapsed} · {segment}"));
            }
            Outcome::Failed => paint("✗", red_bold),
            Outcome::Done => paint("✓", |s| s.green().bold()),
        };
        Some(format!("{mark} {elapsed} · {segment}"))
    }

    /// How the run ended, as the final line's mark says it.
    fn outcome(&self) -> Outcome {
        if self.stopped {
            Outcome::Stopped
        } else if self.state.unsuccessful() {
            Outcome::Failed
        } else {
            Outcome::Done
        }
    }

    /// The dashboard title row's verdict once the run has finished, worded
    /// as the final line is: `✓ done`, `✗ 2 failed` (the line's failed
    /// count; `✗ cancelled` when nothing failed outright) or `✗ stopped`.
    /// `None` while it runs.
    fn verdict(&self) -> Option<crate::tui::dashboard::Verdict> {
        self.finished_at_ms?;
        let (text, ok) = match self.outcome() {
            Outcome::Stopped => ("✗ stopped".to_owned(), false),
            Outcome::Failed => {
                let (_, _, failed) = self.state.count_fields(self.list.scope);
                if failed.starts_with("0 ") {
                    ("✗ cancelled".to_owned(), false)
                } else {
                    (format!("✗ {failed}"), false)
                }
            }
            Outcome::Done => ("✓ done".to_owned(), true),
        };
        Some(crate::tui::dashboard::Verdict { text, ok })
    }

    /// Open the dashboard, as `Tab` from the line does.
    #[cfg(test)]
    pub(crate) fn open_dashboard(&mut self) {
        self.tab(true);
    }

    /// The instant the run's times are measured to: `now_ms` while it runs,
    /// the finish once it has ended, so every clock on screen stops there
    /// (a target still open when the run ended included).
    fn clock_at(&self, now_ms: u64) -> u64 {
        self.finished_at_ms.unwrap_or(now_ms)
    }

    /// The dashboard's title row: the command, the clock and how far along
    /// the run is, in the line's own figures.
    fn dashboard_title_parts(&self, now_ms: u64) -> (String, Option<String>, String) {
        let clock_at = self.clock_at(now_ms);
        let clock = hcore::units::human_duration(std::time::Duration::from_millis(
            self.state.elapsed_ms(clock_at),
        ));
        let session = match &self.role {
            Role::Attached { id, .. } => Some(id.to_string()),
            Role::Live { .. } => None,
        };
        (self.label(), session, clock)
    }

    /// One dashboard frame, `cols` × `rows`:
    /// [`crate::tui::dashboard::dashboard_frame`], its pending requests
    /// leading All and on the Approvals tab.
    fn dashboard_lines(&mut self, now_ms: u64, cols: u16, rows: u16) -> Vec<Line<'static>> {
        use crate::tui::dashboard::{DASHBOARD_HINTS, ListInputs, Title, dashboard_frame};
        self.sync_approvals(Some(self.glyph(now_ms)));
        let (command, session, clock) = self.dashboard_title_parts(now_ms);
        let scope = self.list.scope;
        let tabs = self.state.dash.counts(scope);
        let title = Title {
            command: &command,
            session: session.as_deref(),
            clock: &clock,
            done: tabs.finished_ok(),
            total: (scope == CountScope::All || self.state.dash.matched_seen()).then(|| tabs.all()),
            provisional: scope == CountScope::Matched && !self.state.dash.matched_complete(),
            standing: self.approval_standing().map(|s| s.indicator()),
            verdict: self.verdict(),
        };
        // The filter, while one is typed or kept, replaces the key hints.
        let filter = self.list.filter();
        let hints: std::borrow::Cow<'_, str> = if self.list.is_editing() {
            format!("/{filter}▏  enter keep · esc clear").into()
        } else if !filter.is_empty() {
            format!("filter: {filter} · / edit · esc clear").into()
        } else {
            let keys = if self.finished_at_ms.is_some() {
                FINISHED_DASHBOARD_HINTS
            } else {
                DASHBOARD_HINTS
            };
            match &self.role {
                // A viewer leaves with `q`, and stops nothing.
                Role::Attached { .. } => {
                    let keys = keys.trim_end_matches(" · ctrl-c stop");
                    if self.list.approvals().is_empty() {
                        format!("{keys} · q detach").into()
                    } else {
                        // Read-only there: its own hint says so.
                        format!("{APPROVAL_OPEN_HINT} · {keys} · q detach").into()
                    }
                }
                Role::Live { .. } if !self.list.approvals().is_empty() => {
                    format!("{APPROVAL_OPEN_HINT} · {keys}").into()
                }
                Role::Live { .. } => keys.into(),
            }
        };
        // The reader's thread starts with the first frame that shows the
        // column.
        if let Some(reader) = self.tail_source.take() {
            self.tails = crate::tui::dashboard::TailReader::spawn(reader);
        }
        // What the background reader found since the last frame, for the
        // rows that frame showed.
        if let Some(updates) = self.tails.as_mut().and_then(|t| t.poll()) {
            self.last_lines.apply(updates, self.list.visible());
        }
        let inputs = ListInputs {
            model: &self.state.dash,
            waits: &self.state,
            lines: &self.last_lines,
            now_ms: self.clock_at(now_ms),
            redact: &self.state.redact,
        };
        let out = dashboard_frame(&title, &inputs, &mut self.list, &hints, cols, rows);
        // The rows this frame shows, for the reader to (re)read off this task.
        if let Some(tails) = self.tails.as_mut().filter(|t| t.wants(now_ms)) {
            tails.send(
                self.last_lines
                    .requests(&self.state.dash, self.list.visible()),
                now_ms,
            );
        }
        out
    }

    /// The spotlight's rule 1: the pending approval on show, with how many
    /// are pending. The run's from its queue, a viewer's from the feed. Its
    /// ✦ flashes with the frame's glyph at `now_ms`.
    fn approval_spot(&self, now_ms: u64) -> Option<Spot> {
        let (addr, pending) = self.approval_pending()?;
        let mut spot = Spot::approval(&addr, pending);
        spot.icon = self.glyph(now_ms);
        Some(spot)
    }

    /// The request the prompt row names (the oldest pending) and how many
    /// are pending: the run's from its queue, a viewer's from the feed.
    fn approval_pending(&self) -> Option<(String, u64)> {
        match &self.role {
            Role::Live { approval } => {
                let (addr, pending) = approval.as_ref()?.front()?;
                Some((addr, u64::try_from(pending).unwrap_or(u64::MAX)))
            }
            Role::Attached { feed, .. } => {
                let queue = feed.approval()?.queue;
                let pending = u64::try_from(queue.len()).unwrap_or(u64::MAX);
                let front = queue.into_iter().next()?;
                Some((front.addr, pending))
            }
        }
    }

    /// What the cpu pool holds, of its capacity, as its latest `PoolUsage`
    /// reported it: `cpu 11/12`. `None` before the first report, and before
    /// anything ran: a pool not in play has no gauge here either.
    fn cpu_segment(&self) -> Option<String> {
        let dash = &self.state.dash;
        let g = dash
            .pool(hcore::capacity::CPU)
            .filter(|_| dash.in_play(hcore::capacity::CPU))?;
        Some(format!(
            "cpu {}/{}",
            hcore::events::display_amount(g.unit, g.held),
            hcore::events::display_amount(g.unit, g.capacity)
        ))
    }

    /// The line at `now_ms`, `width` columns wide.
    /// The line. `live`: drawn on a terminal heph reads keys from, so it
    /// carries their hints (`tab ⇥`, and `q to quit` while `--dashboard`
    /// holds the finished run); a printed one (`heph session show`) has none.
    fn line(&self, spinner: &str, now_ms: u64, width: u16, live: bool) -> Line<'static> {
        use std::borrow::Cow;
        let clock_at = self.clock_at(now_ms);
        let clock = hcore::units::human_duration(std::time::Duration::from_millis(
            self.state.elapsed_ms(clock_at),
        ));
        let status = match self.finished_at_ms {
            Some(_) => line::Status::Finished {
                failed: self.state.unsuccessful(),
                stopped: self.stopped,
            },
            None => line::Status::Running(spinner),
        };
        let dashboard = self.model.has_dashboard();
        let scope = self.list.scope;
        let body = if dashboard {
            // The same `TabCounts` the dashboard's tabs read, so the line
            // and the tabs cannot disagree.
            let tabs = self.state.dash.counts(scope);
            let Counts { done, total, .. } = self.state.counts_of(&tabs, scope);
            line::Body::Build(line::BuildCounts {
                done,
                total,
                cached: tabs.cached,
                waiting: tabs.blocked,
                failed: tabs.failed_total(),
                cpu: self.cpu_segment(),
            })
        } else {
            line::Body::Text(Cow::Owned(self.model.segment()))
        };
        // Until the run ends, a label the header keeps up ("Invoking …": what
        // is about to take the terminal, on the line while its dependencies
        // build). The view's own label ("Running //…") is not repeated: the
        // command it names is on the row above.
        let stands = self.finished_at_ms.is_none()
            && matches!(self.role, Role::Live { .. })
            && self.model.label_stands();
        let label = (dashboard && stands)
            .then(|| self.label())
            .filter(|l| !l.is_empty())
            .map(Cow::Owned);
        let model = line::LineModel {
            status,
            clock,
            label,
            body,
            standing: self.approval_standing(),
            spot: self
                .approval_spot(now_ms)
                .or_else(|| self.state.spotlight(clock_at)),
            hint: dashboard && live,
            detach: matches!(self.role, Role::Attached { .. }),
            held: live && self.pinned && self.finished_at_ms.is_some(),
        };
        line::render(&model, width)
    }

    /// Attach the shared approval queue so this view renders pending prompts and
    /// resolves them from key events. Used by commands that gate execution. A
    /// live view's only: [`Self::attached`] drops it.
    pub fn with_approval(mut self, center: crate::tui::approval::ApprovalCenter) -> Self {
        if let Role::Live { approval } = &mut self.role {
            *approval = Some(center);
        }
        self
    }

    /// Fill the dashboard's LAST LINE column from `reader`, on a thread of
    /// its own started by the first dashboard frame: a frame only hands it
    /// the visible rows and takes what it found (I4, I7). The seam an
    /// attached view's reader plugs into too.
    pub fn with_log_reader(
        mut self,
        reader: std::sync::Arc<dyn crate::tui::dashboard::LogReader>,
    ) -> Self {
        self.tails = None;
        self.tail_source = Some(reader);
        self
    }

    /// Serve the target screens' Log and Outputs tabs from `reader`, on a
    /// thread per open screen (I4, I7).
    pub fn with_target_reader(
        mut self,
        reader: std::sync::Arc<dyn crate::tui::dashboard::TargetReader>,
    ) -> Self {
        self.target_reader = Some(reader);
        self
    }

    /// The open target screen's frame, under the dashboard's header.
    fn target_lines(&mut self, now_ms: u64, cols: u16, rows: u16) -> Option<Vec<Line<'static>>> {
        let (command, session, clock) = self.dashboard_title_parts(now_ms);
        let scope = self.list.scope;
        let tabs = self.state.dash.counts(scope);
        let title = crate::tui::dashboard::Title {
            command: &command,
            session: session.as_deref(),
            clock: &clock,
            done: tabs.finished_ok(),
            total: (scope == CountScope::All || self.state.dash.matched_seen()).then(|| tabs.all()),
            provisional: scope == CountScope::Matched && !self.state.dash.matched_complete(),
            standing: self.approval_standing().map(|s| s.indicator()),
            verdict: self.verdict(),
        };
        let inputs = crate::tui::dashboard::TargetInputs {
            model: &self.state.dash,
            waits: &self.state,
            redact: &self.state.redact,
            now_ms: self.clock_at(now_ms),
            scope,
            detach: matches!(self.role, Role::Attached { .. }),
        };
        let target = self.target.as_mut()?;
        Some(target.frame(&title, &inputs, cols, rows))
    }

    /// Redact `redact`'s paths (heph's home) from the error text this view
    /// draws (I9).
    pub fn with_redact(mut self, redact: crate::tui::text::Redact) -> Self {
        self.state.set_redact(redact);
        self
    }

    /// The prompt row above the line while a request is pending, `None`
    /// when none is.
    fn approval_prompt(&self, now_ms: u64) -> Option<Line<'static>> {
        let (addr, pending) = self.approval_pending()?;
        Some(approval_prompt_row(
            self.glyph(now_ms),
            &addr,
            pending,
            matches!(self.role, Role::Live { .. }),
        ))
    }

    /// The open approval view's frame, under the dashboard's header.
    fn approval_lines(&mut self, now_ms: u64, cols: u16, rows: u16) -> Option<Vec<Line<'static>>> {
        self.approval.as_ref()?;
        let glyph = self.glyph(now_ms);
        let queue = self.approval_queue();
        self.sync_approvals_with(&queue, Some(glyph));
        let (command, session, clock) = self.dashboard_title_parts(now_ms);
        let scope = self.list.scope;
        let mut counts = self.state.dash.counts(scope);
        counts.approvals = queue.len();
        counts.approval_glyph = glyph;
        let title = crate::tui::dashboard::Title {
            command: &command,
            session: session.as_deref(),
            clock: &clock,
            done: counts.finished_ok(),
            total: (scope == CountScope::All || self.state.dash.matched_seen())
                .then(|| counts.all()),
            provisional: scope == CountScope::Matched && !self.state.dash.matched_complete(),
            standing: self.approval_standing().map(|s| s.indicator()),
            verdict: self.verdict(),
        };
        let gauges = self.state.dash.gauges();
        let inputs = crate::tui::dashboard::approval::ApprovalInputs {
            title: &title,
            gauges: &gauges,
            counts: &counts,
            queue: &queue,
            glyph,
            now_ms,
        };
        Some(self.approval.as_mut()?.frame(&inputs, cols, rows))
    }
}

impl TUIAppView for TuiProgressView {
    fn apply(&mut self, ev: &BuildEvent) {
        self.state.apply(ev);
        self.model.apply(ev);
    }

    fn session_header(&self) -> crate::tui::session::SessionHeader {
        crate::tui::session::SessionHeader {
            kind: self.model.session_kind(),
            label: self.model.label(),
        }
    }

    fn name_lock_holders(&mut self, names: HolderNames) {
        self.state.name_lock_holders(names);
    }

    fn quit_while_running(&mut self) {
        match &self.role {
            Role::Attached { detach, .. } => detach(),
            // A run of this process: `q` does nothing, Ctrl-C cancels.
            Role::Live { .. } => {}
        }
    }

    fn quits_from_any_screen(&self) -> bool {
        matches!(self.role, Role::Attached { .. })
    }

    fn pending_approval(&self) -> Option<crate::tui::session::ApprovalPrompt> {
        self.approval_center()
            .and_then(crate::tui::approval::ApprovalCenter::prompt)
    }

    fn approval_standing(&self) -> Option<crate::tui::session::ApprovalStanding> {
        match &self.role {
            Role::Live { approval } => approval.as_ref().and_then(|c| c.standing()),
            Role::Attached { feed, .. } => feed.approval_standing(),
        }
    }

    fn rows(&self, _term_height: u16) -> u16 {
        // The line, and the approval prompt above it while one is pending.
        // The dashboard is drawn on the alternate screen, never here.
        1 + u16::from(self.approval_pending().is_some())
    }

    fn full_screen(&self) -> bool {
        self.dashboard_open || self.approval.is_some()
    }

    fn render_full(&mut self, now_ms: u64, cols: u16, rows: u16) -> Vec<Line<'static>> {
        // The approval view overlays whichever screen opened it.
        let mut lines = match self.approval_lines(now_ms, cols, rows) {
            Some(lines) => lines,
            None => match self.target_lines(now_ms, cols, rows) {
                Some(lines) => lines,
                None => self.dashboard_lines(now_ms, cols, rows),
            },
        };
        // An armed stop takes the hint row, the last on every full screen.
        if let Some(prompt) = self.stop_arm.prompt(now_ms)
            && let Some(last) = lines.last_mut()
        {
            *last = crate::tui::dashboard::chrome::hint_line(
                &prompt,
                usize::from(cols),
                ratatui::style::Style::default().fg(ratatui::style::Color::Yellow),
            );
        }
        lines
    }

    fn ctrl_c(
        &mut self,
        now_ms: u64,
        finished: bool,
        stopping: bool,
    ) -> crate::tui::stop_arm::CtrlC {
        let at = crate::tui::stop_arm::Press {
            full_screen: self.full_screen(),
            attached: matches!(self.role, Role::Attached { .. }),
            finished: finished || self.finished_at_ms.is_some(),
            stopping,
        };
        self.stop_arm.press(at, now_ms)
    }

    fn disarm_stop(&mut self) {
        self.stop_arm.disarm();
    }

    fn pin_dashboard(&mut self) {
        if self.model.has_dashboard() {
            self.dashboard_open = true;
            self.pinned = true;
        }
    }

    fn screen_key(&mut self, key: crate::tui::app::ScreenKey) {
        use crate::tui::app::ScreenKey;
        if let Some(a) = self.approval.as_mut() {
            // The notice's own movement; `n`/`N` decide here, upstream.
            match key {
                ScreenKey::PageUp => a.scroll(-a.page()),
                ScreenKey::PageDown => a.scroll(a.page()),
                ScreenKey::Top => a.scroll(isize::MIN),
                ScreenKey::Bottom => a.scroll(isize::MAX),
                ScreenKey::Follow | ScreenKey::Wrap | ScreenKey::Next | ScreenKey::Prev => {}
            }
            return;
        }
        if let Some(t) = self.target.as_mut() {
            let _bound = t.key(key, &self.state.redact);
        }
    }

    fn mouse(&mut self, input: crate::tui::app::MouseInput) {
        use crate::tui::app::MouseInput;
        use crate::tui::dashboard::approval::ApprovalClick;
        use crate::tui::dashboard::{ListHit, TargetClick, WHEEL_ROWS};
        // The approval view: its breadcrumb and notice tabs. No click
        // decides.
        if let Some(a) = self.approval.as_mut() {
            match input {
                MouseInput::Click { col, row } => {
                    if a.click(col, row) == ApprovalClick::Back {
                        self.approval = None;
                    }
                }
                MouseInput::Wheel { down, .. } => a.wheel(down),
            }
            return;
        }
        if !self.dashboard_open {
            return;
        }
        self.sync_approvals(None);
        match (input, self.target.as_mut()) {
            (MouseInput::Click { col, row }, Some(t)) => {
                if t.click(col, row) == TargetClick::Back {
                    self.back_to_main();
                }
            }
            (MouseInput::Wheel { down, .. }, Some(t)) => t.wheel(down),
            // A ✦ row opens its approval view, as Enter does; it never
            // decides.
            (MouseInput::Click { col, row }, None) => {
                match self.list.hit(col, row) {
                    Some(ListHit::Tab(tab)) => self.list.set_tab(tab),
                    // A second click on the selected row opens it, like
                    // enter: no double-click timing.
                    Some(ListHit::Row(id)) if self.list.selected() == Some(id) => {
                        self.open_selected(false);
                    }
                    Some(ListHit::Row(id)) => self.list.select(id),
                    None => {}
                }
            }
            (MouseInput::Wheel { down, .. }, None) => {
                let delta = if down { WHEEL_ROWS } else { -WHEEL_ROWS };
                self.list.wheel(&self.state.dash, delta);
            }
        }
    }

    fn scroll(&mut self, delta: i32) {
        if let Some(a) = self.approval.as_mut() {
            a.scroll(isize::try_from(delta).unwrap_or(0));
        } else if let Some(t) = self.target.as_mut() {
            t.scroll(isize::try_from(delta).unwrap_or(0));
        } else if self.dashboard_open {
            let delta = isize::try_from(delta).unwrap_or(0);
            self.sync_approvals(None);
            self.list.select_move(&self.state.dash, delta);
        }
    }

    fn tab(&mut self, forward: bool) {
        if let Some(a) = self.approval.as_mut() {
            a.cycle_notice(forward);
        } else if let Some(t) = self.target.as_mut() {
            t.cycle_tab(forward);
        } else if self.dashboard_open {
            self.sync_approvals(None);
            self.list.cycle_tab(forward);
        } else if self.model.has_dashboard() {
            // From the line, either direction opens the dashboard; a command
            // with no dashboard has nothing to open.
            self.dashboard_open = true;
        }
    }

    fn toggle_scope(&mut self) {
        if self.target.is_some() || self.approval.is_some() {
            return;
        }
        self.list.scope = match self.list.scope {
            CountScope::Matched => CountScope::All,
            CountScope::All => CountScope::Matched,
        };
    }

    fn is_on_main_view(&self) -> bool {
        !self.full_screen()
    }

    fn has_active_filter(&self) -> bool {
        if self.approval.is_some() {
            return false;
        }
        match &self.target {
            Some(t) => t.has_search(),
            None => self.list.is_editing() || !self.list.filter().is_empty(),
        }
    }

    fn back_to_main(&mut self) {
        // From the approval view, back to the screen that opened it (the
        // request stays pending); from a target screen, the list; from the
        // list, the line.
        if self.approval.take().is_some() {
            return;
        }
        if self.target.take().is_none() {
            self.dashboard_open = false;
        }
    }

    fn open_selected(&mut self, log: bool) {
        if !self.dashboard_open || self.approval.is_some() {
            return;
        }
        if let Some(t) = &mut self.target {
            // Enter on an open target screen: expand a folded output group.
            if !log {
                t.enter();
            }
            return;
        }
        self.sync_approvals(None);
        if let Some(id) = self.list.selected() {
            // A ✦ row: Enter opens its approval view, on that request.
            if !log && let Some(seq) = self.list.approval_of(id).map(|a| a.seq) {
                let _opened = self.open_approval(Some(seq));
                return;
            }
            self.target = Some(crate::tui::dashboard::TargetScreen::open(
                &self.state.dash,
                id,
                log,
                self.target_reader.clone(),
            ));
        }
    }

    // The approval view acts on the run only in a live view: an attached
    // view opens it read-only on the recorded queue, and decides nothing.
    fn approval_open(&mut self) -> bool {
        self.approval.is_none() && self.open_approval(None)
    }

    fn in_approval_view(&self) -> bool {
        self.approval.is_some()
    }

    fn approval_decide(&mut self, decide: crate::tui::app::Decide, now_ms: u64) {
        use crate::tui::app::Decide;
        use crate::tui::session::ApprovalStanding;
        let Some(screen) = self.approval.as_mut() else {
            return;
        };
        let Some(center) = (match &self.role {
            Role::Live { approval } => approval.as_ref(),
            Role::Attached { .. } => None,
        }) else {
            return;
        };
        // Locked out: dropped, never queued for when it arms.
        if !screen.armed(now_ms) {
            return;
        }
        let seq = screen.seq();
        // The center prints each decision's scrollback line; the view only
        // shows it in its header.
        let approved = match decide {
            Decide::Approve | Decide::Reject => {
                let approve = decide == Decide::Approve;
                center
                    .respond(seq, approve, screen.read_names())
                    .map(|d| d.approved)
            }
            Decide::ApproveAll | Decide::RejectAll => {
                let standing = if decide == Decide::ApproveAll {
                    ApprovalStanding::Approve
                } else {
                    ApprovalStanding::Reject
                };
                center
                    .set_standing(seq, standing)
                    .into_iter()
                    .find(|d| d.seq == seq)
                    .map(|d| d.approved)
            }
        };
        if let Some(approved) = approved {
            screen.decided(approved, now_ms);
        }
    }

    fn advance(&mut self, now_ms: u64) {
        if self.approval.is_none() {
            return;
        }
        // A viewer's request leaves the queue when the run's terminal
        // decides it: the view moves on as a live one does.
        let queue = self.approval_queue();
        let Some(screen) = self.approval.as_mut() else {
            return;
        };
        if !screen.advance(now_ms, &queue) {
            self.approval = None;
        }
    }

    fn is_searching(&self) -> bool {
        if self.approval.is_some() {
            return false;
        }
        match &self.target {
            Some(t) => t.is_searching(),
            None => self.list.is_editing(),
        }
    }

    fn search_start(&mut self) {
        // A target screen searches its log; the dashboard filters its list;
        // the line and the approval view have neither.
        if self.approval.is_some() {
            return;
        }
        if let Some(t) = self.target.as_mut() {
            t.search_start();
        } else if self.dashboard_open {
            self.list.filter_start();
        }
    }

    fn search_input(&mut self, c: char) {
        match self.target.as_mut() {
            Some(t) => t.search_input(c),
            None => self.list.filter_input(c),
        }
    }

    fn search_backspace(&mut self) {
        match self.target.as_mut() {
            Some(t) => t.search_backspace(),
            None => self.list.filter_backspace(),
        }
    }

    fn search_cancel(&mut self) {
        match self.target.as_mut() {
            Some(t) => t.search_cancel(),
            None => {
                let _had = self.list.escape();
            }
        }
    }

    fn search_confirm(&mut self) {
        // Leave input mode but keep the filter (or the search).
        match self.target.as_mut() {
            Some(t) => t.search_confirm(&self.state.redact),
            None => self.list.filter_confirm(),
        }
    }

    /// The line, and above it the approval prompt row while a request is
    /// pending. `height` is the rows [`Self::rows`] asked for.
    fn render(&self, spinner: &str, now_ms: u64, width: u16, _height: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(2);
        lines.extend(self.approval_prompt(now_ms));
        lines.push(self.line(spinner, now_ms, width, true));
        lines
    }

    fn hold_after_finish(&self) -> bool {
        // Auto-exit only from the line; while the dashboard is up, keep it
        // until the user leaves (I: closing it under their cursor loses
        // their place). `--dashboard` holds on every screen. A viewer never
        // holds: the session's end prints its summary and report, and attach
        // exits with its code (I11).
        !matches!(self.role, Role::Attached { .. }) && (self.full_screen() || self.pinned)
    }

    fn set_stopped(&mut self) {
        self.stopped = true;
    }

    fn stopped(&self) -> bool {
        self.stopped
    }

    fn set_finished(&mut self, at_ms: u64) {
        // Freeze the elapsed clock at the finish instant; the viewport keeps
        // rendering while held but the time must stop counting. A viewer
        // freezes it where the run's own summary did, once it was recorded,
        // and says "stopped" if the run's did.
        let at = match &self.role {
            Role::Attached { feed, .. } => {
                self.stopped |= feed.stopped();
                feed.finished_at_ms().unwrap_or(at_ms)
            }
            Role::Live { .. } => at_ms,
        };
        self.finished_at_ms = Some(at);
        // Nothing is left to stop: an armed prompt would ask for nothing.
        self.stop_arm.disarm();
    }

    /// Final report — the elapsed clock plus the header model's summary segment,
    /// printed straight to stderr (not the log sink) so it survives the torn-down
    /// inline viewport. Per-target failures are rendered separately (rich
    /// diagnostics from the failure registry). The model returns an empty segment
    /// when there was nothing to report (e.g. inspect/query, or a no-op gc), in
    /// which case nothing is printed.
    fn last_render(&self) {
        if let Some(line) = self.final_line_in(crate::tui::color::stderr_color_enabled()) {
            use std::io::Write;
            drop(writeln!(std::io::stderr().lock(), "{line}"));
        }
    }
}

/// One line the CI view would log, and whether it is a warning.
///
/// Built as a value rather than logged inline so the "stay quiet" contract — no
/// line at all for a warm cache — is testable without capturing a subscriber. A
/// regression there is not cosmetic: it puts one line per target into a CI log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Note {
    pub(crate) text: String,
    pub(crate) warn: bool,
}

/// The line for a snapshot restored somewhere other than where it was produced.
///
/// Names the command that explains it, because an operator — or an agent — must
/// be able to act on this without first working out what to run.
pub(crate) fn path_mismatch_note(scratch: &str) -> String {
    format!(
        "scratch {scratch}: restored at a different path than it was produced at \
         — a path-sensitive cache will be inert. \
         See `heph tool scratch head {scratch}`"
    )
}

/// The line for one finished prepare, or `None` when there is nothing worth
/// saying — which is the overwhelmingly common case.
pub(crate) fn prepare_note(
    scratch: &str,
    outcome: &str,
    bytes: u64,
    error: Option<&str>,
) -> Option<Note> {
    let (text, warn) = match (error, outcome) {
        (Some(err), _) => (
            format!("scratch {scratch}: could not prepare ({err})"),
            true,
        ),
        // The common case, and the whole point of staying quiet. A cold cache
        // is the normal first run, nothing to act on: also silent.
        (None, "" | "warm" | "cold") => return None,
        (None, "pulled") => (
            format!(
                "scratch {scratch}: pulled {} from the remote",
                human_bytes(bytes)
            ),
            false,
        ),
        (None, "seeded") => (
            format!("scratch {scratch}: seeded from a fallback scope"),
            false,
        ),
        (None, "dropped_over_max") => (
            format!("scratch {scratch}: over its max_size — dropped whole, this build runs cold"),
            true,
        ),
        (None, "audit") => (
            format!("scratch {scratch}: --no-scratch, running against an empty cache"),
            false,
        ),
        // An outcome from a host newer than this build. Printing it verbatim
        // beats dropping it: the word is meant to be readable, and this is
        // exactly why `outcome` is a string and not an enum.
        (None, other) => (format!("scratch {scratch}: {other}"), false),
    };
    Some(Note { text, warn })
}

/// A root failure's block in the CI log, printed the moment it fails, in the
/// failure report's words (`src/commands/errors/report.rs`):
///
/// ```text
/// ✗ //web:lint · exit status: 1
///   │ web/a.ts:14:3  error  'useState' is defined but never used
///   full log: /home/u/.heph/failures/…/log.txt
///   log: heph log //web:lint
/// ```
///
/// The header names the exit status or the timeout as the report's block
/// header does; a failure with neither shows its error as a body row, as the
/// report's body does. Then the last `log_lines` lines of the tail the event
/// carries (never a file read on the event path), sanitized, redacted and
/// plain. Then the CI box's last rows ([`left_behind_rows`]) from what the
/// event says the run left behind: the block stands in for the box, which the
/// end of the run leaves out for it. No trailing newline: the log line adds
/// one.
///
/// [`left_behind_rows`]: crate::tui::failure_rows::left_behind_rows
fn live_failure_block(
    addr: &str,
    error: &str,
    exit_status: Option<&str>,
    timed_out: Option<&hcore::events::TimedOutData>,
    tail: Option<&str>,
    log_lines: usize,
    redact: &crate::tui::text::Redact,
) -> String {
    use crate::tui::text::{clean_inline, expand_tabs, sanitize_log};
    let row = |line: &str| expand_tabs(&redact.apply(line)).into_owned();

    let mut out = format!("✗ {}", clean_inline(addr));
    let mut body: Vec<String> = Vec::new();
    if let Some(t) = timed_out {
        out.push_str(" · timed out after ");
        out.push_str(&hcore::units::format_duration(
            std::time::Duration::from_millis(t.timeout_ms),
        ));
        out.push_str(" (its timeout)");
    } else if let Some(status) = exit_status {
        out.push_str(" · ");
        out.push_str(clean_inline(status).trim_end());
    } else {
        body.extend(sanitize_log(error.as_bytes()).lines().map(row));
    }
    if log_lines > 0
        && let Some(text) = tail
    {
        if text.trim().is_empty() {
            body.push("empty log".to_owned());
        } else {
            let clean = sanitize_log(text.as_bytes());
            let n = clean.lines().count();
            body.extend(clean.lines().skip(n.saturating_sub(log_lines)).map(row));
        }
    }
    for line in &body {
        out.push_str("\n  │ ");
        out.push_str(line);
    }
    out
}

/// `block` followed by the box's last rows, so the end of the run need not
/// print the box. Not redacted: they are paths to open and commands to run,
/// which the box prints whole too.
fn with_left_behind(
    mut block: String,
    addr: &str,
    left_behind: Option<&hcore::events::LeftBehindData>,
) -> String {
    if let Some(left) = left_behind {
        let rows = crate::tui::failure_rows::left_behind_rows(addr, left);
        if !rows.is_empty() {
            block.push('\n');
            block.push_str(rows.trim_end_matches('\n'));
        }
    }
    block
}

/// The paved-road [`CIAppView`]: no header line, a concise per-execute line,
/// and a final one-line summary plus per-error lines — all through the log
/// sink. Used by every command's `ci_view`.
pub struct CiProgressView {
    core: ProgressCore,
    /// Caches whose contention has already been announced.
    ///
    /// One `exclusive` cache shared by hundreds of targets produces that many
    /// waits; without this, a GitHub log gets that many identical lines. The
    /// first names the problem, and [`finish`](CIAppView::finish) reports the
    /// total it cost.
    scratch_wait_announced: HashSet<String>,
    /// The user stopped the run ([`CIAppView::set_stopped`]): the summary
    /// line starts `stopped · `, as the TUI's final line does.
    stopped: bool,
    /// How many of a root failure's last log lines it prints the moment it
    /// fails (`--log-lines`).
    log_lines: usize,
    /// Root failures already printed: a same-addr `ResultEnd` double-fire
    /// prints one block, not two.
    failures_printed: HashSet<String>,
}

/// `heph run --log-lines`' default: how many tail lines a failure prints live
/// when the command has no such flag.
const DEFAULT_LOG_LINES: usize = 10;

impl CiProgressView {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            core: ProgressCore::new(label),
            scratch_wait_announced: HashSet::new(),
            stopped: false,
            log_lines: DEFAULT_LOG_LINES,
            failures_printed: HashSet::new(),
        }
    }

    /// Print `n` tail lines per root failure instead of [`DEFAULT_LOG_LINES`].
    #[must_use]
    pub fn with_log_lines(mut self, n: usize) -> Self {
        self.log_lines = n;
        self
    }

    /// Redact `redact` (heph's home) from every error text and log line.
    #[must_use]
    pub fn with_redact(mut self, redact: crate::tui::text::Redact) -> Self {
        self.core.state.set_redact(redact);
        self
    }

    /// The summary line [`CIAppView::finish`] logs last.
    fn summary_line(&self) -> String {
        let summary = self.core.state.summary();
        if self.stopped {
            format!("stopped · {summary}")
        } else {
            summary
        }
    }
}

impl CIAppView for CiProgressView {
    fn session_header(&self) -> crate::tui::session::SessionHeader {
        crate::tui::session::SessionHeader {
            kind: crate::tui::session::HeaderKind::Build,
            label: self.core.label.clone(),
        }
    }

    fn name_lock_holders(&mut self, names: HolderNames) {
        self.core.state.name_lock_holders(names);
    }

    fn failures_printed(&self) -> HashSet<String> {
        self.failures_printed.clone()
    }

    // No `begin`: the run opens with no "Running …" header line. The command
    // that names it is the line above in any log; the label lives on as the
    // session's.

    /// "Not chatty" policy: an execute starting logs nothing — a line per
    /// target is noise on a busy build, and the summary counts them. Only
    /// what the reader must act on or explain (lock waits, scratch notices)
    /// gets a line.
    fn apply(&mut self, ev: &BuildEvent) {
        match &ev.kind {
            // Admission waits stay off the CI log: a line per wait is noise
            // on a busy build, and the live views (the TUI's waiting tab, the
            // GHA live report) show them as they happen.
            BuildEventKind::ResultLockWaitStart {
                addr,
                holder_pid,
                in_use_by_readers,
            } => {
                let holder = match LockHolder::of(*holder_pid, *in_use_by_readers) {
                    LockHolder::Pid(pid) => format!("held by {}", self.core.state.holder(pid)),
                    LockHolder::Readers => "in use".to_string(),
                    LockHolder::Unknown => "holder unknown".to_string(),
                };
                tracing::info!("waiting on result lock for {addr} ({holder})");
            }
            // First waiter per cache only — see `scratch_wait_announced`.
            BuildEventKind::ScratchLockWaitStart {
                scratch,
                access,
                holder_pid,
                ..
            } if self.scratch_wait_announced.insert(scratch.clone()) => match holder_pid {
                Some(pid) => tracing::info!(
                    "waiting on scratch {scratch}, held by {}",
                    self.core.state.holder(*pid)
                ),
                None => tracing::info!(
                    "waiting on scratch {scratch} ({access}); \
                     targets sharing it run one at a time"
                ),
            },
            // The cold-runner case this view exists for: no tty, and a pull that
            // can run for minutes. Silent for `warm`, which is every other run.
            BuildEventKind::ScratchPrepareEnd {
                scratch,
                outcome,
                bytes,
                path_mismatch,
                error,
                ..
            } => {
                if *path_mismatch {
                    tracing::warn!("{}", path_mismatch_note(scratch));
                }
                if let Some(note) = prepare_note(scratch, outcome, *bytes, error.as_deref()) {
                    if note.warn {
                        tracing::warn!("{}", note.text);
                    } else {
                        tracing::info!("{}", note.text);
                    }
                }
            }
            // A root failure's block, the moment it fails: a CI log otherwise
            // says why only once the whole build is over. A collateral
            // failure and a stop print nothing — the root's block explains
            // them, and Ctrl-C is not a failure.
            BuildEventKind::ResultEnd {
                addr,
                error: Some(error),
                upstream_of: None,
                exit_status,
                log_tail,
                timed_out,
                cancelled: false,
                left_behind,
            } if !self.failures_printed.contains(addr) => {
                self.failures_printed.insert(addr.clone());
                let block = with_left_behind(
                    live_failure_block(
                        addr,
                        error,
                        exit_status.as_deref(),
                        timed_out.as_ref(),
                        log_tail.as_ref().map(|t| t.text.as_str()),
                        self.log_lines,
                        &self.core.state.redact,
                    ),
                    addr,
                    left_behind.as_deref(),
                );
                tracing::error!("{block}");
            }
            _ => {}
        }
        self.core.fold(ev);
    }

    fn finish(&self) {
        if let Some(n) = self.core.state.matched_total() {
            tracing::info!("matched {n} targets");
        }
        // The one number that tells someone their `exclusive` default is what
        // made the build slow. Reported per cache, worst first.
        for (scratch, waiters, total_ms) in self.core.state.scratch_wait_totals() {
            let total = human_elapsed(total_ms);
            tracing::info!("scratch {scratch} serialized {waiters} targets, {total} of total wait");
        }
        tracing::info!("{}", self.summary_line());
    }

    fn set_stopped(&mut self) {
        self.stopped = true;
    }
}

/// CI (non-TUI) view for a cache sweep (`heph tool gc`, `heph tool clean`): folds it
/// into a [`GcHeader`] and logs a single summary line at the end, prefixed with
/// the header's label so two commands sharing this view stay distinguishable in
/// a log. Per-target progress is silent.
pub struct GcCiView {
    header: GcHeader,
}

impl GcCiView {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            header: GcHeader::new(label),
        }
    }
}

impl CIAppView for GcCiView {
    fn begin(&self) {
        tracing::info!("{}", self.header.label());
    }

    fn session_header(&self) -> crate::tui::session::SessionHeader {
        crate::tui::session::SessionHeader {
            kind: crate::tui::session::HeaderKind::Gc,
            label: self.header.label(),
        }
    }

    fn apply(&mut self, ev: &BuildEvent) {
        self.header.apply(ev);
    }

    fn finish(&self) {
        tracing::info!("{}: {}", self.header.label(), self.header.segment());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hcore::events::CacheOutcomeKind;

    fn ev(at_unix_ms: u64, kind: BuildEventKind) -> BuildEvent {
        BuildEvent { at_unix_ms, kind }
    }

    /// What the dashboard's blocked row and details say about `addr`'s wait.
    fn wait_detail(s: &BuildState, addr: &str) -> Option<String> {
        use crate::tui::dashboard::Waits as _;
        s.wait_words(addr, 0).map(|w| w.detail)
    }

    /// `addr`'s wait as `(PHASE, details)`, waited up to `now_ms`.
    fn wait_words_at(s: &BuildState, addr: &str, now_ms: u64) -> Option<(String, String)> {
        use crate::tui::dashboard::Waits as _;
        s.wait_words(addr, now_ms).map(|w| (w.phase, w.detail))
    }

    #[test]
    fn lock_wait_shown_with_holder_pid_then_cleared_on_end() {
        let mut s = BuildState::new();
        s.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitStart {
                addr: "//pkg:a".into(),
                holder_pid: Some(4242),
                in_use_by_readers: false,
            },
        ));
        assert_eq!(
            wait_detail(&s, "//pkg:a").as_deref(),
            Some("held by pid 4242")
        );

        // The notice disappears when the wait ends.
        s.apply(&ev(
            1,
            BuildEventKind::ResultLockWaitEnd {
                addr: "//pkg:a".into(),
            },
        ));
        assert_eq!(wait_detail(&s, "//pkg:a"), None);
    }

    /// A live session recorded under `root`, owned by this test process.
    fn live_session(root: &std::path::Path) -> crate::tui::session::Session {
        crate::tui::session::Session::open(
            &crate::tui::session::SessionRoot::new(root),
            crate::tui::session::SessionStart::default(),
        )
        .expect("open a session")
    }

    /// Names that read sessions from `root`, with no session of their own.
    fn names_in(root: &std::path::Path) -> HolderNames {
        HolderNames::new(Some(crate::tui::session::SessionRoot::new(root)), None)
    }

    /// Fold `e` as the backend's receiver does: its holder pid is resolved
    /// first, so the fold only reads the answer.
    fn resolve_then_apply(s: &mut BuildState, e: BuildEvent) {
        if let Some(pid) = crate::tui::session::holder::event_holder_pid(&e) {
            s.holders.resolve_blocking(pid);
        }
        s.apply(&e);
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The lock row and the scratch row name the live session recorded for
    /// the holder's pid, and keep the pid.
    #[test]
    fn lock_waits_name_the_holding_session() {
        let root = tempfile::tempdir().expect("tempdir");
        let holder = live_session(root.path());
        let pid = std::process::id();
        let mut s = BuildState::new();
        s.name_lock_holders(names_in(root.path()));
        resolve_then_apply(&mut s, ev(0, lock_wait_start("//pkg:a", pid)));
        resolve_then_apply(
            &mut s,
            ev(
                0,
                scratch_wait("//pkg:b", "//build:c", "exclusive", Some(pid)),
            ),
        );
        let id = holder.id();
        assert_eq!(
            wait_detail(&s, "//pkg:a"),
            Some(format!("held by session {id} (pid {pid})")),
        );
        assert_eq!(
            wait_detail(&s, "//pkg:b"),
            Some(format!("held by session {id} (pid {pid})")),
        );
        holder.finish(Some(0), false);
    }

    /// No session for the pid, an ended one, or only this process's own: the
    /// notice is the pid alone, exactly as without sessions.
    #[test]
    fn lock_waits_name_only_the_pid_without_a_live_other_session() {
        let pid = std::process::id();
        let pid_only = Some(format!("held by pid {pid}"));
        let render = |names: HolderNames| {
            let mut s = BuildState::new();
            s.name_lock_holders(names);
            resolve_then_apply(&mut s, ev(0, lock_wait_start("//pkg:a", pid)));
            wait_detail(&s, "//pkg:a")
        };

        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(render(names_in(empty.path())), pid_only, "no session");
        let missing = empty.path().join("absent");
        assert_eq!(render(names_in(&missing)), pid_only, "no sessions dir");
        assert_eq!(render(HolderNames::default()), pid_only, "no root");

        let ended = tempfile::tempdir().expect("tempdir");
        live_session(ended.path()).finish(Some(0), false);
        assert_eq!(render(names_in(ended.path())), pid_only, "ended session");

        let own = tempfile::tempdir().expect("tempdir");
        let session = live_session(own.path());
        let names = HolderNames::new(
            Some(crate::tui::session::SessionRoot::new(own.path())),
            Some(session.id()),
        );
        assert_eq!(render(names), pid_only, "this process's own session");
        session.finish(Some(0), false);
    }

    /// A pid is resolved before its first wait is folded, and never again: a
    /// session that appears later does not trigger a second scan.
    #[test]
    fn a_holder_pid_is_resolved_once() {
        let root = tempfile::tempdir().expect("tempdir");
        let pid = std::process::id();
        let mut s = BuildState::new();
        s.name_lock_holders(names_in(root.path()));
        resolve_then_apply(&mut s, ev(0, lock_wait_start("//pkg:a", pid)));
        let late = live_session(root.path());
        assert!(!s.holders.needs(pid), "resolved, so never scanned again");
        resolve_then_apply(&mut s, ev(1, lock_wait_start("//pkg:b", pid)));
        for addr in ["//pkg:a", "//pkg:b"] {
            assert_eq!(
                wait_detail(&s, addr),
                Some(format!("held by pid {pid}")),
                "{addr}"
            );
        }
        late.finish(Some(0), false);
    }

    #[test]
    fn lock_wait_unknown_holder_renders_unknown() {
        let mut s = BuildState::new();
        s.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitStart {
                addr: "//pkg:a".into(),
                holder_pid: None,
                in_use_by_readers: false,
            },
        ));
        assert_eq!(
            wait_detail(&s, "//pkg:a").as_deref(),
            Some("holder unknown")
        );
    }

    /// The wait the old message could not explain: a rebuild or delete parked
    /// behind another command's riding reads. Readers are unstamped, so there
    /// is no pid — but "holder unknown" sent people looking for a stuck lock.
    #[test]
    fn lock_wait_on_readers_says_another_command_is_using_it() {
        let mut s = BuildState::new();
        s.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitStart {
                addr: "//pkg:a".into(),
                holder_pid: None,
                in_use_by_readers: true,
            },
        ));
        assert_eq!(
            wait_detail(&s, "//pkg:a").as_deref(),
            Some("in use by readers")
        );
    }

    fn result_start(addr: &str) -> BuildEventKind {
        BuildEventKind::ResultStart { addr: addr.into() }
    }
    fn result_end(addr: &str, error: Option<String>) -> BuildEventKind {
        BuildEventKind::ResultEnd {
            left_behind: None,
            cancelled: false,
            addr: addr.into(),
            error,
            upstream_of: None,
            exit_status: None,
            log_tail: None,
            timed_out: None,
        }
    }
    fn execute_start(addr: &str) -> BuildEventKind {
        BuildEventKind::ExecuteStart {
            output_groups: Vec::new(),
            log_not_streamed: false,
            addr: addr.into(),
            driver: "exec".into(),
            cache: true,
            declared: None,
            reserved: None,
            queued_ms: None,
            admitted: None,
        }
    }

    fn execute_start_reserving(addr: &str, reserved: &[(&str, u64)]) -> BuildEventKind {
        BuildEventKind::ExecuteStart {
            output_groups: Vec::new(),
            log_not_streamed: false,
            addr: addr.into(),
            driver: "exec".into(),
            cache: true,
            declared: None,
            reserved: Some(reserved.iter().map(|&(p, n)| (p.into(), n)).collect()),
            queued_ms: None,
            admitted: None,
        }
    }

    const CPU: u64 = hcore::capacity::MICROS_PER_CPU;

    /// A pool's report, as the engine sends it on every change.
    fn pool_usage(pool: &str, held: u64, capacity: u64, waiting: u64) -> BuildEventKind {
        BuildEventKind::PoolUsage {
            pool: pool.into(),
            held,
            capacity,
            waiting,
            removals_pending: 0,
        }
    }

    /// Every gauge — the line's `cpu`, the box's cells and pool segments,
    /// the dashboard's header — is the pool's own latest `PoolUsage`:
    /// sandbox draws like any pool, a pool with waiters turns yellow and
    /// shows `⧗N`, and an `ExecuteStart.reserved` moves nothing.
    #[test]
    fn gauges_follow_pool_usage() {
        let mut s = BuildState::new();
        s.apply(&ev(
            0,
            BuildEventKind::RequestConfig {
                max_workers: 4,
                fail_fast: false,
                scratch_disabled: false,
                resources: [
                    ("cpu".into(), Some(12 * CPU)),
                    ("memory".into(), Some(16 << 30)),
                    ("sandbox".into(), Some(48)),
                    ("docker".into(), Some(2)),
                    ("gpu".into(), Some(4)),
                ]
                .into(),
                units: [
                    ("cpu".into(), "cpu".into()),
                    ("memory".into(), "bytes".into()),
                    ("sandbox".into(), "count".into()),
                    ("docker".into(), "count".into()),
                    ("gpu".into(), "count".into()),
                ]
                .into(),
            },
        ));
        // Idle reports before anything ran draw nothing: no pool is in play.
        s.apply(&ev(1, pool_usage("cpu", 0, 12 * CPU, 0)));
        s.apply(&ev(1, pool_usage("gpu", 0, 4, 0)));
        assert!(s.dashboard().gauges().is_empty());
        // A run puts what it reserved in play; a reservation moves no
        // gauge, and a pool that has not reported has none.
        s.apply(&ev(
            1,
            execute_start_reserving("//a:x", &[("cpu", 3 * CPU), ("docker", 1)]),
        ));
        let cpu_held = |s: &BuildState| s.dashboard().pool("cpu").map_or(0, |g| g.held);
        assert_eq!(cpu_held(&s), 0, "nothing derived from reserved");
        let pools = |s: &BuildState| {
            s.dashboard()
                .gauges()
                .iter()
                .map(|g| g.pool.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(pools(&s), ["cpu"]);

        s.apply(&ev(2, pool_usage("cpu", 11 * CPU + CPU / 2, 12 * CPU, 0)));
        s.apply(&ev(2, pool_usage("memory", 9 << 30, 16 << 30, 0)));
        s.apply(&ev(2, pool_usage("sandbox", 9, 48, 0)));
        s.apply(&ev(2, pool_usage("docker", 2, 2, 1)));
        s.apply(&ev(2, pool_usage("gpu", 0, 4, 0)));
        assert_eq!(cpu_held(&s), 11 * CPU + CPU / 2);
        let gauges = s.dashboard().gauges();
        assert_eq!(
            gauges.iter().map(|g| g.pool.as_str()).collect::<Vec<_>>(),
            ["cpu", "memory", "sandbox", "docker"],
            "one gauge per pool in play, in the fixed order; never gpu"
        );
        let title = crate::tui::dashboard::Title {
            command: "heph r build //...",
            session: None,
            clock: "1m05s",
            done: 0,
            total: None,
            provisional: false,
            standing: None,
            verdict: None,
        };
        let (rows, _) = crate::tui::dashboard::header_lines(
            &title,
            &gauges,
            &s.dashboard().counts(CountScope::Matched),
            crate::tui::dashboard::Tab::All,
            200,
        );
        let text = |l: &Line<'_>| {
            l.spans
                .iter()
                .map(|sp| sp.content.as_ref())
                .collect::<String>()
        };
        let header = rows.iter().map(text).collect::<Vec<_>>().join("\n");
        for want in ["11.5/12", "mem ██▎░ 9/16G", "sandbox ▊░░░ 9/48", "2/2 ⧗1"] {
            assert!(header.contains(want), "{want} in\n{header}");
        }
        let docker = rows
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|sp| sp.content.contains("⧗1"))
            .expect("docker's waiters");
        assert_eq!(docker.style.fg, Some(Color::Yellow), "{docker:?}");

        // The next report replaces the last; a run's end moves nothing.
        s.apply(&ev(3, execute_end("//a:x")));
        assert_eq!(cpu_held(&s), 11 * CPU + CPU / 2);
        s.apply(&ev(4, pool_usage("cpu", 0, 12 * CPU, 0)));
        s.apply(&ev(4, pool_usage("docker", 1, 2, 0)));
        assert_eq!(cpu_held(&s), 0);

        // A pool in play stays, idle or not; one never touched stays out.
        s.apply(&ev(5, pool_usage("memory", 0, 16 << 30, 0)));
        s.apply(&ev(5, pool_usage("docker", 0, 2, 0)));
        assert_eq!(pools(&s), ["cpu", "memory", "sandbox", "docker"]);
    }

    /// The line's `cpu` segment, like the header's gauge, waits for
    /// something to run: an idle report alone draws nothing.
    #[test]
    fn the_line_shows_cpu_once_anything_runs() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(1, pool_usage("cpu", 0, 12 * CPU, 0)));
        assert_eq!(v.cpu_segment(), None);
        v.apply(&ev(2, execute_start_reserving("//a:x", &[("cpu", 0)])));
        assert_eq!(v.cpu_segment().as_deref(), Some("cpu 0/12"));
    }

    /// A `sandbox` wait names its cause in the header: how many finished
    /// runs' sandboxes were still being removed, as the pool reports it.
    #[test]
    fn pool_segments_show_removals_pending_on_a_sandbox_wait() {
        let mut s = BuildState::new();
        let segments = |s: &BuildState| {
            s.dashboard()
                .pool("sandbox")
                .map(|g| (g.waiting, g.removals_pending))
        };
        let wait = |addr: &str, pending| BuildEventKind::AdmissionWaitStart {
            addr: addr.into(),
            pool: "sandbox".into(),
            need: 1,
            free: 0,
            ahead: 0,
            removals_pending: Some(pending),
            yielding_to: None,
        };
        let usage = |waiting, removals_pending| BuildEventKind::PoolUsage {
            pool: "sandbox".into(),
            held: 0,
            capacity: 16,
            waiting,
            removals_pending,
        };
        s.apply(&ev(1, wait("//a:x", 0)));
        s.apply(&ev(1, usage(1, 0)));
        assert_eq!(segments(&s), Some((1, 0)), "none pending");
        s.apply(&ev(1, wait("//a:y", 1)));
        s.apply(&ev(1, usage(2, 1)));
        assert_eq!(segments(&s), Some((2, 1)));
        s.apply(&ev(2, wait("//a:z", 3)));
        s.apply(&ev(2, usage(3, 3)));
        assert_eq!(segments(&s), Some((3, 3)));
        let row = wait_detail(&s, "//a:z").expect("a's wait");
        assert!(
            row.ends_with("— cleanup is behind: 3 removals pending"),
            "{row}"
        );
        for addr in ["//a:x", "//a:y", "//a:z"] {
            s.apply(&ev(
                3,
                BuildEventKind::AdmissionWaitEnd { addr: addr.into() },
            ));
        }
        s.apply(&ev(3, usage(0, 0)));
        assert_eq!(segments(&s), Some((0, 0)));
    }

    #[test]
    fn admission_wait_row_names_the_pool_and_the_queue() {
        let mut s = BuildState::new();
        s.apply(&ev(
            1_000,
            BuildEventKind::AdmissionWaitStart {
                addr: "//a:b".into(),
                pool: "cpu".into(),
                need: 4 * CPU,
                free: 2 * CPU,
                ahead: 6,
                removals_pending: None,
                yielding_to: None,
            },
        ));
        assert_eq!(
            wait_words_at(&s, "//a:b", 42_000),
            Some(("waits cpu 4".into(), "(2 free, 41s)".into()))
        );
        s.apply(&ev(
            43_000,
            BuildEventKind::AdmissionWaitEnd {
                addr: "//a:b".into(),
            },
        ));
        assert_eq!(wait_words_at(&s, "//a:b", 43_000), None);
    }

    /// A waiter that fits and is held back for the oldest one says so in its
    /// row, the amount in the unit of the pool it yields, which is not
    /// always the pool it waits on.
    #[test]
    fn admission_wait_row_names_what_it_yields_to() {
        let mut s = BuildState::new();
        let wait = |addr: &str, pool: &str, need| BuildEventKind::AdmissionWaitStart {
            addr: addr.into(),
            pool: "cpu".into(),
            need: CPU,
            free: 3 * CPU,
            ahead: 6,
            removals_pending: None,
            yielding_to: Some(hcore::events::YieldingToData {
                pool: pool.into(),
                need,
            }),
        };
        s.apply(&ev(1_000, wait("//a:b", "cpu", 8 * CPU)));
        s.apply(&ev(1_000, wait("//a:c", "memory", 2 << 30)));
        assert_eq!(
            wait_words_at(&s, "//a:b", 42_000).map(|w| w.1).as_deref(),
            Some("(3 free, yielding to a cpu 8 run, 41s)")
        );
        assert_eq!(
            wait_words_at(&s, "//a:c", 42_000).map(|w| w.1).as_deref(),
            Some("(3 free, yielding to a memory 2.0 GiB run, 41s)")
        );
    }

    fn admission_wait_start(addr: &str) -> BuildEventKind {
        BuildEventKind::AdmissionWaitStart {
            addr: addr.into(),
            pool: "cpu".into(),
            need: 4 * CPU,
            free: 2 * CPU,
            ahead: 6,
            removals_pending: None,
            yielding_to: None,
        }
    }

    /// One 100×14 dashboard frame as one string per line.
    fn dash_frame(v: &mut TuiProgressView, now_ms: u64) -> Vec<String> {
        v.render_full(now_ms, 100, 14)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_waiting_target_moves_to_the_pending_tab_and_back() {
        use crate::tui::dashboard::Tab;
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(1_000, admission_wait_start("//a:b")));

        // The line counts it…
        let line = |v: &TuiProgressView, now| -> String {
            v.render("⠋", now, 200, 1)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert!(
            line(&v, 42_000).contains("1 waiting"),
            "{}",
            line(&v, 42_000)
        );

        // …the Pending tab lists it, with the pool it waits on.
        v.open_dashboard();
        v.list.set_tab(Tab::Pending);
        let pending = dash_frame(&mut v, 42_000).join("\n");
        assert!(pending.contains("//a:b"), "{pending}");
        assert!(pending.contains("waits cpu"), "{pending}");

        // Granted: it leaves Pending for Running, and the line stops counting it.
        v.apply(&ev(
            43_000,
            BuildEventKind::AdmissionWaitEnd {
                addr: "//a:b".into(),
            },
        ));
        v.apply(&ev(43_000, execute_start("//a:b")));
        let pending = dash_frame(&mut v, 50_000).join("\n");
        assert!(!pending.contains("//a:b"), "{pending}");
        v.list.set_tab(Tab::Running);
        let running = dash_frame(&mut v, 50_000).join("\n");
        assert!(running.contains("//a:b"), "{running}");
        assert!(
            !line(&v, 50_000).contains("1 waiting"),
            "{}",
            line(&v, 50_000)
        );
    }

    #[test]
    fn execute_start_ends_an_admission_wait_without_its_end() {
        let mut s = BuildState::new();
        s.apply(&ev(1_000, admission_wait_start("//a:b")));
        s.apply(&ev(1_000, admission_wait_start("//a:c")));
        assert_eq!(s.counts(CountScope::Matched).waiting, "2 waiting");
        s.apply(&ev(2_000, execute_start("//a:b")));
        assert_eq!(s.counts(CountScope::Matched).waiting, "1 waiting");
        assert_eq!(wait_detail(&s, "//a:b"), None, "the started one");
        assert!(wait_detail(&s, "//a:c").is_some(), "the one still waiting");
    }

    /// The line as drawn at `now_ms`, 200 columns wide.
    fn line_at(v: &TuiProgressView, now_ms: u64) -> String {
        line_text(&v.line("⠋", now_ms, 200, true))
    }

    #[test]
    fn the_session_header_counts_waiting_between_cached_and_failed() {
        let v = TuiProgressView::new("L");
        let line = line_at(&v, 0);
        assert!(line.contains("0 cached · 0 waiting · 0 failed"), "{line}");
    }
    fn execute_end(addr: &str) -> BuildEventKind {
        BuildEventKind::ExecuteEnd {
            addr: addr.into(),
            error: None,
        }
    }
    fn local_write_start(addr: &str) -> BuildEventKind {
        BuildEventKind::LocalCacheWriteStart { addr: addr.into() }
    }
    fn local_write_end(addr: &str) -> BuildEventKind {
        BuildEventKind::LocalCacheWriteEnd {
            addr: addr.into(),
            error: None,
        }
    }
    fn remote_write_start(addr: &str) -> BuildEventKind {
        BuildEventKind::RemoteCacheWriteStart { addr: addr.into() }
    }
    fn remote_write_end(addr: &str) -> BuildEventKind {
        BuildEventKind::RemoteCacheWriteEnd {
            addr: addr.into(),
            error: None,
        }
    }

    #[test]
    fn op_timeline_records_execute_then_local_cache_write_breakdown() {
        // Execute runs 0→3s (completed), then LocalCacheWrite opens at 3s and is
        // still live: the finished op is folded into the record, the live one
        // is the open op.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(3_000, execute_end("//a:b")));
        s.apply(&ev(3_000, local_write_start("//a:b")));

        assert_eq!(open_addrs(&s), ["//a:b"]);
        let r = rec(&s, "//a:b");
        assert_eq!(r.op_ms(Op::Execute), 3_000);
        assert_eq!(r.op_ms(Op::LocalCacheWrite), 0, "still open");
        assert_eq!((r.op(), r.op_open()), (Some(Op::LocalCacheWrite), true));
        assert_eq!(r.op_since_ms, 3_000);
    }

    #[test]
    fn op_timeline_overlap_folds_dangling_active() {
        // A new op opening while one is still active folds the dangling one into
        // completed (defensive against a missing/reordered end).
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(3_000, local_write_start("//a:b")));

        let r = rec(&s, "//a:b");
        assert_eq!(r.op_ms(Op::Execute), 3_000);
        assert_eq!((r.op(), r.op_open()), (Some(Op::LocalCacheWrite), true));
        assert_eq!(r.op_since_ms, 3_000);
    }

    #[test]
    fn op_timeline_mismatched_end_ignored() {
        // An end for an op that is not the active one is a no-op.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(1_000, local_write_end("//a:b")));

        let r = rec(&s, "//a:b");
        assert_eq!(
            (r.op(), r.op_open(), r.op_since_ms),
            (Some(Op::Execute), true, 0)
        );
        assert_eq!(r.total_ms(), 0);
    }

    /// `addr`'s record in the model.
    fn rec<'a>(s: &'a BuildState, addr: &str) -> &'a crate::tui::dashboard::Record {
        s.dash
            .id(addr)
            .and_then(|id| s.dash.record(id))
            .expect("a record")
    }

    /// The addrs the open-op index holds, sorted.
    fn open_addrs(s: &BuildState) -> Vec<&str> {
        let mut v: Vec<&str> = s.dash.open_ops().map(|(id, _)| s.dash.addr(id)).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn a_result_end_closes_its_open_op_even_a_failing_one() {
        // A `ResultEnd` with an op still open (a lost End) must not leave the
        // target in the index the slow rows and busy count walk.
        for error in [None, Some("boom".to_owned())] {
            let mut s = BuildState::new();
            s.apply(&ev(0, execute_start("//a:b")));
            assert_eq!(open_addrs(&s), ["//a:b"]);
            s.apply(&ev(2_000, result_end("//a:b", error)));
            assert!(open_addrs(&s).is_empty());
            assert!(!rec(&s, "//a:b").op_open());
        }
    }

    #[test]
    fn a_remote_cache_write_that_trails_result_end_opens_and_closes() {
        // `RemoteCacheWrite` is pushed from a detached background task
        // (`Engine::spawn_remote_upload`) that the engine does not await inside
        // the `ResultEnd` scope, so its Start/End can legitimately arrive after
        // `ResultEnd`. It shows while open and leaves the index at its End.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:b")));
        s.apply(&ev(1_000, execute_end("//a:b")));
        s.apply(&ev(2_000, result_end("//a:b", None)));
        assert!(open_addrs(&s).is_empty());

        s.apply(&ev(3_000, remote_write_start("//a:b")));
        assert_eq!(open_addrs(&s), ["//a:b"]);

        s.apply(&ev(4_000, remote_write_end("//a:b")));
        assert!(open_addrs(&s).is_empty());
    }

    #[test]
    fn busy_workers_counts_only_active_execute() {
        // One target mid-Execute, one mid-LocalCacheWrite: only Execute counts as
        // a busy worker (the semaphore-bound slot).
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//a:exec")));
        s.apply(&ev(0, local_write_start("//a:write")));
        assert_eq!(executing(&s), 1);
    }

    /// Targets whose open op is `Execute`: the ones holding a worker slot.
    fn executing(s: &BuildState) -> usize {
        s.dash
            .open_ops()
            .filter(|(_, r)| r.op() == Some(Op::Execute))
            .count()
    }

    #[test]
    fn op_timeline_non_execute_op_alone_surfaces_target() {
        // A non-Execute op (cache write) with no Execute event still opens an
        // op on the target, proving the timeline is not Execute-specific.
        let mut s = BuildState::new();
        s.apply(&ev(0, local_write_start("//a:b")));
        assert_eq!(open_addrs(&s), ["//a:b"]);
        let r = rec(&s, "//a:b");
        assert_eq!((r.op(), r.op_open()), (Some(Op::LocalCacheWrite), true));
    }

    #[test]
    fn result_start_then_ok_end_completes_and_clears_in_flight() {
        let mut s = BuildState::new();
        s.apply(&ev(1, result_start("//a:b")));
        assert_eq!(s.dash.in_flight_count(), 1);
        s.apply(&ev(2, result_end("//a:b", None)));
        assert_eq!(s.completed, 1);
        assert_eq!(s.errored, 0);
        assert_eq!(s.dash.in_flight_count(), 0);
    }

    #[test]
    fn a_result_end_with_no_start_is_not_counted() {
        let mut s = BuildState::new();
        s.apply(&ev(2, result_end("//a:b", None)));
        assert_eq!((s.completed, s.errored), (0, 0));
        assert_eq!(s.dash.in_flight_count(), 0);
    }

    #[test]
    fn result_end_with_error_increments_errored() {
        let mut s = BuildState::new();
        s.apply(&ev(1, result_start("//a:b")));
        s.apply(&ev(2, result_end("//a:b", Some("boom".into()))));
        assert_eq!(s.errored, 1);
        assert_eq!(s.completed, 0);
        assert_eq!(s.dash.in_flight_count(), 0);
    }

    #[test]
    fn duplicate_result_start_is_idempotent_and_second_end_noop() {
        let mut s = BuildState::new();
        s.apply(&ev(1, result_start("//a:b")));
        s.apply(&ev(1, result_start("//a:b")));
        assert_eq!(s.dash.in_flight_count(), 1);
        s.apply(&ev(2, result_end("//a:b", None)));
        // Second ResultEnd for an already-removed addr must not double-count.
        s.apply(&ev(3, result_end("//a:b", Some("late".into()))));
        // Nor does a start after the end reopen it.
        s.apply(&ev(4, result_start("//a:b")));
        assert_eq!(s.completed, 1);
        assert_eq!(s.errored, 0);
        assert_eq!(s.dash.in_flight_count(), 0);
    }

    #[test]
    fn summary_counts_targets_not_tiers() {
        let mut s = BuildState::new();
        // Nine remote hits, each a local tier miss first, and one full miss:
        // eleven tier misses, one target miss.
        for i in 0..9 {
            let addr = format!("//r:t{i}");
            s.apply(&ev(
                1,
                BuildEventKind::LocalCacheMiss { addr: addr.clone() },
            ));
            s.apply(&ev(
                2,
                BuildEventKind::RemoteCacheHit { addr: addr.clone() },
            ));
            s.apply(&ev(2, remote_cache_hit(&addr)));
        }
        s.apply(&ev(
            3,
            BuildEventKind::LocalCacheMiss {
                addr: "//c:d".into(),
            },
        ));
        s.apply(&ev(
            3,
            BuildEventKind::RemoteCacheMiss {
                addr: "//c:d".into(),
            },
        ));
        s.apply(&ev(
            3,
            BuildEventKind::CacheOutcome {
                addr: "//c:d".into(),
                outcome: CacheOutcomeKind::Miss,
            },
        ));
        let summary = s.summary();
        assert!(
            summary.ends_with("cache 9 hit / 1 miss"),
            "summary: {summary}"
        );
    }

    /// The summary line and the header's cached count, over the stream every
    /// consumer is tested against.
    #[test]
    fn cache_counts_agree_across_consumers() {
        let mut s = BuildState::new();
        let mut fold = CacheCounts::default();
        for e in hcore::events::test_support::cache_stream() {
            fold.apply(&e.kind);
            s.apply(&e);
        }
        assert_eq!(s.cache, fold);
        let (l, r, m, _) = hcore::events::test_support::CACHE_STREAM_EXPECTED;
        assert!(
            s.summary()
                .ends_with(&format!("cache {} hit / {m} miss", l + r)),
            "{}",
            s.summary()
        );
    }

    /// A cache hit the engine later has to rebuild must stop counting as a hit.
    ///
    /// The engine decides a hit from the revision's manifest, which can outlive
    /// its blobs — a local GC, an expiry rule on a shared cache, or blobs that
    /// were never pulled and a remote that is now unreachable. It announces the
    /// hit, discovers the bytes are unreadable, and rebuilds. Counting both would
    /// let "executed + cached" exceed the number of targets, and would report a
    /// fully-offline rebuild of the whole graph as "N cached" — the header would
    /// say the opposite of what happened.
    #[test]
    fn a_hit_that_rebuilds_stops_counting_as_a_hit() {
        let mut s = BuildState::new();
        // A hit can be announced before or after its addr is matched, so
        // retraction has to survive both orders. `//a:local` and `//a:stands`
        // are matched first and hit after; `//a:remote` hits first and is
        // matched after.
        s.apply(&ev(1, matched(&["//a:local", "//a:stands"], false)));
        s.apply(&ev(2, local_cache_hit("//a:local")));
        s.apply(&ev(3, remote_cache_hit("//a:remote")));
        s.apply(&ev(4, local_cache_hit("//a:stands")));
        s.apply(&ev(5, matched(&["//a:remote"], true)));
        assert_eq!(cache3(&s), (2, 1, 0));

        // Two of the three revisions turn out to be unreadable and get rebuilt.
        use hcore::events::CacheHitKind;
        s.apply(&ev(6, hit_rebuilt("//a:local", CacheHitKind::Local)));
        s.apply(&ev(6, execute_start("//a:local")));
        s.apply(&ev(7, hit_rebuilt("//a:remote", CacheHitKind::Remote)));
        s.apply(&ev(7, execute_start("//a:remote")));
        s.apply(&ev(8, execute_end("//a:local")));
        s.apply(&ev(9, execute_end("//a:remote")));

        assert_eq!(
            cache3(&s),
            (1, 0, 2),
            "a rebuilt target is taken back off the tier it hit and counted as a miss"
        );
        assert!(
            s.summary().ends_with("cache 1 hit / 2 miss"),
            "{}",
            s.summary()
        );
        assert_eq!(s.executed, 2);
        for addr in ["//a:local", "//a:remote", "//a:stands"] {
            s.apply(&ev(10, result_end(addr, None)));
        }
        // Through the counts the user reads: `executed + cached` (2 + 1) does
        // not exceed the three targets seen, in either scope.
        for scope in [CountScope::All, CountScope::Matched] {
            let tabs = s.dash.counts(scope);
            assert_eq!(
                (tabs.done, tabs.cached),
                (2, 1),
                "{scope:?}: only the hit that stood may still be reported as cached"
            );
        }
        assert_eq!(s.counts(CountScope::Matched).cached, "1 cached");
        assert_eq!(
            rec(&s, "//a:stands").state,
            crate::tui::dashboard::RowState::Cached
        );
    }

    /// An execute is not cache accounting: a hit that stands stays a hit
    /// however many `ExecuteStart`s land, and only `CacheHitRebuilt` takes it
    /// back — the inference this replaced counted per caller.
    #[test]
    fn executing_moves_no_cache_count() {
        let mut s = BuildState::new();
        s.apply(&ev(1, local_cache_hit("//a:hit")));
        s.apply(&ev(2, execute_start("//a:fresh")));
        s.apply(&ev(3, execute_end("//a:fresh")));
        s.apply(&ev(4, execute_start("//a:hit")));
        assert_eq!(cache3(&s), (1, 0, 0));
        assert_eq!(s.executed, 1);
    }

    #[test]
    fn long_running_ops_keep_their_start() {
        // Each open op carries the instant it opened, which is what a live
        // elapsed is measured from.
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//slow:a")));
        s.apply(&ev(1_000, execute_start("//mid:b")));
        s.apply(&ev(4_000, execute_start("//fresh:c")));

        let mut open: Vec<(&str, Option<Op>, u64)> = s
            .dash
            .open_ops()
            .map(|(id, r)| (s.dash.addr(id), r.op(), r.op_since_ms))
            .collect();
        open.sort_unstable_by_key(|&(addr, ..)| addr);
        assert_eq!(
            open,
            [
                ("//fresh:c", Some(Op::Execute), 4_000),
                ("//mid:b", Some(Op::Execute), 1_000),
                ("//slow:a", Some(Op::Execute), 0),
            ]
        );
    }

    #[test]
    fn execute_end_removes_from_long_running() {
        let mut s = BuildState::new();
        s.apply(&ev(0, execute_start("//slow:a")));
        s.apply(&ev(100, execute_end("//slow:a")));
        assert!(open_addrs(&s).is_empty());
        assert_eq!(rec(&s, "//slow:a").op_ms(Op::Execute), 100);
    }

    fn scratch_wait(addr: &str, scratch: &str, access: &str, pid: Option<u32>) -> BuildEventKind {
        BuildEventKind::ScratchLockWaitStart {
            addr: addr.into(),
            scratch: scratch.into(),
            access: access.into(),
            holder_pid: pid,
        }
    }

    fn scratch_wait_end(addr: &str, scratch: &str) -> BuildEventKind {
        BuildEventKind::ScratchLockWaitEnd {
            addr: addr.into(),
            scratch: scratch.into(),
        }
    }

    /// Waits are grouped by *cache*: many blocked targets collapse into one
    /// group. A shared `exclusive` cache with hundreds of consumers produces
    /// that many simultaneous waits; a spotlight each would bury every other
    /// diagnostic.
    #[test]
    fn scratch_waits_collapse_to_one_row_per_cache() {
        let mut st = BuildState::new();
        for i in 0..40 {
            st.apply(&ev(
                1_000,
                scratch_wait(
                    &format!("//go/pkg{i}:build"),
                    "//build:gocache",
                    "exclusive",
                    None,
                ),
            ));
        }
        st.apply(&ev(
            1_000,
            scratch_wait("//other:x", "//build:gomodcache", "shared", Some(88214)),
        ));

        assert_eq!(
            st.spot.by_cache.len(),
            2,
            "one group per cache, not per waiter"
        );
        assert_eq!(
            st.spot
                .by_cache
                .get("//build:gocache")
                .map(std::collections::BTreeSet::len),
            Some(40),
        );

        assert_eq!(
            wait_words_at(&st, "//go/pkg0:build", 61_000),
            Some(("waits //build:gocache".to_owned(), "queued".to_owned())),
            "each waiter names the cache it waits on",
        );
        assert_eq!(
            wait_detail(&st, "//other:x").as_deref(),
            Some("held by pid 88214"),
            "a named holder is the diagnostic",
        );
    }

    /// A scratch wait must not render as a result-lock wait. They are different
    /// problems: one is "another process holds this target", the other is "your
    /// own build is serialized on a cache you declared `exclusive`".
    #[test]
    fn a_scratch_wait_is_not_rendered_as_a_result_lock_wait() {
        let mut st = BuildState::new();
        st.apply(&ev(
            0,
            scratch_wait("//a:x", "//build:gocache", "exclusive", None),
        ));
        assert!(
            st.lock_waits.is_empty(),
            "a scratch wait must not be folded as a result-lock wait",
        );
        let (phase, _) = wait_words_at(&st, "//a:x", 1_000).expect("a wait");
        assert_eq!(phase, "waits //build:gocache", "not `locked`");
    }

    /// The row disappears the moment the slot is acquired — it reflects only
    /// currently-blocked waits, and a stale row would send someone chasing a
    /// bottleneck that has already cleared.
    #[test]
    fn a_scratch_wait_row_clears_when_the_slot_is_acquired() {
        let mut st = BuildState::new();
        st.apply(&ev(
            1_000,
            scratch_wait("//a:x", "//build:gocache", "exclusive", None),
        ));
        assert!(wait_words_at(&st, "//a:x", 5_000).is_some());
        st.apply(&ev(9_000, scratch_wait_end("//a:x", "//build:gocache")));
        assert_eq!(wait_words_at(&st, "//a:x", 9_000), None);
        assert!(st.spot.by_cache.is_empty(), "the cache's group leaves too");

        // …and is remembered as a total, which is what the summary reports.
        assert_eq!(
            st.scratch_wait_totals(),
            vec![("//build:gocache", 1, 8_000)]
        );
    }

    /// Totals accumulate across every consumer, so the summary can say what the
    /// contention actually cost rather than what one target saw.
    #[test]
    fn scratch_wait_totals_sum_every_waiter() {
        let mut st = BuildState::new();
        for (i, since) in [1_000u64, 2_000, 3_000].iter().enumerate() {
            let addr = format!("//a:x{i}");
            st.apply(&ev(
                *since,
                scratch_wait(&addr, "//build:gocache", "exclusive", None),
            ));
            st.apply(&ev(
                since + 10_000,
                scratch_wait_end(&addr, "//build:gocache"),
            ));
        }
        assert_eq!(
            st.scratch_wait_totals(),
            vec![("//build:gocache", 3, 30_000)]
        );
    }

    /// One line per contended cache, however many targets are queued behind it.
    /// Without the dedup a large build writes one identical line per target.
    #[test]
    fn the_ci_view_announces_each_contended_cache_once() {
        let mut v = CiProgressView::new("Running");
        for i in 0..50 {
            v.apply(&ev(
                0,
                scratch_wait(&format!("//a:x{i}"), "//build:gocache", "exclusive", None),
            ));
        }
        assert_eq!(
            v.scratch_wait_announced.len(),
            1,
            "one cache, one announcement",
        );

        v.apply(&ev(
            0,
            scratch_wait("//a:y", "//build:gomodcache", "shared", None),
        ));
        assert_eq!(
            v.scratch_wait_announced.len(),
            2,
            "a second cache is its own problem and gets its own line",
        );
    }

    /// The contract that keeps a CI log readable: a warm cache says nothing,
    /// and neither does a cold one — the normal first run.
    ///
    /// Every target that uses a cache emits a prepare span, so a regression here
    /// puts one line per target into a GitHub log — thousands on a real build.
    #[test]
    fn a_warm_or_cold_prepare_logs_nothing() {
        assert_eq!(prepare_note("//build:c", "warm", 0, None), None);
        assert_eq!(prepare_note("//build:c", "cold", 0, None), None);
        assert_eq!(
            prepare_note("//build:c", "", 0, None),
            None,
            "an absent outcome from an older host is also silence",
        );
    }

    /// Every outcome that cost the build something says so.
    #[test]
    fn each_non_warm_outcome_reports_itself() {
        let note = |o: &str| prepare_note("//build:c", o, 2048, None).expect("a line");
        assert!(
            note("pulled").text.contains("pulled 2.0 KiB"),
            "{}",
            note("pulled").text
        );
        assert!(!note("pulled").warn);
        assert!(note("seeded").text.contains("seeded"));
        assert!(note("audit").text.contains("--no-scratch"));
        assert!(
            note("dropped_over_max").warn,
            "a dropped cache is a problem"
        );
        assert!(note("dropped_over_max").text.contains("runs cold"));
    }

    /// An outcome this build has never heard of is printed, not dropped. This is
    /// why `outcome` is a string: a newer host can add one without a decode
    /// failure, and the word is written to be readable.
    #[test]
    fn an_unknown_outcome_is_printed_verbatim() {
        let note = prepare_note("//build:c", "reflinked", 0, None).expect("a line");
        assert!(note.text.contains("reflinked"), "{}", note.text);
    }

    /// A failure outranks any outcome.
    #[test]
    fn a_failed_prepare_warns_with_its_error() {
        let note = prepare_note("//build:c", "pulled", 9, Some("disk full")).expect("a line");
        assert!(
            note.warn && note.text.contains("disk full"),
            "{}",
            note.text
        );
    }

    /// The inert-restore line names the command that explains it — the failure
    /// is invisible otherwise, and an operator cannot act on "it will be inert".
    #[test]
    fn the_path_mismatch_note_names_the_command_to_run() {
        let text = path_mismatch_note("//build:gocache");
        assert!(
            text.contains("heph tool scratch head //build:gocache"),
            "{text}"
        );
    }

    /// The prepare span joins the per-target timeline, so a target sitting in a
    /// multi-GB pull is visibly *doing* something rather than looking queued.
    #[test]
    fn a_slow_scratch_prepare_shows_in_the_target_breakdown() {
        let mut st = BuildState::new();
        st.apply(&ev(
            0,
            BuildEventKind::ScratchPrepareStart {
                addr: "//a:x".into(),
                scratch: "//build:gocache".into(),
            },
        ));
        assert_eq!(open_addrs(&st), ["//a:x"]);
        let r = rec(&st, "//a:x");
        assert_eq!((r.op(), r.op_open()), (Some(Op::ScratchPrepare), true));

        st.apply(&ev(
            42_000,
            BuildEventKind::ScratchPrepareEnd {
                addr: "//a:x".into(),
                scratch: "//build:gocache".into(),
                outcome: "pulled".into(),
                bytes: 0,
                path_mismatch: false,
                error: None,
            },
        ));
        assert_eq!(rec(&st, "//a:x").op_ms(Op::ScratchPrepare), 42_000);
    }

    /// Ordering is the pipeline order, so a row reads left-to-right as the
    /// target's own history rather than in event-arrival order.
    #[test]
    fn scratch_prepare_sorts_between_remote_read_and_execute() {
        let mut ops = [
            Op::Execute,
            Op::RemoteCacheWrite,
            Op::ScratchPrepare,
            Op::RemoteCacheRead,
        ];
        ops.sort_by_key(|o| o.order());
        assert_eq!(
            ops,
            [
                Op::RemoteCacheRead,
                Op::ScratchPrepare,
                Op::Execute,
                Op::RemoteCacheWrite
            ],
        );
    }

    #[test]
    fn header_shows_built_cached_failed_counts_no_running() {
        let mut v = TuiProgressView::new("L");
        // One real build (execute end ok), one cache hit, one running, one failed.
        v.apply(&ev(0, execute_start("//a:built")));
        v.apply(&ev(1, execute_end("//a:built")));
        v.apply(&ev(1, result_end("//a:built", None)));
        v.apply(&ev(2, local_cache_hit("//a:cached")));
        v.apply(&ev(2, result_end("//a:cached", None)));
        v.apply(&ev(3, result_start("//a:running")));
        v.apply(&ev(4, result_start("//a:failed")));
        v.apply(&ev(5, result_end("//a:failed", Some("boom".into()))));

        let line = line_at(&v, 100);
        // Done is Done + Cached, as the line and the tabs count it.
        assert!(line.contains("2 done"), "{line}");
        assert!(line.contains("1 cached"), "{line}");
        assert!(line.contains("1 failed"), "{line}");
        // There is no "running" count.
        assert!(!line.contains("running"), "{line}");
    }

    #[test]
    fn header_done_shows_matched_progress_with_provisional_marker() {
        let mut v = TuiProgressView::new("L");
        // Matcher still streaming: total is provisional (`~`).
        v.apply(&ev(0, matched(&["//a:x", "//a:y", "//a:z"], false)));
        v.apply(&ev(1, result_start("//a:x")));
        v.apply(&ev(2, result_end("//a:x", None)));
        let line = line_at(&v, 100);
        assert!(line.contains("1 / ~3 done"), "{line}");

        // Matcher resolves: the `~` drops.
        v.apply(&ev(3, matched(&[], true)));
        let line = line_at(&v, 100);
        assert!(line.contains("1 / 3 done"), "{line}");
        assert!(!line.contains('~'), "{line}");
    }

    #[test]
    fn ci_view_event_folds_error_count() {
        let mut v = CiProgressView::new("Running //a:b");
        v.apply(&ev(1, result_start("//a:b")));
        v.apply(&ev(2, result_end("//a:b", Some("boom".into()))));
        // The failing ResultEnd bumps the errored counter; the message itself is
        // not retained — rich diagnostics come from the failure registry.
        assert_eq!(v.core.state.errored, 1);
    }

    /// What the CI view logs for `events`, captured from its `tracing` lines.
    fn ci_view_log(events: &[BuildEvent]) -> String {
        ci_view_log_with(HolderNames::default(), events)
    }

    /// [`ci_view_log`], naming lock holders with `names`.
    fn ci_view_log_with(names: HolderNames, events: &[BuildEvent]) -> String {
        ci_view_log_of(CiProgressView::new("Running"), names, events)
    }

    /// [`ci_view_log`] of a view built by the test.
    fn ci_view_log_of(mut v: CiProgressView, names: HolderNames, events: &[BuildEvent]) -> String {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .with_target(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            // As the backend does, before any event.
            v.begin();
            v.name_lock_holders(names.clone());
            for e in events {
                // As the backend's receiver does, before the fold.
                if let Some(pid) = crate::tui::session::holder::event_holder_pid(e) {
                    names.resolve_blocking(pid);
                }
                v.apply(e);
            }
        });
        let bytes = buf.0.lock().expect("log buffer").clone();
        String::from_utf8(bytes).expect("utf-8 log")
    }

    /// The CI view opens with no header line: its label ("Running //p:chatty")
    /// is not logged, and a run whose targets start and pass logs nothing
    /// before its summary.
    #[test]
    fn ci_view_prints_no_label_header() {
        let log = ci_view_log_of(
            CiProgressView::new("Running //p:chatty"),
            HolderNames::default(),
            &[
                ev(1, result_start("//p:chatty")),
                ev(2, result_end("//p:chatty", None)),
            ],
        );
        assert_eq!(log, "", "{log}");
    }

    /// The line backend's lock and scratch notices name the live session
    /// holding for the pid; with none recorded (here: it ended), the pid alone.
    #[test]
    fn ci_view_lock_notices_name_the_holding_session() {
        let pid = std::process::id();
        let events = [
            ev(0, lock_wait_start("//a:b", pid)),
            ev(
                0,
                scratch_wait("//a:c", "//build:gocache", "exclusive", Some(pid)),
            ),
        ];

        let root = tempfile::tempdir().expect("tempdir");
        let holder = live_session(root.path());
        let held_by = format!("held by session {} (pid {pid})", holder.id());
        let log = ci_view_log_with(names_in(root.path()), &events);
        assert!(
            log.contains(&format!("waiting on result lock for //a:b ({held_by})")),
            "{log}"
        );
        assert!(
            log.contains(&format!("waiting on scratch //build:gocache, {held_by}")),
            "{log}"
        );

        holder.finish(Some(0), false);
        let log = ci_view_log_with(names_in(root.path()), &events);
        assert!(
            log.contains(&format!(
                "waiting on result lock for //a:b (held by pid {pid})"
            )),
            "{log}"
        );
        assert!(
            log.contains(&format!(
                "waiting on scratch //build:gocache, held by pid {pid}\n"
            )),
            "{log}"
        );
    }

    /// Admission waits stay off the CI log, and so does the execute that
    /// follows: no per-target "running" line.
    #[test]
    fn ci_view_logs_nothing_for_an_admission_wait() {
        let log = ci_view_log(&[
            ev(1_000, admission_wait_start("//a:b")),
            ev(
                42_000,
                BuildEventKind::AdmissionWaitEnd {
                    addr: "//a:b".into(),
                },
            ),
        ]);
        assert_eq!(log, "", "an admission wait logged: {log}");

        let log = ci_view_log(&[
            ev(1_000, admission_wait_start("//a:b")),
            ev(2_000, execute_start("//a:b")),
        ]);
        assert_eq!(log, "", "an execute start logged: {log}");
    }

    /// A failing `ResultEnd` with a status and a log tail.
    fn failed_end(
        addr: &str,
        upstream_of: Option<&str>,
        cancelled: bool,
        tail: Option<&str>,
    ) -> BuildEventKind {
        BuildEventKind::ResultEnd {
            left_behind: None,
            addr: addr.into(),
            error: Some(format!("{addr}: exit status: 1")),
            upstream_of: upstream_of.map(Into::into),
            exit_status: Some("exit status: 1".into()),
            log_tail: tail.map(|text| hcore::events::LogTailData {
                text: text.into(),
                start_line: 1,
            }),
            timed_out: None,
            cancelled,
        }
    }

    /// What the CI view logs for `events` with `--log-lines n`.
    fn ci_live_log(n: usize, events: &[BuildEventKind]) -> String {
        let events: Vec<BuildEvent> = events.iter().cloned().map(|k| ev(1_000, k)).collect();
        ci_view_log_of(
            CiProgressView::new("Running").with_log_lines(n),
            HolderNames::default(),
            &events,
        )
    }

    const TAIL: &str = "one\ntwo\nthree\nfour\nfive\n";

    /// A root failure prints its block the moment it fails: the report's
    /// header, then exactly the last `--log-lines` lines of its tail.
    #[test]
    fn ci_view_prints_a_root_failures_tail_as_it_fails() {
        let end = failed_end("//a:b", None, false, Some(TAIL));
        assert_eq!(
            ci_live_log(3, &[result_start("//a:b"), end.clone()]),
            "ERROR ✗ //a:b · exit status: 1\
             \n  │ three\
             \n  │ four\
             \n  │ five\n",
        );
        // A tail shorter than --log-lines: what the event carried.
        let log = ci_live_log(50, std::slice::from_ref(&end));
        assert_eq!(log.matches("  │ ").count(), 5, "{log}");
        // --log-lines 0: the header alone.
        assert_eq!(ci_live_log(0, &[end]), "ERROR ✗ //a:b · exit status: 1\n");
    }

    /// The block ends with the rows the end-of-run box would have carried,
    /// after the tail and in the box's order, and the view says it printed
    /// it: the end of the run then leaves the box out. A collateral failure
    /// and a stop print nothing, so the box stays theirs.
    #[test]
    fn ci_view_live_block_carries_the_box_rows() {
        let end = BuildEventKind::ResultEnd {
            addr: "//a:b".into(),
            error: Some("//a:b: exit status: 1".into()),
            upstream_of: None,
            exit_status: Some("exit status: 1".into()),
            log_tail: Some(hcore::events::LogTailData {
                text: TAIL.into(),
                start_line: 1,
            }),
            timed_out: None,
            cancelled: false,
            left_behind: Some(Box::new(hcore::events::LeftBehindData {
                log_path: Some("/h/failures/a_b/log.txt".into()),
                sandbox: None,
                outputs: Some("/h/failures/a_b/out".into()),
                fixes: None,
                log_readable: Some(true),
            })),
        };
        assert_eq!(
            ci_live_log(2, std::slice::from_ref(&end)),
            "ERROR ✗ //a:b · exit status: 1\
             \n  │ four\
             \n  │ five\
             \n  full log: /h/failures/a_b/log.txt\
             \n  outputs: /h/failures/a_b/out\
             \n  log: heph log //a:b\n",
        );

        let mut view = CiProgressView::new("Running");
        for kind in [
            end,
            failed_end("//a:c", Some("//a:b"), false, Some(TAIL)),
            failed_end("//a:d", None, true, Some(TAIL)),
        ] {
            view.apply(&ev(1_000, kind));
        }
        assert_eq!(view.failures_printed(), HashSet::from(["//a:b".to_owned()]));
    }

    /// A same-addr `ResultEnd` double-fire prints one block.
    #[test]
    fn ci_view_prints_a_root_failure_once() {
        let end = failed_end("//a:b", None, false, Some(TAIL));
        let log = ci_live_log(1, &[end.clone(), end]);
        assert_eq!(log.matches('✗').count(), 1, "{log}");
    }

    /// Collateral failures and stops print nothing live: the root's block
    /// explains the first, and a stop is not a failure.
    #[test]
    fn ci_view_prints_nothing_for_an_upstream_failure_or_a_stop() {
        let upstream = failed_end("//a:c", Some("//a:b"), false, Some(TAIL));
        let stopped = failed_end("//a:d", None, true, Some(TAIL));
        assert_eq!(ci_live_log(10, &[upstream, stopped]), "");
    }

    /// A timeout names its limit as the report does; a failure with no
    /// status shows its error; control bytes in the tail, the address and
    /// the error never reach the log.
    #[test]
    fn ci_view_live_block_words_and_sanitizing() {
        let timed_out = BuildEventKind::ResultEnd {
            left_behind: None,
            addr: "//e2e:it".into(),
            error: Some("timed out".into()),
            upstream_of: None,
            exit_status: None,
            log_tail: Some(hcore::events::LogTailData {
                text: "a\x1b[31mred\x1b[0m\r\nb\x07ell\n10%\r20%\n".into(),
                start_line: 1,
            }),
            timed_out: Some(hcore::events::TimedOutData {
                timeout_ms: 600_000,
                abandoned: false,
            }),
            cancelled: false,
        };
        assert_eq!(
            ci_live_log(10, std::slice::from_ref(&timed_out)),
            "ERROR ✗ //e2e:it · timed out after 10m (its timeout)\
             \n  │ ared\
             \n  │ bell\
             \n  │ 20%\n",
        );
        // A timeout's tail is held to --log-lines like any failure's.
        assert_eq!(
            ci_live_log(2, &[timed_out]),
            "ERROR ✗ //e2e:it · timed out after 10m (its timeout)\
             \n  │ bell\
             \n  │ 20%\n",
        );

        let no_status = BuildEventKind::ResultEnd {
            left_behind: None,
            addr: "//a:\x1b[2Jb".into(),
            error: Some("staging\x1b[2J failed".into()),
            upstream_of: None,
            exit_status: None,
            log_tail: Some(hcore::events::LogTailData {
                text: "\n\n".into(),
                start_line: 1,
            }),
            timed_out: None,
            cancelled: false,
        };
        assert_eq!(
            ci_live_log(10, &[no_status]),
            "ERROR ✗ //a:b\
             \n  │ staging failed\
             \n  │ empty log\n",
        );
    }

    /// A stop reads first on the CI summary line, as on the TUI's final
    /// line; without one the summary is unchanged.
    #[test]
    fn ci_summary_says_stopped_after_a_stop() {
        let mut v = CiProgressView::new("Running");
        v.apply(&ev(1_000, execute_start("//a:b")));
        let plain = v.summary_line();
        assert!(plain.starts_with("done "), "{plain}");
        v.set_stopped();
        assert_eq!(v.summary_line(), format!("stopped · {plain}"));
    }

    #[test]
    fn human_bytes_formats_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024u64), "3.0 GiB");
    }

    fn gc_swept(revisions_removed: usize, bytes_removed: u64) -> BuildEventKind {
        BuildEventKind::GcTargetSwept {
            revisions_removed,
            bytes_removed,
        }
    }

    #[test]
    fn gc_header_folds_targets_revisions_bytes() {
        let core = BuildState::new();
        let mut h = GcHeader::new("GC");

        h.apply(&ev(0, gc_swept(2, 1024)));
        h.apply(&ev(1, gc_swept(0, 0))); // zero-removal target still counts as explored

        let seg = ProgressHeader::segment(&h);
        assert!(seg.contains("2 targets"), "{seg}");
        assert!(seg.contains("1.0 KiB"), "{seg}");
        assert_eq!(h.label(), "GC");
        assert!(!h.last_render(&core, CountScope::Matched).is_empty());
    }

    /// The failure-log sweep is appended after the unchanged revision segment,
    /// and a sweep without one (`clean`) keeps the old text exactly.
    #[test]
    fn gc_header_appends_failure_logs() {
        let core = BuildState::new();
        let mut h = GcHeader::new("GC");
        h.apply(&ev(0, gc_swept(2, 1024)));
        let before = ProgressHeader::segment(&h);
        assert_eq!(before, "1 targets · 1.0 KiB freed");

        h.apply(&ev(
            1,
            BuildEventKind::GcFailureLogsSwept {
                removed: 3,
                bytes_removed: 2048,
            },
        ));
        let after = ProgressHeader::segment(&h);
        assert_eq!(after, format!("{before} · 3 failure logs, 2.0 KiB freed"));

        // Only failure logs removed: still worth a final line.
        let mut only = GcHeader::new("GC");
        only.apply(&ev(
            0,
            BuildEventKind::GcFailureLogsSwept {
                removed: 1,
                bytes_removed: 5,
            },
        ));
        assert!(!only.last_render(&core, CountScope::Matched).is_empty());
    }

    #[expect(clippy::too_many_arguments, reason = "the event's fields")]
    fn gc_cache_db(
        file_bytes: u64,
        live_bytes: u64,
        free_bytes: u64,
        mode: &str,
        action: &str,
        freed_bytes: u64,
        needed_bytes: u64,
        available_bytes: u64,
        detail: Option<&str>,
    ) -> BuildEventKind {
        BuildEventKind::GcCacheDb {
            file_bytes,
            wal_bytes: 0,
            live_bytes,
            free_bytes,
            mode: mode.to_string(),
            action: action.to_string(),
            freed_bytes,
            needed_bytes,
            available_bytes,
            detail: detail.map(str::to_string),
        }
    }

    /// The `cache.db` pass is the last segment, in each of its outcomes, and
    /// alone is still worth a final line.
    #[test]
    fn gc_header_renders_the_cache_db_pass() {
        const GIB: u64 = 1 << 30;
        let core = BuildState::new();
        let render = |kind: BuildEventKind| {
            let mut h = GcHeader::new("GC");
            h.apply(&ev(0, gc_swept(1, 1024)));
            h.apply(&ev(1, kind));
            ProgressHeader::segment(&h)
        };

        assert_eq!(
            render(gc_cache_db(
                12 * GIB,
                11 * GIB,
                0,
                "incremental",
                "converted",
                9 * GIB,
                0,
                0,
                None
            )),
            "1 targets · 1.0 KiB freed · cache.db 12.0 GiB (live 11.0 GiB, free 0 B, \
             incremental): converted, freed 9.0 GiB"
        );
        assert_eq!(
            render(gc_cache_db(
                21 * GIB,
                12 * GIB,
                9 * GIB,
                "none",
                "skipped",
                0,
                12 * GIB,
                GIB,
                Some("needs 12.0 GiB free in /h, has 1.0 GiB")
            )),
            "1 targets · 1.0 KiB freed · cache.db 21.0 GiB (live 12.0 GiB, free 9.0 GiB, \
             none): not converted: needs 12.0 GiB free, 1.0 GiB available"
        );
        let locked = render(gc_cache_db(
            GIB,
            GIB,
            0,
            "none",
            "skipped",
            0,
            0,
            0,
            Some("another heph holds the cache's migration lock"),
        ));
        assert!(
            locked.ends_with("none): skipped: another heph holds the cache's migration lock"),
            "{locked}"
        );
        let partial = render(gc_cache_db(
            GIB,
            GIB,
            1024,
            "incremental",
            "vacuumed",
            2048,
            0,
            0,
            Some("stopped early; the next gc continues"),
        ));
        assert!(
            partial.ends_with(
                "incremental): vacuumed, freed 2.0 KiB (stopped early; the next gc continues)"
            ),
            "{partial}"
        );
        let failed = render(gc_cache_db(
            0,
            0,
            0,
            "",
            "failed",
            0,
            0,
            0,
            Some("disk I/O error"),
        ));
        assert!(
            failed.ends_with(" · cache.db: vacuum failed: disk I/O error"),
            "{failed}"
        );

        // Nothing else swept: the cache.db line still prints.
        let mut only = GcHeader::new("GC");
        only.apply(&ev(
            0,
            gc_cache_db(GIB, GIB, 0, "incremental", "unchanged", 0, 0, 0, None),
        ));
        let last = only.last_render(&core, CountScope::Matched);
        assert!(last.ends_with("incremental): unchanged"), "{last}");
    }

    #[test]
    fn build_header_segment_has_counts_and_ignores_gc() {
        let mut core = BuildState::new();
        core.apply(&ev(1, execute_start("//a:b")));
        core.apply(&ev(1, execute_end("//a:b")));
        core.apply(&ev(1, result_end("//a:b", None)));
        let before = core.counts_segment(CountScope::Matched);
        assert_eq!(before, "1 done · 0 cached · 0 failed");
        // A gc event must not perturb the build counters.
        core.apply(&ev(2, gc_swept(9, 9)));
        assert_eq!(core.counts_segment(CountScope::Matched), before);
    }

    #[test]
    fn has_activity_gates_final_summary() {
        // Empty view (e.g. an inspect/query run): no summary should print.
        let empty = TuiProgressView::new("Spec //a:b");
        assert!(!empty.state.has_activity());

        // Any observed event flips it on.
        let mut active = TuiProgressView::new("Running //a:b");
        active.apply(&ev(1, result_start("//a:b")));
        active.apply(&ev(2, result_end("//a:b", None)));
        assert!(active.state.has_activity());
    }

    fn matched(addrs: &[&str], complete: bool) -> BuildEventKind {
        BuildEventKind::Matched {
            addrs: addrs.iter().map(|s| (*s).to_string()).collect(),
            complete,
        }
    }

    /// An entrypoint's "Invoking …" is on the line while the matcher streams
    /// and after it completed, while its dependencies build and up to the
    /// pause that hands it the terminal, and leaves with the run's end. An
    /// ordinary label ("Running //…") is never on the line: the command
    /// above already names it.
    #[test]
    fn a_standing_label_outlives_the_matcher() {
        struct Invoking(bool);
        impl ProgressHeader for Invoking {
            fn label(&self) -> String {
                "Invoking //app:tty".to_owned()
            }
            fn label_stands(&self) -> bool {
                self.0
            }
            fn last_render(&self, core: &BuildState, scope: CountScope) -> String {
                BuildHeader::new(String::new()).last_render(core, scope)
            }
        }
        let text = |v: &TuiProgressView| line_text(&v.line("⠋", 1, 200, true));
        for stands in [true, false] {
            let mut v = TuiProgressView::with_header(Box::new(Invoking(stands)));
            v.apply(&ev(0, matched(&["//app:tty"], false)));
            assert_eq!(
                text(&v).contains("Invoking //app:tty"),
                stands,
                "streaming, stands={stands}: {}",
                text(&v)
            );
            v.apply(&ev(1, matched(&[], true)));
            v.apply(&ev(2, result_start("//dep:slow")));
            assert_eq!(
                text(&v).contains("Invoking //app:tty"),
                stands,
                "stands={stands}: {}",
                text(&v)
            );
            v.set_finished(3);
            assert!(!text(&v).contains("Invoking"), "{}", text(&v));
        }
    }

    /// The exit summary follows the `a` scope toggle: a user who switched the
    /// header to `All` reads the all-targets counts after teardown, not a
    /// hardcoded matched-scope line that disagrees with the header they were
    /// just looking at.
    #[test]
    fn final_summary_follows_scope_toggle() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//a:b"], true)));
        v.apply(&ev(1, result_start("//a:b")));
        v.apply(&ev(2, result_end("//a:b", None)));
        // A transitive dep: outside the matched set, counted only under `All`.
        v.apply(&ev(3, result_start("//dep:x")));
        v.apply(&ev(4, result_end("//dep:x", None)));

        // The production seam itself — `last_render` prints exactly this.
        let segment = |v: &TuiProgressView| v.final_summary_segment();
        assert!(segment(&v).contains("1 / 1 done"), "{}", segment(&v));
        assert!(
            !segment(&v).contains("all targets"),
            "matched scope must not be labeled: {}",
            segment(&v)
        );
        v.toggle_scope();
        assert!(segment(&v).contains("2 / 2 done"), "{}", segment(&v));
        // The line outlives the header, so the non-default scope names itself.
        assert!(segment(&v).ends_with("· all targets"), "{}", segment(&v));
        // Toggling back restores the matched-scope summary.
        v.toggle_scope();
        assert!(segment(&v).contains("1 / 1 done"), "{}", segment(&v));
    }

    #[test]
    fn matched_tracks_done_against_matched_set_only() {
        let mut s = BuildState::new();
        s.apply(&ev(0, matched(&["//a:b", "//c:d"], true)));
        assert_eq!(s.matched_total(), Some(2));
        assert!(s.has_activity(), "Matched alone counts as activity");

        // A transitive dep (not in the matched set) does not advance progress.
        s.apply(&ev(1, result_start("//dep:x")));
        s.apply(&ev(2, result_end("//dep:x", None)));
        assert!(s.summary().contains("matched 0 / 2"), "{}", s.summary());

        // A matched target completing advances it.
        s.apply(&ev(3, result_start("//a:b")));
        s.apply(&ev(4, result_end("//a:b", None)));
        assert!(s.summary().contains("matched 1 / 2"), "{}", s.summary());

        // A duplicate ResultEnd for the same matched addr is idempotent.
        s.apply(&ev(5, result_end("//a:b", None)));
        assert!(s.summary().contains("matched 1 / 2"), "{}", s.summary());
    }

    /// The target's local-hit outcome — what the header counts, not the tier
    /// event.
    fn local_cache_hit(addr: &str) -> BuildEventKind {
        BuildEventKind::CacheOutcome {
            addr: addr.into(),
            outcome: CacheOutcomeKind::LocalHit,
        }
    }

    fn remote_cache_hit(addr: &str) -> BuildEventKind {
        BuildEventKind::CacheOutcome {
            addr: addr.into(),
            outcome: CacheOutcomeKind::RemoteHit,
        }
    }

    fn hit_rebuilt(addr: &str, was: hcore::events::CacheHitKind) -> BuildEventKind {
        BuildEventKind::CacheHitRebuilt {
            addr: addr.into(),
            was,
        }
    }

    /// `(local_hits, remote_hits, misses)` as the summary reads them.
    fn cache3(s: &BuildState) -> (u64, u64, u64) {
        (
            s.cache.local_hits(),
            s.cache.remote_hits(),
            s.cache.misses(),
        )
    }

    /// The two matched intersections (finished, finished from the cache)
    /// computed the slow way, over every record. The oracle for the bucket
    /// counts the model keeps.
    fn scan_matched(s: &BuildState) -> (usize, usize) {
        use crate::tui::dashboard::RowState as S;
        let recs: Vec<_> = (0..s.dash.id_count())
            .filter_map(|id| s.dash.record(u32::try_from(id).ok()?))
            .filter(|r| r.matched())
            .collect();
        (
            recs.iter()
                .filter(|r| !matches!(r.state, S::Queued | S::Blocked | S::Running))
                .count(),
            recs.iter().filter(|r| r.state == S::Cached).count(),
        )
    }

    /// The same two intersections as the render path reports them.
    fn rendered_matched(s: &BuildState) -> (usize, usize) {
        let (done, _, _) = s.matched_progress().expect("a Matched event has landed");
        (done, s.dash.counts(CountScope::Matched).cached)
    }

    #[test]
    fn matched_counters_track_the_full_scan_under_out_of_order_events() {
        // `matched ∩ finished` and `matched ∩ cached` are bucket counts kept
        // as events fold rather than rescanned every frame, so they have to
        // move at *both* edges of each intersection. `Engine::result` really
        // does emit a target's `ResultEnd` before the `Matched` event that
        // names it, so a count kept only on the result edge undercounts. Pin
        // both against a full scan of the records, after every interleaving.
        let mut s = BuildState::new();
        let mut clock = 0u64;
        let mut apply = |s: &mut BuildState, kind| {
            clock += 1;
            s.apply(&ev(clock, kind));
        };

        // Edge 1: these finish (and one hits cache) BEFORE anything is matched.
        apply(&mut s, result_start("//early:a"));
        apply(&mut s, local_cache_hit("//early:a"));
        apply(&mut s, result_end("//early:a", None));
        apply(&mut s, result_start("//early:b"));
        apply(&mut s, result_end("//early:b", None));
        // A transitive dep that is never matched — proves these are
        // intersections, not running totals.
        apply(&mut s, result_start("//dep:x"));
        apply(&mut s, remote_cache_hit("//dep:x"));
        apply(&mut s, result_end("//dep:x", None));

        apply(
            &mut s,
            matched(
                &["//early:a", "//early:b", "//late:c", "//late:d", "//late:e"],
                false,
            ),
        );
        assert_eq!(
            rendered_matched(&s),
            scan_matched(&s),
            "matched arrives last"
        );
        assert_eq!(rendered_matched(&s), (2, 1));

        // Edge 2: these finish AFTER the Matched event that named them.
        apply(&mut s, result_start("//late:c"));
        apply(&mut s, remote_cache_hit("//late:c"));
        apply(&mut s, result_end("//late:c", None));
        assert_eq!(
            rendered_matched(&s),
            scan_matched(&s),
            "matched arrives first"
        );
        assert_eq!(rendered_matched(&s), (3, 2));

        // A cache hit on a matched target that has not finished moves
        // neither: cached is a target finished from the cache.
        apply(&mut s, local_cache_hit("//late:d"));
        assert_eq!(
            rendered_matched(&s),
            scan_matched(&s),
            "cache hit, no result"
        );
        assert_eq!(rendered_matched(&s), (3, 2));

        // A matched target that *fails* still advances the done count — the
        // header reads `4 / 5 done · … · 1 failed`. The scan oracle cannot
        // see a regression here (it reads the same states), so this one
        // needs the literal.
        apply(&mut s, result_start("//late:e"));
        apply(&mut s, result_end("//late:e", Some("boom".into())));
        assert_eq!(rendered_matched(&s), scan_matched(&s), "matched failure");
        assert_eq!(rendered_matched(&s), (4, 2));

        // Repeats on every path are idempotent: a duplicate `ResultEnd`, a second
        // cache hit on an already-cached addr, and a `Matched` event re-naming an
        // addr already in the set.
        apply(&mut s, result_end("//early:a", None));
        apply(&mut s, remote_cache_hit("//early:a"));
        apply(&mut s, matched(&["//early:a", "//late:c"], true));
        assert_eq!(rendered_matched(&s), scan_matched(&s), "duplicate events");
        assert_eq!(rendered_matched(&s), (4, 2));
    }

    #[test]
    fn count_scope_toggles_done_and_cached_between_matched_and_all() {
        let mut s = BuildState::new();
        // One matched target plus a transitive dep, both finishing with a cache
        // hit, and a third target still in flight.
        s.apply(&ev(0, matched(&["//a:b"], true)));
        s.apply(&ev(1, result_start("//a:b")));
        s.apply(&ev(2, local_cache_hit("//a:b")));
        s.apply(&ev(3, result_end("//a:b", None)));
        s.apply(&ev(4, result_start("//dep:x")));
        s.apply(&ev(5, local_cache_hit("//dep:x")));
        s.apply(&ev(6, result_end("//dep:x", None)));
        s.apply(&ev(7, result_start("//run:y")));

        // Matched scope: only the matched target is counted.
        let (done, cached, _) = s.count_fields(CountScope::Matched);
        assert_eq!(done, "1 / 1 done", "matched done");
        assert_eq!(cached, "1 cached", "matched cached");

        // All scope: every observed target — both finished deps over the running
        // total (incl. the in-flight one), and both cache hits.
        let (done, cached, _) = s.count_fields(CountScope::All);
        assert_eq!(done, "2 / 3 done", "all done");
        assert_eq!(cached, "2 cached", "all cached");
    }

    /// The three count fields `(done, cached, failed)` as the line draws them
    /// in `scope` (the live-only waiting count is not in the summary).
    fn header_count_fields(v: &mut TuiProgressView, scope: CountScope) -> (String, String, String) {
        v.list.scope = scope;
        let line = line_at(v, 100);
        let field = |suffix: &str| {
            line.split(" · ")
                .find(|seg| seg.ends_with(suffix))
                .unwrap_or_default()
                .to_owned()
        };
        (field(" done"), field(" cached"), field(" failed"))
    }

    /// Shorthand for a `(done, cached, failed)` golden.
    fn fields(done: &str, cached: &str, failed: &str) -> (String, String, String) {
        (done.to_string(), cached.to_string(), failed.to_string())
    }

    #[test]
    fn split_header_count_fields_match_the_plain_count_fields() {
        // The line draws its counts from the dashboard's tab counts;
        // `count_fields` renders `Counts` as flat strings for the final
        // summary. The two must stay character-identical in every scope and in
        // every branch of the done segment.
        let mut v = TuiProgressView::new("L");

        // Before any Matched event: the done segment has no denominator, and
        // done is Done + Cached of every target.
        v.apply(&ev(0, execute_start("//a:built")));
        v.apply(&ev(1, execute_end("//a:built")));
        v.apply(&ev(1, result_end("//a:built", None)));
        v.apply(&ev(2, local_cache_hit("//a:cached")));
        v.apply(&ev(2, result_end("//a:cached", None)));
        v.apply(&ev(3, result_start("//a:failed")));
        v.apply(&ev(4, result_end("//a:failed", Some("boom".into()))));
        for scope in [CountScope::Matched, CountScope::All] {
            assert_eq!(
                header_count_fields(&mut v, scope),
                v.state.count_fields(scope)
            );
        }
        // The no-denominator branch is the one the done segment re-sources, so
        // pin it to a literal rather than only cross-checking it.
        assert_eq!(
            header_count_fields(&mut v, CountScope::Matched),
            fields("2 done", "1 cached", "1 failed"),
        );
        assert_eq!(
            header_count_fields(&mut v, CountScope::All),
            fields("2 / 3 done", "1 cached", "1 failed"),
        );

        // Provisional matched total (`~`): three matched, one finished from
        // cache, one with a hit still running. A hit counts once its target
        // ends; the unmatched failure is out of the matched scope.
        v.apply(&ev(5, matched(&["//m:x", "//m:y", "//m:z"], false)));
        v.apply(&ev(6, result_start("//m:x")));
        v.apply(&ev(7, local_cache_hit("//m:x")));
        v.apply(&ev(8, local_cache_hit("//m:y")));
        v.apply(&ev(9, result_end("//m:x", None)));
        v.apply(&ev(10, result_start("//m:y")));
        for scope in [CountScope::Matched, CountScope::All] {
            assert_eq!(
                header_count_fields(&mut v, scope),
                v.state.count_fields(scope)
            );
        }
        assert_eq!(
            header_count_fields(&mut v, CountScope::Matched),
            fields("1 / ~3 done", "1 cached", "0 failed"),
        );
        assert_eq!(
            header_count_fields(&mut v, CountScope::All),
            fields("3 / 6 done", "2 cached", "1 failed"),
        );

        // Matcher resolves: the `~` drops on both paths.
        v.apply(&ev(11, matched(&[], true)));
        for scope in [CountScope::Matched, CountScope::All] {
            assert_eq!(
                header_count_fields(&mut v, scope),
                v.state.count_fields(scope)
            );
        }
        assert_eq!(
            header_count_fields(&mut v, CountScope::Matched),
            fields("1 / 3 done", "1 cached", "0 failed"),
        );
    }

    #[test]
    fn toggle_scope_flips_view_count_scope() {
        let mut v = TuiProgressView::new("L");
        assert_eq!(v.list.scope, CountScope::Matched);
        v.toggle_scope();
        assert_eq!(v.list.scope, CountScope::All);
        v.toggle_scope();
        assert_eq!(v.list.scope, CountScope::Matched);
    }

    #[test]
    fn matched_progress_is_order_independent() {
        // Regression: in `Engine::result` the spawned target tasks emit their
        // ResultEnd before the `Matched` event lands. Matched progress must
        // still reconcile against results that already finished.
        let mut s = BuildState::new();
        s.apply(&ev(1, result_start("//a:b")));
        s.apply(&ev(2, result_end("//a:b", None)));
        s.apply(&ev(3, result_start("//c:d")));
        s.apply(&ev(4, result_end("//c:d", None)));
        // Matched arrives last, after both matched targets already finished.
        s.apply(&ev(5, matched(&["//a:b", "//c:d"], true)));
        assert!(s.summary().contains("matched 2 / 2"), "{}", s.summary());
    }

    #[test]
    fn matched_accumulates_incrementally_with_provisional_marker() {
        let mut s = BuildState::new();
        // Stream matches one at a time; total is provisional (`~`) meanwhile.
        s.apply(&ev(1, matched(&["//a:b"], false)));
        assert!(s.summary().contains("matched 0 / ~1"), "{}", s.summary());
        s.apply(&ev(2, matched(&["//c:d"], false)));
        assert_eq!(s.matched_total(), Some(2));
        assert!(s.summary().contains("matched 0 / ~2"), "{}", s.summary());

        // Final marker drops the `~` without adding addrs.
        s.apply(&ev(3, matched(&[], true)));
        assert!(s.summary().contains("matched 0 / 2"), "{}", s.summary());
        assert!(!s.summary().contains('~'), "{}", s.summary());
    }

    #[test]
    fn human_elapsed_starts_compact_then_grows_monotonically() {
        // Seconds band is natural width (compact start).
        assert_eq!(human_elapsed(9_000), "9s");
        assert_eq!(human_elapsed(59_000), "59s");
        // Past a minute, width is padded so the field never shrinks.
        for ms in [65_000, 3_600_000, 90_000_000] {
            assert!(human_elapsed(ms).chars().count() >= ELAPSED_MIN_WIDTH);
        }
        // Width is non-decreasing as time advances across band boundaries.
        let mut prev = 0usize;
        for secs in [0u64, 30, 59, 60, 600, 3_599, 3_600, 86_399, 86_400] {
            let w = human_elapsed(secs * 1_000).chars().count();
            assert!(w >= prev, "width shrank at {secs}s: {prev} → {w}");
            prev = w;
        }
        assert!(human_elapsed(65_000).contains("1m05s"));
        assert!(human_elapsed(3_725_000).contains("1h02m"));
        assert!(human_elapsed(90_000_000).contains("1d01h"));
    }

    #[test]
    fn human_elapsed_ms_keeps_millis_under_a_minute() {
        assert_eq!(human_elapsed_ms(9_123), "9.123s");
        assert_eq!(human_elapsed_ms(59_007), "59.007s");
        assert_eq!(human_elapsed_ms(500), "0.500s");
        // Past a minute it falls back to the coarse seconds band.
        assert_eq!(human_elapsed_ms(65_000), human_elapsed(65_000));
    }

    #[test]
    fn elapsed_anchors_to_first_event() {
        let mut s = BuildState::new();
        assert_eq!(s.elapsed_ms(5_000), 0, "no anchor before any event");
        s.apply(&ev(2_000, result_start("//a:b")));
        // Later events do not move the anchor backward or forward.
        s.apply(&ev(4_000, result_end("//a:b", None)));
        assert_eq!(s.elapsed_ms(9_000), 7_000);
    }

    /// `heph session show`'s frame: the line, then the dashboard's All list
    /// with its titles, capped at 15 rows, no key hints, measured against
    /// the clock it is given.
    #[test]
    fn snapshot_lines_render_one_plain_frame() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//a:slow", "//a:locked"], true)));
        v.apply(&ev(0, result_start("//a:slow")));
        v.apply(&ev(
            0,
            BuildEventKind::ResultLockWaitStart {
                addr: "//a:locked".into(),
                holder_pid: Some(4242),
                in_use_by_readers: false,
            },
        ));
        let text: Vec<String> = v
            .snapshot_lines(65_000, 100)
            .iter()
            .map(ToString::to_string)
            .collect();
        let line = text.first().expect("the line");
        assert!(line.contains("1m05s · 0 / 2 done"), "{line}");
        assert!(!line.contains('⇥'), "no key hint on a print: {line}");
        assert!(
            text.get(1).is_some_and(|t| t.contains("TARGET")),
            "{text:?}"
        );
        for addr in ["//a:slow", "//a:locked"] {
            assert_eq!(
                text.iter().skip(1).filter(|l| l.contains(addr)).count(),
                1,
                "{addr} listed once: {text:?}"
            );
        }
        assert!(
            text.iter().all(|l| !l.trim().is_empty()),
            "no padding rows: {text:?}"
        );
        assert!(
            !text
                .iter()
                .any(|l| l.contains("esc") || l.contains("ctrl-c")),
            "no hint row: {text:?}"
        );

        // Capped at 15 rows, whatever the build's size.
        let mut big = TuiProgressView::new("L");
        let addrs: Vec<String> = (0..40).map(|i| format!("//b:t{i:02}")).collect();
        let refs: Vec<&str> = addrs.iter().map(String::as_str).collect();
        big.apply(&ev(0, matched(&refs, true)));
        let rows = big
            .snapshot_lines(1_000, 100)
            .iter()
            .filter(|l| l.to_string().contains("//b:t"))
            .count();
        assert_eq!(rows, usize::from(SNAPSHOT_ROWS));
    }

    fn lock_wait_start(addr: &str, pid: u32) -> BuildEventKind {
        BuildEventKind::ResultLockWaitStart {
            addr: addr.into(),
            holder_pid: Some(pid),
            in_use_by_readers: false,
        }
    }

    #[test]
    fn header_items_are_freestanding_without_separators() {
        // Each count field must carry no ` · ` — `counts_segment` owns the
        // joins.
        let mut core = BuildState::new();
        core.apply(&ev(0, matched(&["//a:b"], false)));
        core.apply(&ev(1, execute_start("//a:b")));
        for scope in [CountScope::Matched, CountScope::All] {
            let (done, cached, failed) = core.count_fields(scope);
            for text in [done, cached, failed] {
                assert!(!text.contains('·'), "field baked in a separator: {text}");
            }
        }
        assert_eq!(
            core.counts_segment(CountScope::Matched),
            "0 / ~1 done · 0 cached · 0 failed"
        );
    }

    #[test]
    fn slash_filters_the_dashboard_list_enter_keeps_esc_clears() {
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//web:server", "//api:server"], true)));
        for (t, addr) in [(1, "//web:server"), (3, "//api:server")] {
            v.apply(&ev(t, result_start(addr)));
            v.apply(&ev(t + 1, result_end(addr, None)));
        }

        // `/` on the line is a no-op: the line has no list to filter.
        v.search_start();
        assert!(!v.is_searching());

        v.open_dashboard();
        v.search_start();
        assert!(v.is_searching());
        for c in "apz".chars() {
            v.search_input(c);
        }
        let typo = dash_frame(&mut v, 10).join("\n");
        assert!(!typo.contains("//api:server"), "{typo}");
        assert!(
            typo.contains("/apz"),
            "the prompt replaces the hints: {typo}"
        );
        v.search_backspace(); // "ap"
        let fixed = dash_frame(&mut v, 10).join("\n");
        assert!(fixed.contains("//api:server"), "{fixed}");
        assert!(!fixed.contains("//web:server"), "{fixed}");

        // Enter confirms: input mode ends but the filter stays applied.
        v.search_confirm();
        assert!(!v.is_searching());
        assert!(v.has_active_filter());
        let kept = dash_frame(&mut v, 10).join("\n");
        assert!(!kept.contains("//web:server"), "{kept}");

        // Esc clears the filter, and the dashboard stays open.
        v.search_cancel();
        assert!(!v.has_active_filter());
        assert!(!v.is_on_main_view());
        let cleared = dash_frame(&mut v, 10).join("\n");
        assert!(
            cleared.contains("//web:server") && cleared.contains("//api:server"),
            "{cleared}"
        );
    }

    #[test]
    fn hold_after_finish_only_off_main_view() {
        let mut v = TuiProgressView::new("L");
        // On the default (main) view: auto-exit, no hold.
        assert!(!v.hold_after_finish());
        // On another tab: hold the viewport open.
        v.tab(true);
        assert!(v.hold_after_finish());
    }

    #[test]
    fn finished_dashboard_hints_say_how_to_leave() {
        use crate::tui::app::TUIAppView;
        // Wide enough for the whole hint row: at 100 columns it is cut before
        // `ctrl-c stop`.
        fn hints(v: &mut TuiProgressView) -> String {
            v.render_full(0, 140, 14)
                .last()
                .map(ToString::to_string)
                .unwrap_or_default()
        }
        let mut v = TuiProgressView::new("L");
        v.open_dashboard();
        // While the run is live, the hint row offers to stop it.
        let live = hints(&mut v);
        assert!(live.contains("ctrl-c stop"), "{live}");

        // Once it has ended there is nothing to stop: `q` leaves.
        v.set_finished(0);
        let hints = hints(&mut v);
        assert!(hints.contains("q or esc leave"), "{hints}");
        assert!(!hints.contains("ctrl-c"), "{hints}");
    }

    #[test]
    fn elapsed_clock_freezes_after_finish() {
        use crate::tui::app::TUIAppView;
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, execute_start("//a:b"))); // anchor the clock at t=0
        v.open_dashboard();

        // Live: two frames at different wall clocks show different elapsed —
        // on the dashboard's title and on the line.
        let title = |v: &mut TuiProgressView, now: u64| dash_frame(v, now)[0].clone();
        let header = |v: &TuiProgressView, now: u64| line_at(v, now);
        assert_ne!(title(&mut v, 5_000), title(&mut v, 9_000));
        assert_ne!(header(&v, 5_000), header(&v, 9_000));

        // Frozen: after finish both are identical regardless of `now_ms`.
        v.set_finished(50_000);
        assert_eq!(title(&mut v, 100_000), title(&mut v, 999_000));
        assert_eq!(header(&v, 100_000), header(&v, 999_000));
    }

    /// A target still open when the run ended (a stopped run) stops counting
    /// with the run's clock: on the dashboard's rows, on its target screen,
    /// and on the line's spotlight.
    #[test]
    fn an_open_target_stops_counting_when_the_run_ends() {
        use crate::tui::app::TUIAppView;
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, execute_start("//a:b")));
        v.set_stopped();
        v.set_finished(20_000);
        let line = |v: &TuiProgressView, now: u64| v.render("⠋", now, 120, 1)[0].to_string();
        assert_eq!(line(&v, 30_000), line(&v, 90_000), "the line's spotlight");
        v.open_dashboard();
        assert_eq!(
            dash_frame(&mut v, 30_000),
            dash_frame(&mut v, 90_000),
            "the list"
        );
        v.open_selected(false);
        assert_eq!(
            dash_frame(&mut v, 30_000),
            dash_frame(&mut v, 90_000),
            "the target screen"
        );
    }

    /// The prompt row names the oldest request and how many are pending,
    /// and offers Enter; its ✦ flashes once a second unless motion is
    /// reduced.
    #[test]
    fn approval_prompt_row_names_the_oldest_and_counts() {
        let center = crate::tui::approval::ApprovalCenter::new();
        let build = center.build();
        let r1 = center.request(&build, "//a:1".to_string(), vec![]);
        let _r2 = center.request(&build, "//a:2".to_string(), vec![]);
        let mut view = TuiProgressView::new("run").with_approval(center.clone());
        view.motion = true;
        let row = |v: &TuiProgressView, now: u64| v.render("⠋", now, 200, 2)[0].to_string();
        assert_eq!(
            row(&view, 0),
            "✦ approval required: //a:1 · 2 pending · enter to review"
        );
        // Yellow throughout; bold up to the address, as the mockup.
        let spans = &view.render("⠋", 0, 200, 2)[0].spans;
        let bold = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let styled: Vec<(&str, Style)> = spans
            .iter()
            .filter(|s| !s.content.is_empty())
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(
            styled.get(..3),
            Some(
                &[
                    ("✦", bold),
                    (" approval required: //a:1", bold),
                    (
                        " · 2 pending · enter to review",
                        Style::default().fg(Color::Yellow)
                    ),
                ][..]
            ),
            "{styled:?}"
        );
        assert!(row(&view, 1_000).starts_with("❖ "), "{}", row(&view, 1_000));
        view.motion = false;
        assert!(
            row(&view, 1_000).starts_with("✦ "),
            "reduced motion: static"
        );
        let _decided = center.respond(r1.seq, true, Vec::new());
        assert_eq!(
            row(&view, 0),
            "✦ approval required: //a:2 · enter to review",
            "no count for one"
        );
    }

    /// The Done and Cached tabs list matched targets only, until `a` widens
    /// them to every target the run touched; a failure is in neither.
    #[test]
    fn done_and_cached_tabs_follow_the_count_scope() {
        use crate::tui::dashboard::Tab;
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, matched(&["//a:top", "//a:hit", "//a:bad"], true)));
        for (addr, hit, error) in [
            ("//a:top", false, None),
            ("//dep:lib", false, None),
            ("//a:hit", true, None),
            ("//dep:hit", true, None),
            ("//a:bad", false, Some("boom".to_owned())),
        ] {
            v.apply(&ev(1, result_start(addr)));
            if hit {
                v.apply(&ev(2, local_cache_hit(addr)));
            }
            v.apply(&ev(3, result_end(addr, error)));
        }
        v.open_dashboard();
        let list = |v: &mut TuiProgressView, tab: Tab| {
            v.list.set_tab(tab);
            dash_frame(v, 10).join("\n")
        };

        let done = list(&mut v, Tab::Done);
        assert!(done.contains("//a:top"), "{done}");
        assert!(
            !done.contains("//dep:lib") && !done.contains("//a:bad"),
            "{done}"
        );
        let cached = list(&mut v, Tab::Cached);
        assert!(
            cached.contains("//a:hit") && !cached.contains("//dep:hit"),
            "{cached}"
        );

        v.toggle_scope();
        let done = list(&mut v, Tab::Done);
        assert!(
            done.contains("//a:top") && done.contains("//dep:lib"),
            "{done}"
        );
        assert!(!done.contains("//a:bad"), "{done}");
        let cached = list(&mut v, Tab::Cached);
        assert!(
            cached.contains("//a:hit") && cached.contains("//dep:hit"),
            "{cached}"
        );
    }

    #[test]
    fn addr_filter_folds_ascii_case_without_allocating() {
        assert!(addr_matches("//WEB:Server", "web"));
        assert!(addr_matches("//web:server", "WEB:SERVER"));
        assert!(addr_matches("//web:server", "//web:server"));
        assert!(!addr_matches("//web:server", "api"));
        // A filter longer than the addr can never match.
        assert!(!addr_matches("//a:b", "//a:b:c"));
        // The empty filter is the unfiltered list.
        assert!(addr_matches("//a:b", ""));
        // Non-ASCII bytes compare as-is — exactly what ASCII lower-casing did.
        assert!(addr_matches("//pkg/ünï:t", "ünï"));
        assert!(!addr_matches("//pkg/ünï:t", "ÜNÏ"));
    }

    #[test]
    fn a_failed_row_keeps_its_error_on_one_line() {
        // A `\n` inside a `Span` does not wrap in ratatui, it corrupts the
        // screen the dashboard is laid out on — and multi-line stderr is
        // exactly what lands on the Failed tab.
        use crate::tui::dashboard::Tab;
        let mut v = TuiProgressView::new("L");
        v.apply(&ev(0, result_start("//a:bad")));
        v.apply(&ev(
            1,
            result_end("//a:bad", Some("first line\nsecond line".into())),
        ));
        v.open_dashboard();
        v.list.set_tab(Tab::Failed);

        let lines = dash_frame(&mut v, 0);
        assert_eq!(
            lines.len(),
            14,
            "a multi-line error must not grow the frame"
        );
        let text = lines.join("\n");
        assert!(text.contains("//a:bad"), "{text}");
        assert!(!text.contains("second line"), "{text}");
    }

    /// `frame_does_no_io`, through the view: the LAST LINE column shows a
    /// running target's latest line, read by the view's background reader —
    /// every call to the log reader is made off the thread drawing frames —
    /// and read again once the log grew.
    #[test]
    fn last_line_column_reads_off_the_frame_thread() {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};

        #[derive(Default)]
        struct Reader {
            log: Mutex<Vec<u8>>,
            threads: Mutex<Vec<std::thread::ThreadId>>,
        }
        impl crate::tui::dashboard::LogReader for Reader {
            fn size(&self, addr: &str) -> Option<u64> {
                self.threads
                    .lock()
                    .expect("threads")
                    .push(std::thread::current().id());
                (addr == "//a:run").then(|| self.log.lock().expect("log").len() as u64)
            }
            fn tail(&self, addr: &str, max: usize) -> Option<Vec<u8>> {
                self.threads
                    .lock()
                    .expect("threads")
                    .push(std::thread::current().id());
                let log = self.log.lock().expect("log");
                (addr == "//a:run").then(|| log[log.len().saturating_sub(max)..].to_vec())
            }
        }

        let reader = std::sync::Arc::new(Reader::default());
        reader
            .log
            .lock()
            .expect("log")
            .extend_from_slice(b"Compiling a\n");
        let mut v = TuiProgressView::new("L").with_log_reader(reader.clone());
        v.apply(&ev(0, result_start("//a:run")));
        v.apply(&ev(1, execute_start("//a:run")));
        // The line alone starts no reader thread; the first dashboard frame does.
        let _line = v.render("⠋", 0, 120, 4);
        assert!(
            v.tails.is_none(),
            "a reader thread before any dashboard frame"
        );
        v.open_dashboard();

        // Frames a reader's interval apart, until the row shows `want`.
        let mut now = 1_000;
        let mut until = |v: &mut TuiProgressView, want: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                now += 300;
                let text = dash_frame(v, now).join("\n");
                if text
                    .lines()
                    .any(|r| r.contains("//a:run") && r.trim_end().ends_with(want))
                {
                    return;
                }
                assert!(Instant::now() < deadline, "never showed {want:?}\n{text}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        until(&mut v, "Compiling a");
        reader
            .log
            .lock()
            .expect("log")
            .extend_from_slice(b"Compiling b\n");
        until(&mut v, "Compiling b");

        let me = std::thread::current().id();
        let threads = reader.threads.lock().expect("threads");
        assert!(!threads.is_empty());
        assert!(
            threads.iter().all(|t| *t != me),
            "a frame called the log reader"
        );
    }

    #[test]
    fn the_live_op_index_holds_only_open_ops_not_the_whole_run() {
        // The model keeps every target's record (none of these 2,000 get a
        // `ResultEnd` here, so all stay Running); the frame path walks the
        // open-op index instead, which stays in-flight-sized.
        let mut s = BuildState::new();
        for i in 0..2_000 {
            let addr = format!("//pkg{i}:t");
            s.apply(&ev(0, execute_start(&addr)));
            s.apply(&ev(1, execute_end(&addr)));
        }
        // Three targets left mid-flight: two executing, one writing to cache.
        s.apply(&ev(2, execute_start("//live:a")));
        s.apply(&ev(2, execute_start("//live:b")));
        s.apply(&ev(2, local_write_start("//live:c")));

        assert_eq!(s.dash.counts(CountScope::All).running, 2_003);
        assert_eq!(
            open_addrs(&s),
            ["//live:a", "//live:b", "//live:c"],
            "the frame path only walks the live ops"
        );
        assert_eq!(executing(&s), 2, "only Execute holds a worker slot");
    }

    #[test]
    fn open_op_index_survives_duplicate_and_mismatched_boundaries() {
        // The index is maintained beside each record's open-op bit; every
        // event order the fold tolerates must leave the two agreeing.
        let open_recs = |s: &BuildState| -> Vec<(u32, Option<Op>)> {
            let mut v: Vec<_> = (0..s.dash.id_count())
                .filter_map(|id| {
                    let id = u32::try_from(id).ok()?;
                    let r = s.dash.record(id)?;
                    r.op_open().then(|| (id, r.op()))
                })
                .collect();
            v.sort_unstable_by_key(|&(id, _)| id);
            v
        };
        let rescan = |s: &BuildState| {
            open_recs(s)
                .iter()
                .filter(|(_, op)| *op == Some(Op::Execute))
                .count()
        };
        let mut s = BuildState::new();
        let steps = [
            execute_start("//a:1"),
            execute_start("//a:2"),
            // A second start on an open target: the overlap guard closes the
            // first span and reopens — still one slot held.
            execute_start("//a:1"),
            // A mismatched close is ignored: //a:1 is executing, not writing.
            local_write_end("//a:1"),
            // Execute → LocalCacheWrite on //a:2 releases its worker slot but
            // leaves the target open.
            local_write_start("//a:2"),
            execute_end("//a:1"),
            // A duplicate close is a no-op.
            execute_end("//a:1"),
            local_write_end("//a:2"),
        ];
        for (i, kind) in steps.into_iter().enumerate() {
            s.apply(&ev(i as u64, kind));
            assert_eq!(executing(&s), rescan(&s), "after step {i}");
            let mut index: Vec<(u32, Option<Op>)> =
                s.dash.open_ops().map(|(id, r)| (id, r.op())).collect();
            index.sort_unstable_by_key(|&(id, _)| id);
            assert_eq!(
                index,
                open_recs(&s),
                "index drifted from the records after step {i}"
            );
        }
        assert_eq!(executing(&s), 0);
        assert!(open_addrs(&s).is_empty(), "nothing left open");
        assert_eq!(s.dash.id_count(), 2, "both targets stay in the history");
    }

    /// `System` with a per-thread allocation counter. Test binary only — the
    /// shipped allocator is untouched.
    struct CountingAlloc;

    use std::cell::Cell;

    thread_local! {
        /// Allocations made by this thread. A `Cell` with a const initializer:
        /// no allocation and no destructor, so the allocator can bump it without
        /// re-entering itself.
        static ALLOCS: Cell<usize> = const { Cell::new(0) };
        /// Bytes this thread allocated minus the bytes it freed. Signed: a
        /// block freed here may have been allocated on another thread.
        static LIVE: Cell<isize> = const { Cell::new(0) };
        /// The most `LIVE` has been since [`live_bytes_during`] last reset it.
        static PEAK: Cell<isize> = const { Cell::new(0) };
    }

    /// Moves this thread's live-byte count by `delta`.
    fn live_add(delta: isize) {
        let live = LIVE.with(|c| {
            let live = c.get().wrapping_add(delta);
            c.set(live);
            live
        });
        PEAK.with(|p| p.set(p.get().max(live)));
    }

    fn signed(n: usize) -> isize {
        isize::try_from(n).unwrap_or(isize::MAX)
    }

    // SAFETY: every method forwards to `System`, which upholds the `GlobalAlloc`
    // contract; the wrapper adds no assumptions of its own. The counters are
    // const-initialised thread-local `Cell`s — they never allocate and have
    // no destructor, so bumping them cannot re-enter the allocator.
    unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            // SAFETY: `layout` is forwarded unchanged from the caller.
            let p = unsafe { std::alloc::System.alloc(layout) };
            if !p.is_null() {
                live_add(signed(layout.size()));
            }
            p
        }

        unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            // SAFETY: `layout` is forwarded unchanged from the caller.
            let p = unsafe { std::alloc::System.alloc_zeroed(layout) };
            if !p.is_null() {
                live_add(signed(layout.size()));
            }
            p
        }

        unsafe fn realloc(
            &self,
            ptr: *mut u8,
            layout: std::alloc::Layout,
            new_size: usize,
        ) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            // SAFETY: `ptr`, `layout` and `new_size` are forwarded unchanged.
            let p = unsafe { std::alloc::System.realloc(ptr, layout, new_size) };
            if !p.is_null() {
                live_add(signed(new_size).wrapping_sub(signed(layout.size())));
            }
            p
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            // SAFETY: `ptr` and `layout` are forwarded unchanged from the caller.
            unsafe { std::alloc::System.dealloc(ptr, layout) };
            live_add(-signed(layout.size()));
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAlloc = CountingAlloc;

    /// Allocations `f` makes on this thread.
    fn allocs_during(f: impl FnOnce()) -> usize {
        let before = ALLOCS.with(Cell::get);
        f();
        ALLOCS.with(Cell::get).saturating_sub(before)
    }

    /// What `f` returns, the heap bytes this thread still holds once it has
    /// returned (what `f` built and kept, net of what it freed), and the most
    /// it held at any point while it ran.
    fn live_bytes_during<T>(f: impl FnOnce() -> T) -> (T, isize, isize) {
        let before = LIVE.with(Cell::get);
        PEAK.with(|p| p.set(before));
        let out = f();
        (
            out,
            LIVE.with(Cell::get).wrapping_sub(before),
            PEAK.with(Cell::get).wrapping_sub(before),
        )
    }

    /// A scripted 100k-target build, folded: the matcher streams the
    /// addresses in batches, then each target starts, and a tenth come from
    /// the cache while the rest run.
    fn fold_100k(state: &mut impl FnMut(&BuildEvent)) {
        const N: usize = 100_000;
        const BATCH: usize = 1_000;
        let addr = |i: usize| format!("//pkg/sub{}/dir{}:target_{i}", i % 997, i % 31);
        let mut t = 0;
        let mut ev = |kind: BuildEventKind| {
            t += 1;
            state(&BuildEvent {
                at_unix_ms: 1_700_000_000_000 + t,
                kind,
            });
        };
        for b in 0..N / BATCH {
            ev(BuildEventKind::Matched {
                addrs: (b * BATCH..(b + 1) * BATCH).map(addr).collect(),
                complete: b + 1 == N / BATCH,
            });
        }
        // Every target's result starts before any runs, as the engine starts
        // a dependent's while it waits on its deps: the peak in flight is the
        // whole build.
        for i in 0..N {
            ev(BuildEventKind::ResultStart { addr: addr(i) });
        }
        for i in 0..N {
            let a = addr(i);
            if i % 10 == 0 {
                ev(BuildEventKind::CacheOutcome {
                    addr: a.clone(),
                    outcome: hcore::events::CacheOutcomeKind::LocalHit,
                });
            } else {
                ev(BuildEventKind::ExecuteStart {
                    output_groups: Vec::new(),
                    log_not_streamed: false,
                    addr: a.clone(),
                    driver: "exec".into(),
                    cache: true,
                    declared: None,
                    reserved: None,
                    queued_ms: None,
                    admitted: None,
                });
                ev(BuildEventKind::ExecuteEnd {
                    addr: a.clone(),
                    error: None,
                });
            }
            ev(BuildEventKind::ResultEnd {
                left_behind: None,
                cancelled: false,
                addr: a,
                error: None,
                upstream_of: None,
                exit_status: None,
                log_tail: None,
                timed_out: None,
            });
        }
    }

    /// The spec's memory budget: folding a 100k-target build keeps the
    /// view's whole heap under 25 MB — counted by the allocator, not
    /// `size_of` the record. The whole [`BuildState`], the dashboard's model
    /// in it, at the end of the build and at its peak (every result in
    /// flight). 15.5 MB at the end and 16.2 MB at the peak when written
    /// (35 MB at the end while the box's String-keyed sets were still here).
    #[test]
    fn heap_at_100k_targets() {
        const BUDGET: isize = 25 * 1024 * 1024;
        let (state, end, peak) = live_bytes_during(|| {
            let mut s = BuildState::new();
            fold_100k(&mut |e| s.apply(e));
            s
        });
        assert_eq!(state.dash.id_count(), 100_000);
        assert_eq!(
            state.dash.counts(CountScope::All).finished_ok(),
            100_000,
            "the scripted build ran to the end",
        );
        assert_eq!(state.completed, 100_000);
        for (when, bytes) in [("at the end", end), ("at the peak", peak)] {
            assert!(
                bytes < BUDGET,
                "the view holds {bytes} B {when} of a 100k-target build (budget {BUDGET})",
            );
        }
        drop(state);
    }

    #[test]
    fn a_dashboard_frame_allocates_the_same_at_1k_and_20k_targets() {
        // The list shows ~20 rows, so one frame must allocate the same whether
        // the tab behind it holds 1k or 20k targets: a heap allocation per
        // row, formatted and then thrown away by the window, must not come
        // back. Allocation *count*, not bytes — the filter's match buffer is
        // one allocation of `O(n)` bytes.
        use crate::tui::dashboard::Tab;
        fn run_of(n: usize) -> TuiProgressView {
            let mut v = TuiProgressView::new("bench");
            let addrs: Vec<String> = (0..n)
                .map(|i| format!("//p{:05}:t{i}", (i * 7919) % n))
                .collect();
            v.apply(&ev(
                0,
                matched(&addrs.iter().map(String::as_str).collect::<Vec<_>>(), true),
            ));
            for (i, a) in addrs.iter().enumerate() {
                v.apply(&ev(1, result_start(a)));
                if i % 3 == 0 {
                    v.apply(&ev(2, local_cache_hit(a)));
                }
                let error = (i % 3 == 1).then(|| "boom".to_owned());
                v.apply(&ev(3, result_end(a, error)));
            }
            v.open_dashboard();
            v
        }
        fn frame_allocs(v: &mut TuiProgressView, tab: Tab, filter: &str) -> usize {
            v.list.set_tab(tab);
            let _kept = v.list.escape();
            if !filter.is_empty() {
                v.list.filter_start();
                filter.chars().for_each(|c| v.list.filter_input(c));
                v.list.filter_confirm();
            }
            // The first frame settles the filter cache and the selection;
            // measure a steady one.
            let settled: Vec<String> = v
                .render_full(0, 120, 30)
                .iter()
                .map(ToString::to_string)
                .collect();
            // Guard against measuring an empty tab: the footer counts rows
            // only when the list overflows the screen.
            assert!(
                settled
                    .iter()
                    .any(|l| l.contains(" of ") || l.contains(" match")),
                "the measured frame does not overflow its list: {settled:#?}",
            );
            allocs_during(|| drop(v.render_full(0, 120, 30)))
        }

        let mut small = run_of(1_000);
        let mut big = run_of(20_000);
        for tab in [Tab::All, Tab::Done, Tab::Cached, Tab::Failed] {
            // Unfiltered, then filtered — the filter has to be tried against
            // every row, so it is its own scaling risk.
            for filter in ["", "t7"] {
                let s = frame_allocs(&mut small, tab, filter);
                let b = frame_allocs(&mut big, tab, filter);
                assert!(
                    b <= s + 64,
                    "{tab:?} tab (filter {filter:?}): {s} allocations for a frame \
                     over 1k targets but {b} over 20k — per-frame cost still \
                     scales with the list length",
                );
            }
        }
    }
}
