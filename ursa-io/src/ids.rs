//! The rules that turn source columns into node ids and edges, shared by every
//! way Ursa reads edges: a scan ([`crate::scan`]), an in-memory table (the
//! Python library's `EdgeFrame` and `NodeFrame`), and the SQL engine's index
//! build.
//!
//! - which column types hold node ids, and the one type each canonicalizes to
//!   ([`canonical_id_type`], [`endpoint_id_type`]);
//! - an in-memory id column canonicalized chunk by chunk ([`id_chunks`]);
//! - `string_view` columns read as plain strings ([`unview_field`],
//!   [`unview_target`], [`unview_chunks`]);
//! - rows with a null endpoint, dropped and counted for `on_null="drop"`
//!   ([`endpoint_mask`], [`drop_null_endpoint_rows`]).
//!
//! Errors carry what went wrong, not a message: the host words them, with its
//! own spelling of a type or a column name.

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, BooleanArray, RecordBatch};
use arrow::compute::kernels::boolean::{and, is_not_null};
use arrow::compute::{cast, filter_record_batch};
use arrow::datatypes::{DataType, Field, UInt64Type};
use arrow::error::ArrowError;

/// The canonical Arrow type for a node-id column of type `dt`, or `None` if it
/// cannot hold node ids.
///
/// Any integer type collapses to `Int64` (the fast path), and `Utf8`,
/// `LargeUtf8` and `Utf8View` to `Utf8` (string ids, covering UUID-as-string).
/// A dictionary column (a pyarrow or pandas categorical, or a Parquet file
/// written from one) canonicalizes by its value type.
pub fn canonical_id_type(dt: &DataType) -> Option<DataType> {
    use DataType::*;
    match dt {
        Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => Some(Int64),
        Utf8 | LargeUtf8 | Utf8View => Some(Utf8),
        Dictionary(_, value) => canonical_id_type(value),
        _ => None,
    }
}

/// The message for a node-id column of a type that cannot hold node ids;
/// `shown` is the type as the host spells it.
pub fn id_type_message(shown: &str) -> String {
    format!("node ids must be an integer or string column; got {shown}")
}

/// Why two endpoint columns give no one node-id type.
#[derive(Debug, Clone, PartialEq)]
pub enum EndpointTypeError {
    /// A column of this type cannot hold node ids.
    NotAnId(DataType),
    /// One column is integer and the other string (their canonical types).
    Mixed(DataType, DataType),
}

/// The canonical node-id type shared by a `src` column of type `src` and a
/// `dst` column of type `dst`: both must hold ids, of the same family.
pub fn endpoint_id_type(src: &DataType, dst: &DataType) -> Result<DataType, EndpointTypeError> {
    let src_type = canonical_id_type(src).ok_or_else(|| EndpointTypeError::NotAnId(src.clone()))?;
    let dst_type = canonical_id_type(dst).ok_or_else(|| EndpointTypeError::NotAnId(dst.clone()))?;
    if src_type != dst_type {
        return Err(EndpointTypeError::Mixed(src_type, dst_type));
    }
    Ok(src_type)
}

/// Why an in-memory id column could not be canonicalized.
#[derive(Debug)]
pub enum IdColumnError {
    /// The column's type cannot hold node ids.
    NotAnId,
    /// A `uint64` id above `i64::MAX`, which an `int64` id cannot hold.
    OutOfRange(u64),
    /// Arrow failed to cast the column.
    Arrow(ArrowError),
}

/// The chunks of an in-memory node-id column, canonicalized one by one: an
/// integer column becomes `Int64` (an `Int64` chunk is shared, not copied), a
/// string column of any layout is kept as it is, and a dictionary column is
/// decoded and canonicalized by its value type.
///
/// Strings are not cast: the id map reads every string layout natively, and a
/// `LargeUtf8` to `Utf8` cast would fail once the string data passes the 2 GiB
/// that `Utf8`'s offsets address, the input a `large_string` column exists to
/// carry. So an in-memory table keeps its string type, where a scan (whose
/// batches are small) reads strings as `Utf8`. A column with no chunks gives
/// one empty chunk of its type.
pub fn id_chunks(chunks: &[ArrayRef], dt: &DataType) -> Result<Vec<ArrayRef>, IdColumnError> {
    if chunks.is_empty() {
        return id_chunk(&arrow::array::new_empty_array(dt)).map(|c| vec![c]);
    }
    chunks.iter().map(id_chunk).collect()
}

fn id_chunk(chunk: &ArrayRef) -> Result<ArrayRef, IdColumnError> {
    use DataType::*;
    match chunk.data_type() {
        Int64 => Ok(Arc::clone(chunk)),
        Int8 | Int16 | Int32 | UInt8 | UInt16 | UInt32 => {
            cast(chunk, &Int64).map_err(IdColumnError::Arrow)
        }
        UInt64 => {
            // A safe cast turns an out-of-range value into a null; name it instead.
            let values = chunk.as_primitive::<UInt64Type>();
            let over = (0..values.len())
                .find(|&i| values.is_valid(i) && values.value(i) > i64::MAX as u64);
            match over {
                Some(i) => Err(IdColumnError::OutOfRange(values.value(i))),
                None => cast(chunk, &Int64).map_err(IdColumnError::Arrow),
            }
        }
        Utf8 | LargeUtf8 | Utf8View => Ok(Arc::clone(chunk)),
        Dictionary(_, value) if canonical_id_type(value).is_some() => {
            let decoded = cast(chunk, value).map_err(IdColumnError::Arrow)?;
            id_chunk(&decoded)
        }
        _ => Err(IdColumnError::NotAnId),
    }
}

/// `f` with a `Utf8View` type replaced by `Utf8`: how a scan, whose batches are
/// small, reads a `string_view` column.
pub fn unview_field(f: &Field) -> Field {
    match f.data_type() {
        DataType::Utf8View => Field::new(f.name(), DataType::Utf8, f.is_nullable()),
        _ => f.clone(),
    }
}

/// The most character data a `Utf8` (i32-offset) array can address.
const UTF8_MAX_BYTES: usize = i32::MAX as usize;

/// The type an in-memory `string_view` column with chunks `chunks` is read as:
/// `Utf8`, or `LargeUtf8` when a chunk may hold more than 2 GiB of character
/// data, which a `Utf8` cast cannot address (#152). A chunk's size (its views,
/// data buffers and validity) bounds its character data from above.
///
/// String views are read as strings once, where a table comes in, because the
/// rest of the pipeline (pyarrow compute, in the Python library) has few
/// string-view kernels; a scan reads them the same way ([`unview_field`]).
pub fn unview_target(chunks: &[ArrayRef]) -> DataType {
    let fits = chunks
        .iter()
        .all(|c| view_bytes(c.as_ref()) <= UTF8_MAX_BYTES);
    if fits {
        DataType::Utf8
    } else {
        DataType::LargeUtf8
    }
}

/// The bytes a `string_view` array's buffers hold.
fn view_bytes(array: &dyn Array) -> usize {
    let views = array.as_string_view();
    let data: usize = views.data_buffers().iter().map(|b| b.len()).sum();
    let nulls = views.nulls().map_or(0, |n| n.buffer().len());
    views.views().inner().len() + data + nulls
}

/// The chunks of a `string_view` column cast to `target` (see
/// [`unview_target`]).
pub fn unview_chunks(chunks: &[ArrayRef], target: &DataType) -> Result<Vec<ArrayRef>, ArrowError> {
    chunks.iter().map(|c| cast(c, target)).collect()
}

/// The rows to keep of endpoint columns `src` and `dst`, as a filter mask, or
/// `None` when neither has a null (every row is kept).
pub fn endpoint_mask(src: &dyn Array, dst: &dyn Array) -> Result<Option<BooleanArray>, ArrowError> {
    if src.null_count() == 0 && dst.null_count() == 0 {
        return Ok(None);
    }
    Ok(Some(and(&is_not_null(src)?, &is_not_null(dst)?)?))
}

/// `batches` without their rows whose endpoint (column `src` or `dst`) is
/// null, and how many rows that removed: the `on_null="drop"` opt-in. A batch
/// with no null endpoint is kept as it is.
pub fn drop_null_endpoint_rows(
    batches: Vec<RecordBatch>,
    src: usize,
    dst: usize,
) -> Result<(Vec<RecordBatch>, usize), ArrowError> {
    let mut dropped = 0;
    let mut out = Vec::with_capacity(batches.len());
    for batch in batches {
        match endpoint_mask(batch.column(src).as_ref(), batch.column(dst).as_ref())? {
            None => out.push(batch),
            Some(keep) => {
                let kept = filter_record_batch(&batch, &keep)?;
                dropped += batch.num_rows() - kept.num_rows();
                out.push(kept);
            }
        }
    }
    Ok((out, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{
        DictionaryArray, Float64Array, Int32Array, Int64Array, LargeStringArray, StringArray,
        StringViewArray, UInt64Array,
    };
    use arrow::datatypes::{Int32Type, Schema};

    #[test]
    fn id_types_canonicalize_by_family() {
        let dict = DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::LargeUtf8));
        assert_eq!(canonical_id_type(&DataType::UInt8), Some(DataType::Int64));
        assert_eq!(canonical_id_type(&DataType::Utf8View), Some(DataType::Utf8));
        assert_eq!(canonical_id_type(&dict), Some(DataType::Utf8));
        assert_eq!(canonical_id_type(&DataType::Float64), None);
        assert_eq!(
            endpoint_id_type(&DataType::Int32, &DataType::Utf8),
            Err(EndpointTypeError::Mixed(DataType::Int64, DataType::Utf8))
        );
        assert_eq!(
            endpoint_id_type(&DataType::Float64, &DataType::Utf8),
            Err(EndpointTypeError::NotAnId(DataType::Float64))
        );
    }

    #[test]
    fn in_memory_ids_keep_their_string_layout_and_share_int64_chunks() {
        let int64: ArrayRef = Arc::new(Int64Array::from(vec![1, 2]));
        let out = id_chunks(&[Arc::clone(&int64)], &DataType::Int64).unwrap();
        assert!(Arc::ptr_eq(&out[0], &int64));
        let int32: ArrayRef = Arc::new(Int32Array::from(vec![1, 2]));
        let out = id_chunks(&[int32], &DataType::Int32).unwrap();
        assert_eq!(out[0].data_type(), &DataType::Int64);
        for strings in [
            Arc::new(StringArray::from(vec!["a"])) as ArrayRef,
            Arc::new(LargeStringArray::from(vec!["a"])),
            Arc::new(StringViewArray::from(vec!["a"])),
        ] {
            let dt = strings.data_type().clone();
            assert_eq!(id_chunks(&[strings], &dt).unwrap()[0].data_type(), &dt);
        }
        let dict: DictionaryArray<Int32Type> = vec!["x", "y", "x"].into_iter().collect();
        let dict: ArrayRef = Arc::new(dict);
        let out = id_chunks(&[Arc::clone(&dict)], dict.data_type()).unwrap();
        assert_eq!(out[0].data_type(), &DataType::Utf8);
        // No chunks: one empty chunk of the canonical type.
        let out = id_chunks(&[], &DataType::Int16).unwrap();
        assert_eq!((out.len(), out[0].len()), (1, 0));
        assert_eq!(out[0].data_type(), &DataType::Int64);
    }

    #[test]
    fn in_memory_ids_refuse_what_cannot_be_an_id() {
        let floats: ArrayRef = Arc::new(Float64Array::from(vec![1.0]));
        assert!(matches!(
            id_chunks(&[floats], &DataType::Float64),
            Err(IdColumnError::NotAnId)
        ));
        let big: ArrayRef = Arc::new(UInt64Array::from(vec![Some(1), None, Some(u64::MAX)]));
        assert!(matches!(
            id_chunks(&[big], &DataType::UInt64),
            Err(IdColumnError::OutOfRange(u64::MAX))
        ));
    }

    #[test]
    fn string_views_read_as_strings() {
        let views: ArrayRef = Arc::new(StringViewArray::from(vec![Some("a"), None]));
        let target = unview_target(&[Arc::clone(&views)]);
        assert_eq!(target, DataType::Utf8);
        let out = unview_chunks(&[views], &target).unwrap();
        assert_eq!(out[0].data_type(), &DataType::Utf8);
        assert_eq!(out[0].null_count(), 1);
        let field = Field::new("s", DataType::Utf8View, false);
        assert_eq!(unview_field(&field), Field::new("s", DataType::Utf8, false));
    }

    #[test]
    fn null_endpoint_rows_are_dropped_and_counted() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("w", DataType::Int64, true),
            Field::new("s", DataType::Int64, true),
            Field::new("d", DataType::Int64, true),
        ]));
        let batch = |w: Vec<i64>, s: Vec<Option<i64>>, d: Vec<Option<i64>>| {
            RecordBatch::try_new(
                Arc::clone(&schema),
                vec![
                    Arc::new(Int64Array::from(w)),
                    Arc::new(Int64Array::from(s)),
                    Arc::new(Int64Array::from(d)),
                ],
            )
            .unwrap()
        };
        let clean = batch(vec![1], vec![Some(1)], vec![Some(2)]);
        let dirty = batch(
            vec![1, 2, 3],
            vec![Some(1), None, Some(3)],
            vec![None, Some(2), Some(3)],
        );
        let (out, dropped) = drop_null_endpoint_rows(vec![clean.clone(), dirty], 1, 2).unwrap();
        assert_eq!(dropped, 2);
        assert_eq!(out[0], clean);
        assert_eq!(out[1], batch(vec![3], vec![Some(3)], vec![Some(3)]));
        assert!(endpoint_mask(clean.column(1), clean.column(2))
            .unwrap()
            .is_none());
    }
}
