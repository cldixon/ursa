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
    /// `ur.lit(value)` — scalar literal (variant set widened during impl).
    LitI64(i64),
    LitF64(f64),
    LitStr(String),
    /// role references: `ur.src()`, `ur.dst()`, `ur.id()`
    Src,
    Dst,
    Id,
    /// binary ops (arithmetic / comparison / boolean) — op kept as a string in
    /// the skeleton; becomes an enum during implementation.
    Binary {
        op: String,
        left: Box<UrsaExpr>,
        right: Box<UrsaExpr>,
    },
    // graph verbs (`ur.degree`, `ur.pagerank`, `ur.neighbors(..).agg(..)`, ...)
    // lower to custom logical nodes rather than DfExpr — added during impl.
}

/// Lower an [`UrsaExpr`] to a DataFusion expression. Covers the subset a weight
/// expression uses (columns, numeric/string literals, `+ - * /`); anything else
/// returns a `NotImplemented` error rather than panicking, so widening the parser
/// without widening this function can never ship a process panic.
pub fn lower(expr: &UrsaExpr) -> Result<DfExpr> {
    use datafusion::logical_expr::{col, lit};
    Ok(match expr {
        UrsaExpr::Column(name) => col(name),
        UrsaExpr::LitI64(v) => lit(*v),
        UrsaExpr::LitF64(v) => lit(*v),
        UrsaExpr::LitStr(v) => lit(v.clone()),
        UrsaExpr::Binary { op, left, right } => {
            let operator = match op.as_str() {
                "+" => Operator::Plus,
                "-" => Operator::Minus,
                "*" => Operator::Multiply,
                "/" => Operator::Divide,
                other => {
                    return Err(DataFusionError::NotImplemented(format!(
                        "weight expression operator {other:?} is not supported (use + - * /)"
                    )))
                }
            };
            binary_expr(lower(left)?, operator, lower(right)?)
        }
        UrsaExpr::Src | UrsaExpr::Dst | UrsaExpr::Id => {
            return Err(DataFusionError::NotImplemented(
                "role references (src/dst/id) are not supported in a weight expression".to_string(),
            ))
        }
    })
}

/// Parse the JSON an Ursa `Expr` tree serializes to (from `ursa-py`'s Python
/// layer) into an [`UrsaExpr`]. Supports the subset a v0.1 **weight** expression
/// needs — `ur.col`, numeric/string literals, and `+ - * /` — erroring clearly on
/// anything else (role refs, comparisons, boolean ops, graph verbs).
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
            if let Some(i) = value.as_i64() {
                Ok(UrsaExpr::LitI64(i))
            } else if let Some(f) = value.as_f64() {
                Ok(UrsaExpr::LitF64(f))
            } else if let Some(s) = value.as_str() {
                Ok(UrsaExpr::LitStr(s.to_string()))
            } else {
                Err(err("unsupported literal type in weight expression"))
            }
        }
        "binary" => {
            let op = v
                .get("op")
                .and_then(Value::as_str)
                .ok_or_else(|| err("binary node missing 'op'"))?;
            if !matches!(op, "+" | "-" | "*" | "/") {
                return Err(DataFusionError::NotImplemented(format!(
                    "weight expression supports + - * / over columns and literals; \
                     operator {op:?} is not supported"
                )));
            }
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
        other => Err(DataFusionError::NotImplemented(format!(
            "weight expression node {other:?} is not supported \
             (use ur.col, numeric literals, and + - * /)"
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
    fn rejects_unsupported_nodes_and_ops() {
        // a comparison op is not a valid weight expression
        let cmp = serde_json::json!({
            "kind": "binary", "op": ">",
            "left": {"kind": "col", "name": "a"},
            "right": {"kind": "lit", "value": 1},
        });
        assert!(parse_ursa_expr(&cmp).is_err());
        // a role reference is not supported
        let role = serde_json::json!({"kind": "src"});
        assert!(parse_ursa_expr(&role).is_err());
    }

    #[test]
    fn lower_returns_err_instead_of_panicking() {
        // lower must not todo!()/panic on a variant the parser would normally reject;
        // it returns a NotImplemented error so widening the parser can't ship a panic.
        assert!(lower(&UrsaExpr::Src).is_err());
        assert!(lower(&UrsaExpr::Binary {
            op: ">".to_string(),
            left: Box::new(UrsaExpr::Column("a".into())),
            right: Box::new(UrsaExpr::LitI64(1)),
        })
        .is_err());
    }
}
