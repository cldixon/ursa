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

use rayon::prelude::*;

use crate::topology::Topology;

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

/// Per-node triangle count, dense-indexed.
pub fn triangle_count(topo: &Topology) -> Vec<u32> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    let out = topo.out();
    let inc = topo.incoming();

    // Undirected, sorted, deduplicated adjacency (no self-loops).
    let adj: Vec<Vec<u32>> = (0..n)
        .into_par_iter()
        .map(|u| {
            let mut nbrs: Vec<u32> = out
                .neighbors(u as u32)
                .iter()
                .chain(inc.neighbors(u as u32))
                .copied()
                .filter(|&w| w != u as u32)
                .collect();
            nbrs.sort_unstable();
            nbrs.dedup();
            nbrs
        })
        .collect();

    // For each node u and each neighbour a, count neighbours of u that are also
    // neighbours of a and greater than a — counting each triangle once per apex.
    (0..n)
        .into_par_iter()
        .map(|u| {
            let nu = &adj[u];
            let mut count = 0u32;
            for (i, &a) in nu.iter().enumerate() {
                // nu[i + 1..] are all > a (nu is sorted & unique), so the
                // intersection with a's adjacency counts triangles {u, a, b}, b > a.
                count += intersection_count(&nu[i + 1..], &adj[a as usize]);
            }
            count
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_triangle_counts_once_per_node() {
        // 0->1, 0->2, 1->2, 2->0  (undirected: the triangle 0-1-2)
        let t = Topology::build(3, vec![0, 0, 1, 2], vec![1, 2, 2, 0]);
        assert_eq!(triangle_count(&t), vec![1, 1, 1]);
    }

    #[test]
    fn path_has_no_triangles() {
        // 0->1->2 : no triangle
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        assert_eq!(triangle_count(&t), vec![0, 0, 0]);
    }

    #[test]
    fn k4_gives_three_per_node() {
        // complete graph on 4 nodes: every node is in C(3,2) = 3 triangles
        let src = vec![0, 0, 0, 1, 1, 2];
        let dst = vec![1, 2, 3, 2, 3, 3];
        let t = Topology::build(4, src, dst);
        assert_eq!(triangle_count(&t), vec![3, 3, 3, 3]);
    }

    #[test]
    fn ignores_self_loops_and_parallel_edges() {
        // triangle 0-1-2 plus a self-loop on 0 and a duplicated 0->1 edge
        let src = vec![0, 0, 1, 2, 0, 0];
        let dst = vec![1, 2, 2, 0, 0, 1];
        let t = Topology::build(3, src, dst);
        assert_eq!(triangle_count(&t), vec![1, 1, 1]);
    }
}
