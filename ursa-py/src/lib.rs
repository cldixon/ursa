//! # ursa-py
//!
//! PyO3 bindings for Ursa. This crate is deliberately *thin*: it moves Arrow
//! across the FFI boundary (zero-copy, via the PyCapsule / C data interface) and
//! calls into `ursa-plan`, which builds and executes one DataFusion plan.
//!
//! ## Two rules this layer enforces (spec §Runtime integration)
//!
//! 1. **Release the GIL** for the duration of execution (`py.detach`).
//! 2. **Arrow FFI via the PyCapsule interface** for zero-copy exchange with
//!    polars/pyarrow — ingress (`FromPyArrow`) and egress (`ToPyArrow`).
//!
//! ## Surface
//!
//! - `build_index` — build (and cache on the Python frame) the CSR topology from
//!   the `(src, dst)` arrays; every graph op over the frame reuses it.
//! - `run_node_query` — node-valued execution: a built index plus a JSON column IR
//!   and a relational tail (filter/sort/limit) in, one pyarrow `RecordBatch` out.
//!   Both `ur.pagerank(edges).collect()` and composed `with_columns(...)`
//!   pipelines funnel through it (weighted algorithms carry an edge batch).
//! - `run_hop_query` / `run_path_query` / `run_walk_query` — the traversals
//!   (`hop`, `shortest_path` incl. weighted Dijkstra, `random_walk`).
//! - `graph_density` / `graph_avg_path_length` / `graph_diameter` /
//!   `graph_describe` — eager whole-graph statistics.
//! - `scan_edges_arrow` / `scan_nodes_arrow` — read a Parquet/CSV edge or node
//!   file (local or object storage) through a DataFusion scan.

use std::collections::HashMap;

use arrow::array::{make_array, Array, ArrayData, ArrayRef, RecordBatch};
use arrow_pyarrow::{FromPyArrow, ToPyArrow};
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ursa_core::topology::Topology;
use ursa_core::IdMap;
use ursa_plan::{
    avg_path_length, build_topology_batches, density, describe, diameter, execute_hop_query,
    execute_node_query, execute_path_query, execute_walk_query, scan_edges_batch, scan_nodes_batch,
    Comparison,
};

// ---------------------------------------------------------------------------
// The Python exception hierarchy (#39.8).
//
// Every error the engine raises reaches Python as a member of this hierarchy, so
// `except ursa.UrsaError` catches the whole native surface and, crucially, so
// does a plain `except Exception` — unlike the `PanicException` (a
// `BaseException`) a Rust panic would raise. The runtime-hardening work (#38/#39)
// removed the panics; this gives what remains a typed, catchable shape.
// ---------------------------------------------------------------------------

create_exception!(
    _ursa,
    UrsaError,
    PyException,
    "Base class for every error Ursa raises from native execution."
);
create_exception!(
    _ursa,
    ColumnNotFoundError,
    UrsaError,
    "A query referenced a column that does not exist in the frame."
);
create_exception!(
    _ursa,
    ComputeError,
    UrsaError,
    "A graph computation failed: an invalid parameter or weight, an unsupported \
     operation, or a kernel/engine error."
);

/// Map an engine error (a `DataFusionError`, or any other `Display` error the
/// plan layer surfaces) to a typed, catchable exception in the Ursa hierarchy. A
/// missing-column error becomes [`ColumnNotFoundError`]; everything else becomes
/// [`ComputeError`]. Both subclass `UrsaError`. The classification is by rendered
/// message rather than by matching internal `DataFusionError`/`SchemaError`
/// variants, so it stays stable across engine version bumps (and lets this layer
/// avoid a direct `datafusion` dependency).
fn to_pyerr<E: std::fmt::Display>(e: E) -> PyErr {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("no field named")
        || (lower.contains("column") && lower.contains("not found"))
        || lower.contains("schema error")
    {
        ColumnNotFoundError::new_err(msg)
    } else {
        ComputeError::new_err(msg)
    }
}

// ---------------------------------------------------------------------------
// Real execution path: pyarrow in -> one DataFusion plan -> pyarrow out.
// ---------------------------------------------------------------------------

/// Import a pyarrow array into an Arrow `ArrayRef`. No type restriction — the id
/// type (int64 or string) is discovered and validated in `ursa-core`
/// (`IdMap::from_edge_arrays` / `dense_from_array`), so ids of either type flow
/// through this one seam.
fn array_from_pyarrow(obj: &Bound<'_, PyAny>) -> PyResult<ArrayRef> {
    Ok(make_array(ArrayData::from_pyarrow_bound(obj)?))
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

/// Build the graph index from an **edge batch list** — a pyarrow list of
/// `RecordBatch`es whose first two columns are the (canonicalized) `src`/`dst`
/// endpoints (int64 or string ids); any further columns are ignored. This is the
/// one place the CSR is constructed. The batches are interned as a *stream* (via
/// [`build_topology_batches`]), so an edge table that arrives in many chunks never
/// has to be concatenated into one contiguous batch first (#60). The id type is
/// auto-detected from the columns; the GIL is released for the build; the counter
/// lets tests assert build-once behaviour.
#[pyfunction]
fn build_index(py: Python<'_>, edges: &Bound<'_, PyAny>) -> PyResult<GraphIndex> {
    let batches = Vec::<RecordBatch>::from_pyarrow_bound(edges)?;
    let (topo, ids) = py.detach(move || {
        let pairs: Vec<(&dyn Array, &dyn Array)> = batches
            .iter()
            .map(|b| (b.column(0).as_ref(), b.column(1).as_ref()))
            .collect();
        build_topology_batches(&pairs).map_err(|e| PyValueError::new_err(e.to_string()))
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
#[pyo3(signature = (index, columns_json, filters, sort=None, limit=None, nodes=None, nodes_id=None, edges=None))]
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
    edges: Option<Bound<'_, PyAny>>,
) -> PyResult<Py<PyAny>> {
    let (topo, ids) = (index.topo.clone(), index.ids.clone());
    let columns_json = columns_json.to_string();
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    // The node attribute table and the edge attribute table (for weight
    // expressions) each cross the FFI as a *list* of RecordBatches, so a large
    // attribute table is never concatenated into one batch (#60).
    let nodes = match nodes {
        Some(obj) => Some(Vec::<RecordBatch>::from_pyarrow_bound(&obj)?),
        None => None,
    };
    let edges = match edges {
        Some(obj) => Some(Vec::<RecordBatch>::from_pyarrow_bound(&obj)?),
        None => None,
    };
    let batches = py.detach(move || {
        execute_node_query(
            topo,
            ids,
            &columns_json,
            &comparisons,
            sort,
            limit,
            nodes,
            nodes_id,
            edges,
        )
        .map_err(to_pyerr)
    })?;
    batches.to_pyarrow(py).map(|obj| obj.unbind())
}

/// Execute a `hop` traversal and return its `(src, dst)` edge batch as pyarrow.
///
/// `seeds` is a pyarrow array of user ids (int64 or string, matching the graph);
/// `n` is the hop count and `direction` one of out/in/both.
/// `filters`/`sort`/`limit`/`distinct` are the optional relational tail applied to
/// the reached edges.
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
) -> PyResult<Py<PyAny>> {
    let (topo, ids) = (index.topo.clone(), index.ids.clone());
    let seeds = array_from_pyarrow(seeds)?;
    let direction = direction.to_string();
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    let batches = py.detach(move || {
        execute_hop_query(
            topo,
            ids,
            seeds.as_ref(),
            n,
            &direction,
            &comparisons,
            sort,
            limit,
            distinct,
        )
        .map_err(to_pyerr)
    })?;
    batches.to_pyarrow(py).map(|obj| obj.unbind())
}

/// Execute a `shortest_path` traversal and return its `(src, dst, hop)` path batch
/// as pyarrow. `source`/`target` are 1-element pyarrow arrays of user ids (int64
/// or string, matching the graph); `direction` is out/in/both. `weight` (an
/// optional serialized weight expression over edge columns) + `edges` (the edge
/// attribute batch) select weighted Dijkstra; omit both for unweighted BFS. The
/// `filters`/`sort`/`limit`/`distinct` tail applies to the path edges.
#[pyfunction]
#[pyo3(signature = (index, source, target, direction, weight=None, edges=None, filters=Vec::new(), sort=None, limit=None, distinct=false))]
#[allow(clippy::too_many_arguments)]
fn run_path_query(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    source: &Bound<'_, PyAny>,
    target: &Bound<'_, PyAny>,
    direction: &str,
    weight: Option<String>,
    edges: Option<Bound<'_, PyAny>>,
    filters: Vec<(String, String, f64)>,
    sort: Option<(String, bool)>,
    limit: Option<usize>,
    distinct: bool,
) -> PyResult<Py<PyAny>> {
    let (topo, ids) = (index.topo.clone(), index.ids.clone());
    let source = array_from_pyarrow(source)?;
    let target = array_from_pyarrow(target)?;
    let direction = direction.to_string();
    // The edge attribute table (for a weighted path) crosses as a batch list.
    let edges = match edges {
        Some(obj) => Some(Vec::<RecordBatch>::from_pyarrow_bound(&obj)?),
        None => None,
    };
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    let batches = py.detach(move || {
        execute_path_query(
            topo,
            ids,
            source.as_ref(),
            target.as_ref(),
            &direction,
            weight.as_deref(),
            edges,
            &comparisons,
            sort,
            limit,
            distinct,
        )
        .map_err(to_pyerr)
    })?;
    batches.to_pyarrow(py).map(|obj| obj.unbind())
}

/// Execute a `random_walk` and return its `(walk_id, step, node)` node batch as
/// pyarrow. `starts` is a pyarrow array of user ids (int64 or string, matching the
/// graph); `seed` (optional) makes the walk reproducible. The
/// `filters`/`sort`/`limit`/`distinct` tail applies to the walk rows.
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
) -> PyResult<Py<PyAny>> {
    let (topo, ids) = (index.topo.clone(), index.ids.clone());
    let starts = array_from_pyarrow(starts)?;
    let comparisons: Vec<Comparison> = filters
        .into_iter()
        .map(|(column, op, value)| Comparison { column, op, value })
        .collect();
    let batches = py.detach(move || {
        execute_walk_query(
            topo,
            ids,
            starts.as_ref(),
            steps,
            walks_per_node,
            seed,
            &comparisons,
            sort,
            limit,
            distinct,
        )
        .map_err(to_pyerr)
    })?;
    batches.to_pyarrow(py).map(|obj| obj.unbind())
}

/// Whole-graph directed edge density (eager scalar).
#[pyfunction]
fn graph_density(py: Python<'_>, index: PyRef<'_, GraphIndex>) -> PyResult<f64> {
    let topo = index.topo.clone();
    py.detach(move || density(&topo).map_err(to_pyerr))
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
    py.detach(move || avg_path_length(&topo, sample).map_err(to_pyerr))
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
    py.detach(move || diameter(&topo, approximate).map_err(to_pyerr))
}

/// Whole-graph one-row summary (`n_nodes, n_edges, density, avg_degree,
/// n_components`) as a pyarrow `RecordBatch`. `full` computes `n_components`.
#[pyfunction]
fn graph_describe(py: Python<'_>, index: PyRef<'_, GraphIndex>, full: bool) -> PyResult<Py<PyAny>> {
    let topo = index.topo.clone();
    let batch = py.detach(move || describe(&topo, full).map_err(to_pyerr))?;
    batch.to_pyarrow(py).map(|obj| obj.unbind())
}

/// Read a Parquet/CSV edge file's `src`/`dst` columns (plus any `weight_columns`
/// needed to evaluate a `weight=` expression) through a DataFusion scan and hand
/// them back as a pyarrow `RecordBatch`. `path` may be local or object storage
/// (`s3://`/`gs://`/`az://`/`file://`); `storage_options` seeds the backend's
/// credentials/config.
#[pyfunction]
#[pyo3(signature = (path, src, dst, storage_options=None, weight_columns=Vec::new()))]
fn scan_edges_arrow(
    py: Python<'_>,
    path: &str,
    src: &str,
    dst: &str,
    storage_options: Option<HashMap<String, String>>,
    weight_columns: Vec<String>,
) -> PyResult<Py<PyAny>> {
    let opts = storage_options.unwrap_or_default();
    let batches =
        py.detach(|| scan_edges_batch(path, src, dst, &opts, &weight_columns).map_err(to_pyerr))?;
    batches.to_pyarrow(py).map(|obj| obj.unbind())
}

/// Read a Parquet/CSV node/attribute file through a DataFusion scan and hand it
/// back as a batch list (`id` cast to int64). It feeds the `nodes` attribute slot
/// of `run_node_query`, exactly like an in-memory table. `columns` is a projection
/// pushdown: when non-empty only those columns are read from the file (it must
/// include the `id` column); empty reads all columns. `path`/`storage_options`
/// support object storage like `scan_edges_arrow`.
#[pyfunction]
#[pyo3(signature = (path, id, storage_options=None, columns=Vec::new()))]
fn scan_nodes_arrow(
    py: Python<'_>,
    path: &str,
    id: &str,
    storage_options: Option<HashMap<String, String>>,
    columns: Vec<String>,
) -> PyResult<Py<PyAny>> {
    let opts = storage_options.unwrap_or_default();
    let batches = py.detach(|| scan_nodes_batch(path, id, &opts, &columns).map_err(to_pyerr))?;
    batches.to_pyarrow(py).map(|obj| obj.unbind())
}

/// The `ursa-core` version — the simplest possible proof the native module loaded.
#[pyfunction]
fn __core_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The native extension module, imported by Python as `ursa._ursa`.
#[pymodule]
fn _ursa(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // the exception hierarchy (see the `create_exception!` block above)
    m.add("UrsaError", m.py().get_type::<UrsaError>())?;
    m.add(
        "ColumnNotFoundError",
        m.py().get_type::<ColumnNotFoundError>(),
    )?;
    m.add("ComputeError", m.py().get_type::<ComputeError>())?;
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
    Ok(())
}
