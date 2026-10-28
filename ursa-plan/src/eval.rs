//! Evaluating a predicate over Arrow batches: the one evaluator behind every
//! `filter` that does not run inside an engine query (#192).
//!
//! A predicate is lowered through [`crate::expr`], type-coerced and compiled to a
//! physical expression once per schema, then evaluated batch by batch. Nothing is
//! repartitioned, so rows keep their order: a plain frame's filter, a scan's
//! filter after its read, a subgraph view's edge mask (one bit per edge row), and
//! a filter the host's tail would have run all go through here.

use std::sync::{Arc, OnceLock};

use arrow::array::{Array, AsArray, BooleanArray, RecordBatch};
use arrow::compute::filter_record_batch;
use arrow::datatypes::{DataType, SchemaRef};
use datafusion::common::tree_node::TreeNode;
use datafusion::common::DFSchema;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{BinaryExpr, Expr, ExprSchemable, Operator};
use datafusion::physical_expr::PhysicalExpr;
use datafusion::prelude::SessionContext;
use serde_json::Value;

use crate::expr::{lower, parse_ursa_expr};

/// A predicate compiled against one schema.
pub type Compiled = Arc<dyn PhysicalExpr>;

/// The session predicates are planned on: type coercion and constant folding,
/// with no state of its own, so one is shared.
fn session() -> &'static SessionContext {
    static CTX: OnceLock<SessionContext> = OnceLock::new();
    CTX.get_or_init(SessionContext::new)
}

/// Compile a predicate (its JSON, role references already resolved) against
/// `schema`.
///
/// Float comparisons use the engine's total order everywhere (#199): NaN equals
/// NaN and is greater than every number, as in `sort()`.
pub fn compile(predicate: &Value, schema: &SchemaRef) -> Result<Compiled> {
    let df_schema = DFSchema::try_from(schema.as_ref().clone())?;
    let expr = logical(predicate, &df_schema)?;
    session().create_physical_expr(expr, &df_schema)
}

/// The predicate as a logical expression over `schema` (for a scan to push down).
pub fn logical(predicate: &Value, schema: &DFSchema) -> Result<Expr> {
    let expr = lower(&parse_ursa_expr(predicate)?)?;
    check_timezones(&expr, schema)?;
    Ok(expr)
}

/// A comparison of a timezone-aware timestamp with a naive one: an error, as it
/// was when pyarrow evaluated filters (DataFusion would read the naive side as
/// UTC, which is wrong whenever it meant local time). Raised as `ValueError`.
#[derive(Debug)]
pub struct TimezoneMismatch(pub String);

impl std::fmt::Display for TimezoneMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TimezoneMismatch {}

/// pyarrow's spelling of a timestamp type, as its error messages print it.
fn arrow_timestamp_name(dt: &DataType) -> String {
    use arrow::datatypes::TimeUnit;
    let DataType::Timestamp(unit, tz) = dt else {
        return dt.to_string();
    };
    let unit = match unit {
        TimeUnit::Second => "s",
        TimeUnit::Millisecond => "ms",
        TimeUnit::Microsecond => "us",
        TimeUnit::Nanosecond => "ns",
    };
    match tz {
        Some(tz) => format!("timestamp[{unit}, tz={tz}]"),
        None => format!("timestamp[{unit}]"),
    }
}

/// Reject a comparison between a timezone-aware and a naive timestamp in `expr`
/// ([`TimezoneMismatch`]).
pub fn check_timezones(expr: &Expr, schema: &DFSchema) -> Result<()> {
    use datafusion::common::tree_node::TreeNodeRecursion;
    let mut err = None;
    expr.apply(|e| {
        if let Expr::BinaryExpr(BinaryExpr { left, op, right }) = e {
            let comparison = matches!(
                op,
                Operator::Eq
                    | Operator::NotEq
                    | Operator::Lt
                    | Operator::LtEq
                    | Operator::Gt
                    | Operator::GtEq
            );
            if comparison {
                if let (Ok(l @ DataType::Timestamp(..)), Ok(r @ DataType::Timestamp(..))) =
                    (left.get_type(schema), right.get_type(schema))
                {
                    let aware = |t: &DataType| matches!(t, DataType::Timestamp(_, Some(_)));
                    if aware(&l) != aware(&r) {
                        // pyarrow's wording, in either order.
                        err = Some(TimezoneMismatch(format!(
                            "Cannot compare timestamp with timezone to timestamp without \
                             timezone, got: {} and {}",
                            arrow_timestamp_name(&l),
                            arrow_timestamp_name(&r)
                        )));
                        return Ok(TreeNodeRecursion::Stop);
                    }
                }
            }
        }
        Ok(TreeNodeRecursion::Continue)
    })?;
    match err {
        Some(e) => Err(DataFusionError::External(Box::new(e))),
        None => Ok(()),
    }
}

/// The predicate's value on `batch`, as a boolean array.
fn evaluate(predicate: &Compiled, batch: &RecordBatch) -> Result<BooleanArray> {
    let value = predicate.evaluate(batch)?.into_array(batch.num_rows())?;
    match value.data_type() {
        DataType::Boolean => Ok(value.as_boolean().clone()),
        DataType::Null => Ok(BooleanArray::new_null(batch.num_rows())),
        other => Err(DataFusionError::Execution(format!(
            "a filter predicate must be boolean; it produced {other:?}"
        ))),
    }
}

/// The rows of `batches` every predicate keeps (a null is not kept), in order.
/// Empty batches are dropped, but at least one batch, carrying the schema, is
/// returned.
pub fn filter_batches(
    batches: Vec<RecordBatch>,
    predicates: &[Compiled],
) -> Result<Vec<RecordBatch>> {
    let Some(schema) = batches.first().map(|b| b.schema()) else {
        return Ok(batches);
    };
    let mut out = Vec::with_capacity(batches.len());
    for mut batch in batches {
        for p in predicates {
            if batch.num_rows() == 0 {
                break;
            }
            let keep = evaluate(p, &batch)?;
            batch = filter_record_batch(&batch, &keep)?;
        }
        if batch.num_rows() > 0 {
            out.push(batch);
        }
    }
    if out.is_empty() {
        out.push(RecordBatch::new_empty(schema));
    }
    Ok(out)
}

/// One flag per row of `batches` (in order): whether every predicate keeps it,
/// a null counting as not kept. The bits of a subgraph-view edge mask.
pub fn keep_flags(batches: &[RecordBatch], predicates: &[Compiled]) -> Result<BooleanArray> {
    let mut parts: Vec<BooleanArray> = Vec::with_capacity(batches.len());
    for batch in batches {
        let mut keep: Option<BooleanArray> = None;
        for p in predicates {
            let m = evaluate(p, batch)?;
            // A null is not kept: fold the validity into the values.
            let m = match m.nulls() {
                Some(nulls) => BooleanArray::new(m.values() & nulls.inner(), None),
                None => m,
            };
            keep = Some(match keep {
                None => m,
                Some(k) => arrow::compute::and(&k, &m)?,
            });
        }
        parts.push(keep.unwrap_or_else(|| BooleanArray::from(vec![true; batch.num_rows()])));
    }
    let refs: Vec<&dyn Array> = parts.iter().map(|a| a as &dyn Array).collect();
    if refs.is_empty() {
        return Ok(BooleanArray::from(Vec::<bool>::new()));
    }
    Ok(arrow::compute::concat(&refs)?.as_boolean().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Float64Array, Int64Array};
    use arrow::datatypes::{Field, Schema};

    fn batch() -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("i", DataType::Int64, true),
                Field::new("f", DataType::Float64, true),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![Some(1), None, Some(3), Some(4)])),
                Arc::new(Float64Array::from(vec![
                    Some(0.5),
                    Some(f64::NAN),
                    None,
                    Some(2.0),
                ])),
            ],
        )
        .unwrap()
    }

    fn kept(pred: Value) -> Vec<usize> {
        let b = batch();
        let p = compile(&pred, &b.schema()).unwrap();
        let flags = keep_flags(&[b], &[p]).unwrap();
        (0..flags.len()).filter(|&i| flags.value(i)).collect()
    }

    fn cmp(op: &str, col: &str, value: Value) -> Value {
        serde_json::json!({"kind": "binary", "op": op,
            "left": {"kind": "col", "name": col}, "right": value})
    }

    #[test]
    fn nan_compares_in_total_order() {
        // f = [0.5, NaN, null, 2.0]: NaN is above every number and equals itself.
        let gt = cmp(">", "f", serde_json::json!({"kind": "lit", "value": 1.0}));
        assert_eq!(kept(gt.clone()), vec![1, 3]);
        let not = serde_json::json!({"kind": "unary", "op": "~", "operand": gt});
        assert_eq!(kept(not), vec![0]);
        let nan = serde_json::json!({"kind": "lit", "type": "float64", "value": "nan"});
        assert_eq!(kept(cmp("!=", "f", nan.clone())), vec![0, 3]);
        assert_eq!(kept(cmp("==", "f", nan)), vec![1]);
    }

    #[test]
    fn filters_keep_row_order_and_the_schema() {
        let b = batch();
        let p = compile(
            &cmp(">", "i", serde_json::json!({"kind": "lit", "value": 2})),
            &b.schema(),
        )
        .unwrap();
        let out = filter_batches(vec![b.clone(), b.clone()], std::slice::from_ref(&p)).unwrap();
        let rows: Vec<i64> = out
            .iter()
            .flat_map(|b| {
                b.column(0)
                    .as_primitive::<arrow::datatypes::Int64Type>()
                    .values()
                    .to_vec()
            })
            .collect();
        assert_eq!(rows, vec![3, 4, 3, 4]);
        let none = compile(
            &cmp(">", "i", serde_json::json!({"kind": "lit", "value": 9})),
            &b.schema(),
        )
        .unwrap();
        let out = filter_batches(vec![b], &[none]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].num_rows(), 0);
    }

    #[test]
    fn a_null_literal_keeps_nothing() {
        let eq_null = cmp("==", "i", serde_json::json!({"kind": "lit", "value": null}));
        assert_eq!(kept(eq_null), Vec::<usize>::new());
    }
}
