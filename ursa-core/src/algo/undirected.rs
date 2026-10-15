//! The undirected view as a symmetric directed topology — the `direction="both"`
//! input for the directed kernels (`pagerank`, `betweenness`).
//!
//! Edges are stored directed. To run a directed kernel "as if undirected" the
//! graph is re-expressed as its **simple undirected view**: every unordered pair
//! `{u, v}` (`u != v`) joined by at least one row in either direction becomes one
//! undirected edge, emitted as the two directed rows `u -> v` and `v -> u`.
//! Self-loops are dropped, and parallel or reciprocal rows collapse to the one
//! pair. That is exactly the [`Topology::undirected`] adjacency the triangle
//! family already uses (and caches), turned back into a [`Topology`] so the
//! unchanged directed kernels can run over it. Dense node ids are preserved: node
//! `i` of the view is node `i` of the source topology, including isolated nodes.
//!
//! The view is built per call and not cached: it is `2·m_undirected` rows, the
//! same order of memory as the undirected CSR it is built from.

use crate::parallel::*;

use super::triangle::undirected_view;
use crate::topology::{EdgeMask, Topology};

/// The unweighted symmetric view of `topo` (honoring `mask`): one `u -> v` and one
/// `v -> u` row per distinct unordered pair, self-loops dropped. Row order is by
/// source then target, so the result is deterministic.
pub fn symmetric_view(topo: &Topology, mask: Option<&EdgeMask>) -> Topology {
    let view = undirected_view(topo, mask);
    let adj = view.get();
    let n = topo.n_nodes();
    let mut src = Vec::with_capacity(adj.targets.len());
    for u in 0..n as u32 {
        src.extend(std::iter::repeat_n(u, adj.degree(u)));
    }
    Topology::build(n, src, adj.targets.clone())
}

/// The weighted symmetric view: as [`symmetric_view`], plus the per-row weight of
/// the returned topology. `weights[e]` is the weight of source row `e`. When
/// several rows join the same pair (parallel edges, or a `u -> v` matched by a
/// `v -> u`), the undirected edge takes the **minimum** of their weights — the
/// cheapest way across, which is what a shortest-path kernel would pick anyway.
/// Both directed rows of a pair carry that same weight.
pub fn symmetric_view_weighted(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
) -> (Topology, Vec<f64>) {
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    let n = topo.n_nodes();
    let out = topo.out();
    // Build the transpose before the parallel loop rather than lazily inside it.
    let inc = topo.incoming();
    let lists: Vec<Vec<(u32, f64)>> = (0..n as u32)
        .into_par_iter()
        .map(|u| {
            let mut nb: Vec<(u32, f64)> = Vec::new();
            for adj in [out, inc] {
                for (&w, &e) in adj.neighbors(u).iter().zip(adj.edge_ids(u)) {
                    if w != u && mask.is_none_or(|m| m.keep(e)) {
                        nb.push((w, weights[e as usize]));
                    }
                }
            }
            // Sort by neighbour, then weight, so the first entry of each run is the
            // pair's minimum weight; keep only that one.
            nb.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
            nb.dedup_by_key(|p| p.0);
            nb
        })
        .collect();
    let m: usize = lists.iter().map(Vec::len).sum();
    let mut src = Vec::with_capacity(m);
    let mut dst = Vec::with_capacity(m);
    let mut wts = Vec::with_capacity(m);
    for (u, nb) in lists.into_iter().enumerate() {
        for (v, w) in nb {
            src.push(u as u32);
            dst.push(v);
            wts.push(w);
        }
    }
    (Topology::build(n, src, dst), wts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(t: &Topology) -> Vec<(u32, u32)> {
        let mut r = Vec::new();
        for u in 0..t.n_nodes() as u32 {
            for &v in t.out().neighbors(u) {
                r.push((u, v));
            }
        }
        r.sort_unstable();
        r
    }

    #[test]
    fn collapses_reciprocal_parallel_and_self_loops() {
        // 0->1, 1->0 (reciprocal), 1->2 twice (parallel), 2->2 (self-loop), node 3
        // isolated only as a self-loop.
        let t = Topology::build(4, vec![0, 1, 1, 1, 2, 3], vec![1, 0, 2, 2, 2, 3]);
        let s = symmetric_view(&t, None);
        assert_eq!(s.n_nodes(), 4);
        assert_eq!(rows(&s), vec![(0, 1), (1, 0), (1, 2), (2, 1)]);
    }

    #[test]
    fn weighted_view_takes_the_minimum_weight_per_pair() {
        // 0->1 (5), 1->0 (2), 1->2 (3), 1->2 (7)
        let t = Topology::build(3, vec![0, 1, 1, 1], vec![1, 0, 2, 2]);
        let (s, w) = symmetric_view_weighted(&t, &[5.0, 2.0, 3.0, 7.0], None);
        let mut got: Vec<(u32, u32, f64)> = Vec::new();
        for u in 0..s.n_nodes() as u32 {
            for (&v, &e) in s.out().neighbors(u).iter().zip(s.out().edge_ids(u)) {
                got.push((u, v, w[e as usize]));
            }
        }
        got.sort_by_key(|a| (a.0, a.1));
        assert_eq!(
            got,
            vec![(0, 1, 2.0), (1, 0, 2.0), (1, 2, 3.0), (2, 1, 3.0)]
        );
    }

    #[test]
    fn mask_restricts_the_view() {
        use crate::topology::EdgeMask;
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        let mask = EdgeMask::from_bools(&[true, false]);
        assert_eq!(rows(&symmetric_view(&t, Some(&mask))), vec![(0, 1), (1, 0)]);
    }
}
