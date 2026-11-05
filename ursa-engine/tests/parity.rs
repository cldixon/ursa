//! The SQL engine against the Python library: the same graph functions with
//! the same parameters must give the same columns (names and types), the same
//! ids and the same values, floats bit for bit.
//!
//! The traversals return rows of their own; those are compared in order, as
//! the Python library returns them. The traversals' parameters and defaults
//! are compared with the Python signatures the fixture records.
//!
//! The Python side is recorded in `fixtures/parity.json` by
//! `fixtures/generate_parity.py`; see that script for how to regenerate it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use datafusion::arrow::array::{
    Array, ArrayRef, Float32Array, Float64Array, Int64Array, StringArray, UInt32Array,
};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::TableReference;
use serde_json::Value;
use ursa_engine::graph::traversal;
use ursa_engine::graph::{index_builds, registry_of, EdgeTable, GraphDef};
use ursa_engine::new_session;

/// A value as the fixture writes it: an id or integer, a string id, a
/// float's bits in hex, or null.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Cell {
    Int(i64),
    Str(String),
    Bits(String),
    Null,
}

/// The Python name of an Arrow type, as `pyarrow` prints it.
fn pyarrow_name(t: &DataType) -> &'static str {
    match t {
        DataType::Int64 => "int64",
        DataType::Utf8 => "string",
        DataType::Float64 => "double",
        DataType::Float32 => "float",
        DataType::UInt32 => "uint32",
        other => panic!("unexpected column type {other}"),
    }
}

fn fixture_cell(v: &Value, pyarrow_type: &str) -> Cell {
    match (v, pyarrow_type) {
        (Value::Null, _) => Cell::Null,
        (Value::String(s), "double" | "float") => Cell::Bits(s.clone()),
        (Value::String(s), _) => Cell::Str(s.clone()),
        (Value::Number(n), _) => Cell::Int(n.as_i64().unwrap()),
        (other, _) => panic!("bad cell {other}"),
    }
}

fn sql_cell(column: &ArrayRef, row: usize) -> Cell {
    if column.is_null(row) {
        return Cell::Null;
    }
    let any = column.as_any();
    match column.data_type() {
        DataType::Int64 => Cell::Int(any.downcast_ref::<Int64Array>().unwrap().value(row)),
        DataType::UInt32 => Cell::Int(i64::from(
            any.downcast_ref::<UInt32Array>().unwrap().value(row),
        )),
        DataType::Utf8 => Cell::Str(
            any.downcast_ref::<StringArray>()
                .unwrap()
                .value(row)
                .to_string(),
        ),
        DataType::Float64 => Cell::Bits(format!(
            "{:016x}",
            any.downcast_ref::<Float64Array>()
                .unwrap()
                .value(row)
                .to_bits()
        )),
        DataType::Float32 => Cell::Bits(format!(
            "{:08x}",
            any.downcast_ref::<Float32Array>()
                .unwrap()
                .value(row)
                .to_bits()
        )),
        other => panic!("unexpected column type {other}"),
    }
}

/// Rows keyed by their `id` (the first cell) for a per-node result, whose row
/// order SQL leaves open; in order for a one-row statistic.
fn keyed(rows: Vec<Vec<Cell>>, per_node: bool) -> BTreeMap<Cell, Vec<Cell>> {
    let n = rows.len();
    let map: BTreeMap<Cell, Vec<Cell>> = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            if per_node {
                (row[0].clone(), row)
            } else {
                (Cell::Int(i as i64), row)
            }
        })
        .collect();
    assert_eq!(map.len(), n, "a repeated id");
    map
}

/// The traversals take the Python library's parameters, by name, with its
/// defaults, the node ids first.
#[test]
fn traversal_parameters_match_python() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parity.json");
    let document: Value =
        serde_json::from_str(&std::fs::read_to_string(&fixtures).unwrap()).unwrap();
    let signatures = document["traversal_signatures"].as_object().unwrap();
    assert_eq!(signatures.len(), traversal::TRAVERSALS.len());
    for spec in traversal::TRAVERSALS {
        let python: Vec<(String, Value)> = signatures[spec.name]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_str().unwrap().to_string(), p[1].clone()))
            .collect();
        let sql: Vec<(String, Value)> = spec
            .params
            .iter()
            .map(|p| {
                let default = match p.default {
                    None => Value::from("<required>"),
                    Some(ursa_core::catalog::Value::Null) => Value::Null,
                    Some(ursa_core::catalog::Value::UInt(u)) => Value::from(u),
                    Some(ursa_core::catalog::Value::Str(s)) => Value::from(s),
                    Some(other) => panic!("{}: unexpected default {other:?}", spec.name),
                };
                (p.name.to_string(), default)
            })
            .collect();
        assert_eq!(sql, python, "{}", spec.name);
        // The node ids come first, as SQL lists them.
        let first_other = spec.params.iter().position(|p| !p.takes_nodes()).unwrap();
        assert!(spec.params[first_other..].iter().all(|p| !p.takes_nodes()));
    }
}

#[tokio::test]
async fn sql_matches_the_python_library() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parity.json");
    let document: Value =
        serde_json::from_str(&std::fs::read_to_string(&fixtures).unwrap()).unwrap();
    let cases = document["cases"].as_array().unwrap();
    assert!(cases.len() >= 100, "the fixture has {} cases", cases.len());

    let ctx = new_session();
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../python/ursa/datasets/data");
    let mut datasets = Vec::new();
    for case in cases {
        let dataset = case["dataset"].as_str().unwrap();
        if datasets.contains(&dataset) {
            continue;
        }
        datasets.push(dataset);
        let path = data.join(case["csv"].as_str().unwrap());
        ctx.sql(&format!(
            "CREATE VIEW {dataset}_edges AS SELECT * FROM '{}'",
            path.display()
        ))
        .await
        .unwrap();
        registry_of(&ctx).put(GraphDef {
            name: dataset.into(),
            edges: EdgeTable {
                table: TableReference::bare(format!("{dataset}_edges")),
                source: "src".into(),
                destination: "dst".into(),
            },
            vertices: None,
        });
    }

    let mut functions = std::collections::BTreeSet::new();
    let mut weighted = 0;
    let data_dir = data.canonicalize().unwrap();
    for case in cases {
        let function = case["function"].as_str().unwrap();
        let dataset = case["dataset"].as_str().unwrap();
        let args = case["sql_args"]
            .as_str()
            .unwrap()
            .replace("{data}", &data_dir.display().to_string());
        let args = args.as_str();
        functions.insert(function);
        weighted += usize::from(args.contains("weight =>"));
        let sql = if args.is_empty() {
            format!("SELECT * FROM {function}({dataset})")
        } else {
            format!("SELECT * FROM {function}({dataset}, {args})")
        };
        let frame = ctx.sql(&sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        let schema = Arc::new(frame.schema().as_arrow().clone());
        let batches = frame
            .collect()
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let columns: Vec<(String, String)> = schema
            .fields()
            .iter()
            .map(|f| (f.name().clone(), pyarrow_name(f.data_type()).to_string()))
            .collect();
        let expected_columns: Vec<(String, String)> = case["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c[0].as_str().unwrap().to_string(),
                    c[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(columns, expected_columns, "{sql}: columns");
        let per_node = columns[0].0 == "id";

        let mut got = Vec::new();
        for batch in &batches {
            for row in 0..batch.num_rows() {
                got.push(
                    batch
                        .columns()
                        .iter()
                        .map(|c| sql_cell(c, row))
                        .collect::<Vec<_>>(),
                );
            }
        }
        let expected: Vec<Vec<Cell>> = case["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                row.as_array()
                    .unwrap()
                    .iter()
                    .zip(&expected_columns)
                    .map(|(v, (_, t))| fixture_cell(v, t))
                    .collect()
            })
            .collect();
        if traversal::lookup(function).is_some() {
            assert_eq!(got, expected, "{sql}");
        } else {
            assert_eq!(keyed(got, per_node), keyed(expected, per_node), "{sql}");
        }
    }
    // Every function SQL offers is covered, with weights.
    for f in ursa_engine::graph::functions::specs() {
        assert!(functions.contains(f.name), "no parity case for {}", f.name);
    }
    for t in traversal::TRAVERSALS {
        assert!(functions.contains(t.name), "no parity case for {}", t.name);
    }
    assert!(weighted >= 5, "{weighted} weighted cases");
    // One index per dataset, however many functions ran over it, weighted or
    // not.
    assert_eq!(index_builds(&ctx), datasets.len() as u64);
}
