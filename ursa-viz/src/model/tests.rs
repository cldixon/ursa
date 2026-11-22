use serde_json::Value;

use super::*;

fn delta(ops: Vec<Op>) -> GraphDelta {
    GraphDelta::new(ops)
}

fn build(ops: Vec<Op>) -> GraphResult {
    GraphResult::build(None, Provenance::default(), &delta(ops)).unwrap()
}

fn message<T: std::fmt::Debug, E: std::fmt::Display>(result: Result<T, E>) -> String {
    result.unwrap_err().to_string()
}

/// A result with one of everything: two labels sharing a key, integer and
/// text keys, a removed node and a removed edge, rows known for some edges,
/// a column of each type with a null, positions, a path, seeds, distances
/// and encodings.
fn sample() -> GraphResult {
    let mut r = GraphResult::build(
        Some("town".into()),
        Provenance {
            sql: Some("FROM GRAPH_TABLE (town MATCH (a)-[e]->(b) COLUMNS (a.id, b.id))".into()),
            rule: Some("edges".into()),
            ..Provenance::default()
        },
        &delta(vec![
            Op::AddNodeLabel(NodeLabel::new("people", KeyType::Int)),
            Op::AddNodeLabel(NodeLabel::new("places", KeyType::Text)),
            Op::AddEdgeLabel("knows".into()),
            Op::AddEdgeLabel("lives_in".into()),
            Op::AddNodes(NodeBatch::of_label(0, [1i64, 2, -3])),
            Op::AddNodes(NodeBatch::of_label(1, ["1", "Zürich"])),
            Op::AddEdges(EdgeBatch::of_label(0, vec![0, 1], vec![1, 2])),
            Op::AddEdges(EdgeBatch::of_label(1, vec![0, 2], vec![3, 4]).with_rows(vec![7, NO_ROW])),
        ]),
    )
    .unwrap();
    // One at a time: an op is made for the result as it is.
    for (target, column) in [
        (
            Target::Nodes,
            Column::new(
                "pagerank",
                Some(Role::Metric),
                Values::F32(vec![0.5, 0.25, f32::NAN, 0.0, -0.0]),
            ),
        ),
        (
            Target::Nodes,
            Column::new(
                "hops",
                Some(Role::Distance),
                Values::U32(vec![0, 1, 2, 1, NULL]),
            ),
        ),
        (
            Target::Nodes,
            Column::new(
                "club",
                Some(Role::Category),
                Values::dict([Some("red"), Some("blue"), None, Some("red"), Some("blå")]),
            ),
        ),
        (
            Target::Edges,
            Column::new("weight", None, Values::F32(vec![1.0, 2.0, 3.0, 4.0])),
        ),
    ] {
        r.apply(&delta(vec![r.set_column_op(target, column)]))
            .unwrap();
    }
    let ops = vec![
        r.set_positions_op(Some(Positions {
            x: vec![0.0, 1.0, 2.0, 3.0, 4.0],
            y: vec![0.5, -1.5, 2.5, f32::INFINITY, 1e-30],
        })),
        Op::RemoveNodes(vec![4]),
        Op::RemoveEdges(vec![1]),
    ];
    r.apply(&delta(ops)).unwrap();
    let ops = vec![
        r.set_highlights_op(Highlights {
            paths: vec![
                Path {
                    nodes: vec![0, 1, 2],
                    edges: vec![0, 1],
                },
                Path {
                    nodes: vec![3, 0],
                    edges: vec![],
                },
            ],
            seeds: vec![0, 3],
            distance: Some("hops".into()),
        }),
        r.set_encodings_op(Encodings {
            size_by: Some("pagerank".into()),
            color_by: Some("club".into()),
            group_by: None,
            layout: Layout::Rings,
        }),
    ];
    r.apply(&delta(ops)).unwrap();
    r
}

// The model.

#[test]
fn an_empty_result_is_valid_and_round_trips() {
    let mut empty = GraphResult::default();
    empty.validate().unwrap();
    assert_eq!((empty.n_nodes(), empty.n_edges()), (0, 0));
    let bytes = empty.to_bytes();
    assert_eq!(GraphResult::from_bytes(&bytes).unwrap(), empty);
    assert_eq!(
        ugr::decode(&GraphDelta::default().to_bytes()).unwrap(),
        ugr::Message::Delta(GraphDelta::default())
    );
}

#[test]
fn keys_are_typed_and_two_labels_may_share_one() {
    let r = sample();
    assert_eq!(r.n_nodes(), 5);
    assert_eq!(r.node_key(2), Key::Int(-3));
    assert_eq!(r.node_key(3), Key::Text("1".into()));
    assert_eq!(r.node_keys().get(4), "Zürich");
    // The key 1 is a person and a place: two nodes.
    assert_eq!(r.node(0, &Key::Int(1)), Some(0));
    assert_eq!(r.node(1, &Key::from("1")), Some(3));
    assert_eq!(r.node(0, &Key::from("1")), None);
    assert_eq!(r.node(1, &Key::from("Bern")), None);
}

#[test]
fn an_identity_cannot_be_added_twice() {
    let labels = || Op::AddNodeLabel(NodeLabel::new("v", KeyType::Int));
    let twice = delta(vec![
        labels(),
        Op::AddNodes(NodeBatch::of_label(0, [1i64, 2, 1])),
    ]);
    let mut r = GraphResult::default();
    assert_eq!(
        message(r.apply(&twice)),
        "op 1 (add_nodes): '1' of the label 'v' is node 0 already"
    );
    assert_eq!(r, GraphResult::default());

    let mut r = build(vec![
        labels(),
        Op::AddNodes(NodeBatch::of_label(0, [1i64, 2])),
    ]);
    let before = r.clone();
    let again = delta(vec![Op::AddNodes(NodeBatch::of_label(0, [3i64, 2]))]);
    assert!(message(r.apply(&again)).contains("'2' of the label 'v' is node 1 already"));
    assert_eq!(r, before);
    // The refused batch left nothing in the index.
    assert_eq!(r.node(0, &Key::Int(3)), None);
    r.apply(&delta(vec![Op::AddNodes(NodeBatch::of_label(0, [3i64]))]))
        .unwrap();
    assert_eq!(r.node(0, &Key::Int(3)), Some(2));
}

#[test]
fn an_integer_label_takes_only_canonical_integers() {
    for text in ["x", "07", "+7", "1.0", "", "9223372036854775808"] {
        let mut batch = NodeBatch::new();
        batch.labels.push(0);
        batch.keys.push(text);
        let mut r = build(vec![Op::AddNodeLabel(NodeLabel::new("v", KeyType::Int))]);
        let e = message(r.apply(&delta(vec![Op::AddNodes(batch)])));
        assert!(
            e.contains(&format!("the key '{text}' is not an integer")),
            "{e}"
        );
    }
    assert_eq!(
        Key::parse(KeyType::Int, "-9223372036854775808").unwrap(),
        Key::Int(i64::MIN)
    );
}

#[test]
fn edges_must_name_nodes_and_labels_that_exist() {
    let mut r = sample();
    let before = r.clone();
    let e = message(r.apply(&delta(vec![Op::AddEdges(EdgeBatch::of_label(
        0,
        vec![0],
        vec![5],
    ))])));
    assert!(
        e.contains("edge 4 names node 5, but the result has 5 nodes"),
        "{e}"
    );
    let e = message(r.apply(&delta(vec![Op::AddEdges(EdgeBatch::of_label(
        2,
        vec![0],
        vec![1],
    ))])));
    assert!(
        e.contains("edge 4 has the label 2, but the result has 2 edge labels"),
        "{e}"
    );
    let e = message(r.apply(&delta(vec![Op::AddNodes(NodeBatch::of_label(9, [1i64]))])));
    assert!(e.contains("new node 0 has the label 9"), "{e}");
    assert_eq!(r, before);
}

#[test]
fn a_removed_node_keeps_its_slot_and_hides_its_edges() {
    let mut r = sample();
    // Node 4 is removed: still there, with its key and its values.
    assert!(r.node_is_removed(4));
    assert_eq!(r.node(1, &Key::from("Zürich")), Some(4));
    assert_eq!(r.node_column("club").unwrap().values.text(4), Some("blå"));
    // Edge 3 ends at it, edge 1 is removed itself.
    let shown: Vec<bool> = (0..4).map(|e| r.edge_is_shown(e)).collect();
    assert_eq!(shown, [true, false, true, false]);
    assert_eq!(
        (r.edge_row(2), r.edge_row(3), r.edge_row(0)),
        (Some(7), None, None)
    );

    r.apply(&delta(vec![Op::RestoreNodes(vec![4])])).unwrap();
    assert!(r.edge_is_shown(3));
    // Indexes did not move.
    assert_eq!((r.src()[3], r.dst()[3]), (2, 4));
}

#[test]
fn removing_is_strict() {
    let mut r = sample();
    let before = r.clone();
    for (ops, want) in [
        (vec![Op::RemoveNodes(vec![4])], "node 4 is removed already"),
        (vec![Op::RestoreNodes(vec![0])], "node 0 is not removed"),
        (vec![Op::RemoveNodes(vec![0, 0])], "node 0 is listed twice"),
        (
            vec![Op::RemoveNodes(vec![5])],
            "node 5, but the result has 5 nodes",
        ),
        (vec![Op::RemoveEdges(vec![1])], "edge 1 is removed already"),
        (
            vec![Op::RestoreEdges(vec![9])],
            "edge 9, but the result has 4 edges",
        ),
    ] {
        let e = message(r.apply(&delta(ops)));
        assert!(e.contains(want), "{e}");
        assert_eq!(r, before);
    }
}

#[test]
fn columns_have_one_value_a_slot_and_codes_in_range() {
    let mut r = sample();
    let before = r.clone();
    let short = r.set_column_op(
        Target::Nodes,
        Column::new("x", None, Values::U32(vec![1, 2])),
    );
    let e = message(r.apply(&delta(vec![short])));
    assert!(
        e.contains("the node column 'x' has 2 values, for 5 rows"),
        "{e}"
    );
    let bad_code = Values::Dict {
        values: Strings::from_iter(["a"]),
        codes: vec![0, 1, NULL, 0],
    };
    let op = r.set_column_op(Target::Edges, Column::new("kind", None, bad_code));
    let e = message(r.apply(&delta(vec![op])));
    assert!(
        e.contains("the edge column 'kind' has the code 1, but only 1 values"),
        "{e}"
    );
    // A second column of a name: only by a hand-made op, and refused.
    let dup = Op::SetColumn {
        target: Target::Nodes,
        index: 3,
        old: None,
        new: Some(Column::new("club", None, Values::U32(vec![0; 5]))),
    };
    let e = message(r.apply(&delta(vec![dup])));
    assert!(e.contains("there is a node column 'club' already"), "{e}");
    assert_eq!(r, before);
}

#[test]
fn highlights_and_encodings_name_what_exists() {
    let mut r = sample();
    let before = r.clone();
    let highlight = |paths, seeds, distance: Option<&str>| Highlights {
        paths,
        seeds,
        distance: distance.map(String::from),
    };
    let path = |nodes: &[u32], edges: &[u32]| Path {
        nodes: nodes.to_vec(),
        edges: edges.to_vec(),
    };
    for (new, want) in [
        (highlight(vec![], vec![5], None), "a seed is node 5"),
        (
            highlight(vec![path(&[], &[])], vec![], None),
            "path 0 has no nodes",
        ),
        (
            highlight(vec![path(&[0, 9], &[])], vec![], None),
            "path 0 names node 9",
        ),
        (
            highlight(vec![path(&[0, 1, 2], &[0])], vec![], None),
            "path 0 has 3 nodes and 1 edges",
        ),
        (
            highlight(vec![path(&[0, 2], &[0])], vec![], None),
            "path 0: edge 0 joins nodes 0 and 1, not 0 and 2",
        ),
        (
            highlight(vec![path(&[0, 1], &[8])], vec![], None),
            "path 0 names edge 8",
        ),
        (
            highlight(vec![], vec![], Some("nope")),
            "there is no node column 'nope'",
        ),
        (
            highlight(vec![], vec![], Some("pagerank")),
            "the distance column 'pagerank' is not a u32 column",
        ),
    ] {
        let e = message(r.apply(&delta(vec![r.set_highlights_op(new)])));
        assert!(e.contains(want), "{e}");
    }
    // A path may run against an edge's direction.
    let back = highlight(vec![path(&[2, 1, 0], &[1, 0])], vec![], None);
    r.clone()
        .apply(&delta(vec![r.set_highlights_op(back)]))
        .unwrap();

    for (new, want) in [
        (
            Encodings {
                size_by: Some("club".into()),
                ..Encodings::default()
            },
            "the column 'club' is text, so it cannot size the nodes",
        ),
        (
            Encodings {
                color_by: Some("weight".into()),
                ..Encodings::default()
            },
            "there is no node column 'weight' to colour by",
        ),
    ] {
        let e = message(r.apply(&delta(vec![r.set_encodings_op(new)])));
        assert!(e.contains(want), "{e}");
    }
    assert_eq!(r, before);
}

#[test]
fn a_column_in_use_cannot_be_dropped_or_retyped() {
    let mut r = sample();
    let before = r.clone();
    let drop = r.drop_column_op(Target::Nodes, "hops").unwrap();
    let e = message(r.apply(&delta(vec![drop.clone()])));
    assert!(
        e.contains("there is no node column 'hops' for the distances"),
        "{e}"
    );
    let retype = r.set_column_op(
        Target::Nodes,
        Column::new("pagerank", None, Values::dict([None; 5])),
    );
    let e = message(r.apply(&delta(vec![retype])));
    assert!(e.contains("cannot size the nodes"), "{e}");
    assert_eq!(r, before);
    // Clear the highlight in the same delta, and the drop goes through.
    let clear = r.set_highlights_op(Highlights::default());
    let both = delta(vec![clear, drop]);
    r.apply(&both).unwrap();
    assert!(r.node_column("hops").is_none());
    r.apply(&both.invert()).unwrap();
    assert_eq!(r, before);
    assert!(r.drop_column_op(Target::Edges, "hops").is_none());
}

#[test]
fn validate_catches_what_apply_would_not_let_in() {
    type Breakage = fn(&mut GraphResult);
    let breakages: Vec<(Breakage, &str)> = vec![
        (|r| r.node_label[0] = 7, "node 0 has the label 7"),
        (
            |r| r.node_label[4] = 0,
            "node 4: the key 'Zürich' is not an integer",
        ),
        (|r| r.dst[0] = 99, "edge 0 names node 99"),
        (|r| r.edge_label.make_mut()[2] = 5, "edge 2 has the label 5"),
        (
            |r| r.node_flags.pop().map(drop).unwrap(),
            "5 nodes, but 5 keys and 4 flags",
        ),
        (
            |r| r.edge_rows = Some(vec![NO_ROW; 4]),
            "the edges' rows are present, but none is known",
        ),
        (|r| r.edge_rows = Some(vec![1]), "4 edges, but 1 rows"),
        (
            |r| r.positions.as_mut().unwrap().y.push(0.0),
            "5 nodes, but 5 x and 6 y positions",
        ),
        (|r| r.highlights.seeds.push(77), "a seed is node 77"),
        (
            |r| r.encodings.group_by = Some("gone".into()),
            "there is no node column 'gone' to group by",
        ),
        (
            |r| {
                let c = r.node_columns[0].clone();
                r.node_columns.push(c)
            },
            "there are two of the node column 'pagerank'",
        ),
    ];
    for (breakage, want) in breakages {
        let mut r = sample();
        breakage(&mut r);
        let e = message(r.validate());
        assert!(e.contains(want), "{e}");
    }
    // Two nodes of one identity.
    let mut r = sample();
    r.node_keys = Strings::from_iter(["1", "1", "-3", "1", "Zürich"]);
    assert_eq!(
        message(r.validate()),
        "nodes 0 and 1 are both '1' of the label 'people'"
    );
    sample().validate().unwrap();
}

#[test]
fn strings_hold_text_as_the_wire_does() {
    let mut s = Strings::from_iter(["a", "", "é", "日本"]);
    assert_eq!(s.len(), 4);
    assert_eq!(s.iter().collect::<Vec<_>>(), ["a", "", "é", "日本"]);
    let (offsets, bytes) = s.parts();
    assert_eq!(offsets, [0, 1, 1, 3, 9]);
    assert_eq!(
        Strings::from_parts(offsets.to_vec(), bytes.to_vec()).unwrap(),
        s
    );
    assert!(s.ends_with(&Strings::from_iter(["é", "日本"])));
    assert!(!s.ends_with(&Strings::from_iter(["é日", "本"])));
    s.truncate(2);
    assert_eq!(s, Strings::from_iter(["a", ""]));
    assert!(Strings::new().is_empty());

    let bad = |offsets: &[u32], bytes: &[u8]| {
        message(Strings::from_parts(offsets.to_vec(), bytes.to_vec()))
    };
    assert_eq!(bad(&[0, 1], &[0xff]), "the text is not UTF-8");
    assert!(bad(&[], b"").contains("do not run from 0"));
    assert!(bad(&[1, 2], b"ab").contains("do not run from 0"));
    assert!(bad(&[0, 1], b"ab").contains("do not run from 0"));
    assert!(bad(&[0, 2, 1, 3], b"abc").contains("do not rise"));
    assert!(bad(&[0, 1, 2], "é".as_bytes()).contains("split a character"));
    assert!(bad(&[0, 9, 2], b"ab").contains("split a character"));
}

// Deltas.

/// Deltas that each apply to [`sample`], covering every kind of op.
fn steps(r: &GraphResult) -> Vec<(&'static str, GraphDelta)> {
    let club = &r.node_column("club").unwrap().values;
    let expand = vec![
        Op::AddNodeLabel(NodeLabel::new("pets", KeyType::Text)),
        Op::AddEdgeLabel("owns".into()),
        Op::AddNodes(NodeBatch {
            labels: vec![2, 0],
            keys: Strings::from_iter(["Rex", "40"]),
            columns: vec![
                BatchColumn::new(
                    "club",
                    BatchValues::dict(club, [Some("green"), Some("blue")]),
                ),
                BatchColumn::new("pagerank", BatchValues::F32(vec![0.125, f32::NAN])),
            ],
            positions: Some(Positions {
                x: vec![9.0, 8.0],
                y: vec![7.0, 6.0],
            }),
        }),
        Op::AddEdges(
            EdgeBatch {
                src: vec![6, 6, 0],
                dst: vec![5, 0, 6],
                labels: vec![2, 0, 0],
                rows: Some(vec![0, NO_ROW, 12]),
                columns: vec![],
            }
            .with_column("weight", BatchValues::F32(vec![0.5, 0.25, 0.125])),
        ),
    ];
    vec![
        ("expand", delta(expand)),
        (
            "remove",
            delta(vec![
                Op::RemoveNodes(vec![0, 2]),
                Op::RemoveEdges(vec![3, 0]),
            ]),
        ),
        (
            "restore",
            delta(vec![Op::RestoreNodes(vec![4]), Op::RestoreEdges(vec![1])]),
        ),
        (
            "add a column",
            delta(vec![r.set_column_op(
                Target::Edges,
                Column::new(
                    "kind",
                    Some(Role::Category),
                    Values::dict([Some("a"), None, Some("b"), Some("a")]),
                ),
            )]),
        ),
        (
            "replace a column",
            delta(vec![r.set_column_op(
                Target::Nodes,
                Column::new(
                    "pagerank",
                    Some(Role::Metric),
                    Values::U32(vec![5, 4, 3, 2, 1]),
                ),
            )]),
        ),
        (
            // From the middle of the columns: the undo puts it back there.
            "drop a column",
            delta(vec![
                r.set_encodings_op(Encodings::default()),
                r.drop_column_op(Target::Nodes, "pagerank").unwrap(),
            ]),
        ),
        (
            "highlights",
            delta(vec![r.set_highlights_op(Highlights {
                paths: vec![Path {
                    nodes: vec![2, 1],
                    edges: vec![1],
                }],
                seeds: vec![],
                distance: None,
            })]),
        ),
        (
            "encodings",
            delta(vec![r.set_encodings_op(Encodings {
                size_by: Some("hops".into()),
                color_by: None,
                group_by: Some("club".into()),
                layout: Layout::Layers,
            })]),
        ),
        (
            "positions",
            delta(vec![r.set_positions_op(Some(Positions {
                x: vec![1.0; 5],
                y: vec![f32::NAN; 5],
            }))]),
        ),
        ("no positions", delta(vec![r.set_positions_op(None)])),
        (
            "labels",
            delta(vec![
                Op::AddNodeLabel(NodeLabel::new("things", KeyType::Int)),
                Op::AddEdgeLabel("has".into()),
            ]),
        ),
    ]
}

#[test]
fn a_delta_then_its_inverse_restores_the_result() {
    let base = sample();
    let bytes = base.to_bytes();
    let steps = steps(&base);
    // Every kind of op is in there, forwards or as an inverse.
    let mut names: Vec<&str> = steps
        .iter()
        .flat_map(|(_, d)| {
            d.ops
                .iter()
                .chain(&d.invert().ops)
                .map(Op::name)
                .collect::<Vec<_>>()
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 16, "{names:?}");

    for (name, step) in steps {
        let mut r = base.clone();
        r.apply(&step).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_ne!(r, base, "{name} changed nothing");
        r.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
        let after = r.clone();

        // Undo: the result, its key index and its bytes are as they were.
        let undo = step.invert();
        r.apply(&undo)
            .unwrap_or_else(|e| panic!("undo {name}: {e}"));
        assert_eq!(r, base, "undo {name}");
        assert_eq!(r.index, base.index, "undo {name}");
        assert_eq!(r.to_bytes(), bytes, "undo {name}");

        // Redo: the inverse's inverse is the delta.
        assert_eq!(undo.invert(), step, "{name}");
        r.apply(&step).unwrap();
        assert_eq!(r, after, "redo {name}");
        assert_eq!(r.index, after.index, "redo {name}");
    }
}

#[test]
fn an_expansion_adds_slots_at_the_end_and_fills_the_columns() {
    let mut r = sample();
    let (_, expand) = steps(&r).remove(0);
    r.apply(&expand).unwrap();
    assert_eq!((r.n_nodes(), r.n_edges()), (7, 7));
    assert_eq!(r.node(2, &Key::from("Rex")), Some(5));
    assert_eq!(r.node_key(6), Key::Int(40));
    // 'green' is new to the dictionary, 'blue' was there.
    let club = &r.node_column("club").unwrap().values;
    assert_eq!(
        (club.text(5), club.text(6), club.text(0)),
        (Some("green"), Some("blue"), Some("red"))
    );
    let Values::Dict { values, .. } = club else {
        panic!()
    };
    assert_eq!(
        values.iter().collect::<Vec<_>>(),
        ["red", "blue", "blå", "green"]
    );
    // 'hops' was not in the batch: nulls.
    assert_eq!(
        r.node_column("hops").unwrap().values,
        Values::U32(vec![0, 1, 2, 1, NULL, NULL, NULL])
    );
    assert_eq!(
        (r.edge_row(4), r.edge_row(5), r.edge_row(6)),
        (Some(0), None, Some(12))
    );
    assert_eq!(r.positions().unwrap().x[5..], [9.0, 8.0]);
}

#[test]
fn rows_appear_with_the_first_known_one_and_go_with_the_last() {
    let mut r = build(vec![
        Op::AddNodeLabel(NodeLabel::new("", KeyType::Int)),
        Op::AddEdgeLabel(String::new()),
        Op::AddNodes(NodeBatch::of_label(0, [1i64, 2])),
        Op::AddEdges(EdgeBatch::of_label(0, vec![0], vec![1]).with_rows(vec![NO_ROW])),
    ]);
    assert_eq!(r.edge_rows(), None);
    let base = r.clone();
    let known = delta(vec![Op::AddEdges(
        EdgeBatch::of_label(0, vec![1], vec![0]).with_rows(vec![3]),
    )]);
    r.apply(&known).unwrap();
    assert_eq!(r.edge_rows(), Some(&[NO_ROW, 3][..]));
    let unknown = delta(vec![Op::AddEdges(EdgeBatch::of_label(0, vec![1], vec![1]))]);
    r.apply(&unknown).unwrap();
    assert_eq!(r.edge_rows(), Some(&[NO_ROW, 3, NO_ROW][..]));
    r.apply(&unknown.invert()).unwrap();
    r.apply(&known.invert()).unwrap();
    assert_eq!(r, base);
}

#[test]
fn a_delta_applies_whole_or_not_at_all() {
    let mut r = sample();
    let before = r.clone();
    let (_, mut expand) = steps(&r).remove(0);
    // Four good ops, then one that does not fit.
    expand.ops.push(Op::RemoveNodes(vec![4]));
    assert_eq!(
        message(r.apply(&expand)),
        "op 4 (remove_nodes): node 4 is removed already"
    );
    assert_eq!(r, before);
    assert_eq!(r.index, before.index);
}

#[test]
fn taking_slots_off_the_end_is_strict() {
    let base = sample();
    let (_, expand) = steps(&base).remove(0);
    let mut r = base.clone();
    r.apply(&expand).unwrap();
    let after = r.clone();
    let [add_label, _, add_nodes, add_edges] = &expand.ops[..] else {
        panic!()
    };
    let pop_nodes = add_nodes.invert();
    let pop_edges = add_edges.invert();

    // The nodes cannot go while the edges name them, nor the label while
    // a node has it.
    let e = message(r.apply(&delta(vec![pop_nodes.clone()])));
    assert!(
        e.contains("an edge still names one of the last nodes"),
        "{e}"
    );
    let e = message(r.apply(&delta(vec![add_label.invert()])));
    assert!(e.contains("nodes still have the label 'pets'"), "{e}");

    // Nor slots that are not the op's: other nodes, other values, or more
    // than there are.
    for (op, want) in [
        (
            Op::PopNodes(NodeBatch::of_label(0, [1i64])),
            "the last nodes are not the ones",
        ),
        (
            Op::PopEdges(EdgeBatch::of_label(0, vec![0], vec![6])),
            "the last edges are not the ones",
        ),
        (
            Op::PopEdges(
                EdgeBatch::of_label(0, vec![0], vec![6])
                    .with_rows(vec![12])
                    .with_column("weight", BatchValues::F32(vec![9.0])),
            ),
            "the last edges' values of 'weight' are not the ones",
        ),
        (
            Op::PopEdges(EdgeBatch::of_label(0, vec![0; 9], vec![0; 9])),
            "9 edges to take off, but the result has 7",
        ),
    ] {
        let e = message(r.apply(&delta(vec![op])));
        assert!(e.contains(want), "{e}");
    }

    // Nor a slot that is removed, or that a highlight names.
    r.apply(&delta(vec![Op::RemoveEdges(vec![6])])).unwrap();
    let e = message(r.apply(&delta(vec![pop_edges.clone()])));
    assert!(
        e.contains("one of the last edges is removed; restore it first"),
        "{e}"
    );
    r.apply(&delta(vec![Op::RestoreEdges(vec![6])])).unwrap();
    let seed = delta(vec![r.set_highlights_op(Highlights {
        seeds: vec![6],
        ..Highlights::default()
    })]);
    r.apply(&seed).unwrap();
    let e = message(r.apply(&delta(vec![pop_edges.clone(), pop_nodes.clone()])));
    assert!(
        e.contains("op 1 (pop_nodes): a highlight still names one of the last nodes"),
        "{e}"
    );
    r.apply(&seed.invert()).unwrap();
    assert_eq!(r, after);

    r.apply(&expand.invert()).unwrap();
    assert_eq!(r, base);
}

#[test]
fn a_stale_set_is_refused() {
    let mut r = sample();
    let stale_column = r.set_column_op(
        Target::Nodes,
        Column::new("hops", None, Values::U32(vec![9; 5])),
    );
    let stale_highlights = r.set_highlights_op(Highlights::default());
    let stale_encodings = r.set_encodings_op(Encodings::default());
    let stale_positions = r.set_positions_op(None);
    // The result moves on.
    r.apply(&delta(vec![
        r.set_highlights_op(Highlights {
            seeds: vec![1],
            ..Highlights::default()
        }),
        r.set_column_op(
            Target::Nodes,
            Column::new("hops", None, Values::U32(vec![1; 5])),
        ),
        r.set_encodings_op(Encodings {
            layout: Layout::Layers,
            ..Encodings::default()
        }),
        r.set_positions_op(Some(Positions {
            x: vec![0.0; 5],
            y: vec![0.0; 5],
        })),
    ]))
    .unwrap();
    let before = r.clone();
    for (op, want) in [
        (
            stale_column,
            "node column 1 is not the 'hops' this op replaces",
        ),
        (
            stale_highlights,
            "the highlights are not the ones this op replaces",
        ),
        (
            stale_encodings,
            "the encodings are not the ones this op replaces",
        ),
        (
            stale_positions,
            "the positions are not the ones this op replaces",
        ),
    ] {
        let e = message(r.apply(&delta(vec![op])));
        assert!(e.contains(want), "{e}");
    }
    assert_eq!(r, before);
}

#[test]
fn batches_must_fit_the_columns() {
    let mut r = sample();
    let before = r.clone();
    let one = |name: &str, values| NodeBatch {
        labels: vec![0],
        keys: Strings::from_iter(["50"]),
        columns: vec![BatchColumn::new(name, values)],
        positions: Some(Positions {
            x: vec![0.0],
            y: vec![0.0],
        }),
    };
    for (batch, want) in [
        (
            one("age", BatchValues::U32(vec![1])),
            "the result does not have the node column 'age'; add it first",
        ),
        (
            one("hops", BatchValues::F32(vec![1.0])),
            "the node column 'hops' is u32, but the batch's values are f32",
        ),
        (
            one("hops", BatchValues::U32(vec![1, 2])),
            "the node column 'hops' has 2 values for 1 new nodes",
        ),
        (
            one(
                "club",
                BatchValues::Dict {
                    new_values: Strings::from_iter(["x"]),
                    codes: vec![4],
                },
            ),
            "the node column 'club' has the code 4, but only 4 values",
        ),
        (
            NodeBatch {
                positions: None,
                ..one("hops", BatchValues::U32(vec![1]))
            },
            "the result has positions, but the new nodes have none",
        ),
    ] {
        let e = message(r.apply(&delta(vec![Op::AddNodes(batch)])));
        assert!(e.contains(want), "{e}");
    }
    assert_eq!(r, before);
    // Codes for text the column has and has not.
    let club = &r.node_column("club").unwrap().values;
    assert_eq!(
        BatchValues::dict(
            club,
            [Some("new"), None, Some("blå"), Some("new"), Some("newer")]
        ),
        BatchValues::Dict {
            new_values: Strings::from_iter(["new", "newer"]),
            codes: vec![3, NULL, 2, 3, 4],
        }
    );
}

#[test]
fn a_selection_is_a_sorted_set_of_slots() {
    let r = sample();
    let mut s = Selection::new(vec![3, 1, 3], vec![2]);
    assert_eq!((s.nodes(), s.edges()), (&[1, 3][..], &[2][..]));
    assert!(s.toggle_node(2) && !s.toggle_node(1) && s.toggle_edge(0));
    assert_eq!((s.nodes(), s.edges()), (&[2, 3][..], &[0, 2][..]));
    assert!(s.has_node(3) && !s.has_node(1) && s.has_edge(0));
    s.check(&r).unwrap();
    s.toggle_node(5);
    assert!(message(s.check(&r)).contains("node 5 is selected, but the result has 5 nodes"));
    s.clip(&r);
    assert_eq!(s.nodes(), [2, 3]);
    s.clear();
    assert!(s.is_empty());
}

// The wire format.

/// The header of a buffer as JSON, and its body.
fn split(bytes: &[u8]) -> (Value, Vec<u8>) {
    let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let header = serde_json::from_slice(&bytes[8..8 + len]).unwrap();
    (header, bytes[(8 + len).next_multiple_of(8)..].to_vec())
}

fn join(header: &Value, body: &[u8]) -> Vec<u8> {
    let json = serde_json::to_vec(header).unwrap();
    let mut out = b"UGR1".to_vec();
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out.resize(out.len().next_multiple_of(8), 0);
    out.extend_from_slice(body);
    out
}

/// The sample's bytes with its header, or its body, changed.
fn tampered(change: impl FnOnce(&mut Value, &mut Vec<u8>)) -> Result<GraphResult, UgrError> {
    let (mut header, mut body) = split(&sample().to_bytes());
    change(&mut header, &mut body);
    GraphResult::from_bytes(&join(&header, &body))
}

fn section<'a>(header: &'a mut Value, name: &str) -> &'a mut Value {
    let sections = header["sections"].as_array_mut().unwrap();
    sections.iter_mut().find(|s| s["name"] == name).unwrap()
}

#[test]
fn a_result_round_trips_bit_for_bit() {
    let r = sample();
    let bytes = r.to_bytes();
    let back = GraphResult::from_bytes(&bytes).unwrap();
    assert_eq!(back, r);
    assert_eq!(back.index, r.index);
    assert_eq!(back.to_bytes(), bytes);
    // NaN, -0.0 and the nulls came through as the bits they were.
    let Values::F32(pagerank) = &back.node_column("pagerank").unwrap().values else {
        panic!()
    };
    assert_eq!(pagerank[2].to_bits(), f32::NAN.to_bits());
    assert_eq!(pagerank[4].to_bits(), (-0.0f32).to_bits());
    // The same after every kind of step, and at any address.
    for (name, step) in steps(&r) {
        let mut stepped = r.clone();
        stepped.apply(&step).unwrap();
        let bytes = stepped.to_bytes();
        for shift in 0..8 {
            let mut moved = vec![0u8; shift];
            moved.extend_from_slice(&bytes);
            assert_eq!(
                GraphResult::from_bytes(&moved[shift..]).unwrap(),
                stepped,
                "{name}"
            );
        }
    }
}

#[test]
fn a_delta_round_trips_bit_for_bit() {
    let r = sample();
    for (name, step) in steps(&r) {
        for d in [
            step.invert(),
            step.with_provenance(Provenance {
                sql: Some("SELECT 1".into()),
                rule: None,
                ..Provenance::default()
            }),
        ] {
            let bytes = d.to_bytes();
            let back = GraphDelta::from_bytes(&bytes).unwrap();
            assert_eq!(back, d, "{name}");
            assert_eq!(back.to_bytes(), bytes, "{name}");
            // And it still applies after the trip.
            if d.provenance.sql.is_some() {
                r.clone().apply(&back).unwrap();
            }
        }
    }
    let bytes = steps(&r).remove(0).1.to_bytes();
    assert_eq!(
        message(GraphResult::from_bytes(&bytes)),
        "UGR: the buffer holds a delta, not a result"
    );
    assert_eq!(
        message(GraphDelta::from_bytes(&r.to_bytes())),
        "UGR: the buffer holds a result, not a delta"
    );
}

#[test]
fn the_layout_is_the_specified_one() {
    let bytes = sample().to_bytes();
    assert_eq!(&bytes[..4], b"UGR1");
    assert_eq!(bytes.len() % 8, 0);
    let (header, body) = split(&bytes);
    assert_eq!(header["kind"], "result");
    assert_eq!(header["body"], body.len());
    assert_eq!(header["nodes"]["count"], 5);
    assert_eq!(
        header["nodes"]["labels"][1],
        serde_json::json!({"name": "places", "key": "text"})
    );
    assert_eq!(
        header["nodes"]["columns"][2],
        serde_json::json!({"name": "club", "type": "dict", "role": "category"})
    );
    assert_eq!(
        header["edges"]["labels"],
        serde_json::json!(["knows", "lives_in"])
    );
    assert_eq!(
        header["highlights"],
        serde_json::json!({"paths": 2, "seeds": 2, "distance": "hops"})
    );
    assert_eq!(header["encodings"]["layout"], "rings");
    assert_eq!(header["provenance"]["rule"], "edges");

    // Sections: in order, 8-aligned, zero padding between, none past the end.
    let mut end = 0;
    for s in header["sections"].as_array().unwrap() {
        let offset = s["offset"].as_u64().unwrap() as usize;
        let size = match s["type"].as_str().unwrap() {
            "u8" => 1,
            "u16" => 2,
            "u32" | "f32" => 4,
            "u64" => 8,
            other => panic!("{other}"),
        };
        assert_eq!(offset % 8, 0, "{s}");
        assert!(offset >= end && offset - end < 8, "{s}");
        assert!(body[end..offset].iter().all(|&b| b == 0), "{s}");
        end = offset + s["count"].as_u64().unwrap() as usize * size;
    }
    assert_eq!(end.next_multiple_of(8), body.len());

    // Little-endian, whatever the host: edge 3 is 2 → 4, and edge 2's row is 7.
    let at = |header: &Value, name: &str| {
        let s = header["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap()
            .clone();
        s["offset"].as_u64().unwrap() as usize
    };
    let src = at(&header, "edges.src");
    assert_eq!(body[src + 12..src + 16], [2, 0, 0, 0]);
    let rows = at(&header, "edges.rows");
    assert_eq!(body[rows + 16..rows + 24], [7, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(body[rows + 24..rows + 32], [0xff; 8]);
    let keys = at(&header, "nodes.keys.bytes");
    assert_eq!(&body[keys..keys + 12], "12-31Zürich".as_bytes());
    let pagerank = at(&header, "nodes.columns.0");
    assert_eq!(body[pagerank..pagerank + 4], 0.5f32.to_le_bytes());

    // Labels and flags that are all 0 are not written.
    let plain = build(vec![
        Op::AddNodeLabel(NodeLabel::new("", KeyType::Int)),
        Op::AddEdgeLabel(String::new()),
        Op::AddNodes(NodeBatch::of_label(0, [1i64, 2])),
        Op::AddEdges(EdgeBatch::of_label(0, vec![0], vec![1])),
    ]);
    let (header, _) = split(&plain.to_bytes());
    let names: Vec<&str> = header["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "nodes.keys.offsets",
            "nodes.keys.bytes",
            "edges.src",
            "edges.dst",
            "highlights.paths.offsets",
            "highlights.paths.nodes",
            "highlights.paths.edge_offsets",
            "highlights.paths.edges",
            "highlights.seeds",
        ]
    );
}

#[test]
fn a_truncated_buffer_is_refused_with_a_message() {
    let bytes = sample().to_bytes();
    let (header, body) = split(&bytes);
    let header_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    assert_eq!(
        message(GraphResult::from_bytes(&bytes[..5])),
        "UGR: the buffer is truncated: 5 bytes, and the magic and the header's length take 8"
    );
    assert_eq!(
        message(GraphResult::from_bytes(&bytes[..100])),
        format!("UGR: the buffer is truncated: its header is {header_len} bytes, and 92 follow")
    );
    assert_eq!(
        message(GraphResult::from_bytes(&bytes[..bytes.len() - 8])),
        format!(
            "UGR: the buffer is truncated: its body is {} bytes, and {} follow the header",
            body.len(),
            body.len() - 8
        )
    );
    assert_eq!(header["body"], body.len());
    // Cut anywhere, it is an error and never a panic. So is a delta.
    let delta_bytes = steps(&sample()).remove(0).1.to_bytes();
    for cut in 0..bytes.len() {
        assert!(
            GraphResult::from_bytes(&bytes[..cut]).is_err(),
            "cut at {cut}"
        );
    }
    for cut in 0..delta_bytes.len() {
        assert!(
            GraphDelta::from_bytes(&delta_bytes[..cut]).is_err(),
            "cut at {cut}"
        );
    }
}

#[test]
fn a_malformed_buffer_is_refused_with_a_message() {
    let bytes = sample().to_bytes();
    let with = |change: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = bytes.clone();
        change(&mut bytes);
        message(GraphResult::from_bytes(&bytes))
    };
    assert_eq!(
        with(&|b| b[0] = b'P'),
        "UGR: not a UGR buffer: it does not start with \"UGR1\""
    );
    assert_eq!(
        with(&|b| b[3] = b'2'),
        "UGR: version '2', and this reader reads version 1"
    );
    assert_eq!(
        with(&|b| b.extend_from_slice(&[0; 8])),
        "UGR: 8 bytes follow the body"
    );
    assert!(with(&|b| b[8] = b'[').starts_with("UGR: the header: "));
    assert!(with(&|b| b[20] = 0xff).starts_with("UGR: the header: "));

    type Change = Box<dyn Fn(&mut Value, &mut Vec<u8>)>;
    let cases: Vec<(Change, &str)> = vec![
        (
            Box::new(|h, _| h["kind"] = "table".into()),
            "the header: unknown variant `table`",
        ),
        (
            Box::new(|h, _| drop(h.as_object_mut().unwrap().remove("edges"))),
            "the header: missing field `edges`",
        ),
        (
            Box::new(|h, _| h["nodes"]["labels"][0]["key"] = "float".into()),
            "the header: unknown variant `float`",
        ),
        (
            Box::new(|h, _| h["nodes"]["columns"][0]["role"] = "weight".into()),
            "the header: unknown variant `weight`",
        ),
        (
            Box::new(|h, _| h["nodes"]["columns"][0]["type"] = "f64".into()),
            "'nodes.columns.0' has the unknown column type 'f64'",
        ),
        (
            Box::new(|h, _| h["nodes"]["count"] = 6.into()),
            "'nodes.keys' has 5 elements, and the header says 6",
        ),
        (
            Box::new(|h, _| h["edges"]["count"] = 3.into()),
            "'edges.src' has 4 elements, and the header says 3",
        ),
        (
            Box::new(|h, _| h["highlights"]["paths"] = 3.into()),
            "'highlights.paths' has 3 and 3 offsets for 3 paths",
        ),
        (
            Box::new(|h, _| h["highlights"]["seeds"] = 1.into()),
            "'highlights.seeds' has 2 elements, and the header says 1",
        ),
        (
            Box::new(|h, _| section(h, "edges.dst")["name"] = "edges.src".into()),
            "two sections are named 'edges.src'",
        ),
        (
            Box::new(|h, _| section(h, "edges.dst")["name"] = "edges.to".into()),
            "the section 'edges.dst' is missing",
        ),
        (
            Box::new(|h, _| section(h, "edges.dst")["type"] = "f32".into()),
            "the section 'edges.dst' is f32, not u32",
        ),
        (
            Box::new(|h, _| section(h, "edges.dst")["offset"] = 4.into()),
            "the section 'edges.dst' starts at 4, not a multiple of 8",
        ),
        (
            Box::new(|h, _| section(h, "edges.dst")["count"] = 4000.into()),
            "the section 'edges.dst' (4000 u32 at ",
        ),
        (
            Box::new(|h, _| section(h, "edges.dst")["count"] = u64::MAX.into()),
            "runs past the body",
        ),
        (
            Box::new(|h, _| section(h, "edges.dst")["offset"] = (u64::MAX - 7).into()),
            "runs past the body",
        ),
        (
            Box::new(|h, _| section(h, "nodes.flags")["count"] = 4.into()),
            "'nodes.flags' has 4 elements, and the header says 5",
        ),
        (
            Box::new(|h, _| section(h, "nodes.x")["count"] = 4.into()),
            "5 nodes, but 4 x and 5 y positions",
        ),
        (
            Box::new(|h, _| section(h, "nodes.columns.1")["count"] = 4.into()),
            "the node column 'hops' has 4 values, for 5 rows",
        ),
        (
            Box::new(|h, _| section(h, "nodes.keys.offsets")["count"] = 0.into()),
            "'nodes.keys': the text's offsets do not run from 0",
        ),
        (
            Box::new(|h, body| {
                let at = section(h, "nodes.keys.bytes")["offset"].as_u64().unwrap() as usize;
                body[at + 6] = 0xff;
            }),
            "'nodes.keys': the text is not UTF-8",
        ),
        (
            // Node 1's key becomes node 0's: two of one identity.
            Box::new(|h, body| {
                let at = section(h, "nodes.keys.bytes")["offset"].as_u64().unwrap() as usize;
                body[at + 1] = b'1';
            }),
            "nodes 0 and 1 are both '1' of the label 'people'",
        ),
        (
            Box::new(|h, body| {
                let at = section(h, "edges.src")["offset"].as_u64().unwrap() as usize;
                body[at] = 200;
            }),
            "edge 0 names node 200, but the result has 5 nodes",
        ),
        (
            Box::new(|h, body| {
                let at = section(h, "nodes.columns.2")["offset"].as_u64().unwrap() as usize;
                body[at] = 3;
            }),
            "the node column 'club' has the code 3, but only 3 values",
        ),
        (
            Box::new(|h, body| {
                let at = section(h, "highlights.paths.offsets")["offset"]
                    .as_u64()
                    .unwrap() as usize;
                body[at + 4] = 9;
            }),
            "'highlights.paths': the offsets of path 0 are out of range",
        ),
        (
            Box::new(|h, body| {
                let at = section(h, "edges.rows")["offset"].as_u64().unwrap() as usize;
                body[at + 16..at + 24].fill(0xff);
            }),
            "'edges.rows' is present, but no row is known",
        ),
        (
            Box::new(|h, _| h["encodings"]["color_by"] = "gone".into()),
            "there is no node column 'gone' to colour by",
        ),
    ];
    for (change, want) in cases {
        let e = message(tampered(change));
        assert!(
            e.starts_with("UGR: ") && e.contains(want),
            "{e}\n  wanted: {want}"
        );
    }
    // An untouched split and join is the buffer itself.
    assert_eq!(tampered(|_, _| {}).unwrap(), sample());
    // A header field this version does not know is ignored.
    assert_eq!(tampered(|h, _| h["future"] = 1.into()).unwrap(), sample());
}

#[test]
fn no_changed_byte_makes_the_decoder_panic() {
    let r = sample();
    for bytes in [r.to_bytes(), steps(&r).remove(0).1.to_bytes()] {
        for at in 0..bytes.len() {
            for flip in [0x01, 0x80, 0xff] {
                let mut changed = bytes.clone();
                changed[at] ^= flip;
                // Ok or Err, either is fine: only a panic is not.
                let _ = ugr::decode(&changed);
            }
        }
    }
}

#[test]
fn the_last_nodes_come_off_only_when_no_edge_names_them_whatever_came_and_went() {
    // What the result keeps to answer that without reading its edges.
    let mut r = build(vec![
        Op::AddNodeLabel(NodeLabel::new("n", KeyType::Int)),
        Op::AddEdgeLabel("e".into()),
        Op::AddNodes(NodeBatch::of_label(0, 0..6i64)),
        Op::AddEdges(EdgeBatch::of_label(0, vec![0, 3, 1, 5], vec![1, 0, 2, 4])),
    ]);
    assert_eq!(r.named, [(0, 1), (1, 3), (3, 5)]);
    let pop_node = |key: i64| delta(vec![Op::PopNodes(NodeBatch::of_label(0, [key]))]);
    let pop_edge = |s, d| delta(vec![Op::PopEdges(EdgeBatch::of_label(0, vec![s], vec![d]))]);
    assert!(message(r.apply(&pop_node(5))).contains("an edge still names"));
    r.apply(&pop_edge(5, 4)).unwrap();
    assert_eq!(r.named, [(0, 1), (1, 3)]);
    r.apply(&pop_node(5)).unwrap();
    r.apply(&pop_node(4)).unwrap();
    // Node 3 is named by edge 1, under an edge that names less.
    assert!(message(r.apply(&pop_node(3))).contains("an edge still names"));
    // An edge to a node already named adds nothing; one past it does.
    r.apply(&delta(vec![Op::AddEdges(EdgeBatch::of_label(
        0,
        vec![2, 2],
        vec![3, 2],
    ))]))
    .unwrap();
    assert_eq!(r.named, [(0, 1), (1, 3)]);
    // Over the wire, it is derived again.
    let back = GraphResult::from_bytes(&r.to_bytes()).unwrap();
    assert_eq!(back.named, r.named);
}

// #501: what a result holds, and how it is read.

#[test]
fn labels_and_flags_that_are_all_zero_hold_nothing() {
    let mut r = build(vec![
        Op::AddNodeLabel(NodeLabel::new("", KeyType::Int)),
        Op::AddEdgeLabel(String::new()),
        Op::AddNodes(NodeBatch::of_label(0, [1i64, 2, 3])),
        Op::AddEdges(EdgeBatch::of_label(0, vec![0, 1, 2], vec![1, 2, 0])),
    ]);
    let lazy = |r: &GraphResult| {
        (
            r.edge_label().as_slice().is_none(),
            r.edge_flags().as_slice().is_none(),
        )
    };
    assert_eq!(lazy(&r), (true, true));
    assert_eq!(r.edge_label().to_vec(), [0, 0, 0]);
    assert_eq!((r.edge_label()[2], r.edge_flags().get(1)), (0, 0));
    assert!(r.edge_label().contains(&0) && !r.edge_label().contains(&1));
    let bytes = r.to_bytes();
    let back = GraphResult::from_bytes(&bytes).unwrap();
    assert_eq!(lazy(&back), (true, true));
    assert_eq!(back, r);

    // Restoring an edge that is not removed is refused, and still holds
    // nothing; removing one holds the flags, and the result is the same
    // one after its undo, held or not.
    let restore = delta(vec![Op::RestoreEdges(vec![1])]);
    assert_eq!(
        message(r.apply(&restore)),
        "op 0 (restore_edges): edge 1 is not removed"
    );
    let past = delta(vec![Op::RestoreEdges(vec![9])]);
    assert_eq!(
        message(r.apply(&past)),
        "op 0 (restore_edges): edge 9, but the result has 3 edges"
    );
    assert_eq!(lazy(&r), (true, true));
    let remove = delta(vec![Op::RemoveEdges(vec![1])]);
    r.apply(&remove).unwrap();
    assert_eq!(lazy(&r), (true, false));
    assert!(r.edge_is_removed(1) && !r.edge_is_shown(1) && r.edge_is_shown(0));
    assert_ne!(r, back);
    r.apply(&remove.invert()).unwrap();
    assert_eq!(r, back);
    assert_eq!(r.to_bytes(), bytes);

    // A second edge label is held from the first edge that has it, and
    // goes with the last.
    let more = delta(vec![
        Op::AddEdgeLabel("other".into()),
        Op::AddEdges(EdgeBatch::of_label(1, vec![0], vec![2])),
    ]);
    r.apply(&more).unwrap();
    assert_eq!(r.edge_label().to_vec(), [0, 0, 0, 1]);
    assert_eq!(
        GraphResult::from_bytes(&r.to_bytes())
            .unwrap()
            .edge_label()
            .to_vec(),
        [0, 0, 0, 1]
    );
    r.apply(&more.invert()).unwrap();
    assert_eq!(r, back);
    assert_eq!(r.to_bytes(), bytes);

    // Labels held from the first edge there is, and zeros after them.
    let mut labelled = EdgeBatch::of_label(0, vec![0, 1, 2, 0], vec![1, 2, 0, 2]);
    labelled.labels = vec![0, 1, 0, 1];
    let mut two = build(vec![
        Op::AddNodeLabel(NodeLabel::new("", KeyType::Int)),
        Op::AddEdgeLabel("a".into()),
        Op::AddEdgeLabel("b".into()),
        Op::AddNodes(NodeBatch::of_label(0, [1i64, 2, 3])),
        Op::AddEdges(labelled),
    ]);
    assert_eq!(two.edge_label().to_vec(), [0, 1, 0, 1]);
    let zeros = delta(vec![Op::AddEdges(EdgeBatch::of_label(0, vec![1], vec![0]))]);
    two.apply(&zeros).unwrap();
    assert_eq!(two.edge_label().to_vec(), [0, 1, 0, 1, 0]);
    assert_eq!(two.edge_label()[3], 1);
    let read = GraphResult::from_bytes(&two.to_bytes()).unwrap();
    assert_eq!(read, two);
    assert_eq!(read.edge_label().to_vec(), [0, 1, 0, 1, 0]);
    two.apply(&zeros.invert()).unwrap();
    assert_eq!(two.edge_label().to_vec(), [0, 1, 0, 1]);
}

/// Bytes that are not this program's slice: read a range at a time, and
/// counted.
struct Elsewhere<'a> {
    bytes: &'a [u8],
    read: std::cell::Cell<usize>,
}

impl Source for Elsewhere<'_> {
    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn read(&self, at: usize, out: &mut [u8]) {
        self.read.set(self.read.get() + out.len());
        out.copy_from_slice(&self.bytes[at..at + out.len()]);
    }
}

#[test]
fn a_result_is_read_from_any_source_with_room_and_is_the_same() {
    let r = sample();
    let bytes = r.to_bytes();
    let source = Elsewhere {
        bytes: &bytes,
        read: std::cell::Cell::new(0),
    };
    let room = |nodes: usize, edges: usize| {
        assert_eq!((nodes, edges), (r.n_nodes(), r.n_edges()));
        Room { nodes: 3, edges: 2 }
    };
    let mut pace = crate::pace::Pace::never();
    let read = crate::pace::ready(GraphResult::read_paced(&source, &room, &mut pace)).unwrap();
    assert_eq!(read, r);
    assert_eq!(read.index, r.index);
    assert_eq!(read.to_bytes(), bytes);
    // Each byte at most once: the frame, the header and the sections.
    assert!(source.read.get() <= bytes.len());
    // The room is there: three more nodes and two more edges move nothing.
    assert!(read.src.capacity() >= r.n_edges() + 2);
    assert!(read.dst.capacity() >= r.n_edges() + 2);
    assert!(read.node_label.capacity() >= r.n_nodes() + 3);

    // A delta is not a result, from a source either.
    let step = delta(vec![Op::RemoveNodes(vec![0])]).to_bytes();
    let source = Elsewhere {
        bytes: &step,
        read: std::cell::Cell::new(0),
    };
    let refused = crate::pace::ready(GraphResult::read_paced(&source, &room, &mut pace));
    assert_eq!(
        message(refused),
        "UGR: the buffer holds a delta, not a result"
    );
    // A header that claims more nodes than its sections hold gets no room
    // for them.
    let lying = |nodes: usize, _: usize| Room {
        nodes: usize::MAX / 16,
        edges: nodes,
    };
    let source = Elsewhere {
        bytes: &bytes,
        read: std::cell::Cell::new(0),
    };
    let read = crate::pace::ready(GraphResult::read_paced(&source, &lying, &mut pace)).unwrap();
    assert_eq!(read, r);
}

#[test]
fn a_result_can_carry_its_adjacency_for_a_layout() {
    use ursa_core::topology::UndirectedCsr;
    let r = build(vec![
        Op::AddNodeLabel(NodeLabel::new("", KeyType::Int)),
        Op::AddEdgeLabel(String::new()),
        Op::AddNodes(NodeBatch::of_label(0, [10i64, 20, 30, 40])),
        // A repeat, both directions, a self-loop, and a node with no edge.
        Op::AddEdges(EdgeBatch::of_label(
            0,
            vec![0, 1, 1, 2, 0],
            vec![1, 0, 2, 2, 1],
        )),
    ]);
    let plain = r.to_bytes();
    assert!(ugr::read_adjacency(&plain).unwrap().is_none());

    let adjacency = UndirectedCsr::from_edges(r.n_nodes(), r.src(), r.dst());
    assert_eq!(adjacency.offsets, [0, 1, 3, 4, 4]);
    assert_eq!(adjacency.targets, [1, 0, 2, 1]);
    let bytes = r.to_bytes_with_adjacency(&adjacency);
    let read = ugr::read_adjacency(&bytes).unwrap().unwrap();
    assert_eq!(
        (read.offsets, read.targets),
        (adjacency.offsets.clone(), adjacency.targets.clone())
    );
    // The result is the same with or without it, and is written without.
    let back = GraphResult::from_bytes(&bytes).unwrap();
    assert_eq!(back, r);
    assert_eq!(back.to_bytes(), plain);

    // An adjacency that is not one is refused when it is read.
    let broken = UndirectedCsr {
        offsets: vec![0, 1, 3, 4, 4],
        targets: vec![1, 0, 2, 3],
    };
    let bytes = r.to_bytes_with_adjacency(&broken);
    let refused = message(ugr::read_adjacency(&bytes));
    assert_eq!(
        refused,
        "UGR: node 1 has the neighbour 2, but node 2 does not have 1"
    );
    assert_eq!(GraphResult::from_bytes(&bytes).unwrap(), r);
}

#[test]
fn the_key_index_reads_the_results_own_keys() {
    let r = sample();
    let n = r.n_nodes();
    assert_eq!(r.index.len(), n);
    assert_eq!(r.index.nodes(), (0..n as u32).collect::<Vec<_>>());
    // A key of the other type than its label's names no node.
    assert_eq!(r.node(0, &Key::Int(1)), Some(0));
    assert_eq!(r.node(0, &Key::Text("1".into())), None);
    assert_eq!(r.node(1, &Key::Text("1".into())), Some(3));
    assert_eq!(r.node(1, &Key::Int(1)), None);
    assert_eq!(r.node(7, &Key::Int(1)), None);
    // A refused batch leaves the keys, the labels and the index as they were.
    let mut r = build(vec![
        Op::AddNodeLabel(NodeLabel::new("people", KeyType::Int)),
        Op::AddNodes(NodeBatch::of_label(0, [1i64, 2, 3])),
    ]);
    let before = r.clone();
    let twice = delta(vec![Op::AddNodes(NodeBatch::of_label(0, [50i64, 51, 50]))]);
    assert_eq!(
        message(r.apply(&twice)),
        "op 0 (add_nodes): '50' of the label 'people' is node 3 already"
    );
    assert_eq!(r, before);
    assert_eq!(r.index, before.index);
    assert_eq!(r.node(0, &Key::Int(51)), None);
    assert_eq!(r.to_bytes(), before.to_bytes());
}
