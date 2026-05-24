use crate::engine::Engine;
use crate::engine::error::CycleError;
use crate::engine::local_cache::CacheArtifact;
use crate::engine::meta::ResultMeta;
use crate::engine::result::{ArtifactMeta, ExtendedTargetDef, OutputMatcher};
use crate::engine::spec::EngineTargetSpec;
use crate::hasync::StdCancellationToken;
use crate::hmemoizer::Memoizer;
use crate::htaddr::{Addr, AddrInner};
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashSet;
use std::ops::Deref;
use std::sync::{Arc, Weak};

type ArcErr = Arc<anyhow::Error>;
type ExecuteCacheResult = Result<(Vec<CacheArtifact>, Vec<ArtifactMeta>), ArcErr>;

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
    pub mem_result:
        Memoizer<(Addr, OutputMatcher), Result<Arc<crate::engine::result::EResult>, ArcErr>>,
    pub mem_execute_cache: Memoizer<(Addr, String), ExecuteCacheResult>,
    pub mem_meta: Memoizer<Addr, Result<ResultMeta, ArcErr>>,
    pub mem_spec: Memoizer<Addr, Result<Arc<EngineTargetSpec>, ArcErr>>,
    pub mem_def: Memoizer<Addr, Result<Arc<ExtendedTargetDef>, ArcErr>>,
    pub mem_expanded_inputs:
        Memoizer<Addr, Result<Arc<Vec<crate::engine::driver::targetdef::Input>>, ArcErr>>,
    pub mem_packages: Memoizer<String, Result<Arc<Vec<String>>, ArcErr>>,
}

/// Per-invocation state. Cheap to clone via with_parent — shares the same RequestStateData.
pub struct RequestState {
    pub data: Arc<RequestStateData>,
    /// The target that triggered this invocation, used for cycle detection in result_addr.
    pub parent: Option<Addr>,
    /// Provider names excluded from query iteration for this request subtree.
    pub skip_providers: Arc<HashSet<String>>,
}

impl RequestState {
    pub fn request_id(&self) -> &String {
        &self.data.request_id
    }

    pub fn ctoken(&self) -> &StdCancellationToken {
        &self.data.ctoken
    }

    /// Returns a child RequestState sharing the same data but with a new parent.
    pub fn with_parent(&self, parent: Addr) -> Arc<RequestState> {
        Arc::new(RequestState {
            data: Arc::clone(&self.data),
            parent: Some(parent),
            skip_providers: Arc::clone(&self.skip_providers),
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
        })
    }
}

impl Drop for RequestStateData {
    fn drop(&mut self) {
        self.ctoken.cancel();
        if let Some(engine) = self.engine.upgrade()
            && let Ok(mut requests) = engine.requests.lock()
        {
            requests.remove(&self.request_id);
        }
    }
}

impl Engine {
    pub fn new_state(self: &Arc<Self>) -> Arc<RequestState> {
        let request_id = "".to_string();
        let data = Arc::new(RequestStateData {
            engine: Arc::downgrade(self),
            request_id: request_id.clone(),
            ctoken: StdCancellationToken::new(),
            dep_dag: Mutex::new(DepDag::new()),
            mem_execute_cache: Memoizer::with_tag("execute_cache"),
            mem_result: Memoizer::with_tag("result"),
            mem_meta: Memoizer::with_tag("meta"),
            mem_spec: Memoizer::with_tag("spec"),
            mem_def: Memoizer::with_tag("def"),
            mem_expanded_inputs: Memoizer::with_tag("expanded_inputs"),
            mem_packages: Memoizer::with_tag("packages"),
        });

        let state = Arc::new(RequestState {
            data: Arc::clone(&data),
            parent: None,
            skip_providers: Arc::new(HashSet::new()),
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
    use crate::htpkg::PkgBuf;
    use std::path::PathBuf;

    fn addr(name: &str) -> Addr {
        Addr::new(PkgBuf::from("pkg"), name.to_string(), Default::default())
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
        let engine = Arc::new(Engine::new(Config {
            root: PathBuf::from("/tmp"),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?);

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
        let engine = Arc::new(Engine::new(Config {
            root: PathBuf::from("/tmp"),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?);

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
    async fn test_skip_provider_child_does_not_cancel_token() -> anyhow::Result<()> {
        let engine = Arc::new(Engine::new(Config {
            root: PathBuf::from("/tmp"),
            home_dir: std::path::PathBuf::new(),
            parallelism: None,
            ..Default::default()
        })?);

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
}
