use crate::engine::Engine;
use crate::engine::local_cache::CacheArtifact;
use crate::engine::meta::ResultMeta;
use crate::engine::provider::TargetSpec;
use crate::engine::result::{ArtifactMeta, ExtendedTargetDef};
use crate::hasync::StdCancellationToken;
use crate::hmemoizer::Memoizer;
use crate::htaddr::Addr;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

type ArcErr = Arc<anyhow::Error>;
type ExecuteCacheResult = Result<(Vec<CacheArtifact>, Vec<ArtifactMeta>), ArcErr>;

/// Returned by [`DepDag::add_dep`] when adding the edge would form a cycle.
#[derive(Debug)]
pub struct WouldCycle;

/// Per-request dep graph used for cross-task cycle detection.
///
/// Stored as adjacency list `from -> children` directly — much cheaper than the
/// previous petgraph/daggy stack (which kept a NodeIndex side-table and grew its
/// internal Graph buffers on every edge). The whole struct lives behind a single
/// `std::sync::Mutex` rather than the previous `tokio::sync::Mutex`: every
/// operation is sync + short, so the async mutex was paying for waker machinery
/// it never used.
pub struct DepDag {
    edges: HashMap<Addr, Vec<Addr>>,
}

impl DepDag {
    fn new() -> Self {
        Self {
            edges: HashMap::new(),
        }
    }

    /// Adds `from -> to`. Returns [`WouldCycle`] if `to` already reaches `from`
    /// (i.e. inserting the edge would close a cycle). The check and the insert
    /// happen under the same lock so concurrent adders cannot both observe
    /// "no cycle" and then race in two edges that together form one.
    pub fn add_dep(&mut self, from: &Addr, to: &Addr) -> Result<(), WouldCycle> {
        if from == to || self.reaches(to, from) {
            return Err(WouldCycle);
        }
        self.edges.entry(from.clone()).or_default().push(to.clone());
        Ok(())
    }

    /// DFS: does `start` reach `target` via the recorded edges?
    fn reaches(&self, start: &Addr, target: &Addr) -> bool {
        let Some(initial) = self.edges.get(start) else {
            return false;
        };
        let mut stack: Vec<&Addr> = initial.iter().collect();
        let mut visited: HashSet<&Addr> = HashSet::new();
        while let Some(node) = stack.pop() {
            if node == target {
                return true;
            }
            if !visited.insert(node) {
                continue;
            }
            if let Some(children) = self.edges.get(node) {
                stack.extend(children.iter());
            }
        }
        false
    }
}

/// Shared mutable state for a request — common across all child RequestStates.
pub struct RequestStateData {
    pub engine: Weak<Engine>,
    pub request_id: String,
    pub ctoken: StdCancellationToken,
    pub dep_dag: Mutex<DepDag>,
    pub mem_result: Memoizer<String, Result<Arc<crate::engine::result::EResult>, ArcErr>>,
    pub mem_execute_cache: Memoizer<String, ExecuteCacheResult>,
    pub mem_meta: Memoizer<Addr, Result<ResultMeta, ArcErr>>,
    pub mem_spec: Memoizer<Addr, Result<Arc<TargetSpec>, ArcErr>>,
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
        Addr {
            package: PkgBuf::from("pkg"),
            name: name.to_string(),
            args: Default::default(),
        }
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

    #[tokio::test]
    async fn test_request_state_tracking() -> anyhow::Result<()> {
        let engine = Arc::new(Engine::new(Config {
            root: PathBuf::from("/tmp"),
            parallelism: None,
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
    async fn test_skip_provider_child_does_not_cancel_token() -> anyhow::Result<()> {
        let engine = Arc::new(Engine::new(Config {
            root: PathBuf::from("/tmp"),
            parallelism: None,
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
