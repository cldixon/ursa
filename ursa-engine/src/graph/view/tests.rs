//! `graph::view`'s unit tests: the rules, the ids, the clauses and the
//! styles (moved here from `ursa-wasm`, #425), and the result they become.

use std::sync::Arc;

use datafusion::arrow::array::{
    ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray, UInt64Array,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::Result;
use ursa_viz::model::{GraphResult, Key, KeyType, Layout, Provenance, Role, Values, NULL};

use super::build::{build, from_shape, EdgeList};
use super::rules::{endpoint_columns, read, read_as, shape_of, MatchEdge, Reading, Shape};
use super::style::{color_by, size_by, OTHER};
use super::*;

/// The edges of a result (`schema`, `batches`), from its endpoint columns.
fn from_result(schema: &Schema, batches: &[RecordBatch]) -> Result<EdgeList> {
    Ok(from_shape(shape_of(schema)?, batches)?.0)
}

fn batch(columns: Vec<(&str, ArrayRef)>) -> RecordBatch {
    RecordBatch::try_from_iter(columns).unwrap()
}

#[test]
fn ids_of_any_type_become_dense_indexes_in_first_seen_order() {
    let b = batch(vec![
        (
            "a",
            Arc::new(StringArray::from(vec!["x", "y", "z"])) as ArrayRef,
        ),
        (
            "b",
            Arc::new(StringArray::from(vec!["y", "z", "x"])) as ArrayRef,
        ),
    ]);
    let g = from_result(&b.schema(), std::slice::from_ref(&b)).unwrap();
    assert_eq!(g.n, 3);
    assert_eq!(g.src, vec![0, 1, 2]);
    assert_eq!(g.dst, vec![1, 2, 0]);
    assert_eq!(g.ids, vec!["x", "y", "z"]);
    // The same result, the same graph.
    assert_eq!(from_result(&b.schema(), &[b]).unwrap(), g);
}

#[test]
fn src_and_dst_columns_are_used_wherever_they_are() {
    let b = batch(vec![
        ("weight", Arc::new(Int64Array::from(vec![5, 6])) as ArrayRef),
        ("DST", Arc::new(Int64Array::from(vec![30, 10])) as ArrayRef),
        ("Src", Arc::new(Int64Array::from(vec![10, 20])) as ArrayRef),
    ]);
    let g = from_result(&b.schema(), &[b]).unwrap();
    assert_eq!(g.ids, vec!["10", "30", "20"]);
    assert_eq!((g.src, g.dst), (vec![0, 2], vec![1, 0]));
}

#[test]
fn nulls_are_skipped_and_mixed_types_compare_as_text() {
    let b = batch(vec![
        (
            "src",
            Arc::new(Int64Array::from(vec![Some(1), None, Some(2)])) as ArrayRef,
        ),
        (
            "dst",
            Arc::new(StringArray::from(vec!["2", "3", "1"])) as ArrayRef,
        ),
    ]);
    let g = from_result(&b.schema(), &[b]).unwrap();
    assert_eq!(g.skipped, 1);
    assert_eq!(g.n, 2);
    assert_eq!((g.src, g.dst), (vec![0, 1], vec![1, 0]));
}

#[test]
fn what_cannot_be_drawn_says_why() {
    let one = Schema::new(vec![Field::new("x", DataType::Int64, true)]);
    let err = from_result(&one, &[]).unwrap_err().to_string();
    assert!(err.contains("it has 1 column (x: Int64)"), "{err}");
    assert!(err.contains(".graph AS nodes(x)"), "{err}");
    let two = Arc::new(Schema::new(vec![
        Field::new("src", DataType::Int64, true),
        Field::new("dst", DataType::Int64, true),
    ]));
    let err = from_result(&two, &[]).unwrap_err().to_string();
    assert!(err.contains("no rows"), "{err}");
    let nulls = RecordBatch::try_new(
        Arc::clone(&two),
        vec![
            Arc::new(Int64Array::from(vec![None::<i64>])),
            Arc::new(Int64Array::from(vec![Some(1)])),
        ],
    )
    .unwrap();
    let err = from_result(&two, &[nulls]).unwrap_err().to_string();
    assert!(err.contains("NULL"), "{err}");
    let big = Arc::new(Int64Array::from_iter_values(0..(MAX_EDGES as i64 + 1))) as ArrayRef;
    let err = build(&[], &[(Arc::clone(&big), big)])
        .unwrap_err()
        .to_string();
    assert!(err.contains("2,000,001 edges"), "{err}");
    assert!(err.contains("at most 2,000,000"), "{err}");
}

fn schema_of(columns: &[(&str, DataType)]) -> Schema {
    Schema::new(
        columns
            .iter()
            .map(|(name, t)| Field::new(*name, t.clone(), true))
            .collect::<Vec<_>>(),
    )
}

fn refusal(columns: &[(&str, DataType)]) -> String {
    endpoint_columns(&schema_of(columns))
        .unwrap_err()
        .to_string()
}

fn list_of(t: DataType) -> DataType {
    DataType::List(Arc::new(Field::new("item", t, true)))
}

#[test]
fn a_score_is_not_a_node() {
    // SELECT * FROM pagerank(karate): id, then the score.
    let err = refusal(&[("id", DataType::Int64), ("pagerank", DataType::Float64)]);
    assert!(
        err.contains("the first two columns (id: Int64, pagerank: Float64)"),
        "{err}"
    );
    assert!(
        err.contains("'pagerank' is a number that is not whole (Float64)"),
        "{err}"
    );
    assert!(err.contains(".graph NAME SIZE BY pagerank"), "{err}");
    assert!(err.contains("AS src, … AS dst"), "{err}");
    // The hint names the algorithm the result is of, wherever its column is.
    let err = refusal(&[
        ("id", DataType::Int64),
        ("score", DataType::Float32),
        ("louvain", DataType::Int64),
    ]);
    assert!(err.contains(".graph NAME SIZE BY louvain"), "{err}");
    // Floats in both places, and a decimal with a fraction.
    let err = refusal(&[("a", DataType::Float64), ("b", DataType::Float32)]);
    assert!(
        err.contains("'a' is a number") && err.contains("and 'b' is a number"),
        "{err}"
    );
    let err = refusal(&[("a", DataType::Int64), ("b", DataType::Decimal128(10, 2))]);
    assert!(err.contains("'b' is a number that is not whole"), "{err}");
}

#[test]
fn a_path_a_flag_and_a_struct_are_not_nodes() {
    // COLUMNS (path_length(p), vertices(p), edges(p)), and its neighbours.
    let err = refusal(&[
        ("path_length(p)", DataType::Int64),
        ("vertices(p)", list_of(DataType::Int64)),
    ]);
    assert!(err.contains("'vertices(p)' is a list (List("), "{err}");
    let err = refusal(&[
        (
            "a",
            DataType::LargeList(Arc::new(Field::new("item", DataType::Utf8, true))),
        ),
        ("b", DataType::Utf8),
    ]);
    assert!(err.contains("'a' is a list"), "{err}");
    let err = refusal(&[("id", DataType::Int64), ("member", DataType::Boolean)]);
    assert!(err.contains("'member' is a boolean (Boolean)"), "{err}");
    let point = DataType::Struct(vec![Field::new("x", DataType::Int64, true)].into());
    let err = refusal(&[("id", DataType::Int64), ("at", point)]);
    assert!(err.contains("'at' is a struct"), "{err}");
}

#[test]
fn the_first_two_columns_are_ids_of_one_kind() {
    // An id and a name are not an edge.
    let err = refusal(&[("id", DataType::Int64), ("name", DataType::Utf8)]);
    assert!(
        err.contains("'id' holds integer ids and 'name' holds string ids"),
        "{err}"
    );
    // Ids of one kind draw as before, whatever their width.
    for (a, b) in [
        (DataType::Int64, DataType::Int64),
        (DataType::Int32, DataType::UInt64),
        (DataType::Utf8, DataType::Utf8View),
        (DataType::Date32, DataType::Date32),
        (DataType::Decimal128(38, 0), DataType::Int64),
        (
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            DataType::Utf8,
        ),
    ] {
        let columns = [("a", a), ("b", b), ("score", DataType::Float64)];
        assert_eq!(endpoint_columns(&schema_of(&columns)).unwrap(), (0, 1));
    }
}

#[test]
fn src_and_dst_must_be_ids_too() {
    let err = refusal(&[
        ("src", DataType::Int64),
        ("dst", DataType::Float64),
        ("id", DataType::Int64),
    ]);
    assert!(
        err.contains("cannot draw columns src and dst as edges"),
        "{err}"
    );
    assert!(
        err.contains("'dst' is a number that is not whole (Float64)"),
        "{err}"
    );
    let err = refusal(&[
        ("x", DataType::Int64),
        ("DST", DataType::Int64),
        ("Src", list_of(DataType::Int64)),
    ]);
    assert!(err.contains("'Src' is a list"), "{err}");
    // Named ends of two kinds are still a pair: their names say so, and
    // their ids compare as text.
    let mixed = schema_of(&[
        ("w", DataType::Float64),
        ("dst", DataType::Utf8),
        ("src", DataType::Int64),
    ]);
    assert_eq!(endpoint_columns(&mixed).unwrap(), (2, 1));
    // One of the two names alone is not the pair: the first two columns are.
    let err = refusal(&[("src", DataType::Int64), ("pagerank", DataType::Float64)]);
    assert!(err.contains("the first two columns"), "{err}");
}

#[test]
fn vertex_keys_come_first_so_lone_nodes_are_drawn() {
    let keys = Arc::new(Int64Array::from(vec![3, 2, 1, 9])) as ArrayRef;
    let s = Arc::new(Int64Array::from(vec![1, 2])) as ArrayRef;
    let d = Arc::new(Int64Array::from(vec![2, 3])) as ArrayRef;
    let g = build(&[keys], &[(s, d)]).unwrap();
    assert_eq!(g.n, 4);
    assert_eq!(g.ids, vec!["3", "2", "1", "9"]);
    assert_eq!((g.src, g.dst), (vec![2, 1], vec![1, 0]));
}

fn parsed(text: &str) -> (String, Style) {
    parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn name(ident: &str, shown: &str) -> Option<Name> {
    Some(Name {
        ident: ident.into(),
        shown: shown.into(),
    })
}

#[test]
fn clauses_follow_the_source_in_any_order() {
    assert_eq!(parsed("karate"), ("karate".into(), Style::default()));
    assert_eq!(parsed(""), ("".into(), Style::default()));
    let (source, style) = parsed("karate SIZE BY PageRank color by louvain;");
    assert_eq!(source, "karate");
    assert_eq!(style.size, name("pagerank", "PageRank"));
    assert_eq!(style.color, name("louvain", "louvain"));
    assert_eq!(style.nodes, None);
    // The last result, a quoted name, COLOUR, NODES first.
    let (source, style) =
        parsed("NODES (SELECT id, (x + 1) AS \"Big X\" FROM t) COLOUR BY \"Big X\"");
    assert_eq!(source, "");
    assert_eq!(
        style.nodes.as_deref(),
        Some("SELECT id, (x + 1) AS \"Big X\" FROM t")
    );
    assert_eq!(style.color, name("Big X", "\"Big X\""));
    // A query keeps its own BYs, and words in strings, comments and
    // parentheses are its own.
    let q = "SELECT src, dst FROM e WHERE note <> 'SIZE BY x' -- COLOR BY y\n\
             AND src IN (SELECT a FROM nodes (1)) GROUP BY src, dst ORDER BY src";
    assert_eq!(parsed(q), (q.into(), Style::default()));
    let (source, style) = parsed(&format!("{q} SIZE BY degree"));
    assert_eq!(source, q);
    assert_eq!(style.size, name("degree", "degree"));
}

#[test]
fn a_clause_that_does_not_fit_says_why() {
    for (text, says) in [
        (
            "karate SIZE BY",
            "SIZE BY needs a numeric column or an algorithm",
        ),
        (
            "karate COLOR BY 'x'",
            "COLOR BY needs a column of categories",
        ),
        ("karate SIZE BY a SIZE BY b", "SIZE BY is given twice"),
        (
            "karate NODES (SELECT 1) NODES (SELECT 2)",
            "NODES is given twice",
        ),
        ("karate NODES (SELECT 1", "NODES ( is not closed"),
        ("karate NODES ( )", "NODES () is empty"),
        (
            "karate SIZE BY pagerank WITH care",
            "unexpected 'WITH' after the graph's source",
        ),
        (
            "karate SIZE BY pagerank GROUP BY club",
            "LAYOUT force|rings|layers]",
        ),
    ] {
        let err = parse(text).unwrap_err().to_string();
        assert!(err.contains(says), "{text}: {err}");
    }
}

fn col(ident: &str) -> Name {
    name(ident, ident).unwrap()
}

#[test]
fn as_on_and_layout_are_clauses_too() {
    // Each form of AS, with the columns it names.
    let (source, style) = parsed("SELECT a.id AS x, b.id AS y FROM t AS edges(x, y)");
    assert_eq!(source, "SELECT a.id AS x, b.id AS y FROM t");
    assert_eq!(
        style.shape,
        Some(As::Edges {
            src: col("x"),
            dst: col("y"),
            weight: None
        })
    );
    let (_, style) = parsed("AS edges(A, \"To\", w)");
    assert_eq!(
        style.shape,
        Some(As::Edges {
            src: name("a", "A").unwrap(),
            dst: name("To", "\"To\"").unwrap(),
            weight: Some(col("w"))
        })
    );
    assert_eq!(style.shape.unwrap().shown(), "AS edges(A, \"To\", w)");
    assert_eq!(
        parsed("as nodes ( who )").1.shape,
        Some(As::Nodes { id: col("who") })
    );
    assert_eq!(
        parsed("AS path(members)").1.shape,
        Some(As::Path {
            nodes: col("members"),
            edges: None
        })
    );
    assert_eq!(
        parsed("AS path(members, ties)").1.shape,
        Some(As::Path {
            nodes: col("members"),
            edges: Some(col("ties"))
        })
    );
    assert_eq!(
        parsed("AS hops(start, reached, far)").1.shape,
        Some(As::Hops {
            seed: col("start"),
            id: col("reached"),
            distance: col("far")
        })
    );
    // ON and LAYOUT, and every clause in any order after the source.
    let (source, style) = parsed(
        "t LAYOUT Rings SIZE BY degree ON Karate AS hops(s, i, d) COLOR BY club NODES (FROM m)",
    );
    assert_eq!(source, "t");
    assert_eq!(style.layout, Some(Layout::Rings));
    assert_eq!(style.on, name("karate", "Karate"));
    assert_eq!(style.size, name("degree", "degree"));
    assert_eq!(style.color, name("club", "club"));
    assert_eq!(style.nodes.as_deref(), Some("FROM m"));
    assert!(matches!(style.shape, Some(As::Hops { .. })));
    let (source, style) = parsed("ON \"My Graph\" LAYOUT force");
    assert_eq!(source, "");
    assert_eq!(style.on, name("My Graph", "\"My Graph\""));
    assert_eq!(style.layout, Some(Layout::Force));
    assert_eq!(parsed("LAYOUT layers").1.layout, Some(Layout::Layers));

    // SQL's own AS and ON stay the query's: aliases, a CTE, a join's
    // condition (even a bare column, even last), a layout column.
    for q in [
        "WITH nodes AS (SELECT 1 AS id) SELECT id AS path, id AS layout FROM nodes AS hops",
        "SELECT a.src, b.dst FROM e AS a JOIN e AS b ON a.dst = b.src",
        "SELECT * FROM a JOIN b ON linked",
        "SELECT * FROM a LEFT JOIN b ON linked JOIN c ON other",
        "SELECT DISTINCT ON (src) src, dst FROM e",
        "SELECT layout FROM t ORDER BY layout",
        "SELECT x FROM (SELECT 1 AS edges(x)) AS t",
        "SELECT * FROM (VALUES (1, 2)) AS t(a, b)",
        "FROM layout l",
        "TABLE ties",
        "a.b",
    ] {
        assert_eq!(parsed(q), (q.into(), Style::default()), "{q}");
    }
    // After a join's own ON, an ON is the clause.
    let (source, style) = parsed("SELECT a.x, b.y FROM a JOIN b ON a.k = b.k ON karate");
    assert_eq!(source, "SELECT a.x, b.y FROM a JOIN b ON a.k = b.k");
    assert_eq!(style.on, name("karate", "karate"));
    let (source, style) = parsed("FROM a JOIN b USING (k) ON g AS nodes(k)");
    assert_eq!(source, "FROM a JOIN b USING (k)");
    assert_eq!(style.on, name("g", "g"));
    let (source, style) = parsed("FROM a CROSS JOIN b ON g");
    assert_eq!(
        (source.as_str(), style.on),
        ("FROM a CROSS JOIN b", name("g", "g"))
    );
    // ON before another clause.
    let (source, style) = parsed("SELECT x, y FROM t ON g AS edges(x, y)");
    assert_eq!(source, "SELECT x, y FROM t");
    assert!(style.on.is_some() && style.shape.is_some());
}

#[test]
fn a_misused_as_on_or_layout_says_what_to_write() {
    for (text, says) in [
        ("AS", "AS needs what the result's rows are and the columns to read: AS edges(src, dst [, weight]), AS nodes(id)"),
        ("AS edges", "AS edges needs its columns in parentheses: AS edges(src, dst [, weight])"),
        ("AS edges(a", "AS edges( is not closed: AS edges(src, dst [, weight])"),
        ("AS edges(a)", "AS edges(…) takes the columns of an edge's two ends, and then its weight's, and was given 1: AS edges(src, dst [, weight])"),
        ("AS edges(a, b, c, d)", "and was given 4"),
        ("AS edges()", "and was given 0"),
        ("AS nodes(a, b)", "AS nodes(…) takes one column, the node's id, and was given 2: AS nodes(id)"),
        ("AS path(a, b, c)", "AS path(…) takes the column of each path's nodes, and then the column of its edges, and was given 3: AS path(nodes [, edges])"),
        ("AS hops(a, b)", "AS hops(…) takes three columns: the seed, the node reached and its distance, and was given 2: AS hops(seed, id, distance)"),
        ("AS edges(a b)", "takes the result's column names, with commas between them: AS edges(src, dst [, weight])"),
        ("AS edges(a, )", "with commas between them"),
        ("AS edges(a, 'b')", "and ''b'' is not one"),
        ("AS nodes(a + 1)", "with commas between them"),
        ("AS edge(a, b)", "AS edge is not a form; write AS edges(src, dst [, weight])"),
        ("AS hop(a, b, c)", "AS hop is not a form; write AS hops(seed, id, distance)"),
        ("AS tree(a, b)", "AS tree is not a form; write one of AS edges(src, dst [, weight]), AS nodes(id), AS path(nodes [, edges]) or AS hops(seed, id, distance)"),
        ("AS nodes(a) AS edges(a, b)", "AS is given twice; a result is drawn one way"),
        ("karate SIZE BY degree ON", "ON needs the name of the property graph the result's ids belong to, as ON karate"),
        ("ON (g)", "ON needs the name of the property graph"),
        ("ON g ON h", "ON is given twice"),
        ("LAYOUT", "LAYOUT needs force, rings or layers: LAYOUT force lets"),
        ("karate SIZE BY degree LAYOUT circle", "LAYOUT needs force, rings or layers, and 'circle' is none of them"),
        ("LAYOUT rings LAYOUT force", "LAYOUT is given twice"),
        // After a source that is one name, everything is clauses.
        ("karate LAYOUT spiral", "LAYOUT needs force, rings or layers, and 'spiral' is none of them"),
        ("karate AS tree(a, b)", "AS tree is not a form"),
        ("\"My Graph\" sized by x", "unexpected 'sized' after the graph's source"),
        ("karate LAYOUT", "LAYOUT needs force, rings or layers: "),
        // In a query, a form typed nearly right is the clause it was meant as.
        ("SELECT s, i FROM t AS hop(s, i, d)", "AS hop is not a form; write AS hops(seed, id, distance)"),
        ("FROM t AS edge(a, b)", "AS edge is not a form; write AS edges(src, dst [, weight])"),
        ("AS nodes(a) GROUP BY z", "unexpected 'GROUP' after the graph's source; the clauses are ON graph, AS …, NODES (…), SIZE BY x, COLOR BY y and LAYOUT l"),
    ] {
        let err = parse(text).unwrap_err().to_string();
        assert!(err.contains(says), "{text}: {err}");
    }
}

fn read_by(fields: Vec<Field>, clause: &str) -> Result<Reading> {
    let (_, style) = parsed(clause);
    read_as(
        &Schema::new(fields),
        &style.shape.expect("an AS clause"),
        style.on.is_some(),
    )
}

#[test]
fn rule_0_as_names_the_shape_and_its_columns() {
    let s = DataType::Utf8;
    let ints = || list_of(DataType::Int64);
    // Columns of any names, wherever they are; nothing is inferred, so the
    // src and dst that rule 3 would read are left alone.
    let fields = || {
        vec![
            plain("src", DataType::Int64),
            plain("dst", DataType::Int64),
            plain("x", s.clone()),
            plain("Y", s.clone()),
            plain("score", DataType::Float64),
            plain("far", DataType::Int32),
            plain("route", ints()),
            plain("rows", list_of(DataType::UInt64)),
        ]
    };
    let r = read_by(fields(), "AS edges(x, y)").unwrap();
    assert_eq!(r.shape, Shape::Edges { src: 2, dst: 3 });
    assert_eq!(roles(&r), [("x", "src"), ("Y", "dst")]);
    assert!(r.attributes.is_empty() && r.graph.is_none() && r.notes.is_empty());
    let r = read_by(fields(), "AS edges(dst, src, score)").unwrap();
    assert_eq!(r.shape, Shape::Edges { src: 1, dst: 0 });
    assert_eq!(r.attributes, [4]);
    assert_eq!(
        roles(&r),
        [("dst", "src"), ("src", "dst"), ("score", "weight")]
    );
    // Ends of two kinds are the user's to pair: they meet as text.
    assert_eq!(
        read_by(fields(), "AS edges(src, x)").unwrap().shape,
        Shape::Edges { src: 0, dst: 2 }
    );
    let r = read_by(fields(), "AS nodes(x)").unwrap();
    assert_eq!(r.shape, Shape::Nodes { id: 2 });
    assert_eq!(roles(&r), [("x", "node")]);
    let r = read_by(fields(), "AS path(route)").unwrap();
    assert_eq!(
        r.shape,
        Shape::Path {
            nodes: Some(6),
            edges: None,
            elements: None
        }
    );
    let r = read_by(fields(), "AS path(route, rows)").unwrap();
    assert_eq!(
        r.shape,
        Shape::Path {
            nodes: Some(6),
            edges: Some(7),
            elements: None
        }
    );
    assert_eq!(roles(&r), [("route", "path_nodes"), ("rows", "path_edges")]);
    let r = read_by(fields(), "AS hops(src, dst, far)").unwrap();
    assert_eq!(
        r.shape,
        Shape::Hop {
            seed: 0,
            id: 1,
            distance: 5
        }
    );
    assert_eq!(
        roles(&r),
        [("src", "seed"), ("dst", "node"), ("far", "distance")]
    );

    // A quoted name is matched as written.
    assert!(read_by(fields(), "AS nodes(\"Y\")").is_ok());
    let err = read_by(fields(), "AS nodes(\"y\")")
        .unwrap_err()
        .to_string();
    assert!(err.contains("the result has no column \"y\""), "{err}");

    // Nodes take rule 5's defaults: a metric sizes, a category colours.
    let r = read_by(
        vec![
            plain("id", I),
            plain("who", I),
            plain("pagerank", F),
            plain("community", I),
        ],
        "AS nodes(who)",
    )
    .unwrap();
    assert_eq!((r.size, r.color), (Some(2), Some(3)));
    assert_eq!(
        roles(&r),
        [
            ("who", "node"),
            ("pagerank", "metric"),
            ("community", "category")
        ]
    );

    // What the columns say of themselves still counts: their graph and
    // tables. A hop result's seed and id as edges, against the rules.
    let hop = || {
        vec![
            marked("seed", I, "seed", Some("members"), None),
            marked("id", I, "node", Some("members"), None),
            marked("distance", DataType::UInt32, "distance", None, None),
        ]
    };
    let r = read_by(hop(), "AS edges(seed, id)").unwrap();
    assert_eq!(r.shape, Shape::Edges { src: 0, dst: 1 });
    assert_eq!(r.graph.as_deref(), Some("karate"));
    assert_eq!(r.label_of(1), Some("members"));
    // ON names the graph instead.
    assert_eq!(
        read_by(hop(), "ON other AS edges(seed, id)").unwrap().graph,
        None
    );
    let two = vec![
        marked("a", I, "node", None, None),
        of_graph("b", I, "node", "lesmis", None, None),
    ];
    let err = read_by(two.clone(), "AS edges(a, b)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("the columns come from the graphs karate and lesmis")
            && err.contains("ON karate"),
        "{err}"
    );
    assert!(read_by(two, "ON karate AS edges(a, b)").is_ok());
    // element_id(p) is a path of nodes and edges alternating.
    let r = read_by(
        vec![marked("els", ints(), "path_elements", None, Some("p"))],
        "AS path(els)",
    )
    .unwrap();
    assert_eq!(
        r.shape,
        Shape::Path {
            nodes: None,
            edges: None,
            elements: Some(0)
        }
    );
}

#[test]
fn rule_0_names_the_column_that_is_missing_or_of_the_wrong_type() {
    let fields = || {
        vec![
            plain("x", I),
            plain("y", DataType::Utf8),
            plain("score", F),
            plain("route", list_of(I)),
            plain("scores", list_of(F)),
        ]
    };
    const COLUMNS: &str = "The result's columns are x (Int64), y (Utf8), score (Float64), \
                           route (List(Int64)), scores (List(Float64))";
    for (clause, says) in [
        ("AS edges(x, z)", "AS edges(x, z): the result has no column z. "),
        ("AS edges(x, score)", "AS edges(x, score): column score is a number that is not whole (Float64), and an edge's end is an id: an integer, a string or a date. "),
        ("AS edges(x, x)", "AS edges(x, x): both ends are the column x, so every edge would join a node to itself. Name the two columns of an edge's ends. "),
        ("AS edges(x, y, w)", "AS edges(x, y, w): the result has no column w. "),
        ("AS edges(x, y, y)", "AS edges(x, y, y): column y is a string (Utf8), and an edge's weight is a number. "),
        ("AS nodes(score)", "AS nodes(score): column score is a number that is not whole (Float64), and a node's id is an id: an integer, a string or a date. "),
        ("AS nodes(route)", "AS nodes(route): column route is a list of integers (List(Int64)), and a node's id is an id"),
        ("AS path(x)", "AS path(x): column x is an integer (Int64), and a path's nodes are a list of ids (integers, strings or dates), as vertices(p) gives. "),
        ("AS path(scores)", "AS path(scores): column scores is a list (List(Float64)), and a path's nodes are a list of ids"),
        ("AS path(route, y)", "AS path(route, y): column y is a string (Utf8), and a path's edges are a list of the edges' row numbers, as edges(p) gives; leave it out to draw the nodes alone, AS path(nodes). "),
        ("AS hops(x, y, score)", "AS hops(x, y, score): column score is a number that is not whole (Float64), and a distance is a whole number of steps (CAST it to INTEGER if it is one). "),
        ("AS hops(score, y, x)", "AS hops(score, y, x): column score is a number that is not whole (Float64), and a seed is an id"),
        ("AS hops(x, nope, x)", "AS hops(x, nope, x): the result has no column nope. "),
    ] {
        let err = read_by(fields(), clause).unwrap_err().to_string();
        assert!(err.contains(says), "{clause}: {err}");
        assert!(err.contains(COLUMNS), "{clause}: {err}");
    }
    let err = read_by(vec![plain("only", F)], "AS nodes(id)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("The result's one column is only (Float64); name one of them"),
        "{err}"
    );
}

#[test]
fn color_by_ranks_categories_and_folds_the_rest_into_other() {
    // Nine categories, the largest first; the ninth is "other".
    let mut values: Vec<Option<&str>> = Vec::new();
    for (c, n) in ["a", "b", "c", "d", "e", "f", "g", "h", "i"]
        .iter()
        .zip([1, 9, 8, 7, 6, 5, 4, 3, 2])
    {
        values.extend(std::iter::repeat_n(Some(*c), n));
    }
    values.push(None);
    let array = Arc::new(StringArray::from(values)) as ArrayRef;
    let c = color_by("letter", &array).unwrap();
    assert_eq!(c.labels(), ["b", "c", "d", "e", "f", "g", "h", "i"]);
    assert_eq!(c.top_counts(), [9, 8, 7, 6, 5, 4, 3, 2]);
    assert_eq!((c.other(), c.missing), (1, 1));
    let index = c.index();
    assert_eq!(index[0], OTHER, "a, the smallest, is other");
    assert_eq!(index[1], 0, "b, the largest, is first");
    assert_eq!(*index.last().unwrap(), OTHER, "no value is drawn as other");
    // Every category is kept, by rank: a is the ninth; no value is null.
    assert_eq!(c.values.len(), 9);
    assert_eq!((c.values[8].as_str(), c.counts[8], c.codes[0]), ("a", 1, 8));
    assert_eq!(*c.codes.last().unwrap(), NULL);
    // Too many: numbers that are a measure, or a value per node.
    let floats = Arc::new(Float64Array::from_iter_values((0..20).map(f64::from))) as ArrayRef;
    let err = color_by("score", &floats).unwrap_err().to_string();
    assert!(
        err.contains("20 distinct values over 20 nodes") && err.contains("SIZE BY score"),
        "{err}"
    );
    let names = Arc::new(StringArray::from_iter_values(
        (0..20).map(|i| format!("n{i}")),
    )) as ArrayRef;
    let err = color_by("name", &names).unwrap_err().to_string();
    assert!(
        err.contains("too many categories") && err.contains("fewer values"),
        "{err}"
    );
    // A few distinct floats are categories.
    let few = Arc::new(Float64Array::from(vec![0.5, 1.5, 0.5])) as ArrayRef;
    assert_eq!(color_by("half", &few).unwrap().labels(), ["0.5", "1.5"]);
}

#[test]
fn size_by_takes_numbers() {
    let ints = Arc::new(Int64Array::from(vec![Some(3), None, Some(1)])) as ArrayRef;
    let s = size_by("n", &ints).unwrap();
    assert_eq!(&s.values[..1], &[3.0]);
    assert!(s.values[1].is_nan());
    assert_eq!(s.missing, 1);
    let text = Arc::new(StringArray::from(vec!["a"])) as ArrayRef;
    let err = size_by("club", &text).unwrap_err().to_string();
    assert!(
        err.contains("SIZE BY club needs numbers, and club is text")
            && err.contains("COLOR BY club"),
        "{err}"
    );
    let none = Arc::new(Int64Array::from(vec![None::<i64>])) as ArrayRef;
    assert!(size_by("n", &none)
        .unwrap_err()
        .to_string()
        .contains("no node has a value"));
}

#[test]
fn a_name_is_one_identifier() {
    assert_eq!(identifier("karate"), Some("karate".into()));
    assert_eq!(identifier(" Karate "), Some("karate".into()));
    assert_eq!(identifier("\"Big Graph\""), Some("Big Graph".into()));
    assert_eq!(identifier("SELECT 1"), None);
    assert_eq!(identifier("1abc"), None);
    assert_eq!(identifier(""), None);
}

// ------------------------------------------------------------ the result

fn ints(values: &[i64]) -> ArrayRef {
    Arc::new(Int64Array::from(values.to_vec()))
}

fn provenance() -> Provenance {
    Provenance {
        sql: Some("SELECT src, dst FROM e".into()),
        rule: Some(Rule::Edges.name().into()),
        ..Provenance::default()
    }
}

#[test]
fn a_drawing_becomes_a_graph_result() {
    let (edges, index) = build::build_indexed(
        &[ints(&[3, 2, 1, 9])],
        &[(ints(&[1, 2, 7]), ints(&[2, 3, 7]))],
    )
    .unwrap();
    let view = to_view(edges, &index.key_type, None, provenance()).unwrap();
    let r = &view.result;
    assert_eq!((r.n_nodes(), r.n_edges(), view.skipped), (5, 3, 0));
    assert_eq!(
        r.node_keys().iter().collect::<Vec<_>>(),
        ["3", "2", "1", "9", "7"]
    );
    assert_eq!((r.src(), r.dst()), (&[2, 1, 4][..], &[1, 0, 4][..]));
    // One label for the nodes and one for the edges, and typed keys.
    assert_eq!(r.node_labels().len(), 1);
    assert_eq!(
        (r.node_labels()[0].name.as_str(), r.node_labels()[0].key),
        ("", KeyType::Int)
    );
    assert_eq!(r.node(0, &Key::Int(9)), Some(3));
    assert_eq!(r.edge_labels(), [String::new()]);
    assert!(r.edge_rows().is_none());
    assert!(r.node_columns().is_empty() && r.graph().is_none());
    assert_eq!(r.encodings(), &Default::default());
    assert_eq!(r.provenance(), &provenance());
    assert!(view.legend().is_none());
    // Over the wire and back, and the same bytes each time.
    let bytes = r.to_bytes();
    assert_eq!(&GraphResult::from_bytes(&bytes).unwrap(), r);
    let (edges, index) = build::build_indexed(
        &[ints(&[3, 2, 1, 9])],
        &[(ints(&[1, 2, 7]), ints(&[2, 3, 7]))],
    )
    .unwrap();
    let again = to_view(edges, &index.key_type, None, provenance()).unwrap();
    assert_eq!(again.result.to_bytes(), bytes);
}

#[test]
fn keys_are_integers_only_when_every_one_is_an_i64() {
    let key_of = |array: ArrayRef| {
        let (edges, index) = build::build_indexed(&[], &[(Arc::clone(&array), array)]).unwrap();
        let view = to_view(edges, &index.key_type, None, Provenance::default()).unwrap();
        view.result.node_labels()[0].key
    };
    assert_eq!(key_of(ints(&[-1, 7])), KeyType::Int);
    assert_eq!(
        key_of(Arc::new(UInt64Array::from(vec![1, 2]))),
        KeyType::Int
    );
    // Past i64::MAX, and anything that is not an integer, is text.
    assert_eq!(
        key_of(Arc::new(UInt64Array::from(vec![1, u64::MAX]))),
        KeyType::Text
    );
    assert_eq!(
        key_of(Arc::new(StringArray::from(vec!["1", "2"]))),
        KeyType::Text
    );
    // Mixed types compare as text, so their keys are text.
    let (edges, index) = build::build_indexed(
        &[],
        &[(
            ints(&[1]),
            Arc::new(StringArray::from(vec!["2"])) as ArrayRef,
        )],
    )
    .unwrap();
    let view = to_view(edges, &index.key_type, None, Provenance::default()).unwrap();
    assert_eq!(view.result.node_labels()[0].key, KeyType::Text);
}

/// A drawing of `n` nodes on a ring, sized and coloured.
fn styled(n: i64, size: Option<&str>, color: Option<(&str, ArrayRef)>) -> GraphView {
    let ids: Vec<i64> = (0..n).collect();
    let next: Vec<i64> = (0..n).map(|i| (i + 1) % n).collect();
    let (mut edges, index) = build::build_indexed(&[], &[(ints(&ids), ints(&next))]).unwrap();
    if let Some(by) = size {
        let values = Arc::new(Int64Array::from(
            (0..n).map(|i| (i != 1).then_some(i)).collect::<Vec<_>>(),
        )) as ArrayRef;
        edges.size = Some(size_by(by, &values).unwrap());
    }
    if let Some((by, values)) = color {
        edges.color = Some(color_by(by, &values).unwrap());
    }
    to_view(
        edges,
        &index.key_type,
        Some("ring".into()),
        Provenance::default(),
    )
    .unwrap()
}

#[test]
fn size_and_colour_are_node_columns_the_encodings_name() {
    // Ten categories over 30 nodes: i % 10, and the first node has none.
    let categories = Arc::new(StringArray::from(
        (0..30)
            .map(|i| (i != 0).then(|| format!("c{}", i % 10)))
            .collect::<Vec<_>>(),
    )) as ArrayRef;
    let view = styled(30, Some("Score"), Some(("\"Team\"", categories)));
    let r = &view.result;
    assert_eq!(r.graph(), Some("ring"));
    assert_eq!(r.encodings().size_by.as_deref(), Some("Score"));
    assert_eq!(r.encodings().color_by.as_deref(), Some("\"Team\""));
    let size = r.node_column("Score").unwrap();
    assert_eq!(size.role, Some(Role::Metric));
    let Values::F32(values) = &size.values else {
        panic!("{size:?}");
    };
    assert_eq!((values[0], values[2]), (0.0, 2.0));
    assert!(values[1].is_nan(), "a node with no value is NaN");
    let color = r.node_column("\"Team\"").unwrap();
    assert_eq!(color.role, Some(Role::Category));
    let Values::Dict { values, codes } = &color.values else {
        panic!("{color:?}");
    };
    // Every category, the largest first (c0 lost a node, so it is last).
    assert_eq!(values.len(), 10);
    assert_eq!((values.get(0), values.get(9)), ("c1", "c0"));
    assert_eq!((codes[0], codes[1], codes[10]), (NULL, 0, 9));
    // The legend: eight coloured, the ninth and tenth are other.
    let legend = view.legend().unwrap();
    assert_eq!(legend.by, "\"Team\"");
    assert_eq!(
        legend.labels,
        ["c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8"]
    );
    assert_eq!(legend.counts, [3; 8]);
    assert_eq!((legend.other, legend.missing), (5, 1));
    // Over the wire and back.
    assert_eq!(&GraphResult::from_bytes(&r.to_bytes()).unwrap(), r);
}

#[test]
fn one_name_for_size_and_colour_is_two_columns() {
    let degree = Arc::new(Int64Array::from(vec![2, 2, 3, 3])) as ArrayRef;
    let (mut edges, index) =
        build::build_indexed(&[], &[(ints(&[0, 1, 2, 3]), ints(&[1, 2, 3, 0]))]).unwrap();
    edges.size = Some(size_by("degree", &degree).unwrap());
    edges.color = Some(color_by("degree", &degree).unwrap());
    let view = to_view(edges, &index.key_type, None, Provenance::default()).unwrap();
    let r = &view.result;
    assert_eq!(r.encodings().size_by.as_deref(), Some("degree"));
    assert_eq!(r.encodings().color_by.as_deref(), Some("degree (colour)"));
    assert_eq!(r.node_columns().len(), 2);
    // The legend is by the name as typed.
    let legend = view.legend().unwrap();
    assert_eq!(legend.by, "degree");
    assert_eq!(
        (legend.labels, legend.counts),
        (vec!["2".to_string(), "3".into()], vec![2, 2])
    );
    // A column that only looks suffixed keeps its name.
    let (mut edges, index) = build::build_indexed(&[], &[(ints(&[0, 1]), ints(&[1, 0]))]).unwrap();
    let values = Arc::new(StringArray::from(vec!["a", "b"])) as ArrayRef;
    edges.color = Some(color_by("x (colour)", &values).unwrap());
    let view = to_view(edges, &index.key_type, None, Provenance::default()).unwrap();
    assert_eq!(view.legend().unwrap().by, "x (colour)");
}

#[test]
fn rules_have_names() {
    assert_eq!((Rule::Graph.name(), Rule::Edges.name()), ("graph", "edges"));
    let schema = schema_of(&[
        ("w", DataType::Float64),
        ("dst", DataType::Int64),
        ("src", DataType::Int64),
    ]);
    assert_eq!(
        shape_of(&schema).unwrap(),
        rules::Shape::Edges { src: 2, dst: 1 }
    );
    assert_eq!(shape_of(&schema).unwrap().rule(), Rule::Edges);
}

// The result → graph rules (#433): what `read` makes of a schema, with the
// `ursa.*` metadata the graph functions write and without it.

/// A field as a graph function marks it: `role`, of graph `karate`, with
/// the vertex or edge table `label` and the pattern variable `var`.
fn marked(name: &str, t: DataType, role: &str, label: Option<&str>, var: Option<&str>) -> Field {
    of_graph(name, t, role, "karate", label, var)
}

fn of_graph(
    name: &str,
    t: DataType,
    role: &str,
    graph: &str,
    label: Option<&str>,
    var: Option<&str>,
) -> Field {
    let mut m = crate::graph::provenance::metadata(role, graph, label);
    if let Some(var) = var {
        m.insert("ursa.var".into(), var.into());
    }
    Field::new(name, t, true).with_metadata(m.into_iter().collect())
}

fn plain(name: &str, t: DataType) -> Field {
    Field::new(name, t, true)
}

fn reading(fields: Vec<Field>) -> Reading {
    read(&Schema::new(fields)).unwrap()
}

fn roles(r: &Reading) -> Vec<(&str, &str)> {
    r.roles
        .iter()
        .map(|(c, role)| (c.as_str(), role.as_str()))
        .collect()
}

const I: DataType = DataType::Int64;
const F: DataType = DataType::Float64;

#[test]
fn rule_1_a_path_column_is_paths() {
    // COLUMNS (path_length(p), vertices(p), edges(p)).
    let r = reading(vec![
        plain("steps", I),
        marked(
            "members",
            list_of(I),
            "path_nodes",
            Some("members"),
            Some("p"),
        ),
        marked("ties", list_of(I), "path_edges", Some("ties"), Some("p")),
    ]);
    assert_eq!(
        r.shape,
        Shape::Path {
            nodes: Some(1),
            edges: Some(2),
            elements: None
        }
    );
    assert_eq!(r.shape.rule(), Rule::Path);
    assert_eq!(r.graph.as_deref(), Some("karate"));
    assert_eq!(r.label_of(1), Some("members"));
    assert_eq!(r.edge_label.as_deref(), Some("ties"));
    assert_eq!(
        roles(&r),
        [("members", "path_nodes"), ("ties", "path_edges")]
    );
    // vertices(p) alone, and element_id(p) alone.
    let r = reading(vec![marked(
        "members",
        list_of(I),
        "path_nodes",
        None,
        Some("p"),
    )]);
    assert_eq!(
        r.shape,
        Shape::Path {
            nodes: Some(0),
            edges: None,
            elements: None
        }
    );
    let r = reading(vec![marked(
        "el",
        list_of(I),
        "path_elements",
        None,
        Some("p"),
    )]);
    assert_eq!(
        r.shape,
        Shape::Path {
            nodes: None,
            edges: None,
            elements: Some(0)
        }
    );
    // Without metadata: a list of ids is a path, when the first two columns
    // are not a pair of ids.
    let r = reading(vec![plain("steps", I), plain("members", list_of(I))]);
    assert_eq!(
        r.shape,
        Shape::Path {
            nodes: Some(1),
            edges: None,
            elements: None
        }
    );
    assert!(!r.marked);
    assert_eq!(roles(&r), [("members", "path_nodes")]);
    let r = reading(vec![plain("route", list_of(DataType::Utf8))]);
    assert_eq!(r.shape.rule(), Rule::Path);
    // A list of floats is no path.
    let err = read(&Schema::new(vec![plain("xs", list_of(F))])).unwrap_err();
    assert!(err.to_string().contains("cannot draw this result"), "{err}");
}

#[test]
fn rule_1_the_steps_of_shortest_path_are_a_path_by_their_roles() {
    let r = reading(vec![
        marked("src", I, "src", Some("members"), None),
        marked("dst", I, "dst", Some("members"), None),
        marked("hop", I, "step", None, None),
        plain("cost", F),
    ]);
    assert_eq!(
        r.shape,
        Shape::Steps {
            src: 0,
            dst: 1,
            step: 2
        }
    );
    assert_eq!(r.shape.rule(), Rule::Path);
    assert_eq!(r.attributes, [3]);
    // By names alone: edges.
    let r = reading(vec![
        plain("src", I),
        plain("dst", I),
        plain("hop", I),
        plain("cost", F),
    ]);
    assert_eq!(r.shape, Shape::Edges { src: 0, dst: 1 });
    assert_eq!(roles(&r), [("src", "src"), ("dst", "dst")]);
}

#[test]
fn rule_2_seed_id_and_distance_are_rings() {
    let hop = Shape::Hop {
        seed: 0,
        id: 1,
        distance: 2,
    };
    let r = reading(vec![
        marked("seed", I, "seed", Some("members"), None),
        marked("id", I, "node", Some("members"), None),
        marked("distance", DataType::UInt32, "distance", None, None),
    ]);
    assert_eq!(r.shape, hop);
    assert_eq!(r.shape.rule(), Rule::Rings);
    assert_eq!(r.graph.as_deref(), Some("karate"));
    // Whatever the columns are called.
    let r = reading(vec![
        marked("from_node", I, "seed", None, None),
        marked("reached", I, "node", None, None),
        marked("far", DataType::UInt32, "distance", None, None),
    ]);
    assert_eq!(r.shape, hop);
    // Without metadata, by the three names together.
    let r = reading(vec![plain("seed", I), plain("id", I), plain("distance", I)]);
    assert_eq!(r.shape, hop);
    assert_eq!(
        roles(&r),
        [("seed", "seed"), ("id", "node"), ("distance", "distance")]
    );
    // A hop result with its distance renamed or computed: edges seed–id,
    // and a note, with metadata and without.
    for fields in [
        vec![
            marked("seed", I, "seed", None, None),
            marked("id", I, "node", None, None),
            plain("d", I),
        ],
        vec![plain("seed", I), plain("id", I), plain("d", I)],
    ] {
        let r = reading(fields);
        assert_eq!(r.shape, Shape::Edges { src: 0, dst: 1 });
        assert!(
            r.notes[0].contains("no distance column, so no rings"),
            "{:?}",
            r.notes
        );
    }
}

#[test]
fn rule_3_endpoints_are_edges() {
    // By role, wherever and whatever they are called.
    let r = reading(vec![
        marked("edge", I, "edge", Some("ties"), None),
        marked("b", I, "dst", Some("members"), None),
        marked("a", I, "src", Some("members"), None),
    ]);
    assert_eq!(r.shape, Shape::Edges { src: 2, dst: 1 });
    assert_eq!(r.edge_label.as_deref(), Some("ties"));
    assert_eq!(roles(&r), [("a", "src"), ("b", "dst")]);
    // Exactly two node variables of a pattern, with their properties.
    let r = reading(vec![
        marked("ca", DataType::Utf8, "property", Some("members"), Some("a")),
        marked("cb", DataType::Utf8, "property", Some("members"), Some("b")),
        marked("x", I, "node", Some("members"), Some("a")),
        marked("y", I, "node", Some("members"), Some("b")),
    ]);
    assert_eq!(r.shape, Shape::Edges { src: 2, dst: 3 });
    assert_eq!(r.properties, [(0, 2), (1, 3)]);
    // Two node keys and a metric: edges; the metric is an edge attribute.
    let r = reading(vec![
        marked("a", I, "node", None, Some("a")),
        marked("b", I, "node", None, Some("b")),
        marked("pagerank", F, "metric", None, None),
    ]);
    assert_eq!(r.shape, Shape::Edges { src: 0, dst: 1 });
    assert_eq!(r.attributes, [2]);
    // A node key twice (a self-join): two keys are two ends.
    let r = reading(vec![
        marked("id", I, "node", Some("members"), None),
        marked("id", I, "node", Some("members"), None),
    ]);
    assert_eq!(r.shape, Shape::Edges { src: 0, dst: 1 });
    // By name: src/dst, source/target, from/to, in any case and place.
    for (s, d) in [("src", "dst"), ("Source", "TARGET"), ("from", "to")] {
        let r = reading(vec![plain("weight", F), plain(d, I), plain(s, I)]);
        assert_eq!(r.shape, Shape::Edges { src: 2, dst: 1 }, "{s}, {d}");
        assert_eq!(r.attributes, [0]);
        assert!(!r.marked);
    }
    // With no metadata and no names: the first two columns, ids of one kind.
    let r = reading(vec![plain("a", I), plain("b", I), plain("w", F)]);
    assert_eq!(r.shape, Shape::Edges { src: 0, dst: 1 });
    assert_eq!(roles(&r), [("a", "src"), ("b", "dst")]);
    // …which a list beside them does not change; the status line says what
    // else it could be.
    let r = reading(vec![
        plain("a", I),
        plain("b", I),
        plain("tags", list_of(I)),
    ]);
    assert_eq!(r.shape, Shape::Edges { src: 0, dst: 1 });
    assert_eq!(r.notes, ["also a path (tags): .graph AS path(tags)"]);
}

#[test]
fn rule_4_three_node_variables_are_pattern_matches() {
    let pattern = r#"[{"directed":false,"edge":null,"from":"a","label":"ties","to":"b"},
                      {"directed":true,"edge":"e","from":"c","label":"pays","to":"b"},
                      {"directed":true,"edge":null,"from":"c","label":"ties","to":"d"}]"#;
    let schema = Schema::new(vec![
        marked("a", I, "node", Some("members"), Some("a")),
        marked(
            "club",
            DataType::Utf8,
            "property",
            Some("members"),
            Some("b"),
        ),
        marked("b", I, "node", Some("members"), Some("b")),
        marked("c", I, "node", Some("firms"), Some("c")),
    ])
    .with_metadata([("ursa.pattern".to_string(), pattern.to_string())].into());
    let r = read(&schema).unwrap();
    // One edge for each edge of the pattern whose two ends are in COLUMNS:
    // d is not, so c→d is not drawn.
    assert_eq!(
        r.shape,
        Shape::Matches {
            keys: vec![0, 2, 3],
            edges: vec![
                MatchEdge {
                    from: 0,
                    to: 1,
                    directed: false,
                    label: "ties".into()
                },
                MatchEdge {
                    from: 2,
                    to: 1,
                    directed: true,
                    label: "pays".into()
                },
            ],
        }
    );
    assert_eq!(r.shape.rule(), Rule::Matches);
    assert_eq!(r.properties, [(1, 2)]);
    assert_eq!(r.label_of(0), Some("members"));
    assert_eq!(r.label_of(3), Some("firms"));
    assert_eq!(roles(&r), [("a", "node"), ("b", "node"), ("c", "node")]);
}

#[test]
fn rule_5_one_node_key_is_nodes() {
    // algorithms(karate, pagerank(), community => louvain()).
    let r = reading(vec![
        marked("id", I, "node", Some("members"), None),
        marked("pagerank", F, "metric", None, None),
        marked("community", DataType::UInt32, "category", None, None),
    ]);
    assert_eq!(r.shape, Shape::Nodes { id: 0 });
    assert_eq!(r.shape.rule(), Rule::Nodes);
    assert_eq!((r.size, r.color), (Some(1), Some(2)));
    assert_eq!(
        roles(&r),
        [
            ("id", "node"),
            ("pagerank", "metric"),
            ("community", "category")
        ]
    );
    // A column is what its call is, whatever it is named; and a plain
    // column beside a node key is read by its name.
    let r = reading(vec![
        marked("score", F, "metric", None, None),
        marked("member", I, "node", None, None),
        plain("louvain", I),
        plain("club", DataType::Utf8),
    ]);
    assert_eq!(r.shape, Shape::Nodes { id: 1 });
    assert_eq!((r.size, r.color), (Some(0), Some(2)));
    // Without metadata: `id` (or `node`) is the key by name. An integer id
    // and an integer community are nodes coloured by community, not edges.
    let r = reading(vec![plain("id", I), plain("community", I)]);
    assert_eq!(r.shape, Shape::Nodes { id: 0 });
    assert_eq!((r.size, r.color), (None, Some(1)));
    assert_eq!(roles(&r), [("id", "node"), ("community", "category")]);
    let r = reading(vec![plain("pagerank", F), plain("Node", DataType::Utf8)]);
    assert_eq!(r.shape, Shape::Nodes { id: 1 });
    assert_eq!((r.size, r.color), (Some(0), None));
    // One column, a vertex table's keys.
    let r = reading(vec![plain("id", I)]);
    assert_eq!(r.shape, Shape::Nodes { id: 0 });
    // A float is never a node id, whatever it is called.
    assert!(read(&Schema::new(vec![plain("id", F), plain("x", F)])).is_err());
}

#[test]
fn rule_6_what_is_not_a_graph_says_what_would_be() {
    let refused = |fields: Vec<Field>| read(&Schema::new(fields)).unwrap_err().to_string();
    let err = refused(vec![
        plain("a", I),
        plain("score", F),
        plain("b", DataType::Utf8),
    ]);
    assert!(
        err.contains("it has 3 columns (a: Int64, score: Float64, b: Utf8)"),
        "{err}"
    );
    assert!(err.contains(".graph AS edges(a, b)"), "{err}");
    assert!(err.contains(".graph AS nodes(a)"), "{err}");
    let err = refused(vec![plain("x", F), plain("y", F)]);
    assert!(
        err.contains("None of its columns can be a node's id"),
        "{err}"
    );
    // Metadata that is not enough for a rule: a metric alone.
    let err = refused(vec![marked("pagerank", F, "metric", None, None)]);
    assert!(err.contains("it has 1 column (pagerank: Float64)"), "{err}");
}

#[test]
fn what_is_ambiguous_is_decided_and_said() {
    // Endpoint columns and a path column: the path, the more specific.
    let r = reading(vec![
        marked("src", I, "src", None, None),
        marked("dst", I, "dst", None, None),
        marked("members", list_of(I), "path_nodes", None, Some("p")),
    ]);
    assert_eq!(r.shape.rule(), Rule::Path);
    assert_eq!(
        r.notes,
        ["also edges (src, dst): .graph AS edges(src, dst)"]
    );
    // Several graphs in one query: ids of two graphs do not meet.
    let err = read(&Schema::new(vec![
        of_graph("a", I, "node", "karate", None, Some("a")),
        of_graph("b", I, "node", "florentine", None, Some("b")),
    ]))
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("its node columns come from the graphs karate and florentine"),
        "{err}"
    );
    // The graph is the node columns': a metric of another graph beside a
    // node key does not make two.
    let r = reading(vec![
        of_graph("id", I, "node", "karate", None, None),
        of_graph("pagerank", F, "metric", "florentine", None, None),
    ]);
    assert_eq!(r.graph.as_deref(), Some("karate"));
    // src and dst named but not ids are an error, not another shape.
    let err = read(&Schema::new(vec![plain("src", I), plain("dst", F)]))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cannot draw columns src and dst as edges"),
        "{err}"
    );
}
