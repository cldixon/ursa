//! Random walks over the CSR — the stochastic member of the frontier family.
//!
//! For each start node, `walks_per_node` independent walks each take up to `steps`
//! hops, choosing a uniform-random out-neighbour at every step. The output is a
//! flat `(walk_id, step, node)` triple stream, ready to feed node2vec-style
//! embedding pipelines. A walk that reaches a node with no out-neighbour stops
//! early (its last emitted step is a dead end).
//!
//! Determinism (spec §Determinism): walks are generated sequentially from a
//! single seeded ChaCha stream, so a given `seed` reproduces the exact same walks
//! regardless of machine or thread count. `seed = None` uses a fixed default, so
//! an unseeded run is reproducible too.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

use super::rng::DEFAULT_SEED;
use crate::topology::Topology;

/// A flat column-oriented walk table; the three vectors are row-aligned. `node`
/// holds dense `u32` indices — translation to user ids happens at the Arrow
/// boundary in `ursa-plan`.
pub struct Walks {
    pub walk_id: Vec<i64>,
    pub step: Vec<i64>,
    pub node: Vec<u32>,
}

/// Generate random walks (following out-edges) from each start node. Out-of-range
/// start ids are skipped. `walk_id` numbers the walks `0..` in generation order
/// (all `walks_per_node` walks of the first start, then the next start, …).
pub fn random_walk(
    topo: &Topology,
    starts: &[u32],
    steps: u32,
    walks_per_node: u32,
    seed: Option<u64>,
) -> Walks {
    let n = topo.n_nodes();
    let out = topo.out();
    let mut rng = ChaCha8Rng::seed_from_u64(seed.unwrap_or(DEFAULT_SEED));

    let mut walk_id = Vec::new();
    let mut step_col = Vec::new();
    let mut node_col = Vec::new();
    let mut next_id = 0i64;

    for &start in starts {
        if (start as usize) >= n {
            continue;
        }
        for _ in 0..walks_per_node {
            let mut cur = start;
            walk_id.push(next_id);
            step_col.push(0);
            node_col.push(cur);
            for s in 1..=steps {
                let nbrs = out.neighbors(cur);
                if nbrs.is_empty() {
                    break; // dead end — the walk stops here
                }
                cur = nbrs[rng.gen_range(0..nbrs.len())];
                walk_id.push(next_id);
                step_col.push(s as i64);
                node_col.push(cur);
            }
            next_id += 1;
        }
    }

    Walks {
        walk_id,
        step: step_col,
        node: node_col,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0 -> 1 -> 2 -> 3 (a directed line, one choice at each step).
    fn line() -> Topology {
        Topology::build(4, vec![0, 1, 2], vec![1, 2, 3])
    }

    #[test]
    fn walk_follows_the_only_path() {
        // From 0, the single out-edge at each step forces the walk 0,1,2,3.
        let w = random_walk(&line(), &[0], 3, 1, Some(42));
        assert_eq!(w.walk_id, vec![0, 0, 0, 0]);
        assert_eq!(w.step, vec![0, 1, 2, 3]);
        assert_eq!(w.node, vec![0, 1, 2, 3]);
    }

    #[test]
    fn walk_stops_at_a_dead_end() {
        // Node 3 has no out-edges; a walk starting at 2 goes 2,3 then stops even
        // though 5 steps were requested.
        let w = random_walk(&line(), &[2], 5, 1, Some(1));
        assert_eq!(w.node, vec![2, 3]);
        assert_eq!(w.step, vec![0, 1]);
    }

    #[test]
    fn multiple_walks_per_node_get_distinct_ids() {
        let w = random_walk(&line(), &[0], 2, 3, Some(7));
        // Three walks, each 3 rows (steps 0,1,2): ids 0,0,0,1,1,1,2,2,2.
        assert_eq!(w.walk_id, vec![0, 0, 0, 1, 1, 1, 2, 2, 2]);
    }

    #[test]
    fn deterministic_given_a_seed() {
        // A branching graph so the RNG actually chooses: 0 -> {1,2,3}.
        let t = Topology::build(4, vec![0, 0, 0], vec![1, 2, 3]);
        let a = random_walk(&t, &[0], 1, 10, Some(99));
        let b = random_walk(&t, &[0], 1, 10, Some(99));
        assert_eq!(a.node, b.node);
    }

    #[test]
    fn out_of_range_start_is_skipped() {
        let w = random_walk(&line(), &[99], 3, 1, None);
        assert!(w.node.is_empty());
    }
}
