//! Graph algorithms as table functions: planning, errors, and the index cache.
//!
//! Graphs are defined through the session's registry, as `CREATE PROPERTY
//! GRAPH` does, over views of CSV files.

use std::path::{Path, PathBuf};

use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::common::TableReference;
use datafusion::error::Result;
use datafusion::prelude::SessionContext;
use ursa_engine::graph::{index_builds, registry_of, EdgeTable, GraphDef};
use ursa_engine::{new_session, user_message};

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

/// Define `graph` over a view `view` of the CSV at `path`, with endpoints
/// `src` and `dst`.
async fn define(ctx: &SessionContext, graph: &str, view: &str, path: &Path) {
    ctx.sql(&format!(
        "CREATE OR REPLACE VIEW {view} AS SELECT * FROM '{}'",
        path.display()
    ))
    .await
    .unwrap();
    registry_of(ctx).put(GraphDef {
        name: graph.into(),
        edges: EdgeTable {
            table: TableReference::bare(view),
            source: "src".into(),
            destination: "dst".into(),
        },
        vertices: None,
    });
}

async fn karate() -> SessionContext {
    let ctx = new_session();
    define(&ctx, "karate", "ties", &data("karate_edges.csv")).await;
    ctx
}

async fn table(ctx: &SessionContext, sql: &str) -> Result<String> {
    let batches = ctx.sql(sql).await?.collect().await?;
    Ok(pretty_format_batches(&batches)?.to_string())
}

async fn error(ctx: &SessionContext, sql: &str) -> String {
    match ctx.sql(sql).await {
        Ok(frame) => match frame.collect().await {
            Ok(_) => panic!("{sql} should have failed"),
            Err(e) => user_message(&e),
        },
        Err(e) => user_message(&e),
    }
}

/// Getting Started, "Run an algorithm".
#[tokio::test]
async fn run_an_algorithm_matches_getting_started() {
    let ctx = karate().await;
    let out = table(
        &ctx,
        "SELECT id, round(pagerank, 4) AS pagerank \
         FROM pagerank(karate, direction => 'both') \
         ORDER BY pagerank DESC LIMIT 5",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+----+----------+
| id | pagerank |
+----+----------+
| 33 | 0.1009   |
| 0  | 0.097    |
| 32 | 0.0717   |
| 2  | 0.0571   |
| 1  | 0.0529   |
+----+----------+"
    );
}

/// Getting Started, "Combine algorithms and attributes", with the node
/// attributes read from a view of the file (`karate.nodes` belongs to the
/// graph DDL), and the joins written with `ON`: DataFusion 55 cannot plan a
/// chain of four `USING` joins, for plain tables too (see
/// `four_using_joins_are_ambiguous_in_datafusion`).
#[tokio::test]
async fn combine_algorithms_and_attributes() {
    let ctx = karate().await;
    ctx.sql(&format!(
        "CREATE VIEW members AS SELECT * FROM '{}'",
        data("karate_nodes.csv").display()
    ))
    .await
    .unwrap();
    let out = table(
        &ctx,
        "SELECT m.id, club, degree, triangle_count AS triangles, louvain AS community \
         FROM members AS m \
         JOIN degree(karate, direction => 'both') AS d ON d.id = m.id \
         JOIN triangle_count(karate)               AS t ON t.id = m.id \
         JOIN louvain(karate, seed => 1)           AS l ON l.id = m.id \
         ORDER BY degree DESC \
         LIMIT 5",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+----+---------+--------+-----------+-----------+
| id | club    | degree | triangles | community |
+----+---------+--------+-----------+-----------+
| 33 | Officer | 17     | 15        | 2         |
| 0  | Mr. Hi  | 16     | 18        | 0         |
| 32 | Officer | 12     | 13        | 2         |
| 2  | Mr. Hi  | 10     | 11        | 0         |
| 1  | Mr. Hi  | 9      | 12        | 0         |
+----+---------+--------+-----------+-----------+"
    );
    // Three algorithms, one graph, one query: one index.
    assert_eq!(index_builds(&ctx), 1);
}

/// The DataFusion limitation the test above works around. When this fails,
/// DataFusion has fixed it: run the Getting Started query as written.
#[tokio::test]
async fn four_using_joins_are_ambiguous_in_datafusion() {
    let ctx = new_session();
    let message = error(
        &ctx,
        "WITH a AS (SELECT 1 AS id), b AS (SELECT 1 AS id), c AS (SELECT 1 AS id), \
         d AS (SELECT 1 AS id) \
         SELECT a.id FROM a JOIN b USING (id) JOIN c USING (id) JOIN d USING (id)",
    )
    .await;
    assert_eq!(message, "Ambiguous reference to unqualified field id");
}

#[tokio::test]
async fn three_queries_build_the_index_once() {
    let ctx = karate().await;
    for sql in [
        "SELECT * FROM pagerank(karate)",
        "SELECT * FROM degree(karate, direction => 'in')",
        "SELECT * FROM connected_components(karate, mode => 'strong')",
    ] {
        table(&ctx, sql).await.unwrap();
    }
    assert_eq!(index_builds(&ctx), 1);
    // Two graphs over the same edges share it too: the key is the edges.
    define(&ctx, "club", "ties", &data("karate_edges.csv")).await;
    table(&ctx, "SELECT * FROM triangle_count(club)")
        .await
        .unwrap();
    assert_eq!(index_builds(&ctx), 1);
}

#[tokio::test]
async fn rewriting_the_file_rebuilds_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edges.csv");
    std::fs::write(&path, "src,dst\n1,2\n2,3\n").unwrap();
    let ctx = new_session();
    define(&ctx, "g", "e", &path).await;
    let sql = "SELECT id, degree FROM degree(g, direction => 'both') ORDER BY id";
    let before = table(&ctx, sql).await.unwrap();
    assert!(before.contains("| 3  | 1      |"), "{before}");
    table(&ctx, sql).await.unwrap();
    assert_eq!(index_builds(&ctx), 1);

    // Same schema, new contents (and a new size and modification time).
    std::fs::write(&path, "src,dst\n1,2\n2,3\n3,1\n3,4\n").unwrap();
    let after = table(&ctx, sql).await.unwrap();
    assert_eq!(index_builds(&ctx), 2);
    assert_eq!(
        after,
        "\
+----+--------+
| id | degree |
+----+--------+
| 1  | 2      |
| 2  | 2      |
| 3  | 3      |
| 4  | 1      |
+----+--------+"
    );
    table(&ctx, sql).await.unwrap();
    assert_eq!(index_builds(&ctx), 2);
}

#[tokio::test]
async fn a_filtered_view_is_a_subgraph_with_its_own_index() {
    let ctx = karate().await;
    ctx.sql("CREATE VIEW near AS SELECT * FROM ties WHERE src < 10 AND dst < 10")
        .await
        .unwrap();
    registry_of(&ctx).put(GraphDef {
        name: "near".into(),
        edges: EdgeTable {
            table: TableReference::bare("near"),
            source: "src".into(),
            destination: "dst".into(),
        },
        vertices: None,
    });
    let out = table(
        &ctx,
        "SELECT count(*) AS n, max(id) AS top FROM degree(near)",
    )
    .await
    .unwrap();
    assert!(out.contains("| 10 | 9   |"), "{out}");
    table(&ctx, "SELECT * FROM degree(karate)").await.unwrap();
    assert_eq!(index_builds(&ctx), 2);
}

#[tokio::test]
async fn the_scan_reads_only_the_endpoints() {
    let ctx = new_session();
    define(&ctx, "lesmis", "scenes", &data("lesmis_edges.csv")).await;
    let plan = table(&ctx, "EXPLAIN SELECT * FROM degree(lesmis)")
        .await
        .unwrap();
    assert!(plan.contains("GraphAlgorithm: graph=lesmis"), "{plan}");
    assert!(plan.contains("GraphAlgorithmExec: graph=lesmis"), "{plan}");
    // lesmis_edges.csv has src, dst and weight; only the endpoints are read.
    assert!(plan.contains("projection=[src, dst]"), "{plan}");
    assert!(!plan.contains("weight"), "{plan}");
}

#[tokio::test]
async fn aliases_and_schema() {
    let ctx = karate().await;
    let out = table(
        &ctx,
        "SELECT p.id, p.pagerank > 0 AS positive FROM pagerank(karate) AS p WHERE p.id = 0",
    )
    .await
    .unwrap();
    assert!(out.contains("| 0  | true     |"), "{out}");
    let schema = table(&ctx, "DESCRIBE SELECT * FROM pagerank(karate)")
        .await
        .unwrap();
    assert!(
        schema.contains("| id          | Int64     | NO          |"),
        "{schema}"
    );
    assert!(
        schema.contains("| pagerank    | Float64   | NO          |"),
        "{schema}"
    );
    // Function and graph names are identifiers, normalized as table names are.
    table(&ctx, "SELECT * FROM PageRank(KARATE)").await.unwrap();
}

#[tokio::test]
async fn string_ids_keep_their_type() {
    let ctx = new_session();
    define(
        &ctx,
        "florentine",
        "marriages",
        &data("florentine_edges.csv"),
    )
    .await;
    let out = table(
        &ctx,
        "SELECT id, degree FROM degree(florentine, direction => 'both') \
         ORDER BY degree DESC, id LIMIT 2",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+----------+--------+
| id       | degree |
+----------+--------+
| Medici   | 6      |
| Guadagni | 4      |
+----------+--------+"
    );
}

#[tokio::test]
async fn planning_errors() {
    let ctx = karate().await;
    for (sql, expected) in [
        (
            "SELECT * FROM pagerank(karete)",
            "graph 'karete' does not exist; did you mean 'karate'?",
        ),
        (
            "SELECT * FROM pagerank(social)",
            "graph 'social' does not exist. The graphs are: karate",
        ),
        (
            "SELECT * FROM pagerenk(karate)",
            "there is no graph function 'pagerenk'; did you mean 'pagerank'?",
        ),
        (
            "SELECT * FROM dijkstra(karate)",
            "there is no graph function 'dijkstra'. The graph functions are: degree, \
             pagerank, connected_components, triangle_count, clustering_coefficient, \
             betweenness, closeness, label_propagation, louvain, density, avg_path_length, diameter, describe, hop, shortest_path, \
             random_walk, algorithms",
        ),
        (
            "SELECT * FROM shortest_paths(karate)",
            "there is no graph function 'shortest_paths'; did you mean 'shortest_path'?",
        ),
        (
            "SELECT * FROM pagerank(karate, dampng => 0.9)",
            "pagerank() has no parameter 'dampng'; did you mean 'damping'? Its parameters are: \
             damping, max_iter, tol, weight, direction",
        ),
        (
            "SELECT * FROM pagerank(karate, damping => 0.9, damping => 0.8)",
            "pagerank() was given 'damping' more than once",
        ),
        (
            "SELECT * FROM pagerank(karate, damping => 'high')",
            "pagerank(damping =>) must be a number; got 'high'",
        ),
        (
            "SELECT * FROM pagerank(karate, max_iter => -1)",
            "pagerank(max_iter =>) must be a non-negative integer below 2^32; got -1",
        ),
        (
            "SELECT * FROM pagerank(karate, direction => 'in')",
            "pagerank(direction => 'in') is not supported; use one of 'out', 'both'",
        ),
        (
            "SELECT * FROM pagerank(karate, damping => 1 - 0.1)",
            "pagerank(damping =>) must be a literal (a number, a string, a boolean or NULL); got \
             1 - 0.1",
        ),
        (
            "SELECT * FROM pagerank()",
            "pagerank() needs a graph as its first argument: pagerank(<graph>, damping => 0.85, \
             max_iter => 30, tol => 1e-6, weight => NULL, direction => 'out')",
        ),
        (
            "SELECT * FROM pagerank(damping => 0.9)",
            "pagerank() needs a graph as its first argument: pagerank(<graph>, damping => 0.85, \
             max_iter => 30, tol => 1e-6, weight => NULL, direction => 'out')",
        ),
        (
            "SELECT * FROM pagerank('karate')",
            "the first argument of pagerank() must be the name of a graph; got 'karate' (write \
             the name without quotes: karate)",
        ),
        (
            "SELECT * FROM pagerank(karate.edges)",
            "the first argument of pagerank() must be the name of a graph; got karate.edges",
        ),
        (
            "SELECT * FROM pagerank(karate, 0.9)",
            "pagerank() takes its parameters by name, as name => value; got 0.9. Write \
             pagerank(<graph>, damping => 0.85, max_iter => 30, tol => 1e-6, weight => NULL, direction => 'out')",
        ),
        (
            "SELECT * FROM triangle_count(karate, direction => 'both')",
            "triangle_count() takes no parameters besides the graph; got 'direction'",
        ),
    ] {
        assert_eq!(error(&ctx, sql).await, expected, "{sql}");
    }
    // A table function that is not a graph function is DataFusion's.
    let out = table(&ctx, "SELECT count(*) AS n FROM generate_series(1, 3)")
        .await
        .unwrap();
    assert!(out.contains("| 3 |"), "{out}");
}

#[tokio::test]
async fn null_endpoints_are_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edges.csv");
    std::fs::write(&path, "src,dst\n1,2\n2,\n").unwrap();
    let ctx = new_session();
    define(&ctx, "g", "e", &path).await;
    let message = error(&ctx, "SELECT * FROM degree(g)").await;
    assert!(
        message.starts_with("graph 'g': an edge has a null source or destination"),
        "{message}"
    );
}

#[tokio::test]
async fn an_empty_edge_set_has_no_nodes() {
    let ctx = karate().await;
    let out = table(
        &ctx,
        "SELECT count(*) AS n FROM (SELECT * FROM pagerank(karate) WHERE id < 0)",
    )
    .await
    .unwrap();
    assert!(out.contains("| 0 |"), "{out}");
    ctx.sql("CREATE VIEW none AS SELECT * FROM ties WHERE src < 0")
        .await
        .unwrap();
    registry_of(&ctx).put(GraphDef {
        name: "empty".into(),
        edges: EdgeTable {
            table: TableReference::bare("none"),
            source: "src".into(),
            destination: "dst".into(),
        },
        vertices: None,
    });
    for f in [
        "degree",
        "pagerank",
        "connected_components",
        "triangle_count",
        "louvain",
    ] {
        let out = table(&ctx, &format!("SELECT count(*) AS n FROM {f}(empty)"))
            .await
            .unwrap();
        assert!(out.contains("| 0 |"), "{f}: {out}");
    }
}
