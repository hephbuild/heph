use crate::engine::Engine;
use crate::engine::driver::targetdef::TargetDef;
use crate::engine::error::{Culprit, CycleError, TargetFailure};
use crate::engine::meta::ResultMeta;
use crate::engine::provider::{State, TargetSpec};
use crate::engine::resource_pool::sampler::PoolSampler;
use crate::engine::result::{
    ExecutedArtifacts, ExtendedTargetDef, LockedResolution, OutputMatcher,
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
type ExecuteCacheResult = Result<ExecutedArtifacts, ArcErr>;
type ProbeStatesResult = Result<Arc<Vec<State>>, ArcErr>;

/// The pools a `RequestConfig` announces: each one's capacity (`None`:
/// unbounded) and its unit's name, keyed by pool name.
#[derive(Debug, Default)]
pub struct PoolAnnouncement {
    pub capacities: std::collections::BTreeMap<String, Option<u64>>,
    pub units: std::collections::BTreeMap<String, String>,
}

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
    /// Buffers for [`DepDag::reorder`], kept across back-edges so a reorder
    /// under the request-wide `dep_dag` lock does not allocate once they have
    /// grown.
    pk: PkScratch,
    /// The nodes the request asked for (its matched selection, or the one
    /// address of a single-target run): where a failure's path starts.
    /// Held here rather than beside the graph so a walk reads it under the
    /// one `dep_dag` lock, which stays a leaf.
    requested: FxHashSet<u32>,
}

/// Unvisited, in the dense per-node tables of a [`DepDag`] walk.
const UNSEEN: u32 = u32::MAX;

/// Working buffers for one Pearce-Kelly reorder. Contents are meaningless
/// between reorders; only the capacity carries over.
#[derive(Debug, Default)]
struct PkScratch {
    delta_plus: Vec<u32>,
    delta_minus: Vec<u32>,
    stack: Vec<u32>,
    positions: Vec<u32>,
    /// Visited set of the current walk: `mark[n] == epoch` iff `n` was
    /// visited. Bumping `epoch` empties it without touching `mark`.
    mark: Vec<u32>,
    epoch: u32,
}

impl PkScratch {
    /// Start a walk over a graph of `nodes` nodes with nothing visited.
    fn new_walk(&mut self, nodes: usize) {
        self.mark.resize(nodes, 0);
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            // Wrapped: a stale mark could equal a reused epoch. 0 is never an
            // epoch, so clearing to 0 empties every mark.
            self.mark.fill(0);
            self.epoch = 1;
        }
    }

    /// Mark `n` visited; `true` iff it was not already.
    #[expect(
        clippy::indexing_slicing,
        reason = "new_walk sized `mark` to the node count, and callers pass node indices"
    )]
    fn visit(&mut self, n: u32) -> bool {
        let m = &mut self.mark[n as usize];
        let fresh = *m != self.epoch;
        *m = self.epoch;
        fresh
    }
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

        // Taken out for the reorder and put back on every exit, the cycle
        // rejection included, so the buffers' capacity is never lost.
        let mut pk = std::mem::take(&mut self.pk);
        let acyclic = self.reorder(f, t, &mut pk);
        self.pk = pk;
        if !acyclic {
            return Err(CycleError {
                from: from.clone(),
                to: to.clone(),
            });
        }

        self.commit_edge(f, t);
        Ok(())
    }

    /// Pearce-Kelly reorder for the back-edge `f → t` (`ord[f] >= ord[t]`)
    /// over the affected region `[ord[t], ord[f]]`. δ⁺ = forward reach from
    /// `t`; δ⁻ = backward reach from `f`. Cycle iff δ⁺ touches `f`: then
    /// returns `false` without having rewritten any `ord`.
    #[expect(
        clippy::indexing_slicing,
        reason = "all u32 indices are valid NodeIndex values produced by get_or_insert and bound to the vectors' lengths"
    )]
    fn reorder(&mut self, f: u32, t: u32, pk: &mut PkScratch) -> bool {
        let upper = self.ord[f as usize];
        let lower = self.ord[t as usize];
        // A cycle bail leaves these partly filled; only the capacity is kept.
        pk.delta_plus.clear();
        pk.delta_minus.clear();
        pk.stack.clear();
        pk.positions.clear();

        pk.new_walk(self.nodes.len());
        pk.visit(t);
        pk.delta_plus.push(t);
        pk.stack.push(t);
        while let Some(n) = pk.stack.pop() {
            for &w in &self.succ[n as usize] {
                if w == f {
                    return false;
                }
                if self.ord[w as usize] <= upper && pk.visit(w) {
                    pk.delta_plus.push(w);
                    pk.stack.push(w);
                }
            }
        }

        pk.new_walk(self.nodes.len());
        pk.visit(f);
        pk.delta_minus.push(f);
        pk.stack.push(f);
        while let Some(n) = pk.stack.pop() {
            for &w in &self.pred[n as usize] {
                if self.ord[w as usize] >= lower && pk.visit(w) {
                    pk.delta_minus.push(w);
                    pk.stack.push(w);
                }
            }
        }

        // Unstable is exact here: `ord` is a permutation, so no two nodes share
        // a key and every sort order is the same.
        pk.delta_minus
            .sort_unstable_by_key(|&n| self.ord[n as usize]);
        pk.delta_plus
            .sort_unstable_by_key(|&n| self.ord[n as usize]);

        pk.positions
            .extend(pk.delta_minus.iter().map(|&n| self.ord[n as usize]));
        pk.positions
            .extend(pk.delta_plus.iter().map(|&n| self.ord[n as usize]));
        pk.positions.sort_unstable();

        for (&n, &p) in pk
            .delta_minus
            .iter()
            .chain(&pk.delta_plus)
            .zip(&pk.positions)
        {
            self.ord[n as usize] = p;
        }
        true
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

    fn index(&self, addr: &Addr) -> Option<u32> {
        self.index_of.get(&ptr_key(addr)).copied()
    }

    /// Record `addr` as one the request asked for.
    pub fn mark_requested(&mut self, addr: &Addr) {
        let n = self.get_or_insert(addr);
        self.requested.insert(n);
    }

    /// The shortest path from a requested target to `culprit`, inclusive:
    /// `[culprit]` when it was itself requested, empty when no requested
    /// target reaches it (or it is not in the graph).
    ///
    /// A predecessor BFS from the culprit, level by level, that stops at the
    /// first level holding a requested target. Ties go by address byte order:
    /// each level is visited sorted, so a node's next hop toward the culprit
    /// is its smallest-addressed neighbour one step closer, and the path
    /// starts at the smallest requested address at the shortest distance.
    /// The same graph gives the same path, whatever order its edges came in.
    /// Each visited node is formatted once; nothing is visited twice.
    #[expect(
        clippy::indexing_slicing,
        reason = "node indices come from index_of or the adjacency lists, both bound to the vectors' lengths"
    )]
    pub fn culprit_path(&self, culprit: &Addr) -> Vec<Addr> {
        let Some(c) = self.index(culprit) else {
            return Vec::new();
        };
        if self.requested.contains(&c) {
            return vec![culprit.clone()];
        }
        // Each visited node's next hop toward the culprit (`UNSEEN` at it).
        let mut next: FxHashMap<u32, u32> = FxHashMap::default();
        next.insert(c, UNSEEN);
        let mut level = vec![c];
        let mut above = Vec::new();
        while !level.is_empty() {
            above.clear();
            for &v in &level {
                for &p in &self.pred[v as usize] {
                    if let std::collections::hash_map::Entry::Vacant(e) = next.entry(p) {
                        e.insert(v);
                        above.push(p);
                    }
                }
            }
            above.sort_by_cached_key(|&n| self.nodes[n as usize].format());
            if let Some(&start) = above.iter().find(|n| self.requested.contains(n)) {
                return self.follow(start, |n| next.get(&n).copied().unwrap_or(UNSEEN));
            }
            std::mem::swap(&mut level, &mut above);
        }
        Vec::new()
    }

    /// `start`'s addresses along `next` until it reaches `UNSEEN`.
    #[expect(
        clippy::indexing_slicing,
        reason = "every hop is a node index a walk over this graph recorded"
    )]
    fn follow(&self, start: u32, next: impl Fn(u32) -> u32) -> Vec<Addr> {
        let mut path = Vec::new();
        let mut n = start;
        while n != UNSEEN {
            path.push(self.nodes[n as usize].clone());
            n = next(n);
        }
        path
    }

    /// The request-end pass: for each of `skipped` that reaches one of
    /// `culprits` (each a culprit and the node it is recorded at: the target,
    /// or for a file the address that led into it), its nearest culprit, the
    /// path from it to that culprit, and how many *other* culprits it reaches.
    ///
    /// One multi-source BFS from every culprit over predecessor edges, level
    /// by level, each level sorted by (root, address): a node takes its root
    /// and next hop from the first neighbour one step closer, so its root is
    /// the smallest culprit (by its text's byte order) at the shortest
    /// distance, and its next hop the smallest address leading there. Then
    /// one pass in reverse topological order ORs each node's successors'
    /// culprit bitsets into its own, for an exact count. O(nodes + edges)
    /// plus the sorts, once per failed request; memory is one bitset of
    /// `culprits / 64` words per node that reaches a culprit.
    #[expect(
        clippy::indexing_slicing,
        reason = "node indices come from index_of or the adjacency lists and dense positions from `pos`, all bound to their tables' lengths"
    )]
    pub fn skipped_report<'a>(
        &self,
        culprits: impl IntoIterator<Item = (Culprit, &'a Addr)>,
        skipped: impl IntoIterator<Item = &'a Addr>,
    ) -> SkippedReport {
        let mut ranked: Vec<(String, Culprit, u32)> = culprits
            .into_iter()
            .filter_map(|(c, at)| Some((c.to_string(), c, self.index(at)?)))
            .collect();
        ranked.sort_by(|a, b| a.0.cmp(&b.0));
        ranked.dedup_by(|a, b| a.1 == b.1);

        let mut root = vec![UNSEEN; self.nodes.len()];
        let mut next = vec![UNSEEN; self.nodes.len()];
        let mut seen: Vec<u32> = Vec::new();
        for (k, (_, _, n)) in ranked.iter().enumerate() {
            if root[*n as usize] == UNSEEN {
                root[*n as usize] = k as u32;
                seen.push(*n);
            }
        }
        seen.sort_by_cached_key(|&n| (root[n as usize], self.nodes[n as usize].format()));
        let mut start = 0;
        while start < seen.len() {
            let end = seen.len();
            for i in start..end {
                let v = seen[i];
                for &p in &self.pred[v as usize] {
                    if root[p as usize] == UNSEEN {
                        root[p as usize] = root[v as usize];
                        next[p as usize] = v;
                        seen.push(p);
                    }
                }
            }
            let (_, level) = seen.split_at_mut(end);
            level.sort_by_cached_key(|&n| (root[n as usize], self.nodes[n as usize].format()));
            start = end;
        }

        // Dense position of each visited node, for the bitsets and the report.
        let mut pos = vec![UNSEEN; self.nodes.len()];
        for (i, &n) in seen.iter().enumerate() {
            pos[n as usize] = i as u32;
        }
        let words = ranked.len().div_ceil(64);
        let mut bits = vec![0u64; seen.len() * words];
        for (k, (_, _, n)) in ranked.iter().enumerate() {
            bits[pos[*n as usize] as usize * words + k / 64] |= 1 << (k % 64);
        }
        let mut topo = seen.clone();
        topo.sort_unstable_by_key(|&n| std::cmp::Reverse(self.ord[n as usize]));
        let mut more = vec![0u32; seen.len()];
        for &u in &topo {
            let pu = pos[u as usize] as usize;
            for &s in &self.succ[u as usize] {
                let ps = pos[s as usize];
                if ps == UNSEEN {
                    continue;
                }
                for w in 0..words {
                    let from = bits[ps as usize * words + w];
                    bits[pu * words + w] |= from;
                }
            }
            let reached: u32 = bits[pu * words..(pu + 1) * words]
                .iter()
                .map(|w| w.count_ones())
                .sum();
            more[pu] = reached.saturating_sub(1);
        }

        let mut index = FxHashMap::default();
        for addr in skipped {
            if let Some(n) = self.index(addr)
                && pos[n as usize] != UNSEEN
            {
                index.insert(ptr_key(addr), pos[n as usize]);
            }
        }
        SkippedReport {
            culprits: ranked.into_iter().map(|(_, c, _)| c).collect(),
            addrs: seen
                .iter()
                .map(|&n| self.nodes[n as usize].clone())
                .collect(),
            next: seen
                .iter()
                .map(|&n| match next[n as usize] {
                    UNSEEN => UNSEEN,
                    hop => pos[hop as usize],
                })
                .collect(),
            root: seen.iter().map(|&n| root[n as usize]).collect(),
            more,
            index,
        }
    }
}

/// What the request-end pass ([`DepDag::skipped_report`]) found for the
/// skipped targets it was asked about. Owns what it read, so it outlives the
/// `dep_dag` lock it was computed under.
#[derive(Debug, Default)]
pub struct SkippedReport {
    /// The culprits, in byte order of their text: what `root` indexes.
    culprits: Vec<Culprit>,
    /// Every node that reaches a culprit, by dense position.
    addrs: Vec<Addr>,
    /// Each node's next hop toward its root (`UNSEEN` at a culprit).
    next: Vec<u32>,
    root: Vec<u32>,
    more: Vec<u32>,
    /// The asked-about skipped targets that reach a culprit, keyed by
    /// pointer (the `addrs` above keep each interned address alive).
    index: FxHashMap<AddrPtrKey, u32>,
}

/// One skipped target's entry in a [`SkippedReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRecord {
    /// Its nearest culprit; ties by byte order.
    pub root: Culprit,
    /// From the skipped target to the address `root` is recorded at,
    /// inclusive both ends.
    pub path: Vec<Addr>,
    /// The exact count of culprits other than `root` it reaches.
    pub more: usize,
}

impl SkippedReport {
    /// `addr`'s record, when it was asked about and reaches a culprit.
    pub fn get(&self, addr: &Addr) -> Option<SkippedRecord> {
        let i = *self.index.get(&ptr_key(addr))?;
        let root = self
            .culprits
            .get(*self.root.get(i as usize)? as usize)?
            .clone();
        let mut path = Vec::new();
        let mut n = i;
        while let Some(a) = self.addrs.get(n as usize) {
            path.push(a.clone());
            n = self.next.get(n as usize).copied().unwrap_or(UNSEEN);
        }
        Some(SkippedRecord {
            root,
            path,
            more: self.more.get(i as usize).copied().unwrap_or(0) as usize,
        })
    }

    /// How many asked-about targets have a record.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

/// The targets one root failure took down: the first few by name, in the
/// order they failed, and how many in all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TookDown {
    pub named: Vec<String>,
    pub total: usize,
}

impl TookDown {
    /// How many a root keeps by name; the report says `+N more` past them.
    pub const NAMED: usize = 5;
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
    // Key includes `is_top`: a top-level frame surfaces a failure as the run's own
    // (`surface_top`) where a dependency frame reports it as collateral, so the two
    // do not produce interchangeable results and must not share a cell. The codegen
    // tree write-back used to be the reason and no longer is — it is
    // is_top-independent and single-flights on `mem_codegen_tree`.
    pub(crate) mem_result: Memoizer<
        (AddrKey, OutputMatcher, bool),
        Result<Arc<crate::engine::result::MemoResult>, ArcErr>,
    >,
    pub(crate) mem_execute_cache: Memoizer<(AddrKey, String), ExecuteCacheResult>,
    /// Single-flights the per-addr result-LOCK + cache-fetch/execute, keyed by
    /// `Addr` ALONE (not `is_top`/`outputs`). The `(outputs, is_top)`
    /// `mem_result` cells all await this, share its one riding read guard (held
    /// only weakly here — the read lives as long as the artifacts carrying it;
    /// see `result::GuardSlot`), then filter outputs on top. Keyed addr-only so
    /// two sibling computations of one
    /// addr can never both hold the non-reentrant per-addr lock — the
    /// self-deadlock this prevents.
    pub(crate) mem_locked_result: Memoizer<AddrKey, Result<Arc<LockedResolution>, ArcErr>>,
    /// Single-flights the codegen tree write-back, keyed by `Addr` alone. Every
    /// frame that resolves the target offers it — the top-level one, each
    /// dependent that reads an output group, and the `meta` walk that only hashes
    /// it — and exactly one performs it, so the tree is written once per addr per
    /// request no matter how many ways the target was reached. The in_place
    /// write-back guard and the fixpoint registration ride inside it, for the same
    /// reason: one write, one guard, one fixpoint.
    pub(crate) mem_codegen_tree: Memoizer<AddrKey, Result<(), ArcErr>>,
    /// Single-flights the lazy pull of one remote blob, keyed by
    /// `(addr, hashin, blob name)`. Two `outputs` cells of the same addr both need
    /// its support files, so without this they would download and write the same
    /// blob concurrently — duplicate transfer, and two writers racing on one cache
    /// key.
    pub(crate) mem_remote_blob: Memoizer<(AddrKey, String, String), Result<bool, ArcErr>>,
    pub mem_meta: Memoizer<AddrKey, Result<ResultMeta, ArcErr>>,
    pub mem_spec: Memoizer<AddrKey, Result<Arc<EngineTargetSpec>, ArcErr>>,
    pub mem_def: Memoizer<AddrKey, Result<Arc<ExtendedTargetDef>, ArcErr>>,
    pub(crate) mem_expanded_inputs:
        Memoizer<AddrKey, Result<Arc<crate::engine::expand::ExpandedInputs>, ArcErr>>,
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
    /// Whether this request's walks act on listed facts — `Engine::query`'s
    /// and the query-target walk (`EngineProviderExecutor::query`). Never read
    /// by `states_under`, `Engine::states`, `get_spec` or `Selection::selects`.
    pub listed_facts_trust: crate::engine::listed::ListedFactsTrust,
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
    /// Keys the fs plugin's codegen reports have already fired for in this
    /// run ([`CodegenReport::first_in_run`]): a `Modified` warning, an
    /// unrecorded-match warning and a `CodegenExcluded` event each fire once
    /// per path per request.
    ///
    /// [`CodegenReport::first_in_run`]: hbuiltins::pluginfs::CodegenReport::first_in_run
    pub(crate) codegen_reported: Mutex<FxHashSet<String>>,
    /// Guards the one-shot `RequestConfig` announcement so it fires once per request
    /// regardless of which entry point (`result` / `result_addr`) is hit first.
    pub workers_announced: std::sync::atomic::AtomicBool,
    /// The `PoolUsage` sampler, started by this request's first admission
    /// ([`RequestState::sample_pool`]) and finished when the request's data
    /// drops, before the hooks close. Empty for a request that admits nothing.
    pool_sampler: std::sync::OnceLock<Arc<PoolSampler>>,
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
    /// This request's sandbox removals that still hold a `sandbox` pool slot:
    /// one per queued removal, dropped once its slot is back. The last
    /// `PoolUsage` waits for it (bounded), so the report a request ends on
    /// shows the slots its runs freed.
    sandbox_removals: crate::engine::sandbox_cleaner::PendingCounter,
    /// Per-request registry of culprits, keyed by what is to blame (a target,
    /// or a BUILD file that did not evaluate). Populated by the `result_addr`
    /// classifier (first-writer-wins dedup) and drained once at the end of
    /// execution for rendering. Shared via `Arc<RequestStateData>`, so
    /// `with_parent` / `with_skip_provider` children record into the same map.
    pub failures: Mutex<indexmap::IndexMap<Culprit, Arc<TargetFailure>>>,
    /// The target behind each `_transitive_<hash>_<i>` origin id a
    /// dependent's merged transitive inputs carry, keyed by `<hash>`
    /// (`Addr::hash_str`): who declared a merged input, for a failure that
    /// blames the declarer ([`MissingDep`](hplugin::error::MissingDep)).
    /// Filled by `collect_transitive_deps` only when a transitive merges, and
    /// read only on a failure. Never reaches a hash: the origin ids keep
    /// their text; this is the reverse lookup, held for the request.
    transitive_declarers: Mutex<FxHashMap<String, Addr>>,
    /// How many request-end passes ([`RequestState::skipped_report`]) ran:
    /// one per failed request is the budget.
    report_passes: std::sync::atomic::AtomicUsize,
    /// What each root failure took down, keyed by the root's formatted addr:
    /// the targets whose `ResultEnd` named it as `upstream_of`. Read by the
    /// interactive failure report's `took down:` row.
    took_down: Mutex<FxHashMap<String, TookDown>>,
    /// The targets whose failing `ResultEnd` was a stop, not a failure
    /// ([`RequestState::end_is_cancelled`]), by formatted addr: roots and
    /// what they took down. The report leaves them out.
    cancelled: Mutex<FxHashSet<String>>,
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
    /// What `--force` / `--no-scratch` on a selector run reach. Set at most once,
    /// by `Engine::result` before its walk starts; see [`Selection`].
    ///
    /// Per `RequestStateData`, so every `with_parent` / speculative child of the
    /// request sees it, and a [`hash_only`](Self::hash_only) request — which is
    /// a separate `RequestStateData` — never does.
    selection: std::sync::OnceLock<Selection>,
    /// `--shell=ADDR`: the one target of this request that is entered instead of
    /// run, and the terminal handed to it. See [`ShellTarget`].
    ///
    /// Fixed when the request is constructed and never changed: "does this
    /// frame shell?" must be a pure function of the addr for the request's whole
    /// lifetime, so no frame can resolve before it is known.
    shell: Option<ShellTarget>,
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
        // The read guards the batch's trims contend with ride on artifacts, not
        // on memoized values (see `result::GuardSlot`). `deferred_trims` is the
        // last field of `RequestStateData`, so by the time this drop runs, the
        // request's memoizers are gone and their abort cascades are in flight —
        // an aborted chain may still hold artifacts, and with them a read. The
        // cascade lands when the runtime processes it, which is why the batch
        // below probes, re-probes, and retries once rather than expecting the
        // guards to be gone already. (The blocking
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
                // The trim freed pages inside cache.db; give them back to the
                // filesystem when due. In this job, not a thread of its own, so
                // the `bg_pending` slot covers it like the retries above; it is
                // bounded by its time budget and never fails the run.
                if report.removed > 0 {
                    engine.reclaim_cache_db_after_trim();
                }
                Ok(())
            }),
            Arc::clone(&self.bg_pending),
        );
    }
}

/// The target `--shell=ADDR` named, and the terminal it gets.
///
/// Request-scoped rather than carried in `ResultOptions`, for two reasons that
/// both reduce to "a `ResultOptions` does not reach every frame":
///
/// - It travels down the selection and group-inlining legs only. Dependency
///   resolution deliberately resets it (`ResultOptions::default()`) and `meta`
///   has no `opts` in scope at all, so an addr named with `--shell=` would be
///   unreachable the moment it appeared as a dependency — and "the flag you
///   passed did nothing" is the bug this feature exists to fix.
/// - `mem_locked_result` is keyed by `Addr` alone. If "does this frame shell?"
///   were a function of the *frame's* options, an addr reached both as a
///   selected target and as some sibling's dependency would bake whichever of
///   the two won that memoizer race. Read from the request it is a pure
///   function of the addr, so every frame agrees.
///
/// The at-most-one-terminal invariant that `Engine::result` and the
/// transparent-group gate enforce for bare `--shell` holds here by
/// construction: exactly one address matches, and every frame that is not it
/// resolves with no terminal at all.
pub struct ShellTarget {
    /// The address to enter. Matched by equality against every addr the request
    /// resolves.
    pub addr: Addr,
    /// `None` when the client has no terminal to hand over. The request is then
    /// refused up front — building without the shell would discard the flag.
    pub terminal: Option<crate::engine::result::InteractiveWrapper>,
}

impl std::fmt::Debug for ShellTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `InteractiveWrapper` is an `Arc<dyn Fn>`; its presence is the only part
        // that is meaningful to a reader anyway.
        f.debug_struct("ShellTarget")
            .field("addr", &self.addr)
            .field("terminal", &self.terminal.is_some())
            .finish()
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

/// The targets `--force` / `--no-scratch` reach on a selector run: every target
/// the run's selector matches, in a package the run's walk enumerates.
///
/// Force is a property of the request and the target, not of the call path. The
/// lock-and-execute step is single-flighted per addr and keeps whichever `force`
/// its first caller passed, and a dependency is always asked for unforced. So
/// when the walk — a provider's `get` building through a target to answer a
/// label check, say — or another selected target reaches a selected target as a
/// dependency first, a per-caller flag would serve it from cache and the
/// top-level forced call would find the cell already settled. Asking this
/// instead, wherever the target is first resolved, gives every caller of one
/// addr the same answer.
///
/// Deciding at first touch is the only sound place: an unforced cache hit keeps
/// a read lock on the target while its artifacts are in use, so forcing it later
/// would deadlock on its own write lock.
///
/// Only packages the walk can enumerate count. `//...` matches every address by
/// prefix, including targets the walk never lists (the go stdlib and third-party
/// modules under `@heph/go/...`): without the guard, `--force //...` would
/// rebuild the Go standard library and re-download every module.
///
/// The guard is per package, not per listed target, and that is deliberate:
/// inside a package the walk enumerates, a target the selector matches is
/// selected even when the provider resolves it without listing it — go's
/// `_lint-analyze@…` and `_go_mod`, reached as dependencies. Narrowing to listed
/// targets would make this check async and fallible (a memoized `list` per
/// package), which is exactly what installing the selection up front avoids. The
/// cost is rebuild time on a forced package selector, never a wrong result.
///
/// Not covered: members of a forced transparent group, which inherit `force`
/// through the call path as before.
#[derive(Debug)]
pub(crate) struct Selection {
    matcher: hmodel::htmatcher::Matcher,
    /// Each provider's package list for the walk, as
    /// `Engine::provider_packages` returns them: sorted byte-lexicographically,
    /// so membership is a binary search. A provider whose listing failed has no
    /// entry; the walk reports that failure itself.
    packages: Vec<Arc<Vec<String>>>,
    force: bool,
    no_scratch: bool,
}

impl Selection {
    pub(crate) fn new(
        matcher: hmodel::htmatcher::Matcher,
        packages: Vec<Arc<Vec<String>>>,
        force: bool,
        no_scratch: bool,
    ) -> Self {
        Self {
            matcher,
            packages,
            force,
            no_scratch,
        }
    }

    /// Whether the selector picks this target. The walk's own ladder — address,
    /// then spec, then def — on values the caller already holds, so it costs no
    /// resolution and cannot fail.
    pub(crate) fn selects(&self, addr: &Addr, spec: &TargetSpec, def: &TargetDef) -> bool {
        use hmodel::htmatcher::MatchResult;
        // The address alone rejects most dependencies of a scoped selector, so
        // it goes before the package search.
        let by_addr = self.matcher.matches_addr(addr);
        if by_addr == MatchResult::MatchNo {
            return false;
        }
        let pkg = addr.package.as_str();
        let enumerated = self
            .packages
            .iter()
            .any(|pkgs| pkgs.binary_search_by(|p| p.as_str().cmp(pkg)).is_ok());
        if !enumerated {
            return false;
        }
        if by_addr == MatchResult::MatchYes {
            return true;
        }
        match crate::engine::matcher_spec::match_spec(&self.matcher, spec) {
            MatchResult::MatchYes => true,
            MatchResult::MatchNo => false,
            MatchResult::MatchShrug => {
                crate::engine::matcher_target::match_target(&self.matcher, spec, def)
                    == MatchResult::MatchYes
            }
        }
    }

    /// The `(force, no_scratch)` a target is resolved with, given what its
    /// caller asked for. Only consults the selector when it would change the
    /// answer, so an unforced run, or a caller already forcing, pays nothing.
    pub(crate) fn effective(
        &self,
        force: bool,
        no_scratch: bool,
        addr: &Addr,
        spec: &TargetSpec,
        def: &TargetDef,
    ) -> (bool, bool) {
        let adds = (self.force && !force) || (self.no_scratch && !no_scratch);
        if !adds || !self.selects(addr, spec, def) {
            return (force, no_scratch);
        }
        tracing::debug!(
            %addr,
            force = self.force,
            no_scratch = self.no_scratch,
            "target is in the forced selection"
        );
        (force || self.force, no_scratch || self.no_scratch)
    }
}

/// Per-invocation state. Cheap to clone via with_parent — shares the same RequestStateData.
pub struct RequestState {
    pub data: Arc<RequestStateData>,
    /// Provider names excluded from query iteration for this request subtree.
    pub skip_providers: Arc<HashSet<String>>,
    /// The ancestor chain of this invocation (parent first), mirroring the
    /// `with_parent` stack. Always maintained so a [`speculative`] fork can seed
    /// its cycle check from the real ancestors at the fork point. Its head is
    /// [`parent`](RequestState::parent).
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

/// Sets `flag`, returning `true` to exactly one caller: the one that set it.
///
/// Loads before it swaps, so once the flag is up every later call — one per
/// edge, from every worker — is a shared read rather than an exclusive write
/// bouncing the cache line between cores.
fn once(flag: &std::sync::atomic::AtomicBool) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    !flag.load(Relaxed) && !flag.swap(true, Relaxed)
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

    /// Whether this request's query walks trust listed facts.
    pub fn listed_facts_trust(&self) -> crate::engine::listed::ListedFactsTrust {
        self.data.listed_facts_trust
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

    /// The request's count of sandbox removals still holding a `sandbox`
    /// slot, for the teardown whose removal carries one.
    pub(crate) fn sandbox_removals(&self) -> crate::engine::sandbox_cleaner::PendingCounter {
        Arc::clone(&self.data.sandbox_removals)
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

    /// The selector a forced selector run applies to, if one is installed. See
    /// [`Selection`].
    pub(crate) fn selection(&self) -> Option<&Selection> {
        self.data.selection.get()
    }

    /// Install the request's [`Selection`]. The first install wins: a nested
    /// `Engine::result` sharing this request must not re-scope a run that is
    /// already resolving targets under the first one. Refused on a
    /// [`hash_only`](Self::hash_only) request, where the force branch would
    /// re-execute a dependency and rewrite its blobs after dependents had
    /// already hashed its first output.
    pub(crate) fn install_selection(&self, selection: Selection) {
        if self.hash_only() {
            return;
        }
        if let Err(ignored) = self.data.selection.set(selection) {
            tracing::debug!(
                kept = %self.selection().map_or_else(String::new, |s| hmodel::htquery::format(&s.matcher)),
                ignored = %hmodel::htquery::format(&ignored.matcher),
                "request already carries a selection; keeping the first"
            );
        }
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

    /// Records a culprit's rich diagnostic. First-writer-wins: if `culprit`
    /// already has an entry (e.g. shared via the memoizer to multiple
    /// waiters, or a broken BUILD file reached through several of its
    /// targets), the existing one is kept. Whether this call was the one
    /// that recorded it: the claim to announce the failure.
    pub fn record_failure(&self, culprit: impl Into<Culprit>, failure: Arc<TargetFailure>) -> bool {
        match self.data.failures.lock().entry(culprit.into()) {
            indexmap::map::Entry::Occupied(_) => false,
            indexmap::map::Entry::Vacant(v) => {
                v.insert(failure);
                true
            }
        }
    }

    /// Drains and returns all recorded failures in insertion order, leaving the
    /// registry empty. Called once at the end of execution to render.
    pub fn take_failures(&self) -> Vec<Arc<TargetFailure>> {
        std::mem::take(&mut *self.data.failures.lock())
            .into_values()
            .collect()
    }

    /// [`take_failures`](Self::take_failures), each failure saying whether
    /// the engine can open its log ([`TargetFailure::log_readable`]): the
    /// log API over local sources, never fetching (a remote hit's log it
    /// would fetch counts: `heph log` does). What the report and the
    /// CI box decide `heph log <addr>` against, so neither points at a log
    /// `heph log` would not find, nor leaves out one it would. Without the
    /// engine (gone), the failures are returned unasked.
    pub async fn take_failures_with_logs(&self) -> Vec<Arc<TargetFailure>> {
        self.take_failures_with_logs_where(|_| true).await
    }

    /// [`take_failures_with_logs`](Self::take_failures_with_logs), asking
    /// only for the failures `wanted` picks; the rest are returned unasked.
    /// The CI end of run asks only for the failures it is about to print:
    /// those printed as they failed carried the answer then.
    pub async fn take_failures_with_logs_where(
        &self,
        wanted: impl Fn(&TargetFailure) -> bool,
    ) -> Vec<Arc<TargetFailure>> {
        use futures::StreamExt as _;
        let failures = self.take_failures();
        let Some(engine) = self.data.engine.upgrade() else {
            return failures;
        };
        futures::stream::iter(failures)
            .map(|f| {
                let (engine, wanted) = (&engine, &wanted);
                async move {
                    if !wanted(&f) {
                        return f;
                    }
                    let readable = engine.log_readable(&f.addr).await;
                    Arc::new(f.as_ref().clone().with_log_readable(readable))
                }
            })
            .buffered(crate::engine::fanout::discovery_concurrency())
            .collect()
            .await
    }

    /// Non-draining lookup of a single recorded failure by addr. Used by the
    /// outermost `result_addr` frame to surface the rich root-cause diagnostic
    /// to its direct caller in place of the `UpstreamFailed` marker.
    pub fn get_failure(&self, addr: &Addr) -> Option<Arc<TargetFailure>> {
        self.get_culprit_failure(&Culprit::Target(addr.clone()))
    }

    /// [`get_failure`](Self::get_failure) for any culprit, a file included.
    pub fn get_culprit_failure(&self, culprit: &Culprit) -> Option<Arc<TargetFailure>> {
        self.data.failures.lock().get(culprit).cloned()
    }

    /// Record `addr` as one this request asked for: where a failure's
    /// [`culprit_path`](Self::culprit_path) starts. Called for each matched
    /// target of the top-level selection, and for the one address of a
    /// single-target run.
    pub fn mark_requested(&self, addr: &Addr) {
        self.data.dep_dag.lock().mark_requested(addr);
    }

    /// The shortest path from a requested target to `addr` (a culprit, or
    /// the address that led into a broken file), inclusive; see
    /// [`DepDag::culprit_path`]. Takes only the `dep_dag` lock, never under
    /// the failures lock.
    pub fn culprit_path(&self, addr: &Addr) -> Vec<Addr> {
        self.data.dep_dag.lock().culprit_path(addr)
    }

    /// The request-end pass over `failures` (what the run recorded, e.g.
    /// from [`take_failures`](Self::take_failures)): each of `skipped`'s
    /// nearest culprit, path to it and count of other culprits reached; see
    /// [`DepDag::skipped_report`]. Run once, after the scheduler stopped
    /// adding edges; counted by [`report_passes`](Self::report_passes).
    pub fn skipped_report<'a>(
        &self,
        failures: impl IntoIterator<Item = &'a TargetFailure>,
        skipped: impl IntoIterator<Item = &'a Addr>,
    ) -> SkippedReport {
        self.data.report_passes.fetch_add(1, Ordering::Relaxed);
        self.data.dep_dag.lock().skipped_report(
            failures.into_iter().map(|f| (f.culprit(), &f.addr)),
            skipped,
        )
    }

    /// How many request-end passes this request ran.
    pub fn report_passes(&self) -> usize {
        self.data.report_passes.load(Ordering::Relaxed)
    }

    /// Commit `from → to` to the request's graph as a real resolution would.
    /// For tests that build a large graph without running it.
    #[cfg(any(test, feature = "test-support"))]
    pub fn add_dep_for_test(&self, from: &Addr, to: &Addr) -> Result<(), CycleError> {
        self.data.dep_dag.lock().add_dep(from, to)
    }

    /// Note that `_transitive_<hash>_…` origin ids name `addr`'s transitive
    /// declarations; `hash` is `addr.hash_str()`.
    pub(crate) fn note_transitive_declarer(&self, hash: &str, addr: &Addr) {
        let mut map = self.data.transitive_declarers.lock();
        if !map.contains_key(hash) {
            map.insert(hash.to_owned(), addr.clone());
        }
    }

    /// The target that declared a merged transitive input, from its origin
    /// id: the innermost `_transitive_<hash>_` segment this request noted
    /// (a group's merged transitive nests its member's), else the next one
    /// out. `None` for an id that is not a merged transitive's, or one whose
    /// declarer this request never merged.
    pub(crate) fn transitive_declarer(&self, origin_id: &str) -> Option<Addr> {
        const PREFIX: &str = "_transitive_";
        let map = self.data.transitive_declarers.lock();
        origin_id
            .rmatch_indices(PREFIX)
            .filter_map(|(at, _)| {
                let rest = origin_id.get(at + PREFIX.len()..)?;
                map.get(rest.split('_').next()?)
            })
            .next()
            .cloned()
    }

    /// Note that `addr` failed only because `root` did. Bounded per root: the
    /// first [`TookDown::NAMED`] addrs and a count, so a root that takes
    /// down thousands costs no more than one that takes down a few.
    pub fn record_took_down(&self, root: &str, addr: &str) {
        let mut map = self.data.took_down.lock();
        let entry = map.entry(root.to_owned()).or_default();
        entry.total = entry.total.saturating_add(1);
        if entry.named.len() < TookDown::NAMED {
            entry.named.push(addr.to_owned());
        }
    }

    /// What `root` took down, as [`record_took_down`](Self::record_took_down)
    /// noted it; empty when it took down nothing.
    pub fn took_down(&self, root: &str) -> TookDown {
        self.data
            .took_down
            .lock()
            .get(root)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether `addr`'s failing end, described by `d`, is a stop rather than
    /// a failure, and remember it if so. Decided once per end, here, so the
    /// event, the line and the report agree.
    ///
    /// A root is a stop when a [`CancelledError`](hplugin::error::CancelledError)
    /// is on its chain. Otherwise `execute` decided, once, before filling a
    /// failure slot: a run it kept ([`ErrorDetail::kept`]) failed, whatever
    /// the token says now — fail-fast may cancel it between that decision and
    /// this end. Only an error that never reached that decision falls back to
    /// the same rule `execute` applies: the request's token is cancelled and
    /// the run did not time out (a driver may report a stop as a bare
    /// `cancelled` of its own).
    ///
    /// [`ErrorDetail::kept`]: crate::engine::event::ErrorDetail::kept
    ///
    /// One that failed only because a dependency did is a stop when that
    /// root was one; a root that genuinely failed before the stop keeps what
    /// it took down. A root's end always precedes its dependents' (they end
    /// on its result), so the set is filled before they ask.
    pub fn end_is_cancelled(&self, addr: &str, d: &crate::engine::event::ErrorDetail) -> bool {
        let mut set = self.data.cancelled.lock();
        let cancelled = match &d.upstream_of {
            Some(root) => set.contains(root),
            None => {
                d.cancelled || (!d.kept && self.ctoken().is_cancelled() && d.timed_out.is_none())
            }
        };
        if cancelled {
            set.insert(addr.to_owned());
        }
        cancelled
    }

    /// Whether `addr`'s failure was a stop ([`end_is_cancelled`](Self::end_is_cancelled)).
    pub fn was_cancelled(&self, addr: &str) -> bool {
        self.data.cancelled.lock().contains(addr)
    }

    /// heph's home, while the engine is alive: what no screen or report
    /// shows (sandboxes live under it).
    pub fn heph_home(&self) -> Option<std::path::PathBuf> {
        self.data.engine.upgrade().map(|e| e.home.to_path_buf())
    }

    /// The workspace root, while the engine is alive: what a report's
    /// messages name paths relative to.
    pub fn workspace_root(&self) -> Option<std::path::PathBuf> {
        self.data.engine.upgrade().map(|e| e.root().to_path_buf())
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

    /// Whether anything receives this request's events: a stream or a hook.
    /// Event data that costs something to build is built only when it does.
    pub fn has_listeners(&self) -> bool {
        self.data.events.is_some() || !self.data.hooks.is_empty()
    }

    /// Start this request's `PoolUsage` sampler, once, on its first admission:
    /// call it before the acquire, so the admission is the first change it
    /// sends. A request nobody listens to starts none.
    pub(crate) fn sample_pool(&self, pool: &Arc<crate::engine::resource_pool::ResourcePool>) {
        if self.data.pool_sampler.get().is_some() || !self.has_listeners() {
            return;
        }
        let mut started = false;
        let sampler = self.data.pool_sampler.get_or_init(|| {
            started = true;
            PoolSampler::new(Arc::clone(pool))
        });
        if !started {
            return;
        }
        // Weak: the ticks must not keep the request alive, or its stream
        // would never end.
        let data = Arc::downgrade(&self.data);
        sampler.spawn(move |sampler| {
            let Some(data) = data.upgrade() else {
                return std::ops::ControlFlow::Break(());
            };
            for kind in sampler.sample() {
                data.dispatch(crate::engine::event::BuildEvent {
                    at_unix_ms: crate::engine::event::now_unix_ms(),
                    kind,
                });
            }
            std::ops::ControlFlow::Continue(())
        });
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
    ///
    /// `count` is the cpu capacity in CPUs; `pools` every pool's capacity
    /// (`None`: unbounded) and unit, called only by the announcing call.
    pub fn announce_request_config(
        &self,
        count: usize,
        scratch_disabled: bool,
        pools: impl FnOnce() -> PoolAnnouncement,
    ) {
        if once(&self.data.workers_announced) {
            let pools = pools();
            self.emit(crate::engine::event::BuildEventKind::RequestConfig {
                max_workers: count,
                fail_fast: self.data.fail_fast,
                scratch_disabled,
                resources: pools.capacities,
                units: pools.units,
            });
        }
    }

    /// Claims ownership of the `Matched` stream for the calling `result`
    /// invocation. Returns `true` exactly once per request (for the first/
    /// top-level call); every later call — including inner `result`s sharing
    /// this request's data — gets `false` and must not emit `Matched`.
    pub fn claim_matched_stream(&self) -> bool {
        once(&self.data.matched_announced)
    }

    /// The target `--shell=ADDR` named, if any. `None` for bare `--shell`, whose
    /// single-target rule is enforced by `Engine::result` and the
    /// transparent-group gate instead. Fixed at construction
    /// ([`Engine::new_state_with_shell`]).
    pub fn shell_target(&self) -> Option<&ShellTarget> {
        self.data.shell.as_ref()
    }

    /// Hands a cloned sender to `emit_scope`'s drop-guard.
    pub(crate) fn events_sender(&self) -> Option<crate::engine::event::EventSender> {
        self.data.events.clone()
    }

    /// The target that triggered this invocation, used for cycle detection in
    /// `result_addr`: the head of the breadcrumb chain.
    pub fn parent(&self) -> Option<&Addr> {
        self.crumbs.as_deref().map(|c| &c.addr)
    }

    /// Returns a child RequestState sharing the same data but with a new parent.
    pub fn with_parent(&self, parent: Addr) -> Arc<RequestState> {
        let crumbs = Some(Arc::new(Crumb {
            addr: parent,
            parent: self.crumbs.clone(),
        }));
        Arc::new(RequestState {
            data: Arc::clone(&self.data),
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
        } else if let Some(parent) = self.parent() {
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

impl RequestStateData {
    /// The request's sandbox removals return their slots after it ends: hand
    /// the stream to a task that reports them as they do
    /// ([`PoolSampler::trail`]), so the report it ends on is the pool's real
    /// state rather than the slots the removals still held. The stream stays
    /// open for at most [`REMOVALS_WAIT`] more; hooks are closed now as
    /// before, so they see the state at the request's end. With nothing
    /// pending, no stream, or no runtime to wait on, the stream ends here.
    ///
    /// [`REMOVALS_WAIT`]: crate::engine::resource_pool::sampler::REMOVALS_WAIT
    fn trail_removals(&mut self, sampler: &Arc<PoolSampler>) {
        use crate::engine::resource_pool::sampler::REMOVALS_WAIT;
        if self.sandbox_removals.load(Ordering::Acquire) == 0 {
            return;
        }
        let runtime = tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| self.engine.upgrade().map(|e| e.runtime.clone()));
        let Some(runtime) = runtime else {
            return;
        };
        let Some(tx) = self.events.take() else {
            return;
        };
        let sampler = Arc::clone(sampler);
        let removals = Arc::clone(&self.sandbox_removals);
        runtime.spawn(async move {
            sampler
                .trail(&removals, REMOVALS_WAIT, |kinds| {
                    for kind in kinds {
                        // Best-effort, as every send on this stream.
                        drop(tx.send(crate::engine::event::BuildEvent {
                            at_unix_ms: crate::engine::event::now_unix_ms(),
                            kind,
                        }));
                    }
                })
                .await;
        });
    }
}

impl Drop for RequestStateData {
    fn drop(&mut self) {
        // The pool's last change since the sampler's last tick, before the
        // stream ends: the trailing edge.
        if let Some(sampler) = self.pool_sampler.get().cloned() {
            for kind in sampler.finish() {
                self.dispatch(crate::engine::event::BuildEvent {
                    at_unix_ms: crate::engine::event::now_unix_ms(),
                    kind,
                });
            }
            self.trail_removals(&sampler);
        }
        self.ctoken.cancel();
        // Signal end-of-stream to each hook so it can flush its final state. The
        // host awaits the actual flush separately, at command teardown
        // (`Hook::drain`, from `bootstrap::block_on`).
        for hook in &self.hooks {
            hook.on_close();
        }
        if let Some(engine) = self.engine.upgrade() {
            if let Ok(mut requests) = engine.requests.lock() {
                requests.remove(&self.request_id);
            }
            if let Ok(mut tokens) = engine.request_ctokens.lock() {
                tokens.remove(&self.request_id);
            }
        }
        // The fs plugin's per-request memos are process-wide: drop this
        // request's buckets whether or not the engine is still alive.
        hbuiltins::pluginfs::forget_request(&self.request_id);
    }
}

/// The engine's live root requests, by id ([`Engine::requests`]).
pub type RequestRegistry =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Weak<RequestState>>>>;

/// Where the fs plugin's codegen reads report (see
/// [`hbuiltins::pluginfs::CodegenReport`]): an event goes on the stream of the
/// request whose id it names, and a key fires once per request. A read with no
/// request (a BUILD evaluation's `heph.fs.glob`), or one whose request is gone,
/// emits nothing and dedupes engine-wide.
///
/// Holds the request registry, not the engine: the fs driver that holds this
/// is owned by the engine, so a strong engine reference would be a cycle.
pub(crate) struct EngineCodegenReport {
    requests: RequestRegistry,
    /// Keys reported with no live request, for the engine's lifetime.
    unscoped: Mutex<FxHashSet<String>>,
}

impl EngineCodegenReport {
    pub(crate) fn new(requests: RequestRegistry) -> Self {
        Self {
            requests,
            unscoped: Mutex::new(FxHashSet::default()),
        }
    }

    fn request(&self, request_id: Option<&str>) -> Option<Arc<RequestState>> {
        let id = request_id?;
        let requests = self.requests.lock().ok()?;
        requests.get(id).and_then(Weak::upgrade)
    }
}

impl hbuiltins::pluginfs::CodegenReport for EngineCodegenReport {
    fn emit(&self, request_id: Option<&str>, kind: crate::engine::event::BuildEventKind) {
        if let Some(rs) = self.request(request_id) {
            rs.emit(kind);
        }
    }

    fn first_in_run(&self, request_id: Option<&str>, key: &str) -> bool {
        let first = |set: &Mutex<FxHashSet<String>>| {
            let mut set = set.lock();
            !set.contains(key) && set.insert(key.to_owned())
        };
        match self.request(request_id) {
            Some(rs) => first(&rs.data.codegen_reported),
            None => first(&self.unscoped),
        }
    }
}

/// How a request built by `Engine::new_state_inner` behaves beyond its
/// front-end options.
#[derive(Debug)]
struct RequestMode {
    /// See [`RequestStateData::hash_only`].
    hash_only: bool,
    listed_facts_trust: crate::engine::listed::ListedFactsTrust,
    /// See [`RequestStateData::shell`].
    shell: Option<ShellTarget>,
}

impl Engine {
    pub fn new_state(self: &Arc<Self>) -> Arc<RequestState> {
        self.new_state_with_fail_fast(true)
    }

    /// Default number of trailing process-log lines shown in a failure box when a
    /// caller (e.g. `gc`, non-`run` commands) does not override it (U5). The
    /// read is also capped at 256 KiB, whichever is less
    /// ([`hplugin::error::TAIL_MAX_BYTES`]).
    pub const DEFAULT_LOG_TAIL_LINES: usize = 1000;

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
            hcore::pending::Pending::new_counter(),
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
        self.new_state_with_shell(
            fail_fast,
            events,
            bg_pending,
            log_tail_lines,
            approval,
            None,
        )
    }

    /// [`new_state_full`](Self::new_state_full) for a request that may name a
    /// `--shell=ADDR` target.
    ///
    /// The shell target is an argument of construction, not something set
    /// afterwards, because the whole design rests on it being constant for the
    /// request's lifetime: "does this frame shell?" is then a pure function of
    /// the addr, and every `mem_result` cell of one addr feeds the addr-keyed
    /// `mem_locked_result` cell the same answer (see [`ShellTarget`]). A request
    /// that exists before its shell target is known could resolve a frame
    /// first.
    pub fn new_state_with_shell(
        self: &Arc<Self>,
        fail_fast: bool,
        events: Option<crate::engine::event::EventSender>,
        bg_pending: crate::engine::sandbox_cleaner::PendingCounter,
        log_tail_lines: usize,
        approval: Option<Arc<dyn crate::engine::approval::ApprovalHandler>>,
        shell: Option<ShellTarget>,
    ) -> Arc<RequestState> {
        self.new_state_inner(
            fail_fast,
            events,
            bg_pending,
            log_tail_lines,
            approval,
            RequestMode {
                hash_only: false,
                listed_facts_trust: self.cfg.listed_facts_trust,
                shell,
            },
        )
    }

    /// [`new_state`](Self::new_state) with this request's listed-facts trust
    /// chosen by the caller rather than taken from the engine's config.
    pub fn new_state_with_listed_facts_trust(
        self: &Arc<Self>,
        trust: crate::engine::listed::ListedFactsTrust,
    ) -> Arc<RequestState> {
        self.new_state_inner(
            true,
            None,
            hcore::pending::Pending::new_counter(),
            Self::DEFAULT_LOG_TAIL_LINES,
            None,
            RequestMode {
                hash_only: false,
                listed_facts_trust: trust,
                shell: None,
            },
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
    ///
    /// `listed_facts_trust` is the enclosing request's: a query target the
    /// nested request re-resolves must see the dep set the outer one did.
    pub fn new_hash_only_state(
        self: &Arc<Self>,
        parent: Addr,
        listed_facts_trust: crate::engine::listed::ListedFactsTrust,
    ) -> Arc<RequestState> {
        self.new_state_inner(
            true,
            None,
            hcore::pending::Pending::new_counter(),
            Self::DEFAULT_LOG_TAIL_LINES,
            None,
            RequestMode {
                hash_only: true,
                listed_facts_trust,
                // A hash-only request never builds, so it never enters anything.
                shell: None,
            },
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
        RequestMode {
            hash_only,
            listed_facts_trust,
            shell,
        }: RequestMode,
    ) -> Arc<RequestState> {
        // A request is past registration by construction (the engine is
        // shared), so every request starts with the function registry sealed —
        // whichever provider path it takes to a BUILD evaluation.
        self.seal_functions();
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
            mem_codegen_tree: Memoizer::with_tag_task("codegen_tree", self.runtime.clone()),
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
            listed_facts_trust,
            events,
            hooks: self.hooks(),
            frozen: std::sync::atomic::AtomicBool::new(false),
            codegen_reported: Mutex::new(FxHashSet::default()),
            workers_announced: std::sync::atomic::AtomicBool::new(false),
            pool_sampler: std::sync::OnceLock::new(),
            matched_announced: std::sync::atomic::AtomicBool::new(false),
            bg_pending: Arc::clone(&bg_pending),
            sandbox_removals: crate::engine::sandbox_cleaner::PendingCounter::default(),
            failures: Mutex::new(indexmap::IndexMap::new()),
            transitive_declarers: Mutex::new(FxHashMap::default()),
            report_passes: std::sync::atomic::AtomicUsize::new(0),
            took_down: Mutex::new(FxHashMap::default()),
            cancelled: Mutex::new(FxHashSet::default()),
            approval,
            hash_only,
            selection: std::sync::OnceLock::new(),
            shell,
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
            skip_providers: Arc::new(HashSet::new()),
            crumbs: None,
            speculative: false,
            tracked: Mutex::new(FxHashSet::default()),
        });

        if let Ok(mut tokens) = self.request_ctokens.lock() {
            tokens.insert(request_id.clone(), data.ctoken.clone());
        }
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

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("pkg"), name.to_string(), Default::default())
    }

    /// However many workers race on a once-flag, exactly one claims it.
    #[test]
    fn once_flag_is_claimed_exactly_once() {
        let flag = std::sync::atomic::AtomicBool::new(false);
        let claims = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16).map(|_| s.spawn(|| once(&flag))).collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("worker panicked"))
                .filter(|claimed| *claimed)
                .count()
        });
        assert_eq!(claims, 1);
        assert!(!once(&flag), "a set flag is never claimed again");
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
            parallelism: None,
            ..Config::for_tests(dir.path())
        })?);
        Ok((dir, engine))
    }

    fn bg_state(
        engine: &Arc<Engine>,
    ) -> (
        hcore::pending::PendingCounter,
        Arc<crate::engine::request_state::RequestState>,
    ) {
        let bg = hcore::pending::Pending::new_counter();
        let rs = engine.new_state_full(
            true,
            None,
            Arc::clone(&bg),
            Engine::DEFAULT_LOG_TAIL_LINES,
            None,
        );
        (bg, rs)
    }

    async fn wait_drained(bg: &hcore::pending::Pending) {
        tokio::time::timeout(std::time::Duration::from_secs(30), bg.wait_drained())
            .await
            .expect("background work never drained");
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
            .try_lock_target(&a)
            .expect("try_lock_target")
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

    /// `ord` is a permutation of the node indices and orders every committed
    /// edge forwards.
    fn assert_topological(dag: &DepDag) {
        let mut seen = vec![false; dag.ord.len()];
        for &o in &dag.ord {
            assert!(
                !std::mem::replace(&mut seen[o as usize], true),
                "ord repeats {o}"
            );
        }
        for (u, succ) in dag.succ.iter().enumerate() {
            for &v in succ {
                assert!(
                    dag.ord[u] < dag.ord[v as usize],
                    "edge {u}→{v} runs backwards: ord {} ≥ {}",
                    dag.ord[u],
                    dag.ord[v as usize]
                );
            }
        }
    }

    /// Whether `to` is reachable from `from` over committed edges.
    fn reaches(dag: &DepDag, from: u32, to: u32) -> bool {
        let mut seen = vec![false; dag.nodes.len()];
        let mut stack = vec![from];
        while let Some(n) = stack.pop() {
            if n == to {
                return true;
            }
            if !std::mem::replace(&mut seen[n as usize], true) {
                stack.extend(&dag.succ[n as usize]);
            }
        }
        false
    }

    #[test]
    fn test_dep_dag_interleaved_reorders_and_rejections_keep_a_topological_order() {
        // Back-edges, rejected cycles and forward edges interleaved on one DAG,
        // so each reorder starts from buffers a previous reorder — or a cycle
        // bail that returned mid-walk — left behind. The epoch starts just
        // short of wrapping, so the visited marks also go through the reset.
        let names: Vec<Addr> = (0..40).map(|i| addr(&format!("n{i}"))).collect();
        let mut dag = DepDag::new();
        dag.pk.epoch = u32::MAX - 3;
        let (mut reorders, mut rejections) = (0, 0);
        let mut rng = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..3000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let (a, b) = (
                &names[(rng % 40) as usize],
                &names[((rng >> 8) % 40) as usize],
            );
            let (ai, bi) = (dag.get_or_insert(a), dag.get_or_insert(b));
            let cycle = ai == bi || reaches(&dag, bi, ai);
            let back = dag.ord[ai as usize] >= dag.ord[bi as usize];
            let res = dag.add_dep(a, b);
            assert_eq!(res.is_err(), cycle, "{a} → {b}: wrong verdict");
            if cycle {
                rejections += 1;
            } else if back {
                reorders += 1;
            }
            assert_topological(&dag);
        }
        assert!(
            reorders > 10 && rejections > 10,
            "{reorders} reorders, {rejections} rejections"
        );
        assert!(dag.pk.epoch < u32::MAX - 3, "the epoch never wrapped");
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

    /// The exit-time drain (R27): the command returned and dropped its root
    /// state, but a remote upload still rides a child of it. A Ctrl-C then
    /// must still reach that upload's token, or heph waits out the upload's
    /// deadline. The token entry goes with the data, not with the root.
    #[tokio::test]
    async fn cancel_all_requests_reaches_work_outliving_the_root_state() -> anyhow::Result<()> {
        let (_tmp, engine) = test_engine()?;
        let rs = engine.new_state();
        let id = rs.request_id().to_string();
        let child = rs.with_parent(hmodel::htaddr::parse_addr("//app:dep")?);
        drop(rs);
        assert!(!child.ctoken().is_cancelled());

        engine.cancel_all_requests();
        assert!(
            child.ctoken().is_cancelled(),
            "the child's work is cancelled"
        );

        drop(child);
        assert!(
            !engine
                .request_ctokens
                .lock()
                .map_err(|_poisoned| anyhow::anyhow!("poisoned"))?
                .contains_key(&id),
            "the token goes with the data"
        );
        Ok(())
    }

    /// A failing end is a stop when Ctrl-C cancelled the request (a driver's
    /// bare `cancelled` included) and it did not time out; a dependent is one
    /// only when its root was; a root that failed before the stop keeps what
    /// it took down.
    #[tokio::test]
    async fn end_is_cancelled_tells_a_stop_from_a_failure() -> anyhow::Result<()> {
        use crate::engine::event::ErrorDetail;
        let (_tmp, engine) = test_engine()?;
        let rs = engine.new_state();
        let detail = |upstream_of: Option<&str>| ErrorDetail {
            message: "boom".into(),
            upstream_of: upstream_of.map(str::to_owned),
            ..ErrorDetail::default()
        };

        assert!(!rs.end_is_cancelled("//p:early", &detail(None)));
        assert!(
            rs.end_is_cancelled(
                "//p:typed",
                &ErrorDetail::from_error(&anyhow::Error::new(hplugin::error::CancelledError)),
            ),
            "a CancelledError is a stop whatever the token says"
        );

        engine.cancel_all_requests();
        let timed_out = ErrorDetail {
            timed_out: Some(hcore::events::TimedOutData {
                timeout_ms: 1,
                abandoned: false,
            }),
            ..detail(None)
        };
        assert!(
            !rs.end_is_cancelled("//p:slow", &timed_out),
            "a timeout fails"
        );
        assert!(
            !rs.end_is_cancelled(
                "//p:kept",
                &ErrorDetail {
                    kept: true,
                    ..detail(None)
                }
            ),
            "execute kept it as a failure before the cancel: it stays one"
        );
        assert!(rs.end_is_cancelled("//p:bare", &detail(None)));
        assert!(rs.end_is_cancelled("//p:behind", &detail(Some("//p:bare"))));
        assert!(
            !rs.end_is_cancelled("//p:down", &detail(Some("//p:early"))),
            "collateral of a real root stays collateral"
        );
        assert!(rs.was_cancelled("//p:bare") && rs.was_cancelled("//p:behind"));
        assert!(!rs.was_cancelled("//p:early") && !rs.was_cancelled("//p:down"));
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
            parallelism: None,
            ..Config::for_tests(dir.path())
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
            parallelism: None,
            ..Config::for_tests(dir.path())
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
                duplicate: false,
                left_behind: None,
                cancelled: false,
                addr: "//a:b".into(),
                error: error.map(crate::engine::event::ErrorDetail::into_message),
                upstream_of: None,
                upstream_file: None,
                path: Vec::new(),
                exit_status: None,
                exit_code: None,
                log_tail: None,
                timed_out: None,
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

    /// The `DepDag` walks behind a failure's `path`, `root` and `more` (D1.e).
    mod walks {
        use super::*;

        fn dag(edges: &[(&str, &str)], requested: &[&str]) -> DepDag {
            let mut dag = DepDag::new();
            for (from, to) in edges {
                dag.add_dep(&addr(from), &addr(to))
                    .expect("acyclic fixture");
            }
            for r in requested {
                dag.mark_requested(&addr(r));
            }
            dag
        }

        fn names(path: &[Addr]) -> Vec<String> {
            path.iter().map(|a| a.name.to_string()).collect()
        }

        fn report(dag: &DepDag, culprits: &[&str], skipped: &[&str]) -> SkippedReport {
            let culprits: Vec<Addr> = culprits.iter().map(|c| addr(c)).collect();
            let skipped: Vec<Addr> = skipped.iter().map(|s| addr(s)).collect();
            dag.skipped_report(
                culprits.iter().map(|c| (Culprit::from(c.clone()), c)),
                &skipped,
            )
        }

        fn record(r: &SkippedReport, s: &str) -> (String, Vec<String>, usize) {
            let rec = r.get(&addr(s)).expect("a skipped target with a record");
            (rec.root.to_string(), names(&rec.path), rec.more)
        }

        #[test]
        fn requested_culprit_path_is_itself() {
            let d = dag(&[("top", "c")], &["top", "c"]);
            assert_eq!(names(&d.culprit_path(&addr("c"))), ["c"]);
        }

        #[test]
        fn shortest_path_wins_over_a_longer_one() {
            let d = dag(&[("r", "a"), ("a", "b"), ("b", "c"), ("r", "c")], &["r"]);
            assert_eq!(names(&d.culprit_path(&addr("c"))), ["r", "c"]);
        }

        /// The diamond's two equal paths: the one through the smaller
        /// address, whichever edge came first.
        #[test]
        fn diamond_path_goes_through_the_smaller_neighbour() {
            let right_first = dag(
                &[
                    ("top", "right"),
                    ("top", "left"),
                    ("right", "leaf"),
                    ("left", "leaf"),
                ],
                &["top"],
            );
            let left_first = dag(
                &[
                    ("left", "leaf"),
                    ("right", "leaf"),
                    ("top", "left"),
                    ("top", "right"),
                ],
                &["top"],
            );
            for d in [&right_first, &left_first] {
                assert_eq!(
                    names(&d.culprit_path(&addr("leaf"))),
                    ["top", "left", "leaf"]
                );
            }
        }

        #[test]
        fn several_requested_at_equal_distance_start_at_the_smallest() {
            let d = dag(&[("r2", "x"), ("r1", "x"), ("x", "c")], &["r2", "r1"]);
            assert_eq!(names(&d.culprit_path(&addr("c"))), ["r1", "x", "c"]);
        }

        /// Distance first: a larger requested address one hop away beats a
        /// smaller one two hops away.
        #[test]
        fn a_nearer_requested_target_beats_a_smaller_farther_one() {
            let d = dag(&[("z", "c"), ("a", "m"), ("m", "c")], &["z", "a"]);
            assert_eq!(names(&d.culprit_path(&addr("c"))), ["z", "c"]);
        }

        #[test]
        fn unrequested_culprit_has_no_path() {
            let d = dag(&[("p", "c")], &[]);
            assert!(d.culprit_path(&addr("c")).is_empty());
            assert!(
                d.culprit_path(&addr("absent")).is_empty(),
                "an address the graph never saw has no path"
            );
        }

        /// The graph is acyclic by construction: a rejected back-edge is never
        /// recorded, so a walk over what remains terminates with the same
        /// answer as before the attempt.
        #[test]
        fn a_rejected_cycle_edge_leaves_the_walks_unchanged() {
            let mut d = dag(&[("r", "a"), ("a", "c")], &["r"]);
            assert!(d.add_dep(&addr("c"), &addr("r")).is_err());
            assert_eq!(names(&d.culprit_path(&addr("c"))), ["r", "a", "c"]);
            let r = report(&d, &["c"], &["r", "a"]);
            assert_eq!(
                record(&r, "r"),
                ("//pkg:c".into(), names_of(&["r", "a", "c"]), 0)
            );
        }

        fn names_of(n: &[&str]) -> Vec<String> {
            n.iter().map(|s| (*s).to_string()).collect()
        }

        #[test]
        fn skipped_nearest_culprit_is_its_root() {
            // s reaches c2 in one hop and c1 in two.
            let d = dag(&[("s", "c2"), ("s", "m"), ("m", "c1")], &["s"]);
            let r = report(&d, &["c1", "c2"], &["s", "m"]);
            assert_eq!(
                record(&r, "s"),
                ("//pkg:c2".into(), names_of(&["s", "c2"]), 1)
            );
            assert_eq!(
                record(&r, "m"),
                ("//pkg:c1".into(), names_of(&["m", "c1"]), 0)
            );
        }

        #[test]
        fn equal_distance_culprits_tie_by_byte_order() {
            // Inserted larger-first, so neither edge order nor insertion
            // order can be what picks c1.
            let d = dag(&[("s", "y"), ("y", "c2"), ("s", "x"), ("x", "c1")], &["s"]);
            let r = report(&d, &["c2", "c1"], &["s"]);
            assert_eq!(
                record(&r, "s"),
                ("//pkg:c1".into(), names_of(&["s", "x", "c1"]), 1)
            );
        }

        /// The root's tie-break wins over the next hop's: through `a` lies
        /// the larger culprit, through `b` the smaller.
        #[test]
        fn root_tie_beats_the_hops_address_order() {
            let d = dag(&[("s", "a"), ("a", "c2"), ("s", "b"), ("b", "c1")], &["s"]);
            let r = report(&d, &["c1", "c2"], &["s"]);
            assert_eq!(
                record(&r, "s"),
                ("//pkg:c1".into(), names_of(&["s", "b", "c1"]), 1)
            );
        }

        #[test]
        fn diamond_counts_its_one_culprit_once() {
            let d = dag(
                &[
                    ("top", "right"),
                    ("top", "left"),
                    ("right", "leaf"),
                    ("left", "leaf"),
                ],
                &["top"],
            );
            let r = report(&d, &["leaf"], &["top", "left", "right"]);
            assert_eq!(
                record(&r, "top"),
                ("//pkg:leaf".into(), names_of(&["top", "left", "leaf"]), 0)
            );
            assert_eq!(r.len(), 3);
        }

        /// A culprit reached through two branches counts once; a second one
        /// below just one branch counts too.
        #[test]
        fn more_is_an_exact_count_of_distinct_culprits() {
            let d = dag(
                &[
                    ("s", "a"),
                    ("a", "c1"),
                    ("s", "b"),
                    ("b", "c1"),
                    ("b", "c2"),
                ],
                &["s"],
            );
            let r = report(&d, &["c1", "c2"], &["s", "a", "b"]);
            assert_eq!(record(&r, "s").2, 1);
            assert_eq!(record(&r, "a").2, 0);
            assert_eq!(record(&r, "b").2, 1);
        }

        /// Past 64 culprits the bitset spans several words.
        #[test]
        fn more_counts_past_one_bitset_word() {
            let names: Vec<String> = (0..70).map(|i| format!("c{i:02}")).collect();
            let edges: Vec<(&str, &str)> = names.iter().map(|c| ("s", c.as_str())).collect();
            let d = dag(&edges, &["s"]);
            let culprits: Vec<&str> = names.iter().map(String::as_str).collect();
            let r = report(&d, &culprits, &["s"]);
            assert_eq!(
                record(&r, "s"),
                ("//pkg:c00".into(), names_of(&["s", "c00"]), 69)
            );
        }

        #[test]
        fn a_target_reaching_no_culprit_has_no_record() {
            let d = dag(&[("s", "c"), ("other", "fine")], &["s", "other"]);
            let r = report(&d, &["c"], &["s", "other", "never-seen"]);
            assert!(r.get(&addr("other")).is_none());
            assert!(r.get(&addr("never-seen")).is_none());
            assert_eq!(r.len(), 1);
        }

        /// A file culprit is recorded at the address that led into it: the
        /// path ends there and the root is the file.
        #[test]
        fn file_culprit_path_ends_at_its_entry() {
            let d = dag(&[("s", "entry")], &["s"]);
            let entry = addr("entry");
            let skipped = [addr("s")];
            let r = d.skipped_report([(Culprit::File(Arc::from("d/BUILD")), &entry)], &skipped);
            assert_eq!(
                record(&r, "s"),
                ("d/BUILD".into(), names_of(&["s", "entry"]), 0)
            );
        }
    }
}
