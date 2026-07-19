//! Degree — the simplest kernel, and the smoke test for the whole stack.

use crate::topology::{Direction, Topology};

/// Per-node degree in the requested direction, dense-indexed (`result[u]` is the
/// degree of dense node `u`).
///
/// - `Out`  — number of outgoing edges (CSR row length).
/// - `In`   — number of incoming edges (builds the transpose on first use).
/// - `Both` — out + in. Self-loops are counted once per direction (i.e. twice in
///   `Both`), consistent with treating multiplicity as rows.
pub fn degree(topo: &Topology, dir: Direction) -> Vec<u32> {
    let n = topo.n_nodes();
    match dir {
        Direction::Out => (0..n as u32).map(|u| topo.out().degree(u)).collect(),
        Direction::In => {
            let inc = topo.incoming();
            (0..n as u32).map(|u| inc.degree(u)).collect()
        }
        Direction::Both => {
            let out = topo.out();
            let inc = topo.incoming();
            (0..n as u32).map(|u| out.degree(u) + inc.degree(u)).collect()
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
        assert_eq!(degree(&t, Direction::Out), vec![2, 1, 1]);
        assert_eq!(degree(&t, Direction::In), vec![1, 1, 2]);
        assert_eq!(degree(&t, Direction::Both), vec![3, 2, 3]);
    }
}
