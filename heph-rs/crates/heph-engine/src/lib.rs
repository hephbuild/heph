//! Build execution engine for Heph

use heph_dag::{Dag, DagError};
use heph_kv::KvStore;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Semaphore;

#[derive(Error, Debug)]
pub enum EngineError {
    #[error("DAG error: {0}")]
    Dag(#[from] DagError),

    #[error("Cache error: {0}")]
    Cache(String),

    #[error("Execution error for target {target}: {message}")]
    Execution { target: String, message: String },

    #[error("Target not found: {0}")]
    TargetNotFound(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Target to be executed
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub name: String,
    pub hash: String,
}

impl Target {
    pub fn new(name: impl Into<String>, hash: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            hash: hash.into(),
        }
    }
}

/// Result of target execution
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionResult {
    /// Target was executed successfully
    Success { target: String, cached: bool },

    /// Target execution failed
    Failed { target: String, error: String },

    /// Target was skipped (e.g., up-to-date)
    Skipped { target: String },
}

impl ExecutionResult {
    pub fn target(&self) -> &str {
        match self {
            ExecutionResult::Success { target, .. } => target,
            ExecutionResult::Failed { target, .. } => target,
            ExecutionResult::Skipped { target } => target,
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, ExecutionResult::Success { .. })
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, ExecutionResult::Failed { .. })
    }
}

/// Execution function type
pub type ExecuteFn = Arc<dyn Fn(&Target) -> Result<Vec<u8>> + Send + Sync>;

/// Build execution engine
pub struct Engine<K: KvStore> {
    dag: Arc<Dag<Target>>,
    cache: Arc<K>,
    parallelism: usize,
    execute_fn: ExecuteFn,
}

impl<K: KvStore + 'static> Engine<K> {
    /// Create a new engine
    pub fn new(dag: Dag<Target>, cache: K, parallelism: usize) -> Self {
        Self {
            dag: Arc::new(dag),
            cache: Arc::new(cache),
            parallelism,
            execute_fn: Arc::new(|_| Ok(Vec::new())),
        }
    }

    /// Set custom execution function
    pub fn with_execute_fn(mut self, execute_fn: ExecuteFn) -> Self {
        self.execute_fn = execute_fn;
        self
    }

    /// Execute targets in topological order
    pub async fn execute(&self, targets: Vec<Target>) -> Result<Vec<ExecutionResult>> {
        // Verify all targets exist in DAG
        for target in &targets {
            if !self.dag.contains(target) {
                return Err(EngineError::TargetNotFound(target.name.clone()));
            }
        }

        // Get topological order
        let order = self.dag.topological_order();

        // Filter to only requested targets and their dependencies
        let mut to_execute: Vec<Target> = Vec::new();
        let mut seen = HashMap::new();

        for target in &targets {
            self.collect_dependencies(target, &mut to_execute, &mut seen)?;
        }

        // Sort by topological order
        to_execute.sort_by_key(|t| {
            order
                .iter()
                .position(|x| x == t)
                .unwrap_or(usize::MAX)
        });

        // Execute targets with limited parallelism
        let semaphore = Arc::new(Semaphore::new(self.parallelism));
        let mut results = Vec::new();

        for target in to_execute {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            let result = self.execute_target(&target).await;
            drop(permit);
            results.push(result);
        }

        Ok(results)
    }

    /// Collect target and all its dependencies
    fn collect_dependencies(
        &self,
        target: &Target,
        result: &mut Vec<Target>,
        seen: &mut HashMap<Target, ()>,
    ) -> Result<()> {
        if seen.contains_key(target) {
            return Ok(());
        }

        seen.insert(target.clone(), ());

        // Add dependents first (nodes that must run before this target)
        // dependents() returns incoming edges - nodes that point to this target
        let deps = self.dag.dependents(target)?;
        for dep in deps {
            self.collect_dependencies(&dep, result, seen)?;
        }

        result.push(target.clone());
        Ok(())
    }

    /// Execute a single target
    async fn execute_target(&self, target: &Target) -> ExecutionResult {
        // Check cache
        let cache_key = format!("target:{}:{}", target.name, target.hash);
        match self.cache.exists(cache_key.as_bytes()) {
            Ok(true) => {
                return ExecutionResult::Success {
                    target: target.name.clone(),
                    cached: true,
                }
            }
            Ok(false) => {}
            Err(e) => {
                return ExecutionResult::Failed {
                    target: target.name.clone(),
                    error: format!("Cache check failed: {}", e),
                }
            }
        }

        // Execute target
        match (self.execute_fn)(target) {
            Ok(output) => {
                // Store in cache
                if let Err(e) = self.cache.set(cache_key.as_bytes(), &output) {
                    return ExecutionResult::Failed {
                        target: target.name.clone(),
                        error: format!("Cache write failed: {}", e),
                    };
                }

                ExecutionResult::Success {
                    target: target.name.clone(),
                    cached: false,
                }
            }
            Err(e) => ExecutionResult::Failed {
                target: target.name.clone(),
                error: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heph_kv::MemoryKvStore;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_execute_single_target() {
        let mut dag = Dag::new();
        let target = Target::new("test", "hash1");
        dag.add_node(target.clone()).unwrap();

        let cache = MemoryKvStore::new();
        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 1);

        let results: Vec<ExecutionResult> = engine.execute(vec![target.clone()]).await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].is_success());
        assert_eq!(results[0].target(), "test");
    }

    #[tokio::test]
    async fn test_execute_with_dependencies() {
        let mut dag = Dag::new();

        let target_a = Target::new("a", "hash_a");
        let target_b = Target::new("b", "hash_b");
        let target_c = Target::new("c", "hash_c");

        dag.add_node(target_a.clone()).unwrap();
        dag.add_node(target_b.clone()).unwrap();
        dag.add_node(target_c.clone()).unwrap();

        // c depends on a and b (a and b must run before c)
        dag.add_edge(&target_a, &target_c).unwrap();
        dag.add_edge(&target_b, &target_c).unwrap();

        let cache = MemoryKvStore::new();
        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 2);

        // Execute only c, should also execute a and b
        let results: Vec<ExecutionResult> = engine.execute(vec![target_c.clone()]).await.unwrap();

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r: &ExecutionResult| r.is_success()));

        // Verify execution order: a and b before c
        let names: Vec<&str> = results.iter().map(|r: &ExecutionResult| r.target()).collect();
        let c_pos = names.iter().position(|&x| x == "c").unwrap();
        let a_pos = names.iter().position(|&x| x == "a").unwrap();
        let b_pos = names.iter().position(|&x| x == "b").unwrap();

        assert!(a_pos < c_pos);
        assert!(b_pos < c_pos);
    }

    #[tokio::test]
    async fn test_cache_hit() {
        let mut dag = Dag::new();
        let target = Target::new("test", "hash1");
        dag.add_node(target.clone()).unwrap();

        let cache = MemoryKvStore::new();
        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 1);

        // First execution
        let results1: Vec<ExecutionResult> = engine.execute(vec![target.clone()]).await.unwrap();
        assert!(!matches!(
            results1[0],
            ExecutionResult::Success { cached: true, .. }
        ));

        // Second execution should hit cache
        let results2: Vec<ExecutionResult> = engine.execute(vec![target.clone()]).await.unwrap();
        assert!(matches!(
            results2[0],
            ExecutionResult::Success { cached: true, .. }
        ));
    }

    #[tokio::test]
    async fn test_custom_execute_fn() {
        let mut dag = Dag::new();
        let target = Target::new("test", "hash1");
        dag.add_node(target.clone()).unwrap();

        let cache = MemoryKvStore::new();

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let execute_fn: ExecuteFn = Arc::new(move |_t| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            Ok(b"custom output".to_vec())
        });

        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 1).with_execute_fn(execute_fn);

        let _: Vec<ExecutionResult> = engine.execute(vec![target.clone()]).await.unwrap();

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_execution_error() {
        let mut dag = Dag::new();
        let target = Target::new("failing", "hash1");
        dag.add_node(target.clone()).unwrap();

        let cache = MemoryKvStore::new();

        let execute_fn: ExecuteFn = Arc::new(|_t| {
            Err(EngineError::Execution {
                target: "failing".to_string(),
                message: "intentional failure".to_string(),
            })
        });

        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 1).with_execute_fn(execute_fn);

        let results: Vec<ExecutionResult> = engine.execute(vec![target.clone()]).await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].is_failed());
    }

    #[tokio::test]
    async fn test_target_not_found() {
        let dag = Dag::new();
        let cache = MemoryKvStore::new();
        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 1);

        let target = Target::new("nonexistent", "hash1");
        let result: Result<Vec<ExecutionResult>> = engine.execute(vec![target]).await;

        assert!(result.is_err());
        assert!(matches!(result, Err(EngineError::TargetNotFound(_))));
    }

    #[tokio::test]
    async fn test_parallelism() {
        let mut dag = Dag::new();

        // Create 5 independent targets
        let mut targets = Vec::new();
        for i in 0..5 {
            let target = Target::new(format!("target_{}", i), format!("hash_{}", i));
            dag.add_node(target.clone()).unwrap();
            targets.push(target);
        }

        let cache = MemoryKvStore::new();
        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 3); // parallelism = 3

        let results: Vec<ExecutionResult> = engine.execute(targets).await.unwrap();

        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|r: &ExecutionResult| r.is_success()));
    }

    #[tokio::test]
    async fn test_complex_dag() {
        let mut dag = Dag::new();

        // Create a diamond DAG:
        // d depends on b and c, b and c depend on a
        // Execution order: a, then b and c (in parallel), then d
        let target_a = Target::new("a", "hash_a");
        let target_b = Target::new("b", "hash_b");
        let target_c = Target::new("c", "hash_c");
        let target_d = Target::new("d", "hash_d");

        dag.add_node(target_a.clone()).unwrap();
        dag.add_node(target_b.clone()).unwrap();
        dag.add_node(target_c.clone()).unwrap();
        dag.add_node(target_d.clone()).unwrap();

        // b depends on a, c depends on a (a must run before b and c)
        dag.add_edge(&target_a, &target_b).unwrap();
        dag.add_edge(&target_a, &target_c).unwrap();
        // d depends on b and c (b and c must run before d)
        dag.add_edge(&target_b, &target_d).unwrap();
        dag.add_edge(&target_c, &target_d).unwrap();

        let cache = MemoryKvStore::new();
        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 2);

        // Execute only d, should execute all
        let results: Vec<ExecutionResult> = engine.execute(vec![target_d.clone()]).await.unwrap();

        assert_eq!(results.len(), 4);
        assert!(results.iter().all(|r: &ExecutionResult| r.is_success()));

        // Verify topological order
        let names: Vec<&str> = results.iter().map(|r: &ExecutionResult| r.target()).collect();
        let a_pos = names.iter().position(|&x| x == "a").unwrap();
        let b_pos = names.iter().position(|&x| x == "b").unwrap();
        let c_pos = names.iter().position(|&x| x == "c").unwrap();
        let d_pos = names.iter().position(|&x| x == "d").unwrap();

        // a must come before b and c
        assert!(a_pos < b_pos);
        assert!(a_pos < c_pos);

        // b and c must come before d
        assert!(b_pos < d_pos);
        assert!(c_pos < d_pos);
    }

    #[tokio::test]
    async fn test_large_dag_execution() {
        // Test with a larger DAG (20 targets)
        let mut dag = Dag::new();

        let mut targets = Vec::new();
        for i in 0..20 {
            let target = Target::new(format!("target_{}", i), format!("hash_{}", i));
            dag.add_node(target.clone()).unwrap();
            targets.push(target);
        }

        // Create a chain: 0 -> 1 -> 2 -> ... -> 19
        for i in 0..19 {
            dag.add_edge(&targets[i], &targets[i + 1]).unwrap();
        }

        let cache = MemoryKvStore::new();
        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 4);

        // Execute the last target, should execute all
        let results: Vec<ExecutionResult> = engine.execute(vec![targets[19].clone()]).await.unwrap();

        assert_eq!(results.len(), 20);
        assert!(results.iter().all(|r: &ExecutionResult| r.is_success()));

        // Verify they're in order
        for (i, result) in results.iter().enumerate() {
            let expected_name = format!("target_{}", i);
            assert_eq!(result.target(), expected_name);
        }
    }

    #[tokio::test]
    async fn test_multiple_roots() {
        // Test DAG with multiple root nodes
        let mut dag = Dag::new();

        let root1 = Target::new("root1", "hash_r1");
        let root2 = Target::new("root2", "hash_r2");
        let leaf = Target::new("leaf", "hash_leaf");

        dag.add_node(root1.clone()).unwrap();
        dag.add_node(root2.clone()).unwrap();
        dag.add_node(leaf.clone()).unwrap();

        dag.add_edge(&root1, &leaf).unwrap();
        dag.add_edge(&root2, &leaf).unwrap();

        let cache = MemoryKvStore::new();
        let engine: Engine<MemoryKvStore> = Engine::new(dag, cache, 2);

        let results: Vec<ExecutionResult> = engine.execute(vec![leaf.clone()]).await.unwrap();

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r: &ExecutionResult| r.is_success()));

        // Both roots should come before leaf
        let names: Vec<&str> = results.iter().map(|r: &ExecutionResult| r.target()).collect();
        let leaf_pos = names.iter().position(|&x| x == "leaf").unwrap();
        let root1_pos = names.iter().position(|&x| x == "root1").unwrap();
        let root2_pos = names.iter().position(|&x| x == "root2").unwrap();

        assert!(root1_pos < leaf_pos);
        assert!(root2_pos < leaf_pos);
    }
}
