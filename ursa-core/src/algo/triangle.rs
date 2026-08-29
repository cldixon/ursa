//! Triangle count — adjacency-intersection kernel.
//!
//! Treats edges as undirected. Builds a symmetric, deduplicated, *sorted*
//! adjacency (out ∪ in neighbours, self-loops removed), then for each node counts
//! the edges among its neighbours via sorted-list intersection — a triangle
//! through `u` is exactly an edge between two of `u`'s neighbours.
//!
//! `result[u]` is the number of triangles containing node `u` (so each triangle
//! contributes to three entries; the whole-graph triangle total is `sum / 3`).
//! Parallelised over vertices with Rayon. Duplicate `(src, dst)` rows and
//! self-loops do not inflate counts (they are deduped / dropped when building the
//! undirected adjacency).
//!
//! This is the straightforward node-iterator form. The GAP-canonical
//! `tc.cc` orders vertices by degree and only intersects "upward" to halve the
//! work; that optimisation is a drop-in refinement here later.

use crate::parallel::*;

use crate::topology::{EdgeMask, Topology, UndirectedCsr};

/// Count of elements common to two sorted, deduplicated `u32` slices.
fn intersection_count(a: &[u32], b: &[u32]) -> u32 {
    let (mut i, mut j, mut count) = (0usize, 0usize, 0u32);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                count += 1;
                i += 1;
                j += 1;
            }
        }
    }
    count
}

/// Per-node triangle count over the topology's cached undirected adjacency.
pub(crate) fn per_node_triangles(adj: &UndirectedCsr) -> Vec<u32> {
    let n = adj.offsets.len().saturating_sub(1);
    (0..n as u32)
        .into_par_iter()
        .map(|u| {
            let nu = adj.neighbors(u);
            let mut count = 0u32;
            for (i, &a) in nu.iter().enumerate() {
                // nu[i + 1..] are all > a (nu is sorted & unique), so the
                // intersection with a's adjacency counts triangles {u, a, b}, b > a.
                count += intersection_count(&nu[i + 1..], adj.neighbors(a));
            }
            count
        })
        .collect()
}

/// Per-node triangle count, dense-indexed. A subgraph `mask` restricts triangles
/// to kept edges — computed over the masked undirected view (built per subgraph;
/// no CSR/id rebuild).
pub fn triangle_count(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<u32> {
    if topo.n_nodes() == 0 {
        return Vec::new();
    }
    match mask {
        None => per_node_triangles(topo.undirected()),
        Some(m) => per_node_triangles(&topo.undirected_masked(m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_triangle_counts_once_per_node() {
        // 0->1, 0->2, 1->2, 2->0  (undirected: the triangle 0-1-2)
        let t = Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 2, 0]);
        assert_eq!(triangle_count(&t, None), vec![1, 1, 1]);
    }

    #[test]
    fn path_has_no_triangles() {
        // 0->1->2 : no triangle
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        assert_eq!(triangle_count(&t, None), vec![0, 0, 0]);
    }

    #[test]
    fn k4_gives_three_per_node() {
        // complete graph on 4 nodes: every node is in C(3,2) = 3 triangles
        let src = vec![0, 0, 0, 1, 1, 2];
        let dst = vec![1, 2, 3, 2, 3, 3];
        let t = Topology::build(4, src, dst);
        assert_eq!(triangle_count(&t, None), vec![3, 3, 3, 3]);
    }

    #[test]
    fn ignores_self_loops_and_parallel_edges() {
        // triangle 0-1-2 plus a self-loop on 0 and a duplicated 0->1 edge
        let src = vec![0, 0, 1, 2, 0, 0];
        let dst = vec![1, 2, 2, 0, 0, 1];
        let t = Topology::build(3, src, dst);
        assert_eq!(triangle_count(&t, None), vec![1, 1, 1]);
    }
}
