//! The plan object a Python frame holds (#193): [`PlanNode`], a handle on an
//! `ursa_plan::lazy::Node`.
//!
//! Each frame method calls one `PlanNode` method, which builds the next node in
//! Rust, validates it there, and returns the new handle; the Python frame wraps
//! it. What Python converts before the call is the user's input: expressions
//! and step arguments to the plan IR (as Python values), tables to Arrow. When
//! that conversion fails, the error is passed in as `error` and raised where a
//! collect would have raised it.
//!
//! [`PyHost`] is the host a tree reads back through: the tables and arrays the
//! Python objects hold, and the few things done in Python (`ursa._execute`): a
//! seed iterable's conversion, the stdlib draw of a `sample`, warnings, and an
//! in-memory table's identity token.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError};

use arrow::array::{make_array, ArrayData, ArrayRef, RecordBatch};
use arrow::datatypes::{Schema, SchemaRef};
use arrow_pyarrow::{FromPyArrow, ToPyArrow};
use pyo3::exceptions::{PyNotImplementedError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::Value;

use ursa_plan::lazy::{
    CellValue, Column, ColumnExpr, Host, HostHandle, Kind, Node, Op, Seeds, State, Step,
};
use ursa_plan::plan::{AggSpec, Expr, IntArg, Rows, ScanPath, SelectItem, SortKey};
use ursa_plan::{PlanError, PlanResult};

use crate::{host_err, plan_err, GraphIndex, NodeTableCache};

// --- Python values to the plan IR -------------------------------------------------

/// A Python value (what the frame layer builds: dicts, lists, strings, numbers,
/// `None`) as JSON, the way `json.dumps` would write it: an `__index__` as an
/// integer, a `__float__` as a float, an integer past 64 bits as the float it
/// rounds to.
pub(crate) fn to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(Value::String(s.to_str()?.to_string()));
    }
    if obj.is_instance_of::<PyInt>() {
        return int_value(obj);
    }
    if let Ok(f) = obj.cast::<PyFloat>() {
        return float_value(f.value());
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        let mut m = serde_json::Map::new();
        for (k, v) in d.iter() {
            let k: String = k.extract()?;
            m.insert(k, to_value(&v)?);
        }
        return Ok(Value::Object(m));
    }
    if let Ok(l) = obj.cast::<PyList>() {
        return l
            .iter()
            .map(|v| to_value(&v))
            .collect::<PyResult<_>>()
            .map(Value::Array);
    }
    if let Ok(t) = obj.cast::<PyTuple>() {
        return t
            .iter()
            .map(|v| to_value(&v))
            .collect::<PyResult<_>>()
            .map(Value::Array);
    }
    if obj.hasattr("__index__")? {
        return int_value(&obj.call_method0("__index__")?);
    }
    if obj.hasattr("__float__")? {
        return float_value(obj.call_method0("__float__")?.extract()?);
    }
    let name = obj.get_type().name()?.to_string();
    Err(PyTypeError::new_err(format!(
        "Object of type {name} is not JSON serializable"
    )))
}

fn int_value(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if let Ok(i) = obj.extract::<i64>() {
        return Ok(Value::from(i));
    }
    if let Ok(u) = obj.extract::<u64>() {
        return Ok(Value::from(u));
    }
    let f: f64 = obj.call_method0("__float__")?.extract()?;
    float_value(f)
}

fn float_value(f: f64) -> PyResult<Value> {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .ok_or_else(|| {
            PyValueError::new_err(format!(
                "invalid plan: a plan argument is {f}, which has no JSON form"
            ))
        })
}

/// A Python value as a typed IR value (`serde` on its JSON).
fn ir<T: serde::de::DeserializeOwned>(obj: &Bound<'_, PyAny>) -> PyResult<T> {
    serde_json::from_value(to_value(obj)?)
        .map_err(|e| PyValueError::new_err(format!("invalid plan: {e}")))
}

fn handle(obj: &Bound<'_, PyAny>) -> HostHandle {
    Arc::new(obj.clone().unbind())
}

fn py_of<'py>(py: Python<'py>, h: &HostHandle) -> PyResult<Bound<'py, PyAny>> {
    h.downcast_ref::<Py<PyAny>>()
        .map(|o| o.bind(py).clone())
        .ok_or_else(|| PyTypeError::new_err("a host handle of another host"))
}

fn error_handle(error: Option<&Bound<'_, PyAny>>) -> Option<HostHandle> {
    error.filter(|e| !e.is_none()).map(handle)
}

/// A string argument of the plan, with the plan parser's error for a value of
/// another type.
fn string_arg(obj: &Bound<'_, PyAny>) -> PyResult<String> {
    ir::<String>(obj)
}

// --- the host --------------------------------------------------------------------

/// The Python side of a frame tree (see the module docs).
pub(crate) struct PyHost;

static GLUE: pyo3::sync::PyOnceLock<Py<PyModule>> = pyo3::sync::PyOnceLock::new();

impl PyHost {
    fn call<T>(
        &self,
        name: &str,
        args: impl for<'py> pyo3::call::PyCallArgs<'py>,
        convert: impl for<'py> FnOnce(&Bound<'py, PyAny>) -> PyResult<T>,
    ) -> PlanResult<T> {
        Python::attach(|py| {
            let out = GLUE
                .get_or_try_init(py, || py.import("ursa._execute").map(Bound::unbind))
                .and_then(|m| m.bind(py).call_method1(name, args));
            out.and_then(|obj| convert(&obj))
                .map_err(|e| host_err(py, e))
        })
    }

    fn attach<T>(&self, f: impl for<'py> FnOnce(Python<'py>) -> PyResult<T>) -> PlanResult<T> {
        Python::attach(|py| f(py).map_err(|e| host_err(py, e)))
    }
}

fn batch_list(obj: &Bound<'_, PyAny>) -> PyResult<Vec<RecordBatch>> {
    Vec::<RecordBatch>::from_pyarrow_bound(obj)
}

impl Host for PyHost {
    fn batches(&self, table: &HostHandle) -> PlanResult<Vec<RecordBatch>> {
        self.attach(|py| {
            let table = py_of(py, table)?;
            let batches = batch_list(&table.call_method0("to_batches")?)?;
            if !batches.is_empty() {
                return Ok(batches);
            }
            // An empty table keeps its schema (#153).
            let schema = Schema::from_pyarrow_bound(&table.getattr("schema")?)?;
            Ok(vec![RecordBatch::new_empty(Arc::new(schema))])
        })
    }

    fn edge_batches(&self, endpoints: &HostHandle, reversed: bool) -> PlanResult<Vec<RecordBatch>> {
        let endpoints = self.attach(|py| py_of(py, endpoints).map(Bound::unbind))?;
        self.call("_edge_batches", (endpoints, reversed), batch_list)
    }

    fn storage_options(&self, spec: &HostHandle) -> PlanResult<HashMap<String, String>> {
        self.attach(|py| {
            let spec = py_of(py, spec)?;
            let store = spec.get_item("store")?;
            if !store.is_none() {
                return Err(PyNotImplementedError::new_err(
                    "store= (a pre-configured obstore store) is not supported yet; pass \
                     storage_options={...} instead.",
                ));
            }
            let opts = spec.get_item("storage_options")?;
            // With the message a native scan's own argument conversion gave.
            opts.extract::<Option<HashMap<String, String>>>()
                .map(Option::unwrap_or_default)
                .map_err(|e| {
                    let value = e.value(py).to_string();
                    PyTypeError::new_err(format!("argument 'storage_options': {value}"))
                })
        })
    }

    fn seeds(&self, value: &HostHandle) -> PlanResult<ArrayRef> {
        let value = self.attach(|py| py_of(py, value).map(Bound::unbind))?;
        self.call("_seeds", (value,), |obj| {
            Ok(make_array(ArrayData::from_pyarrow_bound(obj)?))
        })
    }

    fn sample(&self, draw: &HostHandle, rows: usize) -> PlanResult<Option<Vec<usize>>> {
        let draw = self.attach(|py| py_of(py, draw).map(Bound::unbind))?;
        self.call("_sample", (draw, rows), |obj| {
            obj.extract::<Option<Vec<usize>>>()
        })
    }

    fn warn_dropped(&self, n: usize) -> PlanResult<()> {
        self.call("_warn_dropped", (n,), |_| Ok(()))
    }

    fn identity(&self, table: &HostHandle) -> PlanResult<Option<String>> {
        let table = self.attach(|py| py_of(py, table).map(Bound::unbind))?;
        self.call("_table_token", (table,), |obj| {
            obj.extract::<Option<String>>()
        })
    }

    fn error(&self, error: &HostHandle) -> PlanError {
        Python::attach(|py| match py_of(py, error) {
            Ok(e) => host_err(py, PyErr::from_value(e)),
            Err(e) => host_err(py, e),
        })
    }
}

// --- the plan object ------------------------------------------------------------

/// A frame's plan: a handle on its last step (`ursa_plan::lazy::Node`).
#[pyclass(frozen, module = "ursa._ursa")]
pub(crate) struct PlanNode(pub(crate) Arc<Node>);

fn wrap(node: PlanResult<Arc<Node>>) -> PyResult<PlanNode> {
    node.map(PlanNode).map_err(plan_err)
}

fn scan_path(path: &Bound<'_, PyAny>) -> Option<ScanPath> {
    if let Ok(p) = path.extract::<String>() {
        return Some(ScanPath::One(p));
    }
    if path.is_instance_of::<PyString>() {
        return None;
    }
    path.extract::<Vec<String>>().ok().map(ScanPath::Many)
}

fn seeds_of(obj: &Bound<'_, PyAny>) -> Seeds {
    match obj.cast::<PlanNode>() {
        Ok(n) => Seeds::Frame(n.get().0.clone()),
        Err(_) => Seeds::Value(handle(obj)),
    }
}

impl PlanNode {
    fn step(&self, step: Step, error: Option<&Bound<'_, PyAny>>) -> PyResult<PlanNode> {
        wrap(self.0.step(step, error_handle(error), &PyHost))
    }
}

#[pymethods]
impl PlanNode {
    /// An in-memory edge frame: the edge table, its schema, and its canonical
    /// `(src, dst)` endpoint arrays.
    #[staticmethod]
    fn edge_table(
        table: &Bound<'_, PyAny>,
        endpoints: &Bound<'_, PyAny>,
        src: &str,
        dst: &str,
    ) -> PyResult<PlanNode> {
        let schema: SchemaRef = Arc::new(Schema::from_pyarrow_bound(&table.getattr("schema")?)?);
        wrap(Node::edge_table(
            handle(table),
            schema,
            handle(endpoints),
            src,
            dst,
            &PyHost,
        ))
    }

    /// An in-memory node (attribute) frame.
    #[staticmethod]
    fn node_table(table: &Bound<'_, PyAny>, id: &str) -> PyResult<PlanNode> {
        let schema: SchemaRef = Arc::new(Schema::from_pyarrow_bound(&table.getattr("schema")?)?);
        wrap(Node::node_table(handle(table), schema, id, &PyHost))
    }

    /// `ur.scan_edges`: `spec` is `{"storage_options": ..., "store": ...}`.
    #[staticmethod]
    fn scan_edges(
        path: &Bound<'_, PyAny>,
        src: &str,
        dst: &str,
        on_null: &str,
        spec: &Bound<'_, PyAny>,
    ) -> PyResult<PlanNode> {
        wrap(Node::scan_edges(
            scan_path(path),
            src,
            dst,
            on_null,
            handle(spec),
            &PyHost,
        ))
    }

    /// `ur.scan_nodes`.
    #[staticmethod]
    fn scan_nodes(
        path: &Bound<'_, PyAny>,
        id: &str,
        spec: &Bound<'_, PyAny>,
    ) -> PyResult<PlanNode> {
        wrap(Node::scan_nodes(scan_path(path), id, handle(spec), &PyHost))
    }

    #[pyo3(signature = (predicate, error=None))]
    fn filter(
        &self,
        predicate: &Bound<'_, PyAny>,
        error: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PlanNode> {
        let p = if error.is_some_and(|e| !e.is_none()) {
            Expr::default()
        } else {
            let v = to_value(predicate)?;
            Expr::from_value(v).map_err(|e| PyValueError::new_err(format!("invalid plan: {e}")))?
        };
        self.step(Step::Filter(p), error)
    }

    #[pyo3(signature = (items, error=None))]
    fn select(
        &self,
        items: &Bound<'_, PyAny>,
        error: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PlanNode> {
        let items: Vec<SelectItem> = if error.is_some_and(|e| !e.is_none()) {
            vec![]
        } else {
            ir(items)?
        };
        self.step(Step::Select(items), error)
    }

    /// `with_columns`: `(name, expression IR, edge frame or None)` per column;
    /// a graph verb's IR leaves its edges out.
    fn with_columns(
        &self,
        columns: Vec<(String, Bound<'_, PyAny>, Option<PyRef<'_, PlanNode>>)>,
    ) -> PyResult<PlanNode> {
        let mut out = Vec::with_capacity(columns.len());
        for (name, expr, edges) in columns {
            let v = to_value(&expr)?;
            let expr = match Expr::from_value(v)
                .map_err(|e| PyValueError::new_err(format!("invalid plan: {e}")))?
            {
                Expr::Graph(verb) => ColumnExpr::Graph {
                    verb,
                    edges: edges.map(|e| e.0.clone()),
                },
                Expr::Scalar(v) => ColumnExpr::Scalar(v),
            };
            out.push(Column { name, expr });
        }
        self.step(Step::WithColumns(out), None)
    }

    #[pyo3(signature = (mapping, error=None))]
    fn rename(
        &self,
        mapping: &Bound<'_, PyAny>,
        error: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PlanNode> {
        let mapping: Vec<(String, String)> = if error.is_some_and(|e| !e.is_none()) {
            vec![]
        } else {
            ir(mapping)?
        };
        self.step(Step::Rename(mapping), error)
    }

    #[pyo3(signature = (by, error=None))]
    fn sort(&self, by: &Bound<'_, PyAny>, error: Option<&Bound<'_, PyAny>>) -> PyResult<PlanNode> {
        let by: Vec<SortKey> = if error.is_some_and(|e| !e.is_none()) {
            vec![]
        } else {
            ir(by)?
        };
        self.step(Step::Sort(by), error)
    }

    #[pyo3(signature = (n, error=None))]
    fn head(&self, n: &Bound<'_, PyAny>, error: Option<&Bound<'_, PyAny>>) -> PyResult<PlanNode> {
        let n = if error.is_some_and(|e| !e.is_none()) {
            IntArg::default()
        } else {
            IntArg(to_value(n)?)
        };
        self.step(Step::Head(n), error)
    }

    /// `sample`: `n` and `seed` as the plan holds them, and `draw` (the host's
    /// own `(n, seed)`) the rows are drawn with.
    #[pyo3(signature = (n, seed, draw, error=None))]
    fn sample(
        &self,
        n: &Bound<'_, PyAny>,
        seed: &Bound<'_, PyAny>,
        draw: &Bound<'_, PyAny>,
        error: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PlanNode> {
        let invalid = error.is_some_and(|e| !e.is_none());
        let (n, seed) = if invalid {
            (IntArg::default(), None)
        } else {
            (IntArg(to_value(n)?), Some(IntArg(to_value(seed)?)))
        };
        self.step(
            Step::Sample {
                n,
                seed,
                draw: handle(draw),
            },
            error,
        )
    }

    fn distinct(&self) -> PyResult<PlanNode> {
        self.step(Step::Distinct, None)
    }

    #[pyo3(signature = (right, on, how, error=None))]
    fn join(
        &self,
        right: PyRef<'_, PlanNode>,
        on: &Bound<'_, PyAny>,
        how: &Bound<'_, PyAny>,
        error: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PlanNode> {
        let on: Vec<String> = if error.is_some_and(|e| !e.is_none()) {
            vec![]
        } else {
            ir(on)?
        };
        let how = string_arg(how)?;
        self.step(
            Step::Join {
                right: right.0.clone(),
                on,
                how,
            },
            error,
        )
    }

    #[pyo3(signature = (keys, aggs, error=None))]
    fn group_by_agg(
        &self,
        keys: &Bound<'_, PyAny>,
        aggs: &Bound<'_, PyAny>,
        error: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PlanNode> {
        let invalid = error.is_some_and(|e| !e.is_none());
        let (keys, aggs): (Vec<String>, Vec<AggSpec>) = if invalid {
            (vec![], vec![])
        } else {
            (ir(keys)?, ir(aggs)?)
        };
        self.step(Step::GroupByAgg { keys, aggs }, error)
    }

    fn nodes(&self) -> PyResult<PlanNode> {
        wrap(self.0.nodes(&PyHost))
    }

    fn reverse(&self) -> PyResult<PlanNode> {
        wrap(self.0.reverse(&PyHost))
    }

    #[staticmethod]
    fn hop(
        edges: PyRef<'_, PlanNode>,
        n: &Bound<'_, PyAny>,
        direction: &Bound<'_, PyAny>,
        seeds: &Bound<'_, PyAny>,
    ) -> PyResult<PlanNode> {
        wrap(Node::graph(
            Op::Hop {
                edges: edges.0.clone(),
                n: IntArg(to_value(n)?),
                direction: string_arg(direction)?,
                seeds: seeds_of(seeds),
            },
            &PyHost,
        ))
    }

    #[staticmethod]
    fn shortest_path(
        edges: PyRef<'_, PlanNode>,
        source: &Bound<'_, PyAny>,
        target: &Bound<'_, PyAny>,
        weight: &Bound<'_, PyAny>,
        direction: &Bound<'_, PyAny>,
    ) -> PyResult<PlanNode> {
        let weight = if weight.is_none() {
            None
        } else {
            Some(to_value(weight)?)
        };
        wrap(Node::graph(
            Op::ShortestPath {
                edges: edges.0.clone(),
                source: Seeds::Value(handle(source)),
                target: Seeds::Value(handle(target)),
                weight,
                direction: string_arg(direction)?,
            },
            &PyHost,
        ))
    }

    #[staticmethod]
    fn random_walk(
        edges: PyRef<'_, PlanNode>,
        start: &Bound<'_, PyAny>,
        steps: &Bound<'_, PyAny>,
        walks_per_node: &Bound<'_, PyAny>,
        seed: &Bound<'_, PyAny>,
    ) -> PyResult<PlanNode> {
        wrap(Node::graph(
            Op::RandomWalk {
                edges: edges.0.clone(),
                start: seeds_of(start),
                steps: IntArg(to_value(steps)?),
                walks_per_node: IntArg(to_value(walks_per_node)?),
                seed: Some(IntArg(to_value(seed)?)),
            },
            &PyHost,
        ))
    }

    #[staticmethod]
    fn describe(
        edges: PyRef<'_, PlanNode>,
        full: bool,
        direction: &Bound<'_, PyAny>,
    ) -> PyResult<PlanNode> {
        wrap(Node::graph(
            Op::Describe {
                edges: edges.0.clone(),
                full,
                direction: string_arg(direction)?,
            },
            &PyHost,
        ))
    }

    /// Run the plan: a list of pyarrow `RecordBatch`es, or the `pyarrow.Table`
    /// a plain in-memory frame already is.
    fn collect(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let node = self.0.clone();
        let rows = py
            .detach(|| ursa_plan::lazy::execute(&node, &PyHost))
            .map_err(plan_err)?;
        match rows {
            Rows::Host(h) => match ursa_plan::lazy::host_rows(h) {
                Some(table) => py_of(py, &table).map(Bound::unbind),
                None => Err(PyTypeError::new_err("host rows of another host")),
            },
            Rows::Batches(b) => b.to_pyarrow(py).map(Bound::unbind),
        }
    }

    /// The schema `collect()` returns, as a `pyarrow.Schema`.
    fn schema(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let node = self.0.clone();
        let schema = py
            .detach(|| ursa_plan::lazy::schema(&node, &PyHost))
            .map_err(plan_err)?;
        schema.as_ref().to_pyarrow(py).map(Bound::unbind)
    }

    #[pyo3(signature = (physical=false))]
    fn explain(&self, py: Python<'_>, physical: bool) -> String {
        let node = self.0.clone();
        py.detach(|| ursa_plan::lazy::explain(&node, &PyHost, physical))
    }

    /// The checked graph index of this edge frame (for the eager stats).
    fn graph_index(&self, py: Python<'_>) -> PyResult<GraphIndex> {
        let node = self.0.clone();
        py.detach(|| ursa_plan::lazy::graph_index(&node, &PyHost))
            .map(GraphIndex)
            .map_err(plan_err)
    }

    /// The plan as the conformance fixtures hold it: its JSON, and per slot the
    /// object it reads (a frame's source table, a seed value, or None).
    fn lowered(&self, py: Python<'_>) -> PyResult<(String, Vec<Py<PyAny>>)> {
        let mut slots: Vec<Option<HostHandle>> = vec![];
        let json = ursa_plan::lazy::lowered_json(&self.0, |s| {
            slots.push(match s {
                ursa_plan::lazy::SlotSource::Frame(h) | ursa_plan::lazy::SlotSource::Seeds(h) => {
                    h.cloned()
                }
            })
        });
        let objects = slots
            .iter()
            .map(|h| match h {
                Some(h) => py_of(py, h).map(Bound::unbind),
                None => Ok(py.None()),
            })
            .collect::<PyResult<_>>()?;
        Ok((json, objects))
    }

    /// The number of steps in the frame's own chain.
    #[getter]
    fn n_steps(&self) -> usize {
        self.0.chain().len()
    }

    /// The step names of the frame's own chain, in order.
    fn ops(&self) -> Vec<&'static str> {
        self.0.chain().iter().map(|n| n.op_name()).collect()
    }

    #[getter]
    fn src_col(&self) -> Option<String> {
        match self.0.kind() {
            Kind::Edges { src, .. } => Some(src.clone()),
            Kind::Nodes { .. } => None,
        }
    }

    #[getter]
    fn dst_col(&self) -> Option<String> {
        match self.0.kind() {
            Kind::Edges { dst, .. } => Some(dst.clone()),
            Kind::Nodes { .. } => None,
        }
    }

    #[getter]
    fn id_col(&self) -> Option<String> {
        self.0.id_col().map(str::to_string)
    }

    /// The frame's index cell, shared with the derivations that keep its rows
    /// (one object per cell).
    #[getter]
    fn index_cell(&self, py: Python<'_>) -> PyResult<Py<IndexCell>> {
        // The handle is read and set under the state's lock, and Python runs
        // only outside it (a weakref call or an allocation can run the garbage
        // collector, which can release the GIL to another thread).
        let state = self.0.state();
        let slot = || {
            state
                .host_cell
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        loop {
            let seen = slot().clone();
            if let Some(weak) = seen.as_ref().and_then(|h| h.downcast_ref::<Py<PyAny>>()) {
                let live = weak.bind(py).call0()?;
                if !live.is_none() {
                    return Ok(live.cast_into::<IndexCell>()?.unbind());
                }
            }
            let cell = Py::new(py, IndexCell(state.clone()))?;
            let weakref: HostHandle = Arc::new(
                py.import("weakref")?
                    .getattr("ref")?
                    .call1((cell.clone_ref(py),))?
                    .unbind(),
            );
            let mut current = slot();
            let unchanged = match (current.as_ref(), seen.as_ref()) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if unchanged {
                let old = current.replace(weakref);
                drop(current);
                drop(old);
                return Ok(cell);
            }
            // Another thread set one meanwhile: use that one.
        }
    }

    /// A `scan_nodes` frame's scanned-table cache, shared with its derivations
    /// (`None` before its first read; one object per cache).
    #[getter]
    fn node_cache(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let state = self.0.state();
        let Some(cache) = state.node_cache.get() else {
            return Ok(None);
        };
        let obj = match state.host_cache.get() {
            Some(h) => h.clone(),
            None => {
                let obj = Py::new(py, NodeTableCache(cache.clone()))?.into_any();
                state.host_cache.get_or_init(|| Arc::new(obj)).clone()
            }
        };
        py_of(py, &obj).map(Bound::unbind).map(Some)
    }

    /// This frame's ids as a seed set, once it had to run to get them (one
    /// object for them).
    #[getter]
    fn cached_seeds(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let Some(ids) = self.0.cached_seeds() else {
            return Ok(None);
        };
        let obj = match self.0.seeds_host.get() {
            Some(h) => h.clone(),
            None => {
                let obj = ids.to_data().to_pyarrow(py)?.unbind();
                self.0.seeds_host.get_or_init(|| Arc::new(obj)).clone()
            }
        };
        py_of(py, &obj).map(Bound::unbind).map(Some)
    }

    /// The host objects of the frame's source: `(table, endpoints, scan spec)`.
    #[getter]
    fn source(&self, py: Python<'_>) -> PyResult<(Py<PyAny>, Py<PyAny>, Py<PyAny>)> {
        let (table, endpoints, spec) = self.0.source_handles();
        let get = |h: Option<&HostHandle>| -> PyResult<Py<PyAny>> {
            match h {
                Some(h) => py_of(py, h).map(Bound::unbind),
                None => Ok(py.None()),
            }
        };
        Ok((get(table)?, get(endpoints)?, get(spec)?))
    }
}

/// A frame's index cell (`frame._index_build_cell`): its built `GraphIndex` in
/// `value` (`None` before the first build), and a `lock` for code that sets it.
#[pyclass(frozen, weakref, module = "ursa._ursa")]
pub(crate) struct IndexCell(Arc<State>);

#[pymethods]
impl IndexCell {
    #[getter]
    fn value(&self, py: Python<'_>) -> PyResult<Option<Py<GraphIndex>>> {
        let Some(CellValue { index, host }) = self.0.index.value() else {
            return Ok(None);
        };
        if let Some(obj) = host.as_ref().and_then(|h| h.downcast_ref::<Py<PyAny>>()) {
            return Ok(Some(
                obj.bind(py).clone().cast_into::<GraphIndex>()?.unbind(),
            ));
        }
        let obj = Py::new(py, GraphIndex(index.clone()))?;
        self.0
            .index
            .set_host(&index, Arc::new(obj.clone_ref(py).into_any()));
        Ok(Some(obj))
    }

    #[setter]
    fn set_value(&self, value: Option<Bound<'_, GraphIndex>>) {
        self.0.index.set_value(value.map(|v| CellValue {
            index: v.get().0.clone(),
            host: Some(Arc::new(v.unbind().into_any())),
        }));
    }

    /// A lock for code that sets `value` (one per cell).
    #[getter]
    fn lock(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        // Made outside the cell's initializer, which holds a lock Python must not
        // run under; a lock made by a thread that loses the race is dropped.
        let lock = match self.0.host_lock.get() {
            Some(l) => l,
            None => {
                let l: HostHandle =
                    Arc::new(py.import("threading")?.getattr("Lock")?.call0()?.unbind());
                self.0.host_lock.get_or_init(|| l)
            }
        };
        Ok(lock
            .downcast_ref::<Py<PyAny>>()
            .expect("a Python lock")
            .clone_ref(py))
    }
}
