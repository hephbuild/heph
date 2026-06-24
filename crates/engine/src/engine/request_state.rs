use crate::engine::Engine;
use crate::engine::error::{CycleError, TargetFailure};
use crate::engine::local_cache::CacheArtifact;
use crate::engine::meta::ResultMeta;
use crate::engine::provider::State;
use crate::engine::result::{ArtifactMeta, ExtendedTargetDef, LockedResolution, OutputMatcher};
use crate::engine::spec::EngineTargetSpec;
use hcore::hasync::StdCancellationToken;
use hcore::hmemoizer::Memoizer;
use hmodel::htaddr::{Addr, AddrInner};
use hmodel::htpkg::PkgBuf;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashSet;
use std::ops::Deref;
use std::sync::{Arc, Weak};

type ArcErr = Arc<anyhow::Error>;
type ExecuteCacheResult = Result<(Vec<CacheArtifact>, Vec<ArtifactMeta>), ArcErr>;
type ProbeStatesResult = Result<Arc<Vec<State>>, ArcErr>;

/// Pointer-keyed map entry for `DepDag` nodes.
///
/// `Addr` is interned via the sharded table in `src/htaddr/addr.rs` — content-equal
/// `Addr`s share the same `Arc<AddrInner>`, so `Arc::ptr_eq` is the equality used by
/// `Addr::PartialEq`. The DAG is process-local and never persisted, so pointer
/// identity is the correct (and cheapest) key here. `Addr`'s public `Hash` impl is
/// content-based because it flows into disk cache keys — that path is untouched.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
struct AddrPtrKey(usize);

fn ptr_key(addr: &Addr) -> AddrPtrKey {
    let inner: &AddrInner = addr.deref();
    AddrPtrKey(inner as *const AddrInner as usize)
}

/// Online cycle-detecting DAG built with Pearce & Kelly's incremental topological
/// ordering (2006). Per-edge work is amortized O(δ), where δ is the size of the
/// "affected region" between the endpoints — typically 0 for forward edges (the
/// common case in a build DAG where the parent is inserted before its children).
///
/// Cycle errors are returned synchronously and surfaced as `CycleError` so the
/// engine can downcast them in `engine::query` to skip cycle-inducing providers.
#[derive(Debug, Default)]
pub struct DepDag {
    nodes: Vec<Addr>,
    succ: Vec<Vec<u32>>,
    pred: Vec<Vec<u32>>,
    ord: Vec<u32>,
    index_of: FxHashMap<AddrPtrKey, u32>,
}

impl DepDag {
    fn new() -> Self {
        Self::default()
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "all u32 indices are valid NodeIndex values produced by get_or_insert and bound to the vectors' lengths"
    )]
    pub fn add_dep(&mut self, from: &Addr, to: &Addr) -> Result<(), CycleError> {
        let f = self.get_or_insert(from);
        let t = self.get_or_insert(to);

        if f == t {
            return Err(CycleError {
                from: from.clone(),
                to: to.clone(),
            });
        }

        if self.succ[f as usize].contains(&t) {
            return Ok(());
        }

        let from_ord = self.ord[f as usize];
        let to_ord = self.ord[t as usize];

        if from_ord < to_ord {
            self.succ[f as usize].push(t);
            self.pred[t as usize].push(f);
            return Ok(());
        }

        // Topological violation: ord[from] >= ord[to]. Run PK reorder over the
        // affected region [to_ord, from_ord]. δ⁺ = forward reach from `to`; δ⁻ =
        // backward reach from `from`. Cycle iff δ⁺ touches `from`.
        let upper = from_ord;
        let lower = to_ord;

        let mut delta_plus: Vec<u32> = Vec::new();
        let mut delta_minus: Vec<u32> = Vec::new();
        let mut visited: FxHashSet<u32> = FxHashSet::default();

        visited.insert(t);
        delta_plus.push(t);
        let mut stack = vec![t];
        while let Some(n) = stack.pop() {
            for &w in &self.succ[n as usize] {
                if w == f {
                    return Err(CycleError {
                        from: from.clone(),
                        to: to.clone(),
                    });
                }
                if self.ord[w as usize] <= upper && visited.insert(w) {
                    delta_plus.push(w);
                    stack.push(w);
                }
            }
        }

        visited.clear();
        visited.insert(f);
        delta_minus.push(f);
        stack.push(f);
        while let Some(n) = stack.pop() {
            for &w in &self.pred[n as usize] {
                if self.ord[w as usize] >= lower && visited.insert(w) {
                    delta_minus.push(w);
                    stack.push(w);
                }
            }
        }

        delta_minus.sort_by_key(|&n| self.ord[n as usize]);
        delta_plus.sort_by_key(|&n| self.ord[n as usize]);

        let mut positions: Vec<u32> = Vec::with_capacity(delta_minus.len() + delta_plus.len());
        positions.extend(delta_minus.iter().map(|&n| self.ord[n as usize]));
        positions.extend(delta_plus.iter().map(|&n| self.ord[n as usize]));
        positions.sort_unstable();

        for (i, &n) in delta_minus.iter().enumerate() {
            self.ord[n as usize] = positions[i];
        }
        for (i, &n) in delta_plus.iter().enumerate() {
            self.ord[n as usize] = positions[delta_minus.len() + i];
        }

        self.succ[f as usize].push(t);
        self.pred[t as usize].push(f);
        Ok(())
    }

    fn get_or_insert(&mut self, addr: &Addr) -> u32 {
        let key = ptr_key(addr);
        if let Some(&idx) = self.index_of.get(&key) {
            return idx;
        }
        let idx = self.nodes.len() as u32;
        self.nodes.push(addr.clone());
        self.succ.push(Vec::new());
        self.pred.push(Vec::new());
        self.ord.push(idx);
        self.index_of.insert(key, idx);
        idx
    }
}

/// Shared mutable state for a request — common across all child RequestStates.
pub struct RequestStateData {
    pub engine: Weak<Engine>,
    pub request_id: String,
    pub ctoken: StdCancellationToken,
    pub dep_dag: Mutex<DepDag>,
    // Key includes `is_top`: top-level vs dependency resolution of the same
    // (addr, outputs) must not share a cell, because only the top-level frame
    // writes a codegen target's tree back / stores its fixpoint.
    pub mem_result:
        Memoizer<(Addr, OutputMatcher, bool), Result<Arc<crate::engine::result::EResult>, ArcErr>>,
    pub mem_execute_cache: Memoizer<(Addr, String), ExecuteCacheResult>,
    /// Single-flights the per-addr result-LOCK + cache-fetch/execute, keyed by
    /// `Addr` ALONE (not `is_top`/`outputs`). The `(outputs, is_top)`
    /// `mem_result` cells all await this, share its one riding read guard, then
    /// filter outputs on top. Keyed addr-only so two sibling computations of one
    /// addr can never both hold the non-reentrant per-addr lock — the
    /// self-deadlock this prevents.
    pub(crate) mem_locked_result: Memoizer<Addr, Result<Arc<LockedResolution>, ArcErr>>,
    pub mem_meta: Memoizer<Addr, Result<ResultMeta, ArcErr>>,
    pub mem_spec: Memoizer<Addr, Result<Arc<EngineTargetSpec>, ArcErr>>,
    pub mem_def: Memoizer<Addr, Result<Arc<ExtendedTargetDef>, ArcErr>>,
    pub mem_expanded_inputs:
        Memoizer<Addr, Result<Arc<Vec<crate::engine::driver::targetdef::Input>>, ArcErr>>,
    pub mem_packages: Memoizer<String, Result<Arc<Vec<String>>, ArcErr>>,
    /// Outer memoizer for `Engine::probe_segments`. Keyed by the target package;
    /// the cached value is the flat accumulation of every provider's probe across
    /// every parent package.
    pub mem_probe: Memoizer<PkgBuf, ProbeStatesResult>,
    /// Inner memoizer for `Engine::probe_segments`. Keyed by `(provider_name, pkg)`
    /// so a given provider's probe of a given package runs at most once per request.
    pub mem_probe_inner: Memoizer<(String, PkgBuf), ProbeStatesResult>,
    /// When false, fanout sites await every concurrent child instead of
    /// short-circuiting on the first error; errors are aggregated into a
    /// `MultiError`. Defaults to true (current behavior).
    pub fail_fast: bool,
    /// How many trailing lines of a failing target's process log to show in its
    /// diagnostic box (`heph run --log-lines`). The full log is always saved as
    /// the `log.txt` artifact; this only bounds the rendered tail.
    pub log_tail_lines: usize,
    /// Optional one-way build-progress event stream. Lives in the shared
    /// `Arc<RequestStateData>`, so `with_parent` / `with_skip_provider` children
    /// inherit it for free.
    pub events: Option<crate::engine::event::EventSender>,
    /// Build-event hooks registered on the engine, fanned out from [`emit`].
    /// Cloned from `engine.hooks()` once per top-level request; shared via the
    /// `Arc<RequestStateData>` so `with_parent`/`with_skip_provider` children
    /// inherit them. Usually empty (a no-op slice walk on the emit hot path).
    ///
    /// [`emit`]: RequestState::emit
    pub hooks: Vec<Arc<dyn crate::engine::hook::Hook>>,
    /// Guards the one-shot `MaxWorkers` announcement so it fires once per request
    /// regardless of which entry point (`result` / `result_addr`) is hit first.
    pub workers_announced: std::sync::atomic::AtomicBool,
    /// Guards the `Matched` stream so only the first/top-level `result` (or the
    /// single-addr entry in `run`) announces the matched set. Inner `result`
    /// invocations sharing this request's data must stay silent — re-emitting
    /// would inflate the client's matched denominator and prematurely flip its
    /// `complete` marker.
    pub matched_announced: std::sync::atomic::AtomicBool,
    /// Fire-and-forget sandbox cleanups enqueued by this request but not yet
    /// finished (queued + in-flight on the global cleaner thread). Carried into
    /// each cleanup job so the cleaner decrements it on completion; the shutdown
    /// path keeps the TUI open — and the process alive — until it drains to zero.
    pub bg_pending: crate::engine::sandbox_cleaner::PendingCounter,
    /// Per-request registry of genuinely-failing targets, keyed by addr. Populated
    /// by the `result_addr` classifier (first-writer-wins dedup) and drained once
    /// at the end of execution for rendering. Shared via `Arc<RequestStateData>`,
    /// so `with_parent` / `with_skip_provider` children record into the same map.
    pub failures: Mutex<indexmap::IndexMap<Addr, Arc<TargetFailure>>>,
}

/// One frame of the live resolution path (the breadcrumb chain). Built as an
/// `Arc` cons-list so `with_parent` forks it with a single allocation and child
/// subtrees share ancestors without copying. Used for cycle detection on the
/// *speculative* path (see [`RequestState::speculative`] / [`RequestState::track_dep`]).
struct Crumb {
    addr: Addr,
    parent: Option<Arc<Crumb>>,
}

/// Per-invocation state. Cheap to clone via with_parent — shares the same RequestStateData.
pub struct RequestState {
    pub data: Arc<RequestStateData>,
    /// The target that triggered this invocation, used for cycle detection in result_addr.
    pub parent: Option<Addr>,
    /// Provider names excluded from query iteration for this request subtree.
    pub skip_providers: Arc<HashSet<String>>,
    /// The ancestor chain of this invocation (parent first), mirroring the
    /// `with_parent` stack. Always maintained so a [`speculative`] fork can seed
    /// its cycle check from the real ancestors at the fork point.
    ///
    /// [`speculative`]: RequestState::speculative
    crumbs: Option<Arc<Crumb>>,
    /// When `true`, this subtree is a *speculative* inspection — a query
    /// resolving a candidate's spec/def only to evaluate its matcher, not as a
    /// real dependency. [`track_dep`] then checks the breadcrumb for ancestor
    /// reentry (so the query can skip cycle-inducing candidates) but does **not**
    /// commit edges to the shared [`DepDag`], which would otherwise retain a
    /// phantom dependency and close a false cycle later.
    ///
    /// [`track_dep`]: RequestState::track_dep
    speculative: bool,
}

impl RequestState {
    pub fn request_id(&self) -> &String {
        &self.data.request_id
    }

    pub fn ctoken(&self) -> &StdCancellationToken {
        &self.data.ctoken
    }

    pub fn fail_fast(&self) -> bool {
        self.data.fail_fast
    }

    /// Trailing process-log lines to render in a failure box (see
    /// [`RequestStateData::log_tail_lines`]).
    pub fn log_tail_lines(&self) -> usize {
        self.data.log_tail_lines
    }

    /// The request's in-flight sandbox-cleanup counter. Clone to hand to
    /// `sandbox_cleaner::enqueue`, or to the renderer so it can poll for drain.
    pub fn bg_pending(&self) -> crate::engine::sandbox_cleaner::PendingCounter {
        Arc::clone(&self.data.bg_pending)
    }

    /// Records a genuinely-failing target's rich diagnostic. First-writer-wins:
    /// if `addr` already has an entry (e.g. shared via the memoizer to multiple
    /// waiters), the existing one is kept.
    pub fn record_failure(&self, addr: Addr, failure: Arc<TargetFailure>) {
        self.data.failures.lock().entry(addr).or_insert(failure);
    }

    /// Drains and returns all recorded failures in insertion order, leaving the
    /// registry empty. Called once at the end of execution to render.
    pub fn take_failures(&self) -> Vec<Arc<TargetFailure>> {
        std::mem::take(&mut *self.data.failures.lock())
            .into_values()
            .collect()
    }

    /// Non-draining lookup of a single recorded failure by addr. Used by the
    /// outermost `result_addr` frame to surface the rich root-cause diagnostic
    /// to its direct caller in place of the `UpstreamFailed` marker.
    pub fn get_failure(&self, addr: &Addr) -> Option<Arc<TargetFailure>> {
        self.data.failures.lock().get(addr).cloned()
    }

    /// The first recorded failure in insertion order, if any. Fallback for
    /// boundary surfacing when the marker's named root wasn't itself recorded
    /// (e.g. a link-time resolution aggregation whose causes were recorded
    /// against the individual deps instead).
    pub fn first_failure(&self) -> Option<Arc<TargetFailure>> {
        self.data
            .failures
            .lock()
            .first()
            .map(|(_, v)| Arc::clone(v))
    }

    /// Stamp the server timestamp on `kind` and emit it on the event stream, if any.
    pub fn emit(&self, kind: crate::engine::event::BuildEventKind) {
        // Nobody downstream: skip building the event entirely (the common case
        // for non-`run` commands with no renderer and no hooks). Usage telemetry
        // is itself a registered hook (the built-in `TelemetryHook`, wired in
        // `bootstrap` when enabled), so it rides the fan-out below rather than a
        // dedicated call here — an opt-out leaves `hooks` empty and pays nothing.
        if self.data.events.is_none() && self.data.hooks.is_empty() {
            return;
        }
        let event = crate::engine::event::BuildEvent {
            at_unix_ms: crate::engine::event::now_unix_ms(),
            kind,
        };
        // Fan out to every registered hook (best-effort, sync push).
        for hook in &self.data.hooks {
            hook.on_event(&event);
        }
        if let Some(tx) = &self.data.events {
            // A closed receiver (consumer gone) is expected; events are
            // best-effort, so dropping the send result is intentional.
            drop(tx.send(event));
        }
    }

    /// Emit the `MaxWorkers` capacity event at most once per request. Safe to
    /// call from every top-level entry point (`result`, `result_addr`); only the
    /// first call emits, so dep recursion never re-announces.
    pub fn announce_max_workers(&self, count: usize) {
        if !self
            .data
            .workers_announced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.emit(crate::engine::event::BuildEventKind::MaxWorkers { count });
        }
    }

    /// Claims ownership of the `Matched` stream for the calling `result`
    /// invocation. Returns `true` exactly once per request (for the first/
    /// top-level call); every later call — including inner `result`s sharing
    /// this request's data — gets `false` and must not emit `Matched`.
    pub fn claim_matched_stream(&self) -> bool {
        !self
            .data
            .matched_announced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// Hands a cloned sender to `emit_scope`'s drop-guard.
    pub(crate) fn events_sender(&self) -> Option<crate::engine::event::EventSender> {
        self.data.events.clone()
    }

    /// Returns a child RequestState sharing the same data but with a new parent.
    pub fn with_parent(&self, parent: Addr) -> Arc<RequestState> {
        let crumbs = Some(Arc::new(Crumb {
            addr: parent.clone(),
            parent: self.crumbs.clone(),
        }));
        Arc::new(RequestState {
            data: Arc::clone(&self.data),
            parent: Some(parent),
            skip_providers: Arc::clone(&self.skip_providers),
            crumbs,
            speculative: self.speculative,
        })
    }

    /// Returns a child RequestState with the given provider name added to skip_providers.
    pub fn with_skip_provider(&self, name: &str) -> Arc<RequestState> {
        let mut set = (*self.skip_providers).clone();
        set.insert(name.to_string());
        Arc::new(RequestState {
            data: Arc::clone(&self.data),
            parent: self.parent.clone(),
            skip_providers: Arc::new(set),
            crumbs: self.crumbs.clone(),
            speculative: self.speculative,
        })
    }

    /// Forks this state into a *speculative* inspection subtree: same ancestors
    /// and parent, but resolutions under it check the breadcrumb for cycles
    /// instead of recording edges into the shared [`DepDag`]. Used by query
    /// matching, which `get_spec`/`get_def`s candidates only to evaluate the
    /// matcher — a non-matching candidate must leave no trace, or its phantom
    /// edge would later close a false cycle (see [`track_dep`]).
    ///
    /// [`track_dep`]: RequestState::track_dep
    pub fn speculative(&self) -> Arc<RequestState> {
        Arc::new(RequestState {
            data: Arc::clone(&self.data),
            parent: self.parent.clone(),
            skip_providers: Arc::clone(&self.skip_providers),
            crumbs: self.crumbs.clone(),
            speculative: true,
        })
    }

    /// Record that the current `parent` depends on `addr`, returning a
    /// [`CycleError`] if that closes a cycle.
    ///
    /// Real (non-speculative) resolution commits `parent → addr` to the shared
    /// [`DepDag`] — the always-on graph that catches cross-task cycles before the
    /// memoizer would deadlock. A speculative inspection instead walks the
    /// breadcrumb chain: if `addr` is already an ancestor it's a cycle (skip the
    /// candidate), otherwise it proceeds without touching the shared graph, so a
    /// rejected candidate never pollutes it.
    pub fn track_dep(&self, addr: &Addr) -> Result<(), CycleError> {
        if self.speculative {
            let mut cur = self.crumbs.as_deref();
            while let Some(crumb) = cur {
                if crumb.addr == *addr {
                    return Err(CycleError {
                        from: crumb.addr.clone(),
                        to: addr.clone(),
                    });
                }
                cur = crumb.parent.as_deref();
            }
            Ok(())
        } else if let Some(parent) = &self.parent {
            self.data.dep_dag.lock().add_dep(parent, addr)
        } else {
            Ok(())
        }
    }
}

impl Drop for RequestStateData {
    fn drop(&mut self) {
        self.ctoken.cancel();
        // Signal end-of-stream to each hook so it can flush its final state. The
        // host awaits the actual flush separately via `Engine::await_hooks`.
        for hook in &self.hooks {
            hook.on_close();
        }
        if let Some(engine) = self.engine.upgrade()
            && let Ok(mut requests) = engine.requests.lock()
        {
            requests.remove(&self.request_id);
        }
    }
}

impl Engine {
    pub fn new_state(self: &Arc<Self>) -> Arc<RequestState> {
        self.new_state_with_fail_fast(true)
    }

    /// Default number of trailing process-log lines shown in a failure box when a
    /// caller (e.g. `gc`, non-`run` commands) does not override it.
    pub const DEFAULT_LOG_TAIL_LINES: usize = 10;

    pub fn new_state_with_fail_fast(self: &Arc<Self>, fail_fast: bool) -> Arc<RequestState> {
        self.new_state_with_events(fail_fast, None)
    }

    pub fn new_state_with_events(
        self: &Arc<Self>,
        fail_fast: bool,
        events: Option<crate::engine::event::EventSender>,
    ) -> Arc<RequestState> {
        self.new_state_full(
            fail_fast,
            events,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Self::DEFAULT_LOG_TAIL_LINES,
        )
    }

    /// Like [`new_state_with_events`] but with a caller-supplied background-work
    /// counter, so the renderer that owns the other clone can watch this
    /// request's sandbox cleanups drain during shutdown.
    pub fn new_state_full(
        self: &Arc<Self>,
        fail_fast: bool,
        events: Option<crate::engine::event::EventSender>,
        bg_pending: crate::engine::sandbox_cleaner::PendingCounter,
        log_tail_lines: usize,
    ) -> Arc<RequestState> {
        // Unique per top-level request. `with_parent`/`with_skip_provider`
        // children share this `RequestStateData` (and thus this id), so a request
        // subtree keys into one bucket of any per-request cache (e.g. pluginfs's
        // exclude-`Any` cache, pruned in `Drop`).
        static NEXT_REQUEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request_id = format!("req-{n}");
        let data = Arc::new(RequestStateData {
            engine: Arc::downgrade(self),
            request_id: request_id.clone(),
            ctoken: StdCancellationToken::new(),
            dep_dag: Mutex::new(DepDag::new()),
            mem_execute_cache: Memoizer::with_tag("execute_cache"),
            mem_locked_result: Memoizer::with_tag("locked_result"),
            mem_result: Memoizer::with_tag("result"),
            mem_meta: Memoizer::with_tag("meta"),
            mem_spec: Memoizer::with_tag("spec"),
            mem_def: Memoizer::with_tag("def"),
            mem_expanded_inputs: Memoizer::with_tag("expanded_inputs"),
            mem_packages: Memoizer::with_tag("packages"),
            mem_probe: Memoizer::with_tag("probe"),
            mem_probe_inner: Memoizer::with_tag("probe_inner"),
            fail_fast,
            log_tail_lines,
            events,
            hooks: self.hooks(),
            workers_announced: std::sync::atomic::AtomicBool::new(false),
            matched_announced: std::sync::atomic::AtomicBool::new(false),
            bg_pending,
            failures: Mutex::new(indexmap::IndexMap::new()),
        });

        let state = Arc::new(RequestState {
            data: Arc::clone(&data),
            parent: None,
            skip_providers: Arc::new(HashSet::new()),
            crumbs: None,
            speculative: false,
        });

        if let Ok(mut requests) = self.requests.lock() {
            requests.insert(request_id, Arc::downgrade(&state));
        }

        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Config;
    use hmodel::htpkg::PkgBuf;
    use std::path::PathBuf;

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("pkg"), name.to_string(), Default::default())
    }

    /// Build an `Engine` rooted at a unique temp dir so the sqlite cache db
    /// never collides across parallel tests (a shared path locks the db).
    /// The returned `TempDir` must be held alive for the test's duration.
    fn test_engine() -> anyhow::Result<(tempfile::TempDir, Arc<Engine>)> {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = Arc::new(Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?);
        Ok((dir, engine))
    }

    #[test]
    fn test_dep_dag_acyclic() {
        let mut dag = DepDag::new();
        assert!(dag.add_dep(&addr("a"), &addr("b")).is_ok());
        assert!(dag.add_dep(&addr("b"), &addr("c")).is_ok());
    }

    #[test]
    fn test_dep_dag_direct_cycle() {
        let mut dag = DepDag::new();
        assert!(dag.add_dep(&addr("a"), &addr("b")).is_ok());
        assert!(dag.add_dep(&addr("b"), &addr("a")).is_err());
    }

    #[test]
    fn test_dep_dag_indirect_cycle() {
        let mut dag = DepDag::new();
        assert!(dag.add_dep(&addr("a"), &addr("b")).is_ok());
        assert!(dag.add_dep(&addr("b"), &addr("c")).is_ok());
        assert!(dag.add_dep(&addr("c"), &addr("a")).is_err());
    }

    #[test]
    fn test_dep_dag_self_loop() {
        let mut dag = DepDag::new();
        let a = addr("a");
        assert!(dag.add_dep(&a, &a).is_err());
    }

    #[test]
    fn test_dep_dag_duplicate_edge_idempotent() {
        let mut dag = DepDag::new();
        let a = addr("a");
        let b = addr("b");
        assert!(dag.add_dep(&a, &b).is_ok());
        assert!(dag.add_dep(&a, &b).is_ok());
    }

    #[test]
    fn test_dep_dag_pk_reorder() {
        // Insert a→c, b→c first (so c gets a low ord relative to a/b in insertion
        // order: a=0, c=1, b=2). Then a→b is a back-edge in initial ord (ord[a]=0,
        // ord[b]=2 — wait, forward).
        //
        // Reverse the pattern: insert a→c, then b→c, then b→a forces a reorder
        // because b was inserted after a but now must precede it.
        let mut dag = DepDag::new();
        let a = addr("a");
        let b = addr("b");
        let c = addr("c");
        assert!(dag.add_dep(&a, &c).is_ok());
        assert!(dag.add_dep(&b, &c).is_ok());
        assert!(dag.add_dep(&b, &a).is_ok());

        let ai = *dag.index_of.get(&ptr_key(&a)).unwrap();
        let bi = *dag.index_of.get(&ptr_key(&b)).unwrap();
        let ci = *dag.index_of.get(&ptr_key(&c)).unwrap();
        assert!(dag.ord[bi as usize] < dag.ord[ai as usize]);
        assert!(dag.ord[ai as usize] < dag.ord[ci as usize]);
        assert!(dag.ord[bi as usize] < dag.ord[ci as usize]);
    }

    #[test]
    fn test_dep_dag_concurrent_stress() {
        // 64 threads each adding 100 acyclic edges, plus one closing edge from
        // a designated thread. Assert that the closing edge surfaces exactly one
        // CycleError and all other inserts succeed.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dag = Arc::new(Mutex::new(DepDag::new()));
        // Seed a chain a0 → a1 → ... → a9 so the closing edge a9 → a0 is a cycle.
        {
            let mut g = dag.lock();
            for i in 0..9 {
                g.add_dep(&addr(&format!("a{i}")), &addr(&format!("a{}", i + 1)))
                    .unwrap();
            }
        }
        let closing_attempts = AtomicUsize::new(0);
        let closing_attempts = Arc::new(closing_attempts);
        let ok_count = Arc::new(AtomicUsize::new(0));
        let err_count = Arc::new(AtomicUsize::new(0));

        let threads: Vec<_> = (0..64)
            .map(|tid| {
                let dag = Arc::clone(&dag);
                let closing_attempts = Arc::clone(&closing_attempts);
                let ok_count = Arc::clone(&ok_count);
                let err_count = Arc::clone(&err_count);
                std::thread::spawn(move || {
                    for i in 0..100 {
                        let from = addr(&format!("t{tid}_{i}"));
                        let to = addr(&format!("t{tid}_{}", i + 1));
                        let res = dag.lock().add_dep(&from, &to);
                        assert!(res.is_ok());
                    }
                    // One thread tries to close the seed chain into a cycle.
                    if tid == 0 {
                        closing_attempts.fetch_add(1, Ordering::SeqCst);
                        let res = dag.lock().add_dep(&addr("a9"), &addr("a0"));
                        match res {
                            Ok(_) => ok_count.fetch_add(1, Ordering::SeqCst),
                            Err(_) => err_count.fetch_add(1, Ordering::SeqCst),
                        };
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(closing_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(ok_count.load(Ordering::SeqCst), 0);
        assert_eq!(err_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_request_state_tracking() -> anyhow::Result<()> {
        let (_tmp, engine) = test_engine()?;

        let rs = engine.new_state();
        let request_id = rs.request_id().to_string();

        {
            let requests = engine.requests.lock().unwrap();
            assert!(requests.contains_key(&request_id));
            let weak = requests.get(&request_id).unwrap();
            assert!(weak.upgrade().is_some());
        }

        drop(rs);

        {
            let requests = engine.requests.lock().unwrap();
            assert!(!requests.contains_key(&request_id));
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_cancel_all_requests_cancels_live_tokens() -> anyhow::Result<()> {
        let (_tmp, engine) = test_engine()?;

        let rs = engine.new_state();
        assert!(!rs.ctoken().is_cancelled());

        engine.cancel_all_requests();
        assert!(rs.ctoken().is_cancelled());

        // Idempotent — second call must not panic or change state.
        engine.cancel_all_requests();
        assert!(rs.ctoken().is_cancelled());

        Ok(())
    }

    #[tokio::test]
    async fn fail_fast_defaults_true_overridable() -> anyhow::Result<()> {
        let (_tmp, engine) = test_engine()?;

        assert!(engine.new_state().fail_fast());
        assert!(engine.new_state_with_fail_fast(true).fail_fast());
        assert!(!engine.new_state_with_fail_fast(false).fail_fast());
        Ok(())
    }

    #[tokio::test]
    async fn test_skip_provider_child_does_not_cancel_token() -> anyhow::Result<()> {
        let (_tmp, engine) = test_engine()?;

        let rs = engine.new_state();
        assert!(!rs.ctoken().is_cancelled());

        {
            let child = rs.with_skip_provider("some_provider");
            assert!(!child.ctoken().is_cancelled());
        } // child drops here

        assert!(
            !rs.ctoken().is_cancelled(),
            "child drop must not cancel parent token"
        );

        Ok(())
    }

    // A registered hook is fed every emitted event (via the `emit` chokepoint,
    // even with no renderer channel) and gets `on_close` when the request state
    // drops.
    #[test]
    fn emit_fans_out_to_hooks_and_closes_on_drop() -> anyhow::Result<()> {
        use crate::engine::hook::Hook;
        use hcore::events::{BuildEvent, BuildEventKind};
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Default)]
        struct Rec {
            seen: Mutex<Vec<String>>,
            closed: AtomicBool,
        }
        impl Hook for Rec {
            fn name(&self) -> String {
                "rec".into()
            }
            fn on_event(&self, ev: &BuildEvent) {
                if let BuildEventKind::ResultStart { addr } = &ev.kind {
                    self.seen.lock().push(addr.clone());
                }
            }
            fn on_close(&self) {
                self.closed.store(true, Ordering::Release);
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let mut e = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let rec = Arc::new(Rec::default());
        e.register_hook(Arc::clone(&rec) as Arc<dyn Hook>)?;
        let engine = Arc::new(e);

        // No renderer channel (events = None); the hook still receives events.
        let state = engine.new_state_with_events(true, None);
        state.emit(BuildEventKind::ResultStart {
            addr: "//a:b".into(),
        });
        assert_eq!(rec.seen.lock().clone(), vec!["//a:b".to_string()]);
        assert!(
            !rec.closed.load(Ordering::Acquire),
            "not closed mid-request"
        );

        drop(state);
        assert!(
            rec.closed.load(Ordering::Acquire),
            "on_close fires when the request state drops"
        );
        Ok(())
    }
}
