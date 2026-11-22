//! The UGR fixtures in `tests/fixtures/`: each `NAME.ugr` is what the encoder
//! writes for a value built here, and `NAME.json` says what a reader must
//! find in it. The JSON is written from the model's accessors, or by hand,
//! never from the bytes, so `tests/ugr.test.ts` checks the JavaScript reader
//! against Rust's values and not against its own reading.
//!
//! The fixtures are committed, and this test fails when the encoder no longer
//! writes them. If the format was meant to change (with its version, and
//! `UGR.md`), rewrite them:
//!
//! ```sh
//! UGR_BLESS=1 cargo test -p ursa-viz --test ugr_fixtures
//! ```

use std::path::PathBuf;

use serde_json::{json, Value};
use ursa_viz::model::*;

fn bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|v| v.to_bits()).collect()
}

fn columns(columns: &[Column]) -> Value {
    let column = |c: &Column| {
        let role = c.role.map(|role| match role {
            Role::Metric => "metric",
            Role::Category => "category",
            Role::Distance => "distance",
            Role::Property => "property",
            other => panic!("write {other:?} here as the format does"),
        });
        let (values, dict) = match &c.values {
            Values::F32(v) => (bits(v), Value::Null),
            Values::U32(v) => (v.clone(), Value::Null),
            Values::Dict { values, codes } => {
                (codes.clone(), json!(values.iter().collect::<Vec<_>>()))
            }
        };
        json!({"name": c.name, "type": c.values.type_name(), "role": role, "values": values, "dict": dict})
    };
    columns.iter().map(column).collect()
}

/// What a reader must find in a result. `f32` values are their bits, and
/// rows are decimal text, so JSON loses nothing.
fn expect(r: &GraphResult) -> Value {
    let labels: Vec<Value> = r
        .node_labels()
        .iter()
        .map(|l| {
            let key = match l.key {
                KeyType::Int => "int",
                KeyType::Text => "text",
            };
            json!({"name": l.name, "key": key})
        })
        .collect();
    let layout = match r.encodings().layout {
        Layout::Force => "force",
        Layout::Rings => "rings",
        Layout::Layers => "layers",
    };
    let paths: Vec<Value> = r
        .highlights()
        .paths
        .iter()
        .map(|p| json!({"nodes": p.nodes, "edges": p.edges}))
        .collect();
    json!({
        "graph": r.graph(),
        "encodings": {
            "size_by": r.encodings().size_by,
            "color_by": r.encodings().color_by,
            "group_by": r.encodings().group_by,
            "layout": layout,
        },
        "provenance": {"sql": r.provenance().sql, "rule": r.provenance().rule},
        "nodes": {
            "count": r.n_nodes(),
            "labelNames": labels,
            "keys": r.node_keys().iter().collect::<Vec<_>>(),
            "labels": r.node_label(),
            "flags": r.node_flags(),
            "columns": columns(r.node_columns()),
            "x": r.positions().map(|p| bits(&p.x)),
            "y": r.positions().map(|p| bits(&p.y)),
        },
        "edges": {
            "count": r.n_edges(),
            "labelNames": r.edge_labels(),
            "src": r.src(),
            "dst": r.dst(),
            "labels": r.edge_label().to_vec(),
            "rows": r.edge_rows().map(|rows| rows.iter().map(u64::to_string).collect::<Vec<_>>()),
            "flags": r.edge_flags().to_vec(),
            "columns": columns(r.edge_columns()),
            "adjacency": Value::Null,
        },
        "highlights": {
            "paths": paths,
            "seeds": r.highlights().seeds,
            "distance": r.highlights().distance,
        },
    })
}

/// Apply the op that `make` makes for the result as it is.
fn apply(r: &mut GraphResult, make: impl FnOnce(&GraphResult) -> Op) {
    let op = make(r);
    r.apply(&GraphDelta::new(vec![op])).unwrap();
}

/// One label, integer keys, no columns: what a plain edge list becomes.
fn plain() -> GraphResult {
    let delta = GraphDelta::new(vec![
        Op::AddNodeLabel(NodeLabel::new("", KeyType::Int)),
        Op::AddEdgeLabel(String::new()),
        Op::AddNodes(NodeBatch::of_label(0, [10i64, -20, 30])),
        Op::AddEdges(EdgeBatch::of_label(0, vec![0, 1, 2, 0], vec![1, 2, 0, 0])),
    ]);
    let provenance = Provenance {
        sql: Some("SELECT src, dst FROM 'edges.parquet'".into()),
        rule: Some("edges".into()),
        ..Provenance::default()
    };
    GraphResult::build(None, provenance, &delta).unwrap()
}

/// One of everything: text and integer keys, two labels sharing a key, a
/// column of each type with its null, positions, rows, a removed node and
/// edge, paths, seeds, distances and encodings.
fn town() -> GraphResult {
    let delta = GraphDelta::new(vec![
        Op::AddNodeLabel(NodeLabel::new("people", KeyType::Int)),
        Op::AddNodeLabel(NodeLabel::new("places", KeyType::Text)),
        Op::AddEdgeLabel("knows".into()),
        Op::AddEdgeLabel("lives_in".into()),
        Op::AddNodes(NodeBatch::of_label(0, [1i64, 2, -3])),
        Op::AddNodes(NodeBatch::of_label(1, ["1", "Zürich", "東京"])),
        Op::AddEdges(EdgeBatch::of_label(0, vec![0, 1], vec![1, 2])),
        Op::AddEdges(
            EdgeBatch::of_label(1, vec![0, 2, 1], vec![3, 4, 5]).with_rows(vec![
                7,
                NO_ROW,
                u64::MAX - 1,
            ]),
        ),
    ]);
    let provenance = Provenance {
        sql: Some("FROM GRAPH_TABLE (town MATCH (a)-[e]->(b) COLUMNS (a.id, b.id))".into()),
        rule: Some("pattern".into()),
        ..Provenance::default()
    };
    let mut r = GraphResult::build(Some("town".into()), provenance, &delta).unwrap();
    for (target, column) in [
        (
            Target::Nodes,
            Column::new(
                "pagerank",
                Some(Role::Metric),
                Values::F32(vec![0.5, 0.25, f32::NAN, 0.0, -0.0, f32::MAX]),
            ),
        ),
        (
            Target::Nodes,
            Column::new(
                "hops",
                Some(Role::Distance),
                Values::U32(vec![0, 1, 2, 1, NULL, 2]),
            ),
        ),
        (
            Target::Nodes,
            Column::new(
                "club",
                Some(Role::Category),
                Values::dict([
                    Some("red"),
                    Some("blue"),
                    None,
                    Some("red"),
                    Some("blå"),
                    Some(""),
                ]),
            ),
        ),
        (
            Target::Edges,
            Column::new("weight", None, Values::F32(vec![1.0, 2.0, 3.0, 4.0, 5.5])),
        ),
        (
            Target::Edges,
            Column::new(
                "since",
                Some(Role::Property),
                Values::dict([Some("2019"), None, Some("2024"), Some("2019"), None]),
            ),
        ),
    ] {
        apply(&mut r, |r| r.set_column_op(target, column));
    }
    apply(&mut r, |r| {
        r.set_positions_op(Some(Positions {
            x: vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0],
            y: vec![0.5, -1.5, 2.5, f32::INFINITY, 1e-30, -5.0],
        }))
    });
    apply(&mut r, |_| Op::RemoveNodes(vec![4]));
    apply(&mut r, |_| Op::RemoveEdges(vec![1]));
    apply(&mut r, |r| {
        r.set_highlights_op(Highlights {
            paths: vec![
                Path {
                    nodes: vec![0, 1, 2],
                    edges: vec![0, 1],
                },
                Path {
                    nodes: vec![3, 0, 1, 5],
                    edges: vec![],
                },
            ],
            seeds: vec![0, 3],
            distance: Some("hops".into()),
        })
    });
    apply(&mut r, |r| {
        r.set_encodings_op(Encodings {
            size_by: Some("pagerank".into()),
            color_by: Some("club".into()),
            group_by: None,
            layout: Layout::Rings,
        })
    });
    r
}

/// A step on [`town`]: a new label, two nodes, two edges, a removal and a
/// restyle. Returned with what its sections hold, written out by hand.
fn expand() -> (GraphDelta, Value) {
    let town = town();
    let club = &town.node_column("club").unwrap().values;
    let delta = GraphDelta::new(vec![
        Op::AddNodeLabel(NodeLabel::new("pets", KeyType::Text)),
        Op::AddNodes(NodeBatch {
            labels: vec![2, 0],
            keys: Strings::from_iter(["Rex", "40"]),
            columns: vec![
                BatchColumn::new(
                    "club",
                    BatchValues::dict(club, [Some("green"), Some("blue")]),
                ),
                BatchColumn::new("pagerank", BatchValues::F32(vec![0.125, 1.0])),
            ],
            positions: Some(Positions {
                x: vec![9.0, 8.0],
                y: vec![7.0, -6.0],
            }),
        }),
        Op::AddEdges(
            EdgeBatch::of_label(0, vec![7, 0], vec![6, 7])
                .with_rows(vec![NO_ROW, 12])
                .with_column("weight", BatchValues::F32(vec![0.5, 0.25])),
        ),
        Op::RemoveNodes(vec![0, 5]),
        town.set_encodings_op(Encodings {
            size_by: None,
            color_by: Some("hops".into()),
            group_by: None,
            layout: Layout::Force,
        }),
    ])
    .with_provenance(Provenance {
        sql: Some("SELECT * FROM hop(town, seeds => [1], n => 1)".into()),
        rule: Some("rings".into()),
        ..Provenance::default()
    });
    let mut after = town;
    after
        .apply(&delta)
        .expect("the fixture's delta fits its result");

    let f = |v: f32| v.to_bits();
    let expected = json!({
        "provenance": {"sql": "SELECT * FROM hop(town, seeds => [1], n => 1)", "rule": "rings"},
        "ops": ["add_node_label", "add_nodes", "add_edges", "remove_nodes", "set_encodings"],
        "sections": {
            "ops.1.keys.offsets": [0, 3, 5],
            "ops.1.keys.bytes": "Rex40".as_bytes(),
            "ops.1.labels": [2, 0],
            // The column has four values ('red', 'blue', 'blå', ''), so
            // 'green' is new and gets the code 4; 'blue' was 1.
            "ops.1.columns.0.values.offsets": [0, 5],
            "ops.1.columns.0.values.bytes": "green".as_bytes(),
            "ops.1.columns.0": [4, 1],
            "ops.1.columns.1": [f(0.125), f(1.0)],
            "ops.1.x": [f(9.0), f(8.0)],
            "ops.1.y": [f(7.0), f(-6.0)],
            "ops.2.src": [7, 0],
            "ops.2.dst": [6, 7],
            "ops.2.rows": [u64::MAX.to_string(), "12"],
            "ops.2.columns.0": [f(0.5), f(0.25)],
            "ops.3.index": [0, 5],
        },
    });
    (delta, expected)
}

fn check(name: &str, bytes: &[u8], expected: &Value) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let (ugr, json) = (
        dir.join(format!("{name}.ugr")),
        dir.join(format!("{name}.json")),
    );
    let text = serde_json::to_string(expected).unwrap() + "\n";
    if std::env::var_os("UGR_BLESS").is_some() {
        std::fs::write(&ugr, bytes).unwrap();
        std::fs::write(&json, text).unwrap();
        return;
    }
    let hint =
        "if the format was meant to change: UGR_BLESS=1 cargo test -p ursa-viz --test ugr_fixtures";
    let read = |path: &PathBuf| {
        std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}; {hint}", path.display()))
    };
    assert!(
        read(&ugr) == bytes,
        "the encoder no longer writes {name}.ugr; {hint}"
    );
    assert!(
        read(&json) == text.as_bytes(),
        "{name}.json is out of date; {hint}"
    );
}

#[test]
fn the_fixtures_are_what_the_encoder_writes() {
    for (name, result) in [
        ("empty", GraphResult::default()),
        ("plain", plain()),
        ("town", town()),
    ] {
        let bytes = result.to_bytes();
        assert_eq!(GraphResult::from_bytes(&bytes).unwrap(), result, "{name}");
        check(name, &bytes, &expect(&result));
    }
    let (delta, expected) = expand();
    let bytes = delta.to_bytes();
    assert_eq!(GraphDelta::from_bytes(&bytes).unwrap(), delta);
    check("expand", &bytes, &expected);

    // `plain` with its undirected adjacency beside the edges (#501), by
    // hand: 0 - 1, 1 - 2, 2 - 0, and the self-loop left out.
    let result = plain();
    let adjacency = ursa_core::topology::UndirectedCsr::from_edges(3, result.src(), result.dst());
    let bytes = result.to_bytes_with_adjacency(&adjacency);
    assert_eq!(GraphResult::from_bytes(&bytes).unwrap(), result);
    let mut expected = expect(&result);
    expected["edges"]["adjacency"] = json!({
        "offsets": ["0", "2", "4", "6"],
        "targets": [1, 2, 0, 2, 0, 1],
    });
    check("adjacent", &bytes, &expected);
}

/// The committed bytes still decode to the values: a reader of version 1
/// keeps reading what version 1 wrote.
#[test]
fn the_committed_fixtures_decode() {
    if std::env::var_os("UGR_BLESS").is_some() {
        return;
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let read = |name: &str| std::fs::read(dir.join(name)).unwrap();
    assert_eq!(
        GraphResult::from_bytes(&read("empty.ugr")).unwrap(),
        GraphResult::default()
    );
    assert_eq!(
        GraphResult::from_bytes(&read("plain.ugr")).unwrap(),
        plain()
    );
    // With an adjacency beside its edges it is the same result, and the
    // adjacency reads back.
    assert_eq!(
        GraphResult::from_bytes(&read("adjacent.ugr")).unwrap(),
        plain()
    );
    let adjacency = ugr::read_adjacency(&read("adjacent.ugr")).unwrap().unwrap();
    assert_eq!(adjacency.targets, [1, 2, 0, 2, 0, 1]);
    assert!(ugr::read_adjacency(&read("plain.ugr")).unwrap().is_none());
    let mut town = GraphResult::from_bytes(&read("town.ugr")).unwrap();
    assert_eq!(town, self::town());
    // The delta applies to the result it was made for, and undoes.
    let delta = GraphDelta::from_bytes(&read("expand.ugr")).unwrap();
    town.apply(&delta).unwrap();
    assert_eq!((town.n_nodes(), town.n_edges()), (8, 7));
    town.apply(&delta.invert()).unwrap();
    assert_eq!(town.to_bytes(), read("town.ugr"));
}
