//! Running a graph node's calls on a graph index, through
//! `ursa_core::catalog`'s dispatch: the same implementation the Python
//! library runs, so the two agree bit for bit (`tests/parity.rs`).
//!
//! Every output of one node goes through one [`Evaluator`], so work shared
//! between its calls is done once: the triangle pass behind `triangle_count`
//! and `clustering_coefficient`, the symmetric views behind the
//! `direction => 'both'` forms, a layout's `x` and `y`, and a call repeated
//! under two names.

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::error::{DataFusionError, Result};
use ursa_core::catalog::{Evaluator, Inputs, OutputDtype};

use super::index::GraphIndex;
use super::node::Output;

/// The batch for `outputs` over `index`: `id` first when `per_node`, then one
/// column per output. `weights` holds one column per weight expression, one
/// `f64` per edge row in the order the index was built from.
pub fn batch(
    index: &GraphIndex,
    outputs: &[Output],
    weights: &[std::sync::Arc<Vec<f64>>],
    per_node: bool,
    schema: SchemaRef,
) -> Result<RecordBatch> {
    let mut columns = Vec::with_capacity(outputs.len() + 1);
    if per_node {
        columns.push(index.ids.user_id_array());
    }
    // The evaluator memoizes by weight address: the columns outlive it.
    let mut eval = Evaluator::new(&index.topology, None);
    for o in outputs {
        let inputs = Inputs {
            weights: o.weight.map(|i| weights[i].as_slice()),
            attribute: None,
        };
        let column = eval
            .column(&o.call, inputs, o.field, OutputDtype::F64)
            .map_err(|e| DataFusionError::Internal(format!("{}: {e}", o.call.name())))?;
        columns.push(column);
    }
    RecordBatch::try_new(schema, columns)
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}
