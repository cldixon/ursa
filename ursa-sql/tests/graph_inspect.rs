//! The inspector's rows (#444) through Ursa's whole dialect: what
//! `ursa_engine::graph::view::inspect` reads of the karate club graph's
//! tables for a node and an edge of a drawing, and what it says when there
//! is no table to read.

use std::path::{Path, PathBuf};

use datafusion::prelude::SessionContext;
use ursa_engine::graph::view::inspect::{graph_neighbours, inspect, Inspect, Inspected};
use ursa_engine::graph::view::{of_command, GraphView, RunSql};
use ursa_sql::Sql;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data")
        .join(file)
}

async fn karate() -> SessionContext {
    let ctx = ursa_engine::new_session();
    Sql.run(
        &ctx,
        &format!(
            "CREATE VIEW members AS SELECT * FROM '{}';
             CREATE VIEW ties AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH karate
               VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                 DESTINATION KEY (dst) REFERENCES members (id));",
            data("karate_nodes.csv").display(),
            data("karate_edges.csv").display()
        ),
    )
    .await
    .unwrap();
    ctx
}

async fn draw(ctx: &SessionContext, text: &str) -> GraphView {
    of_command(ctx, &Sql, text, None)
        .await
        .unwrap_or_else(|e| panic!("{text}: {}", ursa_engine::user_message(&e)))
}

fn node(view: &GraphView, key: &str) -> u32 {
    view.result
        .node_keys()
        .iter()
        .position(|k| k == key)
        .unwrap_or_else(|| panic!("no node {key}")) as u32
}

/// The drawing's first edge between the nodes keyed `a` and `b`, either way.
fn edge(view: &GraphView, a: &str, b: &str) -> u32 {
    let (a, b) = (node(view, a), node(view, b));
    let r = &view.result;
    (0..r.n_edges())
        .find(|&e| (r.src()[e], r.dst()[e]) == (a, b) || (r.src()[e], r.dst()[e]) == (b, a))
        .expect("an edge between them") as u32
}

async fn node_row(ctx: &SessionContext, view: &GraphView, key: &str) -> Inspected {
    let what = Inspect::Node {
        node: node(view, key),
        key,
    };
    inspect(ctx, &Sql, &view.result, what).await.unwrap()
}

async fn edge_row(ctx: &SessionContext, view: &GraphView, a: &str, b: &str) -> Inspected {
    let at = edge(view, a, b);
    let keys = view.result.node_keys();
    let what = Inspect::Edge {
        edge: at,
        src: keys.get(view.result.src()[at as usize] as usize),
        dst: keys.get(view.result.dst()[at as usize] as usize),
    };
    inspect(ctx, &Sql, &view.result, what).await.unwrap()
}

/// One row's value in the column `name`.
fn value<'a>(found: &'a Inspected, row: usize, name: &str) -> Option<&'a str> {
    let at = found
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column {name} in {:?}", found.columns));
    found.rows[row][at].as_deref()
}

#[tokio::test]
async fn a_node_of_a_graph_shows_its_vertex_table_row() {
    let ctx = karate().await;
    let view = draw(&ctx, "karate").await;
    let found = node_row(&ctx, &view, "33").await;
    assert_eq!(found.table.as_deref(), Some("members"));
    assert_eq!(
        found.sql.as_deref(),
        Some("SELECT * FROM members WHERE id = 33")
    );
    assert_eq!(found.rows.len(), 1);
    assert_eq!(value(&found, 0, "id"), Some("33"));
    assert_eq!(value(&found, 0, "club"), Some("Officer"));
    assert_eq!(found.note, None);
    // What SIZE BY can read is marked.
    let numeric = |name: &str| {
        found
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap()
            .numeric
    };
    assert!(numeric("id") && !numeric("club"));
    // The query it names gives the same row.
    let (_, batches) = Sql.run(&ctx, found.sql.as_deref().unwrap()).await.unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);

    // Its degree in the whole graph, though a drawing shows part of it.
    assert_eq!(
        graph_neighbours(&ctx, &Sql, &view.result, node(&view, "33"))
            .await
            .unwrap(),
        Some(17)
    );
    let part = draw(
        &ctx,
        "SELECT * FROM hop(karate, seeds => [33], steps => 1, direction => 'both') LIMIT 4",
    )
    .await;
    let at = node(&part, "33");
    assert_eq!(
        graph_neighbours(&ctx, &Sql, &part.result, at)
            .await
            .unwrap(),
        Some(17)
    );
    assert_eq!(
        value(&node_row(&ctx, &part, "33").await, 0, "club"),
        Some("Officer")
    );
}

#[tokio::test]
async fn an_edge_of_a_graph_shows_its_edge_table_row() {
    let ctx = karate().await;
    let view = draw(&ctx, "karate").await;
    let found = edge_row(&ctx, &view, "0", "1").await;
    assert_eq!(found.table.as_deref(), Some("ties"));
    assert_eq!(found.rows.len(), 1, "{found:?}");
    assert_eq!(
        (value(&found, 0, "src"), value(&found, 0, "dst")),
        (Some("0"), Some("1"))
    );
    assert_eq!(found.note, None);
    let sql = found.sql.as_deref().unwrap();
    assert!(sql.starts_with("SELECT * FROM ties WHERE "), "{sql}");
    let (_, batches) = Sql.run(&ctx, sql).await.unwrap();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);

    // A path's steps run against the way the table stores some edges: the
    // row is found either way.
    let path = draw(
        &ctx,
        "SELECT * FROM shortest_path(karate, source => 16, target => 26, direction => 'both')",
    )
    .await;
    let r = &path.result;
    for e in 0..r.n_edges() {
        let keys = r.node_keys();
        let (src, dst) = (keys.get(r.src()[e] as usize), keys.get(r.dst()[e] as usize));
        let what = Inspect::Edge {
            edge: e as u32,
            src,
            dst,
        };
        let found = inspect(&ctx, &Sql, r, what).await.unwrap();
        assert_eq!(found.rows.len(), 1, "{src}-{dst}: {found:?}");
        let ends = [value(&found, 0, "src"), value(&found, 0, "dst")];
        assert!(
            ends == [Some(src), Some(dst)] || ends == [Some(dst), Some(src)],
            "{src}-{dst}: {ends:?}"
        );
    }
}

#[tokio::test]
async fn parallel_edges_are_told_apart_by_row_or_all_given() {
    let ctx = ursa_engine::new_session();
    Sql.run(
        &ctx,
        "CREATE TABLE people AS SELECT * FROM (VALUES (1, 'a'), (2, '<b>bold</b>'), (3, NULL)) \
           AS t(id, name);
         CREATE TABLE calls AS SELECT * FROM (VALUES (1, 2, 'mon'), (2, 3, 'tue'), (1, 2, 'wed')) \
           AS t(src, dst, day);
         CREATE PROPERTY GRAPH phone
           VERTEX TABLES (people KEY (id))
           EDGE TABLES (calls SOURCE KEY (src) REFERENCES people (id)
                              DESTINATION KEY (dst) REFERENCES people (id));",
    )
    .await
    .unwrap();
    // `.graph NAME` knows no row numbers: both rows between 1 and 2.
    let view = draw(&ctx, "phone").await;
    let found = edge_row(&ctx, &view, "1", "2").await;
    assert_eq!(found.rows.len(), 2);
    assert_eq!(
        (value(&found, 0, "day"), value(&found, 1, "day")),
        (Some("mon"), Some("wed"))
    );
    assert!(
        found
            .note
            .as_deref()
            .unwrap()
            .starts_with("2 rows of calls join these two nodes"),
        "{:?}",
        found.note
    );

    // A drawing of nodes has the graph's edges among them, each with its
    // row (induced_edges gives it): the row itself.
    let rows = draw(&ctx, "SELECT * FROM degree(phone, direction => 'both')").await;
    assert!(rows.result.edge_rows().is_some());
    let r = &rows.result;
    let mut days = Vec::new();
    for e in 0..r.n_edges() {
        let keys = r.node_keys();
        let what = Inspect::Edge {
            edge: e as u32,
            src: keys.get(r.src()[e] as usize),
            dst: keys.get(r.dst()[e] as usize),
        };
        let found = inspect(&ctx, &Sql, r, what).await.unwrap();
        assert_eq!(found.rows.len(), 1, "{found:?}");
        assert_eq!(found.note, None);
        assert_eq!(found.row, r.edge_row(e));
        days.push((
            found.row.unwrap(),
            value(&found, 0, "day").unwrap().to_string(),
        ));
    }
    days.sort();
    assert_eq!(
        days,
        [
            (0, "mon".to_string()),
            (1, "tue".to_string()),
            (2, "wed".to_string())
        ]
    );

    // Values are text as they are, markup and all, and NULL is none.
    assert_eq!(
        value(&node_row(&ctx, &view, "2").await, 0, "name"),
        Some("<b>bold</b>")
    );
    assert_eq!(value(&node_row(&ctx, &view, "3").await, 0, "name"), None);
}

#[tokio::test]
async fn a_drawing_with_no_table_says_so_and_a_stale_index_is_an_error() {
    let ctx = karate().await;
    // Plain rows: no property graph.
    let rows = draw(&ctx, "SELECT 1 AS src, 2 AS dst").await;
    let found = node_row(&ctx, &rows, "1").await;
    assert!(found.rows.is_empty() && found.table.is_none());
    assert!(
        found
            .note
            .as_deref()
            .unwrap()
            .contains("not of a property graph"),
        "{:?}",
        found.note
    );
    assert!(edge_row(&ctx, &rows, "1", "2").await.rows.is_empty());
    assert_eq!(
        graph_neighbours(&ctx, &Sql, &rows.result, 0).await.unwrap(),
        None
    );

    // A graph with no vertex tables.
    Sql.run(
        &ctx,
        "CREATE PROPERTY GRAPH bare EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
    )
    .await
    .unwrap();
    let bare = draw(&ctx, "bare").await;
    let found = node_row(&ctx, &bare, "33").await;
    assert!(found.rows.is_empty());
    assert!(found.note.as_deref().unwrap().contains("no vertex tables"));
    assert_eq!(edge_row(&ctx, &bare, "0", "1").await.rows.len(), 1);

    // The asker's picture is not the session's.
    let view = draw(&ctx, "karate").await;
    let stale = Inspect::Node {
        node: node(&view, "33"),
        key: "32",
    };
    let e = inspect(&ctx, &Sql, &view.result, stale).await.unwrap_err();
    assert!(
        ursa_engine::user_message(&e).contains("the drawing has changed"),
        "{e}"
    );
    let past = Inspect::Edge {
        edge: 10_000,
        src: "0",
        dst: "1",
    };
    assert!(inspect(&ctx, &Sql, &view.result, past).await.is_err());
}

/// 3,000 edges, each with its row number as `w`, written in that order:
/// - rows 0..2700 are 300 pairs, each nine times over (parallel edges);
/// - rows 2700..2970 are one edge each;
/// - rows 2970..2995 all join 7000 to 7001 (more than the inspector shows);
/// - rows 2995..3000 join 8000 and 8001, the even rows one way and the odd
///   the other.
const EDGES: &str = "SELECT \
       CASE WHEN i < 2700 THEN i % 50 WHEN i < 2970 THEN 200 + i WHEN i < 2995 THEN 7000 \
            ELSE 8000 + i % 2 END AS src, \
       CASE WHEN i < 2700 THEN 50 + i % 60 WHEN i < 2970 THEN 5000 + i WHEN i < 2995 THEN 7001 \
            ELSE 8001 - i % 2 END AS dst, \
       i AS w, 'row ' || i AS name \
     FROM (SELECT value AS i FROM range(0, 3000)) ORDER BY i";

/// A session that reads a table as one partition (as the browser's does),
/// with the graph `g` of the edge table `edges`, made by `create`.
async fn graph_of_edges(create: &str) -> SessionContext {
    let ctx = ursa_engine::new_session();
    Sql.run(
        &ctx,
        &format!(
            "SET datafusion.execution.target_partitions = 1;
             {create};
             CREATE PROPERTY GRAPH g EDGE TABLES (edges SOURCE KEY (src) DESTINATION KEY (dst));"
        ),
    )
    .await
    .unwrap_or_else(|e| panic!("{create}: {}", ursa_engine::user_message(&e)));
    ctx
}

/// The `w` of each of an inspected edge's rows.
fn ws(found: &Inspected) -> Vec<u64> {
    (0..found.rows.len())
        .map(|row| value(found, row, "w").unwrap().parse().unwrap())
        .collect()
}

/// What the inspector says of the edges of `g` (see [`EDGES`]), whose rows
/// are found through the index when `indexed`: the rows the query by the two
/// ends gives, in the table's order.
async fn check_edge_rows(ctx: &SessionContext, indexed: bool, what: &str) {
    // `.graph g` knows no row numbers: every row between the two ends.
    let whole = draw(ctx, "g").await;
    assert!(whole.result.edge_rows().is_none());

    // One row between two nodes: that row, and the query that reads it.
    let one = edge_row(ctx, &whole, "3000", "7800").await;
    assert_eq!(one.indexed, indexed, "{what}");
    assert_eq!(
        (ws(&one), one.note.as_deref()),
        (vec![2800], None),
        "{what}"
    );
    assert_eq!(value(&one, 0, "name"), Some("row 2800"));
    assert_eq!(
        one.sql.as_deref(),
        Some(
            "SELECT * FROM edges WHERE (src = 3000 AND dst = 7800) OR (src = 7800 AND dst = 3000)"
        )
    );
    let names: Vec<_> = one.columns.iter().map(|c| (&*c.name, c.numeric)).collect();
    assert_eq!(
        names,
        [("src", true), ("dst", true), ("w", true), ("name", false)]
    );

    // Parallel edges with no row number: all nine, in the table's order.
    let nine = edge_row(ctx, &whole, "23", "53").await;
    assert_eq!(nine.indexed, indexed, "{what}");
    assert_eq!(
        ws(&nine),
        (0..9).map(|k| 123 + 300 * k).collect::<Vec<_>>(),
        "{what}"
    );
    assert!(
        nine.note
            .as_deref()
            .unwrap()
            .starts_with("9 rows of edges join these two nodes"),
        "{what}: {:?}",
        nine.note
    );

    // More than the inspector shows: the first twenty, and it says so.
    let many = edge_row(ctx, &whole, "7000", "7001").await;
    assert_eq!(many.indexed, indexed, "{what}");
    assert_eq!(ws(&many), (2970..2990).collect::<Vec<_>>(), "{what}");
    assert!(
        many.note
            .as_deref()
            .unwrap()
            .starts_with("more than 20 rows of edges"),
        "{what}: {:?}",
        many.note
    );

    // Rows both ways between two nodes, with no row number: all five,
    // whichever way the edge is asked for.
    for (a, b) in [("8000", "8001"), ("8001", "8000")] {
        let what = Inspect::Edge {
            edge: edge(&whole, "8000", "8001"),
            src: a,
            dst: b,
        };
        // The drawing has the edge one way; asked the other way, the keys
        // are not the drawing's.
        let r = &whole.result;
        let drawn = r
            .node_keys()
            .get(r.src()[edge(&whole, a, b) as usize] as usize)
            == a;
        let found = inspect(ctx, &Sql, r, what).await;
        if drawn {
            let found = found.unwrap();
            assert_eq!(found.indexed, indexed);
            assert_eq!(ws(&found), (2995..3000).collect::<Vec<_>>());
        } else {
            assert!(found.is_err());
        }
    }

    // An edge drawn against the way the table stores it (a query's rows,
    // with no row number): its row all the same.
    let turned = draw(
        ctx,
        "SELECT dst AS src, src AS dst FROM edges WHERE w IN (2800, 2996, 150) ON g",
    )
    .await;
    assert!(turned.result.edge_rows().is_none());
    let back = edge_row(ctx, &turned, "7800", "3000").await;
    assert_eq!(back.indexed, indexed, "{what}");
    assert_eq!(ws(&back), [2800], "{what}");
    assert_eq!(
        (value(&back, 0, "src"), value(&back, 0, "dst")),
        (Some("3000"), Some("7800"))
    );
    assert_eq!(
        ws(&edge_row(ctx, &turned, "8001", "8000").await),
        (2995..3000).collect::<Vec<_>>()
    );
    assert_eq!(
        ws(&edge_row(ctx, &turned, "50", "0").await),
        (0..9).map(|k| 300 * k).collect::<Vec<_>>()
    );

    // A drawing whose edges have their row numbers (induced_edges gives
    // them): each edge's own row, parallel edges too, the way it is stored.
    let rows = draw(ctx, "SELECT * FROM degree(g, direction => 'both')").await;
    let r = &rows.result;
    assert_eq!(r.n_edges(), 3000);
    for e in (0..r.n_edges()).step_by(41).chain(2960..3000) {
        let keys = r.node_keys();
        let (src, dst) = (keys.get(r.src()[e] as usize), keys.get(r.dst()[e] as usize));
        let asked = Inspect::Edge {
            edge: e as u32,
            src,
            dst,
        };
        let found = inspect(ctx, &Sql, r, asked).await.unwrap();
        let row = r.edge_row(e).unwrap();
        assert_eq!(found.row, Some(row));
        assert_eq!(found.indexed, indexed, "{what}: row {row}");
        if indexed || row >= 2700 && !(2970..2995).contains(&row) {
            // (Read as several partitions, a table's parallel edges cannot
            // be told apart by row: the scan gives them all, with a note.)
            assert_eq!(
                (ws(&found), found.note.as_deref()),
                (vec![row], None),
                "{what}"
            );
            assert_eq!(
                (value(&found, 0, "src"), value(&found, 0, "dst")),
                (Some(src), Some(dst))
            );
            let sql = found.sql.as_deref().unwrap();
            let single = (2700..2970).contains(&row);
            assert_eq!(
                sql,
                if single {
                    format!("SELECT * FROM edges WHERE src = {src} AND dst = {dst}")
                } else {
                    format!("SELECT * FROM edges LIMIT 1 OFFSET {row}")
                }
            );
            // The query shown reads the same row.
            let (_, batches) = Sql
                .run(ctx, &format!("SELECT w FROM ({sql})"))
                .await
                .unwrap();
            let shown: Vec<String> = batches
                .iter()
                .flat_map(|b| {
                    let w = datafusion::arrow::compute::cast(
                        b.column(0),
                        &datafusion::arrow::datatypes::DataType::Utf8,
                    )
                    .unwrap();
                    let w = datafusion::arrow::array::AsArray::as_string::<i32>(&w).clone();
                    (0..w.len())
                        .map(|i| w.value(i).to_string())
                        .collect::<Vec<_>>()
                })
                .collect();
            assert_eq!(shown, [row.to_string()], "{sql}");
        }
    }
}

#[tokio::test]
async fn an_edges_rows_are_found_through_the_index_and_are_the_scans() {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str| dir.path().join(name).display().to_string();
    // The edges as Parquet (six row groups), CSV, and a table in memory.
    let writer = ursa_engine::new_session();
    Sql.run(
        &writer,
        &format!(
            "SET datafusion.execution.target_partitions = 1;
             SET datafusion.execution.parquet.max_row_group_size = 500;
             COPY ({EDGES}) TO '{}';
             COPY ({EDGES}) TO '{}';",
            file("edges.parquet"),
            file("edges.csv")
        ),
    )
    .await
    .unwrap();

    // Read as one partition, an edge's rows come through the index: from a
    // Parquet file by a row selection, from the others by passing over rows.
    let parquet = format!(
        "CREATE VIEW edges AS SELECT * FROM '{}'",
        file("edges.parquet")
    );
    let csv = format!("CREATE VIEW edges AS SELECT * FROM '{}'", file("edges.csv"));
    let memory = format!("CREATE TABLE edges AS {EDGES}");
    let narrowed = format!(
        "CREATE VIEW edges AS SELECT src, dst, w, name FROM '{}'",
        file("edges.parquet")
    );
    for (what, create) in [
        ("parquet", &parquet),
        ("csv", &csv),
        ("memory", &memory),
        ("a view's columns", &narrowed),
    ] {
        let ctx = graph_of_edges(create).await;
        check_edge_rows(&ctx, true, what).await;
    }

    // A view that keeps some rows only: its rows are not the file's, and
    // the rows read are still the view's.
    let ctx = graph_of_edges(&format!(
        "CREATE VIEW edges AS SELECT * FROM '{}' WHERE w >= 2700 OR w % 300 = 123",
        file("edges.parquet")
    ))
    .await;
    let whole = draw(&ctx, "g").await;
    let nine = edge_row(&ctx, &whole, "23", "53").await;
    assert!(nine.indexed);
    assert_eq!(ws(&nine), (0..9).map(|k| 123 + 300 * k).collect::<Vec<_>>());
    assert_eq!(ws(&edge_row(&ctx, &whole, "3000", "7800").await), [2800]);

    // Read as several partitions, a row number finds nothing: the scan, and
    // the same rows.
    let ctx = graph_of_edges(&parquet).await;
    Sql.run(
        &ctx,
        "SET datafusion.execution.target_partitions = 4;
         SET datafusion.optimizer.repartition_file_min_size = 1;",
    )
    .await
    .unwrap();
    check_edge_rows(&ctx, false, "parquet, as four partitions").await;

    // A graph with two edge tables has no one index: the scan.
    let ctx = graph_of_edges(&memory).await;
    Sql.run(
        &ctx,
        "CREATE TABLE more AS SELECT 1 AS src, 2 AS dst;
         CREATE PROPERTY GRAPH two EDGE TABLES (
           edges SOURCE KEY (src) DESTINATION KEY (dst),
           more SOURCE KEY (src) DESTINATION KEY (dst));",
    )
    .await
    .unwrap();
    let two = draw(&ctx, "SELECT src, dst FROM edges WHERE w = 2800 ON two").await;
    let found = edge_row(&ctx, &two, "3000", "7800").await;
    assert!(!found.indexed);
    assert_eq!(ws(&found), [2800]);
}
