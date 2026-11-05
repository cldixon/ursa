//! Running a graph node's calls on a graph index, through
//! `ursa_core::catalog`'s dispatch: the same implementation the Python
//! library runs, so the two agree bit for bit (`tests/parity.rs`).
//!
//! Every output of one node goes through one [`Evaluator`], so work shared
//! between its calls is done once: the triangle pass behind `triangle_count`
//! and `clustering_coefficient`, the symmetric views behind the
//! `direction => 'both'` forms, a layout's `x` and `y`, and a call repeated
//! under two names.

use std::collections::HashMap;

use datafusion::arrow::array::{Array, ArrayRef, AsArray};
use datafusion::arrow::compute::{cast, cast_with_options, CastOptions};
use datafusion::arrow::datatypes::{DataType, Float64Type, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result};
use ursa_core::catalog::{Evaluator, Inputs, OutputDtype};

use super::index::GraphIndex;
use super::node::Output;
use super::traversal;

/// The batch for `outputs` over `index`: `id` first when `per_node`, then one
/// column per output. `weights` holds one column per weight expression, one
/// `f64` per edge row in the order the index was built from; `values` one
/// value per node, in dense order, for a neighbour aggregation.
pub fn batch(
    index: &GraphIndex,
    outputs: &[Output],
    weights: &[std::sync::Arc<Vec<f64>>],
    values: Option<&[Option<f64>]>,
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
            attribute: o.call.spec().reads_attribute().then_some(values).flatten(),
        };
        let column = eval
            .column(&o.call, inputs, o.field, OutputDtype::F64)
            .map_err(|e| DataFusionError::Internal(format!("{}: {e}", o.call.name())))?;
        columns.push(column);
    }
    RecordBatch::try_new(schema, columns)
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

/// Whether a column of type `t` holds strings.
pub fn is_string(t: &DataType) -> bool {
    match t {
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => true,
        DataType::Dictionary(_, value) => is_string(value),
        _ => false,
    }
}

/// The node values of a neighbour aggregation, one per node of `index` in
/// dense order, from the `(id, value)` rows `batches` holds.
///
/// As the Python library reads a node frame's column: a row whose id is
/// null or not a node of the graph is skipped, a null value leaves its node
/// without one, numbers are read as `f64`, and strings (for `count` and
/// `n_unique`, checked when the query is planned) as a code per distinct
/// string. The ids follow the node-id rule of the traversals (#271): integers,
/// or strings that are exactly an integer on a graph with integer ids. A node given two rows is an error: which one
/// counts would depend on row order.
pub fn node_values(
    index: &GraphIndex,
    graph: &str,
    batches: Vec<Vec<ArrayRef>>,
) -> Result<Vec<Option<f64>>> {
    let id_type = index.ids.user_type();
    let wrong_id = |got: String| {
        let kind = match id_type {
            DataType::Int64 => "integers",
            _ => "strings",
        };
        DataFusionError::Execution(format!(
            "neighbors_agg(values =>): the first column must be node ids of graph '{graph}', \
             which are {kind}; got {got}"
        ))
    };
    let mut out: Vec<Option<f64>> = vec![None; index.ids.len()];
    let mut given = vec![false; out.len()];
    let mut codes: HashMap<String, f64> = HashMap::new();
    let strict = CastOptions {
        safe: false,
        ..CastOptions::default()
    };
    for batch in batches {
        let (ids, values) = (&batch[0], &batch[1]);
        // The traversals' rule (#271): integers, or strings that are exactly
        // an integer on a graph with integer ids.
        if let Some(row) = traversal::first_refused_node_id(ids, &id_type) {
            return Err(wrong_id(sql_text(ids, row)));
        }
        let user_ids = cast_with_options(ids, &id_type, &strict).map_err(|_| {
            // The first id that does not cast (out of range), as SQL writes it.
            let lenient = cast(ids, &id_type).ok();
            let row =
                lenient.and_then(|l| (0..ids.len()).find(|&i| ids.is_valid(i) && l.is_null(i)));
            wrong_id(row.map_or_else(
                || format!("a column of type {}", ids.data_type()),
                |row| sql_text(ids, row),
            ))
        })?;
        let dense = index
            .ids
            .dense_from_array(user_ids.as_ref())
            .map_err(|e| DataFusionError::Execution(format!("neighbors_agg(values =>): {e}")))?;
        let value_of: Box<dyn Fn(usize) -> Option<f64>> = if is_string(values.data_type()) {
            let text = cast(values, &DataType::Utf8)?;
            let text = text.as_string::<i32>().clone();
            let mut row_codes = Vec::with_capacity(text.len());
            for i in 0..text.len() {
                row_codes.push(text.is_valid(i).then(|| {
                    let next = codes.len() as f64;
                    *codes.entry(text.value(i).to_string()).or_insert(next)
                }));
            }
            Box::new(move |i| row_codes[i])
        } else {
            let numbers = cast(values, &DataType::Float64)?;
            let numbers = numbers.as_primitive::<Float64Type>().clone();
            Box::new(move |i| numbers.is_valid(i).then(|| numbers.value(i)))
        };
        for (row, node) in dense.iter().enumerate() {
            let Some(node) = *node else { continue };
            let node = node as usize;
            if std::mem::replace(&mut given[node], true) {
                return Err(DataFusionError::Execution(format!(
                    "neighbors_agg(values =>) gives node {} more than one value; give each \
                     node one row",
                    sql_text(ids, row)
                )));
            }
            out[node] = value_of(row);
        }
    }
    Ok(out)
}

/// Row `row` of `values` as SQL writes it (`'Valjean'`, `2.5`).
fn sql_text(values: &ArrayRef, row: usize) -> String {
    let Ok(mut value) = ScalarValue::try_from_array(values, row) else {
        return format!("a value of type {}", values.data_type());
    };
    while let ScalarValue::Dictionary(_, inner) = value {
        value = *inner;
    }
    match value.try_as_str() {
        Some(Some(s)) => format!("'{}'", s.replace('\'', "''")),
        _ => value.to_string(),
    }
}
