//! Running a graph node's calls on a graph index, through
//! `ursa_core::catalog`'s dispatch: the same implementation the Python
//! library runs, so the two agree bit for bit (`tests/parity.rs`).
//!
//! Every output of one node goes through one [`Evaluator`], so work shared
//! between its calls is done once: the triangle pass behind `triangle_count`
//! and `clustering_coefficient`, the symmetric views behind the
//! `direction => 'both'` forms, a layout's `x` and `y`, and a call repeated
//! under two names.

use std::cmp::Ordering;

use datafusion::arrow::array::{make_comparator, Array, ArrayRef, AsArray, UInt32Array};
use datafusion::arrow::compute::{
    cast, cast_with_options, concat, sort_to_indices, take, CastOptions, SortOptions,
};
use datafusion::arrow::datatypes::{DataType, Float64Type, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result};
use ursa_core::algo::AggKind;
use ursa_core::catalog::{Evaluator, Inputs, OutputDtype};

use super::index::GraphIndex;
use super::node::Output;
use super::traversal;

/// The batch for `outputs` over `index`: `id` first when `per_node`, then one
/// column per output. `weights` holds one column per weight expression, one
/// `f64` per edge row in the order the index was built from; `values` one
/// value per node, in dense order, for a neighbour aggregation. A column whose
/// type in `schema` is not the kernel's `Float64` is converted to it: a count
/// to an integer, the `min` or `max` of values read by order back to a value
/// (see [`neighbors_agg_type`]).
pub fn batch(
    index: &GraphIndex,
    outputs: &[Output],
    weights: &[std::sync::Arc<Vec<f64>>],
    values: Option<&NodeValues>,
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
            attribute: o
                .call
                .spec()
                .reads_attribute()
                .then_some(values.map(|v| v.values.as_slice()))
                .flatten(),
        };
        let column = eval
            .column(&o.call, inputs, o.field, OutputDtype::F64)
            .map_err(|e| DataFusionError::Internal(format!("{}: {e}", o.call.name())))?;
        let to = schema.field(columns.len()).data_type();
        let ordered = values.and_then(|v| v.ordered.as_ref());
        columns.push(retype(column, to, ordered)?);
    }
    RecordBatch::try_new(schema, columns)
        .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))
}

/// `column`, a kernel's `Float64`, as type `to`: an integer count cast, or
/// the index of a value in `ordered` (the `min` or `max` of values read by
/// order) taken from it.
fn retype(column: ArrayRef, to: &DataType, ordered: Option<&ArrayRef>) -> Result<ArrayRef> {
    if column.data_type() == to {
        return Ok(column);
    }
    match ordered {
        Some(ordered) if ordered.data_type() == to => {
            let positions: UInt32Array = column
                .as_primitive::<Float64Type>()
                .iter()
                .map(|p| p.map(|p| p as u32))
                .collect();
            Ok(take(ordered.as_ref(), &positions, None)?)
        }
        _ => Ok(cast(&column, to)?),
    }
}

/// The type of `neighbors_agg`'s column for `agg` over values of type
/// `value`: `UInt32` for `count` and `n_unique`, as `degree`'s; the values'
/// type for the `min` or `max` of values read by order (strings, dates,
/// times and timestamps, see [`is_ordered`]); else `Float64`.
pub fn neighbors_agg_type(agg: AggKind, value: &DataType) -> DataType {
    match agg {
        AggKind::Count | AggKind::NUnique => DataType::UInt32,
        AggKind::Min | AggKind::Max if is_ordered(value) => ordered_type(value),
        _ => DataType::Float64,
    }
}

/// Whether node values of type `t` are read by their order, not as numbers:
/// strings, dates, times, timestamps and durations.
pub fn is_ordered(t: &DataType) -> bool {
    match t {
        DataType::Date32
        | DataType::Date64
        | DataType::Timestamp(..)
        | DataType::Time32(_)
        | DataType::Time64(_)
        | DataType::Duration(_) => true,
        DataType::Dictionary(_, value) => is_ordered(value),
        t => is_string(t),
    }
}

/// The type node values of type `t` (read by order) are kept as: strings as
/// `Utf8`, a dictionary as its values.
fn ordered_type(t: &DataType) -> DataType {
    match t {
        DataType::Dictionary(_, value) => ordered_type(value),
        t if is_string(t) => DataType::Utf8,
        t => t.clone(),
    }
}

/// A neighbour aggregation's node values, one per node in dense order.
#[derive(Debug)]
pub struct NodeValues {
    /// Each node's value as the kernels read it: a number, or for values read
    /// by order, the index of its value in `ordered`.
    pub values: Vec<Option<f64>>,
    /// For values read by order ([`is_ordered`]): the distinct values,
    /// sorted, so the order of two indexes is that of their values.
    pub ordered: Option<ArrayRef>,
}

/// For each row of `columns` (values read by order), the index of its value
/// among the distinct values, sorted; and those values.
fn order_codes(columns: &[&ArrayRef]) -> Result<(Vec<Vec<Option<f64>>>, ArrayRef)> {
    let to = ordered_type(columns[0].data_type());
    let cast_columns = columns
        .iter()
        .map(|c| cast(c, &to))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let refs: Vec<&dyn Array> = cast_columns.iter().map(|c| c.as_ref()).collect();
    let all = concat(&refs)?;
    let sorted = sort_to_indices(&all, None, None)?;
    let compare = make_comparator(all.as_ref(), all.as_ref(), SortOptions::default())?;
    let mut codes: Vec<Option<f64>> = vec![None; all.len()];
    let mut distinct: Vec<u32> = Vec::new();
    for &row in sorted.values() {
        if all.is_null(row as usize) {
            continue;
        }
        let new = distinct
            .last()
            .is_none_or(|&last| compare(last as usize, row as usize) != Ordering::Equal);
        if new {
            distinct.push(row);
        }
        codes[row as usize] = Some((distinct.len() - 1) as f64);
    }
    let ordered = take(all.as_ref(), &UInt32Array::from(distinct), None)?;
    let mut per_column = Vec::with_capacity(columns.len());
    let mut rest = codes.as_slice();
    for c in columns {
        let (head, tail) = rest.split_at(c.len());
        per_column.push(head.to_vec());
        rest = tail;
    }
    Ok((per_column, ordered))
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
/// without one, and numbers are read as `f64`. Strings, dates, times and
/// timestamps (for `count`, `n_unique`, `min` and `max`, checked when the
/// query is planned) are read by order: as the index of the value among the
/// distinct values, sorted. The ids follow the node-id rule of the traversals (#271): integers,
/// or strings that are exactly an integer on a graph with integer ids. A node given two rows is an error: which one
/// counts would depend on row order.
pub fn node_values(
    index: &GraphIndex,
    graph: &str,
    batches: Vec<Vec<ArrayRef>>,
) -> Result<NodeValues> {
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
    let by_order = batches
        .first()
        .is_some_and(|b| is_ordered(b[1].data_type()));
    let (mut codes, ordered) = if by_order {
        let columns: Vec<&ArrayRef> = batches.iter().map(|b| &b[1]).collect();
        let (codes, ordered) = order_codes(&columns)?;
        (codes.into_iter(), Some(ordered))
    } else {
        (Vec::new().into_iter(), None)
    };
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
        let value_of: Box<dyn Fn(usize) -> Option<f64>> = if by_order {
            let row_codes = codes.next().unwrap_or_default();
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
    Ok(NodeValues {
        values: out,
        ordered,
    })
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
