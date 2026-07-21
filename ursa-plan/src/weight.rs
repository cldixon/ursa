//! Evaluating a `weight=` expression over edge columns to a dense `f64` array.
//!
//! Weight is a per-operation expression over the edge table (e.g.
//! `ur.col("amount") * ur.col("fx_rate")`), not a blessed column. The Python layer
//! serializes the expression tree to JSON; here it is parsed to an [`UrsaExpr`]
//! ([`crate::expr::parse_ursa_expr`]), lowered to a DataFusion expression
//! ([`crate::expr::lower`]), and evaluated against the edge `RecordBatch` via a
//! one-column projection. The result is an `f64` value per **edge row**, which
//! aligns with the CSR's `edge_ids` permutation so a weighted kernel gathers
//! `weights[edge_ids[k]]` per adjacency slot.

use arrow::array::{Array, Float64Array, RecordBatch};
use arrow::compute::cast;
use arrow::datatypes::DataType;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::SessionContext;

use crate::expr::{lower, parse_ursa_expr};

/// Evaluate the JSON-serialized weight expression against the edge batch, returning
/// one `f64` per edge row (in the batch's row order). Errors if the expression is
/// unsupported, references an unknown column, produces a non-numeric result, or
/// yields any null or negative weight (weights must be non-negative — Dijkstra's
/// requirement, and negatives are meaningless for weighted PageRank too).
pub fn evaluate_weight(edges: &RecordBatch, weight_json: &str) -> Result<Vec<f64>> {
    let value: serde_json::Value = serde_json::from_str(weight_json)
        .map_err(|e| DataFusionError::Execution(format!("invalid weight expression JSON: {e}")))?;
    let expr = parse_ursa_expr(&value)?;
    let df_expr = lower(&expr);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;
    let column = runtime.block_on(async move {
        let ctx = SessionContext::new();
        let df = ctx
            .read_batch(edges.clone())?
            .select(vec![df_expr.alias("__ursa_weight")])?;
        let batches = df.collect().await?;
        let n: usize = batches.iter().map(|b| b.num_rows()).sum();
        let mut out = Vec::with_capacity(n);
        for batch in &batches {
            let col = cast(batch.column(0), &DataType::Float64)
                .map_err(|e| DataFusionError::ArrowError(e, None))?;
            let col = col
                .as_any()
                .downcast_ref::<Float64Array>()
                .expect("cast to Float64 yields Float64Array");
            for i in 0..col.len() {
                if col.is_null(i) {
                    return Err(DataFusionError::Execution(
                        "weight expression produced a null value; weights must be non-null".into(),
                    ));
                }
                let w = col.value(i);
                if w < 0.0 {
                    return Err(DataFusionError::Execution(format!(
                        "weight expression produced a negative value ({w}); weights must be \
                         non-negative"
                    )));
                }
                out.push(w);
            }
        }
        Ok::<Vec<f64>, DataFusionError>(out)
    })?;
    Ok(column)
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
        let w = evaluate_weight(&edge_batch(), json).unwrap();
        assert_eq!(w, vec![3.0, 6.0, 5.0]);
    }

    #[test]
    fn evaluates_col_plus_lit() {
        let json = r#"{"kind":"binary","op":"+",
            "left":{"kind":"col","name":"amount"},
            "right":{"kind":"lit","value":10}}"#;
        let w = evaluate_weight(&edge_batch(), json).unwrap();
        assert_eq!(w, vec![12.0, 13.0, 15.0]);
    }

    #[test]
    fn rejects_negative_weights() {
        let json = r#"{"kind":"binary","op":"-",
            "left":{"kind":"col","name":"amount"},
            "right":{"kind":"lit","value":10}}"#;
        assert!(evaluate_weight(&edge_batch(), json).is_err());
    }

    #[test]
    fn errors_on_unknown_column() {
        let json = r#"{"kind":"col","name":"nope"}"#;
        assert!(evaluate_weight(&edge_batch(), json).is_err());
    }
}
