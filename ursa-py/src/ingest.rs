//! An in-memory table becoming a frame's source: `ur.EdgeFrame(...)` and
//! `ur.NodeFrame(...)` (and `from_arrow` / `from_polars` / `from_pandas`, which
//! call them).
//!
//! The rules are `ursa_io::ids`', the ones a scan applies (#269): `string_view`
//! columns read as strings, rows with a null endpoint dropped and counted for
//! `on_null="drop"`, and the id columns canonicalized. This module moves the
//! table's columns across the FFI (zero-copy) and words the errors as the
//! Python API always has, with Python's own `repr` of a name and pyarrow's
//! spelling of a type. The table itself stays a pyarrow table: a column a rule
//! leaves alone is the caller's own column, not a copy.

use arrow::array::{make_array, ArrayData, ArrayRef, RecordBatch};
use arrow::datatypes::{DataType, Schema};
use arrow_pyarrow::{FromPyArrow, ToPyArrow};
use pyo3::exceptions::{PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyString;
use ursa_io::ids::{self, IdColumnError};

/// The chunks of a pyarrow `ChunkedArray`, as Arrow arrays (zero-copy).
fn chunks_of(column: &Bound<'_, PyAny>) -> PyResult<Vec<ArrayRef>> {
    column
        .getattr("chunks")?
        .try_iter()?
        .map(|c| Ok(make_array(ArrayData::from_pyarrow_bound(&c?)?)))
        .collect()
}

/// A pyarrow `ChunkedArray` of `chunks`, of the pyarrow type `ty`.
fn chunked<'py>(
    py: Python<'py>,
    chunks: &[ArrayRef],
    ty: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let arrays = chunks
        .iter()
        .map(|c| c.to_data().to_pyarrow(py))
        .collect::<PyResult<Vec<_>>>()?;
    py.import("pyarrow")?
        .call_method1("chunked_array", (arrays, ty))
}

/// `table` with each top-level `string_view` column read as strings, by
/// [`ids::unview_target`]'s rule. Other columns are the table's own.
fn without_string_views<'py>(table: Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let py = table.py();
    let schema = Schema::from_pyarrow_bound(&table.getattr("schema")?)?;
    let mut table = table;
    for (i, field) in schema.fields().iter().enumerate() {
        if field.data_type() != &DataType::Utf8View {
            continue;
        }
        let chunks = chunks_of(&table.call_method1("column", (i,))?)?;
        let target = ids::unview_target(&chunks);
        let cast = ids::unview_chunks(&chunks, &target)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let ty = target.to_pyarrow(py)?;
        let pa_field = table
            .getattr("schema")?
            .call_method1("field", (i,))?
            .call_method1("with_type", (&ty,))?;
        let column = chunked(py, &cast, &ty)?;
        table = table.call_method1("set_column", (i, pa_field, column))?;
    }
    Ok(table)
}

/// The column `name` plays `role` (`src`, `dst`, `id`) in: a `TypeError` for a
/// name that is not a string, a `KeyError` naming the table's columns for one
/// it lacks.
fn role_column<'py>(
    table: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
    role: &str,
) -> PyResult<(String, Bound<'py, PyAny>)> {
    let Ok(text) = name.cast::<PyString>() else {
        let ty = name.get_type().name()?;
        return Err(PyTypeError::new_err(format!(
            "{role}= must be a column name (a str); got {} ({ty})",
            name.repr()?
        )));
    };
    let names = table.getattr("column_names")?;
    if !names.contains(text)? {
        return Err(PyKeyError::new_err(format!(
            "{role} column {} is not in the table; it has {}",
            text.repr()?,
            names.repr()?
        )));
    }
    let text = text.to_string();
    let column = table.call_method1("column", (text.as_str(),))?;
    Ok((text, column))
}

/// The column `name` plays `role` in, canonicalized to a node-id column by
/// [`ids::id_chunks`]: a pyarrow `ChunkedArray`.
fn id_column<'py>(
    table: &Bound<'py, PyAny>,
    name: &Bound<'py, PyAny>,
    role: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let py = table.py();
    let (_, column) = role_column(table, name, role)?;
    let ty = column.getattr("type")?;
    let dt = DataType::from_pyarrow_bound(&ty)?;
    let chunks = ids::id_chunks(&chunks_of(&column)?, &dt).map_err(|e| match e {
        IdColumnError::NotAnId => match ty.str() {
            Ok(shown) => PyTypeError::new_err(ids::id_type_message(&shown.to_string())),
            Err(e) => e,
        },
        IdColumnError::OutOfRange(v) => arrow_invalid(
            py,
            format!("Integer value {v} not in range: 0 to {}", i64::MAX),
        ),
        IdColumnError::Arrow(e) => arrow_invalid(py, e.to_string()),
    })?;
    let out = chunks[0].to_data().to_pyarrow(py)?.getattr("type")?;
    chunked(py, &chunks, &out)
}

/// pyarrow's `ArrowInvalid` (a `ValueError`) with `msg`, the error a pyarrow
/// cast raises.
fn arrow_invalid(py: Python<'_>, msg: String) -> PyErr {
    match py
        .import("pyarrow")
        .and_then(|pa| pa.getattr("ArrowInvalid"))
        .and_then(|cls| cls.call1((msg.clone(),)))
    {
        Ok(err) => PyErr::from_value(err),
        Err(_) => PyValueError::new_err(msg),
    }
}

/// An in-memory edge table as an `EdgeFrame` holds it: `(table, (src, dst),
/// dropped)`. The table has its string views read as strings and, with
/// `on_null="drop"`, its rows with a null endpoint removed (`dropped` counts
/// them); `src` and `dst` are its endpoint columns as node ids, chunk for chunk
/// (an `int64` or string chunk is the table's own). Raises for a role that
/// names no column or a column that cannot hold ids, `src` before `dst`.
#[pyfunction]
pub(crate) fn ingest_edges<'py>(
    table: Bound<'py, PyAny>,
    src: Bound<'py, PyAny>,
    dst: Bound<'py, PyAny>,
    on_null: &str,
) -> PyResult<(
    Bound<'py, PyAny>,
    (Bound<'py, PyAny>, Bound<'py, PyAny>),
    usize,
)> {
    let py = table.py();
    let mut table = without_string_views(table)?;
    let mut dropped = 0;
    if on_null == "drop" {
        let (s, _) = role_column(&table, &src, "src")?;
        let (d, _) = role_column(&table, &dst, "dst")?;
        let schema = table.getattr("schema")?;
        let batches: Vec<RecordBatch> = table
            .call_method0("to_batches")?
            .try_iter()?
            .map(|b| RecordBatch::from_pyarrow_bound(&b?))
            .collect::<PyResult<_>>()?;
        let index = |name: &str| -> PyResult<usize> {
            schema.call_method1("get_field_index", (name,))?.extract()
        };
        let (kept, n) = ids::drop_null_endpoint_rows(batches, index(&s)?, index(&d)?)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        if n > 0 {
            let kept = kept
                .into_iter()
                .map(|b| b.to_pyarrow(py))
                .collect::<PyResult<Vec<_>>>()?;
            let tables = py.import("pyarrow")?.getattr("Table")?;
            table = tables.call_method1("from_batches", (kept, schema))?;
        }
        dropped = n;
    }
    let src = id_column(&table, &src, "src")?;
    let dst = id_column(&table, &dst, "dst")?;
    Ok((table, (src, dst), dropped))
}

/// An in-memory node (attribute) table as a `NodeFrame` holds it: string views
/// read as strings, and the `id` column canonicalized to node ids, chunk for
/// chunk. Other columns are the table's own.
#[pyfunction]
pub(crate) fn ingest_nodes<'py>(
    table: Bound<'py, PyAny>,
    id: Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let table = without_string_views(table)?;
    let column = id_column(&table, &id, "id")?;
    let name = id.cast::<PyString>()?.to_string();
    let index = table
        .getattr("schema")?
        .call_method1("get_field_index", (name.as_str(),))?;
    table.call_method1("set_column", (index, name, column))
}
