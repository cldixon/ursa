//! PageRank — pull-based fixpoint.
//!
//! Pull-based: each iteration, every node pulls rank from its *in*-neighbours, so
//! the kernel consults the transpose (CSC). Dangling nodes (out-degree 0) would
//! leak rank, so their mass is redistributed uniformly each iteration.
//!
//! The Rayon parallel sweep over vertex ranges is the shape every fixpoint kernel
//! shares. [`pagerank`] uses unit edge weights; [`pagerank_weighted`] gathers
//! `weight[edge_ids[k]]` per in-edge (the seam is `Adjacency::edge_ids`) and pulls
//! rank in proportion to weighted out-strength.
//!
//! **Multiplicity:** parallel `(u, v)` edges are kept as distinct CSR slots, so an
//! unweighted run pulls rank once per parallel edge (multiplicity-is-rows) — a
//! multigraph's scores differ from collapsing parallels to one edge.

use crate::parallel::*;

use crate::topology::{EdgeMask, Topology};

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

/// Deterministic L1 delta `Σ |a[i] - b[i]|`. Rayon's default `sum()` reduces in a
/// work-stealing-dependent order, so its result varies at the ULP level run to
/// run — which can flip `delta < tol` near threshold and change the iteration
/// count (and thus the output). Fixed-size chunks summed sequentially and combined
/// in order give the same value every run at any thread count, while staying
/// parallel across chunks.
fn l1_delta(a: &[f64], b: &[f64]) -> f64 {
    const CHUNK: usize = 8192;
    a.par_chunks(CHUNK)
        .zip(b.par_chunks(CHUNK))
        .map(|(ac, bc)| ac.iter().zip(bc).map(|(x, y)| (x - y).abs()).sum::<f64>())
        .collect::<Vec<f64>>()
        .iter()
        .sum()
}

/// The pull-based PageRank fixpoint, shared by the unweighted and weighted kernels.
///
/// `dangling` lists the zero-out-degree (or zero-out-strength) nodes, whose rank is
/// redistributed uniformly each iteration. `contrib(u, rank)` returns node `u`'s
/// pulled contribution — the sum over `u`'s in-edges of each in-neighbour's rank
/// share — which is the only part that differs between the variants. Everything
/// else (the uniform init, the `base + d * contrib` update, the deterministic L1
/// convergence check, the buffer swap, and the early break) lives here once, so
/// both kernels iterate through byte-identical float operations.
fn power_iterate(
    n: usize,
    params: PageRankParams,
    dangling: &[usize],
    contrib: impl Fn(usize, &[f64]) -> f64 + Sync,
) -> Vec<f64> {
    let nf = n as f64;
    let d = params.damping;

    let mut rank = vec![1.0 / nf; n];
    let mut next = vec![0.0f64; n];

    for _iter in 0..params.max_iter {
        // Rank stranded on dangling nodes, spread uniformly to everyone. Summed over
        // this fixed ascending-index list (a parallel filter-sum is not deterministic).
        let dangling_mass: f64 = dangling.iter().map(|&u| rank[u]).sum();
        let base = (1.0 - d) / nf + d * dangling_mass / nf;

        next.par_iter_mut().enumerate().for_each(|(u, slot)| {
            *slot = base + d * contrib(u, &rank);
        });

        let delta = l1_delta(&next, &rank);

        std::mem::swap(&mut rank, &mut next);
        if delta < params.tol {
            break;
        }
    }

    rank
}

/// Dense PageRank vector; `result[u]` is the score of dense node `u`. Scores sum
/// to ~1.0 (up to dangling redistribution and early convergence).
pub fn pagerank(topo: &Topology, mask: Option<&EdgeMask>, params: PageRankParams) -> Vec<f64> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }

    // Precompute out-degrees (rank is divided by them when pushed forward). Under a
    // subgraph mask, a node's out-degree counts only its *kept* out-edges — rank
    // splits among the edges present in the subgraph.
    let out = topo.out();
    let out_deg: Vec<u32> = match mask {
        None => (0..n as u32).map(|u| out.degree(u)).collect(),
        Some(m) => (0..n as u32)
            .map(|u| out.edge_ids(u).iter().filter(|&&e| m.keep(e)).count() as u32)
            .collect(),
    };
    let inc = topo.incoming();

    // Dangling nodes (out-degree 0), listed once — O(#dangling) rather than an O(n)
    // scan per iteration.
    let dangling_nodes: Vec<usize> = (0..n).filter(|&u| out_deg[u] == 0).collect();

    power_iterate(n, params, &dangling_nodes, |u, rank| {
        let mut acc = 0.0;
        match mask {
            // Fast path: unmasked full-graph pull (byte-identical to the original).
            None => {
                for &v in inc.neighbors(u as u32) {
                    // v is an in-neighbour of u; it contributes rank[v] / outdeg[v].
                    let od = out_deg[v as usize];
                    if od > 0 {
                        acc += rank[v as usize] / od as f64;
                    }
                }
            }
            // Subgraph pull: skip in-edges whose original row is masked out.
            Some(m) => {
                for (&v, &e) in inc.neighbors(u as u32).iter().zip(inc.edge_ids(u as u32)) {
                    if !m.keep(e) {
                        continue;
                    }
                    let od = out_deg[v as usize];
                    if od > 0 {
                        acc += rank[v as usize] / od as f64;
                    }
                }
            }
        }
        acc
    })
}

/// Weighted PageRank (pull-based). Identical to [`pagerank`] except a node's rank
/// is split among its out-edges in proportion to the edge weights rather than
/// uniformly: each in-neighbour `v` contributes `w * rank[v] / out_strength[v]`,
/// where `w` is the weight of the `v -> u` edge and `out_strength[v]` is `v`'s
/// total outgoing weight. A node with zero out-strength is dangling and its rank
/// is redistributed uniformly, as in the unweighted kernel.
///
/// `weights[e]` is the weight of original edge row `e`; `weights.len()` must equal
/// `topo.n_edges()`. Weights are gathered per CSR slot via `Adjacency::edge_ids`.
pub fn pagerank_weighted(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
    params: PageRankParams,
) -> Vec<f64> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );

    // Precompute weighted out-strength: the total outgoing weight of each node. Under
    // a mask, only kept out-edges contribute to the strength.
    let out = topo.out();
    let out_str: Vec<f64> = (0..n as u32)
        .map(|v| {
            out.edge_ids(v)
                .iter()
                .filter(|&&e| mask.is_none_or(|m| m.keep(e)))
                .map(|&e| weights[e as usize])
                .sum()
        })
        .collect();
    let inc = topo.incoming();

    // Dangling nodes (zero out-strength), listed once — see the unweighted kernel.
    let dangling_nodes: Vec<usize> = (0..n).filter(|&u| out_str[u] <= 0.0).collect();

    power_iterate(n, params, &dangling_nodes, |u, rank| {
        let mut acc = 0.0;
        // In-neighbours and their edge rows are aligned element-for-element.
        let nbrs = inc.neighbors(u as u32);
        let edges = inc.edge_ids(u as u32);
        for (&v, &e) in nbrs.iter().zip(edges) {
            if !mask.is_none_or(|m| m.keep(e)) {
                continue;
            }
            let os = out_str[v as usize];
            if os > 0.0 {
                acc += weights[e as usize] * rank[v as usize] / os;
            }
        }
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_sum_to_one_and_rank_hub_highest() {
        // A small graph where node 2 is a sink hub: 0->2, 1->2, 3->2, 2->0
        let t = Topology::build(4, vec![0, 1, 3, 2], vec![2, 2, 2, 0]);
        let pr = pagerank(&t, None, PageRankParams::default());
        let sum: f64 = pr.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum was {sum}");
        // The hub everyone points at should outrank every other node.
        let hub = pr[2];
        assert!(pr.iter().enumerate().all(|(i, &v)| i == 2 || v < hub));
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(pagerank(&t, None, PageRankParams::default()).is_empty());
    }

    #[test]
    fn weight_shifts_rank_toward_the_heavier_target() {
        // Node 0 splits between 1 and 2 (edge rows 0 and 1); 1 and 2 are sinks
        // pointing back at 0 so mass recirculates. Weighting the 0->2 edge far
        // heavier should make node 2 outrank node 1.
        let t = Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 0, 0]);
        let uniform = pagerank_weighted(&t, &[1.0, 1.0, 1.0, 1.0], None, PageRankParams::default());
        assert!(
            (uniform[1] - uniform[2]).abs() < 1e-9,
            "uniform ties 1 and 2"
        );

        // rows: 0=(0->1), 1=(0->2), 2=(1->0), 3=(2->0). Make 0->2 heavy.
        let weighted =
            pagerank_weighted(&t, &[1.0, 9.0, 1.0, 1.0], None, PageRankParams::default());
        assert!(weighted[2] > weighted[1], "heavier 0->2 lifts node 2");
        let sum: f64 = weighted.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum was {sum}");
    }

    #[test]
    fn zero_weight_out_edges_are_dangling() {
        // 0 -> 1 with weight 0 makes node 0 effectively dangling (no out-strength).
        let t = Topology::build(2, vec![0], vec![1]);
        let pr = pagerank_weighted(&t, &[0.0], None, PageRankParams::default());
        let sum: f64 = pr.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum was {sum}");
    }
}
