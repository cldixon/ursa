//! `graph::view`: what `.graph` draws, as a `GraphResult` (#425). The last
//! result, a query, a table and a property graph; `SIZE BY`, `COLOR BY` and
//! `NODES`; and what cannot be drawn. (The same through `ursa-sql`'s
//! dialect and the shell's last result is in `ursa-wasm`'s tests.)
//!
//! It also keeps the fixtures `tests/fixtures/graph_view_*.ugr`: a view's
//! UGR bytes, each with a `.json` that says what a front end must read from
//! them, written here from the Rust values and never from the bytes.
//! `web/tests/graph-data.test.ts` reads them with the page's own code, so
//! the path `graph::view` → UGR → JavaScript is checked end to end. This
//! test fails when a view no longer encodes to them; if that was meant:
//!
//! ```sh
//! UGR_BLESS=1 cargo test -p ursa-engine --test graph_view
//! ```

use std::path::{Path, PathBuf};

use datafusion::common::TableReference;
use datafusion::prelude::SessionContext;
use serde_json::{json, Value};
use ursa_engine::graph::view::{
    of_command, DataFusionSql, GraphView, Rows, RunSql, MAX_CATEGORIES,
};
use ursa_engine::graph::{create_graph, EdgeTable, GraphDef, VertexTable};
use ursa_engine::{new_session, user_message};
use ursa_viz::model::{GraphResult, KeyType, Values, NULL};

fn def(name: &str, edges: (&str, &str, &str), vertices: Option<(&str, &str)>) -> GraphDef {
    GraphDef {
        name: name.into(),
        edges: EdgeTable::new(TableReference::bare(edges.0), edges.1, edges.2),
        vertices: vertices.map(|(table, key)| VertexTable::new(TableReference::bare(table), key)),
    }
}

async fn run(ctx: &SessionContext, sql: &str) {
    ctx.sql(sql).await.unwrap().collect().await.unwrap();
}

async fn rows(ctx: &SessionContext, sql: &str) -> Option<Rows> {
    Some(DataFusionSql.run(ctx, sql).await.unwrap())
}

async fn draw(ctx: &SessionContext, text: &str, last: Option<Rows>) -> GraphView {
    of_command(ctx, &DataFusionSql, text, last)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"))
}

async fn refusal(ctx: &SessionContext, text: &str, last: Option<Rows>) -> String {
    let err = of_command(ctx, &DataFusionSql, text, last)
        .await
        .expect_err(text);
    user_message(&err)
}

fn ids(view: &GraphView) -> Vec<&str> {
    view.result.node_keys().iter().collect()
}

fn ends(view: &GraphView) -> (&[u32], &[u32]) {
    (view.result.src(), view.result.dst())
}

/// `SIZE BY`'s values, by node.
fn size(view: &GraphView) -> &[f32] {
    let by = view.result.encodings().size_by.as_deref().expect("sized");
    match &view.result.node_column(by).expect("the size column").values {
        Values::F32(values) => values,
        other => panic!("{other:?}"),
    }
}

/// `COLOR BY`'s category of each node, as an index into the legend's, and
/// `MAX_CATEGORIES` for the rest and for no value.
fn color(view: &GraphView) -> Vec<u32> {
    let by = view
        .result
        .encodings()
        .color_by
        .as_deref()
        .expect("coloured");
    match &view
        .result
        .node_column(by)
        .expect("the colour column")
        .values
    {
        Values::Dict { codes, .. } => codes
            .iter()
            .map(|&c| c.min(MAX_CATEGORIES as u32))
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_last_result_a_query_a_table_or_a_property_graph() {
    let ctx = new_session();
    let err = refusal(&ctx, "", None).await;
    assert!(err.contains("no result to draw yet"), "{err}");

    run(
        &ctx,
        "CREATE VIEW people AS SELECT * FROM (VALUES ('ann'), ('bob'), ('cy'), ('dee')) AS t(name)",
    )
    .await;
    run(&ctx, "CREATE VIEW knows AS SELECT * FROM (VALUES ('bob', 'ann', 1), ('cy', 'bob', 2)) AS t(a, b, w)").await;
    // The last result: src and dst by name, not position.
    let last = rows(&ctx, "SELECT w, a AS \"Src\", b AS \"DST\" FROM knows").await;
    let g = draw(&ctx, "", last.clone()).await;
    assert_eq!(ids(&g), ["bob", "ann", "cy"]);
    assert_eq!(ends(&g), (&[0, 2][..], &[1, 0][..]));
    assert_eq!(g.skipped, 0);
    // Its SQL is not known; the rule that drew it is.
    let provenance = g.result.provenance();
    assert_eq!(
        (provenance.sql.as_deref(), provenance.rule.as_deref()),
        (None, Some("edges"))
    );
    assert_eq!(g.result.graph(), None);
    assert_eq!(g.result.node_labels()[0].key, KeyType::Text);

    // A query, with or without its semicolon; the first two columns.
    let g = draw(&ctx, " SELECT b, a FROM knows; ", last.clone()).await;
    assert_eq!(ids(&g), ["ann", "bob", "cy"]);
    assert_eq!(
        g.result.provenance().sql.as_deref(),
        Some("SELECT b, a FROM knows")
    );
    // A table by name.
    let g = draw(&ctx, "knows", last.clone()).await;
    assert_eq!(g.result.n_nodes(), 3);
    assert_eq!(
        g.result.provenance().sql.as_deref(),
        Some("SELECT * FROM knows")
    );

    // A property graph: its vertices first, so dee, with no edges, is drawn.
    create_graph(
        &ctx,
        def("social", ("knows", "a", "b"), Some(("people", "name"))),
        false,
        false,
    )
    .await
    .unwrap();
    let g = draw(&ctx, "Social", last.clone()).await;
    assert_eq!(ids(&g), ["ann", "bob", "cy", "dee"]);
    assert_eq!(ends(&g), (&[1, 2][..], &[0, 1][..]));
    assert_eq!(g.result.graph(), Some("social"));
    let provenance = g.result.provenance();
    assert_eq!(
        (provenance.sql.as_deref(), provenance.rule.as_deref()),
        (None, Some("graph"))
    );

    // What cannot be drawn says why.
    for (text, says) in [
        ("nothing_here", "no graph or table named 'nothing_here'"),
        ("SELECT 1 AS x", "1 column"),
        ("SELECT * FROM knows WHERE false", "no rows"),
        ("SELECT nope", "nope"),
    ] {
        let err = refusal(&ctx, text, last.clone()).await;
        assert!(err.contains(says), "{text}: {err}");
    }
    let err = refusal(&ctx, "", rows(&ctx, "SELECT 42 AS answer").await).await;
    assert!(err.contains("1 column"), "{err}");
    // #427: a score is not a node.
    let err = refusal(&ctx, "SELECT 1 AS id, 0.5 AS pagerank", None).await;
    assert!(
        err.contains("cannot draw the first two columns")
            && err.contains(".graph NAME SIZE BY pagerank"),
        "{err}"
    );
}

/// Two triangles joined by one edge: two communities.
async fn club() -> SessionContext {
    let ctx = new_session();
    run(&ctx, "CREATE VIEW people AS SELECT * FROM (VALUES (1, 'red', 10, 'a'), (2, 'red', 20, 'b'), (3, 'red', 30, 'c'), (4, 'blue', 40, 'd'), (5, 'blue', 50, 'e'), (6, 'blue', 60, 'f')) AS t(id, side, age, name)").await;
    run(&ctx, "CREATE VIEW ties AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (4, 5), (5, 6), (6, 4), (3, 4)) AS t(src, dst)").await;
    create_graph(
        &ctx,
        def("club", ("ties", "src", "dst"), Some(("people", "id"))),
        false,
        false,
    )
    .await
    .unwrap();
    ctx
}

#[tokio::test]
async fn styles_size_and_colour_nodes() {
    let ctx = club().await;

    // Algorithms, run on the graph, joined on id.
    let g = draw(&ctx, "club SIZE BY pagerank COLOR BY louvain", None).await;
    assert_eq!(g.result.encodings().size_by.as_deref(), Some("pagerank"));
    let values = size(&g);
    assert_eq!(values.len(), 6);
    assert!(
        values.iter().all(|v| v.is_finite() && *v > 0.0),
        "{values:?}"
    );
    // 3 and 4, the bridge's ends, rank highest.
    assert!(values[2] > values[0], "{values:?}");
    let legend = g.legend().unwrap();
    assert_eq!(
        (legend.by.as_str(), &legend.counts),
        ("louvain", &vec![3, 3])
    );
    let index = color(&g);
    assert_eq!(index[..3], [index[0]; 3]);
    assert_eq!(index[3..], [index[3]; 3]);
    assert_ne!(index[0], index[3]);
    assert_eq!(g.result.node_labels()[0].key, KeyType::Int);

    // Vertex columns come before algorithms; degree is the drawing's.
    let g = draw(&ctx, "club SIZE BY age COLOR BY Side", None).await;
    assert_eq!(size(&g), [10.0, 20.0, 30.0, 40.0, 50.0, 60.0]);
    let legend = g.legend().unwrap();
    assert_eq!(legend.by, "Side", "as typed");
    assert_eq!(
        (legend.labels, legend.counts),
        (vec!["red".to_string(), "blue".into()], vec![3, 3])
    );
    let g = draw(&ctx, "club SIZE BY degree", None).await;
    assert_eq!(size(&g), [2.0, 2.0, 3.0, 3.0, 2.0, 2.0]);
    assert!(g.legend().is_none());

    // NODES gives values for rows, and wins over the graph's own.
    let g = draw(
        &ctx,
        "SELECT src, dst FROM ties NODES (SELECT id, id * 1.5 AS score, CASE WHEN id < 4 THEN 'x' END AS tag FROM people WHERE id <> 6) SIZE BY score COLOR BY tag",
        None,
    )
    .await;
    let values = size(&g);
    assert_eq!(&values[..5], &[1.5, 3.0, 4.5, 6.0, 7.5]);
    assert!(values[5].is_nan());
    let legend = g.legend().unwrap();
    assert_eq!((legend.labels, legend.missing), (vec!["x".to_string()], 3));
    assert_eq!(color(&g), [0, 0, 0, 8, 8, 8]);
    // The source's SQL, not NODES's.
    assert_eq!(
        g.result.provenance().sql.as_deref(),
        Some("SELECT src, dst FROM ties")
    );
    let g = draw(
        &ctx,
        "club NODES (SELECT id, 7 AS age FROM people) SIZE BY age",
        None,
    )
    .await;
    assert_eq!(size(&g), [7.0; 6]);
    // Rows: degree, from the edges drawn.
    let last = rows(&ctx, "SELECT * FROM ties").await;
    let g = draw(&ctx, "SIZE BY degree", last.clone()).await;
    assert_eq!(size(&g).len(), 6);
    // The key itself is a vertex column too.
    let g = draw(&ctx, "club COLOR BY id", None).await;
    assert_eq!(g.legend().unwrap().labels, ["1", "2", "3", "4", "5", "6"]);
    // One name twice: a column each, and the legend by the name.
    let g = draw(&ctx, "club SIZE BY degree COLOR BY degree", None).await;
    assert_eq!(g.result.node_columns().len(), 2);
    assert_eq!(g.legend().unwrap().by, "degree");
    // Without a style, as before.
    let g = draw(&ctx, "club", None).await;
    assert!(g.result.node_columns().is_empty());
    assert_eq!(g.result.encodings(), &Default::default());

    for (text, says) in [
        ("club SIZE BY nope", "SIZE BY nope: graph 'club' has no vertex column or algorithm named 'nope'. Its vertex columns are age, id, name, side; the algorithms are degree, pagerank"),
        ("club COLOR BY wcc", "no vertex column or algorithm named 'wcc'"),
        ("club SIZE BY side", "SIZE BY side needs numbers, and side is text; colour nodes by it instead: COLOR BY side"),
        ("SELECT src, dst FROM ties SIZE BY pagerank", "SIZE BY pagerank runs pagerank() on a property graph, and this draws rows"),
        ("ties COLOR BY louvain", "draw the graph by name (.graph NAME COLOR BY louvain)"),
        ("SIZE BY age", "SIZE BY age: no column named 'age'. Rows drawn as edges are sized or coloured by degree, or by a column of NODES"),
        ("NODES (SELECT id, age FROM people) SIZE BY agee", "NODES (…) has age"),
        ("club NODES (SELECT id FROM people) SIZE BY id", "NODES (…) needs two columns or more"),
        ("club NODES (SELECT id, age FROM people)", "NODES (…) gives values for SIZE BY or COLOR BY to use"),
        ("club NODES (SELECT 'zz' AS id, 1 AS v) SIZE BY v", "SIZE BY v: no node has a value"),
        ("club SIZE BY", "SIZE BY needs"),
    ] {
        let message = refusal(&ctx, text, last.clone()).await;
        assert!(message.contains(says), "{text}: {message}");
    }
}

// ---------------------------------------------------------------- fixtures

/// What a front end must read from a view's bytes: the drawing, as the
/// page's `graph-data.js` shapes it. Floats are their bits.
fn expect(view: &GraphView) -> Value {
    let r = &view.result;
    let mut out = json!({
        "skipped": view.skipped,
        "n": r.n_nodes(),
        "src": r.src(),
        "dst": r.dst(),
        "ids": ids(view),
        "graph": r.graph(),
        "provenance": {"sql": r.provenance().sql, "rule": r.provenance().rule},
    });
    if r.encodings().size_by.is_some() {
        out["sizeBy"] = json!(r.encodings().size_by);
        out["size"] = json!(size(view).iter().map(|v| v.to_bits()).collect::<Vec<_>>());
    }
    if let Some(legend) = view.legend() {
        out["colorBy"] = json!(legend.by);
        out["color"] = json!(color(view));
        out["colorLabels"] = json!(legend.labels);
        out["colorCounts"] = json!(legend.counts);
        out["colorOther"] = json!(legend.other);
        out["colorMissing"] = json!(legend.missing);
    }
    out
}

fn check_fixture(name: &str, view: &GraphView) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let bytes = view.result.to_bytes();
    assert_eq!(&GraphResult::from_bytes(&bytes).unwrap(), &view.result);
    let text = serde_json::to_string_pretty(&expect(view)).unwrap() + "\n";
    let (ugr, json) = (
        dir.join(format!("{name}.ugr")),
        dir.join(format!("{name}.json")),
    );
    if std::env::var_os("UGR_BLESS").is_some() {
        std::fs::write(&ugr, &bytes).unwrap();
        std::fs::write(&json, &text).unwrap();
    }
    let read = |path: &Path| {
        std::fs::read(path)
            .unwrap_or_else(|e| panic!("{}: {e} (UGR_BLESS=1 writes it)", path.display()))
    };
    assert!(
        read(&ugr) == bytes,
        "{name}.ugr is not what the view encodes to"
    );
    assert_eq!(String::from_utf8(read(&json)).unwrap(), text, "{name}.json");
}

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

#[tokio::test]
async fn views_encode_to_the_fixtures_the_page_reads() {
    // Karate, sized by degree and coloured by its vertex column `club`.
    let ctx = new_session();
    for (name, file) in [
        ("members", "karate_nodes.csv"),
        ("ties", "karate_edges.csv"),
    ] {
        run(
            &ctx,
            &format!(
                "CREATE VIEW {name} AS SELECT * FROM '{}'",
                data(file).display()
            ),
        )
        .await;
    }
    create_graph(
        &ctx,
        def("karate", ("ties", "src", "dst"), Some(("members", "id"))),
        false,
        false,
    )
    .await
    .unwrap();
    let view = draw(&ctx, "karate SIZE BY degree COLOR BY club", None).await;
    assert_eq!((view.result.n_nodes(), view.result.n_edges()), (34, 78));
    check_fixture("graph_view_karate", &view);

    // Rows with text ids, a row skipped, ten categories (two are "other"),
    // a node with no value, and size and colour by one name.
    let view = draw(
        &ctx,
        "SELECT 'n' || CAST(value AS VARCHAR) AS src, \
         CASE WHEN value = 7 THEN NULL WHEN value = 24 THEN 'é' ELSE 'n' || CAST(value + 1 AS VARCHAR) END AS dst \
         FROM generate_series(1, 24) \
         NODES (SELECT 'n' || CAST(value AS VARCHAR) AS id, value % 10 AS \"Rank\" FROM generate_series(1, 24)) \
         SIZE BY \"Rank\" COLOR BY \"Rank\"",
        None,
    )
    .await;
    assert_eq!((view.result.n_nodes(), view.result.n_edges()), (25, 23));
    assert_eq!(view.skipped, 1);
    let legend = view.legend().unwrap();
    assert_eq!((legend.other, legend.missing), (4, 1));
    assert!(color(&view).contains(&(MAX_CATEGORIES as u32)));
    let by = view.result.encodings().color_by.clone().unwrap();
    let Values::Dict { codes, .. } = &view.result.node_column(&by).unwrap().values else {
        panic!("a dictionary column");
    };
    assert!(codes.contains(&NULL) && codes.contains(&9));
    check_fixture("graph_view_rows", &view);
}
