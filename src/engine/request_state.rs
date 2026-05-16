use crate::engine::Engine;
use crate::engine::local_cache::CacheArtifact;
use crate::engine::meta::ResultMeta;
use crate::engine::provider::TargetSpec;
use crate::engine::result::{ArtifactMeta, ExtendedTargetDef};
use crate::hasync::StdCancellationToken;
use crate::hmemoizer::Memoizer;
use crate::htaddr::Addr;
use daggy::petgraph::graph::NodeIndex;
use daggy::{Dag, WouldCycle};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;

type ArcErr = Arc<anyhow::Error>;
type ExecuteCacheResult = Result<(Vec<CacheArtifact>, Vec<ArtifactMeta>), ArcErr>;

pub struct DepDag {
    dag: Dag<Addr, ()>,
    nodes: HashMap<Addr, NodeIndex>,
}

impl DepDag {
    fn new() -> Self {
        Self {
            dag: Dag::new(),
            nodes: HashMap::new(),
        }
    }

    pub fn add_dep(&mut self, from: &Addr, to: &Addr) -> Result<(), WouldCycle<()>> {
        let from_idx = self.get_or_insert(from);
        let to_idx = self.get_or_insert(to);
        self.dag.add_edge(from_idx, to_idx, ())?;
        Ok(())
    }

    fn get_or_insert(&mut self, addr: &Addr) -> NodeIndex {
        if let Some(&idx) = self.nodes.get(addr) {
            idx
        } else {
            let idx = self.dag.add_node(addr.clone());
            self.nodes.insert(addr.clone(), idx);
            idx
        }
    }
}

/// Shared mutable state for a request — common across all child RequestStates.
pub struct RequestStateData {
    pub dep_dag: Mutex<DepDag>,
    pub mem_result: Memoizer<String, Result<crate::engine::result::EResult, ArcErr>>,
    pub mem_execute_cache: Memoizer<String, ExecuteCacheResult>,
    pub mem_meta: Memoizer<String, Result<ResultMeta, ArcErr>>,
    pub mem_spec: Memoizer<String, Result<Arc<TargetSpec>, ArcErr>>,
    pub mem_def: Memoizer<String, Result<Arc<ExtendedTargetDef>, ArcErr>>,
}

/// Per-invocation state. Cheap to clone via with_parent — shares the same RequestStateData.
pub struct RequestState {
    pub engine: Weak<Engine>,
    pub request_id: String,
    pub ctoken: StdCancellationToken,
    pub data: Arc<RequestStateData>,
    /// The target that triggered this invocation, used for cycle detection in result_addr.
    pub parent: Option<Addr>,
    /// Provider names excluded from query iteration for this request subtree.
    pub skip_providers: Arc<HashSet<String>>,
}

impl RequestState {
    /// Returns a child RequestState sharing the same data but with a new parent.
    pub fn with_parent(&self, parent: Addr) -> Arc<RequestState> {
        Arc::new(RequestState {
            engine: self.engine.clone(),
            request_id: self.request_id.clone(),
            ctoken: self.ctoken.clone(),
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
            engine: self.engine.clone(),
            request_id: self.request_id.clone(),
            ctoken: self.ctoken.clone(),
            data: Arc::clone(&self.data),
            parent: self.parent.clone(),
            skip_providers: Arc::new(set),
        })
    }
}

impl Drop for RequestState {
    fn drop(&mut self) {
        // Child states (parent.is_some()) share the root's ctoken and request entry —
        // only the root state should perform cleanup.
        if self.parent.is_some() {
            return;
        }
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
        let state = Arc::new(RequestState {
            engine: Arc::downgrade(self),
            request_id: request_id.clone(),
            ctoken: StdCancellationToken::new(),
            data: Arc::new(RequestStateData {
                dep_dag: Mutex::new(DepDag::new()),
                mem_execute_cache: Memoizer::new(),
                mem_result: Memoizer::new(),
                mem_meta: Memoizer::new(),
                mem_spec: Memoizer::new(),
                mem_def: Memoizer::new(),
            }),
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
        let request_id = rs.request_id.clone();

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
}
