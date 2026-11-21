//! [`Selection`]: the nodes and edges a user has picked.

use super::result::{err, GraphResult, ModelError};

/// A set of nodes and a set of edges, by index, each kept sorted.
///
/// It is not part of a [`GraphResult`] or of the wire format: a selection
/// belongs to the front end that made it. Indexes are append-only, so a
/// selection stays valid as a result grows, and through a removal (a removed
/// node keeps its slot). Only taking nodes off the end can outdate it, and
/// [`clip`](Selection::clip) is for then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    nodes: Vec<u32>,
    edges: Vec<u32>,
}

fn sorted_set(mut indexes: Vec<u32>) -> Vec<u32> {
    indexes.sort_unstable();
    indexes.dedup();
    indexes
}

/// Add `i` if absent, remove it if present; whether it is now present.
fn toggle(set: &mut Vec<u32>, i: u32) -> bool {
    match set.binary_search(&i) {
        Ok(at) => {
            set.remove(at);
            false
        }
        Err(at) => {
            set.insert(at, i);
            true
        }
    }
}

impl Selection {
    pub fn new(nodes: Vec<u32>, edges: Vec<u32>) -> Self {
        Self {
            nodes: sorted_set(nodes),
            edges: sorted_set(edges),
        }
    }

    /// The selected nodes, rising.
    pub fn nodes(&self) -> &[u32] {
        &self.nodes
    }

    /// The selected edges, rising.
    pub fn edges(&self) -> &[u32] {
        &self.edges
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }

    pub fn has_node(&self, node: u32) -> bool {
        self.nodes.binary_search(&node).is_ok()
    }

    pub fn has_edge(&self, edge: u32) -> bool {
        self.edges.binary_search(&edge).is_ok()
    }

    /// Select the node if it is not selected, and the reverse (a shift-click).
    /// Returns whether it is now selected.
    pub fn toggle_node(&mut self, node: u32) -> bool {
        toggle(&mut self.nodes, node)
    }

    pub fn toggle_edge(&mut self, edge: u32) -> bool {
        toggle(&mut self.edges, edge)
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.edges.clear();
    }

    /// Every index names a slot of `result`.
    pub fn check(&self, result: &GraphResult) -> Result<(), ModelError> {
        if let Some(&node) = self
            .nodes
            .last()
            .filter(|&&v| v as usize >= result.n_nodes())
        {
            return err(format!(
                "node {node} is selected, but the result has {} nodes",
                result.n_nodes()
            ));
        }
        if let Some(&edge) = self
            .edges
            .last()
            .filter(|&&e| e as usize >= result.n_edges())
        {
            return err(format!(
                "edge {edge} is selected, but the result has {} edges",
                result.n_edges()
            ));
        }
        Ok(())
    }

    /// Drop what `result` no longer has a slot for, after an undo took nodes
    /// or edges off its end.
    pub fn clip(&mut self, result: &GraphResult) {
        self.nodes.retain(|&v| (v as usize) < result.n_nodes());
        self.edges.retain(|&e| (e as usize) < result.n_edges());
    }
}
