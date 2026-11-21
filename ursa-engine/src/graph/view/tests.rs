//! `graph::view`'s unit tests: the rules, the ids, the clauses and the
//! styles (moved here from `ursa-wasm`, #425), and the result they become.

use std::sync::Arc;

use datafusion::arrow::array::{
    ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray, UInt64Array,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::error::Result;
use ursa_viz::model::{GraphResult, Key, KeyType, Provenance, Role, Values, NULL};

use super::build::{build, from_shape, EdgeList};
use super::rules::{endpoint_columns, shape_of};
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
    assert!(err.contains("the result has 1 column;"), "{err}");
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
    ] {
        let err = parse(text).unwrap_err().to_string();
        assert!(err.contains(says), "{text}: {err}");
    }
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
