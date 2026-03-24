//! Directed Acyclic Graph for build targets

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::Topo;
use petgraph::Direction;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DagError {
    #[error("Cycle detected in graph")]
    CycleDetected,

    #[error("Node not found: {0}")]
    NodeNotFound(String),

    #[error("Duplicate node: {0}")]
    DuplicateNode(String),
}

pub type Result<T> = std::result::Result<T, DagError>;

/// Directed Acyclic Graph
pub struct Dag<T>
where
    T: Clone + Eq + Hash + Debug,
{
    graph: DiGraph<T, ()>,
    nodes: HashMap<T, NodeIndex>,
}

impl<T> Dag<T>
where
    T: Clone + Eq + Hash + Debug,
{
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            nodes: HashMap::new(),
        }
    }

    /// Add a node to the graph
    pub fn add_node(&mut self, value: T) -> Result<()> {
        if self.nodes.contains_key(&value) {
            return Err(DagError::DuplicateNode(format!("{:?}", value)));
        }

        let index = self.graph.add_node(value.clone());
        self.nodes.insert(value, index);
        Ok(())
    }

    /// Add an edge from -> to
    pub fn add_edge(&mut self, from: &T, to: &T) -> Result<()> {
        let from_idx = self
            .nodes
            .get(from)
            .ok_or_else(|| DagError::NodeNotFound(format!("{:?}", from)))?;
        let to_idx = self
            .nodes
            .get(to)
            .ok_or_else(|| DagError::NodeNotFound(format!("{:?}", to)))?;

        self.graph.add_edge(*from_idx, *to_idx, ());

        // Check for cycles
        if petgraph::algo::is_cyclic_directed(&self.graph) {
            // Remove the edge that created the cycle
            if let Some(edge) = self.graph.find_edge(*from_idx, *to_idx) {
                self.graph.remove_edge(edge);
            }
            return Err(DagError::CycleDetected);
        }

        Ok(())
    }

    /// Get topological order of nodes
    pub fn topological_order(&self) -> Vec<T> {
        let mut topo = Topo::new(&self.graph);
        let mut result = Vec::new();

        while let Some(node_idx) = topo.next(&self.graph) {
            if let Some(node) = self.graph.node_weight(node_idx) {
                result.push(node.clone());
            }
        }

        result
    }

    /// Get dependencies of a node (nodes this node depends on)
    pub fn dependencies(&self, node: &T) -> Result<Vec<T>> {
        let idx = self
            .nodes
            .get(node)
            .ok_or_else(|| DagError::NodeNotFound(format!("{:?}", node)))?;

        Ok(self
            .graph
            .neighbors_directed(*idx, Direction::Outgoing)
            .filter_map(|n| self.graph.node_weight(n).cloned())
            .collect())
    }

    /// Get dependents (reverse dependencies) of a node
    /// (nodes that depend on this node)
    pub fn dependents(&self, node: &T) -> Result<Vec<T>> {
        let idx = self
            .nodes
            .get(node)
            .ok_or_else(|| DagError::NodeNotFound(format!("{:?}", node)))?;

        Ok(self
            .graph
            .neighbors_directed(*idx, Direction::Incoming)
            .filter_map(|n| self.graph.node_weight(n).cloned())
            .collect())
    }

    /// Get all nodes
    pub fn nodes(&self) -> Vec<T> {
        self.graph.node_weights().cloned().collect()
    }

    /// Check if node exists
    pub fn contains(&self, node: &T) -> bool {
        self.nodes.contains_key(node)
    }

    /// Get the number of nodes
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Get the number of edges
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }
}

impl<T> Default for Dag<T>
where
    T: Clone + Eq + Hash + Debug,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_node() {
        let mut dag = Dag::new();
        assert!(dag.add_node("a").is_ok());
        assert!(dag.contains(&"a"));
    }

    #[test]
    fn test_duplicate_node() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();
        assert!(dag.add_node("a").is_err());
    }

    #[test]
    fn test_add_edge() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();
        dag.add_node("b").unwrap();
        assert!(dag.add_edge(&"a", &"b").is_ok());
    }

    #[test]
    fn test_cycle_detection() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();
        dag.add_node("b").unwrap();
        dag.add_node("c").unwrap();

        dag.add_edge(&"a", &"b").unwrap();
        dag.add_edge(&"b", &"c").unwrap();

        // This should create a cycle: c -> a -> b -> c
        let result = dag.add_edge(&"c", &"a");
        assert!(result.is_err());
        assert!(matches!(result, Err(DagError::CycleDetected)));
    }

    #[test]
    fn test_topological_order() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();
        dag.add_node("b").unwrap();
        dag.add_node("c").unwrap();

        // a -> b -> c
        dag.add_edge(&"a", &"b").unwrap();
        dag.add_edge(&"b", &"c").unwrap();

        let order = dag.topological_order();
        assert_eq!(order.len(), 3);

        // a should come before b, b before c
        let a_pos = order.iter().position(|&x| x == "a").unwrap();
        let b_pos = order.iter().position(|&x| x == "b").unwrap();
        let c_pos = order.iter().position(|&x| x == "c").unwrap();

        assert!(a_pos < b_pos);
        assert!(b_pos < c_pos);
    }

    #[test]
    fn test_dependencies() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();
        dag.add_node("b").unwrap();
        dag.add_node("c").unwrap();

        dag.add_edge(&"a", &"b").unwrap();
        dag.add_edge(&"a", &"c").unwrap();

        let deps = dag.dependencies(&"a").unwrap();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"b"));
        assert!(deps.contains(&"c"));
    }

    #[test]
    fn test_dependents() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();
        dag.add_node("b").unwrap();
        dag.add_node("c").unwrap();

        dag.add_edge(&"a", &"b").unwrap();
        dag.add_edge(&"c", &"b").unwrap();

        let deps = dag.dependents(&"b").unwrap();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"a"));
        assert!(deps.contains(&"c"));
    }

    #[test]
    fn test_node_count() {
        let mut dag = Dag::new();
        assert_eq!(dag.node_count(), 0);

        dag.add_node("a").unwrap();
        assert_eq!(dag.node_count(), 1);

        dag.add_node("b").unwrap();
        assert_eq!(dag.node_count(), 2);
    }

    #[test]
    fn test_edge_count() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();
        dag.add_node("b").unwrap();
        assert_eq!(dag.edge_count(), 0);

        dag.add_edge(&"a", &"b").unwrap();
        assert_eq!(dag.edge_count(), 1);
    }

    #[test]
    fn test_complex_dag() {
        let mut dag = Dag::new();

        // Create a diamond-shaped DAG
        // a -> b -> d
        // a -> c -> d
        for node in ["a", "b", "c", "d"] {
            dag.add_node(node).unwrap();
        }

        dag.add_edge(&"a", &"b").unwrap();
        dag.add_edge(&"a", &"c").unwrap();
        dag.add_edge(&"b", &"d").unwrap();
        dag.add_edge(&"c", &"d").unwrap();

        let order = dag.topological_order();
        assert_eq!(order.len(), 4);

        // Verify topological properties
        let a_pos = order.iter().position(|&x| x == "a").unwrap();
        let b_pos = order.iter().position(|&x| x == "b").unwrap();
        let c_pos = order.iter().position(|&x| x == "c").unwrap();
        let d_pos = order.iter().position(|&x| x == "d").unwrap();

        assert!(a_pos < b_pos);
        assert!(a_pos < c_pos);
        assert!(b_pos < d_pos);
        assert!(c_pos < d_pos);
    }

    #[test]
    fn test_self_loop_rejected() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();

        // Try to create self-loop: a -> a
        let result = dag.add_edge(&"a", &"a");
        assert!(result.is_err());
        assert!(matches!(result, Err(DagError::CycleDetected)));
    }

    #[test]
    fn test_edge_nonexistent_node() {
        let mut dag = Dag::new();
        dag.add_node("a").unwrap();

        // Try to add edge to nonexistent node
        let result = dag.add_edge(&"a", &"b");
        assert!(result.is_err());
        assert!(matches!(result, Err(DagError::NodeNotFound(_))));
    }
}
