//! Closeness centrality — a BFS from every node, folding finite distances.
//!
//! The cheapest of the centralities: it is `bfs_distances` (the frontier kernel)
//! run once per source, parallelised over sources with Rayon.

use rayon::prelude::*;

use super::{bfs_distances, dijkstra_distances};
use crate::topology::{Direction, Topology};

/// Closeness centrality per node, directed (following out-edges): for each node
/// `u`, `reachable / Σ dist(u, v)` over the `reachable` nodes `v` at finite
/// distance `≥ 1`. A node that reaches nothing scores `0.0`.
///
/// Only reachable pairs contribute, so this is the standard form for
/// disconnected graphs (equivalently `reachable / total_distance`) rather than
/// the classic `(n − 1) / Σ dist`, which is undefined when some `v` is
/// unreachable. `O(n · (n + m))` — document the cost; use `sample`-based stats
/// for very large graphs.
pub fn closeness(topo: &Topology) -> Vec<f64> {
    let n = topo.n_nodes();
    (0..n)
        .into_par_iter()
        .map(|u| {
            let dist = bfs_distances(topo, u as u32, Direction::Out);
            let mut sum = 0i64;
            let mut reachable = 0i64;
            for d in dist {
                if d >= 1 {
                    sum += d as i64;
                    reachable += 1;
                }
            }
            if sum > 0 {
                reachable as f64 / sum as f64
            } else {
                0.0
            }
        })
        .collect()
}

/// Weighted closeness centrality: as [`closeness`], but distances are minimum
/// total edge weight (Dijkstra) rather than hop count. `reachable / Σ dist(u, v)`
/// over the reachable nodes at finite positive distance. `weights[e]` is the
/// weight of edge row `e` (non-negative; `len == n_edges`).
pub fn closeness_weighted(topo: &Topology, weights: &[f64]) -> Vec<f64> {
    let n = topo.n_nodes();
    (0..n)
        .into_par_iter()
        .map(|u| {
            let dist = dijkstra_distances(topo, weights, u as u32, Direction::Out);
            let mut sum = 0.0f64;
            let mut reachable = 0i64;
            for (v, d) in dist.into_iter().enumerate() {
                if v != u && d.is_finite() && d > 0.0 {
                    sum += d;
                    reachable += 1;
                }
            }
            if sum > 0.0 {
                reachable as f64 / sum
            } else {
                0.0
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closeness_of_a_directed_triangle() {
        // 0->1->2->0: from each node, distances {1, 2} to the other two.
        // reachable = 2, Σ dist = 3  ->  2/3.
        let t = Topology::build(3, vec![0, 1, 2], vec![1, 2, 0]);
        let c = closeness(&t);
        for &x in &c {
            assert!((x - 2.0 / 3.0).abs() < 1e-12, "got {x}");
        }
    }

    #[test]
    fn closeness_on_a_directed_line() {
        // 0->1->2->3. Node 0 reaches {1,2,3} at {1,2,3}: 3/6 = 0.5.
        // Node 2 reaches {3} at {1}: 1/1 = 1.0. Node 3 reaches nothing: 0.0.
        let t = Topology::build(4, vec![0, 1, 2], vec![1, 2, 3]);
        let c = closeness(&t);
        assert!((c[0] - 0.5).abs() < 1e-12, "got {}", c[0]);
        assert!((c[1] - 2.0 / 3.0).abs() < 1e-12, "got {}", c[1]);
        assert!((c[2] - 1.0).abs() < 1e-12, "got {}", c[2]);
        assert_eq!(c[3], 0.0);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(closeness(&t).is_empty());
    }

    #[test]
    fn weighted_uses_edge_costs() {
        // 0->1 (cost 1), 1->2 (cost 1), 0->2 (cost 5). From node 0: dist to 1 is 1,
        // to 2 is 2 (via 1, cheaper than the direct 5). reachable 2, Σ 3 -> 2/3.
        let t = Topology::build(3, vec![0, 1, 0], vec![1, 2, 2]);
        let c = closeness_weighted(&t, &[1.0, 1.0, 5.0]);
        assert!((c[0] - 2.0 / 3.0).abs() < 1e-12, "got {}", c[0]);
        // From node 1: only 2 reachable at cost 1 -> 1/1 = 1.
        assert!((c[1] - 1.0).abs() < 1e-12, "got {}", c[1]);
    }
}
