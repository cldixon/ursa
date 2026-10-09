//! Evaluating a `weight=` expression over edge columns to a dense `f64` array.
//!
//! Weight is a per-operation expression over the edge table (e.g.
//! `ur.col("amount") * ur.col("fx_rate")`), not a blessed column. The Python layer
//! serializes the expression tree to JSON; here it is parsed to an `UrsaExpr`
//! ([`crate::expr::parse_ursa_expr`]), lowered to a DataFusion expression
//! ([`crate::expr::lower`]), and evaluated against the edge `RecordBatch` via a
//! one-column projection. The result is an `f64` value per **edge row**, which
//! aligns with the CSR's `edge_ids` permutation so a weighted kernel gathers
//! `weights[edge_ids[k]]` per adjacency slot.

use arrow::array::{Array, Float64Array, RecordBatch};
use arrow::compute::cast;
use arrow::datatypes::DataType;
use datafusion::common::DFSchema;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;

use crate::expr::{lower, parse_ursa_expr};

/// Whether a weight column's declared type can be a numeric weight (castable to
/// f64 without going through string parsing).
fn is_numeric(dt: &DataType) -> bool {
    use DataType::*;
    matches!(
        dt,
        Int8 | Int16
            | Int32
            | Int64
            | UInt8
            | UInt16
            | UInt32
            | UInt64
            | Float16
            | Float32
            | Float64
            | Decimal128(_, _)
            | Decimal256(_, _)
    )
}

/// Evaluate the JSON-serialized weight expression against the edge **batch stream**,
/// returning one `f64` per edge row, concatenated in the input batch order. The
/// edge table crosses the FFI as a list of batches (never concatenated into one),
/// so each is evaluated in turn and its weights appended — the `f64` output aligns
/// with the CSR's `edge_ids` because that same batch order built the topology.
///
/// The expression is type-coerced and compiled to a physical expression **once**,
/// then evaluated directly on each batch: no per-batch logical plan, optimizer
/// pass or async collect, which used to dominate on a scan's many small batches.
///
/// Errors if the expression is unsupported, references an unknown column, produces
/// a non-numeric result, or yields any null or negative weight (weights must be
/// non-negative — Dijkstra's requirement, and negatives are meaningless for
/// weighted PageRank too).
pub fn evaluate_weight(edges: &[RecordBatch], weight_json: &str) -> Result<Vec<f64>> {
    let value: serde_json::Value = serde_json::from_str(weight_json)
        .map_err(|e| DataFusionError::Execution(format!("invalid weight expression JSON: {e}")))?;
    let expr = parse_ursa_expr(&value)?;
    let df_expr = lower(&expr)?;

    let Some(first) = edges.first() else {
        return Ok(Vec::new());
    };
    let schema = first.schema();
    let physical = SessionContext::new()
        .create_physical_expr(df_expr, &DFSchema::try_from(schema.clone())?)?;
    // Branch on the *declared* result type, not on cast success: casting a
    // non-numeric column (e.g. a string weight= ur.col("region")) to Float64
    // silently yields nulls, which would misreport as "produced a null value".
    let dt = physical.data_type(&schema)?;
    if !is_numeric(&dt) {
        return Err(DataFusionError::Execution(format!(
            "weight expression must be numeric; it produced a column of type {dt:?} \
             (a string or other non-numeric column cannot be a weight)"
        )));
    }

    let total: usize = edges.iter().map(|b| b.num_rows()).sum();
    let mut out = Vec::with_capacity(total);
    for batch in edges {
        let col = physical.evaluate(batch)?.into_array(batch.num_rows())?;
        let col = cast(&col, &DataType::Float64)
            .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?;
        let col = col
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("cast to Float64 yields Float64Array");
        if col.null_count() > 0 {
            return Err(DataFusionError::Execution(
                "weight expression produced a null value; weights must be non-null".into(),
            ));
        }
        let values = col.values();
        if let Some(&w) = values.iter().find(|&&w| w < 0.0) {
            return Err(DataFusionError::Execution(format!(
                "weight expression produced a negative value ({w}); weights must be \
                 non-negative"
            )));
        }
        out.extend_from_slice(values);
    }
    Ok(out)
}

/// The weights for a weighted operation: `weight_json` evaluated over `edges`, one
/// per edge row, checked to align with a topology of `n_edges` edges.
///
/// `what` names the operation for the missing-edge-table error (e.g. "a weighted
/// algorithm", "weighted shortest_path"). Every weighted entry point goes through
/// here, so they report the same errors.
pub fn edge_weights(
    edges: Option<&[RecordBatch]>,
    weight_json: &str,
    n_edges: usize,
    what: &str,
) -> Result<Vec<f64>> {
    let edges = edges.ok_or_else(|| {
        DataFusionError::Execution(format!(
            "{what} needs the edge table, but none was provided"
        ))
    })?;
    let w = evaluate_weight(edges, weight_json)?;
    if w.len() != n_edges {
        return Err(DataFusionError::Execution(format!(
            "weight array length ({}) does not match the edge count ({n_edges}); the edge \
             table and the graph are misaligned",
            w.len()
        )));
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int64Array;
    use arrow::datatypes::{Field, Schema};
    use std::sync::Arc;

    fn edge_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("amount", DataType::Int64, false),
            Field::new("fx", DataType::Float64, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![2, 3, 5])),
                Arc::new(Float64Array::from(vec![1.5, 2.0, 1.0])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn evaluates_col_times_col() {
        let json = r#"{"kind":"binary","op":"*",
            "left":{"kind":"col","name":"amount"},
            "right":{"kind":"col","name":"fx"}}"#;
        let w = evaluate_weight(&[edge_batch()], json).unwrap();
        assert_eq!(w, vec![3.0, 6.0, 5.0]);
    }

    #[test]
    fn evaluates_col_plus_lit() {
        let json = r#"{"kind":"binary","op":"+",
            "left":{"kind":"col","name":"amount"},
            "right":{"kind":"lit","value":10}}"#;
        let w = evaluate_weight(&[edge_batch()], json).unwrap();
        assert_eq!(w, vec![12.0, 13.0, 15.0]);
    }

    #[test]
    fn evaluates_across_multiple_batches_in_order() {
        // The same three edges split across two batches must produce the same
        // per-row weights, concatenated in batch order (the alignment the CSR's
        // edge_ids relies on).
        let b0 = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("amount", DataType::Int64, false),
                Field::new("fx", DataType::Float64, false),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![2, 3])),
                Arc::new(Float64Array::from(vec![1.5, 2.0])),
            ],
        )
        .unwrap();
        let b1 = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("amount", DataType::Int64, false),
                Field::new("fx", DataType::Float64, false),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![5])),
                Arc::new(Float64Array::from(vec![1.0])),
            ],
        )
        .unwrap();
        let json = r#"{"kind":"binary","op":"*",
            "left":{"kind":"col","name":"amount"},
            "right":{"kind":"col","name":"fx"}}"#;
        let w = evaluate_weight(&[b0, b1], json).unwrap();
        assert_eq!(w, vec![3.0, 6.0, 5.0]);
    }

    #[test]
    fn rejects_negative_weights() {
        let json = r#"{"kind":"binary","op":"-",
            "left":{"kind":"col","name":"amount"},
            "right":{"kind":"lit","value":10}}"#;
        assert!(evaluate_weight(&[edge_batch()], json).is_err());
    }

    #[test]
    fn errors_on_unknown_column() {
        let json = r#"{"kind":"col","name":"nope"}"#;
        assert!(evaluate_weight(&[edge_batch()], json).is_err());
    }

    #[test]
    fn non_numeric_weight_reports_type_not_null() {
        // A string weight column must report "must be numeric", not the misleading
        // "produced a null value" a Utf8->Float64 cast-to-null would give.
        let schema = Arc::new(Schema::new(vec![Field::new(
            "region",
            DataType::Utf8,
            false,
        )]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(arrow::array::StringArray::from(vec!["us", "eu"]))],
        )
        .unwrap();
        let err = evaluate_weight(&[batch], r#"{"kind":"col","name":"region"}"#).unwrap_err();
        assert!(err.to_string().contains("must be numeric"), "got: {err}");
    }

    #[test]
    fn comparison_weight_expression_is_rejected_as_non_numeric() {
        // The expr seam now parses/lowers comparisons (for filters), so a weight
        // written as a predicate lowers to a Boolean column — the numeric guard must
        // still reject it rather than treating true/false as 1.0/0.0.
        let json = r#"{"kind":"binary","op":">",
            "left":{"kind":"col","name":"amount"},
            "right":{"kind":"lit","value":2}}"#;
        let err = evaluate_weight(&[edge_batch()], json).unwrap_err();
        assert!(err.to_string().contains("must be numeric"), "got: {err}");
    }
}
