//! Label propagation — community detection by iterated neighbour-majority voting.
//!
//! Each node adopts the label held by the plurality of its neighbours; iterated
//! to a fixpoint. Updates are *asynchronous* in a `seed`-derived sweep order (a
//! node sees its neighbours' already-updated labels within a sweep), which is the
//! Raghavan et al. formulation — it avoids the two-colour oscillation that a
//! fully-synchronous update suffers on bipartite structure. Ties are broken
//! toward the smallest label id so the result is deterministic regardless of hash
//! iteration order.
//!
//! **Multiplicity:** votes are tallied per CSR slot, so parallel edges cast
//! parallel votes (multiplicity-is-rows) — a heavily-parallel neighbour pulls
//! proportionally harder.

use super::linked::Linked;
use super::rng::{shuffled_order, DEFAULT_SEED};
use crate::topology::{Adjacency, EdgeMask, Topology};

/// Community label per node. Communities are treated as undirected (a node votes
/// over both its out- and in-neighbours). Iterates until a full sweep changes no
/// label or `max_iter` sweeps have run. Labels are dense node indices (one
/// representative per community), not necessarily a contiguous `0..k`. A subgraph
/// `mask` restricts voting to kept edges.
///
/// An isolated node never votes and is never voted for, so it keeps its own
/// label; a graph with many runs over its linked nodes only (#326), visited in
/// the order the seeded permutation of every node gives them, with the same
/// labels.
pub fn label_propagation(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    max_iter: u32,
    seed: Option<u64>,
) -> Vec<u32> {
    if topo.n_nodes() == 0 {
        return Vec::new();
    }
    topo.incoming();
    let linked = Linked::of(topo, mask);
    label_propagation_over(
        topo,
        mask,
        max_iter,
        seed,
        linked.worth_it().then_some(&linked),
    )
}

/// [`label_propagation`], compact over `linked` when given.
fn label_propagation_over(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    max_iter: u32,
    seed: Option<u64>,
    linked: Option<&Linked>,
) -> Vec<u32> {
    let n = topo.n_nodes();
    let width = linked.map_or(n, Linked::len);
    // The node of compact node `u`, and the compact node of node `v`.
    let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
    let at = |v: u32| linked.map_or(v, |l| l.compact(v));
    let mut label: Vec<u32> = (0..width as u32).collect();
    let out = topo.out();
    let inc = topo.incoming();
    let order = shuffled_order(n, seed.unwrap_or(DEFAULT_SEED));
    let order = match linked {
        None => order,
        Some(l) => l.compact_order(&order).0,
    };
    // Vote tally, dense over label ids (labels are node ids, so `< n`), plus the
    // labels touched for the current node. Counting into an array and resetting
    // only the touched slots replaces a hash map probe per neighbour; the winner
    // depends only on the counts, never on the order labels were seen.
    let mut counts = vec![0u32; width];
    let mut touched: Vec<u32> = Vec::new();

    // Tally each kept-edge neighbour's label for node `u` in one adjacency.
    let tally =
        |adj: &Adjacency, u: u32, label: &[u32], counts: &mut [u32], touched: &mut Vec<u32>| {
            let u = node(u);
            for (&v, &e) in adj.neighbors(u).iter().zip(adj.edge_ids(u)) {
                if mask.is_none_or(|m| m.keep(e)) {
                    let lab = label[at(v) as usize];
                    let c = &mut counts[lab as usize];
                    if *c == 0 {
                        touched.push(lab);
                    }
                    *c += 1;
                }
            }
        };

    for _ in 0..max_iter {
        let mut changed = false;
        for &u in &order {
            tally(out, u, &label, &mut counts, &mut touched);
            tally(inc, u, &label, &mut counts, &mut touched);
            if touched.is_empty() {
                continue; // isolated node keeps its own label
            }
            // Plurality label; ties resolved toward the smallest label id so the
            // choice does not depend on the order labels were tallied.
            let cur = label[u as usize];
            let mut best_label = cur;
            let mut best_count = 0u32;
            for &lab in &touched {
                let c = std::mem::take(&mut counts[lab as usize]);
                if c > best_count || (c == best_count && lab < best_label) {
                    best_count = c;
                    best_label = lab;
                }
            }
            touched.clear();
            if best_label != cur {
                label[u as usize] = best_label;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    match linked {
        None => label,
        Some(l) => l.expand_labels(&label),
    }
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
        let lab = label_propagation(&t, None, 20, Some(7));
        // Each clique collapses to one label; the two labels differ.
        assert!(lab[0] == lab[1] && lab[1] == lab[2] && lab[2] == lab[3]);
        assert!(lab[4] == lab[5] && lab[5] == lab[6] && lab[6] == lab[7]);
        assert_ne!(lab[0], lab[4]);
    }

    #[test]
    fn deterministic_given_a_seed() {
        let t = two_cliques();
        assert_eq!(
            label_propagation(&t, None, 20, Some(3)),
            label_propagation(&t, None, 20, Some(3))
        );
    }

    /// The compact run (#326) gives the full run's labels.
    #[test]
    fn compact_runs_give_the_full_labels() {
        use crate::algo::linked::testing::{sparse, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m);
            for mask in [None, Some(&mask)] {
                let linked = Linked::of(&t, mask);
                for s in [None, Some(5)] {
                    for iters in [1, 3, 30] {
                        assert_eq!(
                            label_propagation_over(&t, mask, iters, s, Some(&linked)),
                            label_propagation_over(&t, mask, iters, s, None),
                            "graph {seed}, masked {}, seed {s:?}, {iters} iterations",
                            mask.is_some()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(label_propagation(&t, None, 20, None).is_empty());
    }
}
