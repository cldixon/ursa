//! Float aggregations with the semantics `group_by().agg()` has always had
//! (pyarrow's hash aggregates), bit for bit (#191).
//!
//! DataFusion's own float `sum` / `mean` / `min` / `max` differ in their
//! treatment of NaN, so these run as user-defined aggregates inside the stock
//! aggregate node. Each folds a group's values in input order (the lowering
//! hands the aggregate each group's rows in order, in one partition, see
//! [`crate::partition`]):
//!
//! - `sum` starts at `0.0` and adds each value (so `-0.0` alone sums to `0.0`).
//!   When the sum is already NaN and the value is NaN too, the result keeps the
//!   later NaN, or the earlier one when the column has nulls (pyarrow takes two
//!   different kernels in those cases, and NaN sign bits follow them).
//! - `mean` is that sum over the count of values, in `float64`.
//! - `min` / `max` skip NaN unless every value is NaN, keep the earlier of two
//!   equal values (so `-0.0` vs `0.0` keeps the first), and keep the later NaN
//!   while they have seen nothing else.
//!
//! A `float32` column sums in `float64`, and its `min` / `max` stay `float32`.
//! A group with no values (all null) aggregates to null.

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, BooleanArray, Float64Array, UInt64Array};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, FieldRef, Float64Type, UInt64Type};
use datafusion::common::ScalarValue;
use datafusion::error::Result;
use datafusion::logical_expr::function::{AccumulatorArgs, StateFieldsArgs};
use datafusion::logical_expr::{
    Accumulator, AggregateUDF, AggregateUDFImpl, EmitTo, Expr, GroupsAccumulator, Signature,
    Volatility,
};

/// Which float aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FloatAgg {
    Sum,
    Mean,
    Min,
    Max,
}

/// `agg(operand)` over a float column of type `input`. `nulls` says whether the
/// column holds a null anywhere (it picks the NaN rule of `sum` and `mean`).
pub fn float_agg(agg: FloatAgg, input: &DataType, nulls: bool, operand: Expr) -> Expr {
    let udf = AggregateUDF::new_from_impl(PyArrowFloat {
        agg,
        input: input.clone(),
        nulls,
        signature: Signature::any(1, Volatility::Immutable),
    });
    udf.call(vec![operand])
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct PyArrowFloat {
    agg: FloatAgg,
    input: DataType,
    nulls: bool,
    signature: Signature,
}

impl PyArrowFloat {
    fn output(&self) -> DataType {
        match self.agg {
            FloatAgg::Sum | FloatAgg::Mean => DataType::Float64,
            FloatAgg::Min | FloatAgg::Max => self.input.clone(),
        }
    }
}

impl AggregateUDFImpl for PyArrowFloat {
    fn name(&self) -> &str {
        match self.agg {
            FloatAgg::Sum => "ursa_sum",
            FloatAgg::Mean => "ursa_mean",
            FloatAgg::Min => "ursa_min",
            FloatAgg::Max => "ursa_max",
        }
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, _args: &[DataType]) -> Result<DataType> {
        Ok(self.output())
    }

    fn accumulator(&self, _args: AccumulatorArgs) -> Result<Box<dyn Accumulator>> {
        Ok(Box::new(Fold {
            agg: self.agg,
            nulls: self.nulls,
            output: self.output(),
            acc: None,
            n: 0,
        }))
    }

    fn groups_accumulator_supported(&self, _args: AccumulatorArgs) -> bool {
        true
    }

    fn create_groups_accumulator(
        &self,
        _args: AccumulatorArgs,
    ) -> Result<Box<dyn GroupsAccumulator>> {
        Ok(Box::new(Folds {
            agg: self.agg,
            nulls: self.nulls,
            output: self.output(),
            acc: vec![],
            n: vec![],
        }))
    }

    fn state_fields(&self, args: StateFieldsArgs) -> Result<Vec<FieldRef>> {
        Ok(vec![
            Arc::new(Field::new(
                format!("{}_acc", args.name),
                DataType::Float64,
                true,
            )),
            Arc::new(Field::new(
                format!("{}_n", args.name),
                DataType::UInt64,
                false,
            )),
        ])
    }
}

/// One group's fold.
#[derive(Debug)]
struct Fold {
    agg: FloatAgg,
    nulls: bool,
    output: DataType,
    /// The running value (`None` before the first value).
    acc: Option<f64>,
    /// Values folded so far.
    n: u64,
}

impl Fold {
    fn step(&mut self, v: f64) {
        self.n += 1;
        self.acc = Some(step(self.agg, self.nulls, self.acc, v));
    }
}

/// Fold `v` into a group's running value (`None` before its first value).
pub(crate) fn step(agg: FloatAgg, nulls: bool, acc: Option<f64>, v: f64) -> f64 {
    let Some(acc) = acc else {
        return match agg {
            FloatAgg::Sum | FloatAgg::Mean => 0.0 + v,
            FloatAgg::Min | FloatAgg::Max => v,
        };
    };
    match agg {
        FloatAgg::Sum | FloatAgg::Mean => {
            if acc.is_nan() && v.is_nan() {
                if nulls {
                    acc
                } else {
                    v
                }
            } else {
                acc + v
            }
        }
        FloatAgg::Min => pick(acc, v, v < acc),
        FloatAgg::Max => pick(acc, v, v > acc),
    }
}

/// `fmin` / `fmax` with ties kept: a NaN gives way to the other value, and
/// `better` (the new value beats the old) decides between two numbers.
fn pick(acc: f64, v: f64, better: bool) -> f64 {
    if acc.is_nan() {
        v
    } else if v.is_nan() || !better {
        acc
    } else {
        v
    }
}

impl Accumulator for Fold {
    fn update_batch(&mut self, values: &[ArrayRef]) -> Result<()> {
        let values = cast(&values[0], &DataType::Float64)?;
        let values = values.as_primitive::<Float64Type>();
        match values.nulls() {
            None => values.values().iter().for_each(|&v| self.step(v)),
            Some(_) => values.iter().flatten().for_each(|v| self.step(v)),
        }
        Ok(())
    }

    fn merge_batch(&mut self, states: &[ArrayRef]) -> Result<()> {
        // A group's rows are in one partition, in order, so its partial states
        // arrive in order; each one continues the fold where the last stopped.
        let accs = states[0].as_primitive::<Float64Type>();
        let ns = states[1].as_primitive::<UInt64Type>();
        for i in 0..accs.len() {
            let n = ns.value(i);
            if n == 0 {
                continue;
            }
            let acc = accs.value(i);
            match self.acc {
                None => {
                    self.acc = Some(acc);
                    self.n = n;
                }
                Some(_) => {
                    self.step(acc);
                    self.n += n - 1;
                }
            }
        }
        Ok(())
    }

    fn state(&mut self) -> Result<Vec<ScalarValue>> {
        Ok(vec![
            ScalarValue::Float64(self.acc),
            ScalarValue::UInt64(Some(self.n)),
        ])
    }

    fn evaluate(&mut self) -> Result<ScalarValue> {
        let v = match (self.agg, self.acc) {
            (_, None) => None,
            (FloatAgg::Mean, Some(sum)) => Some(sum / self.n as f64),
            (_, Some(acc)) => Some(acc),
        };
        ScalarValue::Float64(v).cast_to(&self.output)
    }

    fn size(&self) -> usize {
        std::mem::size_of_val(self)
    }
}

/// Every group's fold at once (the aggregate's vectorized form): each batch's
/// values are folded into their groups in row order.
#[derive(Debug)]
struct Folds {
    agg: FloatAgg,
    nulls: bool,
    output: DataType,
    /// Each group's running value, meaningful once its count is non-zero.
    acc: Vec<f64>,
    /// Values folded per group.
    n: Vec<u64>,
}

impl Folds {
    fn grow(&mut self, groups: usize) {
        self.acc.resize(groups, 0.0);
        self.n.resize(groups, 0);
    }

    fn fold(&mut self, g: usize, v: f64, count: u64) {
        let acc = (self.n[g] > 0).then_some(self.acc[g]);
        self.acc[g] = step(self.agg, self.nulls, acc, v);
        self.n[g] += count;
    }

    /// The emitted groups' values (null for a group with none), and counts.
    fn take(&mut self, emit_to: EmitTo) -> (Float64Array, Vec<u64>) {
        let acc = emit_to.take_needed(&mut self.acc);
        let n = emit_to.take_needed(&mut self.n);
        let values = acc
            .iter()
            .zip(&n)
            .map(|(&a, &k)| (k > 0).then_some(a))
            .collect();
        (values, n)
    }
}

impl GroupsAccumulator for Folds {
    fn update_batch(
        &mut self,
        values: &[ArrayRef],
        group_indices: &[usize],
        opt_filter: Option<&BooleanArray>,
        total_num_groups: usize,
    ) -> Result<()> {
        self.grow(total_num_groups);
        let values = cast(&values[0], &DataType::Float64)?;
        let values = values.as_primitive::<Float64Type>();
        let raw = values.values();
        for (i, &g) in group_indices.iter().enumerate() {
            if values.is_null(i) || opt_filter.is_some_and(|f| !f.value(i) || f.is_null(i)) {
                continue;
            }
            self.fold(g, raw[i], 1);
        }
        Ok(())
    }

    fn merge_batch(
        &mut self,
        values: &[ArrayRef],
        group_indices: &[usize],
        opt_filter: Option<&BooleanArray>,
        total_num_groups: usize,
    ) -> Result<()> {
        // A group's rows are in one partition, in order, so its partial states
        // arrive in order; each one continues the fold where the last stopped.
        self.grow(total_num_groups);
        let accs = values[0].as_primitive::<Float64Type>();
        let ns = values[1].as_primitive::<UInt64Type>();
        for (i, &g) in group_indices.iter().enumerate() {
            let k = ns.value(i);
            if k == 0 || opt_filter.is_some_and(|f| !f.value(i) || f.is_null(i)) {
                continue;
            }
            if self.n[g] == 0 {
                self.acc[g] = accs.value(i);
                self.n[g] = k;
            } else {
                self.fold(g, accs.value(i), k);
            }
        }
        Ok(())
    }

    fn evaluate(&mut self, emit_to: EmitTo) -> Result<ArrayRef> {
        let (values, n) = self.take(emit_to);
        let values: Float64Array = match self.agg {
            FloatAgg::Mean => values
                .iter()
                .zip(&n)
                .map(|(v, &k)| v.map(|s| s / k as f64))
                .collect(),
            _ => values,
        };
        Ok(cast(&values, &self.output)?)
    }

    fn state(&mut self, emit_to: EmitTo) -> Result<Vec<ArrayRef>> {
        let (values, n) = self.take(emit_to);
        Ok(vec![Arc::new(values), Arc::new(UInt64Array::from(n))])
    }

    fn size(&self) -> usize {
        self.acc.capacity() * 8 + self.n.capacity() * 8
    }
}

/// Whether a column holds a null anywhere in `batches` (column `i`).
pub fn has_nulls(batches: &[arrow::array::RecordBatch], i: usize) -> bool {
    batches.iter().any(|b| b.column(i).null_count() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(agg: FloatAgg, nulls: bool, vals: &[f64]) -> u64 {
        let mut f = Fold {
            agg,
            nulls,
            output: DataType::Float64,
            acc: None,
            n: 0,
        };
        let a: ArrayRef = Arc::new(Float64Array::from(vals.to_vec()));
        f.update_batch(&[a]).unwrap();
        match f.evaluate().unwrap() {
            ScalarValue::Float64(Some(v)) => v.to_bits(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn folds_like_pyarrow() {
        let neg_nan = f64::from_bits(0xfff8_0000_0000_0000);
        let nan = f64::NAN;
        // Sum: the later NaN, or the earlier when the column has nulls.
        assert_eq!(
            fold(FloatAgg::Sum, false, &[nan, neg_nan]),
            neg_nan.to_bits()
        );
        assert_eq!(fold(FloatAgg::Sum, true, &[nan, neg_nan]), nan.to_bits());
        assert_eq!(fold(FloatAgg::Sum, false, &[-0.0]), 0.0f64.to_bits());
        // Min/max: NaN skipped, ties keep the first, all-NaN keeps the last.
        assert_eq!(
            fold(FloatAgg::Min, false, &[nan, 2.0, 1.0]),
            1.0f64.to_bits()
        );
        assert_eq!(
            fold(FloatAgg::Max, false, &[-0.0, 0.0]),
            (-0.0f64).to_bits()
        );
        assert_eq!(fold(FloatAgg::Min, false, &[0.0, -0.0]), 0.0f64.to_bits());
        assert_eq!(
            fold(FloatAgg::Max, false, &[nan, neg_nan]),
            neg_nan.to_bits()
        );
        assert_eq!(fold(FloatAgg::Mean, false, &[1.0, 2.0]), 1.5f64.to_bits());
    }

    #[test]
    fn empty_state_merges_as_nothing() {
        let mut f = Fold {
            agg: FloatAgg::Sum,
            nulls: false,
            output: DataType::Float64,
            acc: None,
            n: 0,
        };
        let accs: ArrayRef = Arc::new(Float64Array::from(vec![None, Some(-0.0)]));
        let ns: ArrayRef = Arc::new(UInt64Array::from(vec![0, 1]));
        f.merge_batch(&[accs, ns]).unwrap();
        assert_eq!(f.evaluate().unwrap(), ScalarValue::Float64(Some(-0.0)));
    }

    #[test]
    fn groups_fold_each_group_in_row_order() {
        let neg_nan = f64::from_bits(0xfff8_0000_0000_0000);
        let mut g = Folds {
            agg: FloatAgg::Sum,
            nulls: false,
            output: DataType::Float64,
            acc: vec![],
            n: vec![],
        };
        let v: ArrayRef = Arc::new(Float64Array::from(vec![
            Some(f64::NAN),
            Some(-0.0),
            None,
            Some(neg_nan),
        ]));
        g.update_batch(&[v], &[0, 1, 2, 0], None, 3).unwrap();
        let out = g.evaluate(EmitTo::All).unwrap();
        let out = out.as_primitive::<Float64Type>();
        assert_eq!(out.value(0).to_bits(), neg_nan.to_bits());
        assert_eq!(out.value(1).to_bits(), 0.0f64.to_bits());
        assert!(out.is_null(2));
    }
}
