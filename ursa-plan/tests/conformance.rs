//! Plan conformance: JSON plan fixtures run through [`execute_plan`] over small
//! bundled graphs, with their expected output.
//!
//! Each file in `tests/plans/` is one case:
//!
//! ```json
//! {
//!   "description": "...",
//!   "inputs": [{"edges": "karate"}, {"nodes": "karate"}, {"ids": [0, 33]}],
//!   "plan": { ...the plan IR... },
//!   "expect": {"columns": [...], "rows": [[...], ...]}
//! }
//! ```
//!
//! `inputs` binds the plan's slots, by position: an edge frame or node table over
//! a dataset in `tests/data/`, a seed id set, or `{"unused": true}`. A case may instead expect an
//! error: `"expect": {"error": "not_implemented", "message": "..."}`.
//!
//! The fixtures were serialized from Python frames and their expected output
//! recorded from the Python package (see `tests/plans/gen_fixtures.py`), so they
//! pin what `collect()` returns today. Any front end that builds these plans has to
//! produce the same rows.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array, RecordBatch,
    StringArray, UInt32Array, UInt64Array,
};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, Schema};
use serde_json::{json, Value};
use ursa_core::EdgeMask;
use ursa_plan::plan::{HostRows, Roles, Rows, Slot};
use ursa_plan::{build_topology_batches, execute_plan, GraphIndex, Inputs, Plan, PlanError};

type Result<T> = ursa_plan::PlanResult<T>;

/// One fixture slot.
enum Input {
    Edges(Vec<RecordBatch>, OnceCell<GraphIndex>),
    Nodes(Vec<RecordBatch>),
    Ids(ArrayRef),
    /// A slot the plan never reads (e.g. the frame a `nodes()` result came from).
    Unused,
}

/// An in-memory host: tables read from `tests/data/`, indexes built on first use.
/// It has no relational evaluator and no mask evaluator, so a fixture runs only
/// steps the engine runs itself.
struct MemoryInputs(Vec<Input>);

fn unsupported<T>(what: &str) -> Result<T> {
    Err(PlanError::NotImplemented(format!(
        "the conformance host has no {what}"
    )))
}

impl Inputs for MemoryInputs {
    fn index(&self, frame: Slot) -> Result<GraphIndex> {
        let Input::Edges(batches, index) = &self.0[frame] else {
            return unsupported("index for a non-edge slot");
        };
        if let Some(i) = index.get() {
            return Ok(i.clone());
        }
        let pairs: Vec<(&dyn Array, &dyn Array)> = batches
            .iter()
            .map(|b| (b.column(0).as_ref(), b.column(1).as_ref()))
            .collect();
        let (topo, ids) =
            build_topology_batches(&pairs).map_err(|e| PlanError::Value(e.to_string()))?;
        Ok(index.get_or_init(|| GraphIndex::new(topo, ids)).clone())
    }

    fn edge_mask(&self, _frame: Slot, _n_edges: usize) -> Result<Option<Arc<EdgeMask>>> {
        unsupported("edge mask evaluator")
    }

    fn edge_weights(
        &self,
        frame: Slot,
        _columns: &[String],
        _keys: &[String],
    ) -> Result<Option<Vec<RecordBatch>>> {
        match &self.0[frame] {
            Input::Edges(batches, _) => Ok(Some(batches.clone())),
            _ => unsupported("edge table for a non-edge slot"),
        }
    }

    fn node_table(&self, frame: Slot, _columns: Option<&[String]>) -> Result<Rows> {
        match &self.0[frame] {
            Input::Nodes(batches) => Ok(Rows::Batches(batches.clone())),
            _ => unsupported("node table for a non-node slot"),
        }
    }

    fn edge_table(&self, frame: Slot, _columns: Option<&[String]>) -> Result<Rows> {
        match &self.0[frame] {
            Input::Edges(batches, _) => Ok(Rows::Batches(batches.clone())),
            _ => unsupported("edge table for a non-edge slot"),
        }
    }

    fn seeds(&self, slot: Slot) -> Result<ArrayRef> {
        match &self.0[slot] {
            Input::Ids(ids) => Ok(ids.clone()),
            _ => unsupported("seed set in a non-seed slot"),
        }
    }

    fn apply_steps(&self, _rows: Rows, steps: &[&Plan], _roles: &Roles) -> Result<Rows> {
        let ops: Vec<&str> = steps.iter().map(|s| s.op()).collect();
        unsupported(&format!("relational evaluator (asked to run {ops:?})"))
    }

    fn batches(&self, _rows: HostRows) -> Result<Vec<RecordBatch>> {
        unsupported("host rows")
    }
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// A bundled CSV as one batch: integer columns as Int64, a `weight` column as
/// Float64, anything else as Utf8, the way the Python package reads them.
fn read_csv(name: &str) -> RecordBatch {
    let text = std::fs::read_to_string(data_dir().join(name)).unwrap();
    let mut lines = text.lines().filter(|l| !l.is_empty());
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).collect();
    let mut fields = vec![];
    let mut columns: Vec<ArrayRef> = vec![];
    for (i, name) in header.iter().enumerate() {
        let cells: Vec<&str> = rows.iter().map(|r| r[i]).collect();
        let (dt, col): (DataType, ArrayRef) = if *name == "weight" {
            let v: Vec<f64> = cells.iter().map(|c| c.parse().unwrap()).collect();
            (DataType::Float64, Arc::new(Float64Array::from(v)))
        } else if let Ok(v) = cells
            .iter()
            .map(|c| c.parse::<i64>())
            .collect::<std::result::Result<Vec<_>, _>>()
        {
            (DataType::Int64, Arc::new(Int64Array::from(v)))
        } else {
            (DataType::Utf8, Arc::new(StringArray::from(cells)))
        };
        fields.push(Field::new(*name, dt, false));
        columns.push(col);
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
}

fn input(spec: &Value) -> Input {
    if let Some(name) = spec.get("edges").and_then(Value::as_str) {
        return Input::Edges(
            vec![read_csv(&format!("{name}_edges.csv"))],
            OnceCell::new(),
        );
    }
    if let Some(name) = spec.get("nodes").and_then(Value::as_str) {
        return Input::Nodes(vec![read_csv(&format!("{name}_nodes.csv"))]);
    }
    if spec.get("unused").is_some() {
        return Input::Unused;
    }
    let ids = spec
        .get("ids")
        .and_then(Value::as_array)
        .expect("an input slot");
    if ids.iter().all(Value::is_i64) {
        let v: Vec<i64> = ids.iter().map(|v| v.as_i64().unwrap()).collect();
        Input::Ids(Arc::new(Int64Array::from(v)))
    } else {
        let v: Vec<&str> = ids.iter().map(|v| v.as_str().unwrap()).collect();
        Input::Ids(Arc::new(StringArray::from(v)))
    }
}

/// One cell as JSON.
fn cell(array: &dyn Array, i: usize) -> Value {
    if array.is_null(i) {
        return Value::Null;
    }
    let any = array.as_any();
    macro_rules! num {
        ($t:ty) => {
            if let Some(a) = any.downcast_ref::<$t>() {
                return json!(a.value(i));
            }
        };
    }
    num!(Int64Array);
    num!(Int32Array);
    num!(UInt32Array);
    num!(UInt64Array);
    num!(Float64Array);
    num!(BooleanArray);
    if let Some(a) = any.downcast_ref::<Float32Array>() {
        return json!(a.value(i) as f64);
    }
    let strings = cast(array, &DataType::Utf8).unwrap();
    json!(strings
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .value(i))
}

/// Floats are compared to a relative 1e-12, everything else exactly.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            x == y || (x - y).abs() <= 1e-12 * x.abs().max(y.abs())
        }
        _ => a == b,
    }
}

fn run_case(path: &Path) {
    let case: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let name = path.file_name().unwrap().to_string_lossy();
    let inputs = MemoryInputs(
        case["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(input)
            .collect(),
    );
    let plan = Plan::from_json(&case["plan"].to_string())
        .unwrap_or_else(|e| panic!("{name}: the plan does not parse: {e}"));
    let expect = &case["expect"];
    let result = execute_plan(&plan, &inputs);
    if let Some(kind) = expect.get("error").and_then(Value::as_str) {
        let err = result
            .err()
            .unwrap_or_else(|| panic!("{name}: expected an error"));
        let got_kind = match &err {
            PlanError::NotImplemented(_) => "not_implemented",
            PlanError::ColumnNotFound(_) => "column_not_found",
            PlanError::Value(_) => "value",
            PlanError::Engine(_) => "engine",
            PlanError::Invalid(_) => "invalid",
            PlanError::Host(_) => "host",
        };
        assert_eq!(got_kind, kind, "{name}: {err}");
        let message = expect["message"].as_str().unwrap();
        assert_eq!(err.to_string(), message, "{name}");
        return;
    }
    let batches = result.unwrap_or_else(|e| panic!("{name}: {e}"));
    let schema = batches[0].schema();
    let columns: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    let want_columns: Vec<&str> = expect["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(columns, want_columns, "{name}: columns");
    let mut rows: Vec<Vec<Value>> = vec![];
    for b in &batches {
        for i in 0..b.num_rows() {
            rows.push(b.columns().iter().map(|c| cell(c.as_ref(), i)).collect());
        }
    }
    let want = expect["rows"].as_array().unwrap();
    assert_eq!(rows.len(), want.len(), "{name}: row count");
    for (r, (got, want)) in rows.iter().zip(want).enumerate() {
        let want = want.as_array().unwrap();
        let ok = got.len() == want.len() && got.iter().zip(want).all(|(a, b)| same(a, b));
        assert!(ok, "{name}: row {r}: got {got:?}, want {want:?}");
    }
}

#[test]
fn plan_fixtures_conform() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plans");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    cases.sort();
    assert!(cases.len() >= 10, "expected the plan fixtures in {dir:?}");
    for case in &cases {
        run_case(case);
    }
}

/// Every former per-shape entry point has at least one fixture.
#[test]
fn fixtures_cover_every_former_entry_point() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/plans");
    let all: String = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    for op in [
        "\"with_columns\"",  // run_node_query
        "\"join\"",          // run_join_query
        "\"hop\"",           // run_hop_query
        "\"shortest_path\"", // run_path_query
        "\"random_walk\"",   // run_walk_query
        "\"describe\"",      // graph_describe
    ] {
        assert!(all.contains(op), "no fixture exercises {op}");
    }
}
