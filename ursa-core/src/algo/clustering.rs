//! Local clustering coefficient — derived from triangles and undirected degree.
//!
//! For node `u` with undirected degree `k(u)` and `T(u)` triangles through it,
//! the local clustering coefficient is
//!
//! ```text
//! C(u) = 2 * T(u) / (k(u) * (k(u) - 1))
//! ```
//!
//! the fraction of `u`'s neighbour pairs that are themselves connected. Nodes
//! with degree < 2 have no pairs and are defined to 0.0. Reuses the undirected
//! adjacency and per-node triangle counts from [`super::triangle`], so it costs
//! essentially one extra pass over the (already computed) counts.

use crate::parallel::*;

use super::triangle::{per_node_triangles, undirected_view};
use crate::topology::{EdgeMask, Topology, UndirectedCsr};

/// Local clustering coefficient from per-node triangle counts already computed
/// over the *same* undirected adjacency.
///
/// Split out from [`clustering_coefficient`] because the triangle pass is the
/// expensive half and `triangle_count` computes exactly the same thing: a query
/// naming both kernels resolves the view and counts triangles once, then calls
/// this. `triangles` must be indexed by the same dense node ids as `adj` — pair it
/// with `per_node_triangles(adj)`, not with counts from a different view.
pub fn clustering_from_triangles(adj: &UndirectedCsr, triangles: &[u32]) -> Vec<f64> {
    (0..triangles.len())
        .into_par_iter()
        .map(|u| {
            let k = adj.degree(u as u32) as f64;
            if k < 2.0 {
                0.0
            } else {
                2.0 * triangles[u] as f64 / (k * (k - 1.0))
            }
        })
        .collect()
}

/// Per-node local clustering coefficient, dense-indexed (`0.0..=1.0`). A subgraph
/// `mask` restricts both the triangles and the degree to kept edges (computed over
/// the masked undirected view; no CSR/id rebuild).
pub fn clustering_coefficient(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<f64> {
    if topo.n_nodes() == 0 {
        return Vec::new();
    }
    let view = undirected_view(topo, mask);
    let adj = view.get();
    clustering_from_triangles(adj, &per_node_triangles(adj))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triangle_is_fully_clustered() {
        // 0-1-2 triangle: every node's two neighbours are connected -> C = 1.0
        let t = Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 2, 0]);
        assert_eq!(clustering_coefficient(&t, None), vec![1.0, 1.0, 1.0]);
    }

    #[test]
    fn path_center_has_zero_clustering() {
        // 0-1-2 path: node 1 has two unconnected neighbours -> C(1) = 0; ends deg<2
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        assert_eq!(clustering_coefficient(&t, None), vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn star_hub_has_zero_leaves_undefined() {
        // star: hub 0 connected to 1,2,3 (no edges among leaves) -> C(0) = 0
        let t = Topology::build(4, vec![0, 0, 0], vec![1, 2, 3]);
        let cc = clustering_coefficient(&t, None);
        assert_eq!(cc[0], 0.0); // hub: neighbours not interconnected
        assert_eq!(cc[1], 0.0); // leaf: degree 1 -> 0.0
    }

    #[test]
    fn half_clustered_node() {
        // node 0 has neighbours 1,2,3; one edge among them (1-2) -> C(0) = 2*1/(3*2) = 1/3
        let t = Topology::build(4, vec![0, 0, 0, 1], vec![1, 2, 3, 2]);
        let cc = clustering_coefficient(&t, None);
        assert!((cc[0] - 1.0 / 3.0).abs() < 1e-12);
    }
}
