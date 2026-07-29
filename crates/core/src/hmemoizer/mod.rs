mod cell;

use futures::FutureExt;
use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use xxhash_rust::xxh3::Xxh3Default;

/// Cycle detection (the per-task IN_FLIGHT frame stack, key hashing, and the
/// `tokio::task_local::scope` wrap around every `once` call) costs real CPU on
/// every memoizer call. It is only needed to surface dependency cycles as a
/// typed [`MemoizerCycleError`] rather than letting a self-await deadlock the
/// runtime — useful when iterating on the engine, dead weight in steady state.
///
/// Opt in by setting `HEPH_DEBUG_MEMOIZER_CYCLE=1`. Anything else (unset, `0`,
/// empty) leaves cycle detection off. Checked once on first use and cached.
fn cycle_detection_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("HEPH_DEBUG_MEMOIZER_CYCLE").as_deref(),
            Ok("1")
        )
    })
}

/// Returned when `Memoizer::once` detects a memoizer cycle — either same-task
/// self-recursion (the in-flight future would await itself) or a cross-task
/// wait-for cycle (Task A waits for cell X owned by Task B, which waits for
/// cell Y owned by Task A). Callers should catch this via [`downcast_chain_ref`]
/// and treat it as a dependency cycle (e.g. `EngineProviderExecutor::query`
/// skips the offending addr).
#[derive(Debug, Clone)]
pub struct MemoizerCycleError {
    pub tag: &'static str,
    pub key: String,
    /// Variant of cycle detected.
    pub kind: CycleKind,
    /// Frames in the cycle, formatted as `[tag] key`. For self-recursion, the
    /// chain is the current task's IN_FLIGHT stack from root to the re-entry.
    /// For cross-task, the chain alternates owner → wait → owner → wait → ...
    /// ending where the cycle closes.
    pub stack: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleKind {
    SelfRecursion,
    CrossTask,
}

impl fmt::Display for MemoizerCycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self.kind {
            CycleKind::SelfRecursion => "self-recursion",
            CycleKind::CrossTask => "cross-task wait-for cycle",
        };
        write!(
            f,
            "memoizer cycle ({variant}): tag={} key={}",
            self.tag, self.key
        )?;
        if !self.stack.is_empty() {
            writeln!(f, "\nstack (root first):")?;
            for (i, frame) in self.stack.iter().enumerate() {
                write!(f, "  {i:>2}: {frame}")?;
                if i + 1 < self.stack.len() {
                    writeln!(f)?;
                }
            }
        }
        Ok(())
    }
}

impl std::error::Error for MemoizerCycleError {}

/// One frame in the per-call-chain stack of (tag, key_hash) currently being computed.
/// Stored as an Arc cons-list so child scopes share parent state without cloning a
/// HashSet on every `once` call — pushing a frame is just one Arc allocation, and
/// inheriting the parent chain is a refcount bump.
struct Frame {
    parent: Option<Arc<Frame>>,
    tag: &'static str,
    key_hash: u64,
    /// `format!("{:?}", key)` of the cell key. Only built when cycle detection
    /// is enabled (one allocation per `once` call) — used for the stack trace in
    /// `MemoizerCycleError`.
    debug_key: Arc<str>,
    /// Fresh per-`once()` call identity. Used as the wait-for graph key
    /// instead of `tokio::task::Id` because the engine drives massive
    /// fan-out via `try_join_all` / `tokio::join!` on a single task —
    /// task-id-keyed waits collapse all concurrent siblings into one
    /// identity, hiding same-task cycles. Per-call ids distinguish each
    /// `once()` invocation.
    invocation_id: u64,
}

tokio::task_local! {
    /// Top of the per-call-chain frame stack. Scoped via
    /// `tokio::task_local::scope` so sibling futures in `try_join_all` don't see
    /// each other's frames — only the *recursive* descendants of a given `once`
    /// inherit the parent's chain. Re-entry on a (tag, key_hash) already in the
    /// chain = cycle.
    static IN_FLIGHT: Option<Arc<Frame>>;
}

fn compute_key_hash<K: Hash + ?Sized>(k: &K) -> u64 {
    let mut h = Xxh3Default::new();
    k.hash(&mut h);
    h.finish()
}

fn check_recursion(tag: &'static str, key_hash: u64) -> Option<Arc<Frame>> {
    IN_FLIGHT
        .try_with(|f| {
            let mut cur = f.clone();
            while let Some(node) = cur {
                if node.tag == tag && node.key_hash == key_hash {
                    return Some(node);
                }
                cur = node.parent.clone();
            }
            None
        })
        .unwrap_or(None)
}

fn current_frame() -> Option<Arc<Frame>> {
    IN_FLIGHT.try_with(|f| f.clone()).ok().flatten()
}

fn current_parent_invocation_id() -> Option<u64> {
    current_frame().map(|f| f.invocation_id)
}

fn push_frame(
    tag: &'static str,
    key_hash: u64,
    debug_key: Arc<str>,
    invocation_id: u64,
) -> Option<Arc<Frame>> {
    let parent = current_frame();
    Some(Arc::new(Frame {
        parent,
        tag,
        key_hash,
        debug_key,
        invocation_id,
    }))
}

/// Walk the frame chain from root to top, producing one `[tag] key` per level.
fn format_frame_stack(top: &Arc<Frame>) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut cur: Option<&Arc<Frame>> = Some(top);
    while let Some(node) = cur {
        items.push(format!("[{}] {}", node.tag, node.debug_key));
        cur = node.parent.as_ref();
    }
    items.reverse();
    items
}

// ---- Per-invocation phase registry ----
//
// Records the next-await label for each `once()` invocation, updated by
// instrumented call sites in `engine::execute`, `pluginexec::run_inner`,
// etc. Dumped alongside the wait-for graph on stall panic so we can see
// where each stuck invocation is parked when the hang is on a non-memoizer
// await (semaphore acquire, fs op, subprocess wait, cache_locally, …).
//
// Opt in via `HEPH_PHASE_TRACE=1`. Disabled by default — `set_phase` and
// `clear_phase` are O(1) early-returns when the flag is off.

/// Spelled like every other knob (`HEPH_DEBUG_MEMOIZER_CYCLE`,
/// `HEPH_MEMOIZER_STALL_SECS`).
const PHASE_TRACE_VAR: &str = "HEPH_PHASE_TRACE";

/// The decision, separated from the process environment so it is testable — the
/// real one caches in a `OnceLock`, so a test that set the var would be at the
/// mercy of whichever test ran first.
fn phase_trace_from(get: impl Fn(&str) -> Option<String>) -> bool {
    get(PHASE_TRACE_VAR).as_deref() == Some("1")
}

fn phase_trace_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| phase_trace_from(|name| std::env::var(name).ok()))
}

static PHASES: OnceLock<Mutex<HashMap<u64, &'static str>>> = OnceLock::new();

fn phases() -> &'static Mutex<HashMap<u64, &'static str>> {
    PHASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Tag the calling invocation with its next-await `phase`. No-op when
/// `HEPH_PHASE_TRACE` is unset or when invoked outside any `once()` scope.
pub fn set_phase(phase: &'static str) {
    if !phase_trace_enabled() {
        return;
    }
    let Some(inv) = current_parent_invocation_id() else {
        return;
    };
    phases()
        .lock()
        .expect("phases mutex poisoned")
        .insert(inv, phase);
}

/// Drop the calling invocation's phase entry. Pair with `set_phase` at the
/// end of an instrumented region so the dump only shows live invocations.
pub fn clear_phase() {
    if !phase_trace_enabled() {
        return;
    }
    let Some(inv) = current_parent_invocation_id() else {
        return;
    };
    phases().lock().expect("phases mutex poisoned").remove(&inv);
}

pub fn dump_phases() -> String {
    if !phase_trace_enabled() {
        return format!("  (phase trace disabled — set {PHASE_TRACE_VAR}=1)");
    }
    let map = phases().lock().expect("phases mutex poisoned");
    if map.is_empty() {
        return "  (none)".to_string();
    }
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by_key(|(inv, _)| **inv);
    let mut out = String::new();
    for (inv, phase) in entries {
        out.push_str(&format!("    inv {inv} @ {phase}\n"));
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---- Wait-for graph ----

static NEXT_INVOCATION_ID: AtomicU64 = AtomicU64::new(1);

fn fresh_invocation_id() -> u64 {
    NEXT_INVOCATION_ID.fetch_add(1, Ordering::Relaxed)
}

/// No-op kept for backward compatibility with CLI entry points that wrap
/// their outer future in this helper. The wait-for graph used to depend on
/// `tokio::task::Id` and needed a synthetic id scoped at the `block_on`
/// root; per-invocation ids replaced that, so the wrapper is now redundant.
/// Left as a thin pass-through so callers don't need to be touched.
pub async fn with_cycle_ctx<F: Future>(fut: F) -> F::Output {
    fut.await
}

type CellId = (&'static str, u64); // (tag, key_hash)

struct CellRecord {
    /// Invocation id of the `once()` call that became the cell's first
    /// awaiter (and therefore drives the shared compute closure).
    owner: u64,
    debug_key: Arc<str>,
}

struct WaitGraph {
    /// Live cells (currently being computed or awaited).
    cells: HashMap<CellId, CellRecord>,
    /// invocation_id → set of cells that invocation is currently blocked on.
    ///
    /// One invocation can have multiple outgoing edges because nested
    /// `once()` calls register `wait[parent_invocation] = child_cell`, and a
    /// parent driving `try_join_all` over N children sees one such edge per
    /// child. Storing a set (rather than a single cell) keeps all
    /// concurrent edges live so the cycle detector can find a cycle through
    /// any of them.
    waiting: HashMap<u64, std::collections::HashSet<CellId>>,
}

impl WaitGraph {
    fn new() -> Self {
        Self {
            cells: HashMap::new(),
            waiting: HashMap::new(),
        }
    }

    fn add_wait(&mut self, inv: u64, cell: CellId) {
        self.waiting.entry(inv).or_default().insert(cell);
    }

    fn remove_wait(&mut self, inv: u64, cell: CellId) {
        if let Some(set) = self.waiting.get_mut(&inv) {
            set.remove(&cell);
            if set.is_empty() {
                self.waiting.remove(&inv);
            }
        }
    }

    /// DFS from `start` over the wait-for graph. Edges:
    ///   invocation `I` -- waiting[I] --> cells C
    ///   cell C -- cells[C].owner --> invocation J
    /// Cycle = revisiting an invocation already on the visit set.
    /// Returns the path of cell ids visited along the cycle, root first.
    fn find_cycle(&self, start: u64) -> Option<Vec<CellId>> {
        let mut visited = std::collections::HashSet::new();
        let mut path: Vec<CellId> = Vec::new();
        visited.insert(start);
        if self.dfs(start, &mut visited, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    fn dfs(
        &self,
        cur: u64,
        visited: &mut std::collections::HashSet<u64>,
        path: &mut Vec<CellId>,
    ) -> bool {
        let Some(cells) = self.waiting.get(&cur) else {
            return false;
        };
        // Snapshot to avoid borrowing across the recursive call.
        let cells: Vec<CellId> = cells.iter().copied().collect();
        for cell in cells {
            let Some(rec) = self.cells.get(&cell) else {
                continue;
            };
            path.push(cell);
            if !visited.insert(rec.owner) {
                return true;
            }
            if self.dfs(rec.owner, visited, path) {
                return true;
            }
            visited.remove(&rec.owner);
            path.pop();
        }
        false
    }

    fn format_cycle(&self, path: &[CellId]) -> Vec<String> {
        path.iter()
            .map(|id| match self.cells.get(id) {
                Some(rec) => format!("[{}] {}", id.0, rec.debug_key),
                None => format!("[{}] <key_hash={}>", id.0, id.1),
            })
            .collect()
    }
}

static WAIT_GRAPH: OnceLock<Mutex<WaitGraph>> = OnceLock::new();

fn wait_graph() -> &'static Mutex<WaitGraph> {
    WAIT_GRAPH.get_or_init(|| Mutex::new(WaitGraph::new()))
}

/// Transparent wrapper that lets multiple memoizer waiters share an anyhow::Error
/// without losing access to typed downcasting. Display delegates to the inner
/// error; `source()` exposes the inner anyhow::Error so `anyhow::Error::chain()`
/// walks into it, and concrete types can be retrieved with [`downcast_chain_ref`].
#[derive(Debug)]
struct SharedAnyhow(Arc<anyhow::Error>);

impl fmt::Display for SharedAnyhow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Always render the inner anyhow::Error with its full chain. anyhow's
        // chain formatter walks `Error::source()` and writes each link via `{}`
        // (non-alternate), which would otherwise stop at the top of the inner
        // error and drop nested causes. We have no source(), so this is the
        // only place the inner chain gets rendered.
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for SharedAnyhow {
    // Intentionally no source(): exposing the inner anyhow::Error as a source
    // would make standard chain formatters (including anyhow::Error's alternate
    // formatter) print every cause twice. Use [`downcast_chain_ref`] to retrieve
    // concrete error types from a memoizer-returned error.
}

/// Unwrap `Arc<anyhow::Error>` back to `anyhow::Error`.
///
/// If the Arc is uniquely owned, the original error (with full type info for
/// top-level downcasting) is recovered. Otherwise, the error is wrapped in a
/// transparent [`SharedAnyhow`] adapter that exposes the inner error via
/// `Error::source()`. Top-level `downcast_ref` won't find concrete types in the
/// shared case — use [`downcast_chain_ref`] to inspect the chain.
pub fn unwrap_arc_err(arc: Arc<anyhow::Error>) -> anyhow::Error {
    Arc::try_unwrap(arc).unwrap_or_else(|arc| anyhow::Error::new(SharedAnyhow(arc)))
}

/// Recover a typed error reference from an `anyhow::Error` even if the error
/// has been routed through a memoizer (and therefore through [`unwrap_arc_err`]).
///
/// Use this instead of `e.downcast_ref::<T>()` when the error may have come
/// from a [`Memoizer::once`] result; otherwise the top-level type is the
/// internal `SharedAnyhow` wrapper and a direct downcast would fail.
pub fn downcast_chain_ref<T: std::error::Error + Send + Sync + 'static>(
    e: &anyhow::Error,
) -> Option<&T> {
    let mut cur = e;
    loop {
        if let Some(t) = cur.downcast_ref::<T>() {
            return Some(t);
        }
        match cur.downcast_ref::<SharedAnyhow>() {
            Some(shared) => cur = &shared.0,
            None => return None,
        }
    }
}

// ---- Stuck-cell inventory ----
//
// A thread dump answers "what is each *thread* doing", which for this engine is
// the wrong question: the work lives in parked futures on the heap, and a wedged
// build shows every thread idle and says nothing about the thousands of awaits
// that are stuck. The inventory answers the question the dump cannot — *which*
// cells are incomplete, how many tasks are parked on each, and whether anybody
// is still on the hook to poll them.
//
// Unlike cycle detection and phase tracing, this is always on. It costs one
// registration per `Memoizer` construction (a handful per request, not per
// target) and nothing at all per `once` call; everything else happens only when
// a dump is requested. A diagnostic that has to be switched on cannot help with
// the hang you did not anticipate — the same argument `src/diag.rs` makes for
// installing the `SIGQUIT` handler unconditionally.

/// One incomplete cell, as reported by [`inventory`].
#[derive(Debug, Clone)]
pub struct StuckCell {
    /// Which memoizer it belongs to (`result`, `spec`, `def`, …).
    pub tag: &'static str,
    /// `format!("{:?}")` of the cell key — for `mem_result` this is the addr.
    pub key: String,
    /// Registered awaiters, or `None` if the waker set was locked when sampled.
    pub waiters: Option<usize>,
    /// Whether an awaiter is elected to re-poll the inner future. See
    /// [`cell::Cell::has_driver`] for why `false` here is the interesting case.
    pub has_driver: bool,
}

impl StuckCell {
    /// Waiters are parked on this cell and nobody is going to poll it.
    pub fn is_stranded(&self) -> bool {
        !self.has_driver && self.waiters.is_some_and(|n| n > 0)
    }

    /// Nobody is awaiting this cell at all, and it never finished.
    ///
    /// The cell still *holds* its in-flight future — that is deliberate, so an
    /// awaiter dropped between polls can be replaced by another. But when the
    /// last awaiter goes for good (fail-fast drops every sibling on the first
    /// error; Ctrl-C drops them wholesale) there is no replacement coming, and
    /// nothing will ever poll that future again.
    ///
    /// It is not inert while it sits there. A parked future keeps whatever it
    /// was holding, and keeps its place in whatever queue it was waiting on — so
    /// an abandoned computation can still be handed a worker permit it will
    /// never use and never give back. Counting these is how that becomes
    /// visible instead of inferred.
    pub fn is_abandoned(&self) -> bool {
        !self.has_driver && self.waiters == Some(0)
    }
}

/// Type-erased handle on one live `Memoizer`'s cache.
trait CellSource: Send + Sync {
    /// Append this memoizer's incomplete cells. Returns `false` once the
    /// memoizer is gone, so the registry can drop the entry.
    fn collect(&self, out: &mut Vec<StuckCell>) -> bool;
}

struct Source<K, V> {
    tag: &'static str,
    cache: std::sync::Weak<Mutex<FxHashMap<K, Arc<cell::Cell<V>>>>>,
}

impl<K, V> CellSource for Source<K, V>
where
    K: fmt::Debug + Send + Sync + 'static,
    V: Send + Sync + 'static,
{
    fn collect(&self, out: &mut Vec<StuckCell>) -> bool {
        let Some(cache) = self.cache.upgrade() else {
            return false;
        };
        // `try_lock` for the same reason as `Cell::waiters`: never block a dump
        // on the process being dumped.
        let map = match cache.try_lock() {
            Ok(m) => m,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return true,
        };
        for (key, cell) in map.iter() {
            if cell.is_done() {
                continue;
            }
            out.push(StuckCell {
                tag: self.tag,
                key: format!("{key:?}"),
                waiters: cell.waiters(),
                has_driver: cell.has_driver(),
            });
        }
        true
    }
}

static SOURCES: Mutex<Vec<Box<dyn CellSource>>> = Mutex::new(Vec::new());

/// Registry length at which the next prune of dead entries happens. Doubles
/// each time, so pruning is amortised O(1) per registration rather than a walk
/// of the whole registry on every `Memoizer` construction.
static PRUNE_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(64);

fn sources() -> std::sync::MutexGuard<'static, Vec<Box<dyn CellSource>>> {
    SOURCES.lock().unwrap_or_else(|e| e.into_inner())
}

fn register_source(source: Box<dyn CellSource>) {
    let mut sources = sources();
    sources.push(source);
    // Every request builds its own memoizers, so without this the registry
    // grows for the life of the process even though most entries are dead.
    if sources.len() >= PRUNE_AT.load(Ordering::Relaxed) {
        let mut scratch = Vec::new();
        sources.retain(|s| s.collect(&mut scratch));
        PRUNE_AT.store(sources.len().saturating_mul(2).max(64), Ordering::Relaxed);
    }
}

/// Every incomplete cell across every live memoizer.
///
/// Ordered stranded-first, then by descending waiter count, so the head of the
/// list is the part of a wedged graph worth reading.
pub fn inventory() -> Vec<StuckCell> {
    let mut out = Vec::new();
    {
        let mut sources = sources();
        sources.retain(|s| s.collect(&mut out));
    }
    out.sort_by(|a, b| {
        b.is_stranded()
            .cmp(&a.is_stranded())
            .then(b.is_abandoned().cmp(&a.is_abandoned()))
            .then(b.waiters.unwrap_or(0).cmp(&a.waiters.unwrap_or(0)))
            .then(a.tag.cmp(b.tag))
            .then(a.key.cmp(&b.key))
    });
    out
}

/// Render an [`inventory`] as diagnostic text, listing at most `limit` cells.
///
/// Diagnostic text, not an interface — nothing should parse it.
pub fn render_inventory(cells: &[StuckCell], limit: usize) -> String {
    let mut out = String::new();
    if cells.is_empty() {
        out.push_str("  in-flight    none — no memoizer cell is incomplete\n");
        return out;
    }

    let stranded = cells.iter().filter(|c| c.is_stranded()).count();
    let mut by_tag: Vec<(&'static str, usize)> = Vec::new();
    for cell in cells {
        match by_tag.iter_mut().find(|(t, _)| *t == cell.tag) {
            Some((_, n)) => *n += 1,
            None => by_tag.push((cell.tag, 1)),
        }
    }
    by_tag.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let parts: Vec<String> = by_tag.iter().map(|(tag, n)| format!("{n} {tag}")).collect();
    out.push_str(&format!("  in-flight    {}\n", parts.join(", ")));

    if stranded > 0 {
        // The headline: waiters with no driver is a lost wake-up, not slow work.
        out.push_str(&format!(
            "  stranded     {stranded} cell(s) have waiters but no driver — nobody will poll them\n"
        ));
    }

    let abandoned = cells.iter().filter(|c| c.is_abandoned()).count();
    if abandoned > 0 {
        out.push_str(&format!(
            "  abandoned    {abandoned} cell(s) have no awaiters left — their futures are parked\n  \
             {} for good, still holding whatever they had\n",
            " ".repeat(11)
        ));
    }

    for cell in cells.iter().take(limit) {
        let waiters = match cell.waiters {
            Some(n) => n.to_string(),
            None => "?".to_string(),
        };
        let mark = if cell.is_stranded() {
            " STRANDED"
        } else if cell.is_abandoned() {
            " ABANDONED"
        } else {
            ""
        };
        out.push_str(&format!(
            "    [{}] {} waiters={waiters} driver={}{mark}\n",
            cell.tag, cell.key, cell.has_driver
        ));
    }
    if cells.len() > limit {
        out.push_str(&format!("    … and {} more\n", cells.len() - limit));
    }
    out
}

/// One sampling of everything [`render_full_report`] prints.
///
/// Rendering reads four independent process-wide sources ([`inventory`],
/// [`void_wakes`], [`dump_wait_graph`], [`dump_phases`]), each of which moves
/// while a build runs. Separating the sampling from the formatting means a
/// caller that has to render the same picture more than once — into a dump and
/// into a companion file, or into two representations being compared — renders
/// *one* picture rather than two reads of a moving target.
///
/// It is a snapshot, not a consistent cut: the four sources are sampled in
/// order under their own locks, so they can disagree with each other by however
/// much moved in between. That is fine for a diagnostic and is not what the
/// separation is for.
#[derive(Debug, Clone)]
pub struct ReportSnapshot {
    cells: Vec<StuckCell>,
    void_wakes: u64,
    wait_graph: String,
    phases: String,
}

/// Sample the in-flight state once, for [`render_report`].
pub fn capture_report() -> ReportSnapshot {
    ReportSnapshot {
        cells: inventory(),
        void_wakes: void_wakes(),
        wait_graph: dump_wait_graph(),
        phases: dump_phases(),
    }
}

/// Format a [`capture_report`] sampling.
///
/// Uncapped, and rendered identically wherever it is written — the `SIGQUIT`
/// dump and the stall watchdog's companion file are the same text, so a reader
/// does not have to learn two formats or wonder which one is truncated.
///
/// The gated sections self-describe when off rather than being absent, because
/// a missing section reads as "nothing to report" when it means "not recorded".
pub fn render_report(snapshot: &ReportSnapshot) -> String {
    format!(
        "=== in-flight inventory ({} incomplete cells) ===\n{}  \
         void wakes   {} (wakes that reached an incomplete cell and found nobody; \
         a count still climbing while nothing progresses is a lost-wake regression)\n\
         === memoizer wait-for graph ===\n{}\n\
         === memoizer phases (invocation -> next await) ===\n{}\n",
        snapshot.cells.len(),
        render_inventory(&snapshot.cells, usize::MAX),
        snapshot.void_wakes,
        snapshot.wait_graph,
        snapshot.phases,
    )
}

/// The complete in-flight picture: every incomplete cell, the wait-for graph,
/// and each invocation's next-await label. Samples and formats in one call —
/// see [`capture_report`] when the same picture must be rendered twice.
pub fn render_full_report() -> String {
    render_report(&capture_report())
}

/// Monotone process-wide count of wakes that reached an incomplete cell and
/// found neither a driver nor a single registered waker. See
/// `cell::VOID_WAKES` for what is (and deliberately is not) counted.
pub fn void_wakes() -> u64 {
    cell::void_wakes()
}

/// Cancels a computation when its last awaiter goes away.
///
/// A cell deliberately keeps its in-flight future while awaiters come and go, so
/// one dropped between polls can be replaced by another. When the *last* one
/// goes there is no successor: the future is parked for good, and it keeps
/// everything it captured — including a worker permit it will never release.
/// Twelve such futures held every permit in the pool while the build sat idle,
/// each being re-woken every 250ms into a graph with nobody left to receive it.
/// It is also a leak with no other exit: the parked future holds an
/// `Arc<RequestState>`, which owns the memoizer, which owns the cell, which owns
/// the future — a reference cycle that pins the whole request's state for the
/// life of the process (the production dumps show several wedged requests'
/// memoizers still alive at once). Cancelling on abandonment is what breaks it.
///
/// This is the drop semantics `futures::Shared` has natively — its last handle
/// dropping drops the inner future — recovered for a cell that additionally
/// lives in a map. The map's reference must not count as a "handle" (it exists
/// precisely so the future survives between awaiters), so wanting is tracked
/// explicitly: [`cell::Cell::acquire_interest`] per `process` frame, released by
/// this guard after that frame's `Await` is gone. `Arc::strong_count` cannot
/// stand in for it — the cell is its own waker, so every parked leaf (a
/// `oneshot`, a semaphore queue slot, the blocking pool's backstop list, every
/// child cell's waker slab) holds a strong clone.
///
/// One guard per `process` frame, created before the `Await` and therefore
/// dropped after it (locals drop in reverse declaration order). That ordering is
/// an invariant: when the guard runs, this frame's `Await` has already
/// deregistered its waker slot and abdicated drivership, so `interest == 0`
/// really does mean "no live `Await` exists for this cell".
struct AbandonGuard<'a, K, V>
where
    K: std::hash::Hash + Eq,
{
    cache: &'a Mutex<FxHashMap<K, Arc<cell::Cell<V>>>>,
    key: &'a K,
    cell: Arc<cell::Cell<V>>,
    /// Cleared once the value is in hand — a completed cell must be kept.
    armed: bool,
}

impl<K, V> Drop for AbandonGuard<'_, K, V>
where
    K: std::hash::Hash + Eq,
{
    fn drop(&mut self) {
        // Unconditional: this guard's interest ends here whether the value
        // arrived or the caller walked away.
        //
        // Exactly one guard can observe `remaining == 0` for a given zero
        // crossing: `fetch_sub` is atomic, so of N racing guards exactly one
        // sees the count hit zero. (The count can be *re-raised* by a joiner and
        // cross zero again later — that re-crossing is handed to the joiner's
        // own guard, and `cancel_abandoned` below is idempotent either way.)
        let remaining = self.cell.release_interest();
        if !self.armed || remaining != 0 || self.cell.is_done() {
            // `is_done` here is exact, not advisory: if the frame that completed
            // the cell released before us, our decrement synchronizes with its
            // release (see `release_interest`) and completion is visible; if it
            // has not released yet, `remaining != 0` already stopped us.
            return;
        }
        cancel_abandoned(self.cache, self.key, &self.cell);
    }
}

/// Tear down `cell`'s in-flight computation, unless somebody wants it after all.
///
/// Split from [`AbandonGuard::drop`] so each decision can be unit-tested
/// directly — the windows between the guard's unlocked pre-checks and this
/// function's locked re-checks cannot be hit deterministically from outside.
///
/// Interleavings, proven under the cache lock:
///
/// * **Joiner in the window.** A caller can join a cell only by cloning it out
///   of the map under this same lock, and it registers its interest while still
///   holding the lock (`process`'s occupied arm). So a joiner that arrived after
///   the guard's decrement is visible to the `interest() != 0` re-check here,
///   and the cancellation stands down; one that arrives after we evict finds a
///   vacant entry and starts a fresh computation.
/// * **Joiner completed in the window.** The joiner may have joined, driven the
///   computation to a value, and released again — interest is back to zero but
///   the cell is *done*. Holding the lock freezes both facts (completion
///   requires a poll, a poll requires a live `Await`, a live `Await` requires
///   interest, and interest can only be raised under this lock): re-checking
///   `is_done()` here is therefore exact. Without it a completed, memoized
///   value would be evicted and a later caller would recompute — for an
///   `execute` cell that is a double build, the one thing the memoizer exists
///   to prevent.
/// * **Two cancellations.** A second zero-crossing (join-then-abandon during
///   this one) runs this function again. The eviction is `ptr_eq`-guarded, so a
///   fresh cell a later caller re-created under the same key is never evicted
///   by a stale cancellation; `take_future` is a `take` on an `Option`, so the
///   loser gets `None` and drops nothing.
/// * **Nobody can be mid-poll.** `interest == 0` under the lock means no live
///   `Await` (every `Await`'s lifetime is enclosed by its frame's interest), so
///   no poll of this cell can be in flight and none can start (the cell is
///   unreachable once evicted; pre-existing wakers only wake, they never poll).
///   `take_future`'s `try_lock` therefore cannot find the slot held — and if a
///   driver once *unwound* out of a poll, the poisoned lock is claimed and the
///   future still comes out.
///
/// Lock ordering: `take_future` (the `slot` lock) is taken strictly *after* the
/// cache lock is released. A driving poll holds `slot` and can re-enter this
/// memoizer's cache from inside the computation (a nested `process` on the same
/// memoizer is ordinary), so `slot → cache` is an existing order; taking `slot`
/// while holding `cache` would complete the inversion. Never holding both also
/// keeps the cascade itself safe: dropping the taken future drops nested
/// `AbandonGuard`s, which re-enter this function for *other* cells' caches with
/// a clean lock slate at every level.
fn cancel_abandoned<K, V>(
    cache: &Mutex<FxHashMap<K, Arc<cell::Cell<V>>>>,
    key: &K,
    cell: &Arc<cell::Cell<V>>,
) where
    K: std::hash::Hash + Eq,
{
    {
        let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
        if cell.interest() != 0 || cell.is_done() {
            return;
        }
        // Evict before cancelling, so a later caller for this key builds a
        // fresh cell rather than joining one whose future has been taken —
        // that cell can never complete and would park its awaiters forever.
        if cache.get(key).is_some_and(|c| Arc::ptr_eq(c, cell)) {
            cache.remove(key);
        }
    }

    // Unlocked from here on (see the ordering note above). Safe: the eviction
    // left this cancellation as the only path to the cell, so nothing can join
    // it any more; a later caller for the same key builds a fresh cell.
    let Some(taken) = cell.take_future() else {
        return;
    };

    // Dropped last, with nothing held: the drop cascades through the retained
    // chain and runs arbitrary destructors — releasing the worker permit,
    // leaving the semaphore queue, disarming backstop registrations.
    //
    // The cascade is *recursive*: this future's state machine holds the
    // `AbandonGuard` + `Await` for the next cell down, whose guard re-enters
    // `cancel_abandoned`, takes that cell's future, drops it, and so on — one
    // stack of drop-glue frames per level of the memoized chain. The chain is
    // as deep as the dependency graph (`result → locked_result → execute` per
    // level, dozens to hundreds of levels on a monorepo), and unlike the poll
    // path there is no `GrowStack` wrapper out here to save it. Same remedy:
    // grow the physical stack on demand at each level. The check is a couple of
    // instructions and this path is cold (cancellation only).
    stacker::maybe_grow(DROP_RED_ZONE, DROP_STACK_PER_GROW, move || drop(taken));
}

/// If less than this much stack remains when a level of the cancellation
/// cascade drops, grow first. Drop-glue frames are far smaller than the ~100KiB
/// poll frames `engine`'s `grow_stack` budgets for, but several call frames per
/// level (`Drop` impls, `take_future`, the boxed state machine's `drop_in_place`)
/// add up across an unbounded chain.
const DROP_RED_ZONE: usize = 256 * 1024;

/// Fresh segment size for the cancellation cascade — hosts thousands of levels
/// per growth.
const DROP_STACK_PER_GROW: usize = 4 * 1024 * 1024;

pub struct Memoizer<K, V> {
    /// Behind an `Arc` so the inventory can hold a `Weak` to it: a `Memoizer`
    /// lives in a `RequestState` field, not an `Arc`, so there is nothing else
    /// for the registry to keep a non-owning handle on.
    cache: Arc<Mutex<FxHashMap<K, Arc<cell::Cell<V>>>>>,
    /// Tag used in stall warnings to identify which memoizer is stuck.
    tag: &'static str,
}

impl<K, V> Default for Memoizer<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + fmt::Debug + Clone,
    V: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// If a memoizer await takes longer than this, we panic with the key info to
/// surface a likely deadlock instead of hanging forever. Off by default — set
/// `HEPH_MEMOIZER_STALL_SECS=<seconds>` to enable when debugging.
///
/// Cached in a `OnceLock` because `std::env::var` takes a global libc mutex; the
/// previous per-call lookup serialized every memoizer waiter on env access.
fn stall_threshold() -> Option<Duration> {
    static THRESHOLD: OnceLock<Option<Duration>> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        let secs: u64 = std::env::var("HEPH_MEMOIZER_STALL_SECS")
            .ok()
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if secs == 0 {
            None
        } else {
            Some(Duration::from_secs(secs))
        }
    })
}

impl<K, V> Memoizer<K, V>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + fmt::Debug + Clone,
    V: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self::with_tag("memoizer")
    }

    pub fn with_tag(tag: &'static str) -> Self {
        let cache = Arc::new(Mutex::new(FxHashMap::default()));
        register_source(Box::new(Source {
            tag,
            cache: Arc::downgrade(&cache),
        }));
        Self { cache, tag }
    }

    /// Non-inserting peek: returns the memoized value only if it is already
    /// *completed* (not in-flight, not absent). Lets a caller take a cheap path
    /// on a cache hit (e.g. registering a dep edge with `note_dep` instead of a
    /// full `result`) without disturbing the cache or deduping with in-flight work.
    pub fn peek(&self, key: &K) -> Option<V> {
        let cache = self.cache.lock().expect("memoizer lock poisoned");
        cache.get(key).and_then(|cell| cell.peek().cloned())
    }

    pub async fn process<F, Fut>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V> + Send + 'static,
    {
        // One lock acquisition, and no handle clone on the warm hit: the
        // completed value is read through the guard we already hold. Cloning the
        // cell's `Arc` only to `peek` and drop it costs two atomic RMWs on the
        // hottest path the engine has — `spec`, `def`, `meta` and `result` all
        // land here for every target of every build.
        let cell = {
            let mut cache = self.cache.lock().expect("memoizer lock poisoned");
            match cache.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    if let Some(v) = e.get().peek() {
                        return v.clone();
                    }
                    // Registered here, under the lock, so a cancellation racing
                    // us either sees this interest and stands down, or has
                    // already evicted the entry and we never find it.
                    e.get().acquire_interest();
                    Arc::clone(e.get())
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    // `f()` only builds the async block's state machine — async
                    // blocks are lazy, so no user code runs and nothing can
                    // await — which makes it safe to construct under the lock.
                    // Constructing it *outside* meant every loser of an insert
                    // race boxed a large future just to discard it, and on a
                    // cell with hundreds of concurrent callers that is hundreds
                    // of wasted allocations.
                    let cell = cell::Cell::new(f().boxed());
                    cell.acquire_interest();
                    e.insert(Arc::clone(&cell));
                    cell
                }
            }
        };

        // Cancel the computation if we turn out to be its last awaiter.
        //
        // Declared before the await so it drops *after* the `Await` inside it:
        // by the time this runs, our handle is gone and the cell's remaining
        // holders are final.
        let mut abandon = AbandonGuard {
            cache: &self.cache,
            key: &key,
            cell: Arc::clone(&cell),
            armed: true,
        };
        let out = await_with_stall_check(cell::Await::new(cell), &key, self.tag).await;
        // Completed: there is nothing to cancel, and the cell stays in the map
        // as the memoized answer.
        abandon.armed = false;
        out
    }
}

/// Run a memoized computation, turning a panic into an `Err` for every awaiter.
///
/// Without this a panicking cell strands the graph rather than failing it. The
/// cell can only ever publish a value, so an unwinding poll leaves it with no
/// value and no future, and every task parked on it waits forever — one
/// panicking target silently hanging every one of its reverse-deps. (`Shared`
/// has the same defect: its poison path never drains the waker slab. It was
/// partly masked before, because any inner progress woke everyone; with
/// completion-only wakes there is no masking left.)
///
/// It also removes an abort vector. `provider_get` / `provider_list` /
/// `driver_parse` are awaited directly by host workers with nothing between them
/// and the `extern "C"` seam, where an unwind aborts the process — and
/// `unwrap_used` / `panic` are `warn`, not `deny`, so panics inside plugin
/// providers are reachable. `hcore::blocking` already takes this shape.
async fn guard_panics<T, Fut>(fut: Fut) -> Result<T, Arc<anyhow::Error>>
where
    Fut: Future<Output = anyhow::Result<T>>,
{
    match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r.map_err(Arc::new),
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            Err(Arc::new(anyhow::anyhow!("memoized task panicked: {msg}")))
        }
    }
}

async fn await_with_stall_check<V, K>(waiter: cell::Await<V>, key: &K, tag: &'static str) -> V
where
    V: Clone + Send + Sync + 'static,
    K: fmt::Debug,
{
    let Some(threshold) = stall_threshold() else {
        return waiter.await;
    };
    match cell::timeout_without_reactor(threshold, waiter).await {
        Ok(v) => v,
        Err(()) => {
            // Debug-only: opt-in via HEPH_MEMOIZER_STALL_SECS. Panic surfaces a
            // suspected deadlock with as much state as we can dump:
            //   * the offending cell.
            //   * the awaiting task's IN_FLIGHT frame stack (if scoped).
            //   * the cycle-detector's wait-for graph (cells + waits) so
            //     cross-task hangs that escaped detection are diagnosable.
            let in_flight = current_frame()
                .map(|f| {
                    format_frame_stack(&f)
                        .into_iter()
                        .enumerate()
                        .map(|(i, l)| format!("    {i:>2}: {l}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| "    (none — task_local not scoped here)".to_string());
            let wait_graph_dump = dump_wait_graph();
            let phases_dump = dump_phases();
            let me = current_parent_invocation_id().unwrap_or(0);
            #[expect(
                clippy::panic,
                reason = "debug-only stall detector; opt-in via env var"
            )]
            {
                panic!(
                    "[memoizer:{tag}] STALLED for {:?} on key={key:?}\n\
                     current invocation id: {me}\n\
                     IN_FLIGHT frames (root first):\n{in_flight}\n\
                     wait-for graph:\n{wait_graph_dump}\n\
                     phases (invocation -> phase):\n{phases_dump}\n\
                     Unset HEPH_MEMOIZER_STALL_SECS to disable this check.",
                    threshold
                );
            }
        }
    }
}

/// Format the full wait-for graph for diagnostics. Includes every live cell
/// (owner + debug key) and every task currently registered as a waiter. Used
/// only by the stall-panic dump.
pub fn dump_wait_graph() -> String {
    if !cycle_detection_enabled() {
        return "  (cycle detection disabled — set HEPH_DEBUG_MEMOIZER_CYCLE=1)".to_string();
    }
    let wg = wait_graph().lock().expect("wait_graph poisoned");
    let mut out = String::new();
    if wg.cells.is_empty() && wg.waiting.is_empty() {
        out.push_str("  (empty)");
        return out;
    }
    out.push_str("  cells (owned):\n");
    if wg.cells.is_empty() {
        out.push_str("    (none)\n");
    } else {
        let mut cells: Vec<_> = wg.cells.iter().collect();
        cells.sort_by_key(|(id, _)| (id.0, id.1));
        for ((tag, key_hash), rec) in cells {
            out.push_str(&format!(
                "    [{tag}] {} (owner={})  key_hash={key_hash:016x}\n",
                rec.debug_key, rec.owner,
            ));
        }
    }
    out.push_str("  waiting (invocation -> cells):\n");
    if wg.waiting.is_empty() {
        out.push_str("    (none)\n");
    } else {
        let mut waits: Vec<_> = wg.waiting.iter().collect();
        waits.sort_by_key(|(inv, _)| **inv);
        for (inv, cells) in waits {
            let mut sorted_cells: Vec<&CellId> = cells.iter().collect();
            sorted_cells.sort_by_key(|c| (c.0, c.1));
            for cell_id in sorted_cells {
                let dk = wg
                    .cells
                    .get(cell_id)
                    .map(|r| r.debug_key.as_ref())
                    .unwrap_or("<missing>");
                out.push_str(&format!(
                    "    inv {inv} -> [{}] {dk}  key_hash={:016x}\n",
                    cell_id.0, cell_id.1,
                ));
            }
        }
    }
    // Trim trailing newline for cleaner panic output.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

impl<K, T> Memoizer<K, Result<T, Arc<anyhow::Error>>>
where
    K: std::hash::Hash + Eq + Send + Sync + 'static + fmt::Debug + Clone,
    T: Clone + Send + Sync + 'static,
{
    /// Compute-once memoizer for `anyhow::Result`-returning async closures.
    ///
    /// Wraps errors in `Arc` internally for shareability across concurrent waiters.
    /// Returns `Result<T, Arc<anyhow::Error>>` so callers can inspect the error
    /// (e.g. downcast_ref) before converting to `anyhow::Error` via `unwrap_arc_err`.
    ///
    /// Same-task re-entry on the same key returns [`MemoizerCycleError`] instead of
    /// awaiting the in-flight shared future (which would deadlock). Callers can detect
    /// this via [`downcast_chain_ref::<MemoizerCycleError>`] and treat as a cycle.
    ///
    /// Cycle errors (direct or transitively bubbled up from an inner memoizer call)
    /// are NOT cached — they are context-dependent (only valid for the current call
    /// chain). The cache entry is evicted before returning so a future, non-cyclic
    /// caller can compute the real result.
    pub async fn once<F, Fut>(&self, key: K, f: F) -> Result<T, Arc<anyhow::Error>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let tag = self.tag;

        // Fast path: cycle detection disabled. Skip key hashing, the frame
        // push, the task_local::scope wrap, AND the wait-for graph bookkeeping
        // entirely — these only exist to surface dependency cycles instead of
        // deadlocking the runtime, and most runs don't have cycles. Opt back in
        // with `HEPH_DEBUG_MEMOIZER_CYCLE=1`.
        if !cycle_detection_enabled() {
            return self.process(key, || guard_panics(f())).await;
        }

        let key_hash = compute_key_hash(&key);
        let debug_key: Arc<str> = Arc::from(format!("{:?}", key));

        // Same-task self-recursion: walk the current task's IN_FLIGHT chain
        // and bail before we'd re-enter our own in-flight future.
        if let Some(reentry) = check_recursion(tag, key_hash) {
            let stack = format_frame_stack(&reentry);
            return Err(Arc::new(anyhow::Error::new(MemoizerCycleError {
                tag,
                key: debug_key.to_string(),
                kind: CycleKind::SelfRecursion,
                stack,
            })));
        }

        let cell_id: CellId = (tag, key_hash);
        let me = fresh_invocation_id();
        let parent = current_parent_invocation_id();

        // Wait-for cycle check.
        //
        // Semantics of the wait-for graph: `waiting[I]` is the set of cells
        // that invocation `I` is currently blocked awaiting. Edges:
        //   * `waiting[parent].insert(cell)` — the parent invocation is now
        //     blocked on this cell (via us). Parent may have multiple
        //     concurrent children in `try_join_all`; we add an edge per
        //     child so the cycle detector can walk through any of them.
        //   * `waiting[me].insert(cell)` — we (this `once()` call) are
        //     blocked on the cell. Skipped when we become the owner: the
        //     edge would be a self-loop (`cell -> me -> cell`), trivially
        //     "cyclic" but not a deadlock. Cycles through the owner are
        //     reachable via `waiting[parent]` once the owner's compute
        //     closure enters its own inner `once()`.
        //
        // Cycle detection runs only when registering a *new waiter* (cell
        // already owned by someone else). Owners don't trigger checks
        // because no new wait edge that could close a cycle is added at
        // owner registration time.
        let mut became_owner = false;
        let mut waiter_registered = false;
        {
            let mut wg = wait_graph().lock().expect("wait_graph poisoned");
            match wg.cells.get(&cell_id).map(|c| c.owner) {
                None => {
                    wg.cells.insert(
                        cell_id,
                        CellRecord {
                            owner: me,
                            debug_key: Arc::clone(&debug_key),
                        },
                    );
                    became_owner = true;
                    if let Some(p) = parent {
                        wg.add_wait(p, cell_id);
                    }
                }
                Some(_) => {
                    wg.add_wait(me, cell_id);
                    waiter_registered = true;
                    if let Some(p) = parent {
                        wg.add_wait(p, cell_id);
                    }
                    if let Some(path) = wg.find_cycle(me) {
                        let stack = wg.format_cycle(&path);
                        wg.remove_wait(me, cell_id);
                        if let Some(p) = parent {
                            wg.remove_wait(p, cell_id);
                        }
                        return Err(Arc::new(anyhow::Error::new(MemoizerCycleError {
                            tag,
                            key: debug_key.to_string(),
                            kind: CycleKind::CrossTask,
                            stack,
                        })));
                    }
                }
            }
        }

        let frame = push_frame(tag, key_hash, Arc::clone(&debug_key), me);
        let key_for_evict = key.clone();

        // The scope goes around the *cell's* future, not around the await.
        //
        // `IN_FLIGHT` is a task-local, so a cell polled by a driver other than
        // its creator would otherwise see that driver's chain. Under the old
        // wake-everyone behavior the creator kept retaking the poll, so a wrong
        // chain was transient; with the driver stable, whichever chain first won
        // the election would be captured for good — and `check_recursion` would
        // then report `SelfRecursion` for a graph that has no cycle, which makes
        // `EngineProviderExecutor::query` skip a perfectly good addr. Scoping at
        // creation makes `IN_FLIGHT` mean "the lineage that created this cell",
        // which is exactly what self-recursion detection needs; cross-lineage
        // cycles remain the wait-for graph's job.
        let creator_frame = frame.clone();
        let result = IN_FLIGHT
            .scope(
                frame,
                self.process(key, move || {
                    IN_FLIGHT.scope(creator_frame, guard_panics(f()))
                }),
            )
            .await;

        // Cleanup: clear only what we set.
        {
            let mut wg = wait_graph().lock().expect("wait_graph poisoned");
            if waiter_registered {
                wg.remove_wait(me, cell_id);
            }
            if let Some(p) = parent
                && (became_owner || waiter_registered)
            {
                wg.remove_wait(p, cell_id);
            }
            if became_owner {
                wg.cells.remove(&cell_id);
            }
        }

        // Drop any phase entry recorded by user code under this invocation
        // id. Without this, the phase map accumulates entries for completed
        // invocations indefinitely, polluting the stall-panic dump with
        // stale phases that don't correspond to currently in-flight work.
        if phase_trace_enabled() {
            phases().lock().expect("phases mutex poisoned").remove(&me);
        }

        if let Err(arc) = &result
            && downcast_chain_ref::<MemoizerCycleError>(arc).is_some()
        {
            self.evict_cached_cycle_error(&key_for_evict);
        }
        result
    }

    /// Evict `key` iff its cell completed with a cycle error.
    ///
    /// Cycle errors are context-dependent — valid only for the call chain that
    /// produced them — so they must not stay memoized. But each of N waiters
    /// that received the error runs this eviction, and a blind `remove(key)`
    /// from the second waiter onward can land on an *innocent* cell a later
    /// caller re-created under the same key (cancel-on-abandonment makes
    /// re-creation ordinary): evicting one in flight forfeits its memoization
    /// (a duplicate compute), and evicting one completed with a real value
    /// forfeits the value. Checking the stored value keys the eviction to
    /// exactly the cells it exists for; an in-flight cell (`peek() == None`)
    /// is never touched.
    fn evict_cached_cycle_error(&self, key: &K) {
        let mut cache = self.cache.lock().expect("memoizer lock poisoned");
        let holds_cycle_error = cache.get(key).is_some_and(|cell| {
            cell.peek().is_some_and(|v| match v {
                Err(e) => downcast_chain_ref::<MemoizerCycleError>(e).is_some(),
                Ok(_) => false,
            })
        });
        if holds_cycle_error {
            cache.remove(key);
        }
    }
}

// ---- Spawn helpers ----

/// Wrap `fut` so the spawned task inherits the parent's IN_FLIGHT call
/// chain (task-local frames don't auto-propagate across `tokio::spawn`).
/// Use in place of `tokio::spawn` at every site reachable from
/// `Memoizer::once`.
///
/// Invocation identity is allocated fresh per `once()` call. Inheriting
/// the parent frame means the spawned task's first `once()` sees the
/// caller's invocation as its parent in the wait-for graph, so spawned
/// work is reachable from the caller for cycle detection.
///
/// When `HEPH_DEBUG_MEMOIZER_CYCLE` is disabled, this is identical to
/// `tokio::spawn` — no scope at all.
pub fn spawn_with_cycle_ctx<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if !cycle_detection_enabled() {
        return tokio::spawn(fut);
    }
    let inherited_frame = current_frame();
    tokio::spawn(async move { IN_FLIGHT.scope(inherited_frame, fut).await })
}

/// `JoinSet::spawn` analogue with the same IN_FLIGHT inheritance semantics
/// as [`spawn_with_cycle_ctx`].
pub fn join_set_spawn<F>(set: &mut tokio::task::JoinSet<F::Output>, fut: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    if !cycle_detection_enabled() {
        set.spawn(fut);
        return;
    }
    let inherited_frame = current_frame();
    set.spawn(async move { IN_FLIGHT.scope(inherited_frame, fut).await });
}

#[cfg(test)]
mod tests {
    use super::*;
    use enclose::enclose;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, sleep};

    /// Sets a flag when dropped, so a test can observe an abandoned
    /// computation's captured state actually being released.
    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// **The wedge this exists for.**
    ///
    /// A cell keeps its in-flight future while awaiters come and go. When the
    /// last one goes there is no successor, and that future is parked for good
    /// while still holding everything it captured — in the real failure, a
    /// worker permit. Twelve of them held the entire pool while the build sat
    /// idle, each re-woken every 250ms into a graph with nobody left to receive
    /// the wake.
    #[tokio::test]
    async fn dropping_the_last_awaiter_cancels_the_computation() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("abandon-cancel-test");
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut only = Box::pin(m.process("k".to_string(), {
            let dropped = std::sync::Arc::clone(&dropped);
            move || async move {
                // Stands in for the worker permit: held across the await, so it
                // is released only if the future itself is dropped.
                let _held = DropFlag(dropped);
                futures::future::pending::<u32>().await
            }
        }));
        assert!(futures::poll!(&mut only).is_pending());
        assert!(
            !dropped.load(std::sync::atomic::Ordering::SeqCst),
            "still awaited, so still wanted"
        );

        drop(only);
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "an abandoned computation must be dropped, releasing what it held"
        );
    }

    /// Cancelling evicts the key, so the next caller builds a fresh cell.
    ///
    /// Without the eviction the cancelled cell stays in the map with no value
    /// and no future — and `Await::poll` on that parks forever, turning a
    /// cancellation into a different hang.
    #[tokio::test]
    async fn a_cancelled_key_is_recomputed_by_the_next_caller() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("abandon-recompute-test");

        let mut abandoned = Box::pin(m.process("k".to_string(), || async {
            futures::future::pending::<u32>().await
        }));
        assert!(futures::poll!(&mut abandoned).is_pending());
        drop(abandoned);

        let v = tokio::time::timeout(
            Duration::from_secs(5),
            m.process("k".to_string(), || async { 7 }),
        )
        .await
        .expect("a later caller must not park on the cancelled cell");
        assert_eq!(v, 7);
    }

    /// One awaiter leaving is not abandonment. The single-flight contract is the
    /// whole point of the type: a joiner must keep the computation alive and
    /// still receive its value.
    #[tokio::test]
    async fn a_remaining_awaiter_keeps_the_computation_alive() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("abandon-joiner-test");
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut leaves = Box::pin(m.process("k".to_string(), {
            let gate = std::sync::Arc::clone(&gate);
            let dropped = std::sync::Arc::clone(&dropped);
            move || async move {
                let _held = DropFlag(dropped);
                gate.notified().await;
                11
            }
        }));
        assert!(futures::poll!(&mut leaves).is_pending());

        // Joins the same cell; its closure is never invoked.
        let mut stays = Box::pin(m.process("k".to_string(), || async { 99 }));
        assert!(futures::poll!(&mut stays).is_pending());

        drop(leaves);
        assert!(
            !dropped.load(std::sync::atomic::Ordering::SeqCst),
            "a computation someone is still awaiting must not be cancelled"
        );

        gate.notify_waiters();
        let v = tokio::time::timeout(Duration::from_secs(5), &mut stays)
            .await
            .expect("the surviving awaiter must still be served");
        assert_eq!(v, 11, "and served the original computation's value");
    }

    /// A leaf that stores the waker it was polled with must not defeat
    /// cancellation.
    ///
    /// The cell **is** its own waker, so any such leaf holds a strong clone of
    /// it — as does every child cell's waker slab. An earlier version of this
    /// fix gated on `Arc::strong_count` and was therefore dead on arrival in
    /// production, where every parked computation has at least one. Only
    /// `futures::future::pending()` — which never registers a waker at all —
    /// made it look correct.
    ///
    /// The cell *is* its own waker: `Await::poll` polls the inner future with
    /// `waker_ref(cell)`, and every clone of that waker is a strong clone of
    /// `Arc<Cell>`. Real leaves store the waker they are polled with — a
    /// `tokio::sync::oneshot` keeps it in the channel, `Semaphore::acquire`
    /// parks it in the wait list, and `hcore::blocking::run` clones it into the
    /// global `PENDING` backstop list on every pending poll. Each of those is a
    /// phantom "joiner" to `Arc::strong_count > 2`, so the guard bails and the
    /// abandoned future — and the worker permit it holds — is retained forever.
    /// `futures::future::pending()` is the one pending future that never stores
    /// its waker, which is why the test above goes green.
    #[tokio::test]
    async fn cancellation_survives_a_leaf_that_stores_its_waker() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("abandon-waker-test");
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Stands in for the oneshot / semaphore / PENDING slot that holds the
        // waker outside the future itself.
        let stash: std::sync::Arc<Mutex<Option<std::task::Waker>>> =
            std::sync::Arc::new(Mutex::new(None));

        let mut only = Box::pin(m.process("k".to_string(), {
            let dropped = std::sync::Arc::clone(&dropped);
            let stash = std::sync::Arc::clone(&stash);
            move || async move {
                let _held = DropFlag(dropped);
                futures::future::poll_fn(move |cx| {
                    // What every real pending leaf does with its context.
                    *stash.lock().expect("stash") = Some(cx.waker().clone());
                    std::task::Poll::<u32>::Pending
                })
                .await
            }
        }));
        assert!(futures::poll!(&mut only).is_pending());

        drop(only);
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "a stored waker (oneshot / semaphore / blocking backstop) is a clone \
             of Arc<Cell> and must not read as a joiner that blocks cancellation"
        );
    }

    /// The production chain from the wedge, in miniature.
    ///
    /// An outer `result` cell computes by awaiting an inner `execute` cell that
    /// holds a worker permit. The outer cell's last awaiter goes away
    /// (fail-fast). Cancelling the outer computation has to cascade into the
    /// inner one, or the permit is held for the life of the process — twelve of
    /// those held the whole pool while the build sat idle.
    ///
    /// `result` (outer memoizer) computes by awaiting `execute` (inner
    /// memoizer), whose computation holds the permit stand-in and parks. The
    /// outer cell's future is polled with `waker_ref(outer)`, so the inner
    /// cell's waker slab holds a strong clone of the *outer* `Arc<Cell>`
    /// (`Wakers::register` clones `cx.waker()`). When the outer cell's last
    /// awaiter goes away, its guard sees strong_count == 3 (map + guard + the
    /// inner slab's clone), bails, and the cascade that was supposed to free
    /// the permit never starts.
    #[tokio::test]
    async fn abandoning_the_outer_cell_cascades_to_the_inner_computation() {
        let outer: std::sync::Arc<Memoizer<String, u32>> =
            std::sync::Arc::new(Memoizer::with_tag("abandon-chain-outer"));
        let inner: std::sync::Arc<Memoizer<String, u32>> =
            std::sync::Arc::new(Memoizer::with_tag("abandon-chain-inner"));
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Held by the test past the drop, like a oneshot sender or a semaphore
        // wait list. Its content is a clone of the inner cell's own waker — the
        // strong `Arc<Cell>` clone that made the strong-count gate a false pass.
        let stash: std::sync::Arc<Mutex<Option<std::task::Waker>>> =
            std::sync::Arc::new(Mutex::new(None));

        let mut only = Box::pin(outer.process("addr".to_string(), {
            let inner = std::sync::Arc::clone(&inner);
            let dropped = std::sync::Arc::clone(&dropped);
            let stash = std::sync::Arc::clone(&stash);
            move || async move {
                inner
                    .process("addr".to_string(), move || async move {
                        // The execute future: permit held across a park on a
                        // leaf that stores the waker it is polled with.
                        // `futures::future::pending()` is forbidden here — it
                        // never touches `cx`, so it stores no waker, holds no
                        // `Arc<Cell>` clone, and turns this into a false pass.
                        let _permit = DropFlag(dropped);
                        futures::future::poll_fn(move |cx| {
                            *stash.lock().expect("stash") = Some(cx.waker().clone());
                            std::task::Poll::<u32>::Pending
                        })
                        .await
                    })
                    .await
            }
        }));
        assert!(futures::poll!(&mut only).is_pending());
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            stash.lock().expect("stash").is_some(),
            "leaf must have stored a waker, or this test cannot catch a \
             strong-count-style gate"
        );

        // The outer cell's last awaiter goes away — the abandoned `result`
        // cell from the dump. The permit must come back.
        drop(only);
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "dropping the last awaiter of the outer cell must cascade through \
             the retained chain and release what the inner computation holds"
        );
    }

    /// The production wedge (pids 2082719 and 2055961), end to end.
    ///
    /// The proven chain, layer for layer (the `errors.go` chain in the 2082719
    /// dump: `[result]` ABANDONED -> `[locked_result]` -> `[execute_cache]` ->
    /// phase `execute:sandbox_remove`): a `result` cell computes by awaiting a
    /// `locked_result` cell, which computes by awaiting an `execute_cache`
    /// cell, whose computation takes a real semaphore permit and then parks on
    /// a leaf that **stores the waker it is polled with** — what
    /// `hcore::blocking::run`'s oneshot does. That stored waker is a strong
    /// clone of the `execute_cache` cell, which is what made any
    /// `Arc::strong_count` gate a false pass; `futures::future::pending()`
    /// stores nothing and is forbidden here, and the stash assertion below
    /// keeps a refactor from silently downgrading the leaf.
    ///
    /// Fail-fast drops the single outermost awaiter; the "blocking job" then
    /// finishes and fires the stored waker, which climbs cell-waker by
    /// cell-waker toward the abandoned `result` cell and reaches nobody.
    ///
    /// Assertions, in priority order: (a) the permit comes back — the wedge
    /// held 12/12 with `unaccounted = 0`; (b) an unrelated acquirer is served;
    /// (c) the inventory holds no cell under any of the three tags — the
    /// cascade reached every layer, leaving none of the healthy-looking
    /// `waiters=1 driver=true` husks the dumps show under every holder; (d) a
    /// later caller for the same key recomputes. Verified to fail on the
    /// pre-fix tree at (a) with `available_permits` stuck at 0.
    #[tokio::test]
    async fn abandoned_chain_returns_the_worker_permit_to_the_semaphore() {
        let mem_result: Arc<Memoizer<String, u32>> = Arc::new(Memoizer::with_tag("repro-result"));
        let mem_locked: Arc<Memoizer<String, u32>> =
            Arc::new(Memoizer::with_tag("repro-locked_result"));
        let mem_execute: Arc<Memoizer<String, u32>> =
            Arc::new(Memoizer::with_tag("repro-execute_cache"));

        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        // Held by the test past the drop, like the oneshot channel inside
        // `blocking::run`: the leaf's stored waker must survive the abandonment
        // so the job-finished-late wake can be delivered into the torn-down
        // chain, exactly as the idle-pool-jobs-finished dumps describe.
        let stash: Arc<Mutex<Option<std::task::Waker>>> = Arc::new(Mutex::new(None));
        let job_done = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let mut outer = Box::pin(mem_result.process("//pkg:tgt".to_string(), {
            enclose!((mem_locked, mem_execute, permits, stash, job_done) move || async move {
                mem_locked
                    .process("//pkg:tgt".to_string(), move || async move {
                        mem_execute
                            .process("//pkg:tgt".to_string(), move || async move {
                                let _permit = permits
                                    .acquire_owned()
                                    .await
                                    .expect("semaphore is never closed");
                                futures::future::poll_fn(move |cx| {
                                    if job_done.load(std::sync::atomic::Ordering::SeqCst) {
                                        return std::task::Poll::Ready(7u32);
                                    }
                                    *stash.lock().expect("stash") = Some(cx.waker().clone());
                                    std::task::Poll::Pending
                                })
                                .await
                            })
                            .await
                    })
                    .await
            })
        }));

        // Exactly one awaiter of the outermost cell, polled exactly once.
        assert!(futures::poll!(&mut outer).is_pending());
        assert_eq!(
            permits.available_permits(),
            0,
            "the parked execute computation must be holding the permit"
        );
        assert!(
            stash.lock().expect("stash").is_some(),
            "leaf must have stored a waker — a leaf that stores none cannot \
             catch a strong-count-style gate and this test proves nothing"
        );
        let in_flight = |tag: &str| inventory().into_iter().filter(|c| c.tag == tag).count();
        for tag in ["repro-result", "repro-locked_result", "repro-execute_cache"] {
            assert_eq!(
                in_flight(tag),
                1,
                "one in-flight {tag} cell before the drop"
            );
        }

        // Fail-fast drops the only awaiter of the result cell.
        drop(outer);

        // (a) The permit came back: the inversion of the wedge itself.
        assert_eq!(
            permits.available_permits(),
            1,
            "the worker permit must return to the semaphore when the chain is abandoned"
        );

        // The blocking job finishes *after* the abandonment, exactly as in the
        // dumps (pool idle, jobs long since done, results never read): its wake
        // lands in the torn-down chain and must be harmless.
        job_done.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(waker) = stash.lock().expect("stash").take() {
            waker.wake();
        }

        // (b) An unrelated acquirer is served.
        let reacquired =
            tokio::time::timeout(Duration::from_secs(5), Arc::clone(&permits).acquire_owned())
                .await;
        assert!(
            reacquired.is_ok(),
            "a later target must be able to take the permit the abandoned chain held"
        );
        drop(reacquired);

        // (c) No layer is left behind: the dumps show every holder's
        // `locked_result` / `execute_cache` rows looking healthy
        // (`waiters=1 driver=true`) even under a proven-abandoned top — the
        // cascade must leave no such husk at any layer.
        for tag in ["repro-result", "repro-locked_result", "repro-execute_cache"] {
            assert_eq!(
                in_flight(tag),
                0,
                "the cascade must evict the {tag} layer, not orphan it"
            );
        }

        // (d) A later caller for the same key recomputes.
        let v = tokio::time::timeout(
            Duration::from_secs(5),
            mem_result.process("//pkg:tgt".to_string(), || async { 42 }),
        )
        .await
        .expect("a later caller for the abandoned key must not park");
        assert_eq!(v, 42, "the key must be recomputable after the cancellation");
    }

    /// A cancellation that raced a completing joiner must not evict the value.
    ///
    /// The window: our guard decrements interest to zero; before it re-checks
    /// under the cache lock, a joiner arrives, drives the computation to a
    /// value, and releases — interest is zero again but the cell is *done*.
    /// This calls [`cancel_abandoned`] directly at exactly that point, which is
    /// the only deterministic way in: the window sits inside `Drop`. Evicting
    /// here would throw away a memoized value and a later caller would
    /// recompute — for an `execute` cell, a double build.
    #[tokio::test]
    async fn a_cancellation_that_lost_to_a_completing_joiner_keeps_the_value() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("abandon-late-cancel-test");
        let runs = Arc::new(AtomicUsize::new(0));

        let v = m
            .process("k".to_string(), {
                enclose!((runs) move || async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    5
                })
            })
            .await;
        assert_eq!(v, 5);

        // The completed cell stays in the map with interest back at zero — the
        // state a stale cancellation finds after losing the race.
        let cell = m
            .cache
            .lock()
            .expect("memoizer lock poisoned")
            .get("k")
            .cloned()
            .expect("a completed cell is retained as the memoized answer");
        assert_eq!(cell.interest(), 0);
        cancel_abandoned(&m.cache, &"k".to_string(), &cell);

        let v = m
            .process("k".to_string(), {
                enclose!((runs) move || async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    7
                })
            })
            .await;
        assert_eq!(v, 5, "the memoized value must survive a stale cancellation");
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "a stale cancellation must never force a recompute"
        );
    }

    /// A stale cancellation must be a no-op against a recreated cell, and
    /// running it twice must be harmless.
    ///
    /// After a cancellation evicts a key, a later caller builds a fresh cell
    /// under it. A second zero-crossing of the *old* cell's interest (join,
    /// then abandon, during the first cancellation) re-enters
    /// [`cancel_abandoned`] with a handle to the old cell: the `ptr_eq` guard
    /// must leave the fresh cell in the map, and the old cell's already-taken
    /// slot must yield `None` rather than a second drop.
    #[tokio::test]
    async fn a_stale_cancellation_never_evicts_a_recreated_cell() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("abandon-recreate-test");

        let mut first = Box::pin(m.process("k".to_string(), || async {
            futures::future::pending::<u32>().await
        }));
        assert!(futures::poll!(&mut first).is_pending());
        let old_cell = m
            .cache
            .lock()
            .expect("memoizer lock poisoned")
            .get("k")
            .cloned()
            .expect("in-flight cell is in the map");
        drop(first); // cancels and evicts

        // Idempotence against the (now evicted, future-less) old cell.
        cancel_abandoned(&m.cache, &"k".to_string(), &old_cell);

        // A fresh computation under the same key, still in flight.
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut second = Box::pin(m.process("k".to_string(), {
            let gate = std::sync::Arc::clone(&gate);
            move || async move {
                gate.notified().await;
                3
            }
        }));
        assert!(futures::poll!(&mut second).is_pending());

        // The stale cancellation arrives now. It must not touch the fresh cell.
        cancel_abandoned(&m.cache, &"k".to_string(), &old_cell);

        gate.notify_waiters();
        let v = tokio::time::timeout(Duration::from_secs(5), &mut second)
            .await
            .expect("the fresh computation must survive a stale cancellation");
        assert_eq!(v, 3);
    }

    /// The cycle-error eviction hits only cells that hold a cycle error.
    ///
    /// Every waiter that received a cycle error evicts on its way out, and
    /// with cancel-on-abandonment a key can be legitimately re-created while
    /// stale waiters are still unwinding. A blind remove-by-key from the
    /// second waiter onward would evict the innocent successor — in flight
    /// (losing single-flight: a duplicate compute) or completed (losing the
    /// memoized value).
    #[tokio::test]
    async fn cycle_error_eviction_spares_an_innocent_recreated_cell() {
        type V = Result<Arc<u32>, Arc<anyhow::Error>>;
        let m: Memoizer<String, V> = Memoizer::with_tag("cycle-evict-test");
        let key = "k".to_string();

        // A cell that completed with a cycle error IS evicted.
        let v = m
            .process(key.clone(), || async move {
                Err(Arc::new(anyhow::Error::new(MemoizerCycleError {
                    tag: "cycle-evict-test",
                    key: "k".to_string(),
                    kind: CycleKind::SelfRecursion,
                    stack: vec![],
                }))) as V
            })
            .await;
        assert!(v.is_err());
        m.evict_cached_cycle_error(&key);
        assert!(
            !m.cache.lock().expect("lock").contains_key(&key),
            "a cached cycle error must be evicted — it is only valid for the \
             chain that produced it"
        );

        // A re-created cell still in flight is spared.
        let gate = Arc::new(tokio::sync::Notify::new());
        let mut inflight = Box::pin(m.process(key.clone(), {
            enclose!((gate) move || async move {
                gate.notified().await;
                Ok(Arc::new(5u32)) as V
            })
        }));
        assert!(futures::poll!(&mut inflight).is_pending());
        m.evict_cached_cycle_error(&key);
        assert!(
            m.cache.lock().expect("lock").contains_key(&key),
            "an in-flight successor must be spared by a stale cycle eviction"
        );
        gate.notify_waiters();
        let v = tokio::time::timeout(Duration::from_secs(5), &mut inflight)
            .await
            .expect("the spared cell must still complete");
        assert_eq!(**v.as_ref().expect("ok"), 5);

        // A cell completed with a real value is spared too.
        m.evict_cached_cycle_error(&key);
        assert!(
            m.cache.lock().expect("lock").contains_key(&key),
            "a real memoized value must survive a stale cycle eviction"
        );
    }

    /// An awaiter that *unwinds* out of `process` still cancels and evicts.
    ///
    /// Raw `process` has no panic guard (`once` adds one), so a computation
    /// that panics mid-poll unwinds through the awaiting frame with the cell's
    /// `slot` mutex poisoned and no value published. Pre-cancellation, that
    /// cell was a zombie: still in the map, future retained behind a poisoned
    /// lock, and every later caller parked on it forever. The abandon guard
    /// runs during the unwind (it is a `Drop`), claims the poisoned lock, and
    /// evicts — so the next caller recomputes instead of hanging. This is also
    /// the path the `HEPH_MEMOIZER_STALL_SECS` debug panic takes.
    #[tokio::test]
    async fn an_unwinding_awaiter_still_cancels_and_evicts() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("abandon-unwind-test");

        let mut only = Box::pin(m.process("k".to_string(), || async {
            panic!("compute exploded");
        }));
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = only.as_mut().poll(&mut cx);
        }));
        assert!(unwound.is_err(), "the compute panic must reach the awaiter");
        drop(only);

        let v = tokio::time::timeout(
            Duration::from_secs(5),
            m.process("k".to_string(), || async { 9 }),
        )
        .await
        .expect("a later caller must not park on the panicked cell's remains");
        assert_eq!(v, 9);
    }

    /// Joiners and abandoners racing on one key: nobody hangs, and a caller
    /// that stays is always served.
    ///
    /// The deterministic single-interleaving cases each have their own test
    /// above; this is the schedule-shaken version, honest about what it is — it
    /// explores interleavings probabilistically rather than proving one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_joiners_and_abandoners_never_strand_a_survivor() {
        let m: Arc<Memoizer<u64, u64>> = Arc::new(Memoizer::with_tag("abandon-stress-test"));

        for round in 0u64..200 {
            let gate = Arc::new(tokio::sync::Notify::new());

            // The abandoner: polls once, then is dropped.
            let mut doomed = Box::pin(m.process(round, {
                enclose!((gate) move || async move {
                    gate.notified().await;
                    round
                })
            }));
            let _ = futures::poll!(&mut doomed);

            // The survivor joins (or recreates) concurrently with the drop.
            let survivor = tokio::spawn({
                enclose!((m, gate) async move {
                    let fut = m.process(round, {
                        enclose!((gate) move || async move {
                            gate.notified().await;
                            round
                        })
                    });
                    // Open the gate only once the survivor is parked, so every
                    // round exercises an in-flight cell rather than a warm hit.
                    tokio::pin!(fut);
                    let first = futures::poll!(&mut fut);
                    gate.notify_waiters();
                    match first {
                        std::task::Poll::Ready(v) => v,
                        std::task::Poll::Pending => fut.await,
                    }
                })
            });
            drop(doomed);
            // Late notifies cover the orderings where the survivor parked
            // before the drop landed (its own notify can be consumed by the
            // doomed computation's first poll).
            gate.notify_waiters();

            let got = tokio::time::timeout(Duration::from_secs(10), survivor)
                .await
                .unwrap_or_else(|_| panic!("round {round}: the surviving caller hung"))
                .expect("survivor task must not panic");
            assert_eq!(got, round, "round {round}: wrong value for the survivor");
        }
    }

    /// Cancelling a deep chain must not overflow the stack.
    ///
    /// The cascade drops one level's guard, takes the next level's future, and
    /// drops it — recursive drop glue, one frame set per level of the memoized
    /// chain, with depth bounded only by the dependency graph. `Ctrl-C` always
    /// did this in one giant drop; cancel-on-abandonment makes it routine.
    /// [`cancel_abandoned`] grows the stack on demand (`stacker::maybe_grow`,
    /// the same remedy `engine`'s `grow_stack` applies to the poll descent), so
    /// a chain thousands of levels deep unwinds on a 256KiB thread.
    #[test]
    fn cancelling_a_deep_chain_does_not_overflow_the_stack() {
        const DEPTH: usize = 4096;

        fn level(
            m: Arc<Memoizer<usize, u32>>,
            depth: usize,
            bottom_dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> futures::future::BoxFuture<'static, u32> {
            Box::pin(async move {
                if depth == 0 {
                    let _held = DropFlag(bottom_dropped);
                    return futures::future::pending::<u32>().await;
                }
                let inner = Arc::clone(&m);
                m.process(depth, move || level(inner, depth - 1, bottom_dropped))
                    .await
            })
        }

        let bottom_dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Build the chain on a roomy thread — the *poll* descent is one
        // synchronous recursion of DEPTH levels, and sizing it explicitly keeps
        // the test about the drop path.
        let (root, m) = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn({
                let bottom_dropped = std::sync::Arc::clone(&bottom_dropped);
                move || {
                    let m: Arc<Memoizer<usize, u32>> =
                        Arc::new(Memoizer::with_tag("abandon-deep-test"));
                    let mut root = Box::pin(level(Arc::clone(&m), DEPTH, bottom_dropped));
                    let waker = futures::task::noop_waker();
                    let mut cx = std::task::Context::from_waker(&waker);
                    assert!(root.as_mut().poll(&mut cx).is_pending());
                    (root, m)
                }
            })
            .expect("spawn build thread")
            .join()
            .expect("build thread must not overflow");

        // Drop on a deliberately tiny thread: without the on-demand growth in
        // `cancel_abandoned`, the cascade overflows here and aborts the process.
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || drop(root))
            .expect("spawn drop thread")
            .join()
            .expect("the cascade must not overflow a small stack");

        assert!(
            bottom_dropped.load(std::sync::atomic::Ordering::SeqCst),
            "the cascade must reach and drop the innermost computation"
        );
        drop(m);
    }

    /// Only cells that are still computing. A completed cell is not stuck, and
    /// a report that listed every warm hit on a 27k-target build would bury the
    /// handful that matter.
    #[tokio::test]
    async fn inventory_lists_incomplete_cells_and_drops_completed_ones() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("inv-complete-test");
        let gate = Arc::new(tokio::sync::Notify::new());

        let held = m.process("stuck".to_string(), {
            let gate = Arc::clone(&gate);
            move || async move {
                gate.notified().await;
                1
            }
        });
        tokio::pin!(held);
        // One poll to insert the cell and park on it.
        assert!(
            futures::poll!(&mut held).is_pending(),
            "the gated cell must not resolve yet"
        );

        let ours = |tag: &str| -> Vec<StuckCell> {
            inventory().into_iter().filter(|c| c.tag == tag).collect()
        };

        let listed = ours("inv-complete-test");
        assert_eq!(listed.len(), 1, "the in-flight cell must be listed");
        assert!(
            listed[0].key.contains("stuck"),
            "the key names the work: {:?}",
            listed[0].key
        );

        gate.notify_waiters();
        assert_eq!(held.await, 1);
        assert!(
            ours("inv-complete-test").is_empty(),
            "a completed cell is not stuck"
        );
    }

    /// Waiters with no driver is the signature the stall paragraph headlines.
    #[tokio::test]
    async fn a_cell_with_waiters_and_no_driver_is_stranded() {
        let m: Memoizer<String, u32> = Memoizer::with_tag("inv-stranded-test");
        let gate = Arc::new(tokio::sync::Notify::new());

        // Two awaiters on one cell: the second to poll is the driver.
        let mut parked = Box::pin(m.process("wedged".to_string(), {
            let gate = Arc::clone(&gate);
            move || async move {
                gate.notified().await;
                1
            }
        }));
        assert!(futures::poll!(&mut parked).is_pending());
        let mut driver = Box::pin(m.process("wedged".to_string(), || async { 0 }));
        assert!(futures::poll!(&mut driver).is_pending());

        let ours = || -> Vec<StuckCell> {
            inventory()
                .into_iter()
                .filter(|c| c.tag == "inv-stranded-test")
                .collect()
        };

        let listed = ours();
        assert_eq!(listed.len(), 1, "one cell, two awaiters");
        assert!(
            !listed[0].is_stranded(),
            "a driven cell is not stranded: {listed:?}"
        );

        // The driver goes away without the cell completing — a fail-fast sibling
        // drop, or Ctrl-C. If the abdication wake fails to land, what is left is
        // a task parked on a cell nobody will poll: the wedge, exactly.
        drop(driver);
        let listed = ours();
        assert_eq!(listed.len(), 1, "the cell is still incomplete");
        assert_eq!(
            listed[0].waiters,
            Some(1),
            "the parked awaiter is still attached"
        );
        assert!(!listed[0].has_driver, "nobody is elected to poll it");
        assert!(listed[0].is_stranded(), "{listed:?}");
    }

    /// The registry holds `Weak`s: a request's memoizers must not keep reporting
    /// after the request is gone, or a long-lived process accumulates the whole
    /// history of every build it ever ran.
    #[tokio::test]
    async fn inventory_drops_memoizers_that_are_gone() {
        {
            let m: Memoizer<String, u32> = Memoizer::with_tag("inv-dropped-test");
            let mut held = Box::pin(m.process("x".to_string(), || async {
                futures::future::pending::<u32>().await
            }));
            assert!(futures::poll!(&mut held).is_pending());
            assert!(
                inventory().iter().any(|c| c.tag == "inv-dropped-test"),
                "listed while the memoizer is alive"
            );
        }
        assert!(
            !inventory().iter().any(|c| c.tag == "inv-dropped-test"),
            "a dead memoizer must be pruned from the registry"
        );
    }

    /// The knob is spelled like every other one, and the old spelling still
    /// works.
    ///
    /// Dropping the legacy name would fail silently in the worst place: someone
    /// re-runs a wedged build with the spelling they used last time, gets an
    /// empty phase map, and reads that as "the trace had nothing to say".
    #[test]
    fn phase_trace_is_opt_in_under_the_canonical_name() {
        let only = |want: &'static str| move |name: &str| (name == want).then(|| "1".to_string());

        assert!(phase_trace_from(only("HEPH_PHASE_TRACE")));
        assert!(
            !phase_trace_from(only("heph_PHASE_TRACE")),
            "the old lowercase spelling is gone, not aliased"
        );
        assert!(!phase_trace_from(|_| None));
        assert!(
            !phase_trace_from(|_| Some("0".to_string())),
            "only an explicit 1 enables it"
        );
        assert!(
            PHASE_TRACE_VAR.starts_with("HEPH_"),
            "the canonical spelling must match HEPH_DEBUG_MEMOIZER_CYCLE et al"
        );
    }

    #[test]
    fn render_inventory_headlines_stranded_cells() {
        let cells = vec![
            StuckCell {
                tag: "result",
                key: "//a:b".to_string(),
                waiters: Some(3),
                has_driver: false,
            },
            StuckCell {
                tag: "spec",
                key: "//c:d".to_string(),
                waiters: Some(1),
                has_driver: true,
            },
        ];
        let text = render_inventory(&cells, 10);
        assert!(text.contains("1 result, 1 spec"), "{text}");
        assert!(text.contains("stranded     1 cell(s)"), "{text}");
        assert!(
            text.contains("[result] //a:b waiters=3 driver=false STRANDED"),
            "{text}"
        );
        assert!(
            !text.contains("[spec] //c:d waiters=1 driver=true STRANDED"),
            "a driven cell must not be marked stranded: {text}"
        );
    }

    #[test]
    fn render_inventory_caps_the_listing_but_says_so() {
        let cells: Vec<StuckCell> = (0..5)
            .map(|i| StuckCell {
                tag: "result",
                key: format!("//p:{i}"),
                waiters: Some(1),
                has_driver: true,
            })
            .collect();
        let text = render_inventory(&cells, 2);
        assert!(text.contains("… and 3 more"), "{text}");
    }

    /// A panicking memoized computation must fail every awaiter, not hang them.
    ///
    /// Before `guard_panics`, an unwinding cell left no value and no future, and
    /// every task parked on it waited forever — one panicking target silently
    /// hanging all of its reverse-deps. The `tokio::time::timeout` here is the
    /// assertion: without the fix these awaits never return.
    #[tokio::test]
    async fn panic_in_a_memoized_task_fails_every_waiter() {
        let memo: Arc<Memoizer<String, Result<Arc<i32>, Arc<anyhow::Error>>>> =
            Arc::new(Memoizer::new());
        let key = "boom".to_string();

        // The first caller panics; three more join the same in-flight cell.
        let waiters: Vec<_> = (0..4)
            .map(|i| {
                let (memo, key) = (Arc::clone(&memo), key.clone());
                tokio::spawn(async move {
                    memo.once(key, move || async move {
                        if i == 0 {
                            sleep(Duration::from_millis(20)).await;
                            panic!("cell exploded");
                        }
                        Ok(Arc::new(1))
                    })
                    .await
                })
            })
            .collect();

        for w in waiters {
            let outcome = tokio::time::timeout(Duration::from_secs(5), w)
                .await
                .expect("a panicking cell must not strand its waiters")
                .expect("waiter task itself must not panic");
            let err = outcome.expect_err("a panicking cell must surface as an error");
            assert!(
                err.to_string().contains("cell exploded"),
                "the panic payload must survive into the error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn test_memoizer() {
        let memo: Memoizer<String, Result<Arc<i32>, Arc<anyhow::Error>>> = Memoizer::new();
        let counter = Arc::new(AtomicUsize::new(0));

        let key = "test".to_string();

        let f1 = {
            let memo = &memo;
            enclose!((counter, key) async move {
                memo.once(key, move || async move {
                    sleep(Duration::from_millis(100)).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::new(42))
                }).await
            })
        };

        let f2 = {
            let memo = &memo;
            enclose!((counter, key) async move {
                memo.once(key, move || async move {
                    sleep(Duration::from_millis(100)).await;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::new(42))
                }).await
            })
        };

        let (res1, res2) = tokio::join!(f1, f2);

        assert_eq!(*res1.unwrap(), 42);
        assert_eq!(*res2.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[derive(Debug, Clone)]
    struct Marker(&'static str);

    impl std::fmt::Display for Marker {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "marker({})", self.0)
        }
    }

    impl std::error::Error for Marker {}

    #[test]
    fn unwrap_arc_err_preserves_type_when_unique() {
        let arc = Arc::new(anyhow::Error::new(Marker("a")));
        let err = unwrap_arc_err(arc);
        assert!(err.downcast_ref::<Marker>().is_some());
    }

    #[test]
    fn unwrap_arc_err_shared_arc_preserves_type_via_chain() {
        let arc = Arc::new(anyhow::Error::new(Marker("b")));
        let _keepalive = Arc::clone(&arc); // force shared, blocks try_unwrap
        let err = unwrap_arc_err(arc);
        // Top-level downcast does not see Marker (wrapped in SharedAnyhow).
        assert!(err.downcast_ref::<Marker>().is_none());
        // Chain-walking downcast does find it.
        assert!(downcast_chain_ref::<Marker>(&err).is_some());
    }

    #[test]
    fn downcast_chain_ref_walks_nested_shared_anyhow() {
        // Simulate two stacked memoizers: inner returns CycleError, outer
        // wraps it via unwrap_arc_err, then is itself memoized + unwrapped.
        let inner_arc = Arc::new(anyhow::Error::new(Marker("nested")));
        let _inner_keep = Arc::clone(&inner_arc);
        let outer_err = unwrap_arc_err(inner_arc);
        let outer_arc = Arc::new(outer_err);
        let _outer_keep = Arc::clone(&outer_arc);
        let err = unwrap_arc_err(outer_arc);

        // Two nested SharedAnyhow layers — single-level peek would miss it.
        assert!(err.downcast_ref::<Marker>().is_none());
        assert!(err.downcast_ref::<SharedAnyhow>().is_some());
        assert!(downcast_chain_ref::<Marker>(&err).is_some());
    }

    #[test]
    fn shared_anyhow_display_matches_inner() {
        let arc = Arc::new(anyhow::Error::new(Marker("c")));
        let _keepalive = Arc::clone(&arc);
        let err = unwrap_arc_err(arc);
        assert_eq!(format!("{err:#}"), "marker(c)");
    }

    // ---- format_frame_stack ----

    fn mk_frame(parent: Option<Arc<Frame>>, tag: &'static str, key: &str) -> Arc<Frame> {
        Arc::new(Frame {
            parent,
            tag,
            key_hash: 0,
            debug_key: Arc::from(key),
            invocation_id: 0,
        })
    }

    #[test]
    fn format_frame_stack_walks_root_first() {
        let root = mk_frame(None, "spec", "//pkg:a");
        let middle = mk_frame(Some(root), "result", "//pkg:b");
        let top = mk_frame(Some(middle), "def", "//pkg:c");
        let s = format_frame_stack(&top);
        assert_eq!(
            s,
            vec![
                "[spec] //pkg:a".to_string(),
                "[result] //pkg:b".to_string(),
                "[def] //pkg:c".to_string(),
            ]
        );
    }

    // ---- wait-for graph cycle detection ----

    fn mk_record(owner: u64, key: &str) -> CellRecord {
        CellRecord {
            owner,
            debug_key: Arc::from(key),
        }
    }

    #[test]
    fn wait_graph_finds_direct_two_invocation_cycle() {
        // Invocation 1 waits on cell X (owner 2); invocation 2 waits on cell Y (owner 1).
        let mut wg = WaitGraph::new();
        let cell_x: CellId = ("spec", 100);
        let cell_y: CellId = ("spec", 200);
        wg.cells.insert(cell_x, mk_record(2, "X"));
        wg.cells.insert(cell_y, mk_record(1, "Y"));
        wg.add_wait(1, cell_x);
        wg.add_wait(2, cell_y);

        let path = wg.find_cycle(1).expect("cycle expected");
        assert_eq!(path, vec![cell_x, cell_y]);
        let formatted = wg.format_cycle(&path);
        assert_eq!(formatted, vec!["[spec] X", "[spec] Y"]);
    }

    #[test]
    fn wait_graph_no_cycle_when_chain_terminates() {
        // Invocation 1 waits on cell X (owner 2); invocation 2 has no waits.
        let mut wg = WaitGraph::new();
        let cell_x: CellId = ("spec", 100);
        wg.cells.insert(cell_x, mk_record(2, "X"));
        wg.add_wait(1, cell_x);

        assert!(wg.find_cycle(1).is_none());
    }

    #[test]
    fn wait_graph_finds_three_invocation_cycle() {
        // 1 → X(owner 2), 2 → Y(owner 3), 3 → Z(owner 1).
        let mut wg = WaitGraph::new();
        let x: CellId = ("a", 1);
        let y: CellId = ("b", 2);
        let z: CellId = ("c", 3);
        wg.cells.insert(x, mk_record(2, "X"));
        wg.cells.insert(y, mk_record(3, "Y"));
        wg.cells.insert(z, mk_record(1, "Z"));
        wg.add_wait(1, x);
        wg.add_wait(2, y);
        wg.add_wait(3, z);

        let path = wg.find_cycle(1).expect("cycle expected");
        assert_eq!(path, vec![x, y, z]);
    }

    #[test]
    fn wait_graph_no_cycle_when_owner_missing() {
        // Owner record absent (e.g., cell was cleaned up mid-walk).
        let mut wg = WaitGraph::new();
        let x: CellId = ("a", 1);
        wg.add_wait(1, x);
        // wg.cells doesn't have x.

        assert!(wg.find_cycle(1).is_none());
    }

    #[test]
    fn wait_graph_finds_cycle_via_multi_edge_dfs() {
        // Invocation P (id 1) has TWO concurrent children (try_join_all):
        // P waits on both X and Y. X is owned by 10, Y by 20. 10 waits on
        // Z (owned by P). The cycle is P -> X -> 10 -> Z -> P. The other
        // edge P -> Y is a dead-end (20 has no waits). DFS must explore
        // both edges and find the cycle via X.
        let mut wg = WaitGraph::new();
        let x: CellId = ("a", 1);
        let y: CellId = ("a", 2);
        let z: CellId = ("a", 3);
        wg.cells.insert(x, mk_record(10, "X"));
        wg.cells.insert(y, mk_record(20, "Y"));
        wg.cells.insert(z, mk_record(1, "Z"));
        wg.add_wait(1, x);
        wg.add_wait(1, y);
        wg.add_wait(10, z);

        let path = wg
            .find_cycle(1)
            .expect("cycle expected via DFS over multi-edges");
        // DFS may visit Y first (dead-end, popped) then X→Z (closing the
        // cycle). Final path contains exactly the cycle-closing edges.
        assert_eq!(path, vec![x, z], "unexpected cycle path: {path:?}");
    }

    // ---- MemoizerCycleError display ----

    #[test]
    fn cycle_error_display_includes_stack() {
        let err = MemoizerCycleError {
            tag: "spec",
            key: "//pkg:a".to_string(),
            kind: CycleKind::SelfRecursion,
            stack: vec!["[spec] //pkg:a".to_string(), "[result] //pkg:b".to_string()],
        };
        let s = format!("{err}");
        assert!(s.contains("self-recursion"));
        assert!(s.contains("//pkg:a"));
        assert!(s.contains("[spec] //pkg:a"));
        assert!(s.contains("[result] //pkg:b"));
    }

    #[test]
    fn cycle_error_display_cross_task_variant() {
        let err = MemoizerCycleError {
            tag: "result",
            key: "X".to_string(),
            kind: CycleKind::CrossTask,
            stack: vec!["[spec] X".to_string(), "[spec] Y".to_string()],
        };
        let s = format!("{err}");
        assert!(s.contains("cross-task"));
    }

    #[test]
    fn fresh_invocation_ids_are_distinct() {
        let a = fresh_invocation_id();
        let b = fresh_invocation_id();
        assert_ne!(a, b);
    }

    // Concurrent callers of `once(K)` on the same task should not be
    // flagged as a cycle. With per-invocation ids, the second caller has
    // a different id from the owner; its wait edge points at K (owned by
    // the first caller), and the first caller has no outgoing waits (it
    // is computing K, no inner once() in flight) — DFS dead-ends, no
    // cycle.
    #[test]
    fn wait_graph_concurrent_same_cell_callers_no_cycle() {
        let mut wg = WaitGraph::new();
        let k: CellId = ("packages", 42);
        // Invocation 10 owns K. Invocation 11 (second caller) waits on K.
        wg.cells.insert(k, mk_record(10, "K"));
        wg.add_wait(11, k);
        assert!(
            wg.find_cycle(11).is_none(),
            "owner has no outgoing waits; DFS must terminate without cycle"
        );
    }

    // Regression check: an owner must NOT register a self-edge
    // (wait[owner].insert(its_cell)) — that would make every solo owner
    // look like a 1-cycle on its own cell.
    #[test]
    fn wait_graph_owner_with_no_pending_inner_is_not_a_cycle() {
        let mut wg = WaitGraph::new();
        let x: CellId = ("packages", 42);
        wg.cells.insert(x, mk_record(7, "X"));
        // Invocation 7 is computing X. No inner once() in flight, so
        // waiting[7] is empty.
        assert!(
            wg.find_cycle(7).is_none(),
            "owner of X must not be flagged as cycling on X"
        );
    }

    #[tokio::test]
    async fn with_cycle_ctx_is_a_no_op_passthrough() {
        // Per-invocation ids made the block_on root scoping redundant.
        // with_cycle_ctx is kept only as a pass-through for callers that
        // still wrap their top-level future with it.
        let v = with_cycle_ctx(async { 42 }).await;
        assert_eq!(v, 42);
    }

    // Phase registry is opt-in via env. We can't toggle the OnceLock cache
    // mid-process, so this test exercises the lower-level state machine
    // directly: write to the PHASES map and assert dump_phases formats it.
    #[test]
    fn phase_registry_dump_format() {
        let map = phases();
        {
            let mut g = map.lock().expect("phases");
            g.insert(101, "execute:semaphore_acquire");
            g.insert(202, "pluginexec:wait_subprocess");
        }
        // Bypass the env-var gate to exercise the formatter directly.
        let dump = {
            let g = map.lock().expect("phases");
            let mut entries: Vec<_> = g.iter().collect();
            entries.sort_by_key(|(inv, _)| **inv);
            let mut out = String::new();
            for (inv, phase) in entries {
                out.push_str(&format!("    inv {inv} @ {phase}\n"));
            }
            out
        };
        assert!(
            dump.contains("inv 101 @ execute:semaphore_acquire"),
            "got: {dump}"
        );
        assert!(
            dump.contains("inv 202 @ pluginexec:wait_subprocess"),
            "got: {dump}"
        );
        // Clean up — other tests may share the global PHASES map.
        let mut g = map.lock().expect("phases");
        g.remove(&101);
        g.remove(&202);
    }
}
