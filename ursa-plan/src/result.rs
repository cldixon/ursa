//! Assembling kernel outputs into Arrow `RecordBatch`es.
//!
//! The "Arrow arrays out" half of the operator contract. A query names one or
//! more node-valued algorithms as output columns; because every algorithm over
//! the same topology shares one `IdMap`, they are all row-aligned, so the result
//! is a single `(id, col_1, col_2, ...)` batch with dense→user id translation
//! done once, here, at the boundary. Kernels never see user ids.

use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, Int64Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use ursa_core::algo::{
    clustering_coefficient, connected_components_weak, degree, pagerank, triangle_count,
    PageRankParams,
};
use ursa_core::{IdMap, Topology};

use crate::logical::GraphAlgo;

/// One output column: a name and the algorithm that produces it.
pub type OutputColumn = (String, GraphAlgo);

/// Whether an algorithm has a kernel wired into the execution path.
pub fn is_executable(algo: &GraphAlgo) -> bool {
    matches!(
        algo,
        GraphAlgo::Degree { .. }
            | GraphAlgo::PageRank { .. }
            | GraphAlgo::ConnectedComponents { .. }
            | GraphAlgo::TriangleCount
            | GraphAlgo::ClusteringCoefficient
    )
}

/// Arrow value type produced by an algorithm.
fn value_type(algo: &GraphAlgo) -> DataType {
    match algo {
        GraphAlgo::PageRank { .. } | GraphAlgo::ClusteringCoefficient => DataType::Float64,
        _ => DataType::UInt32,
    }
}

/// The output schema for a query: `id: Int64` followed by one column per output.
pub fn query_schema(columns: &[OutputColumn]) -> SchemaRef {
    let mut fields = vec![Field::new("id", DataType::Int64, false)];
    for (name, algo) in columns {
        fields.push(Field::new(name, value_type(algo), false));
    }
    Arc::new(Schema::new(fields))
}

/// Run one algorithm over `topo`, returning its dense value column as Arrow.
fn value_array(topo: &Topology, algo: &GraphAlgo) -> ArrayRef {
    match algo {
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
        GraphAlgo::ClusteringCoefficient => {
            Arc::new(Float64Array::from(clustering_coefficient(topo)))
        }
        other => unreachable!("non-executable algorithm reached value_array: {other:?}"),
    }
}

/// Materialize the `(id, values...)` batch for a query.
///
/// Precondition: every column's algorithm is [`is_executable`] (validated at
/// query-build time). Row `i` of every column describes the same node.
pub fn query_batch(topo: &Topology, ids: &IdMap, columns: &[OutputColumn]) -> RecordBatch {
    let mut arrays: Vec<ArrayRef> = vec![Arc::new(Int64Array::from(ids.user_ids().to_vec()))];
    for (_, algo) in columns {
        arrays.push(value_array(topo, algo));
    }
    RecordBatch::try_new(query_schema(columns), arrays)
        .expect("all columns have length n_nodes and match the schema")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical::Direction;
    use crate::topology::build_topology;

    fn diamond() -> (Arc<Topology>, Arc<IdMap>) {
        let src = Int64Array::from(vec![0, 0, 1, 2]);
        let dst = Int64Array::from(vec![1, 2, 2, 0]);
        build_topology(&src, &dst).unwrap()
    }

    #[test]
    fn multi_column_batch_is_aligned() {
        let (topo, ids) = diamond();
        let columns = vec![
            (
                "deg".to_string(),
                GraphAlgo::Degree {
                    direction: Direction::Out,
                },
            ),
            (
                "pr".to_string(),
                GraphAlgo::PageRank {
                    damping: 0.85,
                    max_iter: 30,
                    tol: 1e-6,
                },
            ),
        ];
        let batch = query_batch(&topo, &ids, &columns);
        assert_eq!(batch.num_columns(), 3); // id, deg, pr
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema().field(1).name(), "deg");
        assert_eq!(batch.schema().field(2).name(), "pr");
        let deg = batch
            .column(1)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(deg.value(0), 2); // node 0 out-degree
    }
}
