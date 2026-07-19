//! # ursa-py
//!
//! PyO3 bindings for Ursa. This crate is deliberately *thin*: it moves Arrow
//! across the FFI boundary (zero-copy, via the PyCapsule / C data interface) and
//! calls into `ursa-plan`, which builds and executes one DataFusion plan.
//!
//! ## Two rules this layer enforces (spec §Runtime integration)
//!
//! 1. **Release the GIL** for the duration of execution (`py.allow_threads`).
//! 2. **Arrow FFI via the PyCapsule interface** for zero-copy exchange with
//!    polars/pyarrow — ingress (`FromPyArrow`) and egress (`ToPyArrow`).
//!
//! ## Surface
//!
//! - `run_node_query` — the one execution entry point: pyarrow edge arrays plus a
//!   JSON column IR and a relational tail (filter/sort/limit) in, one pyarrow
//!   `RecordBatch` out. Both `ur.pagerank(edges).collect()` and composed
//!   `with_columns(...)` pipelines funnel through it.
//! - `scan_edges_arrow` — read a Parquet/CSV edge file through a DataFusion scan.
//! - `_demo_*` — plain-list Python→Rust smoke kernels (no Arrow); kept for tests.

use arrow::array::{make_array, Array, ArrayData, Int64Array};
use arrow::pyarrow::{FromPyArrow, ToPyArrow};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use ursa_core::algo::{connected_components_weak, degree, pagerank, PageRankParams};
use ursa_core::topology::{Direction as CoreDirection, Topology};
use ursa_plan::{density, execute_node_query, scan_edges_batch, Comparison};

// ---------------------------------------------------------------------------
// Real execution path: pyarrow in -> one DataFusion plan -> pyarrow out.
// ---------------------------------------------------------------------------

/// Read a pyarrow array into an `Int64Array`, erroring if it is not int64.
fn int64_from_pyarrow(obj: &Bound<'_, PyAny>) -> PyResult<Int64Array> {
    let array = make_array(ArrayData::from_pyarrow_bound(obj)?);
    array
        .as_any()
        .downcast_ref::<Int64Array>()
        .cloned()
        .ok_or_else(|| {
            PyValueError::new_err("src/dst must be an int64 array (cast ids to int64 first)")
        })
}

/// Execute a graph query and return its `(id, values...)` batch as pyarrow.
///
/// `columns_json` is the query's output-column IR (a JSON list of
/// `{name, kind, ...params}`); `filters` are `(column, op, value)` comparisons;
/// `sort` is `(column, descending)`. The GIL is released across build + compute.
#[pyfunction]
#[pyo3(signature = (src, dst, columns_json, filters, sort=None, limit=None))]
fn run_node_query(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &Bound<'_, PyAny>,
    columns_json: &str,
    filters: Vec<(String, String, f64)>,
    sort: Option<(String, bool)>,
    limit: Option<usize>,
) -> PyResult<PyObject> {
    let src = int64_from_pyarrow(src)?;
    let dst = int64_from_pyarrow(dst)?;
    let columns_json = columns_json.to_string();
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    let batch = py.allow_threads(move || {
        execute_node_query(&src, &dst, &columns_json, &comparisons, sort, limit)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

/// Whole-graph directed edge density (eager scalar).
#[pyfunction]
fn graph_density(py: Python<'_>, src: &Bound<'_, PyAny>, dst: &Bound<'_, PyAny>) -> PyResult<f64> {
    let src = int64_from_pyarrow(src)?;
    let dst = int64_from_pyarrow(dst)?;
    py.allow_threads(move || {
        density(&src, &dst).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })
}

/// Read a Parquet/CSV edge file's `src`/`dst` columns through a DataFusion scan
/// and hand them back as a two-column `(src, dst)` pyarrow `RecordBatch`.
#[pyfunction]
fn scan_edges_arrow(py: Python<'_>, path: &str, src: &str, dst: &str) -> PyResult<PyObject> {
    let batch = py.allow_threads(|| {
        scan_edges_batch(path, src, dst).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

// ---------------------------------------------------------------------------
// Demo kernels (plain lists, no Arrow) — the pure Python->PyO3->ursa-core proof.
// ---------------------------------------------------------------------------

fn build_demo_topology(src: &[i64], dst: &[i64]) -> (Topology, Vec<i64>) {
    use ursa_core::IdMap;
    let mut map = IdMap::default();
    let src_dense: Vec<u32> = src.iter().map(|&u| map.intern(u)).collect();
    let dst_dense: Vec<u32> = dst.iter().map(|&u| map.intern(u)).collect();
    let n = map.len();
    (
        Topology::build(n, src_dense, dst_dense),
        map.user_ids().to_vec(),
    )
}

/// The `ursa-core` version — the simplest possible proof the native module loaded.
#[pyfunction]
fn __core_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[pyfunction]
#[pyo3(signature = (src, dst, damping=0.85, max_iter=30, tol=1e-6))]
fn _demo_pagerank(
    py: Python<'_>,
    src: Vec<i64>,
    dst: Vec<i64>,
    damping: f64,
    max_iter: u32,
    tol: f64,
) -> Vec<(i64, f64)> {
    let (topo, ids) = build_demo_topology(&src, &dst);
    let scores = py.allow_threads(|| {
        pagerank(
            &topo,
            PageRankParams {
                damping,
                max_iter,
                tol,
            },
        )
    });
    ids.into_iter().zip(scores).collect()
}

#[pyfunction]
#[pyo3(signature = (src, dst, direction="out"))]
fn _demo_degree(src: Vec<i64>, dst: Vec<i64>, direction: &str) -> PyResult<Vec<(i64, u32)>> {
    let dir = match direction {
        "out" => CoreDirection::Out,
        "in" => CoreDirection::In,
        "both" => CoreDirection::Both,
        other => {
            return Err(PyValueError::new_err(format!(
                "direction must be 'out', 'in', or 'both'; got {other:?}"
            )))
        }
    };
    let (topo, ids) = build_demo_topology(&src, &dst);
    let deg = degree(&topo, dir);
    Ok(ids.into_iter().zip(deg).collect())
}

#[pyfunction]
fn _demo_connected_components(src: Vec<i64>, dst: Vec<i64>) -> Vec<(i64, u32)> {
    let (topo, ids) = build_demo_topology(&src, &dst);
    let cc = connected_components_weak(&topo);
    ids.into_iter().zip(cc).collect()
}

/// The native extension module, imported by Python as `ursa._ursa`.
#[pymodule]
fn _ursa(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(__core_version, m)?)?;
    // real execution path
    m.add_function(wrap_pyfunction!(run_node_query, m)?)?;
    m.add_function(wrap_pyfunction!(graph_density, m)?)?;
    m.add_function(wrap_pyfunction!(scan_edges_arrow, m)?)?;
    // demo path
    m.add_function(wrap_pyfunction!(_demo_pagerank, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_degree, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_connected_components, m)?)?;
    Ok(())
}
