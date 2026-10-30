//! One float semantics for keys (#215): `group_by` keys, `distinct`, `n_unique`
//! and join keys compare floats the way filters and `sort` do (#199, #201).
//! `-0.0` equals `0.0`, every NaN equals every NaN (whatever its sign and
//! payload), and NaN joins NaN. A null stays a null: one group, and never a join
//! match.
//!
//! A key is made canonical once, before anything hashes or groups it: every NaN
//! becomes the type's positive quiet NaN and `-0.0` becomes `0.0`. After that,
//! the engine's own bit-wise hashing and equality (and the order-preserving
//! partition's `create_hashes`, #212) see one value per class. Only the key is
//! canonicalized, as a hidden copy: an aggregate over the same column still folds
//! its original values.
//!
//! This covers `float16`, `float32`, `float64` and dictionaries of them (over
//! their values). Everything works on the bits, so it needs no float type.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, PrimitiveArray, UInt64Array};
use arrow::buffer::{Buffer, ScalarBuffer};
use arrow::datatypes::{
    ArrowPrimitiveType, DataType, FieldRef, Float16Type, Float32Type, Float64Type,
};
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{
    ColumnarValue, Expr, ReturnFieldArgs, ScalarFunctionArgs, ScalarUDF, ScalarUDFImpl, Signature,
    Volatility,
};

/// Whether a column of type `dt` is a float key: a float, or a dictionary of
/// floats.
pub fn is_float_key(dt: &DataType) -> bool {
    match dt {
        DataType::Dictionary(_, v) => v.is_floating(),
        dt => dt.is_floating(),
    }
}

/// A float's bits, with what makes one canonical.
trait FloatBits: Copy + PartialEq + arrow::datatypes::ArrowNativeType {
    /// The canonical bits: every NaN the positive quiet NaN, `-0.0` as `0.0`.
    fn canon(self) -> Self;
}

macro_rules! float_bits {
    ($u:ty, $exp:expr, $mant:expr, $nan:expr, $neg_zero:expr) => {
        impl FloatBits for $u {
            #[inline(always)]
            fn canon(self) -> $u {
                if self & $exp == $exp && self & $mant != 0 {
                    $nan
                } else if self == $neg_zero {
                    0
                } else {
                    self
                }
            }
        }
    };
}
float_bits!(u16, 0x7c00, 0x03ff, 0x7e00, 0x8000);
float_bits!(u32, 0x7f80_0000, 0x007f_ffff, 0x7fc0_0000, 1 << 31);
float_bits!(
    u64,
    0x7ff0_0000_0000_0000,
    0x000f_ffff_ffff_ffff,
    0x7ff8_0000_0000_0000,
    1 << 63
);

/// The values of float array `a` (of type `F`) as their bits.
fn bits<F: ArrowPrimitiveType, U: FloatBits>(a: &dyn Array) -> &[U] {
    a.as_primitive::<F>().values().inner().typed_data::<U>()
}

/// The valid positions of float array `a` whose value is not canonical.
fn changed<F: ArrowPrimitiveType, U: FloatBits>(a: &dyn Array) -> Vec<usize> {
    let b = bits::<F, U>(a);
    // A read-only pass first: most columns hold nothing to change.
    if !b.iter().any(|&x| x.canon() != x) {
        return vec![];
    }
    b.iter()
        .enumerate()
        .filter(|&(i, &x)| x.canon() != x && a.is_valid(i))
        .map(|(i, _)| i)
        .collect()
}

/// Float array `a` with every value made canonical.
fn map_bits<F: ArrowPrimitiveType, U: FloatBits>(a: &dyn Array) -> ArrayRef {
    let out: Vec<U> = bits::<F, U>(a).iter().map(|&x| x.canon()).collect();
    let len = out.len();
    let values = ScalarBuffer::<F::Native>::new(Buffer::from_vec(out), 0, len);
    Arc::new(PrimitiveArray::<F>::new(values, a.nulls().cloned()))
}

/// The rows of float key `array` (see [`is_float_key`]) whose value is valid and
/// not canonical: a `-0.0`, or a NaN other than the positive quiet one. Empty
/// when canonicalizing changes nothing a key compares.
pub fn noncanonical_rows(array: &dyn Array) -> Vec<usize> {
    if let DataType::Dictionary(_, _) = array.data_type() {
        let d = array.as_any_dictionary();
        let values = d.values();
        let bad = noncanonical_rows(values.as_ref());
        if bad.is_empty() {
            return vec![];
        }
        let mut is_bad = vec![false; values.len()];
        for i in bad {
            is_bad[i] = true;
        }
        let keys = d.keys();
        return d
            .normalized_keys()
            .into_iter()
            .enumerate()
            .filter(|&(r, k)| keys.is_valid(r) && is_bad[k])
            .map(|(r, _)| r)
            .collect();
    }
    match array.data_type() {
        DataType::Float16 => changed::<Float16Type, u16>(array),
        DataType::Float32 => changed::<Float32Type, u32>(array),
        DataType::Float64 => changed::<Float64Type, u64>(array),
        _ => vec![],
    }
}

/// Float key `array` made canonical, of the same type (a dictionary over
/// canonical values). Any other array is returned as it is.
pub fn canonicalize(array: &ArrayRef) -> ArrayRef {
    match array.data_type() {
        DataType::Dictionary(_, _) => {
            let d = array.as_any_dictionary();
            d.with_values(canonicalize(d.values()))
        }
        DataType::Float16 => map_bits::<Float16Type, u16>(array.as_ref()),
        DataType::Float32 => map_bits::<Float32Type, u32>(array.as_ref()),
        DataType::Float64 => map_bits::<Float64Type, u64>(array.as_ref()),
        _ => array.clone(),
    }
}

/// `e`, a float key, made canonical in a plan (see [`canonicalize`]): the type
/// and nullability stay.
pub fn canonical_expr(e: Expr) -> Expr {
    ScalarUDF::new_from_impl(Canonical {
        signature: Signature::any(1, Volatility::Immutable),
    })
    .call(vec![e])
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct Canonical {
    signature: Signature,
}

impl ScalarUDFImpl for Canonical {
    fn name(&self) -> &str {
        "ursa_canonical_key"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, args: &[DataType]) -> Result<DataType> {
        Ok(args[0].clone())
    }

    fn return_field_from_args(&self, args: ReturnFieldArgs) -> Result<FieldRef> {
        Ok(args.arg_fields[0].clone())
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let [arg] = args.args.as_slice() else {
            return Err(DataFusionError::Internal(
                "ursa_canonical_key takes one argument".into(),
            ));
        };
        Ok(match arg {
            ColumnarValue::Array(a) => ColumnarValue::Array(canonicalize(a)),
            ColumnarValue::Scalar(s) => {
                let a = canonicalize(&s.to_array()?);
                ColumnarValue::Scalar(datafusion::common::ScalarValue::try_from_array(&a, 0)?)
            }
        })
    }
}

/// The original values of a float key at the rows canonicalizing changed, by
/// the row's ordinal: what a group or distinct row whose first row is one of them
/// shows instead of the canonical value.
#[derive(Debug)]
pub struct Originals {
    /// The original values, of the key's type.
    pub values: ArrayRef,
    /// Each changed row's ordinal and its value's position in `values`,
    /// sorted by ordinal.
    pub at: Vec<(u64, usize)>,
}

impl Originals {
    /// The position in `values` of the row with ordinal `ord`, if it changed.
    fn find(&self, ord: u64) -> Option<usize> {
        self.at
            .binary_search_by_key(&ord, |&(o, _)| o)
            .ok()
            .map(|i| self.at[i].1)
    }
}

/// `key` (a canonical key column after an aggregate) with each row whose first
/// input row (ordinal `first`) had a non-canonical value showing that value: the
/// group's first-appearance value. The type and nullability stay.
pub fn representative(key: Expr, first: Expr, originals: Arc<Originals>) -> Expr {
    ScalarUDF::new_from_impl(Representative {
        signature: Signature::any(2, Volatility::Immutable),
        originals,
    })
    .call(vec![key, first])
}

#[derive(Debug)]
struct Representative {
    signature: Signature,
    originals: Arc<Originals>,
}

impl PartialEq for Representative {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.originals, &other.originals)
    }
}

impl Eq for Representative {}

impl Hash for Representative {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.originals).hash(state);
    }
}

impl ScalarUDFImpl for Representative {
    fn name(&self) -> &str {
        "ursa_first_key"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, args: &[DataType]) -> Result<DataType> {
        Ok(args[0].clone())
    }

    fn return_field_from_args(&self, args: ReturnFieldArgs) -> Result<FieldRef> {
        Ok(args.arg_fields[0].clone())
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> Result<ColumnarValue> {
        let [key, first] = args.args.as_slice() else {
            return Err(DataFusionError::Internal(
                "ursa_first_key takes two arguments".into(),
            ));
        };
        let n = args.number_rows;
        let key = key.to_array(n)?;
        let first = first.to_array(n)?;
        let first = first
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| DataFusionError::Internal("an ordinal is UInt64".into()))?;
        let mut any = false;
        let picks: Vec<(usize, usize)> = (0..n)
            .map(|r| {
                match first
                    .is_valid(r)
                    .then(|| self.originals.find(first.value(r)))
                    .flatten()
                {
                    Some(j) => {
                        any = true;
                        (1, j)
                    }
                    None => (0, r),
                }
            })
            .collect();
        if !any {
            return Ok(ColumnarValue::Array(key));
        }
        let out =
            arrow::compute::interleave(&[key.as_ref(), self.originals.values.as_ref()], &picks)?;
        Ok(ColumnarValue::Array(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{DictionaryArray, Float16Array, Float32Array, Float64Array, Int8Array};

    fn neg_nan64(payload: u64) -> f64 {
        f64::from_bits(0xfff0_0000_0000_0000 | payload)
    }

    #[test]
    fn canonical_bits() {
        assert_eq!(FloatBits::canon((-0.0f64).to_bits()), 0);
        assert_eq!(FloatBits::canon(neg_nan64(1).to_bits()), f64::NAN.to_bits());
        assert_eq!(FloatBits::canon(f64::NAN.to_bits()), f64::NAN.to_bits());
        assert_eq!(
            FloatBits::canon(f64::NEG_INFINITY.to_bits()),
            f64::NEG_INFINITY.to_bits()
        );
        assert_eq!(
            FloatBits::canon(f64::INFINITY.to_bits()),
            f64::INFINITY.to_bits()
        );
        assert_eq!(FloatBits::canon((-0.0f32).to_bits()), 0);
        assert_eq!(
            FloatBits::canon(f32::from_bits(0xffc0_0001).to_bits()),
            f32::NAN.to_bits()
        );
        assert_eq!(
            FloatBits::canon(f32::INFINITY.to_bits()),
            f32::INFINITY.to_bits()
        );
        assert_eq!(FloatBits::canon(0x8000u16), 0);
        assert_eq!(FloatBits::canon(0xfe01u16), 0x7e00);
        assert_eq!(FloatBits::canon(0x7c00u16), 0x7c00); // +inf
        assert_eq!(FloatBits::canon(0xfc00u16), 0xfc00); // -inf
        assert_eq!((f32::NAN as f64).to_bits(), f64::NAN.to_bits());
    }

    #[test]
    fn noncanonical_rows_skip_nulls_and_canonical_values() {
        let a: ArrayRef = Arc::new(Float64Array::from(vec![
            Some(1.0),
            Some(-0.0),
            None,
            Some(f64::NAN),
            Some(neg_nan64(5)),
            Some(0.0),
            Some(f64::NEG_INFINITY),
        ]));
        assert_eq!(noncanonical_rows(a.as_ref()), vec![1, 4]);
        let c = canonicalize(&a);
        assert!(noncanonical_rows(c.as_ref()).is_empty());
        assert!(c.is_null(2));
        let c = c.as_primitive::<Float64Type>();
        assert_eq!(c.value(1).to_bits(), 0);
        assert_eq!(c.value(4).to_bits(), f64::NAN.to_bits());
        assert_eq!(c.value(6), f64::NEG_INFINITY);

        let f: ArrayRef = Arc::new(Float32Array::from(vec![-0.0f32, 2.5]));
        assert_eq!(noncanonical_rows(f.as_ref()), vec![0]);
        let h: ArrayRef = Arc::new(Float16Array::from_iter_values(
            [0x8000u16, 0x3c00, 0xfe00].map(half_from_bits),
        ));
        assert_eq!(noncanonical_rows(h.as_ref()), vec![0, 2]);
    }

    fn half_from_bits(b: u16) -> <Float16Type as ArrowPrimitiveType>::Native {
        let a = PrimitiveArray::<Float16Type>::new(
            ScalarBuffer::new(Buffer::from_vec(vec![b]), 0, 1),
            None,
        );
        a.value(0)
    }

    #[test]
    fn dictionaries_canonicalize_their_values() {
        let values = Float64Array::from(vec![-0.0, 1.0, neg_nan64(3), 7.0]);
        let keys = Int8Array::from(vec![Some(1), Some(0), None, Some(3), Some(2), Some(1)]);
        let d: ArrayRef = Arc::new(DictionaryArray::new(keys, Arc::new(values)));
        assert_eq!(noncanonical_rows(d.as_ref()), vec![1, 4]);
        let c = canonicalize(&d);
        assert_eq!(c.data_type(), d.data_type());
        assert!(noncanonical_rows(c.as_ref()).is_empty());
        // Unused non-canonical values change no row.
        let values = Float64Array::from(vec![-0.0, 1.0]);
        let keys = Int8Array::from(vec![1, 1]);
        let d: ArrayRef = Arc::new(DictionaryArray::new(keys, Arc::new(values)));
        assert!(noncanonical_rows(d.as_ref()).is_empty());
    }
}
