//! DuckDB's names for functions DataFusion has under another name (#417).
//!
//! | DuckDB | Ursa runs |
//! |---|---|
//! | `list_value(a, b, …)` | `make_array(a, b, …)` |
//! | `strftime(value, format)` | `to_char(value, format)`; `strftime(format, value)` too |
//! | `epoch_ms(n)` | the timestamp `n` milliseconds after the epoch (`to_timestamp_millis(n)`) |
//! | `epoch_ms(t)` | the milliseconds from the epoch to `t`, a `BIGINT` |
//! | `arg_max(x, y)` | `first_value(x ORDER BY y DESC) FILTER (WHERE x IS NOT NULL AND y IS NOT NULL)` |
//! | `arg_min(x, y)` | `first_value(x ORDER BY y ASC) FILTER (WHERE x IS NOT NULL AND y IS NOT NULL)` |
//!
//! Each is a thin layer over a DataFusion function: `list_value` is
//! `make_array`'s alias, `strftime` calls `to_char`, `epoch_ms` is a cast,
//! and `arg_max`/`arg_min` are rewritten into `first_value` when the query is
//! simplified. `arg_max` and `arg_min` are aggregates only, not window
//! functions. DuckDB's lambdas (`list_transform(l, x -> x + 1)`) and reuse
//! of a column alias in the same `SELECT` are not here: they are features
//! of the planner, not functions.

use std::sync::Arc;

use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, Field, FieldRef, TimeUnit};
use datafusion::common::{exec_err, not_impl_err, plan_err, Result};
use datafusion::logical_expr::expr::{AggregateFunction, AggregateFunctionParams};
use datafusion::logical_expr::function::{AccumulatorArgs, AggregateFunctionSimplification};
use datafusion::logical_expr::{
    Accumulator, AggregateUDF, AggregateUDFImpl, ColumnarValue, Expr, ScalarFunctionArgs,
    ScalarUDF, ScalarUDFImpl, Signature, SortExpr, Volatility,
};
use datafusion::prelude::SessionContext;

/// Register DuckDB's names on `ctx`.
pub(crate) fn register(ctx: &SessionContext) {
    let make_array = datafusion::functions_nested::make_array::make_array_udf();
    ctx.register_udf(make_array.as_ref().clone().with_aliases(["list_value"]));
    ctx.register_udf(ScalarUDF::from(Strftime::new()));
    ctx.register_udf(ScalarUDF::from(EpochMs::new()));
    ctx.register_udaf(AggregateUDF::from(ArgBest::new(true)));
    ctx.register_udaf(AggregateUDF::from(ArgBest::new(false)));
}

/// Whether `ty` is a type `to_char` formats.
fn is_temporal(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Date32
            | DataType::Date64
            | DataType::Time32(_)
            | DataType::Time64(_)
            | DataType::Timestamp(..)
            | DataType::Duration(_)
    )
}

fn is_text(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Utf8 | DataType::Utf8View | DataType::LargeUtf8 | DataType::Null
    )
}

/// `strftime(value, format)`, DuckDB's name for `to_char`. SQLite's order,
/// `strftime(format, value)`, works too: whichever argument is a date, a time,
/// a timestamp or a duration is the value.
#[derive(Debug, PartialEq, Eq, Hash)]
struct Strftime {
    signature: Signature,
    to_char: Arc<ScalarUDF>,
}

impl Strftime {
    fn new() -> Self {
        Self {
            signature: Signature::user_defined(Volatility::Immutable),
            to_char: datafusion::functions::datetime::to_char(),
        }
    }
}

impl ScalarUDFImpl for Strftime {
    fn name(&self) -> &str {
        "strftime"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _arg_types: &[DataType]) -> Result<DataType> {
        Ok(DataType::Utf8)
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        let [a, b] = arg_types else {
            return plan_err!(
                "strftime takes two arguments, a date or timestamp and a format: \
                 strftime(t, '%Y-%m-%d')"
            );
        };
        let value = |ty: &DataType| {
            if is_temporal(ty) {
                ty.clone()
            } else {
                DataType::Timestamp(TimeUnit::Nanosecond, None)
            }
        };
        match (is_temporal(a), is_temporal(b)) {
            (_, false) if is_text(b) && (is_temporal(a) || is_text(a)) => {
                Ok(vec![value(a), DataType::Utf8])
            }
            (false, true) if is_text(a) => Ok(vec![DataType::Utf8, b.clone()]),
            _ => plan_err!(
                "strftime takes a date or timestamp and a format string, as in \
                 strftime(t, '%Y-%m-%d'); got {a} and {b}"
            ),
        }
    }

    fn invoke_with_args(&self, mut args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        // After coercion the format is Utf8 and the value is not.
        if args.arg_fields.first().map(|f| f.data_type()) == Some(&DataType::Utf8)
            && args
                .arg_fields
                .get(1)
                .is_some_and(|f| is_temporal(f.data_type()))
        {
            args.args.swap(0, 1);
            args.arg_fields.swap(0, 1);
        }
        self.to_char.invoke_with_args(args)
    }
}

/// `epoch_ms(n)`: the timestamp `n` milliseconds after 1970-01-01 00:00:00;
/// `epoch_ms(t)`: the milliseconds from then to `t`.
#[derive(Debug, PartialEq, Eq, Hash)]
struct EpochMs {
    signature: Signature,
}

impl EpochMs {
    fn new() -> Self {
        Self {
            signature: Signature::user_defined(Volatility::Immutable),
        }
    }
}

const MILLIS: DataType = DataType::Timestamp(TimeUnit::Millisecond, None);

impl ScalarUDFImpl for EpochMs {
    fn name(&self) -> &str {
        "epoch_ms"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        match arg_types {
            [DataType::Int64] => Ok(MILLIS),
            _ => Ok(DataType::Int64),
        }
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        match arg_types {
            [ty] if ty.is_integer() || ty.is_null() => Ok(vec![DataType::Int64]),
            [DataType::Timestamp(_, tz)] => {
                Ok(vec![DataType::Timestamp(TimeUnit::Millisecond, tz.clone())])
            }
            [DataType::Date32 | DataType::Date64] => Ok(vec![MILLIS]),
            [ty] => plan_err!(
                "epoch_ms takes milliseconds (an integer) or a date or timestamp; got {ty}"
            ),
            _ => plan_err!("epoch_ms takes one argument"),
        }
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let [arg] = args.args.as_slice() else {
            return exec_err!("epoch_ms takes one argument");
        };
        let to = args.return_field.data_type();
        Ok(match arg {
            ColumnarValue::Array(a) => ColumnarValue::Array(cast(a, to)?),
            ColumnarValue::Scalar(s) => ColumnarValue::Scalar(s.cast_to(to)?),
        })
    }
}

/// `arg_max(x, y)` (`arg_min`): the `x` of the row with the largest
/// (smallest) `y`, skipping rows where `x` or `y` is NULL. Rewritten into
/// `first_value(x ORDER BY y …) FILTER (WHERE x IS NOT NULL AND y IS NOT
/// NULL)`.
#[derive(Debug, PartialEq, Eq, Hash)]
struct ArgBest {
    max: bool,
    signature: Signature,
}

impl ArgBest {
    fn new(max: bool) -> Self {
        Self {
            max,
            signature: Signature::any(2, Volatility::Immutable),
        }
    }
}

impl AggregateUDFImpl for ArgBest {
    fn name(&self) -> &str {
        if self.max {
            "arg_max"
        } else {
            "arg_min"
        }
    }

    fn aliases(&self) -> &[String] {
        &[]
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        Ok(arg_types[0].clone())
    }

    fn state_fields(
        &self,
        args: datafusion::logical_expr::function::StateFieldsArgs,
    ) -> Result<Vec<FieldRef>> {
        Ok(vec![Arc::new(Field::new(
            args.name,
            args.return_field.data_type().clone(),
            true,
        ))])
    }

    fn accumulator(&self, _args: AccumulatorArgs) -> Result<Box<dyn Accumulator>> {
        // Reached only where the query is not simplified: as a window function.
        let order = if self.max { "DESC" } else { "ASC" };
        not_impl_err!(
            "{}(x, y) is an aggregate, not a window function; over a window, use \
             first_value(x ORDER BY y {order})",
            self.name()
        )
    }

    fn simplify(&self) -> Option<AggregateFunctionSimplification> {
        let max = self.max;
        let name = self.name().to_string();
        Some(Box::new(move |function: AggregateFunction, _| {
            let AggregateFunctionParams {
                args,
                filter,
                order_by,
                null_treatment,
                ..
            } = function.params;
            if !order_by.is_empty() {
                return plan_err!("{name}(x, y) takes no ORDER BY: y is what it orders by");
            }
            let [x, y] = <[Expr; 2]>::try_from(args).map_err(|_| {
                datafusion::error::DataFusionError::Plan(format!(
                    "{name} takes two arguments, as in {name}(x, y)"
                ))
            })?;
            // As DuckDB's arg_max: rows where either is NULL are skipped.
            let present = x.clone().is_not_null().and(y.clone().is_not_null());
            let filter = match filter {
                Some(f) => (*f).and(present),
                None => present,
            };
            Ok(Expr::AggregateFunction(AggregateFunction::new_udf(
                datafusion::functions_aggregate::first_last::first_value_udaf(),
                vec![x],
                false,
                Some(Box::new(filter)),
                vec![SortExpr::new(y, !max, false)],
                null_treatment,
            )))
        }))
    }
}
