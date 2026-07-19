//! PageRank — pull-based fixpoint.
//!
//! Pull-based: each iteration, every node pulls rank from its *in*-neighbours, so
//! the kernel consults the transpose (CSC). Dangling nodes (out-degree 0) would
//! leak rank, so their mass is redistributed uniformly each iteration.
//!
//! This is a real, if straightforward, implementation. The Rayon parallel sweep
//! over vertex ranges is the shape every fixpoint kernel shares; wiring it here
//! establishes the pattern. Weights are not yet threaded (unweighted PageRank);
//! the weighted variant gathers `weight[edge_ids[k]]` per in-edge — the seam is
//! `Adjacency::edge_ids`.

use rayon::prelude::*;

use crate::topology::Topology;

/// PageRank parameters, mirroring `ur.pagerank(...)`.
#[derive(Debug, Clone, Copy)]
pub struct PageRankParams {
    pub damping: f64,
    pub max_iter: u32,
    pub tol: f64,
}

impl Default for PageRankParams {
    fn default() -> Self {
        PageRankParams {
            damping: 0.85,
            max_iter: 30,
            tol: 1e-6,
        }
    }
}

/// Dense PageRank vector; `result[u]` is the score of dense node `u`. Scores sum
/// to ~1.0 (up to dangling redistribution and early convergence).
pub fn pagerank(topo: &Topology, params: PageRankParams) -> Vec<f64> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    let nf = n as f64;
    let d = params.damping;

    // Precompute out-degrees (rank is divided by them when pushed forward).
    let out = topo.out();
    let out_deg: Vec<u32> = (0..n as u32).map(|u| out.degree(u)).collect();
    let inc = topo.incoming();

    let mut rank = vec![1.0 / nf; n];
    let mut next = vec![0.0f64; n];

    for _iter in 0..params.max_iter {
        // Rank stranded on dangling nodes, spread uniformly to everyone.
        let dangling: f64 = (0..n)
            .into_par_iter()
            .filter(|&u| out_deg[u] == 0)
            .map(|u| rank[u])
            .sum();
        let base = (1.0 - d) / nf + d * dangling / nf;

        next.par_iter_mut().enumerate().for_each(|(u, slot)| {
            let mut acc = 0.0;
            for &v in inc.neighbors(u as u32) {
                // v is an in-neighbour of u; it contributes rank[v] / outdeg[v].
                let od = out_deg[v as usize];
                if od > 0 {
                    acc += rank[v as usize] / od as f64;
                }
            }
            *slot = base + d * acc;
        });

        let delta: f64 = next
            .par_iter()
            .zip(rank.par_iter())
            .map(|(a, b)| (a - b).abs())
            .sum();

        std::mem::swap(&mut rank, &mut next);
        if delta < params.tol {
            break;
        }
    }

    rank
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_sum_to_one_and_rank_hub_highest() {
        // A small graph where node 2 is a sink hub: 0->2, 1->2, 3->2, 2->0
        let t = Topology::build(4, vec![0, 1, 3, 2], vec![2, 2, 2, 0]);
        let pr = pagerank(&t, PageRankParams::default());
        let sum: f64 = pr.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum was {sum}");
        // The hub everyone points at should outrank every other node.
        let hub = pr[2];
        assert!(pr.iter().enumerate().all(|(i, &v)| i == 2 || v < hub));
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(pagerank(&t, PageRankParams::default()).is_empty());
    }
}
