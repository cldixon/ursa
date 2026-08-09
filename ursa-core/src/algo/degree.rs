//! Degree — the simplest kernel, and the smoke test for the whole stack.

use crate::topology::{Adjacency, Direction, EdgeMask, Topology};

/// Per-node degree in the requested direction, dense-indexed (`result[u]` is the
/// degree of dense node `u`).
///
/// - `Out`  — number of outgoing edges (CSR row length).
/// - `In`   — number of incoming edges (builds the transpose on first use).
/// - `Both` — out + in. Self-loops are counted once per direction (i.e. twice in
///   `Both`), consistent with treating multiplicity as rows.
///
/// With a subgraph `mask`, only kept edges count — the O(1) CSR row length gives
/// way to an O(degree) scan that tallies unmasked incident rows.
pub fn degree(topo: &Topology, mask: Option<&EdgeMask>, dir: Direction) -> Vec<u32> {
    let n = topo.n_nodes();
    // Masked degree of node `u` in one adjacency: count its kept incident rows.
    let masked_deg = |adj: &Adjacency, u: u32, m: &EdgeMask| -> u32 {
        adj.edge_ids(u).iter().filter(|&&e| m.keep(e)).count() as u32
    };
    match (dir, mask) {
        (Direction::Out, None) => (0..n as u32).map(|u| topo.out().degree(u)).collect(),
        (Direction::In, None) => {
            let inc = topo.incoming();
            (0..n as u32).map(|u| inc.degree(u)).collect()
        }
        (Direction::Both, None) => {
            let out = topo.out();
            let inc = topo.incoming();
            (0..n as u32)
                .map(|u| out.degree(u) + inc.degree(u))
                .collect()
        }
        (Direction::Out, Some(m)) => {
            let out = topo.out();
            (0..n as u32).map(|u| masked_deg(out, u, m)).collect()
        }
        (Direction::In, Some(m)) => {
            let inc = topo.incoming();
            (0..n as u32).map(|u| masked_deg(inc, u, m)).collect()
        }
        (Direction::Both, Some(m)) => {
            let out = topo.out();
            let inc = topo.incoming();
            (0..n as u32)
                .map(|u| masked_deg(out, u, m) + masked_deg(inc, u, m))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions() {
        // 0->1, 0->2, 1->2, 2->0
        let t = Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 2, 0]);
        assert_eq!(degree(&t, None, Direction::Out), vec![2, 1, 1]);
        assert_eq!(degree(&t, None, Direction::In), vec![1, 1, 2]);
        assert_eq!(degree(&t, None, Direction::Both), vec![3, 2, 3]);
    }

    #[test]
    fn masked_degree_counts_only_kept_edges() {
        use crate::topology::EdgeMask;
        // rows: 0=(0->1), 1=(0->2), 2=(1->2), 3=(2->0). Drop row 1 (0->2).
        let t = Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 2, 0]);
        let mask = EdgeMask::from_bools(&[true, false, true, true]);
        // out: node 0 loses one out-edge -> 1; node 1 -> 1; node 2 -> 1
        assert_eq!(degree(&t, Some(&mask), Direction::Out), vec![1, 1, 1]);
        // in: node 2 loses one in-edge -> 1; node 1 -> 1; node 0 -> 1
        assert_eq!(degree(&t, Some(&mask), Direction::In), vec![1, 1, 1]);
    }
}
