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
    inspect(ctx, &view.result, what).await.unwrap()
}

async fn edge_row(ctx: &SessionContext, view: &GraphView, a: &str, b: &str) -> Inspected {
    let at = edge(view, a, b);
    let keys = view.result.node_keys();
    let what = Inspect::Edge {
        edge: at,
        src: keys.get(view.result.src()[at as usize] as usize),
        dst: keys.get(view.result.dst()[at as usize] as usize),
    };
    inspect(ctx, &view.result, what).await.unwrap()
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
        "SELECT * FROM hop(karate, seeds => [33], n => 1, direction => 'both') LIMIT 4",
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
        let found = inspect(&ctx, r, what).await.unwrap();
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
        let found = inspect(&ctx, r, what).await.unwrap();
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
    let e = inspect(&ctx, &view.result, stale).await.unwrap_err();
    assert!(
        ursa_engine::user_message(&e).contains("the drawing has changed"),
        "{e}"
    );
    let past = Inspect::Edge {
        edge: 10_000,
        src: "0",
        dst: "1",
    };
    assert!(inspect(&ctx, &view.result, past).await.is_err());
}
