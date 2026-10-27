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
//! - `run_plan` — the one execution entry point: a serialized plan (see
//!   `ursa_plan::plan`) and a host object in, the result batches out. The plan
//!   reads its inputs (cached indexes, tables, masks, seeds) from the host by slot.
//! - `build_index` / `scan_build_index` — build (and cache on the Python frame) the
//!   CSR topology; every graph op over the frame reuses it.
//! - `resolve_index` — the checked index of an edge plan, for the eager stats.
//! - `hop_reached_nodes_query` / `shortest_path_nodes_query` — the reached node
//!   set behind a graph op over a traversal result (the host's edge mask, #116).
//! - `graph_density` / `graph_avg_path_length` / `graph_diameter` — eager
//!   whole-graph statistics.
//! - `scan_edges_arrow` / `scan_edges_table_arrow` / `scan_nodes_arrow` — read a
//!   Parquet/CSV edge or node file (local or object storage) through a
//!   DataFusion scan.

use std::collections::HashMap;

use arrow::array::{make_array, Array, ArrayData, ArrayRef, BooleanArray, RecordBatch};
use arrow_pyarrow::{FromPyArrow, ToPyArrow};
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyNotImplementedError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ursa_core::topology::Topology;
use ursa_core::{EdgeMask, IdMap};

use ursa_plan::plan::{EdgeInput, HostError, Roles, Slot};
use ursa_plan::{
    avg_path_length, build_topology_batches, density, diameter, execute_plan, hop_reached_nodes,
    parse_stat_direction, require_index, scan_edges_batch, scan_edges_table, scan_edges_topology,
    scan_nodes_batch, shortest_path_nodes, weight_cache_key, Inputs, Plan, PlanError, PlanResult,
    ScanBuildError, ScannedTopology,
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

/// Build a subgraph [`EdgeMask`] from a pyarrow **boolean** array whose element `e`
/// says whether original edge row `e` is in the subgraph (#114). The array is in
/// original edge-row order — the order the topology was built from — so `mask.keep`
/// aligns with the CSR's `edge_ids`. A null is treated as *not kept*.
///
/// The mask must cover exactly the graph's `n_edges` rows: a shorter one would
/// silently drop every edge past its end (`keep` is false out of range).
fn edge_mask_from_pyarrow(obj: &Bound<'_, PyAny>, n_edges: usize) -> PyResult<Arc<EdgeMask>> {
    let arr = array_from_pyarrow(obj)?;
    let b = arr
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| PyValueError::new_err("edge_mask must be a pyarrow boolean array"))?;
    if b.len() != n_edges {
        return Err(PyValueError::new_err(format!(
            "edge_mask length ({}) does not match the edge count ({n_edges})",
            b.len()
        )));
    }
    Ok(Arc::new(EdgeMask::from_boolean_array(b)))
}

/// Counts topology builds, so tests can prove the index-preservation contract
/// (a multi-algorithm pipeline over one frame builds the CSR exactly once).
static TOPOLOGY_BUILDS: AtomicUsize = AtomicUsize::new(0);

/// A built graph index — the CSR `Topology` plus the `IdMap` — cached on the
/// Python `EdgeFrame` and passed to every graph op over that frame, so the
/// topology is built once instead of on every `collect()` (spec §index-
/// preservation contract). Opaque to Python; produced only by [`build_index`].
///
/// It also carries the weight vectors evaluated over the frame's edge rows, so a
/// repeated weighted query evaluates its `weight=` expression once (#175).
#[pyclass]
struct GraphIndex(ursa_plan::GraphIndex);

impl GraphIndex {
    fn new(topo: Arc<Topology>, ids: Arc<IdMap>) -> Self {
        GraphIndex(ursa_plan::GraphIndex::new(topo, ids))
    }
}

#[pymethods]
impl GraphIndex {
    /// Whether the weights for this serialized weight expression are already
    /// cached, so a weighted query need not read the edge weight columns again.
    fn has_weights(&self, weight_json: &str) -> PyResult<bool> {
        let key = weight_cache_key(weight_json).map_err(to_pyerr)?;
        Ok(self.0.weights.get(&key).is_some())
    }

    /// Weight evaluations run against this index (test/observability hook).
    fn _weight_eval_count(&self) -> usize {
        self.0.weights.evaluations()
    }
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
    Ok(GraphIndex::new(topo, ids))
}

/// Scan an edge file and build its graph index in one native call, returning the
/// index and the number of null-endpoint rows dropped (always 0 unless
/// `drop_nulls`, the `on_null="drop"` opt-in).
///
/// Equivalent to `build_index(scan_edges_arrow(...))` for an unweighted scan, but
/// the scanned Arrow endpoints never accumulate: each batch is interned and freed
/// as it is decoded (`scan_edges_topology`), which takes the 16 B/edge input out of
/// the build's peak (#149). A weighted scan keeps the two-step path, since its
/// weight columns must outlive the build.
#[pyfunction]
#[pyo3(signature = (path, src, dst, storage_options=None, drop_nulls=false))]
fn scan_build_index(
    py: Python<'_>,
    path: &str,
    src: &str,
    dst: &str,
    storage_options: Option<HashMap<String, String>>,
    drop_nulls: bool,
) -> PyResult<(GraphIndex, usize)> {
    let opts = storage_options.unwrap_or_default();
    let scanned = py.detach(|| {
        scan_edges_topology(path, src, dst, &opts, drop_nulls).map_err(|e| match e {
            ScanBuildError::Scan(e) => to_pyerr(e),
            ScanBuildError::Build(e) => PyValueError::new_err(e.to_string()),
        })
    })?;
    TOPOLOGY_BUILDS.fetch_add(1, Ordering::Relaxed);
    let ScannedTopology { topo, ids, dropped } = scanned;
    Ok((GraphIndex::new(topo, ids), dropped))
}

/// Total topology builds so far (test/observability hook for the index contract).
#[pyfunction]
fn _topology_build_count() -> usize {
    TOPOLOGY_BUILDS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// The plan entry point and its host.
// ---------------------------------------------------------------------------

/// Map a [`PlanError`] to the Python exception it stands for: a shape guard to
/// `NotImplementedError` (with the message the Python layer used to raise), an
/// engine error through [`to_pyerr`], and a host error back to the exception the
/// host raised.
fn plan_err(e: PlanError) -> PyErr {
    match e {
        PlanError::NotImplemented(m) => PyNotImplementedError::new_err(m),
        PlanError::ColumnNotFound(m) => ColumnNotFoundError::new_err(m),
        PlanError::Value(m) => PyValueError::new_err(m),
        e @ PlanError::Invalid(_) => PyValueError::new_err(e.to_string()),
        PlanError::Engine(e) => to_pyerr(e),
        PlanError::Host(e) => match e.payload.downcast::<PyErr>() {
            Ok(err) => *err,
            Err(_) => ComputeError::new_err(e.message),
        },
    }
}

/// Carry an exception the host raised through the plan executor unchanged.
fn host_err(py: Python<'_>, err: PyErr) -> PlanError {
    let message = err
        .value(py)
        .str()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let not_implemented = err.is_instance_of::<PyNotImplementedError>(py);
    PlanError::Host(HostError {
        message,
        not_implemented,
        payload: Box::new(err),
    })
}

/// The Python host of a plan (`ursa._execute._Host`): each [`Inputs`] method calls
/// the host method of the same name, with the GIL held for just that call.
struct PyInputs {
    host: Py<PyAny>,
}

impl PyInputs {
    fn call<T>(
        &self,
        name: &str,
        args: impl for<'py> pyo3::call::PyCallArgs<'py>,
        convert: impl for<'py> FnOnce(&Bound<'py, PyAny>) -> PyResult<T>,
    ) -> PlanResult<T> {
        Python::attach(|py| {
            let out = self.host.bind(py).call_method1(name, args);
            out.and_then(|obj| convert(&obj))
                .map_err(|e| host_err(py, e))
        })
    }
}

fn batch_list(obj: &Bound<'_, PyAny>) -> PyResult<Vec<RecordBatch>> {
    Vec::<RecordBatch>::from_pyarrow_bound(obj)
}

impl Inputs for PyInputs {
    fn index(&self, frame: Slot) -> PlanResult<ursa_plan::GraphIndex> {
        self.call("index", (frame,), |obj| {
            Ok(obj.extract::<PyRef<'_, GraphIndex>>()?.0.clone())
        })
    }

    fn edge_mask(&self, frame: Slot, n_edges: usize) -> PlanResult<Option<Arc<EdgeMask>>> {
        self.call("edge_mask", (frame,), |obj| {
            if obj.is_none() {
                return Ok(None);
            }
            edge_mask_from_pyarrow(obj, n_edges).map(Some)
        })
    }

    fn edge_weights(
        &self,
        frame: Slot,
        columns: &[String],
        keys: &[String],
    ) -> PlanResult<Option<Vec<RecordBatch>>> {
        let args = (frame, columns.to_vec(), keys.to_vec());
        self.call("edge_weights", args, |obj| {
            if obj.is_none() {
                return Ok(None);
            }
            batch_list(obj).map(Some)
        })
    }

    fn node_table(&self, frame: Slot, columns: Option<&[String]>) -> PlanResult<Vec<RecordBatch>> {
        self.call(
            "node_table",
            (frame, columns.map(<[String]>::to_vec)),
            batch_list,
        )
    }

    fn edge_table(&self, frame: Slot, columns: Option<&[String]>) -> PlanResult<Vec<RecordBatch>> {
        self.call(
            "edge_table",
            (frame, columns.map(<[String]>::to_vec)),
            batch_list,
        )
    }

    fn seeds(&self, slot: Slot) -> PlanResult<ArrayRef> {
        self.call("seeds", (slot,), array_from_pyarrow)
    }

    fn apply_steps(
        &self,
        rows: Vec<RecordBatch>,
        steps: &[&Plan],
        roles: &Roles,
    ) -> PlanResult<Vec<RecordBatch>> {
        let handles: Vec<Slot> = steps.iter().filter_map(|s| s.step()).collect();
        Python::attach(|py| {
            let call = || -> PyResult<Vec<RecordBatch>> {
                let roles_dict = PyDict::new(py);
                for (role, column) in roles {
                    roles_dict.set_item(*role, column)?;
                }
                let rows = rows.to_pyarrow(py)?;
                let out = self
                    .host
                    .bind(py)
                    .call_method1("apply_steps", (rows, handles, roles_dict))?;
                batch_list(&out)
            };
            call().map_err(|e| host_err(py, e))
        })
    }
}

/// Execute a serialized plan (`ursa_plan::plan::Plan` as JSON) and return its
/// result as a list of pyarrow `RecordBatch`es. `host` resolves the plan's slots
/// (see [`PyInputs`]). The GIL is released for the whole execution and taken
/// back only for the host's callbacks.
#[pyfunction]
fn run_plan(py: Python<'_>, plan: &str, host: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let plan = Plan::from_json(plan).map_err(plan_err)?;
    let inputs = PyInputs { host };
    let batches = py
        .detach(|| execute_plan(&plan, &inputs))
        .map_err(plan_err)?;
    batches.to_pyarrow(py).map(|obj| obj.unbind())
}

/// The graph index of a serialized edge input (`ursa_plan::plan::EdgeInput`),
/// after the checks a graph op over it needs. Backs the eager whole-graph stats,
/// which run outside a plan.
#[pyfunction]
fn resolve_index(py: Python<'_>, edges: &str, host: Py<PyAny>) -> PyResult<GraphIndex> {
    let edges = EdgeInput::from_json(edges).map_err(plan_err)?;
    let inputs = PyInputs { host };
    let index = py
        .detach(|| require_index(&edges, &inputs))
        .map_err(plan_err)?;
    Ok(GraphIndex(index))
}

/// The set of nodes reached within `n` hops of `seeds` (seeds included), as a
/// pyarrow array of user ids. Backs graph ops over a `hop` result (#116): the
/// reached region induces the subgraph mask a node-valued kernel runs over.
#[pyfunction]
fn hop_reached_nodes_query(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    seeds: &Bound<'_, PyAny>,
    n: u32,
    direction: &str,
) -> PyResult<Py<PyAny>> {
    let (topo, ids) = (index.0.topo.clone(), index.0.ids.clone());
    let seeds = array_from_pyarrow(seeds)?;
    let direction = direction.to_string();
    let arr = py.detach(move || {
        hop_reached_nodes(topo, ids, seeds.as_ref(), n, &direction).map_err(to_pyerr)
    })?;
    arr.into_data().to_pyarrow(py).map(|obj| obj.unbind())
}

/// The nodes on the shortest path from `source` to `target` (inclusive), as a
/// pyarrow array of user ids (empty when there is no path). Backs graph ops over a
/// `shortest_path` result (#116). `weight` + `edges` select weighted Dijkstra.
#[pyfunction]
#[pyo3(signature = (index, source, target, direction, weight=None, edges=None))]
fn shortest_path_nodes_query(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    source: &Bound<'_, PyAny>,
    target: &Bound<'_, PyAny>,
    direction: &str,
    weight: Option<String>,
    edges: Option<Bound<'_, PyAny>>,
) -> PyResult<Py<PyAny>> {
    let (topo, ids) = (index.0.topo.clone(), index.0.ids.clone());
    let source = array_from_pyarrow(source)?;
    let target = array_from_pyarrow(target)?;
    let direction = direction.to_string();
    let edges = match edges {
        Some(obj) => Some(Vec::<RecordBatch>::from_pyarrow_bound(&obj)?),
        None => None,
    };
    let arr = py.detach(move || {
        shortest_path_nodes(
            topo,
            ids,
            source.as_ref(),
            target.as_ref(),
            &direction,
            weight.as_deref(),
            edges,
        )
        .map_err(to_pyerr)
    })?;
    arr.into_data().to_pyarrow(py).map(|obj| obj.unbind())
}

/// Whole-graph edge density (eager scalar). `direction` is out/in/both; `both`
/// is the undirected density.
#[pyfunction]
#[pyo3(signature = (index, direction="out"))]
fn graph_density(py: Python<'_>, index: PyRef<'_, GraphIndex>, direction: &str) -> PyResult<f64> {
    let dir = parse_stat_direction(direction).map_err(to_pyerr)?;
    let topo = index.0.topo.clone();
    py.detach(move || density(&topo, dir).map_err(to_pyerr))
}

/// Average shortest-path length over reachable ordered pairs (eager scalar).
/// `sample` (a fraction) estimates from a subset of sources; omit for exact.
#[pyfunction]
#[pyo3(signature = (index, sample=None, direction="out"))]
fn graph_avg_path_length(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    sample: Option<f64>,
    direction: &str,
) -> PyResult<f64> {
    let dir = parse_stat_direction(direction).map_err(to_pyerr)?;
    let topo = index.0.topo.clone();
    py.detach(move || avg_path_length(&topo, sample, dir).map_err(to_pyerr))
}

/// Graph diameter (eager scalar). `approximate` (default true) is a lower-bound
/// estimate; pass false for exact.
#[pyfunction]
#[pyo3(signature = (index, approximate, direction="out"))]
fn graph_diameter(
    py: Python<'_>,
    index: PyRef<'_, GraphIndex>,
    approximate: bool,
    direction: &str,
) -> PyResult<i64> {
    let dir = parse_stat_direction(direction).map_err(to_pyerr)?;
    let topo = index.0.topo.clone();
    py.detach(move || diameter(&topo, approximate, dir).map_err(to_pyerr))
}

/// Read a Parquet/CSV edge file's `src`/`dst` columns (plus any `weight_columns`
/// needed to evaluate a `weight=` expression, by their own names) through a DataFusion scan and hand
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

/// Read a Parquet/CSV edge file as a table for the relational paths (a plain
/// `collect()` and its tail, the sinks, a subgraph view's edge mask): every column
/// with its file name and type (`Utf8View` read as `Utf8`), endpoints validated but
/// not cast, so it matches the same file read into an in-memory `EdgeFrame`
/// (#160). `columns` is a projection pushdown: when non-empty only those columns
/// (plus `src`/`dst`) are read, in file order; empty reads all of them.
#[pyfunction]
#[pyo3(signature = (path, src, dst, storage_options=None, columns=Vec::new()))]
fn scan_edges_table_arrow(
    py: Python<'_>,
    path: &str,
    src: &str,
    dst: &str,
    storage_options: Option<HashMap<String, String>>,
    columns: Vec<String>,
) -> PyResult<Py<PyAny>> {
    let opts = storage_options.unwrap_or_default();
    let batches =
        py.detach(|| scan_edges_table(path, src, dst, &opts, &columns).map_err(to_pyerr))?;
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
    m.add_function(wrap_pyfunction!(scan_build_index, m)?)?;
    m.add_function(wrap_pyfunction!(_topology_build_count, m)?)?;
    // the one execution entry point
    m.add_function(wrap_pyfunction!(run_plan, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_index, m)?)?;
    m.add_function(wrap_pyfunction!(hop_reached_nodes_query, m)?)?;
    m.add_function(wrap_pyfunction!(shortest_path_nodes_query, m)?)?;
    m.add_function(wrap_pyfunction!(graph_density, m)?)?;
    m.add_function(wrap_pyfunction!(graph_avg_path_length, m)?)?;
    m.add_function(wrap_pyfunction!(graph_diameter, m)?)?;
    m.add_function(wrap_pyfunction!(scan_edges_arrow, m)?)?;
    m.add_function(wrap_pyfunction!(scan_edges_table_arrow, m)?)?;
    m.add_function(wrap_pyfunction!(scan_nodes_arrow, m)?)?;
    Ok(())
}
