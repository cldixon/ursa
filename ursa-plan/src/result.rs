//! Assembling kernel outputs into Arrow `RecordBatch`es.
//!
//! This is the "Arrow arrays out" half of the operator contract: an `ursa-core`
//! kernel returns a dense, `u32`-indexed `Vec`, and this module gathers it into a
//! two-column `(id, value)` batch, translating dense indices back to user ids via
//! the [`IdMap`]. Kernels never touch user ids; this boundary does.

use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use ursa_core::algo::{
    connected_components_weak, degree, pagerank, triangle_count, PageRankParams,
};
use ursa_core::{IdMap, Topology};

use crate::logical::GraphAlgo;

/// Whether this algorithm has a real kernel wired into the execution path yet.
/// The walking skeleton supports the node-valued kernels that already exist in
/// `ursa-core`; the rest lower to the same node but are not yet executable.
pub fn is_executable(algo: &GraphAlgo) -> bool {
    matches!(
        algo,
        GraphAlgo::Degree { .. }
            | GraphAlgo::PageRank { .. }
            | GraphAlgo::ConnectedComponents { .. }
            | GraphAlgo::TriangleCount
    )
}

/// The output schema for an algorithm: `(id: Int64, <value>: <type>)`.
pub fn algorithm_schema(algo: &GraphAlgo) -> SchemaRef {
    let (name, dtype) = value_field(algo);
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new(name, dtype, false),
    ]))
}

fn value_field(algo: &GraphAlgo) -> (&'static str, DataType) {
    match algo {
        GraphAlgo::PageRank { .. } => ("pagerank", DataType::Float64),
        GraphAlgo::Degree { .. } => ("degree", DataType::UInt32),
        GraphAlgo::ConnectedComponents { .. } => ("component", DataType::UInt32),
        GraphAlgo::TriangleCount => ("triangle_count", DataType::UInt32),
        // Non-executable algorithms still declare a nominal Float64 value column.
        _ => ("value", DataType::Float64),
    }
}

/// Run `algo` over `topo` and materialize the `(id, value)` batch.
///
/// Precondition: [`is_executable`] returns `true` for `algo` (validated at
/// `GraphAlgorithmExec` construction). The dense value vector is produced by the
/// kernel; the id column is the `IdMap`'s dense→user vector, so row `i` of both
/// columns describes the same node.
pub fn algorithm_batch(topo: &Topology, ids: &IdMap, algo: &GraphAlgo) -> RecordBatch {
    let id_col: ArrayRef = Arc::new(Int64Array::from(ids.user_ids().to_vec()));
    let value_col: ArrayRef = match algo {
        GraphAlgo::Degree { direction } => {
            Arc::new(UInt32Array::from(degree(topo, (*direction).into())))
        }
        GraphAlgo::PageRank {
            damping,
            max_iter,
            tol,
        } => Arc::new(Float64Array::from(pagerank(
            topo,
            PageRankParams {
                damping: *damping,
                max_iter: *max_iter,
                tol: *tol,
            },
        ))),
        GraphAlgo::ConnectedComponents { .. } => {
            Arc::new(UInt32Array::from(connected_components_weak(topo)))
        }
        GraphAlgo::TriangleCount => Arc::new(UInt32Array::from(triangle_count(topo))),
        other => unreachable!("non-executable algorithm reached algorithm_batch: {other:?}"),
    };

    RecordBatch::try_new(algorithm_schema(algo), vec![id_col, value_col])
        .expect("id/value column lengths equal n_nodes and match the schema")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical::Direction;
    use crate::topology::build_topology;

    fn diamond() -> (Arc<Topology>, Arc<IdMap>) {
        // 0->1, 0->2, 1->2, 2->0
        let src = Int64Array::from(vec![0, 0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 2, 0]);
        build_topology(&src, &dst).unwrap()
    }

    #[test]
    fn degree_batch_shape_and_values() {
        let (topo, ids) = diamond();
        let batch = algorithm_batch(
            &topo,
            &ids,
            &GraphAlgo::Degree {
                direction: Direction::Out,
            },
        );
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema().field(1).name(), "degree");
        let deg = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        // node 0 (dense 0) has out-degree 2
        assert_eq!(deg.value(0), 2);
    }

    #[test]
    fn pagerank_batch_sums_to_one() {
        let (topo, ids) = diamond();
        let batch = algorithm_batch(
            &topo,
            &ids,
            &GraphAlgo::PageRank {
                damping: 0.85,
                max_iter: 30,
                tol: 1e-6,
            },
        );
        assert_eq!(batch.schema().field(1).name(), "pagerank");
        let pr = batch
            .column(1)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        let sum: f64 = pr.values().iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "pagerank sum was {sum}");
    }
}
