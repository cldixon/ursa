//! The edges among a set of nodes.
//!
//! "Which edges connect these nodes?" is what drawing a node set, a `hop`
//! neighbourhood or an expanded node needs. Scanning the edge table for it
//! costs the whole table. The index answers it from the nodes alone: mark
//! them in a bitmap, walk each one's out-adjacency, keep the edges whose
//! other end is marked. Every edge among the nodes leaves one of them, so
//! the out-adjacency finds each exactly once, and the cost is the sum of the
//! nodes' out-degrees (plus `n_nodes / 8` bytes of bitmap), whatever the
//! graph's edge count.
//!
//! Serial on purpose: the walk is a few thousand slots for the node sets
//! this is for, and the output is the same on any thread count by
//! construction.

use crate::topology::Topology;

/// The edges of a graph whose two ends are both in a node set, as parallel
/// vectors in edge order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InducedEdges {
    /// The dense node each edge leaves.
    pub src: Vec<u32>,
    /// The dense node each edge enters.
    pub dst: Vec<u32>,
    /// Each edge's id: its row among the index's edges
    /// ([`crate::topology::Adjacency::edge_ids`]). Ascending.
    pub edge: Vec<u32>,
}

impl InducedEdges {
    /// How many edges.
    pub fn len(&self) -> usize {
        self.edge.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.edge.is_empty()
    }
}

/// The edges of `topo` whose source and destination are both in `nodes`, in
/// the direction the graph stores them, ordered by edge id.
///
/// A self-loop on a node of the set is one of its edges; parallel edges are
/// separate edges. A node listed twice counts once, and one out of range
/// (`>= n_nodes`) is in no edge. The result depends only on the set, not on
/// the order `nodes` lists it in.
///
/// The time is proportional to the sum of the nodes' out-degrees, and to
/// sorting the edges found.
pub fn induced_edges(topo: &Topology, nodes: &[u32]) -> InducedEdges {
    let n = topo.n_nodes();
    let mut marked = vec![0u64; n.div_ceil(64)];
    // The set's nodes, each once, in the order first listed.
    let mut members = Vec::with_capacity(nodes.len().min(n));
    for &u in nodes {
        if (u as usize) < n {
            let (word, bit) = (u as usize / 64, 1u64 << (u % 64));
            if marked[word] & bit == 0 {
                marked[word] |= bit;
                members.push(u);
            }
        }
    }
    let is_marked = |v: u32| marked[v as usize / 64] & (1u64 << (v % 64)) != 0;
    let out = topo.out();
    // (edge, src, dst): edge ids are unique, so sorting by them is a total
    // order and the result does not depend on the order of `members`.
    let mut found: Vec<(u32, u32, u32)> = Vec::new();
    for &u in &members {
        for (&v, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
            if is_marked(v) {
                found.push((e, u, v));
            }
        }
    }
    found.sort_unstable();
    let mut edges = InducedEdges {
        src: Vec::with_capacity(found.len()),
        dst: Vec::with_capacity(found.len()),
        edge: Vec::with_capacity(found.len()),
    };
    for (e, u, v) in found {
        edges.src.push(u);
        edges.dst.push(v);
        edges.edge.push(e);
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The edges among `nodes` by scanning every edge: the definition.
    fn scan(src: &[u32], dst: &[u32], nodes: &[u32]) -> InducedEdges {
        let mut edges = InducedEdges::default();
        for (e, (&u, &v)) in src.iter().zip(dst).enumerate() {
            if nodes.contains(&u) && nodes.contains(&v) {
                edges.src.push(u);
                edges.dst.push(v);
                edges.edge.push(e as u32);
            }
        }
        edges
    }

    fn graph() -> (Vec<u32>, Vec<u32>) {
        // A self-loop (2→2), parallel edges (0→1 twice), both directions
        // (1→0), and node 5 on no edge.
        (vec![0, 0, 1, 2, 2, 3, 0, 4], vec![1, 1, 0, 2, 3, 4, 3, 0])
    }

    #[test]
    fn matches_a_scan_of_the_edges() {
        let (src, dst) = graph();
        let topo = Topology::build(6, src.clone(), dst.clone());
        let sets: [&[u32]; 8] = [
            &[],
            &[0],
            &[2],
            &[0, 1],
            &[0, 1, 2, 3],
            &[3, 4, 0],
            &[5],
            &[0, 1, 2, 3, 4, 5],
        ];
        for nodes in sets {
            assert_eq!(
                induced_edges(&topo, nodes),
                scan(&src, &dst, nodes),
                "{nodes:?}"
            );
        }
    }

    #[test]
    fn keeps_self_loops_and_parallel_edges_in_edge_order() {
        let (src, dst) = graph();
        let topo = Topology::build(6, src, dst);
        let edges = induced_edges(&topo, &[2, 1, 0]);
        assert_eq!(edges.edge, [0, 1, 2, 3]);
        assert_eq!(edges.src, [0, 0, 1, 2]);
        assert_eq!(edges.dst, [1, 1, 0, 2]);
        assert_eq!(edges.len(), 4);
        assert!(!edges.is_empty());
    }

    #[test]
    fn a_set_is_a_set() {
        let (src, dst) = graph();
        let topo = Topology::build(6, src, dst);
        let once = induced_edges(&topo, &[0, 3, 4]);
        // Repeats, another order, and ids out of range change nothing.
        assert_eq!(induced_edges(&topo, &[4, 0, 3, 0, 4, 6, u32::MAX]), once);
        assert_eq!(once.edge, [5, 6, 7]);
    }

    #[test]
    fn an_empty_graph_has_no_edges() {
        let topo = Topology::build(0, Vec::new(), Vec::new());
        assert!(induced_edges(&topo, &[0, 1]).is_empty());
    }

    /// Against the scan on a random graph, for a large set listed backwards.
    #[test]
    fn matches_a_scan_on_a_random_graph() {
        let (n, m) = (500u32, 4000usize);
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as u32
        };
        let src: Vec<u32> = (0..m).map(|_| next() % n).collect();
        let dst: Vec<u32> = (0..m).map(|_| next() % n).collect();
        let topo = Topology::build(n as usize, src.clone(), dst.clone());
        let nodes: Vec<u32> = (0..n).filter(|u| u % 3 != 0).rev().collect();
        let edges = induced_edges(&topo, &nodes);
        assert_eq!(edges, scan(&src, &dst, &nodes));
        assert!(edges.len() > 1000);
    }
}
