//! The dashboard's fold: one fixed-size record per target, kept in per-state
//! buckets in activity order. Folding an event is `O(log n)` at most; no
//! frame sorts or walks the targets.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use hcore::capacity::Unit;
use hcore::events::{BuildEvent, BuildEventKind, CacheOutcomeKind};
use ratatui::style::{Color, Modifier, Style};

use super::bucket::Bucket;
use super::detail::{Detail, Details, Phase, Shape};
use super::intern::{Id, Interner};
use crate::tui::progress::{Boundary, CountScope, Op, event_op_boundary, failure_reason};
use crate::tui::text::Redact;

/// What a target is doing, as its row's glyph says. Approval (`✦ ❖`) is the
/// approvals unit's: it adds a state and its tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RowState {
    /// `·` matched or requested, not started.
    Queued,
    /// `⧗` not started, waiting on a resource or a lock.
    Blocked,
    /// `▶` an op has started.
    Running,
    /// `✓` finished, ran.
    Done,
    /// `●` finished from the cache.
    Cached,
    /// `✗` failed on its own (a timeout included): a root.
    Failed,
    /// `⊘` failed because a dependency did.
    DepFailed,
    /// `○` a stop ended it: not a failure, and in no tab but All.
    Stopped,
}

const STATES: usize = 8;

impl RowState {
    #[cfg(test)]
    pub const ALL: [RowState; STATES] = [
        RowState::Queued,
        RowState::Blocked,
        RowState::Running,
        RowState::Done,
        RowState::Cached,
        RowState::Failed,
        RowState::DepFailed,
        RowState::Stopped,
    ];

    pub fn glyph(self) -> &'static str {
        match self {
            RowState::Queued => "·",
            RowState::Blocked => "⧗",
            RowState::Running => "▶",
            RowState::Done => "✓",
            RowState::Cached => "●",
            RowState::Failed => "✗",
            RowState::DepFailed => "⊘",
            RowState::Stopped => "○",
        }
    }

    pub fn style(self) -> Style {
        let fg = match self {
            RowState::Queued | RowState::Stopped => return Style::default().fg(Color::DarkGray),
            RowState::Blocked => Color::Yellow,
            RowState::Running => Color::Cyan,
            RowState::Done => Color::Green,
            RowState::Cached => Color::Blue,
            RowState::Failed => {
                return Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
            }
            RowState::DepFailed => Color::Red,
        };
        Style::default().fg(fg)
    }

    fn finished(self) -> bool {
        matches!(
            self,
            RowState::Done
                | RowState::Cached
                | RowState::Failed
                | RowState::DepFailed
                | RowState::Stopped
        )
    }

    /// In All's "most recently finished" section.
    fn recent(self) -> bool {
        matches!(self, RowState::Done | RowState::Cached | RowState::Stopped)
    }
}

/// A list tab. `Approvals` is shown, right after All, only while a request
/// is pending; the model holds no row of it (the view lays the pending
/// requests over the list, see `ListState::set_approvals`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    All,
    Approvals,
    Running,
    Pending,
    Failed,
    Done,
    Cached,
}

/// One run of rows in a tab: a bucket, oldest or newest first.
#[derive(Debug, Clone, Copy)]
enum Seg {
    State(RowState),
    /// Done, cached and stopped together, newest first.
    Recent,
}

impl Seg {
    fn rev(self) -> bool {
        match self {
            Seg::State(s) => s.finished() && s.recent(),
            Seg::Recent => true,
        }
    }
}

impl Tab {
    /// Every tab of the model's, in the tab row's order. `Approvals` is not
    /// one: [`Tab::shown`] puts it after All while it is up.
    pub const LIST: [Tab; 6] = [
        Tab::All,
        Tab::Running,
        Tab::Pending,
        Tab::Failed,
        Tab::Done,
        Tab::Cached,
    ];

    /// The tabs the tab row draws, in order: `Approvals` right after All
    /// while `approvals` (a request is pending).
    pub fn shown(approvals: bool) -> impl Iterator<Item = Tab> {
        let (first, rest) = Tab::LIST.split_at(1);
        first
            .iter()
            .copied()
            .chain(approvals.then_some(Tab::Approvals))
            .chain(rest.iter().copied())
    }

    pub fn name(self) -> &'static str {
        match self {
            Tab::All => "All",
            Tab::Approvals => "Approvals",
            Tab::Running => "Running",
            Tab::Pending => "Pending",
            Tab::Failed => "Failed",
            Tab::Done => "Done",
            Tab::Cached => "Cached",
        }
    }

    /// The glyph its rows show; `All` has none.
    pub fn glyph(self) -> Option<&'static str> {
        let state = match self {
            Tab::All => return None,
            Tab::Approvals => return Some(APPROVAL_GLYPH),
            Tab::Running => RowState::Running,
            Tab::Pending => RowState::Blocked,
            Tab::Failed => RowState::Failed,
            Tab::Done => RowState::Done,
            Tab::Cached => RowState::Cached,
        };
        Some(state.glyph())
    }

    pub fn style(self) -> Style {
        match self {
            Tab::All => Style::default(),
            Tab::Approvals => Style::default().fg(Color::Yellow),
            Tab::Running => RowState::Running.style(),
            Tab::Pending => RowState::Blocked.style(),
            Tab::Failed => Style::default().fg(Color::Red),
            Tab::Done => RowState::Done.style(),
            Tab::Cached => RowState::Cached.style(),
        }
    }

    /// The next tab (`forward`) or the previous one, wrapping, among the
    /// tabs [`Tab::shown`] draws.
    pub fn cycle(self, forward: bool, approvals: bool) -> Tab {
        let n = Tab::shown(approvals).count();
        let i = Tab::shown(approvals).position(|t| t == self).unwrap_or(0);
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        Tab::shown(approvals).nth(j).unwrap_or(Tab::All)
    }

    /// The tab's rows, in order: All by activity (running, waiting, failed,
    /// most recently finished, pending); Pending blocked first; Failed roots
    /// first; Done and Cached newest first.
    fn segs(self) -> &'static [Seg] {
        use RowState as S;
        match self {
            Tab::All => &[
                Seg::State(S::Running),
                Seg::State(S::Blocked),
                Seg::State(S::Failed),
                Seg::State(S::DepFailed),
                Seg::Recent,
                Seg::State(S::Queued),
            ],
            Tab::Approvals => &[],
            Tab::Running => &[Seg::State(S::Running)],
            Tab::Pending => &[Seg::State(S::Blocked), Seg::State(S::Queued)],
            Tab::Failed => &[Seg::State(S::Failed), Seg::State(S::DepFailed)],
            Tab::Done => &[Seg::State(S::Done)],
            Tab::Cached => &[Seg::State(S::Cached)],
        }
    }
}

/// Record flags.
const MATCHED: u8 = 1;
const CACHE_HIT: u8 = 1 << 1;
const REMOTE_HIT: u8 = 1 << 2;
/// An op has started: the target runs (or ran).
const STARTED: u8 = 1 << 3;
const OP_OPEN: u8 = 1 << 4;
const ENDED: u8 = 1 << 5;
/// Between its `ResultStart` and its `ResultEnd`.
const IN_FLIGHT: u8 = 1 << 6;

/// Wait bits.
const WAIT_ADMISSION: u8 = 1;
const WAIT_LOCK: u8 = 1 << 1;
const WAIT_SCRATCH: u8 = 1 << 2;

/// No root named.
const NO_ROOT: Id = Id::MAX;

/// One target. Fixed size: the address lives in the interner, the wait's
/// words in the view's wait maps (bounded by what waits), a root failure's
/// reason in a map bounded by the roots.
#[derive(Debug, Clone, Copy)]
pub struct Record {
    /// When the current op started; with no op open, when the last began.
    pub op_since_ms: u64,
    /// `ResultEnd`'s time.
    pub end_ms: u64,
    /// Summed time of each op's finished runs, by [`Op::order`].
    pub op_ms: [u32; 5],
    /// The CPU the run was granted, in thousandths.
    pub cpu_milli: u32,
    /// Slot in its state's bucket.
    slot: u32,
    /// Slot in the recent bucket, for done, cached and stopped.
    recent_slot: u32,
    /// The root a dependency failure names.
    root: Id,
    pub state: RowState,
    /// The current (or last) op's [`Op::order`] + 1; 0 before any.
    op: u8,
    waits: u8,
    flags: u8,
}

impl Record {
    fn new() -> Self {
        Self {
            op_since_ms: 0,
            end_ms: 0,
            op_ms: [0; 5],
            cpu_milli: 0,
            slot: 0,
            recent_slot: 0,
            root: NO_ROOT,
            state: RowState::Queued,
            op: 0,
            waits: 0,
            flags: 0,
        }
    }

    pub fn matched(&self) -> bool {
        self.flags & MATCHED != 0
    }

    /// The current op, or the last one when none is open.
    pub(crate) fn op(&self) -> Option<Op> {
        Op::BY_ORDER
            .get(usize::from(self.op.checked_sub(1)?))
            .copied()
    }

    pub fn op_open(&self) -> bool {
        self.flags & OP_OPEN != 0
    }

    pub fn remote_hit(&self) -> bool {
        self.flags & REMOTE_HIT != 0
    }

    /// Time spent in ops, summed.
    pub fn total_ms(&self) -> u64 {
        self.op_ms.iter().map(|&ms| u64::from(ms)).sum()
    }

    pub(crate) fn op_ms(&self, op: Op) -> u64 {
        self.op_ms
            .get(usize::from(op.order()))
            .map_or(0, |&ms| u64::from(ms))
    }

    fn add_op_ms(&mut self, op: Op, ms: u64) {
        if let Some(slot) = self.op_ms.get_mut(usize::from(op.order())) {
            *slot = slot.saturating_add(u32::try_from(ms).unwrap_or(u32::MAX));
        }
    }
}

/// Per-state row counts, one scope's. The line and the tabs both read this:
/// the line's done figure is `done + cached`, its waiting count the `⧗` rows
/// of Pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabCounts {
    pub running: usize,
    pub blocked: usize,
    pub queued: usize,
    pub failed: usize,
    pub dep_failed: usize,
    pub done: usize,
    pub cached: usize,
    pub stopped: usize,
    /// Pending approval requests: the view's, not the model's (0 from
    /// [`Model::counts`]). Not a state, so not in [`Self::all`].
    pub approvals: usize,
    /// The glyph the Approvals tab draws this frame: `✦` or `❖` while it
    /// flashes. Empty reads as `✦`.
    pub approval_glyph: &'static str,
}

/// The approval glyph. While a request is pending it flashes with
/// [`APPROVAL_GLYPH_ALT`] once a second, unless motion is reduced.
pub const APPROVAL_GLYPH: &str = "✦";
pub const APPROVAL_GLYPH_ALT: &str = "❖";

/// The approval glyph at `now_ms`: `✦` and `❖` swap once a second; always
/// `✦` when `motion` is off (reduced motion).
pub fn approval_glyph(now_ms: u64, motion: bool) -> &'static str {
    if motion && (now_ms / 1000) % 2 == 1 {
        APPROVAL_GLYPH_ALT
    } else {
        APPROVAL_GLYPH
    }
}

impl TabCounts {
    /// Every target in scope.
    pub fn all(&self) -> usize {
        self.running
            + self.blocked
            + self.queued
            + self.failed
            + self.dep_failed
            + self.done
            + self.cached
            + self.stopped
    }

    /// The line's done figure: done and cached.
    pub fn finished_ok(&self) -> usize {
        self.done + self.cached
    }

    /// Roots and what they took down.
    pub fn failed_total(&self) -> usize {
        self.failed + self.dep_failed
    }

    /// Every target that reached its `ResultEnd`, whatever its outcome.
    pub fn ended(&self) -> usize {
        self.finished_ok() + self.failed_total() + self.stopped
    }

    pub fn tab(&self, tab: Tab) -> usize {
        match tab {
            Tab::All => self.all(),
            Tab::Approvals => self.approvals,
            Tab::Running => self.running,
            Tab::Pending => self.blocked + self.queued,
            Tab::Failed => self.failed_total(),
            Tab::Done => self.done,
            Tab::Cached => self.cached,
        }
    }
}

/// One bounded pool's latest `PoolUsage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gauge {
    pub pool: String,
    pub unit: Unit,
    pub held: u64,
    pub capacity: u64,
    pub waiting: u64,
    pub removals_pending: u64,
}

/// State `s`'s bucket.
fn pick_mut(buckets: &mut [Bucket; STATES], s: RowState) -> &mut Bucket {
    let [q, b, r, d, c, f, df, st] = buckets;
    match s {
        RowState::Queued => q,
        RowState::Blocked => b,
        RowState::Running => r,
        RowState::Done => d,
        RowState::Cached => c,
        RowState::Failed => f,
        RowState::DepFailed => df,
        RowState::Stopped => st,
    }
}

/// The most of one root failure's error the model keeps: a Starlark trace or
/// an aggregate of every broken input fits; a runaway message is cut.
const ERROR_MAX: usize = 16 * 1024;

/// `error`, cut at a character boundary to [`ERROR_MAX`] bytes, with an
/// ellipsis when cut.
fn capped_error(error: &str) -> Box<str> {
    if error.len() <= ERROR_MAX {
        return error.into();
    }
    let mut end = ERROR_MAX;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", error.get(..end).unwrap_or_default()).into_boxed_str()
}

/// The dashboard's state, folded from the event stream.
#[derive(Debug, Default)]
pub struct Model {
    names: Interner,
    recs: Vec<Record>,
    buckets: [Bucket; STATES],
    recent: Bucket,
    /// A root failure's PHASE text, by root.
    reasons: HashMap<Id, Box<str>>,
    /// A root failure's whole error, redacted, by root: kept when no exit
    /// status or timeout says it all, so the Log tab can show what the
    /// reason cuts short. At most [`ERROR_MAX`] bytes each.
    errors: HashMap<Id, Box<str>>,
    /// The latest report of each bounded pool.
    pools: BTreeMap<String, Gauge>,
    /// The pools in play this build: one an `ExecuteStart` declared or
    /// reserved, or whose `PoolUsage` showed some held or waiting. Only
    /// these get a gauge, and a pool never leaves.
    in_play: BTreeSet<String>,
    units: BTreeMap<String, String>,
    /// The targets with an op open: what the slow rows, the busy-worker
    /// count and a session's running list walk, bounded by what runs rather
    /// than by the build. Follows `OP_OPEN` at its every change.
    open: HashSet<Id>,
    /// How many records carry `IN_FLIGHT`.
    in_flight: usize,
    matched_seen: bool,
    matched_complete: bool,
    /// What the target screen's Overview reads beside the record: phase
    /// stamps, the run's driver and resources, how it was admitted and
    /// what each cache tier said.
    details: Details,
}

impl Model {
    /// Fold one event. `redact` takes heph's home out of a failure's text.
    pub fn apply(&mut self, ev: &BuildEvent, redact: &Redact) {
        let at = ev.at_unix_ms;
        if let Some((addr, op, boundary)) = event_op_boundary(&ev.kind) {
            let id = self.touch(addr);
            if matches!(boundary, Boundary::Start) {
                self.details.op_started(id, op, at);
            }
            self.fold_op(id, op, boundary, at);
        }
        if let Some(addr) = Details::addr_of(&ev.kind) {
            let id = self.touch(addr);
            self.details.fold(id, &ev.kind, at);
        }
        match &ev.kind {
            BuildEventKind::RequestConfig { units, .. } => {
                self.units.clone_from(units);
            }
            BuildEventKind::Matched { addrs, complete } => {
                self.matched_seen = true;
                for addr in addrs {
                    let id = self.touch(addr);
                    self.mark_matched(id);
                }
                if *complete {
                    self.matched_complete = true;
                }
            }
            BuildEventKind::ResultStart { addr } => {
                let id = self.touch(addr);
                if let Some(r) = self.rec_mut(id)
                    && r.flags & (ENDED | IN_FLIGHT) == 0
                {
                    r.flags |= IN_FLIGHT;
                    self.in_flight += 1;
                }
            }
            BuildEventKind::AdmissionWaitStart { addr, .. } => {
                self.wait(addr, WAIT_ADMISSION, true);
            }
            BuildEventKind::AdmissionWaitEnd { addr } => self.wait(addr, WAIT_ADMISSION, false),
            BuildEventKind::ResultLockWaitStart { addr, .. } => {
                self.wait(addr, WAIT_LOCK, true);
            }
            BuildEventKind::ResultLockWaitEnd { addr } => self.wait(addr, WAIT_LOCK, false),
            BuildEventKind::ScratchLockWaitStart { addr, .. } => {
                self.wait(addr, WAIT_SCRATCH, true);
            }
            BuildEventKind::ScratchLockWaitEnd { addr, .. } => {
                self.wait(addr, WAIT_SCRATCH, false);
            }
            BuildEventKind::ExecuteStart {
                addr,
                declared,
                reserved,
                ..
            } => {
                // Every pool it declared or reserved is in play.
                if let Some(r) = reserved {
                    self.play(r.keys());
                }
                if let Some(d) = declared {
                    self.play(d.keys());
                }
                // A target that runs is no longer waiting at admission.
                let id = self.touch(addr);
                let cpu = reserved
                    .as_ref()
                    .and_then(|r| r.get(hcore::capacity::CPU))
                    .map_or(0, |&micros| micros / 1_000);
                if let Some(r) = self.rec_mut(id) {
                    r.cpu_milli = u32::try_from(cpu).unwrap_or(u32::MAX);
                    r.waits &= !WAIT_ADMISSION;
                }
                self.restate(id);
            }
            BuildEventKind::CacheOutcome { addr, outcome } => {
                if matches!(
                    outcome,
                    CacheOutcomeKind::LocalHit | CacheOutcomeKind::RemoteHit
                ) {
                    let id = self.touch(addr);
                    let remote = matches!(outcome, CacheOutcomeKind::RemoteHit);
                    if let Some(r) = self.rec_mut(id) {
                        r.flags |= CACHE_HIT;
                        if remote {
                            r.flags |= REMOTE_HIT;
                        }
                    }
                    if self.state(id) == Some(RowState::Done) {
                        self.set_state(id, RowState::Cached);
                    }
                }
            }
            BuildEventKind::CacheHitRebuilt { addr, .. } => {
                if let Some(id) = self.names.get(addr) {
                    if let Some(r) = self.rec_mut(id) {
                        r.flags &= !(CACHE_HIT | REMOTE_HIT);
                    }
                    if self.state(id) == Some(RowState::Cached) {
                        self.set_state(id, RowState::Done);
                    }
                }
            }
            BuildEventKind::ResultEnd {
                addr,
                error,
                upstream_of,
                exit_status,
                timed_out,
                cancelled,
                ..
            } => {
                let id = self.touch(addr);
                let Some(r) = self.rec_mut(id) else {
                    return;
                };
                if r.flags & ENDED != 0 {
                    return;
                }
                let was_in_flight = r.flags & IN_FLIGHT != 0;
                r.flags = (r.flags | ENDED) & !(OP_OPEN | IN_FLIGHT);
                r.waits = 0;
                r.end_ms = at;
                let cached = r.flags & CACHE_HIT != 0;
                self.open.remove(&id);
                if was_in_flight {
                    self.in_flight -= 1;
                }
                let state = match (error, upstream_of) {
                    (Some(_), _) if *cancelled => RowState::Stopped,
                    (Some(_), Some(root)) => {
                        let root = self.names.get(root).unwrap_or(NO_ROOT);
                        if let Some(r) = self.rec_mut(id) {
                            r.root = root;
                        }
                        RowState::DepFailed
                    }
                    (Some(err), None) => {
                        let reason =
                            failure_reason(err, exit_status.as_deref(), timed_out.as_ref(), redact);
                        self.reasons.insert(id, reason.into_boxed_str());
                        if exit_status.is_none() && timed_out.is_none() {
                            self.errors.insert(id, capped_error(&redact.apply(err)));
                        }
                        RowState::Failed
                    }
                    (None, _) if cached => RowState::Cached,
                    (None, _) => RowState::Done,
                };
                self.set_state(id, state);
            }
            BuildEventKind::PoolUsage {
                pool,
                held,
                capacity,
                waiting,
                removals_pending,
            } => {
                if *held > 0 || *waiting > 0 {
                    self.play(std::iter::once(pool));
                }
                let unit = hcore::events::unit_of(&self.units, pool);
                let gauge = Gauge {
                    pool: pool.clone(),
                    unit,
                    held: *held,
                    capacity: *capacity,
                    waiting: *waiting,
                    removals_pending: *removals_pending,
                };
                match self.pools.get_mut(pool) {
                    Some(g) => *g = gauge,
                    None => {
                        self.pools.insert(pool.clone(), gauge);
                    }
                }
            }
            _ => {}
        }
    }

    /// `addr`'s id, with a queued record when it is new.
    fn touch(&mut self, addr: &str) -> Id {
        let (id, new) = self.names.intern(addr);
        if new {
            let mut r = Record::new();
            r.slot = self.bucket(RowState::Queued).push(id, false);
            self.recs.push(r);
            self.details.push();
        }
        id
    }

    /// `id`'s Overview facts beside its record.
    pub fn detail(&self, id: Id) -> Option<&Detail> {
        self.details.get(id)
    }

    /// The driver, resources and declared outputs of the run `d` describes.
    pub fn shape(&self, d: &Detail) -> Option<&Shape> {
        self.details.shape(d)
    }

    /// The pool units the request declared, for displaying amounts.
    pub fn units(&self) -> &BTreeMap<String, String> {
        &self.units
    }

    /// `id`'s phases on one time scale, the open one ending at `now_ms`.
    pub fn phases(&self, id: Id, now_ms: u64) -> Vec<Phase> {
        match (self.record(id), self.detail(id)) {
            (Some(r), Some(d)) => self.details.phases(r, d, now_ms),
            _ => Vec::new(),
        }
    }

    fn rec_mut(&mut self, id: Id) -> Option<&mut Record> {
        self.recs.get_mut(id as usize)
    }

    pub fn record(&self, id: Id) -> Option<&Record> {
        self.recs.get(id as usize)
    }

    fn state(&self, id: Id) -> Option<RowState> {
        self.record(id).map(|r| r.state)
    }

    pub fn addr(&self, id: Id) -> &str {
        self.names.addr(id)
    }

    pub fn id(&self, addr: &str) -> Option<Id> {
        self.names.get(addr)
    }

    /// How many addresses were interned: ids run below this.
    pub fn id_count(&self) -> usize {
        self.names.len()
    }

    /// The targets with an op open, in no order. Bounded by what runs.
    pub fn open_ops(&self) -> impl Iterator<Item = (Id, &Record)> + '_ {
        self.open
            .iter()
            .filter_map(|&id| Some((id, self.record(id)?)))
    }

    /// When `addr`'s open op started; `None` with no op open.
    pub fn open_since(&self, addr: &str) -> Option<u64> {
        let r = self.record(self.names.get(addr)?)?;
        r.op_open().then_some(r.op_since_ms)
    }

    /// Whether `addr` is between its `ResultStart` and its `ResultEnd`.
    pub fn in_flight(&self, addr: &str) -> bool {
        self.names
            .get(addr)
            .and_then(|id| self.record(id))
            .is_some_and(|r| r.flags & IN_FLIGHT != 0)
    }

    /// How many targets are between their `ResultStart` and `ResultEnd`.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight
    }

    /// The failures, roots and the ones they took down alike, in the order
    /// they ended. Walks the two failed buckets: `O(failed)`.
    pub fn failures(&self) -> impl Iterator<Item = Id> + '_ {
        let end = |id: &Id| self.record(*id).map_or(u64::MAX, |r| r.end_ms);
        let mut roots = self.bucket_ref(RowState::Failed).ids().peekable();
        let mut deps = self.bucket_ref(RowState::DepFailed).ids().peekable();
        std::iter::from_fn(move || match (roots.peek(), deps.peek()) {
            (Some(a), Some(b)) if end(b) < end(a) => deps.next(),
            (Some(_), _) => roots.next(),
            (None, _) => deps.next(),
        })
    }

    /// A root failure's PHASE text.
    pub fn reason(&self, id: Id) -> Option<&str> {
        self.reasons.get(&id).map(|r| &**r)
    }

    /// A root failure's whole error, redacted: `None` when its exit status
    /// or timeout is the whole story.
    pub fn error(&self, id: Id) -> Option<&str> {
        self.errors.get(&id).map(|e| &**e)
    }

    /// The root a dependency failure names.
    pub fn root_of(&self, id: Id) -> Option<&str> {
        let root = self.record(id)?.root;
        (root != NO_ROOT).then(|| self.names.addr(root))
    }

    /// The targets `root`'s failure took down: the first `max` by the
    /// order they ended, and how many there are in all. Walks the
    /// dependency failures, so a target screen calls it, never a list row.
    pub fn took_down(&self, root: Id, max: usize) -> (Vec<&str>, usize) {
        let mut names = Vec::new();
        let mut n = 0;
        for id in self.bucket_ref(RowState::DepFailed).ids() {
            if self.record(id).is_some_and(|r| r.root == root) {
                if names.len() < max {
                    names.push(self.names.addr(id));
                }
                n += 1;
            }
        }
        (names, n)
    }

    fn bucket(&mut self, s: RowState) -> &mut Bucket {
        pick_mut(&mut self.buckets, s)
    }

    fn bucket_ref(&self, s: RowState) -> &Bucket {
        let [q, b, r, d, c, f, df, st] = &self.buckets;
        match s {
            RowState::Queued => q,
            RowState::Blocked => b,
            RowState::Running => r,
            RowState::Done => d,
            RowState::Cached => c,
            RowState::Failed => f,
            RowState::DepFailed => df,
            RowState::Stopped => st,
        }
    }

    fn mark_matched(&mut self, id: Id) {
        let Some(r) = self.rec_mut(id) else {
            return;
        };
        if r.flags & MATCHED != 0 {
            return;
        }
        r.flags |= MATCHED;
        let (state, slot, recent_slot) = (r.state, r.slot, r.recent_slot);
        self.bucket(state).mark_matched(slot);
        if state.recent() {
            self.recent.mark_matched(recent_slot);
        }
    }

    fn fold_op(&mut self, id: Id, op: Op, boundary: Boundary, at: u64) {
        let Some(r) = self.rec_mut(id) else {
            return;
        };
        match boundary {
            Boundary::Start => {
                if r.flags & OP_OPEN != 0
                    && let Some(prev) = r.op()
                {
                    r.add_op_ms(prev, at.saturating_sub(r.op_since_ms));
                }
                r.op = op.order() + 1;
                r.op_since_ms = at;
                r.flags |= OP_OPEN | STARTED;
                self.open.insert(id);
            }
            Boundary::End => {
                if r.flags & OP_OPEN != 0 && r.op() == Some(op) {
                    r.add_op_ms(op, at.saturating_sub(r.op_since_ms));
                    r.flags &= !OP_OPEN;
                    self.open.remove(&id);
                }
            }
        }
        self.restate(id);
    }

    fn wait(&mut self, addr: &str, bit: u8, on: bool) {
        let id = self.touch(addr);
        if let Some(r) = self.rec_mut(id) {
            if r.flags & ENDED != 0 {
                return;
            }
            if on {
                r.waits |= bit;
            } else {
                r.waits &= !bit;
            }
        }
        self.restate(id);
    }

    /// Move an unfinished target to the state its flags say.
    fn restate(&mut self, id: Id) {
        let Some(r) = self.record(id) else {
            return;
        };
        if r.flags & ENDED != 0 {
            return;
        }
        let state = if r.waits != 0 {
            RowState::Blocked
        } else if r.flags & STARTED != 0 {
            RowState::Running
        } else {
            RowState::Queued
        };
        if state != r.state {
            self.set_state(id, state);
        }
    }

    fn set_state(&mut self, id: Id, to: RowState) {
        let Some(&r) = self.record(id) else {
            return;
        };
        let matched = r.matched();
        let from = r.state;
        self.bucket(from).remove(r.slot, matched);
        if from.recent() && !to.recent() {
            self.recent.remove(r.recent_slot, matched);
        }
        let slot = self.bucket(to).push(id, matched);
        let recent_slot = if to.recent() && !from.recent() {
            self.recent.push(id, matched)
        } else {
            r.recent_slot
        };
        if let Some(r) = self.rec_mut(id) {
            r.state = to;
            r.slot = slot;
            r.recent_slot = recent_slot;
        }
        self.compact(from);
        if from.recent() && !to.recent() {
            self.compact_recent();
        }
    }

    fn compact(&mut self, s: RowState) {
        let Self { buckets, recs, .. } = self;
        let bucket = pick_mut(buckets, s);
        let matched = |id: Id| recs.get(id as usize).is_some_and(Record::matched);
        if bucket.compact_if_sparse(matched) {
            for (slot, &id) in bucket.slots().iter().enumerate() {
                if let Some(r) = recs.get_mut(id as usize) {
                    r.slot = u32::try_from(slot).unwrap_or(u32::MAX);
                }
            }
        }
    }

    fn compact_recent(&mut self) {
        let Self { recent, recs, .. } = self;
        let matched = |id: Id| recs.get(id as usize).is_some_and(Record::matched);
        if recent.compact_if_sparse(matched) {
            for (slot, &id) in recent.slots().iter().enumerate() {
                if let Some(r) = recs.get_mut(id as usize) {
                    r.recent_slot = u32::try_from(slot).unwrap_or(u32::MAX);
                }
            }
        }
    }

    /// Whether `scope` narrows to the matched targets. Before any `Matched`
    /// event (a single-target request) nothing is matched, so the matched
    /// scope shows every target.
    pub fn matched_only(&self, scope: CountScope) -> bool {
        scope == CountScope::Matched && self.matched_seen
    }

    pub fn matched_seen(&self) -> bool {
        self.matched_seen
    }

    /// The matcher finished: the total is final.
    pub fn matched_complete(&self) -> bool {
        self.matched_complete
    }

    /// How many targets are matched; `None` before any `Matched` event.
    pub fn matched_total(&self) -> Option<usize> {
        self.matched_seen
            .then(|| self.counts(CountScope::Matched).all())
    }

    /// Every state's count in `scope`. `O(1)`.
    pub fn counts(&self, scope: CountScope) -> TabCounts {
        let m = self.matched_only(scope);
        let n = |s: RowState| self.bucket_ref(s).len(m);
        TabCounts {
            running: n(RowState::Running),
            blocked: n(RowState::Blocked),
            queued: n(RowState::Queued),
            failed: n(RowState::Failed),
            dep_failed: n(RowState::DepFailed),
            done: n(RowState::Done),
            cached: n(RowState::Cached),
            stopped: n(RowState::Stopped),
            approvals: 0,
            approval_glyph: APPROVAL_GLYPH,
        }
    }

    fn seg_bucket(&self, seg: Seg) -> &Bucket {
        match seg {
            Seg::State(s) => self.bucket_ref(s),
            Seg::Recent => &self.recent,
        }
    }

    /// Rows in `tab`.
    pub fn tab_len(&self, tab: Tab, scope: CountScope) -> usize {
        let m = self.matched_only(scope);
        tab.segs()
            .iter()
            .map(|&seg| self.seg_bucket(seg).len(m))
            .sum()
    }

    /// The `k`-th row of `tab`. `O(log n)`.
    pub fn row(&self, tab: Tab, scope: CountScope, mut k: usize) -> Option<Id> {
        let m = self.matched_only(scope);
        for &seg in tab.segs() {
            let b = self.seg_bucket(seg);
            let n = b.len(m);
            if k < n {
                return b.nth(m, k, seg.rev());
            }
            k -= n;
        }
        None
    }

    /// Where `id` is in `tab`, if it is there. `O(log n)`.
    pub fn index_of(&self, tab: Tab, scope: CountScope, id: Id) -> Option<usize> {
        let m = self.matched_only(scope);
        let r = self.record(id)?;
        if m && !r.matched() {
            return None;
        }
        let mut before = 0;
        for &seg in tab.segs() {
            let b = self.seg_bucket(seg);
            let here = match seg {
                Seg::State(s) => (s == r.state).then_some(r.slot),
                Seg::Recent => r.state.recent().then_some(r.recent_slot),
            };
            if let Some(slot) = here {
                return Some(before + b.rank(m, slot, seg.rev()));
            }
            before += b.len(m);
        }
        None
    }

    /// `tab`'s rows in order, for a filter to thin. Walks the tab.
    pub fn rows(&self, tab: Tab, scope: CountScope) -> impl Iterator<Item = Id> + '_ {
        let m = self.matched_only(scope);
        tab.segs()
            .iter()
            .flat_map(move |&seg| {
                let ids = self.seg_bucket(seg).ids();
                let ids: Box<dyn Iterator<Item = Id>> = if seg.rev() {
                    Box::new(ids.rev())
                } else {
                    Box::new(ids)
                };
                ids
            })
            .filter(move |&id| !m || self.record(id).is_some_and(Record::matched))
    }

    /// One gauge per pool in play that reported, cpu, memory and sandbox
    /// first, then declared pools by name. None before any report or before
    /// anything ran or waited; a pool this build never
    /// touched has none.
    pub fn gauges(&self) -> Vec<&Gauge> {
        let mut gauges: Vec<&Gauge> = self
            .pools
            .values()
            .filter(|g| self.in_play(&g.pool))
            .collect();
        let rank = |g: &Gauge| {
            hcore::capacity::BUILTIN_POOLS
                .iter()
                .position(|&p| p == g.pool)
                .unwrap_or(hcore::capacity::BUILTIN_POOLS.len())
        };
        // Stable: declared pools keep the map's name order.
        gauges.sort_by_key(|g| rank(g));
        gauges
    }

    /// The latest report of `pool`.
    pub fn pool(&self, pool: &str) -> Option<&Gauge> {
        self.pools.get(pool)
    }

    /// Whether `pool` is in play this build: some run declared or reserved
    /// it, or it held or had waiters.
    pub fn in_play(&self, pool: &str) -> bool {
        self.in_play.contains(pool)
    }

    /// Puts `pools` in play, allocating only for one not seen yet.
    fn play<S: AsRef<str>>(&mut self, pools: impl IntoIterator<Item = S>) {
        for p in pools {
            let p = p.as_ref();
            if !self.in_play.contains(p) {
                self.in_play.insert(p.to_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_size_budget() {
        // The spec's budget is 100 bytes a target beside its address, with
        // the buckets and the table counted; the record is most of it.
        assert!(
            std::mem::size_of::<Record>() <= 64,
            "Record is {} bytes",
            std::mem::size_of::<Record>()
        );
    }

    /// xorshift64*: the churn's seeded randomness.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % n as u64).unwrap_or(0)
        }
    }

    /// The tabs as plain lists: each state's targets in the order they
    /// entered it, and the recent ones, maintained from nothing but each
    /// record's state after every event — no slot, no count.
    #[derive(Default)]
    struct Naive {
        states: HashMap<RowState, Vec<Id>>,
        recent: Vec<Id>,
        last: Vec<RowState>,
    }

    impl Naive {
        fn follow(&mut self, m: &Model) {
            for id in 0..m.id_count() {
                let id = Id::try_from(id).unwrap_or(Id::MAX);
                let now = m.record(id).expect("a record per id").state;
                let before = match self.last.get(id as usize) {
                    Some(&s) => s,
                    None => {
                        self.last.push(RowState::Queued);
                        self.states.entry(RowState::Queued).or_default().push(id);
                        RowState::Queued
                    }
                };
                if before == now {
                    continue;
                }
                self.states.entry(before).or_default().retain(|&x| x != id);
                self.states.entry(now).or_default().push(id);
                if before.recent() && !now.recent() {
                    self.recent.retain(|&x| x != id);
                }
                if now.recent() && !before.recent() {
                    self.recent.push(id);
                }
                self.last[id as usize] = now;
            }
        }

        fn rows(&self, m: &Model, tab: Tab, scope: CountScope) -> Vec<Id> {
            let only = m.matched_only(scope);
            let mut out = Vec::new();
            for &seg in tab.segs() {
                let ids = match seg {
                    Seg::State(s) => self.states.get(&s).map_or(&[][..], Vec::as_slice),
                    Seg::Recent => &self.recent,
                };
                let keep = |id: &&Id| !only || m.record(**id).is_some_and(Record::matched);
                if seg.rev() {
                    out.extend(ids.iter().rev().filter(keep));
                } else {
                    out.extend(ids.iter().filter(keep));
                }
            }
            out
        }
    }

    /// One random event for one of `n` targets: arrival in a `Matched`
    /// batch, a start, waits on and off, an execute, a cache hit and its
    /// rebuild, and every kind of end.
    fn churn_event(rng: &mut Rng, n: usize) -> BuildEventKind {
        let addr = |rng: &mut Rng| format!("//c:t{}", rng.below(n));
        match rng.below(12) {
            0 => BuildEventKind::Matched {
                addrs: (0..1 + rng.below(8)).map(|_| addr(rng)).collect(),
                complete: false,
            },
            1 => BuildEventKind::ResultStart { addr: addr(rng) },
            2 | 3 => BuildEventKind::AdmissionWaitStart {
                addr: addr(rng),
                pool: "cpu".into(),
                need: 2,
                free: 1,
                ahead: 0,
                removals_pending: None,
                yielding_to: None,
            },
            4 => BuildEventKind::AdmissionWaitEnd { addr: addr(rng) },
            5 | 6 => BuildEventKind::ExecuteStart {
                output_groups: Vec::new(),
                log_not_streamed: false,
                addr: addr(rng),
                driver: "exec".into(),
                cache: true,
                declared: None,
                reserved: None,
                queued_ms: None,
                admitted: None,
            },
            7 => BuildEventKind::CacheOutcome {
                addr: addr(rng),
                outcome: CacheOutcomeKind::LocalHit,
            },
            8 => BuildEventKind::CacheHitRebuilt {
                addr: addr(rng),
                was: hcore::events::CacheHitKind::Local,
            },
            k => {
                let (error, upstream_of, cancelled) = match k {
                    9 => (None, None, false),
                    10 => (Some("boom".to_owned()), None, rng.below(3) == 0),
                    _ => (Some("boom".to_owned()), Some(addr(rng)), false),
                };
                BuildEventKind::ResultEnd {
                    left_behind: None,
                    cancelled,
                    addr: addr(rng),
                    error,
                    upstream_of,
                    exit_status: None,
                    log_tail: None,
                    timed_out: None,
                }
            }
        }
    }

    /// Seeded churn over 300 targets: after every event, each tab in both
    /// scopes has the naive list's length, its `k`-th row is the list's
    /// `k`-th, every target's `index_of` is its place in the list (or none),
    /// and `rows` walks the list. Buckets compact many times on the way (the
    /// queued one alone fills to 300 and drains), so a slot left stale by a
    /// compaction shows up as a wrong row or index. (Not the recent bucket:
    /// a finished target never leaves it, so no event vacates a slot there.)
    #[test]
    fn churn_keeps_rows_and_indexes_in_step_with_a_naive_list() {
        use crate::tui::progress::CountScope;
        const TARGETS: usize = 300;
        let redact = Redact::default();
        let mut compactions = 0;
        for seed in 1..=4u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut m = Model::default();
            let mut naive = Naive::default();
            for step in 0..2_000u64 {
                let kind = churn_event(&mut rng, TARGETS);
                let slots_before: usize = m.buckets.iter().map(|b| b.slots().len()).sum();
                m.apply(
                    &BuildEvent {
                        at_unix_ms: 1_000 + step,
                        kind: kind.clone(),
                    },
                    &redact,
                );
                let slots_after: usize = m.buckets.iter().map(|b| b.slots().len()).sum();
                if slots_after < slots_before {
                    compactions += 1;
                }
                naive.follow(&m);
                for tab in Tab::LIST {
                    for scope in [CountScope::Matched, CountScope::All] {
                        let want = naive.rows(&m, tab, scope);
                        let ctx =
                            || format!("seed {seed} step {step} {tab:?} {scope:?} after {kind:?}");
                        assert_eq!(m.tab_len(tab, scope), want.len(), "len, {}", ctx());
                        let rows: Vec<Id> = (0..want.len())
                            .map(|k| m.row(tab, scope, k).unwrap_or(Id::MAX))
                            .collect();
                        assert_eq!(rows, want, "row(k), {}", ctx());
                        assert_eq!(
                            m.row(tab, scope, want.len()),
                            None,
                            "past the end, {}",
                            ctx()
                        );
                        assert_eq!(
                            m.rows(tab, scope).collect::<Vec<_>>(),
                            want,
                            "rows, {}",
                            ctx()
                        );
                        let mut place = vec![None; m.id_count()];
                        for (k, &id) in want.iter().enumerate() {
                            place[id as usize] = Some(k);
                        }
                        for (id, &want) in place.iter().enumerate() {
                            let id = Id::try_from(id).unwrap_or(Id::MAX);
                            assert_eq!(
                                m.index_of(tab, scope, id),
                                want,
                                "index_of({id}), {}",
                                ctx()
                            );
                        }
                    }
                }
            }
        }
        assert!(
            compactions > 6,
            "the churn compacted only {compactions} times"
        );
    }
}
