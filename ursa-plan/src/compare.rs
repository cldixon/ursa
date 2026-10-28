//! Comparison semantics: which operand types compare, at which common type, and
//! how floats order (#192, #199).
//!
//! Filters used to run in pyarrow, whose comparison kernels decide what a
//! predicate means. DataFusion's own coercion differs in ways that change rows
//! silently (it compares timestamps at the coarser unit, parses strings as
//! numbers or dates, rounds large integers into floats, and orders floats by
//! `total_cmp`, where a negative-signed NaN sorts below every number and
//! `-0.0 != 0.0`). So every comparison is resolved here first, on every path
//! (plain frames, subgraph masks, scans, engine queries), the way pyarrow
//! dispatches it:
//!
//! - operand families pyarrow has no kernel for are rejected
//!   (`Function 'greater' has no kernel matching input types (int64, string)`);
//! - timestamps, durations and times compare at the finer unit;
//! - an integer meets a float as `float64`, checked to be exact (|v| <= 2^53),
//!   and a `uint64` meets a signed integer as `int64`, checked to fit, with
//!   pyarrow's "not in range" error otherwise;
//! - floats compare in total order with NaN canonical: every NaN equals every
//!   NaN and is greater than every number, and `-0.0 == 0.0` (#199).

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Int64Type, TimeUnit, UInt64Type};
use datafusion::common::tree_node::{Transformed, TreeNode, TreeNodeRecursion};
use datafusion::common::{DFSchema, ScalarValue};
use datafusion::error::{DataFusionError, Result};
use datafusion::functions::math::expr_fn::isnan;
use datafusion::logical_expr::{
    lit, when, BinaryExpr, Cast, ColumnarValue, Expr, ExprSchemable, Operator, ScalarFunctionArgs,
    ScalarUDF, ScalarUDFImpl, Signature, Volatility,
};

use crate::eval::TimezoneMismatch;

/// Resolve every comparison in `expr` as pyarrow would (see the module docs).
pub fn resolve_comparisons(expr: Expr, schema: &DFSchema) -> Result<Expr> {
    expr.transform_up(|e| {
        let Expr::BinaryExpr(BinaryExpr { left, op, right }) = &e else {
            return Ok(Transformed::no(e));
        };
        if !is_comparison(op) {
            return Ok(Transformed::no(e));
        }
        let (lt, rt) = (left.get_type(schema)?, right.get_type(schema)?);
        let (l, r) = match resolve(op, &lt, &rt)? {
            Resolved::Keep => return Ok(Transformed::no(e)),
            Resolved::Cast(lc, rc) => (
                coerce(left.as_ref().clone(), &lt, lc)?,
                coerce(right.as_ref().clone(), &rt, rc)?,
            ),
        };
        Ok(Transformed::yes(Expr::BinaryExpr(BinaryExpr::new(
            Box::new(l),
            *op,
            Box::new(r),
        ))))
    })
    .map(|t| t.data)
}

/// Whether `expr` (resolved) compares floats: a filter that must not be pushed
/// into a Parquet scan, whose min/max statistics leave NaN out.
pub fn compares_floats(expr: &Expr, schema: &DFSchema) -> bool {
    let mut floats = false;
    let _ = expr.apply(|e| {
        if let Expr::BinaryExpr(BinaryExpr { left, op, .. }) = e {
            if is_comparison(op) && left.get_type(schema).is_ok_and(|t| is_float(&t)) {
                floats = true;
                return Ok(TreeNodeRecursion::Stop);
            }
        }
        Ok(TreeNodeRecursion::Continue)
    });
    floats
}

fn is_comparison(op: &Operator) -> bool {
    matches!(
        op,
        Operator::Eq
            | Operator::NotEq
            | Operator::Lt
            | Operator::LtEq
            | Operator::Gt
            | Operator::GtEq
    )
}

fn is_float(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Float16 | DataType::Float32 | DataType::Float64
    )
}

/// How one side of a comparison is brought to the common type.
enum To {
    /// Leave it as it is.
    Same,
    /// Cast it.
    Cast(DataType),
    /// Cast it, checking every value converts exactly (pyarrow's safe cast).
    Checked(DataType),
}

enum Resolved {
    /// The engine's own coercion agrees with pyarrow's here.
    Keep,
    Cast(To, To),
}

/// The type a dictionary compares as: its values'.
fn value_type(dt: &DataType) -> &DataType {
    match dt {
        DataType::Dictionary(_, v) => v.as_ref(),
        other => other,
    }
}

fn int_bits(dt: &DataType) -> Option<(bool, u8)> {
    use DataType::*;
    Some(match dt {
        Int8 => (true, 8),
        Int16 => (true, 16),
        Int32 => (true, 32),
        Int64 => (true, 64),
        UInt8 => (false, 8),
        UInt16 => (false, 16),
        UInt32 => (false, 32),
        UInt64 => (false, 64),
        _ => return None,
    })
}

fn is_decimal(dt: &DataType) -> bool {
    matches!(dt, DataType::Decimal128(..) | DataType::Decimal256(..))
}

fn is_string(dt: &DataType) -> bool {
    matches!(dt, DataType::Utf8 | DataType::LargeUtf8)
}

fn is_binary(dt: &DataType) -> bool {
    matches!(dt, DataType::Binary | DataType::LargeBinary)
}

fn unit_rank(u: &TimeUnit) -> u8 {
    match u {
        TimeUnit::Second => 0,
        TimeUnit::Millisecond => 1,
        TimeUnit::Microsecond => 2,
        TimeUnit::Nanosecond => 3,
    }
}

fn finer(a: &TimeUnit, b: &TimeUnit) -> TimeUnit {
    if unit_rank(a) >= unit_rank(b) {
        *a
    } else {
        *b
    }
}

/// `To` for a side of type `dt` that must become `target`.
fn to(dt: &DataType, target: DataType) -> To {
    if value_type(dt) == &target {
        To::Same
    } else {
        To::Cast(target)
    }
}

/// pyarrow's dispatch for a comparison of `l` and `r`.
fn resolve(op: &Operator, l: &DataType, r: &DataType) -> Result<Resolved> {
    use DataType::*;
    let (lv, rv) = (value_type(l), value_type(r));
    if matches!(lv, Null) || matches!(rv, Null) {
        return Ok(Resolved::Keep);
    }
    let numeric = |t: &DataType| int_bits(t).is_some() || is_float(t) || is_decimal(t);
    let no_kernel = || Err(no_kernel(op, l, r));
    Ok(match (lv, rv) {
        (a, b) if numeric(a) && numeric(b) => {
            if is_float(a) || is_float(b) {
                // Floats compare as float64, canonical: an integer converts only
                // when exact, a decimal by value.
                let side = |t: &DataType| match int_bits(t) {
                    Some((_, 64)) => To::Checked(Float64),
                    _ => to(t, Float64),
                };
                Resolved::Cast(side(lv), side(rv))
            } else {
                match (int_bits(a), int_bits(b)) {
                    // A uint64 meets a signed integer as int64, checked.
                    (Some((false, 64)), Some((true, _))) => {
                        Resolved::Cast(To::Checked(Int64), to(rv, Int64))
                    }
                    (Some((true, _)), Some((false, 64))) => {
                        Resolved::Cast(to(lv, Int64), To::Checked(Int64))
                    }
                    // Otherwise a signed/unsigned mix widens to int64.
                    (Some((sa, _)), Some((sb, _))) if sa != sb => {
                        Resolved::Cast(to(lv, Int64), to(rv, Int64))
                    }
                    _ => Resolved::Keep,
                }
            }
        }
        (a, b) if is_string(a) && is_string(b) => Resolved::Keep,
        (Utf8View, Utf8View) => Resolved::Keep,
        // string_view against string: accepted (pyarrow had no kernel).
        (Utf8View, b) | (b, Utf8View) if is_string(b) => Resolved::Keep,
        (a, b) if (is_string(a) || is_binary(a)) && (is_string(b) || is_binary(b)) => {
            Resolved::Cast(to(lv, LargeBinary), to(rv, LargeBinary))
        }
        (Boolean, Boolean) => Resolved::Keep,
        (Date32 | Date64, Date32 | Date64) => Resolved::Cast(to(lv, Date64), to(rv, Date64)),
        (Date32 | Date64, Timestamp(u, tz)) => {
            Resolved::Cast(to(lv, Timestamp(*u, tz.clone())), To::Same)
        }
        (Timestamp(u, tz), Date32 | Date64) => {
            Resolved::Cast(To::Same, to(rv, Timestamp(*u, tz.clone())))
        }
        (Timestamp(lu, lz), Timestamp(ru, rz)) => {
            if lz.is_some() != rz.is_some() {
                if lu == ru {
                    return Err(DataFusionError::External(Box::new(TimezoneMismatch(
                        format!(
                            "Cannot compare timestamp with timezone to timestamp without \
                             timezone, got: {} and {}",
                            pa_name(l),
                            pa_name(r)
                        ),
                    ))));
                }
                return no_kernel();
            }
            if lu == ru {
                Resolved::Keep
            } else if lz.is_some() {
                // Two aware timestamps of different units: no kernel.
                return no_kernel();
            } else {
                let u = finer(lu, ru);
                Resolved::Cast(to(lv, Timestamp(u, None)), to(rv, Timestamp(u, None)))
            }
        }
        (Duration(lu), Duration(ru)) => {
            let u = finer(lu, ru);
            Resolved::Cast(to(lv, Duration(u)), to(rv, Duration(u)))
        }
        (Time32(lu) | Time64(lu), Time32(ru) | Time64(ru)) => {
            let u = finer(lu, ru);
            let t = match u {
                TimeUnit::Second | TimeUnit::Millisecond => Time32(u),
                _ => Time64(u),
            };
            Resolved::Cast(to(lv, t.clone()), to(rv, t))
        }
        _ => return no_kernel(),
    })
}

/// pyarrow's error for operand types it has no comparison kernel for.
fn no_kernel(op: &Operator, l: &DataType, r: &DataType) -> DataFusionError {
    let f = match op {
        Operator::Eq => "equal",
        Operator::NotEq => "not_equal",
        Operator::Lt => "less",
        Operator::LtEq => "less_equal",
        Operator::Gt => "greater",
        _ => "greater_equal",
    };
    DataFusionError::Execution(format!(
        "Function '{f}' has no kernel matching input types ({}, {})",
        pa_name(l),
        pa_name(r)
    ))
}

/// pyarrow's spelling of an Arrow type, as its messages print it.
pub fn pa_name(dt: &DataType) -> String {
    use DataType::*;
    let unit = |u: &TimeUnit| match u {
        TimeUnit::Second => "s",
        TimeUnit::Millisecond => "ms",
        TimeUnit::Microsecond => "us",
        TimeUnit::Nanosecond => "ns",
    };
    match dt {
        Null => "null".into(),
        Boolean => "bool".into(),
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => {
            dt.to_string().to_lowercase()
        }
        Float16 => "halffloat".into(),
        Float32 => "float".into(),
        Float64 => "double".into(),
        Utf8 => "string".into(),
        LargeUtf8 => "large_string".into(),
        Utf8View => "string_view".into(),
        Binary => "binary".into(),
        LargeBinary => "large_binary".into(),
        Date32 => "date32[day]".into(),
        Date64 => "date64[ms]".into(),
        Timestamp(u, None) => format!("timestamp[{}]", unit(u)),
        Timestamp(u, Some(tz)) => format!("timestamp[{}, tz={tz}]", unit(u)),
        Duration(u) => format!("duration[{}]", unit(u)),
        Time32(u) => format!("time32[{}]", unit(u)),
        Time64(u) => format!("time64[{}]", unit(u)),
        Decimal128(p, s) => format!("decimal128({p}, {s})"),
        Decimal256(p, s) => format!("decimal256({p}, {s})"),
        List(f) | LargeList(f) => format!("list<item: {}>", pa_name(f.data_type())),
        Dictionary(k, v) => format!(
            "dictionary<values={}, indices={}, ordered=0>",
            pa_name(v),
            pa_name(k)
        ),
        other => other.to_string(),
    }
}

/// Bring one side to its common type; a float side becomes canonical.
fn coerce(e: Expr, dt: &DataType, how: To) -> Result<Expr> {
    let (e, target) = match how {
        To::Same => (e, value_type(dt).clone()),
        To::Cast(t) => (Expr::Cast(Cast::new(Box::new(e), t.clone())), t),
        To::Checked(t) => (checked_cast(e, t.clone()), t),
    };
    Ok(if is_float(&target) { canonical(e) } else { e })
}

/// A float made canonical for comparison: every NaN the positive NaN, `-0.0`
/// as `0.0`, so total order reads "NaN equals NaN, above every number".
pub fn canonical(e: Expr) -> Expr {
    if let Expr::Literal(v, _) = &e {
        if let Some(c) = canonical_scalar(v) {
            return lit(c);
        }
        return e;
    }
    let plus_zero = Expr::BinaryExpr(BinaryExpr::new(
        Box::new(e.clone()),
        Operator::Plus,
        Box::new(lit(0.0_f64)),
    ));
    when(isnan(e), lit(f64::NAN))
        .otherwise(plus_zero)
        .expect("a CASE with one branch and an ELSE builds")
}

fn canonical_scalar(v: &ScalarValue) -> Option<ScalarValue> {
    let f = match v {
        ScalarValue::Float64(Some(f)) => *f,
        ScalarValue::Float32(Some(f)) => *f as f64,
        _ => return None,
    };
    Some(ScalarValue::Float64(Some(if f.is_nan() {
        f64::NAN
    } else {
        f + 0.0
    })))
}

/// `CAST(e AS target)` that fails, as pyarrow's safe cast does, on a value that
/// doesn't convert exactly: an integer past 2^53 into `float64`, a `uint64` past
/// `i64::MAX` into `int64`.
fn checked_cast(e: Expr, target: DataType) -> Expr {
    let udf = ScalarUDF::new_from_impl(CheckedCast {
        signature: Signature::any(1, Volatility::Immutable),
        target,
    });
    udf.call(vec![e])
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct CheckedCast {
    signature: Signature,
    target: DataType,
}

const F64_EXACT: i128 = 1 << 53;

impl ScalarUDFImpl for CheckedCast {
    fn name(&self) -> &str {
        "ursa_checked_cast"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _args: &[DataType]) -> Result<DataType> {
        Ok(self.target.clone())
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let [arg] = args.args.as_slice() else {
            return Err(DataFusionError::Internal(
                "ursa_checked_cast takes one argument".into(),
            ));
        };
        let scalar = matches!(arg, ColumnarValue::Scalar(_));
        let array: ArrayRef = arg.to_array(1.max(if scalar { 1 } else { args.number_rows }))?;
        let range = |lo: i128, hi: i128| -> Result<()> {
            let check = |v: i128| -> Result<()> {
                if v < lo || v > hi {
                    return Err(DataFusionError::Execution(format!(
                        "Integer value {v} not in range: {lo} to {hi}"
                    )));
                }
                Ok(())
            };
            match array.data_type() {
                DataType::Int64 => array
                    .as_primitive::<Int64Type>()
                    .iter()
                    .flatten()
                    .try_for_each(|v| check(v as i128)),
                DataType::UInt64 => array
                    .as_primitive::<UInt64Type>()
                    .iter()
                    .flatten()
                    .try_for_each(|v| check(v as i128)),
                _ => Ok(()),
            }
        };
        let unsigned = array.data_type() == &DataType::UInt64;
        match &self.target {
            DataType::Float64 if unsigned => range(0, F64_EXACT)?,
            DataType::Float64 => range(-F64_EXACT, F64_EXACT)?,
            DataType::Int64 => range(0, i64::MAX as i128)?,
            _ => {}
        }
        let out = cast(&array, &self.target)?;
        Ok(if scalar {
            ColumnarValue::Scalar(ScalarValue::try_from_array(&out, 0)?)
        } else {
            ColumnarValue::Array(Arc::new(out) as ArrayRef)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyarrow_type_names() {
        assert_eq!(pa_name(&DataType::Int64), "int64");
        assert_eq!(pa_name(&DataType::Float64), "double");
        assert_eq!(
            pa_name(&DataType::Timestamp(
                TimeUnit::Nanosecond,
                Some("UTC".into())
            )),
            "timestamp[ns, tz=UTC]"
        );
        assert_eq!(pa_name(&DataType::Decimal128(5, 2)), "decimal128(5, 2)");
    }

    #[test]
    fn mismatched_families_have_no_kernel() {
        let err = resolve(&Operator::Gt, &DataType::Int64, &DataType::Utf8)
            .err()
            .unwrap();
        assert!(err
            .to_string()
            .contains("Function 'greater' has no kernel matching input types (int64, string)"));
        assert!(resolve(&Operator::Eq, &DataType::Boolean, &DataType::Float64).is_err());
        let ts = |u, tz: Option<&str>| DataType::Timestamp(u, tz.map(Into::into));
        // aware vs naive: the same unit is the ValueError, units that differ no kernel.
        let same = resolve(
            &Operator::Gt,
            &ts(TimeUnit::Microsecond, Some("UTC")),
            &ts(TimeUnit::Microsecond, None),
        );
        assert!(matches!(same, Err(DataFusionError::External(_))));
        let units = resolve(
            &Operator::Gt,
            &ts(TimeUnit::Nanosecond, Some("UTC")),
            &ts(TimeUnit::Microsecond, None),
        );
        assert!(matches!(units, Err(DataFusionError::Execution(_))));
    }
}
