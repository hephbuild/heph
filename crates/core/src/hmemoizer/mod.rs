use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
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
// Opt in via `heph_PHASE_TRACE=1`. Disabled by default — `set_phase` and
// `clear_phase` are O(1) early-returns when the flag is off.

fn phase_trace_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| matches!(std::env::var("heph_PHASE_TRACE").as_deref(), Ok("1")))
}

static PHASES: OnceLock<Mutex<HashMap<u64, &'static str>>> = OnceLock::new();

fn phases() -> &'static Mutex<HashMap<u64, &'static str>> {
    PHASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Tag the calling invocation with its next-await `phase`. No-op when
/// `heph_PHASE_TRACE` is unset or when invoked outside any `once()` scope.
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

fn dump_phases() -> String {
    if !phase_trace_enabled() {
        return "  (phase trace disabled — set heph_PHASE_TRACE=1)".to_string();
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

pub struct Memoizer<K, V> {
    cache: Mutex<FxHashMap<K, Shared<BoxFuture<'static, V>>>>,
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
        Self {
            cache: Mutex::new(FxHashMap::default()),
            tag,
        }
    }

    /// Non-inserting peek: returns the memoized value only if it is already
    /// *completed* (not in-flight, not absent). Lets a caller take a cheap path
    /// on a cache hit (e.g. registering a dep edge with `note_dep` instead of a
    /// full `result`) without disturbing the cache or deduping with in-flight work.
    pub fn peek(&self, key: &K) -> Option<V> {
        let cache = self.cache.lock().expect("memoizer lock poisoned");
        cache.get(key).and_then(|shared| shared.peek().cloned())
    }

    pub async fn process<F, Fut>(&self, key: K, f: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = V> + Send + 'static,
    {
        // Phase 1: fast path — check without creating the future.
        // Block ensures MutexGuard is dropped before any await.
        let existing = {
            let cache = self.cache.lock().expect("memoizer lock poisoned");
            cache.get(&key).cloned()
        };

        if let Some(shared) = existing {
            // Post-completion fast path: a Shared future that has already
            // resolved exposes its output via `peek`. Cloning that directly
            // skips the full `Shared::poll` path (waker registration, inner
            // Mutex acquire) and the `take_or_clone_output` profile hotspot.
            if let Some(v) = shared.peek() {
                return v.clone();
            }
            return await_with_stall_check(shared, &key, self.tag).await;
        }

        // Phase 2: create candidate, then re-lock to insert.
        // Another caller may have inserted between phase 1 and phase 2;
        // entry().or_insert_with() handles that race correctly.
        let candidate = f().boxed().shared();
        let shared = {
            let mut cache = self.cache.lock().expect("memoizer lock poisoned");
            cache
                .entry(key.clone())
                .or_insert_with(|| candidate)
                .clone()
        };

        await_with_stall_check(shared, &key, self.tag).await
    }
}

async fn await_with_stall_check<V, K>(
    shared: Shared<BoxFuture<'static, V>>,
    key: &K,
    tag: &'static str,
) -> V
where
    V: Clone,
    K: fmt::Debug,
{
    let Some(threshold) = stall_threshold() else {
        return shared.await;
    };
    match tokio::time::timeout(threshold, shared).await {
        Ok(v) => v,
        Err(_) => {
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
fn dump_wait_graph() -> String {
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
            return self
                .process(key, || async move { f().await.map_err(Arc::new) })
                .await;
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

        let result = IN_FLIGHT
            .scope(
                frame,
                self.process(key, || async move { f().await.map_err(Arc::new) }),
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
            let mut cache = self.cache.lock().expect("memoizer lock poisoned");
            cache.remove(&key_for_evict);
        }
        result
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
