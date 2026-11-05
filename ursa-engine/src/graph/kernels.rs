//! Running an [`Algorithm`] on a graph index: one `ursa-core` kernel call each.
//!
//! This mirrors the unweighted, unmasked arms of `ursa-plan`'s kernel dispatch
//! (`KernelEval::compute` in `ursa-plan/src/result.rs`), which the Python
//! library runs; the parity tests (`tests/parity.rs`) check the two agree bit
//! for bit. What is repeated here, and must change with it:
//!
//! - `pagerank(direction => 'both')` runs the directed kernel over the
//!   topology's cached symmetric view (`Topology::symmetric`);
//! - `triangle_count` counts over the cached undirected view, and an empty
//!   graph gives an empty column;
//! - the output types: `pagerank` is `Float64`, the others `UInt32`.

use std::sync::Arc;

use datafusion::arrow::array::{ArrayRef, Float64Array, UInt32Array};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::{
    connected_components_strong, connected_components_weak, degree, louvain, pagerank,
    per_node_triangles, undirected_view, PageRankParams,
};
use ursa_core::{Direction, Topology};

use super::functions::Algorithm;
use super::index::GraphIndex;

/// The value column of `algorithm` over `topo`, one row per node in dense order.
pub fn run(topo: &Topology, algorithm: &Algorithm) -> ArrayRef {
    match *algorithm {
        Algorithm::Degree { direction } => {
            Arc::new(UInt32Array::from(degree(topo, None, direction)))
        }
        Algorithm::PageRank {
            damping,
            max_iter,
            tol,
            direction,
        } => {
            let params = PageRankParams {
                damping,
                max_iter,
                tol,
            };
            let scores = match direction {
                Direction::Both => pagerank(topo.symmetric(), None, params),
                _ => pagerank(topo, None, params),
            };
            Arc::new(Float64Array::from(scores))
        }
        Algorithm::ConnectedComponents { strong } => Arc::new(UInt32Array::from(if strong {
            connected_components_strong(topo, None)
        } else {
            connected_components_weak(topo, None)
        })),
        Algorithm::TriangleCount => {
            if topo.n_nodes() == 0 {
                return Arc::new(UInt32Array::from(Vec::<u32>::new()));
            }
            Arc::new(UInt32Array::from(per_node_triangles(
                undirected_view(topo, None).get(),
            )))
        }
        Algorithm::Louvain { resolution, seed } => {
            Arc::new(UInt32Array::from(louvain(topo, None, resolution, seed)))
        }
    }
}

/// The `(id, value...)` batch for `outputs` over `index`.
pub fn batch(index: &GraphIndex, outputs: &[Algorithm], schema: SchemaRef) -> Result<RecordBatch> {
    let mut columns = vec![index.ids.user_id_array()];
    columns.extend(outputs.iter().map(|a| run(&index.topology, a)));
    RecordBatch::try_new(schema, columns)
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}
