//! Triangle count — adjacency-intersection kernel. SKELETON.
//!
//! Treats edges as undirected. The canonical technique: build a symmetric,
//! deduplicated, *sorted* adjacency, then for each edge (u, v) count the size of
//! the sorted-list intersection `N(u) ∩ N(v)` (parallel merge-intersection over
//! Rayon). Sorting each adjacency list at topology-build time (or on first
//! intersection use) is the enabling step called out in the spec.
//!
//! Not yet implemented: returning a real per-node count requires the symmetric
//! sorted adjacency, which the skeleton `Topology` does not yet materialize
//! (it stores directed CSR only). Wiring that is the first task when this kernel
//! is picked up. Until then this returns zeros so the plumbing above it — the
//! `ur.triangle_count` expression, the DataFusion node, the Python binding — can
//! be exercised end-to-end against a defined (if trivial) result.

use crate::topology::Topology;

/// Per-node triangle count, dense-indexed. **Placeholder:** returns all zeros.
/// See module docs for the intended implementation.
pub fn triangle_count(topo: &Topology) -> Vec<u32> {
    // TODO(v0.1): symmetric sorted adjacency + parallel merge-intersection.
    // Port from GAP `tc.cc`.
    vec![0; topo.n_nodes()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_shape_is_correct() {
        let t = Topology::build(3, vec![0, 1, 2], vec![1, 2, 0]);
        assert_eq!(triangle_count(&t).len(), 3);
    }
}
