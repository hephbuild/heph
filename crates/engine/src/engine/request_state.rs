use crate::engine::Engine;
use crate::engine::error::{CycleError, TargetFailure};
use crate::engine::meta::ResultMeta;
use crate::engine::provider::State;
use crate::engine::result::{
    ArtifactMeta, ExtendedTargetDef, LockedResolution, OutputMatcher, ResultArtifact,
};
use crate::engine::spec::EngineTargetSpec;
use hcore::hasync::StdCancellationToken;
use hcore::hmemoizer::Memoizer;
use hmodel::htaddr::{Addr, AddrInner};
use hmodel::htpkg::PkgBuf;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashSet;
use std::ops::Deref;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

type ArcErr = Arc<anyhow::Error>;
type ExecuteCacheResult = Result<(Vec<ResultArtifact>, Vec<ArtifactMeta>), ArcErr>;
type ProbeStatesResult = Result<Arc<Vec<State>>, ArcErr>;

/// Process-local memoizer key for an `Addr`.
///
/// `Addr` is interned, so `Arc::ptr_eq` is already what `Addr::PartialEq`
/// does — but `Addr::Hash` walks package + name + every arg, because that same
/// impl feeds driver def hashes and therefore the persisted cache key (see the
/// comment on `impl Hash for Addr` in `htaddr/addr.rs`). Memoizer maps are
/// per-request and never persisted, so they can hash the pointer instead.
///
/// This is not a behaviour change, it is the removal of redundant work: a map
/// that compares by pointer and hashes by content already treats two
/// ptr-distinct, content-equal `Addr`s as two entries (they collide in a bucket
/// and then compare unequal). Hashing the pointer puts them in different
/// buckets and reaches the same two entries. Every lookup outcome is identical;
/// only the cost differs — measured 7.1x cheaper per lookup (10.2 ns) on
/// realistic addresses.
///
/// `Debug` still renders the `Addr`, so the SIGQUIT memoizer inventory and the
/// stall diagnostics keep naming targets rather than pointers.
#[derive(Clone)]
pub struct AddrKey(pub Addr);

impl From<Addr> for AddrKey {
    #[inline]
    fn from(addr: Addr) -> Self {
        Self(addr)
    }
}

impl PartialEq for AddrKey {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        // `Addr::PartialEq` is already `Arc::ptr_eq`.
        self.0 == other.0
    }
}

impl Eq for AddrKey {}

impl std::hash::Hash for AddrKey {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let inner: &AddrInner = self.0.deref();
        (inner as *const AddrInner as usize).hash(state);
    }
}

impl std::fmt::Debug for AddrKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

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

/// Pack a directed edge into one key for [`DepDag::edges`]. Node indices are
/// `u32`, so the two halves never collide.
const fn edge_key(from: u32, to: u32) -> u64 {
    ((from as u64) << 32) | to as u64
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
    /// Membership index over the same edges held in `succ`/`pred`, used *only*
    /// for the already-present short-circuit in [`DepDag::add_dep`].
    ///
    /// `succ`/`pred` stay dense `Vec`s because the Pearce-Kelly reorder scans
    /// whole adjacency lists — a hash set there would trade a sequential read
    /// for pointer chasing. What the `Vec`s are bad at is the membership test:
    /// `succ[f].contains(&t)` is a linear scan under the `DepDag` mutex, and it
    /// scans the *whole* list on a miss, so filling a node to out-degree D cost
    /// Θ(D²/2) even with no repeats. Repeats then pay D per offer, and they are
    /// the common case rather than an oddity: a transparent group's re-inline
    /// (`result.rs`, before `mem_result.once`) re-walks all D of the group's
    /// deps once per parent that reaches it, so K parents made the old check
    /// Θ(K·D²) — 250 ms at D=100k.
    ///
    /// Two invariants, both load-bearing:
    ///
    /// - An edge is inserted here only *after* `add_dep` has committed it,
    ///   never before. Recording an edge that the cycle check then rejects
    ///   would make the next identical `add_dep` short-circuit to `Ok`,
    ///   silently accepting the cycle.
    /// - The index and the adjacency lists are written together, by
    ///   [`DepDag::commit_edge`] alone. There is no retraction path today
    ///   (`RequestState::speculative` exists precisely so a rejected candidate
    ///   never records an edge that would have to be taken back); if one is
    ///   ever added it must remove from both sides, or a live edge becomes
    ///   invisible to the cycle check.
    ///
    /// Keys are packed `(from, to)` rather than a tuple so the hash is one
    /// `FxHasher` round instead of two, at identical size.
    edges: FxHashSet<u64>,
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

        if self.edges.contains(&edge_key(f, t)) {
            return Ok(());
        }

        let from_ord = self.ord[f as usize];
        let to_ord = self.ord[t as usize];

        if from_ord < to_ord {
            self.commit_edge(f, t);
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

        self.commit_edge(f, t);
        Ok(())
    }

    /// Append `f → t` to the adjacency lists and record it in the membership
    /// index. The single writer of [`DepDag::edges`], so the index cannot drift
    /// from `succ`/`pred`. Both call sites are in `add_dep`, after every
    /// rejection path — see the note on [`DepDag::edges`].
    #[expect(
        clippy::indexing_slicing,
        reason = "callers pass node indices already bound to the vectors' lengths by get_or_insert"
    )]
    fn commit_edge(&mut self, f: u32, t: u32) {
        self.succ[f as usize].push(t);
        self.pred[t as usize].push(f);
        self.edges.insert(edge_key(f, t));
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
    /// Speculative `RequestState`s alive on this request — see
    /// [`RequestState::speculative_live`].
    speculative_live: std::sync::atomic::AtomicUsize,
    // Key includes `is_top`: top-level vs dependency resolution of the same
    // (addr, outputs) must not share a cell, because only the top-level frame
    // rewrites an `in_place` codegen target's own sources / stores its fixpoint.
    // (A `copy` target's tree write-back is is_top-independent and single-flights
    // on `mem_codegen_copy` instead.)
    pub mem_result: Memoizer<
        (AddrKey, OutputMatcher, bool),
        Result<Arc<crate::engine::result::EResult>, ArcErr>,
    >,
    pub mem_execute_cache: Memoizer<(AddrKey, String), ExecuteCacheResult>,
    /// Single-flights the per-addr result-LOCK + cache-fetch/execute, keyed by
    /// `Addr` ALONE (not `is_top`/`outputs`). The `(outputs, is_top)`
    /// `mem_result` cells all await this, share its one riding read guard, then
    /// filter outputs on top. Keyed addr-only so two sibling computations of one
    /// addr can never both hold the non-reentrant per-addr lock — the
    /// self-deadlock this prevents.
    pub(crate) mem_locked_result: Memoizer<AddrKey, Result<Arc<LockedResolution>, ArcErr>>,
    /// Single-flights the `codegen = "copy"` tree write-back, keyed by `Addr`
    /// alone. Every frame that resolves the target offers it — the top-level one,
    /// each dependent that reads an output group, and the `meta` walk that only
    /// hashes it — and exactly one performs it, so the tree is written once per
    /// addr per request no matter how many ways the target was reached.
    pub(crate) mem_codegen_copy: Memoizer<AddrKey, Result<(), ArcErr>>,
    /// Single-flights the lazy pull of one remote blob, keyed by
    /// `(addr, hashin, blob name)`. Two `outputs` cells of the same addr both need
    /// its support files, so without this they would download and write the same
    /// blob concurrently — duplicate transfer, and two writers racing on one cache
    /// key.
    pub(crate) mem_remote_blob: Memoizer<(AddrKey, String, String), Result<bool, ArcErr>>,
    pub mem_meta: Memoizer<AddrKey, Result<ResultMeta, ArcErr>>,
    pub mem_spec: Memoizer<AddrKey, Result<Arc<EngineTargetSpec>, ArcErr>>,
    pub mem_def: Memoizer<AddrKey, Result<Arc<ExtendedTargetDef>, ArcErr>>,
    pub mem_expanded_inputs:
        Memoizer<AddrKey, Result<Arc<Vec<crate::engine::driver::targetdef::Input>>, ArcErr>>,
    pub mem_packages: Memoizer<String, Result<Arc<Vec<String>>, ArcErr>>,
    /// Outer memoizer for `Engine::probe_segments`. Keyed by the target package;
    /// the cached value is the flat accumulation of every provider's probe across
    /// every parent package.
    pub mem_probe: Memoizer<PkgBuf, ProbeStatesResult>,
    /// Inner memoizer for `Engine::probe_segments`. Keyed by `(provider_name, pkg)`
    /// so a given provider's probe of a given package runs at most once per request.
    pub mem_probe_inner: Memoizer<(String, PkgBuf), ProbeStatesResult>,
    /// Memoizer for `ProviderExecutor::states_under`. Keyed by the subtree prefix,
    /// so the (potentially whole-workspace) package walk + probes for a given
    /// prefix run at most once per request — a `list` that calls it repeatedly
    /// (e.g. the go go_src query listing many packages) pays the walk once, not
    /// once per package.
    pub mem_states_under: Memoizer<PkgBuf, ProbeStatesResult>,
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
    /// `--frozen`: this run verifies the codegen tree instead of writing it.
    ///
    /// Request-scoped, not per-call, because the `copy` write-back fires on
    /// dependency frames too and those resolve with `ResultOptions::default()` —
    /// the flag on `ResultOptions` reaches only the frame the user's options were
    /// built for. Stamped by that frame (the top-level one) before it resolves any
    /// dependency, so every frame beneath it reads the run's real mode.
    pub(crate) frozen: std::sync::atomic::AtomicBool,
    /// Guards the one-shot `RequestConfig` announcement so it fires once per request
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
    ///
    /// **Wait on this only after releasing the request.** A request's last piece
    /// of background work — its [`DeferredTrims`] batch — is submitted *by*
    /// `RequestStateData`'s drop, so a waiter that keeps its
    /// `Arc<RequestState>` alive can see zero and conclude the run is finished
    /// while a trim is still owed. `heph run` unwinds in the right order
    /// already: the drain loops in `tui::backend` run after the app future,
    /// which owns the request, has returned.
    ///
    /// The counter deliberately never counts work that has not been handed to
    /// the cleaner yet — see [`sandbox_cleaner::enqueue`] for why a
    /// reserve-now/submit-later split would make a pinned request an unexitable
    /// process rather than a leak.
    ///
    /// [`sandbox_cleaner::enqueue`]: crate::engine::sandbox_cleaner::enqueue
    pub bg_pending: crate::engine::sandbox_cleaner::PendingCounter,
    /// Per-request registry of genuinely-failing targets, keyed by addr. Populated
    /// by the `result_addr` classifier (first-writer-wins dedup) and drained once
    /// at the end of execution for rendering. Shared via `Arc<RequestStateData>`,
    /// so `with_parent` / `with_skip_provider` children record into the same map.
    pub failures: Mutex<indexmap::IndexMap<Addr, Arc<TargetFailure>>>,
    /// Decision maker for `approval`-gated targets. `None` in non-interactive
    /// contexts that never set one (gc, tests) — a target requiring approval then
    /// fails with a clear error. Shared via `Arc<RequestStateData>`, so child
    /// states inherit it.
    pub approval: Option<Arc<dyn crate::engine::approval::ApprovalHandler>>,
    /// This request may hash, probe and read, but must never take the exclusive
    /// per-addr result lock.
    ///
    /// Set only by [`Engine::new_hash_only_state`], which is used to re-read the
    /// tree from *inside* an in-flight resolution — the in_place write-back guard
    /// and the fixpoint recompute. Those nested requests share the engine's one
    /// `ResultLock` with the request they are nested inside, but not its
    /// `mem_locked_result` — which is the memoizer that makes per-addr
    /// acquisition idempotent within a request. So a nested write acquire
    /// contends its own outer request's riding read guard, which is held until
    /// the nested call returns: a self-deadlock, not contention.
    ///
    /// A cacheable miss therefore answers [`HashUnknownError`] instead of
    /// building. Targets that take no lock at all (`@heph/fs`, see
    /// `resolve_locked_inner`'s `skip_lock`) still execute normally — re-reading
    /// the tree is the entire point of these requests.
    pub hash_only: bool,
    /// Post-write `cache.history` trims held back until this request's cache
    /// read guards are gone.
    ///
    /// **Must stay the last field.** See [`DeferredTrims`] — being last is what
    /// makes the drop order correct, not a stylistic choice.
    deferred_trims: DeferredTrims,
}

/// One deferred post-write trim: how many revisions of a target to keep, and
/// the revision this request wrote (which the trim must never delete).
#[derive(Debug)]
struct PendingTrim {
    keep: u32,
    hashin: String,
}

/// The request's post-write `cache.history` trims, submitted once the request
/// state — and every cache read guard it holds — is gone.
///
/// Trimming deletes revisions, so it needs the target's **write** lock; the
/// lock is per-addr, not per-revision, and `gc.rs` documents why a read is not
/// enough. Running the trim inline, where `cache_locally` finishes, therefore
/// could never succeed: the addr's riding read guard lives in
/// [`RequestStateData::mem_locked_result`] and is cloned into every artifact
/// handed out, and the memoizer only evicts on a cycle error. The read is held
/// for the whole request, so the trim's non-blocking `try_write` always lost
/// and `cache.history` was silently never enforced *during* a run — only by the
/// next `heph gc`.
///
/// So the decision is recorded here as the revision is written and executed
/// afterwards, on the background cleaner lane.
///
/// **This must stay the last field of [`RequestStateData`].** Rust drops a
/// struct's fields in declaration order *after* its own `Drop::drop` returns,
/// so being last is exactly what guarantees every memoizer — and every riding
/// read guard it holds — has already dropped by the time this one's `Drop`
/// submits the trims. Moving it earlier reinstates the bug silently.
struct DeferredTrims {
    engine: Weak<Engine>,
    bg_pending: crate::engine::sandbox_cleaner::PendingCounter,
    /// Keyed by addr. At most one entry per addr can be recorded anyway —
    /// `execute_and_cache_inner` runs inside the addr-keyed `mem_locked_result`
    /// cell, so one request writes at most one revision per addr — but the map
    /// makes that structural rather than assumed.
    ///
    /// The one case that *does* file two revisions under one addr, the in_place
    /// fixpoint (`maybe_store_fixpoint` → `duplicate_cache_revision`), never
    /// reaches here. It survives the trim on its own terms: the duplicate is
    /// stamped strictly newer than the primary, and an in_place def's
    /// `cache.history` is doubled. Both are documented at their own sites; if
    /// either changes, this is one of the places that finds out.
    trims: Mutex<FxHashMap<Addr, PendingTrim>>,
    /// Set by the `heph gc` sweep for its own phase-1 resolution state. That
    /// sweep *is* the authoritative trim: letting phase 1 also submit background
    /// trims would race phase 2's write locks, make `GcStats` report whichever
    /// of the two got there first, and surface a contended-lock notice naming
    /// this very process as the holder.
    suppressed: std::sync::atomic::AtomicBool,
    /// The batch's retry delay, in nanoseconds.
    ///
    /// A field rather than the constant read straight from `gc` so a test can
    /// widen it. `the_exit_gate_is_held_across_the_trim_retry` is the only thing
    /// pinning that the retry runs *inside* the cleaner job — i.e. under the
    /// counter that gates process exit — and it proves it by timing the drain.
    /// Against the 25ms production value that check is one loaded runner away
    /// from passing for the wrong reason; against a second it is unambiguous.
    retry_delay_nanos: std::sync::atomic::AtomicU64,
}

impl DeferredTrims {
    fn push(&self, addr: &Addr, keep: u32, hashin: String) {
        if self.suppressed.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.trims
            .lock()
            .insert(addr.clone(), PendingTrim { keep, hashin });
    }
}

impl Drop for DeferredTrims {
    fn drop(&mut self) {
        let trims = std::mem::take(&mut *self.trims.lock());
        if trims.is_empty() {
            return;
        }
        let Some(engine) = self.engine.upgrade() else {
            tracing::debug!(
                trims = trims.len(),
                "engine dropped before its deferred cache-history trims ran"
            );
            return;
        };
        // The read guards the batch's trims contend with are unpinned by this
        // request's own teardown: `deferred_trims` is the last field of
        // `RequestStateData`, so by the time this drop runs, the request's
        // memoizers are gone and their abort cascades are in flight — each
        // tears down a chain whose `mem_locked_result` value *is* an addr's
        // riding cache read. The cascade lands when the runtime processes it,
        // which is why the batch below probes, re-probes, and retries once
        // rather than expecting the guards to be gone already. (The blocking
        // pool's backstop registry, which used to be flushed here because it
        // could retain those guards past their wait's end, no longer exists.)

        // One job for the batch: `try_trim_after_write` is non-blocking and
        // short, and a job per target would allocate a boxed closure per
        // written revision — which is exactly what this replaces. One job is
        // also what lets the batch charge its one wait once rather than per
        // target.
        //
        // Bookkeeping lane, never the reclaim one. This lands at request-state
        // drop, which is exactly when the reclaim backlog is deepest — every
        // sandbox the run finished with is still queued for `remove_dir_all`. On
        // a shared queue the batch would sit behind all of them, so
        // `cache.history` would only be enforced after the last 5k-50k-inode
        // removal, and process exit (gated on `bg_pending`, which both lanes
        // feed) would pay the rmdir drain *plus* the trim instead of the max of
        // the two. The retry's one sleep rides in the bookkeeping lane too, so it
        // never delays a reclaim.
        //
        // The retries run *inside* this job, so the `bg_pending` slot taken by
        // `enqueue` is still held across them and the process cannot exit
        // mid-retry. Moving them onto a thread of their own would release the
        // slot after the first pass and silently reintroduce that.
        let delay = Duration::from_nanos(
            self.retry_delay_nanos
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        crate::engine::sandbox_cleaner::enqueue(
            crate::engine::sandbox_cleaner::Lane::Bookkeeping,
            format!("gc trim {} target(s)", trims.len()),
            Box::new(move || {
                let report = engine.run_trim_batch_with_delay(
                    trims.into_iter().map(|(addr, t)| (addr, t.keep, t.hashin)),
                    delay,
                );
                // Say what the batch did. A drain that reports nothing cannot be
                // told from a drain that enforced nothing, which is the shape the
                // original bug hid in.
                tracing::debug!(
                    batch = report.batch,
                    retried = report.retried,
                    delayed = report.delayed,
                    still_contended = report.still_contended,
                    failed = report.failed,
                    removed = report.removed,
                    bytes = report.bytes,
                    "deferred cache-history trims drained",
                );
                // Losing *every* target is not ordinary contention, it is the
                // signature of the read guards never being released at all — a
                // `deferred_trims` that stopped being the last field of
                // `RequestStateData`, or a new memoizer that retains one. That is
                // the original bug returning, and at `debug` it would come back
                // exactly as quietly as it went unnoticed the first time.
                if report.batch > 0 && report.still_contended == report.batch {
                    tracing::warn!(
                        batch = report.batch,
                        "every deferred cache-history trim lost its lock; \
                         cache.history was not enforced by this run",
                    );
                }
                if report.failed > 0 {
                    tracing::warn!(
                        failed = report.failed,
                        batch = report.batch,
                        "deferred cache-history trims failed; those targets keep their stale revisions",
                    );
                }
                Ok(())
            }),
            Arc::clone(&self.bg_pending),
        );
    }
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
    /// Children this state has already committed to the shared [`DepDag`].
    ///
    /// Resolving one target walks its input list three times — `link` calls
    /// `get_def` per input, `collect_transitive_deps` calls `get_spec` per
    /// input, and `inputs_result_meta` calls `result_addr` per input — and all
    /// three run against *this* `RequestState`, so each one offers the engine
    /// the same `parent → input` edge. `DepDag::add_dep` already treats the
    /// second and third as no-ops, but only after taking the process-wide
    /// `dep_dag` mutex to look them up: profiling a fully-cached Go corpus put
    /// 10.4% of all CPU in `track_dep`, of which 91% was acquiring that one
    /// lock and under 4% was `add_dep` doing any work.
    ///
    /// Answering the repeat here keeps it off the shared lock entirely. Scoped
    /// to the `RequestState` rather than to the request so it holds only this
    /// target's own children and is freed with the state — a request-wide set
    /// would mirror `DepDag::edges` for the whole build.
    ///
    /// Only *successful* edges are recorded: a rejected one must stay
    /// unrecorded so an identical later attempt is re-checked and re-rejected,
    /// which is the same rule `add_dep` follows for `DepDag::edges` itself.
    tracked: Mutex<FxHashSet<AddrKey>>,
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

    /// Record a post-write `cache.history` trim for `addr`, to run once this
    /// request's cache read guards are released. `hashin` is the revision just
    /// written, which the trim will preserve. See [`DeferredTrims`] for why it
    /// cannot run inline.
    ///
    /// A [`hash_only`](RequestStateData::hash_only) request is refused: it drops
    /// *inside* the resolution it is nested in, while that outer request still
    /// holds the addr's riding read, so its trim would be contended by
    /// construction — and its `bg_pending` is a private counter no drain loop
    /// ever observes. Unreachable today (such a request cannot build a cacheable
    /// target), asserted so it stays that way.
    pub(crate) fn defer_trim(&self, addr: &Addr, keep: u32, hashin: String) {
        debug_assert!(
            !self.hash_only(),
            "a hash_only request must never write a cacheable revision"
        );
        if self.hash_only() {
            tracing::debug!(%addr, "hash_only request: post-write trim not deferred");
            return;
        }
        self.data.deferred_trims.push(addr, keep, hashin);
    }

    /// Stop this request recording post-write trims. Used by the `heph gc`
    /// sweep for its phase-1 resolution state — see [`DeferredTrims::suppressed`].
    pub(crate) fn suppress_deferred_trims(&self) {
        self.data
            .deferred_trims
            .suppressed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Widen this request's deferred-trim retry delay. Tests only — see
    /// [`DeferredTrims::retry_delay_nanos`].
    #[cfg(test)]
    pub(crate) fn set_trim_retry_delay(&self, delay: Duration) {
        self.data.deferred_trims.retry_delay_nanos.store(
            delay.as_nanos() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// True when this request may hash, probe and read but must never take the
    /// exclusive per-addr result lock. See [`RequestStateData::hash_only`].
    pub fn hash_only(&self) -> bool {
        self.data.hash_only
    }

    /// True when this run verifies the codegen tree instead of writing it
    /// (`--frozen`). See [`RequestStateData::frozen`].
    pub fn frozen(&self) -> bool {
        // Acquire/Release, not Relaxed: the top-level frame stores this before
        // spawning the dependency work that reads it, and on a weakly-ordered
        // target (aarch64) a relaxed pair does not order that store against the
        // dependency's load — a dep could observe `false` on a frozen run and
        // write the tree.
        self.data.frozen.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record the run's `--frozen` mode. Called by the top-level frame only, from
    /// the options the user's command built.
    pub(crate) fn set_frozen(&self, frozen: bool) {
        self.data
            .frozen
            .store(frozen, std::sync::atomic::Ordering::Release);
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
        self.data.dispatch(event);
    }

    /// The shared request data, for consumers that must fan an event out
    /// themselves after `self` is no longer borrowable — notably
    /// [`emit_scope`](crate::engine::event::emit_scope)'s end-of-scope drop guard,
    /// which fires after the scoped future (which borrowed the `RequestState`)
    /// has been dropped.
    pub(crate) fn data(&self) -> Arc<RequestStateData> {
        Arc::clone(&self.data)
    }

    /// Emit the `RequestConfig` announcement at most once per request. Safe to
    /// call from every top-level entry point (`result`, `result_addr`); only the
    /// first call emits, so dep recursion never re-announces.
    pub fn announce_request_config(&self, count: usize) {
        if !self
            .data
            .workers_announced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.emit(crate::engine::event::BuildEventKind::RequestConfig {
                max_workers: count,
                fail_fast: self.data.fail_fast,
            });
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
            tracked: Mutex::new(FxHashSet::default()),
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
            tracked: Mutex::new(FxHashSet::default()),
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
        self.data.speculative_live.fetch_add(1, Ordering::AcqRel);
        Arc::new(RequestState {
            data: Arc::clone(&self.data),
            parent: self.parent.clone(),
            skip_providers: Arc::clone(&self.skip_providers),
            crumbs: self.crumbs.clone(),
            speculative: true,
            tracked: Mutex::new(FxHashSet::default()),
        })
    }

    /// How many speculative states are alive on this request right now.
    ///
    /// The invariant `Engine::query`'s `MatchShrug` arm depends on is "at most
    /// one speculative chain at a time", and until this counter it was enforced
    /// only by the shape of the code — a walk's arm sits in the consumer of its
    /// own fan-out, so *that* walk cannot overlap itself. Nothing stopped two
    /// walks on one request from overlapping each other, and nothing could
    /// observe it when they did. Now a test can.
    pub fn speculative_live(&self) -> usize {
        self.data.speculative_live.load(Ordering::Acquire)
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
            // Already committed from this state: `add_dep` would find the edge
            // in `DepDag::edges` and return `Ok(())` unchanged, so answer it
            // here instead of queueing for the shared lock. See `tracked`.
            //
            // One `tracked` acquisition, held across the `dep_dag` one, rather
            // than lock-check-unlock / lock-insert-unlock around it. A repeat —
            // the case this exists for, two calls in three — still costs exactly
            // one uncontended acquisition, and a first offer now costs two rather
            // than three.
            //
            // Lock order is `tracked` then `dep_dag`, and only ever that way:
            // this is the sole place either is taken together, and `add_dep`
            // touches nothing but the `DepDag` it is called on. Holding `tracked`
            // across the `dep_dag` wait does not cost a sibling anything real —
            // a sibling is resolving a *different* input, so its own check would
            // miss and queue for `dep_dag` regardless.
            let mut tracked = self.tracked.lock();
            let key = AddrKey(addr.clone());
            if tracked.contains(&key) {
                return Ok(());
            }
            // `?` before the insert: a rejected edge must stay unrecorded so an
            // identical later attempt is re-checked and re-rejected.
            self.data.dep_dag.lock().add_dep(parent, addr)?;
            tracked.insert(key);
            Ok(())
        } else {
            Ok(())
        }
    }
}

impl RequestStateData {
    /// Fan a fully-built event out to every registered hook and the event
    /// channel (both best-effort). The single delivery path for a `BuildEvent`,
    /// shared by [`RequestState::emit`] and [`emit_scope`](crate::engine::event::emit_scope)'s
    /// drop guard so paired `*End` events reach hooks, not just the channel.
    pub(crate) fn dispatch(&self, event: crate::engine::event::BuildEvent) {
        // Fan out to every registered hook (best-effort, sync push).
        for hook in &self.hooks {
            hook.on_event(&event);
        }
        if let Some(tx) = &self.events {
            // A closed receiver (consumer gone) is expected; events are
            // best-effort, so dropping the send result is intentional.
            drop(tx.send(event));
        }
    }
}

impl Drop for RequestState {
    fn drop(&mut self) {
        // Only speculative states counted up, so only they count down. A
        // non-speculative state pays one predictable branch here; there is no
        // atomic on that path.
        if self.speculative {
            self.data.speculative_live.fetch_sub(1, Ordering::AcqRel);
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
            None,
        )
    }

    /// Like [`new_state_with_events`] but with a caller-supplied background-work
    /// counter, so the renderer that owns the other clone can watch this
    /// request's sandbox cleanups drain during shutdown. `approval` is the
    /// front-end's decision maker for `approval`-gated targets (`None` to fail
    /// any gated target).
    pub fn new_state_full(
        self: &Arc<Self>,
        fail_fast: bool,
        events: Option<crate::engine::event::EventSender>,
        bg_pending: crate::engine::sandbox_cleaner::PendingCounter,
        log_tail_lines: usize,
        approval: Option<Arc<dyn crate::engine::approval::ApprovalHandler>>,
    ) -> Arc<RequestState> {
        self.new_state_inner(
            fail_fast,
            events,
            bg_pending,
            log_tail_lines,
            approval,
            false,
        )
    }

    /// A request that re-reads the tree from *inside* an in-flight resolution.
    ///
    /// `parent` is the addr being resolved, and is not optional: `meta`'s dep walk
    /// derives `is_top` from `RequestState::parent`, so a parent-less nested
    /// request would promote every direct dep to a top-level frame — running each
    /// one's own in_place write-back guard, which spins yet another nested
    /// request per level.
    ///
    /// The returned request is [`hash_only`](RequestStateData::hash_only): it may
    /// not build, so it can never contend the per-addr result lock its own caller
    /// is holding.
    pub fn new_hash_only_state(self: &Arc<Self>, parent: Addr) -> Arc<RequestState> {
        self.new_state_inner(
            true,
            None,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            Self::DEFAULT_LOG_TAIL_LINES,
            None,
            true,
        )
        .with_parent(parent)
    }

    fn new_state_inner(
        self: &Arc<Self>,
        fail_fast: bool,
        events: Option<crate::engine::event::EventSender>,
        bg_pending: crate::engine::sandbox_cleaner::PendingCounter,
        log_tail_lines: usize,
        approval: Option<Arc<dyn crate::engine::approval::ApprovalHandler>>,
        hash_only: bool,
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
            speculative_live: std::sync::atomic::AtomicUsize::new(0),
            mem_execute_cache: Memoizer::with_tag_task("execute_cache", self.runtime.clone()),
            mem_locked_result: Memoizer::with_tag_task("locked_result", self.runtime.clone()),
            mem_codegen_copy: Memoizer::with_tag_task("codegen_copy", self.runtime.clone()),
            mem_remote_blob: Memoizer::with_tag_task("remote_blob", self.runtime.clone()),
            mem_result: Memoizer::with_tag_task("result", self.runtime.clone()),
            mem_meta: Memoizer::with_tag_task("meta", self.runtime.clone()),
            mem_spec: Memoizer::with_tag_task("spec", self.runtime.clone()),
            mem_def: Memoizer::with_tag_task("def", self.runtime.clone()),
            mem_expanded_inputs: Memoizer::with_tag_task("expanded_inputs", self.runtime.clone()),
            mem_packages: Memoizer::with_tag_task("packages", self.runtime.clone()),
            mem_probe: Memoizer::with_tag_task("probe", self.runtime.clone()),
            mem_probe_inner: Memoizer::with_tag_task("probe_inner", self.runtime.clone()),
            mem_states_under: Memoizer::with_tag_task("states_under", self.runtime.clone()),
            fail_fast,
            log_tail_lines,
            events,
            hooks: self.hooks(),
            frozen: std::sync::atomic::AtomicBool::new(false),
            workers_announced: std::sync::atomic::AtomicBool::new(false),
            matched_announced: std::sync::atomic::AtomicBool::new(false),
            bg_pending: Arc::clone(&bg_pending),
            failures: Mutex::new(indexmap::IndexMap::new()),
            approval,
            hash_only,
            deferred_trims: DeferredTrims {
                engine: Arc::downgrade(self),
                bg_pending,
                trims: Mutex::new(FxHashMap::default()),
                suppressed: std::sync::atomic::AtomicBool::new(false),
                retry_delay_nanos: std::sync::atomic::AtomicU64::new(
                    crate::engine::gc::TRIM_RETRY_DELAY.as_nanos() as u64,
                ),
            },
        });

        let state = Arc::new(RequestState {
            data: Arc::clone(&data),
            parent: None,
            skip_providers: Arc::new(HashSet::new()),
            crumbs: None,
            speculative: false,
            tracked: Mutex::new(FxHashSet::default()),
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

    /// A node's index in the DAG's parallel vectors, for white-box assertions.
    fn idx(dag: &DepDag, a: &Addr) -> usize {
        *dag.index_of.get(&ptr_key(a)).expect("addr not in dag") as usize
    }

    /// Build an `Engine` rooted at a unique temp dir so the sqlite cache db
    /// never collides across parallel tests (a shared path locks the db).
    /// The returned `TempDir` must be held alive for the test's duration.
    fn test_engine() -> anyhow::Result<(tempfile::TempDir, Arc<Engine>)> {
        let dir = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let engine = Arc::new(Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?);
        Ok((dir, engine))
    }

    fn bg_state(
        engine: &Arc<Engine>,
    ) -> (
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<crate::engine::request_state::RequestState>,
    ) {
        let bg: Arc<std::sync::atomic::AtomicUsize> =
            Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rs = engine.new_state_full(
            true,
            None,
            Arc::clone(&bg),
            Engine::DEFAULT_LOG_TAIL_LINES,
            None,
        );
        (bg, rs)
    }

    async fn wait_drained(bg: &std::sync::atomic::AtomicUsize) {
        use std::sync::atomic::Ordering;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while bg.load(Ordering::Acquire) > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "background work never drained"
            );
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    /// Recording a trim must *not* take a background slot: the slot is taken
    /// when the batch is handed to the cleaner, which is what `Drop` does.
    ///
    /// This is load-bearing, not incidental. `bg_pending` gates process exit
    /// through an untimed loop in both TUI backends, and a `RequestStateData`
    /// can be pinned indefinitely by an abandoned memoizer cell — so a slot held
    /// on behalf of work that has not been submitted yet would turn that leak
    /// into a process that never exits.
    #[tokio::test]
    async fn recording_a_trim_takes_no_background_slot() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        let (_dir, engine) = test_engine()?;
        let (bg, rs) = bg_state(&engine);

        rs.defer_trim(&addr("t"), 1, "h1".to_string());
        rs.defer_trim(&addr("u"), 1, "h1".to_string());
        assert_eq!(
            bg.load(Ordering::Acquire),
            0,
            "an unsubmitted batch must not gate shutdown"
        );

        drop(rs);
        // Submitted on drop, released by the cleaner once it has run.
        wait_drained(&bg).await;
        Ok(())
    }

    /// The background slot must be held across the trim batch's *delayed*
    /// attempt, not just its first pass.
    ///
    /// `bg_pending` is what keeps the TUI open and the process alive. Moving the
    /// retries off this job — onto a spawned sleeper, the tempting way to keep
    /// the cleaner lane free — would release the slot after the first pass and
    /// let the process exit mid-retry, invisibly: every other test here only
    /// asserts the counter eventually reaches zero, which it still would.
    ///
    /// Measured as "the drain could not have finished sooner than one delay",
    /// which has no upper bound to overshoot and so cannot flake the other way.
    /// The delay is widened well past the production 25ms first, so the margin is
    /// not something a loaded runner can close by accident.
    #[tokio::test]
    async fn the_exit_gate_is_held_across_the_trim_retry() -> anyhow::Result<()> {
        let (_dir, engine) = test_engine()?;
        let (bg, rs) = bg_state(&engine);
        let a = addr("t");

        // Two cache entries, so the trim is genuinely over its `keep = 1` budget
        // and reaches the lock at all — the unlocked pre-count returns early for
        // a target with nothing to delete, and would never contend.
        //
        // Barriered, not just written: `writer` hands the bytes to the sqlite
        // write-behind queue, and the batch enumerates on a read connection that
        // does not wait for it. An unbarriered write is seen as a target with
        // *one* revision, which is within budget — the batch then settles without
        // ever asking for the lock, drains in single-digit ms, and this test fails
        // for a reason that has nothing to do with what it is checking.
        for hashin in ["h1", "h2"] {
            let mut w = engine
                .local_cache
                .writer(&a, hashin, crate::engine::local_cache::MANIFEST_V1)
                .expect("manifest writer");
            std::io::Write::write_all(&mut w, b"x").expect("write");
            w.commit().expect("commit");
            assert!(
                engine
                    .local_cache
                    .exists(&a, hashin, crate::engine::local_cache::MANIFEST_V1)
                    .expect("exists"),
                "revision {hashin} must have landed before the batch enumerates",
            );
        }

        // Held for the whole batch: every pass finds it contended, so the batch
        // is guaranteed to reach — and finish — its one delay.
        let _held = engine
            .result_lock()
            .try_write(&a)
            .expect("try_write")
            .expect("lock free");

        let delay = Duration::from_millis(500);
        rs.set_trim_retry_delay(delay);
        rs.defer_trim(&a, 1, "h2".to_string());

        let started = std::time::Instant::now();
        drop(rs);
        wait_drained(&bg).await;

        assert!(
            started.elapsed() >= delay,
            "the slot was released before the delayed attempt could have run: {:?}",
            started.elapsed(),
        );
        Ok(())
    }

    /// A request that never wrote a cacheable revision must not enqueue anything.
    #[tokio::test]
    async fn no_deferred_trim_means_no_background_work() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        let (_dir, engine) = test_engine()?;
        let (bg, rs) = bg_state(&engine);
        drop(rs);
        assert_eq!(bg.load(Ordering::Acquire), 0);
        Ok(())
    }

    /// The `heph gc` sweep suppresses its phase-1 resolution state's trims — that
    /// sweep is itself the authoritative trim, and a background one would race
    /// its phase-2 write locks.
    #[tokio::test]
    async fn suppressed_request_records_no_trims() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        let (_dir, engine) = test_engine()?;
        let (bg, rs) = bg_state(&engine);
        rs.suppress_deferred_trims();
        rs.defer_trim(&addr("t"), 1, "h1".to_string());
        drop(rs);
        assert_eq!(
            bg.load(Ordering::Acquire),
            0,
            "a suppressed request must enqueue nothing"
        );
        Ok(())
    }

    /// Dropping the engine before the request must release the batch rather than
    /// enqueue work that can never run.
    #[tokio::test]
    async fn engine_dropped_first_discards_the_batch() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        let (_dir, engine) = test_engine()?;
        let (bg, rs) = bg_state(&engine);
        rs.defer_trim(&addr("t"), 1, "h1".to_string());
        drop(engine);
        drop(rs);
        assert_eq!(
            bg.load(Ordering::Acquire),
            0,
            "no slot may be left outstanding when the engine is gone"
        );
        Ok(())
    }

    /// Speculative states are counted while alive, so "at most one speculative
    /// chain at a time" is observable rather than merely structural.
    ///
    /// `Engine::query`'s `MatchShrug` arm resolves candidates on a speculative
    /// state whose cycle check walks per-chain breadcrumbs instead of the shared
    /// `DepDag` — sound only one chain at a time. A walk guarantees that against
    /// itself; nothing guaranteed it *between* walks on one request, and nothing
    /// could see it. `heph validate` awaits its three walks one at a time for
    /// exactly this reason (`src/commands/validate.rs`).
    #[test]
    fn speculative_states_are_counted_while_alive() -> anyhow::Result<()> {
        let (_dir, engine) = test_engine()?;
        let root = engine.new_state();
        assert_eq!(root.speculative_live(), 0, "no chain to begin with");

        {
            let a = root.speculative();
            assert_eq!(root.speculative_live(), 1);
            {
                // Two at once is the hazard the counter exists to expose.
                let b = root.speculative();
                assert_eq!(
                    root.speculative_live(),
                    2,
                    "overlapping chains must be visible, not silent"
                );
                drop(b);
            }
            assert_eq!(root.speculative_live(), 1, "the inner chain released");
            drop(a);
        }
        assert_eq!(root.speculative_live(), 0, "every chain released");

        // Sequential use — what `validate` now does — never exceeds one.
        for _ in 0..3 {
            let s = root.speculative();
            assert_eq!(root.speculative_live(), 1);
            drop(s);
        }
        assert_eq!(root.speculative_live(), 0);

        // A non-speculative child is not counted; only the shrug arm's states are.
        let child = root.with_parent(addr("a"));
        assert_eq!(root.speculative_live(), 0, "ordinary states are not chains");
        drop(child);
        Ok(())
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

    /// `track_dep` answers a repeated edge from the per-state `tracked` set
    /// instead of the shared `DepDag`, so that set must learn only about edges
    /// the DAG actually accepted — otherwise a cycle would be rejected once and
    /// silently allowed on every later attempt.
    #[test]
    fn track_dep_caches_accepted_edges_but_never_rejected_ones() -> anyhow::Result<()> {
        let (_dir, engine) = test_engine()?;
        let root = engine.new_state();

        let a = addr("a");
        let b = addr("b");

        // From A: A→B is accepted, and repeats stay accepted (this is the
        // repeat the cache is here to absorb — `link`, `collect_transitive_deps`
        // and `inputs_result_meta` each offer it once).
        let from_a = root.with_parent(a.clone());
        for _ in 0..3 {
            assert!(from_a.track_dep(&b).is_ok(), "A→B must stay accepted");
        }

        // From B: B→A closes the cycle. Rejected, and it must be rejected every
        // single time — a cached rejection would read as "already tracked".
        let from_b = root.with_parent(b.clone());
        for i in 0..3 {
            assert!(
                from_b.track_dep(&a).is_err(),
                "B→A closes a cycle and must stay rejected (attempt {i})"
            );
        }

        // A self-loop bails before the DAG records anything, so it must not be
        // cached as accepted either.
        for i in 0..3 {
            assert!(
                from_a.track_dep(&a).is_err(),
                "A→A is a self-loop and must stay rejected (attempt {i})"
            );
        }
        Ok(())
    }

    #[test]
    fn test_dep_dag_rejected_edge_is_not_recorded() {
        // The membership index must only ever learn about edges `add_dep`
        // actually committed. If a cycle-rejected edge were recorded, the
        // second attempt would short-circuit to Ok and the cycle would be
        // silently accepted.
        let mut dag = DepDag::new();
        let a = addr("a");
        let b = addr("b");
        let c = addr("c");
        assert!(dag.add_dep(&a, &b).is_ok());
        assert!(dag.add_dep(&b, &c).is_ok());

        for _ in 0..3 {
            assert!(
                dag.add_dep(&c, &a).is_err(),
                "a rejected edge must stay rejected on every retry"
            );
        }
        // Self-loops take the earlier `f == t` bail; they must not be recorded
        // either.
        for _ in 0..3 {
            assert!(dag.add_dep(&a, &a).is_err());
        }

        // A rejection must also leave the ordering usable: the cycle bail
        // returns from inside the δ⁺ walk, before any `ord` is rewritten, so a
        // later legitimate reorder must still produce a valid topological
        // order. If a rejection ever corrupted `ord`, the only symptom would be
        // a wrong verdict on some unrelated later edge.
        let d = addr("d");
        assert!(dag.add_dep(&d, &a).is_ok());
        let (ai, bi, ci, di) = (idx(&dag, &a), idx(&dag, &b), idx(&dag, &c), idx(&dag, &d));
        assert!(dag.ord[di] < dag.ord[ai]);
        assert!(dag.ord[ai] < dag.ord[bi]);
        assert!(dag.ord[bi] < dag.ord[ci]);
    }

    #[test]
    fn test_dep_dag_duplicate_check_does_not_scan_adjacency_lists() {
        // White-box: the already-present short-circuit must consult the
        // membership index, not the adjacency lists. Emptying them behind the
        // DAG's back must not make a known edge look new — the old
        // `succ[f].contains(&t)` scan would miss, take the forward branch, and
        // re-push into both lists.
        let mut dag = DepDag::new();
        let hub = addr("hub");
        let leaf = addr("leaf");
        assert!(dag.add_dep(&hub, &leaf).is_ok());

        let (hi, li) = (idx(&dag, &hub), idx(&dag, &leaf));
        dag.succ[hi].clear();
        dag.pred[li].clear();
        assert!(dag.add_dep(&hub, &leaf).is_ok());
        assert!(
            dag.succ[hi].is_empty() && dag.pred[li].is_empty(),
            "duplicate add must neither scan nor re-push the adjacency lists"
        );
    }

    #[test]
    fn test_dep_dag_wide_node_repeat_edges_never_rescan() {
        // The transparent-group re-inline (`result.rs`, before
        // `mem_result.once`) re-walks a group's whole dep list once per parent
        // that reaches it, so the same D edges are offered over and over. Each
        // repeat offer must be answered from the membership index alone.
        //
        // Deterministic rather than timed: `succ[hub]` is emptied between the
        // passes, so an implementation that rescans the adjacency list misses,
        // takes the forward branch, and rebuilds the list to D entries. D is
        // wide enough to be the shape this guards without making the test
        // expensive — the assertion is structural, not statistical.
        const D: usize = 2_000;

        let hub = addr("hub");
        let leaves: Vec<Addr> = (0..D).map(|i| addr(&format!("wide_leaf{i}"))).collect();

        let mut dag = DepDag::new();
        for leaf in &leaves {
            dag.add_dep(&hub, leaf).unwrap();
        }
        let hi = idx(&dag, &hub);
        assert_eq!(dag.succ[hi].len(), D);

        dag.succ[hi].clear();
        for leaf in &leaves {
            dag.add_dep(&hub, leaf).unwrap();
        }
        assert!(
            dag.succ[hi].is_empty(),
            "re-offering {D} known edges rescanned (and rebuilt) the adjacency list"
        );
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

        let (ai, bi, ci) = (idx(&dag, &a), idx(&dag, &b), idx(&dag, &c));
        assert!(dag.ord[bi] < dag.ord[ai]);
        assert!(dag.ord[ai] < dag.ord[ci]);
        assert!(dag.ord[bi] < dag.ord[ci]);

        // b→a was committed through the reorder branch, the second of
        // `add_dep`'s two commit sites. Re-offering it must short-circuit on the
        // membership index exactly like a forward-path edge. It is the reorder
        // site that makes this worth pinning: the reorder *fixed* the ordering,
        // so ord[b] < ord[a] now holds and a re-offer that missed the index
        // would sail through the forward branch and push a duplicate into both
        // adjacency lists — silently, since a duplicate only costs an extra
        // visit in the δ walks.
        let (succ_b, pred_a) = (dag.succ[bi].len(), dag.pred[ai].len());
        for _ in 0..3 {
            assert!(dag.add_dep(&b, &a).is_ok());
        }
        assert_eq!(dag.succ[bi].len(), succ_b);
        assert_eq!(dag.pred[ai].len(), pred_a);

        // Same white-box check as the forward path: with the lists emptied, a
        // known reorder-committed edge must still be answered from the index.
        dag.succ[bi].clear();
        dag.pred[ai].clear();
        assert!(dag.add_dep(&b, &a).is_ok());
        assert!(dag.succ[bi].is_empty() && dag.pred[ai].is_empty());
    }

    #[test]
    fn test_dep_dag_concurrent_stress() {
        // 64 threads each adding 100 acyclic edges, and every one of them also
        // attempting the same chain-closing edge. Assert that all the acyclic
        // inserts succeed and that the closing edge is rejected on *every*
        // attempt — a rejected edge must never be recorded in the membership
        // index, or a later attempt would short-circuit to Ok and the cycle
        // would be silently accepted. Interleaving the attempts with 6400
        // concurrent inserts (which force reorders) is the point: the verdict
        // must not depend on what else landed in between.
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
        let closing_attempts = Arc::new(AtomicUsize::new(0));
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
                    // Every thread tries to close the seed chain into a cycle.
                    closing_attempts.fetch_add(1, Ordering::SeqCst);
                    let res = dag.lock().add_dep(&addr("a9"), &addr("a0"));
                    match res {
                        Ok(()) => ok_count.fetch_add(1, Ordering::SeqCst),
                        Err(_) => err_count.fetch_add(1, Ordering::SeqCst),
                    };
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(closing_attempts.load(Ordering::SeqCst), 64);
        assert_eq!(ok_count.load(Ordering::SeqCst), 0);
        assert_eq!(err_count.load(Ordering::SeqCst), 64);
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
        let _rt = crate::engine::test_rt_enter();
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

    // The `*End` event of an `emit_scope` fans out to registered hooks — not just
    // the renderer channel. Regression: the end-of-scope drop guard used to emit
    // via the event sender only, so an out-of-process hook (e.g. the GHA status
    // plugin) saw every `*Start` but no `*End`, tallying `done`/`built` as zero.
    // Exercised with no renderer channel (events = None), the exact failing case.
    #[tokio::test]
    async fn emit_scope_end_reaches_hooks_without_renderer() -> anyhow::Result<()> {
        use crate::engine::hook::Hook;
        use hcore::events::{BuildEvent, BuildEventKind};

        #[derive(Default)]
        struct Rec {
            ends: Mutex<Vec<String>>,
        }
        impl Hook for Rec {
            fn name(&self) -> String {
                "rec".into()
            }
            fn on_event(&self, ev: &BuildEvent) {
                if let BuildEventKind::ResultEnd { addr, .. } = &ev.kind {
                    self.ends.lock().push(addr.clone());
                }
            }
            fn on_close(&self) {}
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let _rt = crate::engine::test_rt_enter();
        let mut e = Engine::new(Config {
            root: dir.path().to_path_buf(),
            home_dir: PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?;
        let rec = Arc::new(Rec::default());
        e.register_hook(Arc::clone(&rec) as Arc<dyn Hook>)?;
        let engine = Arc::new(e);

        // No renderer channel (events = None): the hook is the only consumer.
        let state = engine.new_state_with_events(true, None);
        crate::engine::event::emit_scope(
            &state,
            BuildEventKind::ResultStart {
                addr: "//a:b".into(),
            },
            |error: Option<crate::engine::event::ErrorDetail>| BuildEventKind::ResultEnd {
                addr: "//a:b".into(),
                error: error.map(crate::engine::event::ErrorDetail::into_message),
                upstream_of: None,
                exit_status: None,
                log_tail: None,
            },
            async { anyhow::Ok(()) },
        )
        .await?;

        assert_eq!(
            rec.ends.lock().clone(),
            vec!["//a:b".to_string()],
            "the *End event must reach the hook even with no renderer channel"
        );
        Ok(())
    }

    /// Two content-equal `Addr`s are ONE memoizer key.
    ///
    /// This is the invariant `AddrKey`'s pointer hash rests on, and it is
    /// interning that supplies it — `Addr::PartialEq` is already `Arc::ptr_eq`,
    /// so the map's behaviour is unchanged by hashing the pointer. If interning
    /// ever stopped handing out one `Arc` per distinct content, this goes red
    /// and every addr-keyed memoizer would silently double-compute — worse than
    /// slow for `mem_locked_result`, whose whole job is to stop two sibling
    /// computations of one addr both reaching for the non-reentrant per-addr
    /// lock.
    #[test]
    fn content_equal_addrs_are_one_addr_key() {
        let a = addr("t");
        let b = addr("t");
        let c = addr("u");

        let mut m: FxHashMap<AddrKey, u32> = FxHashMap::default();
        m.insert(AddrKey(a.clone()), 1);
        m.insert(AddrKey(b.clone()), 2);
        m.insert(AddrKey(c.clone()), 3);

        assert_eq!(m.len(), 2, "content-equal addrs must share one cell");
        assert_eq!(
            m.get(&AddrKey(b)),
            Some(&2),
            "the second insert addressed the same entry"
        );
        assert_eq!(m.get(&AddrKey(c)), Some(&3));
        assert!(
            !m.contains_key(&AddrKey(addr("other"))),
            "a distinct addr must not alias an existing cell"
        );
        drop(a);
    }

    /// `Hash` agrees with `Eq` — the contract a `HashMap` key owes.
    #[test]
    fn addr_key_hash_agrees_with_eq() {
        use std::hash::{Hash, Hasher};

        fn h(k: &AddrKey) -> u64 {
            let mut hasher = rustc_hash::FxHasher::default();
            k.hash(&mut hasher);
            hasher.finish()
        }

        let a = AddrKey(addr("t"));
        let b = AddrKey(addr("t"));
        assert_eq!(a, b);
        assert_eq!(h(&a), h(&b), "equal keys must hash equal");
    }

    /// The memoizer inventory and stall diagnostics render the key with
    /// `Debug`. A key that printed a pointer would turn "which target is
    /// stuck?" into an unanswerable question, so `AddrKey` forwards to `Addr`.
    #[test]
    fn addr_key_debug_names_the_target() {
        let a = addr("t");
        assert_eq!(format!("{:?}", AddrKey(a.clone())), format!("{a:?}"));
    }
}
