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
//!
//! Graph verbs share the grammar: a `{"kind": "graph", "verb": ...}` node is a
//! [`GraphVerb`], carrying its parameters and the edge input it runs over (see
//! [`crate::plan::EdgeInput`]). They never lower to a DataFusion expression; the
//! plan executor turns them into graph output columns.

use datafusion::common::ScalarValue;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::Expr as DfExpr;
use datafusion::logical_expr::{binary_expr, Operator};
use serde::Deserialize;
use serde_json::Value;

use crate::plan::EdgeInput;

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
    /// A typed literal the JSON scalars can't carry: null, NaN/infinity, a
    /// timestamp, date, time, duration or decimal (see [`parse_typed_literal`]).
    Lit(ScalarValue),
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
    // [`GraphVerb`] nodes of the same grammar. They have no scalar lowering: the
    // plan executor turns them into the output columns of a graph node.
}

/// A graph verb in the expression grammar: `{"kind": "graph", "verb": <name>,
/// "edges": <edge input>, ...params}`.
///
/// `deny_unknown_fields` makes a parameter the Python layer sends but this side
/// doesn't know a hard error rather than a silent drop. Parameter values are kept
/// as given and typed where the verb is lowered ([`GraphVerb::param`]), so a
/// malformed value is reported as an engine error at that point, as it was before
/// the plan IR. Which parameters a verb accepts is checked by the plan executor.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraphVerb {
    /// The verb's name: `pagerank`, `degree`, `neighbors_agg`, `layout_fr`, ...
    pub verb: String,
    /// The edges the verb runs over. `None` only for a malformed expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edges: Option<EdgeInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub damping: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iter: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tol: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Value>,
    /// `betweenness`: the fraction of sources to sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<Value>,
    /// A weight expression over edge columns (this grammar, unparsed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<Value>,
    /// `connected_components`: `"weak"` or `"strong"`, as given. An explicit
    /// null is kept (`Some(Null)`): it is an invalid mode, not the default.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub mode: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gravity: Option<Value>,
    /// Which output of a multi-output kernel this column takes (#115): 0 = x,
    /// 1 = y for a layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<Value>,
    /// Output dtype narrowing (#117): `"f32"` or `"f64"`, as given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dtype: Option<Value>,
    /// `neighbors_agg`: the aggregation, `ur.col(<name>).<fn>()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agg: Option<Value>,
    /// `neighbors_agg`: whether `from_=` was given (not supported yet).
    #[serde(default, rename = "from_", skip_serializing_if = "Option::is_none")]
    pub from: Option<bool>,
}

/// A field that is present (`null` included) deserializes to `Some`.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

impl GraphVerb {
    /// A parameter typed as `T` (`None` when absent or null). A value of the wrong
    /// type is an engine error, as for any malformed column spec.
    pub fn param<T: serde::de::DeserializeOwned>(value: &Option<Value>) -> Result<Option<T>> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(v) => serde_json::from_value(v.clone())
                .map(Some)
                .map_err(|e| DataFusionError::Execution(format!("invalid columns spec: {e}"))),
        }
    }

    /// The `direction` parameter (`"out"` when absent).
    pub fn direction(&self) -> Result<String> {
        Ok(Self::param::<String>(&self.direction)?.unwrap_or_else(|| "out".into()))
    }

    /// Parse a `{"kind": "graph", ...}` node.
    pub fn from_value(v: &Value) -> Result<GraphVerb> {
        let mut v = v.clone();
        if let Some(obj) = v.as_object_mut() {
            obj.remove("kind");
        }
        serde_json::from_value(v)
            .map_err(|e| DataFusionError::Execution(format!("invalid graph expression: {e}")))
    }

    /// The aggregation function of a `neighbors_agg` (`mean`, `sum`, ...).
    pub fn agg_fn(&self) -> Option<&str> {
        self.agg.as_ref()?.get("fn")?.as_str()
    }

    /// The attribute column a `neighbors_agg` aggregates, when its operand is a
    /// plain `ur.col(<name>)`.
    pub fn agg_column(&self) -> Option<&str> {
        let operand = self.agg.as_ref()?.get("operand")?;
        if operand.get("kind")?.as_str()? != "col" {
            return None;
        }
        operand.get("name")?.as_str()
    }
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
        UrsaExpr::Lit(v) => lit(v.clone()),
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
            if v.get("type").is_some() {
                return parse_typed_literal(v).map(UrsaExpr::Lit);
            }
            let value = v
                .get("value")
                .ok_or_else(|| err("lit node missing 'value'"))?;
            if value.is_null() {
                return Ok(UrsaExpr::Lit(ScalarValue::Null));
            }
            // Check bool before the numeric ladder: serde reports a JSON bool only
            // via `as_bool`, so ordering is for clarity, not correctness.
            if let Some(b) = value.as_bool() {
                Ok(UrsaExpr::LitBool(b))
            } else if let Some(i) = value.as_i64() {
                Ok(UrsaExpr::LitI64(i))
            } else if let Some(u) = value.as_u64() {
                // Past i64::MAX: pyarrow reads such an integer as uint64.
                Ok(UrsaExpr::Lit(ScalarValue::UInt64(Some(u))))
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

/// A typed literal: `{"kind": "lit", "type": <type>, "value": ...}`, the form a
/// literal with no JSON scalar takes (the Python layer derives it from
/// `pyarrow.scalar(value)`, so the type is the one pyarrow infers):
///
/// - `float64`: `"nan"`, `"inf"` or `"-inf"`;
/// - `timestamp`: an integer count of `unit` (`s`/`ms`/`us`/`ns`) since the
///   epoch, with an optional `tz`;
/// - `date32`: days since the epoch;
/// - `time`: an integer count of `unit` since midnight;
/// - `duration`: an integer count of `unit`;
/// - `decimal`: the unscaled value as a string, with `precision` and `scale`
///   (`decimal256` past 38 digits);
/// - `binary`: the bytes, hex-encoded;
/// - `arrow`: any other scalar, as a one-row Arrow IPC stream, hex-encoded.
pub fn parse_typed_literal(v: &Value) -> Result<ScalarValue> {
    let err = |m: String| DataFusionError::Execution(format!("invalid literal: {m}"));
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    let value = v.get("value").unwrap_or(&Value::Null);
    let int = || {
        value
            .as_i64()
            .ok_or_else(|| err(format!("{ty} literal needs an integer value")))
    };
    let unit = || v.get("unit").and_then(Value::as_str).unwrap_or("");
    let bad_unit = || err(format!("unknown {ty} unit {:?}", unit()));
    Ok(match ty {
        "float64" => ScalarValue::Float64(Some(match value.as_str() {
            Some("nan") => f64::NAN,
            Some("inf") => f64::INFINITY,
            Some("-inf") => f64::NEG_INFINITY,
            _ => return Err(err(format!("float64 literal {value}"))),
        })),
        "timestamp" => {
            let tz: Option<std::sync::Arc<str>> =
                v.get("tz").and_then(Value::as_str).map(Into::into);
            let i = Some(int()?);
            match unit() {
                "s" => ScalarValue::TimestampSecond(i, tz),
                "ms" => ScalarValue::TimestampMillisecond(i, tz),
                "us" => ScalarValue::TimestampMicrosecond(i, tz),
                "ns" => ScalarValue::TimestampNanosecond(i, tz),
                _ => return Err(bad_unit()),
            }
        }
        "date32" => {
            ScalarValue::Date32(Some(i32::try_from(int()?).map_err(|e| err(e.to_string()))?))
        }
        "time" => {
            let i = int()?;
            let i32_of = |i: i64| i32::try_from(i).map_err(|e| err(e.to_string()));
            match unit() {
                "s" => ScalarValue::Time32Second(Some(i32_of(i)?)),
                "ms" => ScalarValue::Time32Millisecond(Some(i32_of(i)?)),
                "us" => ScalarValue::Time64Microsecond(Some(i)),
                "ns" => ScalarValue::Time64Nanosecond(Some(i)),
                _ => return Err(bad_unit()),
            }
        }
        "duration" => {
            let i = Some(int()?);
            match unit() {
                "s" => ScalarValue::DurationSecond(i),
                "ms" => ScalarValue::DurationMillisecond(i),
                "us" => ScalarValue::DurationMicrosecond(i),
                "ns" => ScalarValue::DurationNanosecond(i),
                _ => return Err(bad_unit()),
            }
        }
        "decimal" => {
            let text = value
                .as_str()
                .ok_or_else(|| err(format!("decimal literal {value}")))?;
            let field = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(-1);
            let (precision, scale) = (field("precision"), field("scale"));
            if !(1..=76).contains(&precision) || !(-128..=127).contains(&scale) {
                return Err(err(format!("decimal({precision}, {scale})")));
            }
            if precision <= 38 {
                let digits = text
                    .parse::<i128>()
                    .map_err(|_| err(format!("decimal literal {value}")))?;
                ScalarValue::Decimal128(Some(digits), precision as u8, scale as i8)
            } else {
                let digits = arrow::datatypes::i256::from_string(text)
                    .ok_or_else(|| err(format!("decimal literal {value}")))?;
                ScalarValue::Decimal256(Some(digits), precision as u8, scale as i8)
            }
        }
        "binary" => ScalarValue::Binary(Some(
            hex(value.as_str().unwrap_or("")).map_err(|_| err("binary literal".into()))?,
        )),
        // Any other scalar pyarrow makes: a one-row Arrow IPC stream, hex-encoded.
        "arrow" => {
            let bytes =
                hex(value.as_str().unwrap_or("")).map_err(|_| err("arrow literal".into()))?;
            let mut reader =
                arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None)
                    .map_err(|e| err(e.to_string()))?;
            let batch = reader
                .next()
                .ok_or_else(|| err("empty arrow literal".into()))?
                .map_err(|e| err(e.to_string()))?;
            ScalarValue::try_from_array(batch.column(0), 0)?
        }
        other => return Err(err(format!("unknown literal type {other:?}"))),
    })
}

/// Bytes from their hex digits.
fn hex(s: &str) -> std::result::Result<Vec<u8>, std::num::ParseIntError> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap_or("x"), 16))
        .collect()
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
