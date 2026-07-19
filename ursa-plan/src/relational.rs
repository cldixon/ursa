//! The relational finish of a composed `collect()`.
//!
//! A composed pipeline — `edges.nodes().with_columns(pr=ur.pagerank(edges), ...)
//! .filter(...).sort(...).head(n)` — computes each graph algorithm into an
//! aligned `(id, value...)` batch (all algorithms over the same edges share one
//! `IdMap`, hence one id ordering), and then runs the *relational* tail
//! (`filter` / `sort` / `limit`) through DataFusion. This module is that tail: it
//! registers the assembled batch as an in-memory table and drives a DataFusion
//! `DataFrame`, so the relational engine — not Python, not hand-rolled loops —
//! does the filtering and ordering.
//!
//! The filter surface is intentionally small for the walking skeleton: a
//! conjunction of `column <op> literal` comparisons, which covers the canonical
//! `filter(ur.col("pagerank") > 0.001)`. Anything richer is rejected upstream in
//! Python with a clear message; widening it is expression-lowering work for
//! `crate::expr`.

use arrow::array::RecordBatch;
use arrow::compute::concat_batches;
use datafusion::error::{DataFusionError, Result};
use datafusion::prelude::{col, lit, SessionContext};

/// A single `column <op> literal` comparison. `op` is one of
/// `> >= < <= == !=`; `value` is numeric (DataFusion coerces to the column type).
#[derive(Debug, Clone)]
pub struct Comparison {
    pub column: String,
    pub op: String,
    pub value: f64,
}

/// Apply `filters` (conjoined), then optional `sort` (`(column, descending)`),
/// then optional `limit`, to `batch` via DataFusion.
pub fn apply_relational(
    batch: RecordBatch,
    filters: &[Comparison],
    sort: Option<(String, bool)>,
    limit: Option<usize>,
) -> Result<RecordBatch> {
    let schema = batch.schema();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DataFusionError::Execution(format!("failed to build runtime: {e}")))?;

    runtime.block_on(async move {
        let ctx = SessionContext::new();
        ctx.register_batch("frame", batch)?;
        let mut df = ctx.table("frame").await?;

        for f in filters {
            df = df.filter(comparison_expr(f)?)?;
        }
        if let Some((column, descending)) = sort {
            // Expr::sort(ascending, nulls_first)
            df = df.sort(vec![col(&column).sort(!descending, false)])?;
        }
        if let Some(n) = limit {
            df = df.limit(0, Some(n))?;
        }

        let batches = df.collect().await?;
        concat_batches(&schema, &batches).map_err(|e| DataFusionError::ArrowError(e, None))
    })
}

fn comparison_expr(f: &Comparison) -> Result<datafusion::logical_expr::Expr> {
    let c = col(&f.column);
    let v = lit(f.value);
    Ok(match f.op.as_str() {
        ">" => c.gt(v),
        ">=" => c.gt_eq(v),
        "<" => c.lt(v),
        "<=" => c.lt_eq(v),
        "==" => c.eq(v),
        "!=" => c.not_eq(v),
        other => {
            return Err(DataFusionError::NotImplemented(format!(
                "comparison operator {other:?} is not supported in collect() filters"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    fn frame() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("pagerank", DataType::Float64, false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3, 4])),
                Arc::new(Float64Array::from(vec![0.4, 0.1, 0.3, 0.2])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn filter_sort_limit_pipeline() {
        let out = apply_relational(
            frame(),
            &[Comparison {
                column: "pagerank".into(),
                op: ">".into(),
                value: 0.15,
            }],
            Some(("pagerank".into(), true)), // descending
            Some(2),
        )
        .unwrap();
        // rows with pagerank > 0.15: ids 1(0.4), 3(0.3), 4(0.2); top-2 desc: 0.4, 0.3
        assert_eq!(out.num_rows(), 2);
        let ids = out.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(ids.values(), &[1, 3]);
    }
}
