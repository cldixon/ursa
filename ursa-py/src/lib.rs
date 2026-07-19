//! # ursa-py
//!
//! PyO3 bindings for Ursa. This crate is deliberately *thin*: its job is to build
//! plans, orchestrate `collect()`, and move Arrow across the FFI boundary
//! (zero-copy, via the Arrow PyCapsule / C data interface). Compute lives in
//! `ursa-core`; planning and execution live in `ursa-plan`.
//!
//! ## Two rules this layer enforces (spec §Runtime integration)
//!
//! 1. **Release the GIL** for the duration of execution (`py.allow_threads`) so
//!    Ursa behaves inside threaded Python servers.
//! 2. **Arrow FFI via the PyCapsule interface** for zero-copy exchange with
//!    polars/pyarrow — both ingress (`FromPyArrow`) and egress (`ToPyArrow`).
//!
//! ## Status: walking skeleton
//!
//! `run_*` execute the node-valued kernels end-to-end through a real DataFusion
//! `ExecutionPlan` (`ursa_plan::GraphAlgorithmExec`): pyarrow edge arrays in →
//! `Arc<Topology>` → kernel → pyarrow `RecordBatch` out. The Python `collect()`
//! path calls these. The older `_demo_*` functions (plain lists, no Arrow) remain
//! for the pure Python→Rust smoke tests.

use arrow::array::{make_array, Array, ArrayData, Int64Array};
use arrow::pyarrow::{FromPyArrow, ToPyArrow};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use ursa_core::algo::{connected_components_weak, degree, pagerank, PageRankParams};
use ursa_core::topology::{Direction as CoreDirection, Topology};
use ursa_plan::logical::Direction;
use ursa_plan::{build_topology, run_algorithm, GraphAlgo};

// ---------------------------------------------------------------------------
// Real execution path: pyarrow in → DataFusion ExecutionPlan → pyarrow out.
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

/// Run a node-valued algorithm and hand the `(id, value)` batch back as a
/// zero-copy pyarrow `RecordBatch`. The GIL is released across build + compute.
fn run_to_pyarrow(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &Bound<'_, PyAny>,
    algo: GraphAlgo,
) -> PyResult<PyObject> {
    let src = int64_from_pyarrow(src)?;
    let dst = int64_from_pyarrow(dst)?;
    let batch = py.allow_threads(move || {
        let (topo, ids) =
            build_topology(&src, &dst).map_err(|e| PyValueError::new_err(e.to_string()))?;
        run_algorithm(topo, ids, algo).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

#[pyfunction]
#[pyo3(signature = (src, dst, damping=0.85, max_iter=30, tol=1e-6))]
fn run_pagerank(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &Bound<'_, PyAny>,
    damping: f64,
    max_iter: u32,
    tol: f64,
) -> PyResult<PyObject> {
    run_to_pyarrow(
        py,
        src,
        dst,
        GraphAlgo::PageRank {
            damping,
            max_iter,
            tol,
        },
    )
}

#[pyfunction]
#[pyo3(signature = (src, dst, direction="out"))]
fn run_degree(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &Bound<'_, PyAny>,
    direction: &str,
) -> PyResult<PyObject> {
    run_to_pyarrow(
        py,
        src,
        dst,
        GraphAlgo::Degree {
            direction: parse_direction(direction)?,
        },
    )
}

#[pyfunction]
fn run_connected_components(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    run_to_pyarrow(
        py,
        src,
        dst,
        GraphAlgo::ConnectedComponents { strong: false },
    )
}

#[pyfunction]
fn run_triangle_count(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &Bound<'_, PyAny>,
) -> PyResult<PyObject> {
    run_to_pyarrow(py, src, dst, GraphAlgo::TriangleCount)
}

fn parse_direction(direction: &str) -> PyResult<Direction> {
    match direction {
        "out" => Ok(Direction::Out),
        "in" => Ok(Direction::In),
        "both" => Ok(Direction::Both),
        other => Err(PyValueError::new_err(format!(
            "direction must be 'out', 'in', or 'both'; got {other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Demo kernels (plain lists, no Arrow) — the pure Python→PyO3→ursa-core proof.
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
    m.add_function(wrap_pyfunction!(run_pagerank, m)?)?;
    m.add_function(wrap_pyfunction!(run_degree, m)?)?;
    m.add_function(wrap_pyfunction!(run_connected_components, m)?)?;
    m.add_function(wrap_pyfunction!(run_triangle_count, m)?)?;
    // demo path
    m.add_function(wrap_pyfunction!(_demo_pagerank, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_degree, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_connected_components, m)?)?;
    Ok(())
}
