//! The dialect seam: lowering Ursa's Polars-shaped expressions to DataFusion.
//!
//! This is the module that contains the cost of the DataFusion decision. Ursa's
//! public expression dialect (`ur.col`, `ur.lit`, arithmetic, comparisons,
//! `.str`/`.dt`/`.list`, aggregations, and the graph verbs) is *not* DataFusion's
//! expression surface. Everything that translates one into the other lives here
//! and nowhere else, so the "we own a Polars-shaped frontend" tax is a single
//! bounded surface we can reason about, test, and evolve.
//!
//! `pl.Expr` objects are **not** accepted and no Polars-expression translator is
//! built or maintained (spec §The expression dialect / §Architecture). Interop is
//! Arrow, and Arrow interop is zero-copy regardless of engine.
//!
//! ## Shape
//!
//! The Python layer builds an [`UrsaExpr`] tree (mirrored across the FFI from
//! `ursa-py`); [`lower`] walks it to a `datafusion::logical_expr::Expr`, emitting
//! the graph verbs as the custom logical nodes in [`crate::logical`] instead.

use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::Expr as DfExpr;
use datafusion::logical_expr::{binary_expr, Operator};
use serde_json::Value;

/// A node in Ursa's expression dialect. This is the stable, engine-independent
/// representation the Python bindings construct; `ursa-plan` owns its translation.
#[derive(Debug, Clone)]
pub enum UrsaExpr {
    /// `ur.col("name")`
    Column(String),
    /// `ur.lit(value)` — scalar literal.
    LitI64(i64),
    LitF64(f64),
    LitStr(String),
    LitBool(bool),
    /// role references: `ur.src()`, `ur.dst()`, `ur.id()`. These are only reachable
    /// in a *weight* expression, where they are unsupported; predicate contexts
    /// resolve them to concrete columns in the Python layer before serializing, so
    /// `lower` never sees them from a filter.
    Src,
    Dst,
    Id,
    /// binary ops — the op string is validated in `lower`. Covers arithmetic
    /// (`+ - * /`), comparisons (`> >= < <= == !=`), and boolean (`&` / `|`).
    Binary {
        op: String,
        left: Box<UrsaExpr>,
        right: Box<UrsaExpr>,
    },
    /// unary ops — currently only boolean not (`~`).
    Unary {
        op: String,
        operand: Box<UrsaExpr>,
    },
    /// an aggregation over its operand — `ur.col("x").mean()` etc. Only valid inside
    /// a `group_by().agg()`; `func` is one of mean/sum/min/max/count/n_unique.
    Agg {
        func: String,
        operand: Box<UrsaExpr>,
    },
    /// `expr.alias("name")` — renames the (aggregation) output column.
    Alias {
        name: String,
        operand: Box<UrsaExpr>,
    },
    // Graph verbs (`ur.degree`, `ur.pagerank`, `ur.neighbors(..).agg(..)`, ...) are
    // not part of this expression enum; they lower to the custom logical nodes in
    // `crate::logical`/`crate::node` at plan-build time.
}

/// Lower an [`UrsaExpr`] to a DataFusion expression. Covers columns, literals
/// (numeric/string/bool), arithmetic (`+ - * /`), comparisons (`> >= < <= == !=`),
/// and boolean combinators (`&` / `|` / `~`) — the surface a filter predicate or a
/// weight expression uses. Anything else returns a `NotImplemented` error rather
/// than panicking, so widening the parser without widening this function can never
/// ship a process panic.
pub fn lower(expr: &UrsaExpr) -> Result<DfExpr> {
    use datafusion::logical_expr::{col, lit};
    Ok(match expr {
        UrsaExpr::Column(name) => col(name),
        UrsaExpr::LitI64(v) => lit(*v),
        UrsaExpr::LitF64(v) => lit(*v),
        UrsaExpr::LitStr(v) => lit(v.clone()),
        UrsaExpr::LitBool(v) => lit(*v),
        UrsaExpr::Binary { op, left, right } => {
            let (l, r) = (lower(left)?, lower(right)?);
            match op.as_str() {
                "+" => binary_expr(l, Operator::Plus, r),
                "-" => binary_expr(l, Operator::Minus, r),
                "*" => binary_expr(l, Operator::Multiply, r),
                "/" => binary_expr(l, Operator::Divide, r),
                ">" => l.gt(r),
                ">=" => l.gt_eq(r),
                "<" => l.lt(r),
                "<=" => l.lt_eq(r),
                "==" => l.eq(r),
                "!=" => l.not_eq(r),
                "&" => datafusion::logical_expr::and(l, r),
                "|" => datafusion::logical_expr::or(l, r),
                other => {
                    return Err(DataFusionError::NotImplemented(format!(
                        "expression operator {other:?} is not supported \
                         (use + - * /, > >= < <= == !=, & |)"
                    )))
                }
            }
        }
        UrsaExpr::Unary { op, operand } => match op.as_str() {
            "~" => datafusion::logical_expr::not(lower(operand)?),
            other => {
                return Err(DataFusionError::NotImplemented(format!(
                    "unary expression operator {other:?} is not supported (use ~)"
                )))
            }
        },
        UrsaExpr::Agg { func, operand } => {
            use datafusion::functions_aggregate::expr_fn::{
                avg, count, count_distinct, max, min, sum,
            };
            let inner = lower(operand)?;
            match func.as_str() {
                "mean" => avg(inner),
                "sum" => sum(inner),
                "min" => min(inner),
                "max" => max(inner),
                "count" => count(inner),
                "n_unique" => count_distinct(inner),
                other => {
                    return Err(DataFusionError::NotImplemented(format!(
                        "aggregation {other:?} is not supported \
                         (use mean/sum/min/max/count/n_unique)"
                    )))
                }
            }
        }
        UrsaExpr::Alias { name, operand } => lower(operand)?.alias(name),
        UrsaExpr::Src | UrsaExpr::Dst | UrsaExpr::Id => {
            return Err(DataFusionError::NotImplemented(
                "role references (src/dst/id) are not supported in a weight expression; \
                 in a filter they resolve to columns before lowering"
                    .to_string(),
            ))
        }
    })
}

/// Parse the JSON an Ursa `Expr` tree serializes to (from `ursa-py`'s Python
/// layer) into an [`UrsaExpr`]. Supports `ur.col`, numeric/string/bool literals,
/// binary ops (arithmetic + comparison + boolean), and unary `~`. The op string is
/// carried through unvalidated and checked in [`lower`], which is the single
/// operator-validity authority. Role refs / graph verbs / aggregations still error.
pub fn parse_ursa_expr(v: &Value) -> Result<UrsaExpr> {
    let err = |m: &str| DataFusionError::Execution(m.to_string());
    let kind = v
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| err("expression node missing 'kind'"))?;
    match kind {
        "col" => {
            let name = v
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| err("col node missing 'name'"))?;
            Ok(UrsaExpr::Column(name.to_string()))
        }
        "lit" => {
            let value = v
                .get("value")
                .ok_or_else(|| err("lit node missing 'value'"))?;
            // Check bool before the numeric ladder: serde reports a JSON bool only
            // via `as_bool`, so ordering is for clarity, not correctness.
            if let Some(b) = value.as_bool() {
                Ok(UrsaExpr::LitBool(b))
            } else if let Some(i) = value.as_i64() {
                Ok(UrsaExpr::LitI64(i))
            } else if let Some(f) = value.as_f64() {
                Ok(UrsaExpr::LitF64(f))
            } else if let Some(s) = value.as_str() {
                Ok(UrsaExpr::LitStr(s.to_string()))
            } else {
                Err(err("unsupported literal type in expression"))
            }
        }
        "binary" => {
            let op = v
                .get("op")
                .and_then(Value::as_str)
                .ok_or_else(|| err("binary node missing 'op'"))?;
            let left = parse_ursa_expr(
                v.get("left")
                    .ok_or_else(|| err("binary node missing 'left'"))?,
            )?;
            let right = parse_ursa_expr(
                v.get("right")
                    .ok_or_else(|| err("binary node missing 'right'"))?,
            )?;
            Ok(UrsaExpr::Binary {
                op: op.to_string(),
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        "unary" => {
            let op = v
                .get("op")
                .and_then(Value::as_str)
                .ok_or_else(|| err("unary node missing 'op'"))?;
            let operand = parse_ursa_expr(
                v.get("operand")
                    .ok_or_else(|| err("unary node missing 'operand'"))?,
            )?;
            Ok(UrsaExpr::Unary {
                op: op.to_string(),
                operand: Box::new(operand),
            })
        }
        "agg" => {
            let func = v
                .get("fn")
                .and_then(Value::as_str)
                .ok_or_else(|| err("agg node missing 'fn'"))?;
            let operand = parse_ursa_expr(
                v.get("operand")
                    .ok_or_else(|| err("agg node missing 'operand'"))?,
            )?;
            Ok(UrsaExpr::Agg {
                func: func.to_string(),
                operand: Box::new(operand),
            })
        }
        "alias" => {
            let name = v
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| err("alias node missing 'name'"))?;
            let operand = parse_ursa_expr(
                v.get("operand")
                    .ok_or_else(|| err("alias node missing 'operand'"))?,
            )?;
            Ok(UrsaExpr::Alias {
                name: name.to_string(),
                operand: Box::new(operand),
            })
        }
        other => Err(DataFusionError::NotImplemented(format!(
            "expression node {other:?} is not supported here \
             (use ur.col, literals, arithmetic/comparison/boolean ops, ~, and aggregations)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_lowers_col_times_col() {
        let json = serde_json::json!({
            "kind": "binary", "op": "*",
            "left": {"kind": "col", "name": "amount"},
            "right": {"kind": "col", "name": "fx_rate"},
        });
        let expr = parse_ursa_expr(&json).unwrap();
        // lowers to a DataFusion binary expr
        let _df = lower(&expr).unwrap();
        assert!(matches!(expr, UrsaExpr::Binary { .. }));
    }

    #[test]
    fn lowers_the_full_predicate_algebra() {
        // comparison
        let cmp = serde_json::json!({
            "kind": "binary", "op": ">",
            "left": {"kind": "col", "name": "a"},
            "right": {"kind": "col", "name": "b"},
        });
        lower(&parse_ursa_expr(&cmp).unwrap()).unwrap();
        // boolean composition of two comparisons
        let both = serde_json::json!({
            "kind": "binary", "op": "&",
            "left": {"kind": "binary", "op": ">",
                     "left": {"kind": "col", "name": "a"},
                     "right": {"kind": "lit", "value": 1}},
            "right": {"kind": "binary", "op": "<",
                      "left": {"kind": "col", "name": "b"},
                      "right": {"kind": "lit", "value": 10}},
        });
        lower(&parse_ursa_expr(&both).unwrap()).unwrap();
        // unary not
        let neg = serde_json::json!({
            "kind": "unary", "op": "~",
            "operand": {"kind": "binary", "op": "==",
                        "left": {"kind": "col", "name": "a"},
                        "right": {"kind": "lit", "value": 1}},
        });
        lower(&parse_ursa_expr(&neg).unwrap()).unwrap();
        // string and bool equality
        let seq = serde_json::json!({
            "kind": "binary", "op": "==",
            "left": {"kind": "col", "name": "x"},
            "right": {"kind": "lit", "value": "foo"},
        });
        lower(&parse_ursa_expr(&seq).unwrap()).unwrap();
        let beq = serde_json::json!({
            "kind": "binary", "op": "==",
            "left": {"kind": "col", "name": "flag"},
            "right": {"kind": "lit", "value": true},
        });
        lower(&parse_ursa_expr(&beq).unwrap()).unwrap();
        // nested arithmetic inside a predicate
        let arith = serde_json::json!({
            "kind": "binary", "op": ">",
            "left": {"kind": "binary", "op": "+",
                     "left": {"kind": "col", "name": "a"},
                     "right": {"kind": "col", "name": "b"}},
            "right": {"kind": "lit", "value": 5},
        });
        lower(&parse_ursa_expr(&arith).unwrap()).unwrap();
    }

    #[test]
    fn role_refs_and_unknown_ops_still_error() {
        // role references are only reachable in a weight expr and stay unsupported;
        // filters resolve them to columns in Python before serializing.
        assert!(parse_ursa_expr(&serde_json::json!({"kind": "src"})).is_err());
        // an unknown aggregation fn parses (fn carried through) but fails to lower.
        let bad_agg = serde_json::json!({
            "kind": "agg", "fn": "median", "operand": {"kind": "col", "name": "x"},
        });
        assert!(lower(&parse_ursa_expr(&bad_agg).unwrap()).is_err());
        // an unknown binary op parses (op carried through) but fails to lower.
        let bad = UrsaExpr::Binary {
            op: "^".to_string(),
            left: Box::new(UrsaExpr::Column("a".into())),
            right: Box::new(UrsaExpr::LitI64(1)),
        };
        assert!(lower(&bad).is_err());
    }

    #[test]
    fn parses_and_lowers_aliased_aggregation() {
        // group_by().agg() serializes alias(agg(col)); it must parse and lower.
        for func in ["mean", "sum", "min", "max", "count", "n_unique"] {
            let json = serde_json::json!({
                "kind": "alias", "name": "out",
                "operand": {"kind": "agg", "fn": func,
                            "operand": {"kind": "col", "name": "amount"}},
            });
            lower(&parse_ursa_expr(&json).unwrap()).unwrap();
        }
    }

    #[test]
    fn lower_returns_err_instead_of_panicking() {
        // lower must not todo!()/panic on a variant that has no lowering; it returns
        // a NotImplemented error so widening the parser can't ship a panic.
        assert!(lower(&UrsaExpr::Src).is_err());
        assert!(lower(&UrsaExpr::Unary {
            op: "?".to_string(),
            operand: Box::new(UrsaExpr::Column("a".into())),
        })
        .is_err());
    }
}
