//! Label propagation — community detection by iterated neighbour-majority voting.
//!
//! Each node adopts the label held by the plurality of its neighbours; iterated
//! to a fixpoint. Updates are *asynchronous* in a `seed`-derived sweep order (a
//! node sees its neighbours' already-updated labels within a sweep), which is the
//! Raghavan et al. formulation — it avoids the two-colour oscillation that a
//! fully-synchronous update suffers on bipartite structure. Ties are broken
//! toward the smallest label id so the result is deterministic regardless of hash
//! iteration order.

use std::collections::HashMap;

use super::rng::{shuffled_order, DEFAULT_SEED};
use crate::topology::Topology;

/// Community label per node. Communities are treated as undirected (a node votes
/// over both its out- and in-neighbours). Iterates until a full sweep changes no
/// label or `max_iter` sweeps have run. Labels are dense node indices (one
/// representative per community), not necessarily a contiguous `0..k`.
pub fn label_propagation(topo: &Topology, max_iter: u32, seed: Option<u64>) -> Vec<u32> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    let mut label: Vec<u32> = (0..n as u32).collect();
    let out = topo.out();
    let inc = topo.incoming();
    let order = shuffled_order(n, seed.unwrap_or(DEFAULT_SEED));
    let mut counts: HashMap<u32, u32> = HashMap::new();

    for _ in 0..max_iter {
        let mut changed = false;
        for &u in &order {
            counts.clear();
            for &v in out.neighbors(u) {
                *counts.entry(label[v as usize]).or_insert(0) += 1;
            }
            for &v in inc.neighbors(u) {
                *counts.entry(label[v as usize]).or_insert(0) += 1;
            }
            if counts.is_empty() {
                continue; // isolated node keeps its own label
            }
            // Plurality label; ties resolved toward the smallest label id so the
            // choice does not depend on the map's iteration order.
            let cur = label[u as usize];
            let mut best_label = cur;
            let mut best_count = 0u32;
            for (&lab, &c) in &counts {
                if c > best_count || (c == best_count && lab < best_label) {
                    best_count = c;
                    best_label = lab;
                }
            }
            if best_label != cur {
                label[u as usize] = best_label;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 4-cliques {0,1,2,3} and {4,5,6,7} joined by a single bridge 3-4.
    fn two_cliques() -> Topology {
        let mut src = Vec::new();
        let mut dst = Vec::new();
        for &(a, b) in &[
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 2),
            (1, 3),
            (2, 3),
            (4, 5),
            (4, 6),
            (4, 7),
            (5, 6),
            (5, 7),
            (6, 7),
            (3, 4), // bridge
        ] {
            src.push(a);
            dst.push(b);
        }
        Topology::build(8, src, dst)
    }

    #[test]
    fn recovers_two_communities() {
        let t = two_cliques();
        let lab = label_propagation(&t, 20, Some(7));
        // Each clique collapses to one label; the two labels differ.
        assert!(lab[0] == lab[1] && lab[1] == lab[2] && lab[2] == lab[3]);
        assert!(lab[4] == lab[5] && lab[5] == lab[6] && lab[6] == lab[7]);
        assert_ne!(lab[0], lab[4]);
    }

    #[test]
    fn deterministic_given_a_seed() {
        let t = two_cliques();
        assert_eq!(
            label_propagation(&t, 20, Some(3)),
            label_propagation(&t, 20, Some(3))
        );
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(label_propagation(&t, 20, None).is_empty());
    }
}
