//! Whole-graph scalar statistics.
//!
//! These are the spec's deliberate *eager* exceptions to laziness: they build the
//! topology and return a plain number. `density` is unblocked (it needs only node
//! and edge counts); `diameter` / `avg_path_length` await the frontier/BFS kernels.

use arrow::array::Int64Array;
use datafusion::error::{DataFusionError, Result};

use crate::topology::build_topology;

/// Directed edge density: `m / (n * (n - 1))`, i.e. edges present over edges
/// possible (excluding self-loops). Returns `0.0` for fewer than two nodes.
///
/// Multiplicity is counted as given — duplicate `(src, dst)` rows and self-loops
/// contribute to `m`, so a multigraph can report density > 1. Call `.distinct()`
/// upstream for the simple-graph value.
pub fn density(src: &Int64Array, dst: &Int64Array) -> Result<f64> {
    let (topology, _ids) =
        build_topology(src, dst).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let n = topology.n_nodes() as f64;
    if n < 2.0 {
        return Ok(0.0);
    }
    Ok(topology.n_edges() as f64 / (n * (n - 1.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_of_a_directed_triangle() {
        // 3 nodes, 3 directed edges; possible = 3*2 = 6 -> 0.5
        let src = Int64Array::from(vec![0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 0]);
        assert!((density(&src, &dst).unwrap() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn single_node_has_zero_density() {
        let src = Int64Array::from(vec![0]);
        let dst = Int64Array::from(vec![0]);
        assert_eq!(density(&src, &dst).unwrap(), 0.0);
    }
}
