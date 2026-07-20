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

use std::collections::HashMap;

use arrow::array::{make_array, Array, ArrayData, Int64Array, RecordBatch};
use arrow::pyarrow::{FromPyArrow, ToPyArrow};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ursa_core::algo::{connected_components_weak, degree, pagerank, PageRankParams};
use ursa_core::topology::{Direction as CoreDirection, Topology};
use ursa_core::IdMap;
use ursa_plan::{
    avg_path_length, build_topology, density, describe, diameter, execute_hop_query,
    execute_node_query, execute_path_query, execute_walk_query, scan_edges_batch, scan_nodes_batch,
    Comparison,
};

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

/// Counts topology builds, so tests can prove the index-preservation contract
/// (a multi-algorithm pipeline over one frame builds the CSR exactly once).
static TOPOLOGY_BUILDS: AtomicUsize = AtomicUsize::new(0);

/// A built graph index — the CSR `Topology` plus the `IdMap` — cached on the
/// Python `EdgeFrame` and passed to every graph op over that frame, so the
/// topology is built once instead of on every `collect()` (spec §index-
/// preservation contract). Opaque to Python; produced only by [`build_index`].
#[pyclass]
struct GraphIndex {
    topo: Arc<Topology>,
    ids: Arc<IdMap>,
}

/// Build the graph index from `(src, dst)` int64 arrays — the one place the CSR
/// is constructed. The GIL is released for the build; the counter lets tests
/// assert build-once behaviour.
#[pyfunction]
fn build_index(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &Bound<'_, PyAny>,
) -> PyResult<GraphIndex> {
    let src = int64_from_pyarrow(src)?;
    let dst = int64_from_pyarrow(dst)?;
    let (topo, ids) = py.allow_threads(move || {
        build_topology(&src, &dst).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    TOPOLOGY_BUILDS.fetch_add(1, Ordering::Relaxed);
    Ok(GraphIndex { topo, ids })
}

/// Total topology builds so far (test/observability hook for the index contract).
#[pyfunction]
fn _topology_build_count() -> usize {
    TOPOLOGY_BUILDS.load(Ordering::Relaxed)
}

/// Execute a graph query and return its `(id, values...)` batch as pyarrow.
///
/// `columns_json` is the query's output-column IR (a JSON list of
/// `{name, kind, ...params}`); `filters` are `(column, op, value)` comparisons;
/// `sort` is `(column, descending)`. The GIL is released across build + compute.
#[pyfunction]
#[pyo3(signature = (index, columns_json, filters, sort=None, limit=None, nodes=None, nodes_id=None))]
#[allow(clippy::too_many_arguments)]
fn run_node_query(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    columns_json: &str,
    filters: Vec<(String, String, f64)>,
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    nodes: Option<Bound<'_, PyAny>>,
    nodes_id: Option<String>,
) -> PyResult<PyObject> {
    let (topo, ids) = (index.topo.clone(), index.ids.clone());
    let columns_json = columns_json.to_string();
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    let nodes = match nodes {
        Some(obj) => Some(RecordBatch::from_pyarrow_bound(&obj)?),
        None => None,
    };
    let batch = py.allow_threads(move || {
        execute_node_query(
            topo,
            ids,
            &columns_json,
            &comparisons,
            sort,
            limit,
            nodes,
            nodes_id,
        )
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

/// Execute a `hop` traversal and return its `(src, dst)` edge batch as pyarrow.
///
/// `seeds` is an int64 pyarrow array of user ids; `n` is the hop count and
/// `direction` one of out/in/both. `filters`/`sort`/`limit`/`distinct` are the
/// optional relational tail applied to the reached edges.
#[pyfunction]
#[pyo3(signature = (index, seeds, n, direction, filters, sort=None, limit=None, distinct=false))]
#[allow(clippy::too_many_arguments)]
fn run_hop_query(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    seeds: &Bound<'_, PyAny>,
    n: u32,
    direction: &str,
    filters: Vec<(String, String, f64)>,
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    distinct: bool,
) -> PyResult<PyObject> {
    let (topo, ids) = (index.topo.clone(), index.ids.clone());
    let seeds = int64_from_pyarrow(seeds)?;
    let direction = direction.to_string();
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    let batch = py.allow_threads(move || {
        execute_hop_query(
            topo,
            ids,
            &seeds,
            n,
            &direction,
            &comparisons,
            sort,
            limit,
            distinct,
        )
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

/// Execute a `shortest_path` traversal and return its `(src, dst, hop)` path batch
/// as pyarrow. `source`/`target` are int64 user ids; `direction` is out/in/both.
/// `weighted` is reserved (errors for now — v0.1 is unweighted BFS). The
/// `filters`/`sort`/`limit`/`distinct` tail applies to the path edges.
#[pyfunction]
#[pyo3(signature = (index, source, target, direction, weighted=false, filters=Vec::new(), sort=None, limit=None, distinct=false))]
#[allow(clippy::too_many_arguments)]
fn run_path_query(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    source: i64,
    target: i64,
    direction: &str,
    weighted: bool,
    filters: Vec<(String, String, f64)>,
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    distinct: bool,
) -> PyResult<PyObject> {
    let (topo, ids) = (index.topo.clone(), index.ids.clone());
    let direction = direction.to_string();
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    let batch = py.allow_threads(move || {
        execute_path_query(
            topo,
            ids,
            source,
            target,
            &direction,
            weighted,
            &comparisons,
            sort,
            limit,
            distinct,
        )
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

/// Execute a `random_walk` and return its `(walk_id, step, node)` node batch as
/// pyarrow. `starts` is an int64 pyarrow array of user ids; `seed` (optional)
/// makes the walk reproducible. The `filters`/`sort`/`limit`/`distinct` tail
/// applies to the walk rows.
#[pyfunction]
#[pyo3(signature = (index, starts, steps, walks_per_node, seed=None, filters=Vec::new(), sort=None, limit=None, distinct=false))]
#[allow(clippy::too_many_arguments)]
fn run_walk_query(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    starts: &Bound<'_, PyAny>,
    steps: u32,
    walks_per_node: u32,
    seed: Option<u64>,
    filters: Vec<(String, String, f64)>,
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    distinct: bool,
) -> PyResult<PyObject> {
    let (topo, ids) = (index.topo.clone(), index.ids.clone());
    let starts = int64_from_pyarrow(starts)?;
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    let batch = py.allow_threads(move || {
        execute_walk_query(
            topo,
            ids,
            &starts,
            steps,
            walks_per_node,
            seed,
            &comparisons,
            sort,
            limit,
            distinct,
        )
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

/// Whole-graph directed edge density (eager scalar).
#[pyfunction]
fn graph_density(py: Python<'_>, index: PyRef<'_, GraphIndex>) -> PyResult<f64> {
    let topo = index.topo.clone();
    py.allow_threads(move || density(&topo).map_err(|e| PyRuntimeError::new_err(e.to_string())))
}

/// Average shortest-path length over reachable ordered pairs (eager scalar).
/// `sample` (a fraction) estimates from a subset of sources; omit for exact.
#[pyfunction]
#[pyo3(signature = (index, sample=None))]
fn graph_avg_path_length(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    sample: Option<f64>,
) -> PyResult<f64> {
    let topo = index.topo.clone();
    py.allow_threads(move || {
        avg_path_length(&topo, sample).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })
}

/// Graph diameter (eager scalar). `approximate` (default true) is a lower-bound
/// estimate; pass false for exact.
#[pyfunction]
fn graph_diameter(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    approximate: bool,
) -> PyResult<i64> {
    let topo = index.topo.clone();
    py.allow_threads(move || {
        diameter(&topo, approximate).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })
}

/// Whole-graph one-row summary (`n_nodes, n_edges, density, avg_degree,
/// n_components`) as a pyarrow `RecordBatch`. `full` computes `n_components`.
#[pyfunction]
fn graph_describe(py: Python<'_>, index: PyRef<'_, GraphIndex>, full: bool) -> PyResult<PyObject> {
    let topo = index.topo.clone();
    let batch = py.allow_threads(move || {
        describe(&topo, full).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

/// Read a Parquet/CSV edge file's `src`/`dst` columns through a DataFusion scan
/// and hand them back as a two-column `(src, dst)` pyarrow `RecordBatch`. `path`
/// may be local or object storage (`s3://`/`gs://`/`az://`/`file://`);
/// `storage_options` seeds the backend's credentials/config.
#[pyfunction]
#[pyo3(signature = (path, src, dst, storage_options=None))]
fn scan_edges_arrow(
    py: Python<'_>,
    path: &str,
    src: &str,
    dst: &str,
    storage_options: Option<HashMap<String, String>>,
) -> PyResult<PyObject> {
    let opts = storage_options.unwrap_or_default();
    let batch = py.allow_threads(|| {
        scan_edges_batch(path, src, dst, &opts).map_err(|e| PyRuntimeError::new_err(e.to_string()))
    })?;
    batch.to_pyarrow(py)
}

/// Read a Parquet/CSV node/attribute file through a DataFusion scan and hand it
/// back as a full `RecordBatch` (all columns; `id` cast to int64). It feeds the
/// `nodes` attribute slot of `run_node_query`, exactly like an in-memory table.
/// `path`/`storage_options` support object storage like `scan_edges_arrow`.
#[pyfunction]
#[pyo3(signature = (path, id, storage_options=None))]
fn scan_nodes_arrow(
    py: Python<'_>,
    path: &str,
    id: &str,
    storage_options: Option<HashMap<String, String>>,
) -> PyResult<PyObject> {
    let opts = storage_options.unwrap_or_default();
    let batch = py.allow_threads(|| {
        scan_nodes_batch(path, id, &opts).map_err(|e| PyRuntimeError::new_err(e.to_string()))
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
    // the cached graph index (built once per frame)
    m.add_class::<GraphIndex>()?;
    m.add_function(wrap_pyfunction!(build_index, m)?)?;
    m.add_function(wrap_pyfunction!(_topology_build_count, m)?)?;
    // real execution path
    m.add_function(wrap_pyfunction!(run_node_query, m)?)?;
    m.add_function(wrap_pyfunction!(run_hop_query, m)?)?;
    m.add_function(wrap_pyfunction!(run_path_query, m)?)?;
    m.add_function(wrap_pyfunction!(run_walk_query, m)?)?;
    m.add_function(wrap_pyfunction!(graph_density, m)?)?;
    m.add_function(wrap_pyfunction!(graph_avg_path_length, m)?)?;
    m.add_function(wrap_pyfunction!(graph_diameter, m)?)?;
    m.add_function(wrap_pyfunction!(graph_describe, m)?)?;
    m.add_function(wrap_pyfunction!(scan_edges_arrow, m)?)?;
    m.add_function(wrap_pyfunction!(scan_nodes_arrow, m)?)?;
    // demo path
    m.add_function(wrap_pyfunction!(_demo_pagerank, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_degree, m)?)?;
    m.add_function(wrap_pyfunction!(_demo_connected_components, m)?)?;
    Ok(())
}
