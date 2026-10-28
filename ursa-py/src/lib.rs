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
//! - `NodeTableCache` — the scanned table of a `scan_nodes` frame, kept on the
//!   frame and read through by the plan.
//! - `graph_density` / `graph_avg_path_length` / `graph_diameter` — eager
//!   whole-graph statistics.

use std::collections::HashMap;

use arrow::array::{make_array, Array, ArrayData, ArrayRef, RecordBatch};
use arrow_pyarrow::{FromPyArrow, ToPyArrow};
use pyo3::create_exception;
use pyo3::exceptions::{
    PyException, PyNotImplementedError, PyOverflowError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;

use std::sync::Arc;

use ursa_core::topology::Topology;
use ursa_core::IdMap;

use ursa_plan::plan::{
    count_topology_build, execute, topology_builds, EdgeInput, HostError, HostRows, Rows, Slot,
};
use ursa_plan::{
    avg_path_length, build_topology_batches, density, diameter, parse_stat_direction,
    require_index, scan_edges_topology, weight_cache_key, Inputs, Plan, PlanError, PlanResult,
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
/// `ColumnNotFoundError`: a query referenced a column that does not exist in
/// the frame. It subclasses both `UrsaError` and `ValueError` (so `except
/// ValueError` written for the errors plain frames raised keeps working), which
/// `create_exception!` can't express: the class is built once, with `type()`.
static COLUMN_NOT_FOUND: pyo3::sync::PyOnceLock<Py<pyo3::types::PyType>> =
    pyo3::sync::PyOnceLock::new();

fn column_not_found_type(py: Python<'_>) -> PyResult<&Bound<'_, pyo3::types::PyType>> {
    let ty = COLUMN_NOT_FOUND.get_or_try_init(py, || -> PyResult<_> {
        let bases = (
            py.get_type::<UrsaError>(),
            py.get_type::<pyo3::exceptions::PyValueError>(),
        );
        let ns = pyo3::types::PyDict::new(py);
        ns.set_item("__module__", "ursa._ursa")?;
        ns.set_item(
            "__doc__",
            "A query referenced a column that does not exist in the frame.",
        )?;
        let ty = py
            .get_type::<pyo3::types::PyType>()
            .call1(("ColumnNotFoundError", bases, ns))?;
        Ok(ty.cast_into::<pyo3::types::PyType>()?.unbind())
    })?;
    Ok(ty.bind(py))
}

/// A `ColumnNotFoundError` carrying `msg`.
struct ColumnNotFoundError;

impl ColumnNotFoundError {
    fn new_err(msg: impl Into<String>) -> PyErr {
        let msg = msg.into();
        Python::attach(|py| match column_not_found_type(py) {
            Ok(ty) => PyErr::from_type(ty.clone(), msg),
            Err(e) => e,
        })
    }
}
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

    /// Subgraph-view masks computed over this index (test/observability hook).
    fn _mask_build_count(&self) -> usize {
        self.0.masks.builds()
    }
}

/// The scanned table of one `scan_nodes` frame (`ursa_plan::scan::NodeTableCache`):
/// one superset of the columns read so far, kept on the frame and shared by its
/// derivations, so repeated collects read the file once. Opaque to Python.
#[pyclass]
#[derive(Default)]
struct NodeTableCache(Arc<ursa_plan::scan::NodeTableCache>);

#[pymethods]
impl NodeTableCache {
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Scans this cache has run (test/observability hook).
    fn _scan_count(&self) -> usize {
        self.0.scans()
    }

    /// The columns it holds: `None` before the first read, `[]` for every column
    /// (test/observability hook).
    fn _held(&self) -> Option<Vec<String>> {
        self.0.held().map(Option::unwrap_or_default)
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
    count_topology_build();
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
    count_topology_build();
    let ScannedTopology { topo, ids, dropped } = scanned;
    Ok((GraphIndex::new(topo, ids), dropped))
}

/// Total topology builds so far (test/observability hook for the index contract).
#[pyfunction]
fn _topology_build_count() -> usize {
    topology_builds()
}

/// Turn the native scan log on (clearing it) or off, returning what was recorded:
/// `(kind, columns, filters pushed)` per scan (test/observability hook).
#[pyfunction]
fn _scan_log(record: bool) -> Vec<(String, Vec<String>, usize)> {
    ursa_plan::scan::scan_log(record)
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
        PlanError::Type(m) => PyTypeError::new_err(m),
        PlanError::Overflow(m) => PyOverflowError::new_err(m),
        PlanError::Wrapped { message, cause } => Python::attach(|py| {
            let err = PyNotImplementedError::new_err(message);
            err.set_cause(py, Some(plan_err(*cause)));
            err
        }),
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

    fn built_index(&self, frame: Slot) -> PlanResult<Option<ursa_plan::GraphIndex>> {
        self.call("built_index", (frame,), |obj| {
            if obj.is_none() {
                return Ok(None);
            }
            Ok(Some(obj.extract::<PyRef<'_, GraphIndex>>()?.0.clone()))
        })
    }

    fn put_index(
        &self,
        frame: Slot,
        index: ursa_plan::GraphIndex,
    ) -> PlanResult<ursa_plan::GraphIndex> {
        self.call("put_index", (frame, GraphIndex(index)), |obj| {
            Ok(obj.extract::<PyRef<'_, GraphIndex>>()?.0.clone())
        })
    }

    fn table(&self, frame: Slot) -> PlanResult<Option<Rows>> {
        self.call("table", (frame,), |obj| {
            if obj.is_none() {
                return Ok(None);
            }
            host_rows(obj).map(Some)
        })
    }

    fn storage_options(&self, frame: Slot) -> PlanResult<HashMap<String, String>> {
        self.call("storage_options", (frame,), |obj| {
            // With the message a native scan's own argument conversion gave.
            obj.extract::<Option<HashMap<String, String>>>()
                .map(Option::unwrap_or_default)
                .map_err(|e| {
                    let value = e.value(obj.py()).to_string();
                    PyTypeError::new_err(format!("argument 'storage_options': {value}"))
                })
        })
    }

    fn node_cache(&self, frame: Slot) -> PlanResult<Arc<ursa_plan::scan::NodeTableCache>> {
        self.call("node_cache", (frame,), |obj| {
            Ok(obj.extract::<PyRef<'_, NodeTableCache>>()?.0.clone())
        })
    }

    fn seeds(&self, slot: Slot) -> PlanResult<ArrayRef> {
        self.call("seeds", (slot,), array_from_pyarrow)
    }

    fn sample(&self, step: Slot, rows: usize) -> PlanResult<Option<Vec<usize>>> {
        self.call("sample", (step, rows), |obj| {
            obj.extract::<Option<Vec<usize>>>()
        })
    }

    fn batches(&self, rows: HostRows) -> PlanResult<Vec<RecordBatch>> {
        let table = py_rows(rows);
        self.call("batches", (table,), batch_list)
    }

    fn step_error(&self, step: Slot) -> PlanError {
        // The host raises the error it stored for the step.
        match self.call("step_error", (step,), |_| Ok(())) {
            Err(e) => e,
            Ok(()) => PlanError::Invalid(format!("step {step} has no stored error")),
        }
    }

    fn warn_dropped(&self, n: usize) -> PlanResult<()> {
        self.call("warn_dropped", (n,), |_| Ok(()))
    }

    fn identity(&self, frame: Slot) -> PlanResult<Option<String>> {
        self.call("identity", (frame,), |obj| obj.extract::<Option<String>>())
    }
}

/// A host table (a `pyarrow.Table`) the executor holds without converting it.
fn host_rows(obj: &Bound<'_, PyAny>) -> PyResult<Rows> {
    Ok(Rows::Host(Box::new(obj.clone().unbind())))
}

fn py_rows(rows: HostRows) -> Py<PyAny> {
    *rows
        .downcast::<Py<PyAny>>()
        .expect("host rows come from this host")
}

/// Rows as the Python host takes them: its own table, or a batch list.
fn rows_to_python(py: Python<'_>, rows: Rows) -> PyResult<Py<PyAny>> {
    match rows {
        Rows::Host(h) => Ok(py_rows(h)),
        Rows::Batches(b) => b.to_pyarrow(py).map(Bound::unbind),
    }
}

/// Execute a serialized plan (`ursa_plan::plan::Plan` as JSON) and return its
/// result: a list of pyarrow `RecordBatch`es from the engine, or the
/// `pyarrow.Table` the host finished it with. `host` resolves the plan's slots
/// (see [`PyInputs`]). The GIL is released for the whole execution and taken
/// back only for the host's callbacks.
#[pyfunction]
fn run_plan(py: Python<'_>, plan: &str, host: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let plan = Plan::from_json(plan).map_err(plan_err)?;
    let inputs = PyInputs { host };
    let rows = py.detach(|| execute(&plan, &inputs)).map_err(plan_err)?;
    rows_to_python(py, rows)
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
    m.add("ColumnNotFoundError", column_not_found_type(m.py())?)?;
    m.add("ComputeError", m.py().get_type::<ComputeError>())?;
    m.add_function(wrap_pyfunction!(__core_version, m)?)?;
    // the cached graph index (built once per frame)
    m.add_class::<GraphIndex>()?;
    m.add_function(wrap_pyfunction!(build_index, m)?)?;
    m.add_function(wrap_pyfunction!(scan_build_index, m)?)?;
    m.add_function(wrap_pyfunction!(_topology_build_count, m)?)?;
    m.add_function(wrap_pyfunction!(_scan_log, m)?)?;
    m.add_class::<NodeTableCache>()?;
    // the one execution entry point
    m.add_function(wrap_pyfunction!(run_plan, m)?)?;
    m.add_function(wrap_pyfunction!(resolve_index, m)?)?;
    m.add_function(wrap_pyfunction!(graph_density, m)?)?;
    m.add_function(wrap_pyfunction!(graph_avg_path_length, m)?)?;
    m.add_function(wrap_pyfunction!(graph_diameter, m)?)?;
    Ok(())
}
