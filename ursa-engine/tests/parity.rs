//! The SQL engine against the Python library: the same graph functions with
//! the same parameters must give the same ids, the same values (floats bit for
//! bit) and the same column types.
//!
//! The Python side is recorded in `fixtures/parity.json` by
//! `fixtures/generate_parity.py`; see that script for how to regenerate it.

use std::collections::BTreeMap;
use std::path::Path;

use datafusion::arrow::array::{Array, Float64Array, Int64Array, StringArray, UInt32Array};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::TableReference;
use serde_json::Value;
use ursa_engine::graph::{index_builds, registry_of, EdgeTable, GraphDef};
use ursa_engine::new_session;

/// An id as the fixture writes it: a JSON integer or string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Id {
    Int(i64),
    Str(String),
}

/// A value as the fixture writes it: an integer, or a float's bits in hex.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cell {
    UInt(u64),
    FloatBits(u64),
}

/// The Python name of an Arrow type, as `pyarrow` prints it.
fn pyarrow_name(t: &DataType) -> &'static str {
    match t {
        DataType::Int64 => "int64",
        DataType::Utf8 => "string",
        DataType::Float64 => "double",
        DataType::UInt32 => "uint32",
        other => panic!("unexpected column type {other}"),
    }
}

fn expected_rows(case: &Value) -> BTreeMap<Id, Cell> {
    let float = case["value_type"] == "double";
    case["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let id = match &row[0] {
                Value::Number(n) => Id::Int(n.as_i64().unwrap()),
                Value::String(s) => Id::Str(s.clone()),
                other => panic!("bad id {other}"),
            };
            let cell = if float {
                Cell::FloatBits(u64::from_str_radix(row[1].as_str().unwrap(), 16).unwrap())
            } else {
                Cell::UInt(row[1].as_u64().unwrap())
            };
            (id, cell)
        })
        .collect()
}

#[tokio::test]
async fn sql_matches_the_python_library() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/parity.json");
    let document: Value =
        serde_json::from_str(&std::fs::read_to_string(&fixtures).unwrap()).unwrap();
    let cases = document["cases"].as_array().unwrap();
    assert!(cases.len() >= 20, "the fixture has {} cases", cases.len());

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

    for case in cases {
        let function = case["function"].as_str().unwrap();
        let dataset = case["dataset"].as_str().unwrap();
        let args = case["sql_args"].as_str().unwrap();
        let sql = if args.is_empty() {
            format!("SELECT * FROM {function}({dataset})")
        } else {
            format!("SELECT * FROM {function}({dataset}, {args})")
        };
        let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        let schema = batches[0].schema();
        assert_eq!(
            pyarrow_name(schema.field(0).data_type()),
            case["id_type"],
            "{sql}: id type"
        );
        assert_eq!(schema.field(1).name(), function, "{sql}");
        assert_eq!(
            pyarrow_name(schema.field(1).data_type()),
            case["value_type"],
            "{sql}: value type"
        );

        let mut got = BTreeMap::new();
        for batch in &batches {
            let ids = batch.column(0);
            let values = batch.column(1);
            for row in 0..batch.num_rows() {
                let id = match ids.data_type() {
                    DataType::Int64 => Id::Int(
                        ids.as_any()
                            .downcast_ref::<Int64Array>()
                            .unwrap()
                            .value(row),
                    ),
                    _ => Id::Str(
                        ids.as_any()
                            .downcast_ref::<StringArray>()
                            .unwrap()
                            .value(row)
                            .to_string(),
                    ),
                };
                let cell = match values.data_type() {
                    DataType::Float64 => Cell::FloatBits(
                        values
                            .as_any()
                            .downcast_ref::<Float64Array>()
                            .unwrap()
                            .value(row)
                            .to_bits(),
                    ),
                    _ => Cell::UInt(u64::from(
                        values
                            .as_any()
                            .downcast_ref::<UInt32Array>()
                            .unwrap()
                            .value(row),
                    )),
                };
                assert!(got.insert(id, cell).is_none(), "{sql}: a repeated id");
            }
        }
        assert_eq!(got, expected_rows(case), "{sql}");
    }
    // One index per dataset, however many functions ran over it.
    assert_eq!(index_builds(&ctx), datasets.len() as u64);
}
