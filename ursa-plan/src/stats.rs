//! Whole-graph scalar statistics.
//!
//! These are the spec's deliberate *eager* exceptions to laziness: they build the
//! topology and return a plain number. `density` is unblocked (it needs only node
//! and edge counts); `diameter` / `avg_path_length` await the frontier/BFS kernels.

use std::collections::HashSet;
use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::connected_components_weak;
use ursa_core::Topology;

use crate::topology::build_topology;

/// Directed edge density off an already-built topology: `m / (n * (n - 1))`.
/// Returns `0.0` for fewer than two nodes.
fn density_of(topo: &Topology) -> f64 {
    let n = topo.n_nodes() as f64;
    if n < 2.0 {
        0.0
    } else {
        topo.n_edges() as f64 / (n * (n - 1.0))
    }
}

/// Directed edge density: `m / (n * (n - 1))`, i.e. edges present over edges
/// possible (excluding self-loops). Returns `0.0` for fewer than two nodes.
///
/// Multiplicity is counted as given — duplicate `(src, dst)` rows and self-loops
/// contribute to `m`, so a multigraph can report density > 1. Call `.distinct()`
/// upstream for the simple-graph value.
pub fn density(src: &Int64Array, dst: &Int64Array) -> Result<f64> {
    let (topology, _ids) =
        build_topology(src, dst).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    Ok(density_of(&topology))
}

/// A one-row whole-graph summary: `n_nodes, n_edges, density, avg_degree,
/// n_components`. Per spec §Open questions #4 the expensive `n_components` is
/// gated behind `full`; when `full` is false it is null (the column stays present
/// so the schema is stable). `avg_degree` is the mean out-degree (`m / n`).
pub fn describe(src: &Int64Array, dst: &Int64Array, full: bool) -> Result<RecordBatch> {
    let (topology, _ids) =
        build_topology(src, dst).map_err(|e| DataFusionError::Execution(e.to_string()))?;
    let n_nodes = topology.n_nodes() as i64;
    let n_edges = topology.n_edges() as i64;
    let density = density_of(&topology);
    let avg_degree = if n_nodes == 0 {
        0.0
    } else {
        n_edges as f64 / n_nodes as f64
    };
    let n_components: Option<i64> = if full {
        let distinct: HashSet<u32> = connected_components_weak(&topology).into_iter().collect();
        Some(distinct.len() as i64)
    } else {
        None
    };

    let schema = Arc::new(Schema::new(vec![
        Field::new("n_nodes", DataType::Int64, false),
        Field::new("n_edges", DataType::Int64, false),
        Field::new("density", DataType::Float64, false),
        Field::new("avg_degree", DataType::Float64, false),
        Field::new("n_components", DataType::Int64, true),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![n_nodes])),
            Arc::new(Int64Array::from(vec![n_edges])),
            Arc::new(Float64Array::from(vec![density])),
            Arc::new(Float64Array::from(vec![avg_degree])),
            Arc::new(Int64Array::from(vec![n_components])),
        ],
    )
    .map_err(|e| DataFusionError::ArrowError(e, None))
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

    #[test]
    fn describe_summarizes_a_directed_triangle() {
        use arrow::array::Float64Array;
        let src = Int64Array::from(vec![0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 0]);
        let batch = describe(&src, &dst, true).unwrap();
        assert_eq!(batch.num_rows(), 1);
        let get_i64 = |name: &str| {
            let i = batch.schema().index_of(name).unwrap();
            batch
                .column(i)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .value(0)
        };
        let get_f64 = |name: &str| {
            let i = batch.schema().index_of(name).unwrap();
            batch
                .column(i)
                .as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(0)
        };
        assert_eq!(get_i64("n_nodes"), 3);
        assert_eq!(get_i64("n_edges"), 3);
        assert!((get_f64("density") - 0.5).abs() < 1e-12);
        assert!((get_f64("avg_degree") - 1.0).abs() < 1e-12);
        assert_eq!(get_i64("n_components"), 1);
    }

    #[test]
    fn describe_gates_n_components_behind_full() {
        let src = Int64Array::from(vec![0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 0]);
        let batch = describe(&src, &dst, false).unwrap();
        let i = batch.schema().index_of("n_components").unwrap();
        // null unless full=true
        assert!(batch.column(i).is_null(0));
    }
}
