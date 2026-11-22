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
    interpret, interpret_with, of_command, parse, DataFusionSql, GraphView, Rows, RunSql,
    MAX_CATEGORIES,
};
use ursa_engine::graph::{create_graph, EdgeTable, GraphDef, VertexTable};
use ursa_engine::{new_session, user_message};
use ursa_viz::model::{GraphResult, Key, KeyType, Layout, Path as ModelPath, Role, Values, NULL};

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
    let err = refusal(&ctx, "SELECT 1 AS a, 0.5 AS score", None).await;
    assert!(
        err.contains(
            "cannot draw this result as a graph: it has 2 columns (a: Int64, score: Float64)"
        ) && err.contains(".graph AS nodes(a)"),
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
        "provenance": serde_json::to_value(r.provenance()).unwrap(),
        "drawnAs": view.drawn_as(),
    });
    // A layout of rings or layers, with each node's level.
    if let (Some(level), true) = (
        r.highlights().distance.as_deref(),
        r.encodings().layout != Layout::Force,
    ) {
        let Values::U32(level) = &r.node_column(level).unwrap().values else {
            panic!("a u32 column");
        };
        let kind = serde_json::to_value(r.encodings().layout).unwrap();
        out["layout"] = json!({"kind": kind, "level": level});
    }
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

    // The rules (#433): a hop result as rings, and paths as layers.
    let last = rows(
        &ctx,
        "SELECT * FROM hop(karate, seeds => [0], n => 2, direction => 'both')",
    )
    .await;
    let view = draw(&ctx, "", last).await;
    assert_eq!((view.result.n_nodes(), view.result.n_edges()), (26, 59));
    check_fixture("graph_view_hop", &view);
    let view = draw(
        &ctx,
        "SELECT * FROM (VALUES (2, [16, 5, 0]), (2, [16, 6, 0])) AS t(steps, members)",
        None,
    )
    .await;
    assert_eq!(view.drawn_as(), "2 paths (members)");
    check_fixture("graph_view_paths", &view);
}

/// A session with karate as a property graph.
async fn karate() -> SessionContext {
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
    ctx
}

async fn interpreted(ctx: &SessionContext, sql: &str) -> GraphView {
    let (schema, batches) = DataFusionSql.run(ctx, sql).await.unwrap();
    interpret(ctx, &DataFusionSql, schema, batches)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", user_message(&e)))
}

fn u32s<'a>(view: &'a GraphView, column: &str) -> &'a [u32] {
    match &view.result.node_column(column).expect(column).values {
        Values::U32(values) => values,
        other => panic!("{other:?}"),
    }
}

/// Every edge joins nodes that are neighbours in karate, and is one of its
/// edge table's rows.
async fn real_edges(ctx: &SessionContext, view: &GraphView) {
    let (_, ties) = DataFusionSql
        .run(ctx, "SELECT src, dst FROM ties")
        .await
        .unwrap();
    let mut pairs = Vec::new();
    for b in &ties {
        let cast = |i: usize| {
            datafusion::arrow::compute::cast(
                b.column(i),
                &datafusion::arrow::datatypes::DataType::Int64,
            )
            .unwrap()
        };
        let (s, d) = (cast(0), cast(1));
        let (s, d) = (
            s.as_any()
                .downcast_ref::<datafusion::arrow::array::Int64Array>()
                .unwrap()
                .clone(),
            d.as_any()
                .downcast_ref::<datafusion::arrow::array::Int64Array>()
                .unwrap()
                .clone(),
        );
        pairs.extend((0..b.num_rows()).map(|i| (s.value(i), d.value(i))));
    }
    let r = &view.result;
    let key = |i: u32| match r.node_key(i as usize) {
        Key::Int(v) => v,
        other => panic!("{other:?}"),
    };
    for e in 0..r.n_edges() {
        let ends = (key(r.src()[e]), key(r.dst()[e]));
        let row = r.edge_row(e).expect("the edge's row") as usize;
        assert_eq!(pairs[row], ends, "edge {e}");
    }
}

#[tokio::test]
async fn a_hop_result_is_rings_with_the_graphs_real_edges() {
    let ctx = karate().await;
    let view = interpreted(
        &ctx,
        "SELECT * FROM hop(karate, seeds => [0], n => 2, direction => 'both')",
    )
    .await;
    let r = &view.result;
    assert_eq!(r.provenance().rule.as_deref(), Some("rings"));
    assert_eq!(view.drawn_as(), "rings on karate (seed, id, distance)");
    assert_eq!(r.graph(), Some("karate"));
    // The seed and the 25 nodes within two steps; not a star: the 59 edges
    // of karate among them, each with its row.
    assert_eq!((r.n_nodes(), r.n_edges()), (26, 59));
    real_edges(&ctx, &view).await;
    assert_eq!(r.node_labels()[0].name, "members");
    assert_eq!(r.edge_labels(), ["ties"]);
    // The seed, and each node's distance: 0, then 16 at 1 and 9 at 2.
    let seed = r.node(0, &Key::Int(0)).unwrap();
    assert_eq!(r.highlights().seeds, [seed]);
    assert_eq!(r.highlights().distance.as_deref(), Some("distance"));
    let distance = u32s(&view, "distance");
    assert_eq!(distance[seed as usize], 0);
    let count = |d: u32| distance.iter().filter(|&&x| x == d).count();
    assert_eq!((count(0), count(1), count(2)), (1, 16, 9));
    assert_eq!(
        r.node_column("distance").unwrap().role,
        Some(Role::Distance)
    );
    // One seed: rings. Distance colours the nodes, nearest first.
    assert_eq!(r.encodings().layout, Layout::Rings);
    let legend = view.legend().unwrap();
    assert_eq!(legend.by, "distance");
    assert_eq!(legend.labels, ["0", "1", "2"]);
    assert_eq!(legend.counts, [1, 16, 9]);

    // Several seeds: layers, by distance from the nearest; COLOR BY says
    // otherwise.
    let last = rows(
        &ctx,
        "SELECT * FROM hop(karate, seeds => [0, 33], n => 1, direction => 'both')",
    )
    .await;
    let view = draw(&ctx, "COLOR BY club", last).await;
    let r = &view.result;
    assert_eq!(r.encodings().layout, Layout::Layers);
    assert_eq!(r.highlights().seeds.len(), 2);
    assert_eq!(view.legend().unwrap().by, "club");
    let distance = u32s(&view, "distance");
    assert!(distance.iter().all(|&d| d <= 1), "{distance:?}");
    real_edges(&ctx, &view).await;

    // Without metadata (a file read back, say), by the three names; the
    // graph is the session's only one.
    let view = interpreted(
        &ctx,
        "SELECT * FROM (VALUES (0, 1, 1), (0, 2, 1), (0, 9, 2)) AS t(seed, id, distance)",
    )
    .await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "rings on karate (seed, id, distance)");
    assert_eq!(ids(&view), ["0", "1", "2", "9"]);
    // 0–1, 0–2, 1–2 and 2–9.
    assert_eq!(r.n_edges(), 4);
    real_edges(&ctx, &view).await;

    // With no graph to read the edges from: each seed's edges to the nodes
    // one step away, and the status line says so.
    let alone = new_session();
    let view = interpreted(
        &alone,
        "SELECT * FROM (VALUES (0, 1, 1), (0, 2, 1), (0, 9, 2)) AS t(seed, id, distance)",
    )
    .await;
    assert_eq!(ends(&view), (&[0, 0][..], &[1, 2][..]));
    assert_eq!(view.result.n_nodes(), 4);
    assert_eq!(
        view.drawn_as(),
        "rings (seed, id, distance) · there is no property graph to read the edges from: only \
         each seed's edges to the nodes one step away are drawn"
    );
    // A hop result without its distance: edges seed–id, with a note.
    let view = interpreted(
        &ctx,
        "SELECT seed, id FROM hop(karate, seeds => [0], n => 1, direction => 'both')",
    )
    .await;
    assert_eq!(view.result.provenance().rule.as_deref(), Some("edges"));
    assert_eq!(view.result.n_edges(), 16);
    assert!(view.drawn_as().contains("no distance column, so no rings"));
}

#[tokio::test]
async fn a_node_table_is_nodes_with_the_edges_among_them() {
    let ctx = karate().await;
    // The five top nodes by PageRank, sized by it, with the edges among them.
    let view = interpreted(
        &ctx,
        "SELECT * FROM pagerank(karate, direction => 'both') ORDER BY pagerank DESC LIMIT 5",
    )
    .await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "nodes of karate (id, pagerank)");
    assert_eq!(ids(&view), ["33", "0", "32", "2", "1"]);
    assert_eq!(r.encodings().size_by.as_deref(), Some("pagerank"));
    assert!(size(&view).windows(2).all(|w| w[0] >= w[1]));
    // 33–32, 0–2, 0–1, 32–2, 2–1.
    assert_eq!(r.n_edges(), 5);
    real_edges(&ctx, &view).await;

    // Every node, with a metric and a category: the graph, sized and
    // coloured.
    let view = interpreted(
        &ctx,
        "SELECT * FROM algorithms(karate, pagerank(direction => 'both'), \
         community => louvain(seed => 1))",
    )
    .await;
    let r = &view.result;
    assert_eq!((r.n_nodes(), r.n_edges()), (34, 78));
    assert_eq!(view.drawn_as(), "nodes of karate (id, pagerank, community)");
    assert_eq!(
        (
            r.encodings().size_by.as_deref(),
            r.encodings().color_by.as_deref()
        ),
        (Some("pagerank"), Some("community"))
    );
    assert_eq!(view.legend().unwrap().counts.iter().sum::<u32>(), 34);
    real_edges(&ctx, &view).await;

    // What the clauses say comes first, and reads the result's own columns
    // and the graph's.
    let last = rows(&ctx, "SELECT * FROM pagerank(karate)").await;
    let view = draw(&ctx, "SIZE BY degree COLOR BY club", last.clone()).await;
    assert_eq!(view.result.encodings().size_by.as_deref(), Some("degree"));
    assert_eq!(view.legend().unwrap().by, "club");
    let view = draw(&ctx, "COLOR BY louvain", last).await;
    assert_eq!(view.result.encodings().size_by.as_deref(), Some("pagerank"));
    assert_eq!(view.legend().unwrap().by, "louvain");

    // Without metadata: `id` by name, the session's only graph, and a
    // column named for what it is.
    let view = interpreted(
        &ctx,
        "SELECT id, CASE WHEN id < 10 THEN 1 ELSE 2 END AS community, club FROM members \
         WHERE id < 20",
    )
    .await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "nodes of karate (id, community)");
    assert_eq!(r.n_nodes(), 20);
    assert_eq!(view.legend().unwrap().counts, [10, 10]);
    real_edges(&ctx, &view).await;
    // One column of keys is a graph too; a key twice is one node, and a
    // NULL is skipped.
    let view = interpreted(
        &ctx,
        "SELECT * FROM (VALUES (0), (1), (1), (NULL), (2)) AS t(id)",
    )
    .await;
    assert_eq!(ids(&view), ["0", "1", "2"]);
    assert_eq!((view.result.n_edges(), view.skipped), (3, 1));

    // With no graph, the nodes are drawn without edges, and it says so.
    let alone = new_session();
    let view = interpreted(
        &alone,
        "SELECT * FROM (VALUES ('a', 0.5), ('b', 0.25)) AS t(id, pagerank)",
    )
    .await;
    let r = &view.result;
    assert_eq!((r.n_nodes(), r.n_edges()), (2, 0));
    assert_eq!(r.encodings().size_by.as_deref(), Some("pagerank"));
    assert_eq!(
        view.drawn_as(),
        "nodes (id, pagerank) · there is no property graph to read the edges from: the nodes \
         are drawn without edges"
    );
    // Ids that are not the graph's: no edges, and why.
    let view = interpreted(&ctx, "SELECT * FROM (VALUES ('a'), ('b')) AS t(id)").await;
    assert_eq!(view.result.n_edges(), 0);
    assert_eq!(view.result.graph(), None);
    assert!(
        view.drawn_as()
            .starts_with("nodes (id) · the edges of 'karate' could not be read"),
        "{}",
        view.drawn_as()
    );
}

#[tokio::test]
async fn paths_and_edges_without_metadata() {
    let ctx = new_session();
    // A list of ids is a path: the union of the rows' paths, each edge once.
    let view = interpreted(
        &ctx,
        "SELECT * FROM (VALUES (2, ['a', 'b', 'c']), (2, ['a', 'd', 'c']), (1, ['c', 'b']), \
         (0, NULL)) AS t(steps, members)",
    )
    .await;
    let r = &view.result;
    assert_eq!(r.provenance().rule.as_deref(), Some("path"));
    assert_eq!(ids(&view), ["a", "b", "c", "d"]);
    // a–b, b–c, a–d, d–c; c–b is b–c again.
    assert_eq!(ends(&view), (&[0, 1, 0, 3][..], &[1, 2, 3, 2][..]));
    assert_eq!(view.skipped, 1);
    assert_eq!(
        r.highlights().paths,
        [
            ModelPath {
                nodes: vec![0, 1, 2],
                edges: vec![0, 1]
            },
            ModelPath {
                nodes: vec![0, 3, 2],
                edges: vec![2, 3]
            },
            ModelPath {
                nodes: vec![2, 1],
                edges: vec![1]
            },
        ]
    );
    // Different ends: no layers. Each node's step is its first.
    assert_eq!(r.encodings().layout, Layout::Force);
    assert_eq!(u32s(&view, "step"), [0, 1, 0, 1]);
    // One row is a route, laid out in layers by position.
    let view = interpreted(&ctx, "SELECT [16, 5, 0, 8] AS route").await;
    assert_eq!(view.drawn_as(), "a path (route)");
    assert_eq!(view.result.encodings().layout, Layout::Layers);
    assert_eq!(u32s(&view, "step"), [0, 1, 2, 3]);
    assert_eq!(view.result.node_labels()[0].key, KeyType::Int);

    // A plain edge table is what it was: its nodes, its edges, their order.
    // A numeric `weight` is now an edge column.
    let view = interpreted(
        &ctx,
        "SELECT * FROM (VALUES (1, 2, 0.5), (2, 3, 1.5), (NULL, 3, 9.0)) AS t(src, dst, weight)",
    )
    .await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "edges (src, dst)");
    assert_eq!(ids(&view), ["1", "2", "3"]);
    assert_eq!(ends(&view), (&[0, 1][..], &[1, 2][..]));
    assert!(r.highlights().is_empty() && r.node_columns().is_empty());
    assert_eq!(
        r.edge_column("weight").unwrap().values,
        Values::F32(vec![0.5, 1.5])
    );
}

/// A result read as `clauses` say (`AS …`, `ON graph`, `LAYOUT …`).
async fn interpreted_as(
    ctx: &SessionContext,
    sql: &str,
    clauses: &str,
) -> datafusion::error::Result<GraphView> {
    let (source, style) = parse(clauses).unwrap_or_else(|e| panic!("{clauses}: {e}"));
    assert_eq!(source, "", "{clauses}");
    let (schema, batches) = DataFusionSql.run(ctx, sql).await.unwrap();
    interpret_with(ctx, &DataFusionSql, schema, batches, &style).await
}

/// `AS …` (#434) draws a result whose columns have any names, by what the
/// clause says and not by the rules; `ON graph` names the graph, and
/// `LAYOUT` the layout.
#[tokio::test]
async fn as_on_and_layout_override_the_rules() {
    let ctx = karate().await;
    let drawn = |sql: &'static str, clauses: &'static str| {
        let ctx = &ctx;
        async move {
            interpreted_as(ctx, sql, clauses)
                .await
                .unwrap_or_else(|e| panic!("{sql} {clauses}: {}", user_message(&e)))
        }
    };
    let refused = |sql: &'static str, clauses: &'static str| {
        let ctx = &ctx;
        async move { user_message(&interpreted_as(ctx, sql, clauses).await.expect_err(clauses)) }
    };

    // Edges: the rules refuse these columns (a tag and an id are no pair).
    let edges = "SELECT 'tie' AS tag, src AS x, dst AS y, 2.5 AS w FROM ties ORDER BY src, dst";
    let err = refused(edges, "").await;
    assert!(err.contains("cannot draw"), "{err}");
    let view = drawn(edges, "AS edges(x, y)").await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "edges (x, y)");
    assert_eq!((r.n_nodes(), r.n_edges()), (34, 78));
    assert_eq!(r.graph(), None);
    assert!(r.edge_columns().is_empty());
    assert_eq!(r.encodings().layout, Layout::Force);
    // The same edges as the edge table's own, in its order.
    let plain = interpreted(&ctx, "SELECT src, dst FROM ties ORDER BY src, dst").await;
    assert_eq!(ends(&view), ends(&plain));
    assert_eq!(ids(&view), ids(&plain));
    // The other way round, with a weight.
    let view = drawn(edges, "AS edges(y, x, w)").await;
    assert_eq!(view.drawn_as(), "edges (y, x, w)");
    // The first row, 0 → 1, is now 1 → 0: nodes are numbered as first seen.
    assert_eq!(ids(&view)[..2], ["1", "0"]);
    assert_eq!((ends(&view).0[0], ends(&view).1[0]), (0, 1));
    assert_eq!(
        view.result.edge_column("w").unwrap().values,
        Values::F32(vec![2.5; 78])
    );
    // ON names the graph, so an algorithm can size its nodes.
    let view = drawn(edges, "ON karate AS edges(x, y) SIZE BY pagerank").await;
    assert_eq!(view.drawn_as(), "edges of karate (x, y)");
    assert_eq!(view.result.graph(), Some("karate"));
    assert_eq!(size(&view).len(), 34);
    assert!(size(&view).iter().all(|v| *v > 0.0));

    // Nodes: `who` is the key, though the rules would take `id`.
    let nodes = "SELECT id + 100 AS id, id AS who, club FROM members WHERE id < 5 ORDER BY id";
    let view = drawn(nodes, "AS nodes(who)").await;
    assert_eq!(ids(&view), ["0", "1", "2", "3", "4"]);
    assert!(
        view.drawn_as().starts_with("nodes of karate (who"),
        "{}",
        view.drawn_as()
    );
    // Karate's ties among 0..4, each a row of its edge table.
    assert_eq!(view.result.n_edges(), 7);
    real_edges(&ctx, &view).await;
    // By the rules, the ids are 100..104, which are no one in karate.
    let by_rule = interpreted(&ctx, nodes).await;
    assert_eq!(ids(&by_rule), ["100", "101", "102", "103", "104"]);
    assert_eq!(by_rule.result.n_edges(), 0);

    // A path: the list, though the rules would draw `id` as a node.
    let path = "SELECT 'route' AS id, [16, 5, 0, 8, 33] AS stops";
    assert_eq!(
        interpreted(&ctx, path)
            .await
            .result
            .provenance()
            .rule
            .as_deref(),
        Some("nodes")
    );
    let view = drawn(path, "AS path(stops)").await;
    assert_eq!(view.drawn_as(), "a path (stops)");
    assert_eq!(ids(&view), ["16", "5", "0", "8", "33"]);
    assert_eq!(
        view.result.highlights().paths,
        [ModelPath {
            nodes: vec![0, 1, 2, 3, 4],
            edges: vec![0, 1, 2, 3]
        }]
    );
    assert_eq!(view.result.encodings().layout, Layout::Layers);
    // With its edges' rows, on the graph ON names.
    let view = drawn(
        "SELECT [0, 8, 33] AS stops, [7, 60] AS rows",
        "AS path(stops, rows) ON karate LAYOUT force",
    )
    .await;
    assert_eq!(view.drawn_as(), "a path on karate (stops, rows)");
    assert_eq!(view.result.edge_row(1), Some(60));
    assert_eq!(view.result.encodings().layout, Layout::Force);
    assert_eq!(u32s(&view, "step"), [0, 1, 2]);

    // Hops: computed columns carry no metadata, and by the rules their
    // first two are edges.
    let hops = "SELECT seed + 0 AS s, id + 0 AS i, CAST(distance AS INT) AS far \
                FROM hop(karate, seeds => [0], n => 2, direction => 'both')";
    assert_eq!(
        interpreted(&ctx, hops)
            .await
            .result
            .provenance()
            .rule
            .as_deref(),
        Some("edges")
    );
    let view = drawn(hops, "AS hops(s, i, far)").await;
    let r = &view.result;
    assert_eq!(view.drawn_as(), "rings on karate (s, i, far)");
    // What the hop result itself draws: 26 nodes, the 59 real edges.
    let own = interpreted(
        &ctx,
        "SELECT * FROM hop(karate, seeds => [0], n => 2, direction => 'both')",
    )
    .await;
    assert_eq!((r.n_nodes(), r.n_edges()), (26, 59));
    assert_eq!(ids(&view), ids(&own));
    assert_eq!(ends(&view), ends(&own));
    assert_eq!(u32s(&view, "distance"), u32s(&own, "distance"));
    assert_eq!(r.highlights().seeds, [0]);
    assert_eq!(r.encodings().layout, Layout::Rings);
    real_edges(&ctx, &view).await;
    // LAYOUT overrides the rule's layout, and nothing else.
    for (clause, layout) in [
        ("LAYOUT layers", Layout::Layers),
        ("LAYOUT force", Layout::Force),
        ("LAYOUT rings", Layout::Rings),
    ] {
        let other = interpreted_as(&ctx, hops, &format!("AS hops(s, i, far) {clause}"))
            .await
            .unwrap();
        assert_eq!(other.result.encodings().layout, layout, "{clause}");
        assert_eq!(other.result.highlights(), r.highlights(), "{clause}");
        assert_eq!(ends(&other), ends(&view), "{clause}");
    }
    // On a result the rules read too.
    let view = drawn(
        "SELECT * FROM hop(karate, seeds => [0], n => 2, direction => 'both')",
        "LAYOUT layers",
    )
    .await;
    assert_eq!(view.result.encodings().layout, Layout::Layers);

    // What cannot be done says what to write.
    for (sql, clauses, says) in [
        (
            edges,
            "AS edges(x, z)",
            "AS edges(x, z): the result has no column z. The result's columns are tag (Utf8), \
             x (Int64), y (Int64), w (Float64); name one of them",
        ),
        (
            edges,
            "AS edges(x, w)",
            "AS edges(x, w): column w is a number that is not whole (Float64), and an edge's \
             end is an id: an integer, a string or a date. The result's columns are tag (Utf8)",
        ),
        (
            edges,
            "AS edges(x, y) ON nope",
            "ON nope: there is no property graph named 'nope'. The graphs are karate; name one \
             of them",
        ),
        (
            edges,
            "AS edges(x, y) LAYOUT rings",
            "LAYOUT rings places each node by its distance from a seed or its step along a \
             path, and edges have neither. Draw a hop result or paths",
        ),
        (
            nodes,
            "AS nodes(who) LAYOUT layers",
            "and nodes have neither",
        ),
    ] {
        let err = refused(sql, clauses).await;
        assert!(err.contains(says), "{clauses}: {err}");
    }
    // A property graph is drawn whole.
    let err = refusal(&ctx, "karate AS nodes(id)", None).await;
    assert!(
        err.contains(
            "AS nodes(id) says how to read a result's rows, and 'karate' is a property graph, \
             drawn whole. Leave the clause out (.graph karate)"
        ),
        "{err}"
    );
    let err = refusal(&ctx, "karate ON karate", None).await;
    assert!(
        err.contains("'karate' is a property graph already"),
        "{err}"
    );
    let err = refusal(&ctx, "karate LAYOUT rings", None).await;
    assert!(err.contains("a property graph has neither"), "{err}");
    // LAYOUT force is what a property graph has anyway.
    let view = draw(&ctx, "karate LAYOUT force", None).await;
    assert_eq!(view.result.encodings().layout, Layout::Force);
    // The whole command: a table by name, and a query, with the clause.
    let view = draw(&ctx, "ties AS edges(dst, src)", None).await;
    assert_eq!(view.drawn_as(), "edges (dst, src)");
    let view = draw(
        &ctx,
        "SELECT m.id AS x, t.dst AS y FROM members m JOIN ties t ON t.src = m.id AS edges(x, y) \
         ON karate",
        None,
    )
    .await;
    assert_eq!(view.drawn_as(), "edges of karate (x, y)");
    assert_eq!(view.result.n_edges(), 78);
}

/// The explore session (#440) on DataFusion's SQL alone: an expansion adds
/// the neighbours of highest degree, then by key, and the same nodes and
/// edges in the same order every time. The commands as a shell runs them are
/// tested in `ursa-shell`.
#[tokio::test]
async fn an_expansion_is_capped_by_degree_then_key_and_is_the_same_every_time() {
    use ursa_engine::graph::view::Session;
    use ursa_viz::model::{GraphDelta, GraphResult, Op};

    let ctx = karate().await;
    let base = interpreted(
        &ctx,
        "SELECT * FROM hop(karate, seeds => [16], n => 1, direction => 'both')",
    )
    .await
    .result;
    assert_eq!(
        base.node_keys().iter().collect::<Vec<_>>(),
        ["16", "5", "6"]
    );

    let mut sent = Vec::new();
    for _ in 0..3 {
        let mut session = Session::new(base.clone());
        let mut bytes = Vec::new();
        let first = session
            .run(&ctx, &DataFusionSql, ".expand 5 LIMIT 2")
            .await
            .unwrap();
        // 5's neighbours not shown are 0 (degree 16) and 10 (3).
        assert_eq!(first.text, "+2 nodes, +4 edges");
        let second = session
            .run(&ctx, &DataFusionSql, ".expand 0 LIMIT 5")
            .await
            .unwrap();
        assert_eq!(
            second.text,
            "+5 nodes, +9 edges (8 more neighbours of 0 are not shown: .expand 0 again adds \
             the next)"
        );
        let keys: Vec<&str> = session.result().node_keys().iter().collect();
        // 0 has 16 neighbours, of which 5, 6 and 10 are shown. Of the rest:
        // 2 (degree 10), 1 (9), then 3 and 31 with 6 each, by key, and of
        // those with 5 (8 and 13) the lower key.
        assert_eq!(keys[3..], ["0", "10", "2", "1", "3", "31", "8"]);
        // The delta: new nodes, then new edges in the edge table's order,
        // each with its row.
        let delta = second.delta.unwrap();
        let [Op::AddNodes(nodes), Op::AddEdges(edges)] = &delta.ops[..] else {
            panic!("{:?}", delta.ops);
        };
        assert_eq!(nodes.len(), 5);
        let rows = edges.rows.as_ref().unwrap();
        assert!(rows.windows(2).all(|w| w[0] < w[1]), "{rows:?}");
        assert_eq!(
            delta.provenance.sql.as_deref(),
            Some(session.steps()[1].sql.join(";\n").as_str())
        );
        // A view that has the base applies the bytes, and undoes them.
        let mut view = GraphResult::from_bytes(&base.to_bytes()).unwrap();
        for step in session.steps() {
            let wire = step.delta.to_bytes();
            view.apply(&GraphDelta::from_bytes(&wire).unwrap()).unwrap();
            bytes.push(wire);
        }
        assert_eq!(view, *session.result());
        for step in session.steps().iter().rev() {
            view.apply(&step.inverse()).unwrap();
        }
        assert_eq!(view.to_bytes(), base.to_bytes());
        bytes.push(session.result().to_bytes());
        sent.push(bytes);
    }
    assert!(
        sent.windows(2).all(|w| w[0] == w[1]),
        "the same bytes every time"
    );
}

/// `.find` (#445) on DataFusion's SQL alone: its statements, and what they
/// select of a view and name outside it. The command as a shell runs it is
/// tested in `ursa-shell` (`find_tests.rs`).
#[tokio::test]
async fn find_asks_the_vertex_table_and_splits_the_view_from_the_graph() {
    use ursa_engine::graph::view::find::{find, NotShown};
    use ursa_engine::graph::view::Session;

    let ctx = karate().await;
    let base = interpreted(
        &ctx,
        "SELECT * FROM hop(karate, seeds => [16], n => 1, direction => 'both')",
    )
    .await
    .result;
    let session = Session::new(base.clone());
    let keys = |nodes: &[u32]| -> Vec<String> {
        let all = session.result().node_keys();
        nodes.iter().map(|&i| all.get(i as usize).into()).collect()
    };

    // A key: the key itself, then the keys it starts; the label column last.
    let found = find(&session, &ctx, &DataFusionSql, ".find 6")
        .await
        .unwrap();
    assert_eq!(
        found.sql,
        ["SELECT CAST(\"id\" AS VARCHAR) AS node, \
          CASE WHEN lower(CAST(\"id\" AS VARCHAR)) = '6' THEN 0 \
          WHEN starts_with(lower(CAST(\"id\" AS VARCHAR)), '6') THEN 1 ELSE 2 END AS rank \
          FROM members \
          WHERE starts_with(lower(CAST(\"id\" AS VARCHAR)), '6') \
          OR strpos(lower(\"club\"), '6') > 0 ORDER BY rank, \"id\""]
    );
    assert_eq!(keys(&found.selected), ["6"]);
    assert_eq!((found.matches, found.not_shown_total), (1, 0));
    assert_eq!(found.text, "Selected 1 node");

    // A condition: the rows of the vertex table, those drawn selected.
    let found = find(
        &session,
        &ctx,
        &DataFusionSql,
        ".find WHERE club = 'Mr. Hi' AND id BETWEEN 4 AND 7",
    )
    .await
    .unwrap();
    assert_eq!(
        found.sql,
        ["SELECT CAST(\"id\" AS VARCHAR) AS node FROM members \
          WHERE (club = 'Mr. Hi' AND id BETWEEN 4 AND 7) ORDER BY \"id\""]
    );
    assert_eq!(keys(&found.selected), ["5", "6"]);
    let outside = |key: &str| NotShown {
        key: key.into(),
        hidden: false,
    };
    assert_eq!(found.not_shown, [outside("4"), outside("7")]);
    assert_eq!(found.text, "Selected 2 nodes\nNot shown: 4, 7");

    // The same answer every time, and the session is as it was.
    let again = find(&session, &ctx, &DataFusionSql, ".find WHERE id < 20")
        .await
        .unwrap();
    for _ in 0..2 {
        let next = find(&session, &ctx, &DataFusionSql, ".find WHERE id < 20")
            .await
            .unwrap();
        assert_eq!(next, again);
    }
    assert_eq!(*session.result(), base);
    assert!(session.steps().is_empty());

    let e = find(&session, &ctx, &DataFusionSql, ".find WHERE rank > 1")
        .await
        .unwrap_err();
    assert!(user_message(&e).contains("rank"), "{}", user_message(&e));
}
