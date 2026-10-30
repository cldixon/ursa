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
//! - `PlanNode` — the plan object a Python frame holds (`ursa_plan::lazy`):
//!   one method per frame step, each building and validating the next node in
//!   Rust; `collect`, `schema`, `explain` and the frame's caches (`IndexCell`,
//!   `NodeTableCache`) hang off it. See [`frame`].
//! - `check_agg_fn` / `check_direction` / `check_on_null` — the engine's lists,
//!   for the arguments the Python API checks as it is called.
//! - `build_index` — build a CSR topology from edge batches (a test hook; frames
//!   build their own index on first use).

mod frame;

use arrow::array::{Array, RecordBatch};
use arrow_pyarrow::FromPyArrow;
use pyo3::create_exception;
use pyo3::exceptions::{
    PyException, PyNotImplementedError, PyOverflowError, PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;

use std::sync::Arc;

use ursa_core::topology::Topology;
use ursa_core::IdMap;

use ursa_plan::plan::{count_topology_build, topology_builds, HostError};
use ursa_plan::{build_topology_batches, weight_cache_key, PlanError};

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

/// Map an engine error (any `Display` error the plan layer surfaces, such as a
/// stat's) to a typed, catchable exception in the Ursa hierarchy, in Ursa's words
/// with the original text as its `__cause__`. A missing-column error becomes
/// [`ColumnNotFoundError`]; everything else becomes [`ComputeError`]. Both
/// subclass `UrsaError`. A `DataFusionError` goes through [`engine_err`], which
/// reads its structure instead.
fn to_pyerr<E: std::fmt::Display>(e: E) -> PyErr {
    let raw = e.to_string();
    let class = if ursa_plan::message::says_column_not_found(&raw) {
        column_not_found
    } else {
        compute
    };
    with_cause(class(ursa_plan::message::reword(&raw)), raw)
}

fn column_not_found(m: String) -> PyErr {
    ColumnNotFoundError::new_err(m)
}

fn compute(m: String) -> PyErr {
    ComputeError::new_err(m)
}

/// `err`, with the engine's own text `raw` as its `__cause__` (a `RuntimeError`)
/// when its message was reworded from it.
fn with_cause(err: PyErr, raw: String) -> PyErr {
    Python::attach(|py| {
        if err.value(py).to_string() != raw {
            err.set_cause(py, Some(PyRuntimeError::new_err(raw)));
        }
    });
    err
}

/// A DataFusion error as the Ursa exception [`to_pyerr`] gives, in Ursa's words
/// ([`ursa_plan::message`]), classified by its structure
/// ([`ursa_plan::message::is_column_not_found`]), with DataFusion's own text as
/// its `__cause__`.
fn engine_err(e: &ursa_plan::message::DataFusionError) -> PyErr {
    let class = if ursa_plan::message::is_column_not_found(e) {
        column_not_found
    } else {
        compute
    };
    with_cause(class(ursa_plan::message::engine_message(e)), e.to_string())
}

// ---------------------------------------------------------------------------
// Real execution path: pyarrow in -> one DataFusion plan -> pyarrow out.
// ---------------------------------------------------------------------------

/// A built graph index — the CSR `Topology` plus the `IdMap` — cached on the
/// Python `EdgeFrame` and passed to every graph op over that frame, so the
/// topology is built once instead of on every `collect()` (spec §index-
/// preservation contract). Opaque to Python; produced only by [`build_index`].
///
/// It also carries the weight vectors evaluated over the frame's edge rows, so a
/// repeated weighted query evaluates its `weight=` expression once (#175).
#[pyclass(frozen)]
pub(crate) struct GraphIndex(pub(crate) ursa_plan::GraphIndex);

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
pub(crate) struct NodeTableCache(pub(crate) Arc<ursa_plan::scan::NodeTableCache>);

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
pub(crate) fn plan_err(e: PlanError) -> PyErr {
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
        PlanError::Engine(e) => engine_err(&e),
        PlanError::Host(e) => match e.payload.downcast::<PyErr>() {
            Ok(err) => *err,
            Err(_) => ComputeError::new_err(e.message),
        },
    }
}

/// Carry an exception the host raised through the plan executor unchanged.
pub(crate) fn host_err(py: Python<'_>, err: PyErr) -> PlanError {
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

/// Raise the error for an aggregation the engine does not compute.
#[pyfunction]
fn check_agg_fn(func: &str) -> PyResult<()> {
    ursa_plan::plan::check_agg_fn(func).map_err(plan_err)
}

/// Raise the `ValueError` for a `direction=` the verb or stat `verb` does not
/// take (a verb that checks its direction in the engine takes any here).
#[pyfunction]
fn check_direction(verb: &str, direction: &Bound<'_, PyAny>) -> PyResult<()> {
    let Some((allowed, stat)) = ursa_plan::plan::verb_directions(verb) else {
        return Ok(());
    };
    let given = direction.extract::<String>().ok();
    if given.as_deref().is_some_and(|d| allowed.contains(&d)) {
        return Ok(());
    }
    let shown = direction.repr()?.to_string();
    let msg = if stat {
        format!("{verb}(direction={shown}) is not supported; use 'out', 'in', or 'both'.")
    } else {
        let options: Vec<String> = allowed.iter().map(|d| format!("'{d}'")).collect();
        let hint = if given.as_deref() == Some("in") {
            " ('in' is not supported here; to run it on the reversed graph, swap the src/dst \
             roles when you build the frame)"
        } else {
            ""
        };
        format!(
            "{verb}(direction={shown}) is not supported; use one of {}{hint}.",
            options.join(", ")
        )
    };
    Err(PyValueError::new_err(msg))
}

/// Raise the `ValueError` for an `on_null=` mode an edge source does not have.
#[pyfunction]
fn check_on_null(on_null: &Bound<'_, PyAny>) -> PyResult<()> {
    let modes = ursa_plan::plan::ON_NULL_MODES;
    if on_null
        .extract::<String>()
        .is_ok_and(|m| modes.contains(&m.as_str()))
    {
        return Ok(());
    }
    let options: Vec<String> = modes.iter().map(|m| format!("'{m}'")).collect();
    Err(PyValueError::new_err(format!(
        "on_null must be one of ({}); got {}.",
        options.join(", "),
        on_null.repr()?
    )))
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
    m.add_function(wrap_pyfunction!(_topology_build_count, m)?)?;
    m.add_function(wrap_pyfunction!(_scan_log, m)?)?;
    m.add_class::<NodeTableCache>()?;
    // the plan object a frame holds
    m.add_class::<frame::PlanNode>()?;
    m.add_class::<frame::IndexCell>()?;
    m.add_function(wrap_pyfunction!(check_agg_fn, m)?)?;
    m.add_function(wrap_pyfunction!(check_direction, m)?)?;
    m.add_function(wrap_pyfunction!(check_on_null, m)?)?;
    Ok(())
}
