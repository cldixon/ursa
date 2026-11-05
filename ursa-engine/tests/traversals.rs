//! Traversals as table functions: `hop`, `shortest_path` and `random_walk`,
//! their node-id arguments (a list, a subquery, one id), plans and errors.
//! Their values against the Python library are in `parity.rs`.

use std::path::{Path, PathBuf};

use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::common::TableReference;
use datafusion::error::Result;
use datafusion::prelude::SessionContext;
use ursa_engine::graph::{create_graph, index_builds, EdgeTable, GraphDef, VertexTable};
use ursa_engine::{new_session, user_message};

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

fn def(name: &str, edges: &str, vertices: Option<(&str, &str)>) -> GraphDef {
    GraphDef {
        name: name.into(),
        edges: EdgeTable {
            table: TableReference::bare(edges),
            source: "src".into(),
            destination: "dst".into(),
        },
        vertices: vertices.map(|(table, key)| VertexTable {
            table: TableReference::bare(table),
            key: key.into(),
        }),
    }
}

async fn view(ctx: &SessionContext, name: &str, path: &Path) {
    ctx.sql(&format!(
        "CREATE OR REPLACE VIEW {name} AS SELECT * FROM '{}'",
        path.display()
    ))
    .await
    .unwrap();
}

/// Getting Started's `karate`, with its `members` vertex table.
async fn karate() -> SessionContext {
    let ctx = new_session();
    view(&ctx, "members", &data("karate_nodes.csv")).await;
    view(&ctx, "ties", &data("karate_edges.csv")).await;
    create_graph(
        &ctx,
        def("karate", "ties", Some(("members", "id"))),
        false,
        false,
    )
    .await
    .unwrap();
    ctx
}

/// `karate` and Les Misérables (string ids, a weight column).
async fn karate_and_lesmis() -> SessionContext {
    let ctx = karate().await;
    view(&ctx, "lesmis_edges", &data("lesmis_edges.csv")).await;
    create_graph(&ctx, def("lesmis", "lesmis_edges", None), false, false)
        .await
        .unwrap();
    ctx
}

async fn table(ctx: &SessionContext, sql: &str) -> Result<String> {
    let batches = ctx.sql(sql).await?.collect().await?;
    Ok(pretty_format_batches(&batches)?.to_string())
}

async fn rows(ctx: &SessionContext, sql: &str) -> usize {
    let batches = ctx
        .sql(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .collect()
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    batches.iter().map(|b| b.num_rows()).sum()
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

/// Getting Started, "Traversals".
#[tokio::test]
async fn traversals_match_getting_started() {
    let ctx = karate().await;
    // "Member 0 has 16 friends, so this returns 16 rows."
    let sql = "SELECT * FROM hop(karate, seeds => [0], n => 1, direction => 'both')";
    assert_eq!(rows(&ctx, sql).await, 16);
    let out = table(
        &ctx,
        "SELECT * FROM hop(karate,
                           seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                           n => 2)",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+-----+-----+
| src | dst |
+-----+-----+
| 9   | 33  |
| 14  | 32  |
| 14  | 33  |
| 15  | 32  |
| 15  | 33  |
| 18  | 32  |
| 18  | 33  |
| 20  | 32  |
| 20  | 33  |
| 22  | 32  |
| 22  | 33  |
| 23  | 25  |
| 23  | 27  |
| 23  | 29  |
| 23  | 32  |
| 23  | 33  |
| 23  | 31  |
| 24  | 25  |
| 24  | 27  |
| 24  | 31  |
| 24  | 33  |
| 24  | 32  |
| 25  | 31  |
| 25  | 32  |
| 25  | 33  |
| 26  | 29  |
| 26  | 33  |
| 26  | 32  |
| 27  | 33  |
| 28  | 31  |
| 28  | 33  |
| 28  | 32  |
| 29  | 32  |
| 29  | 33  |
| 30  | 32  |
| 30  | 33  |
| 31  | 32  |
| 31  | 33  |
| 32  | 33  |
+-----+-----+"
    );
    let out = table(
        &ctx,
        "SELECT * FROM shortest_path(karate, source => 0, target => 33, direction => 'both')",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+-----+-----+-----+------+
| src | dst | hop | cost |
+-----+-----+-----+------+
| 0   | 8   | 0   | 1.0  |
| 8   | 33  | 1   | 2.0  |
+-----+-----+-----+------+"
    );
}

/// `docs/guides/traversals.md`'s random walk, from the Python library.
#[tokio::test]
async fn random_walk_matches_the_traversals_guide() {
    let ctx = karate().await;
    let out = table(
        &ctx,
        "SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => 2, \
         seed => 7) LIMIT 10",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+---------+------+------+
| walk_id | step | node |
+---------+------+------+
| 0       | 0    | 0    |
| 0       | 1    | 13   |
| 0       | 2    | 33   |
| 1       | 0    | 0    |
| 1       | 1    | 2    |
| 1       | 2    | 9    |
| 1       | 3    | 33   |
| 2       | 0    | 1    |
| 2       | 1    | 13   |
| 2       | 2    | 33   |
+---------+------+------+"
    );
    let sql = "SELECT * FROM random_walk(karate, start => [0, 1], steps => 4, walks_per_node => \
               2, seed => 7)";
    assert_eq!(rows(&ctx, sql).await, 15);
    // A fixed seed gives the same walks every time.
    assert_eq!(
        table(&ctx, sql).await.unwrap(),
        table(&ctx, sql).await.unwrap()
    );
}

#[tokio::test]
async fn node_ids_three_ways() {
    let ctx = karate_and_lesmis().await;
    let list = table(&ctx, "SELECT * FROM hop(karate, seeds => [0])")
        .await
        .unwrap();
    for same in [
        "SELECT * FROM hop(karate, seeds => 0)",
        "SELECT * FROM hop(karate, seeds => (SELECT 0))",
        "SELECT * FROM hop(karate, seeds => (SELECT id FROM karate.nodes WHERE id = 0))",
        // Cast to the graph's id type.
        "SELECT * FROM hop(karate, seeds => ['0'])",
        "SELECT * FROM hop(karate, seeds => (SELECT CAST(0 AS TINYINT)))",
    ] {
        assert_eq!(table(&ctx, same).await.unwrap(), list, "{same}");
    }
    // The seeds' order is the rows' order.
    let out = table(&ctx, "SELECT * FROM hop(karate, seeds => [33, 32])")
        .await
        .unwrap();
    assert!(out.starts_with("+-----+-----+\n| src | dst |\n+-----+-----+\n| 32  | 33  |"));
    let out = table(
        &ctx,
        "SELECT * FROM hop(lesmis, seeds => ['Valjean', 'Marius'], direction => 'both')",
    )
    .await
    .unwrap();
    assert!(out.contains("| Valjean | Labarre "), "{out}");
    assert!(out.contains("| Marius  | Cosette "), "{out}");
    let sql = "SELECT * FROM shortest_path(karate, source => (SELECT min(id) FROM karate.nodes), \
               target => (SELECT max(id) FROM karate.nodes), direction => 'both')";
    assert_eq!(rows(&ctx, sql).await, 2);
}

/// An id that is not in the graph reaches nothing, as in the Python library
/// (#225); so does a null, an empty list, or a subquery with no rows.
#[tokio::test]
async fn ids_not_in_the_graph_reach_nothing() {
    let ctx = karate_and_lesmis().await;
    for sql in [
        "SELECT * FROM hop(karate, seeds => [99])",
        "SELECT * FROM hop(karate, seeds => [])",
        "SELECT * FROM hop(karate, seeds => NULL)",
        "SELECT * FROM hop(karate, seeds => (SELECT id FROM karate.nodes WHERE id < 0))",
        "SELECT * FROM hop(lesmis, seeds => [0])",
        "SELECT * FROM shortest_path(karate, source => 0, target => 99)",
        "SELECT * FROM shortest_path(karate, source => NULL, target => 33)",
        "SELECT * FROM shortest_path(karate, source => 33, target => 0)",
        "SELECT * FROM shortest_path(karate, source => 0, target => 0)",
        "SELECT * FROM random_walk(karate, start => [99], steps => 3)",
        "SELECT * FROM hop(karate, seeds => [0], n => 0)",
    ] {
        assert_eq!(rows(&ctx, sql).await, 0, "{sql}");
    }
    // A repeated seed repeats its rows; an unknown one adds none.
    assert_eq!(
        rows(&ctx, "SELECT * FROM hop(karate, seeds => [0, 99, 0])").await,
        32
    );
}

#[tokio::test]
async fn schemas() {
    let ctx = karate_and_lesmis().await;
    let describe = |sql: &str| format!("DESCRIBE {sql}");
    let out = table(&ctx, &describe("SELECT * FROM hop(karate, seeds => [0])"))
        .await
        .unwrap();
    assert!(
        out.contains("| src         | Int64     | NO          |"),
        "{out}"
    );
    assert!(
        out.contains("| dst         | Int64     | NO          |"),
        "{out}"
    );
    let out = table(
        &ctx,
        &describe("SELECT * FROM shortest_path(lesmis, source => 'a', target => 'b')"),
    )
    .await
    .unwrap();
    assert!(
        out.contains("| src         | Utf8      | NO          |"),
        "{out}"
    );
    assert!(
        out.contains("| hop         | Int64     | NO          |"),
        "{out}"
    );
    assert!(
        out.contains("| cost        | Float64   | NO          |"),
        "{out}"
    );
    let out = table(
        &ctx,
        &describe("SELECT * FROM random_walk(lesmis, start => ['Valjean'], steps => 2)"),
    )
    .await
    .unwrap();
    assert!(
        out.contains("| walk_id     | Int64     | NO          |"),
        "{out}"
    );
    assert!(
        out.contains("| step        | Int64     | NO          |"),
        "{out}"
    );
    assert!(
        out.contains("| node        | Utf8      | NO          |"),
        "{out}"
    );
}

/// The node ids are a child plan of the traversal node, read when it runs;
/// the edge scan reads only what the traversal needs.
#[tokio::test]
async fn node_ids_are_a_child_plan() {
    let ctx = karate_and_lesmis().await;
    let plan = table(
        &ctx,
        "EXPLAIN SELECT * FROM hop(karate,
                                   seeds => (SELECT id FROM karate.nodes WHERE club = 'Officer'),
                                   n => 2)",
    )
    .await
    .unwrap();
    assert!(
        plan.contains(
            "GraphTraversal: graph=karate, edges=(ties.src, ties.dst), call=hop(seeds => \
             (SELECT id FROM karate.nodes WHERE club = 'Officer'), n => 2, direction => 'out')"
        ),
        "{plan}"
    );
    assert!(plan.contains("GraphTraversalExec: graph=karate"), "{plan}");
    assert!(plan.contains("FilterExec: club@1 = Officer"), "{plan}");
    assert!(
        plan.contains("karate_nodes.csv]]}, projection=[id, club]"),
        "{plan}"
    );

    // The weight is read only by a weighted path.
    let plan = table(
        &ctx,
        "EXPLAIN SELECT * FROM shortest_path(lesmis, source => 'Valjean', target => 'Marius')",
    )
    .await
    .unwrap();
    assert!(plan.contains("projection=[src, dst]"), "{plan}");
    assert!(!plan.contains("weight"), "{plan}");
    let plan = table(
        &ctx,
        "EXPLAIN SELECT * FROM shortest_path(lesmis, source => 'Valjean', target => 'Marius', \
         weight => weight)",
    )
    .await
    .unwrap();
    assert!(
        plan.contains(
            "call=shortest_path(source => 'Valjean', target => 'Marius', weight => \
             weight, direction => 'out')"
        ),
        "{plan}"
    );
    assert!(plan.contains("projection=[src, dst, weight]"), "{plan}");

    // A list is a VALUES plan.
    let plan = table(&ctx, "EXPLAIN SELECT * FROM hop(karate, seeds => [0, 33])")
        .await
        .unwrap();
    assert!(plan.contains("Values: (Int64(0)), (Int64(33))"), "{plan}");
}

/// A seed subquery runs when the query runs, not when it is planned: a file it
/// reads that changes in between gives the new seeds.
#[tokio::test]
async fn seed_subqueries_run_with_the_query() {
    let dir = tempfile::tempdir().unwrap();
    let members = dir.path().join("members.csv");
    std::fs::write(&members, "id,club\n0,Officer\n33,Mr. Hi\n").unwrap();
    let ctx = karate().await;
    view(&ctx, "flags", &members).await;
    let frame = ctx
        .sql("SELECT * FROM hop(karate, seeds => (SELECT id FROM flags WHERE club = 'Officer'))")
        .await
        .unwrap();
    assert_eq!(index_builds(&ctx), 0);
    std::fs::write(&members, "id,club\n0,Mr. Hi\n1,Officer\n").unwrap();
    let batches = frame.collect().await.unwrap();
    let out = pretty_format_batches(&batches).unwrap().to_string();
    // Member 1's friends, not member 0's.
    assert!(out.contains("| 1   | 2   |"), "{out}");
    assert!(!out.contains("| 0   |"), "{out}");
}

/// Traversals and algorithms over one graph share one index.
#[tokio::test]
async fn one_index_for_algorithms_and_traversals() {
    let ctx = karate_and_lesmis().await;
    for sql in [
        "SELECT * FROM degree(karate)",
        "SELECT * FROM hop(karate, seeds => [0])",
        "SELECT * FROM shortest_path(karate, source => 0, target => 33)",
        "SELECT * FROM random_walk(karate, start => [0], steps => 3)",
    ] {
        rows(&ctx, sql).await;
    }
    assert_eq!(index_builds(&ctx), 1);
    for sql in [
        "SELECT * FROM pagerank(lesmis, weight => weight)",
        "SELECT * FROM shortest_path(lesmis, source => 'Valjean', target => 'Marius', \
         weight => weight)",
        "SELECT * FROM shortest_path(lesmis, source => 'Valjean', target => 'Marius')",
    ] {
        rows(&ctx, sql).await;
    }
    assert_eq!(index_builds(&ctx), 2);
}

/// A traversal is a relation: filter, join, aggregate and alias it.
#[tokio::test]
async fn traversals_compose() {
    let ctx = karate().await;
    let out = table(
        &ctx,
        "SELECT n.club, count(*) AS reached
         FROM hop(karate, seeds => [0], n => 2, direction => 'both') AS h
         JOIN karate.nodes AS n ON h.dst = n.id
         GROUP BY n.club ORDER BY n.club",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+---------+---------+
| club    | reached |
+---------+---------+
| Mr. Hi  | 16      |
| Officer | 9       |
+---------+---------+"
    );
    // A filter on the output stays above the traversal.
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM hop(karate, seeds => [0], direction => 'both') WHERE dst > 10"
        )
        .await,
        7
    );
    let out = table(
        &ctx,
        "SELECT max(hop) AS hops, max(cost) AS cost
         FROM shortest_path(karate, source => 0, target => 26, direction => 'both')",
    )
    .await
    .unwrap();
    assert!(out.contains("| 2    | 3.0  |"), "{out}");
    // Unaliased, the columns are qualified by the function's name.
    rows(
        &ctx,
        "SELECT hop.dst FROM hop(karate, seeds => [0]) WHERE hop.src = 0",
    )
    .await;
}

#[tokio::test]
async fn weighted_paths() {
    let ctx = karate_and_lesmis().await;
    let out = table(
        &ctx,
        "SELECT * FROM shortest_path(lesmis, source => 'Valjean', target => 'Marius', \
         weight => weight)",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+----------------+----------------+-----+------+
| src            | dst            | hop | cost |
+----------------+----------------+-----+------+
| Valjean        | Gillenormand   | 0   | 2.0  |
| Gillenormand   | LtGillenormand | 1   | 3.0  |
| LtGillenormand | Marius         | 2   | 4.0  |
+----------------+----------------+-----+------+"
    );
    assert_eq!(
        error(
            &ctx,
            "SELECT * FROM shortest_path(lesmis, source => 'Valjean', target => 'Marius', \
             weight => weight - 2)"
        )
        .await,
        "weight expression produced a negative value (-1); weights must be non-negative"
    );
}

#[tokio::test]
async fn errors_are_for_people() {
    let ctx = karate_and_lesmis().await;
    for (sql, expected) in [
        (
            "SELECT * FROM hop(karate)",
            "hop() needs 'seeds': hop(<graph>, seeds => [<id>, ...], n => 1, direction => 'out')",
        ),
        (
            "SELECT * FROM hop(karate, [0])",
            "hop() takes its parameters by name, as name => value; got [0]. Write \
             hop(<graph>, seeds => [<id>, ...], n => 1, direction => 'out')",
        ),
        (
            "SELECT * FROM hop(karate, seed => [0])",
            "hop() has no parameter 'seed'; did you mean 'seeds'? Its parameters are: seeds, \
             n, direction",
        ),
        (
            "SELECT * FROM hop(karate, seeds => [0], seeds => [1])",
            "hop() was given 'seeds' more than once",
        ),
        (
            "SELECT * FROM hop(karate, seeds => [0], n => -1)",
            "hop(n =>) must be a non-negative integer below 2^32; got -1",
        ),
        (
            "SELECT * FROM hop(karate, seeds => [0], direction => 'up')",
            "hop(direction => 'up') is not supported; use one of 'out', 'in', 'both'",
        ),
        (
            "SELECT * FROM hop(karate, seeds => [0], weight => weight)",
            "hop() has no parameter 'weight'. Its parameters are: seeds, n, direction",
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT id, club FROM karate.nodes))",
            "hop(seeds =>) must be a query of one column of node ids; it returns 2 columns",
        ),
        (
            "SELECT * FROM hop(lesmis, seeds => [Valjean])",
            "hop(seeds =>): Valjean is not a node id (a string id is quoted: 'Valjean'): \
             No field named valjean.",
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT id FROM nowhere))",
            "hop(seeds =>): table 'datafusion.public.nowhere' not found",
        ),
        (
            "SELECT * FROM shortest_path(karate, source => [0, 1], target => 33)",
            "shortest_path(source =>) takes one node id, not a list; got [0, 1]",
        ),
        (
            "SELECT * FROM shortest_path(karate, source => 0)",
            "shortest_path() needs 'target': shortest_path(<graph>, source => <id>, target => \
             <id>, weight => NULL, direction => 'out')",
        ),
        (
            "SELECT * FROM shortest_path(lesmis, source => 'Valjean', target => 'Marius', \
             weight => src)",
            "shortest_path(weight => src): weight expression must be numeric; it produced a \
             column of type Utf8 (a string or other non-numeric column cannot be a weight)",
        ),
        (
            "SELECT * FROM random_walk(karate, start => [0])",
            "random_walk() needs 'steps': random_walk(<graph>, start => [<id>, ...], steps => \
             <integer>, walks_per_node => 1, seed => NULL)",
        ),
        (
            "SELECT * FROM random_walk(karate, start => [0], steps => 2, seed => -1)",
            "random_walk(seed =>) must be a non-negative integer or NULL; got -1",
        ),
        (
            "SELECT * FROM random_walk(karate, start => [0], steps => 2, direction => 'both')",
            "random_walk() has no parameter 'direction'. Its parameters are: start, steps, \
             walks_per_node, seed",
        ),
        (
            "SELECT * FROM algorithms(karate, hop(seeds => [0]))",
            "algorithms(): hop() is a traversal, which returns rows of its own, not one per \
             node; call it on its own: SELECT * FROM hop(<graph>, seeds => [<id>, ...], n => \
             1, direction => 'out')",
        ),
        (
            "SELECT * FROM hops(karate, seeds => [0])",
            "there is no graph function 'hops'; did you mean 'hop'?",
        ),
        // The values must be ids of the graph's type (see
        // `node_id_type_errors_name_the_parameter`), and a path has one
        // source.
        (
            "SELECT * FROM hop(karate, seeds => ['Valjean'])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got \
             'Valjean'",
        ),
        // Each value of a list is cast on its own, not coerced to one type.
        (
            "SELECT * FROM hop(karate, seeds => [0, 'a'])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got 'a'",
        ),
        (
            "SELECT * FROM shortest_path(karate, source => (SELECT id FROM karate.nodes), \
             target => 33)",
            "shortest_path(source =>) must be one node id; its query returned 34 rows",
        ),
    ] {
        assert_eq!(error(&ctx, sql).await, expected, "{sql}");
    }
}

/// Node ids not of the graph's id type, and not castable to it, name the
/// function, the parameter, the id type and the offending value or type: at
/// plan time for a list or a single value, and for a subquery's type; when
/// the node runs for a subquery's values.
#[tokio::test]
async fn node_id_type_errors_name_the_parameter() {
    let ctx = karate_and_lesmis().await;
    let at_plan_time = [
        // Mixed lists: each value is cast on its own.
        (
            "SELECT * FROM hop(karate, seeds => [0, 'a'])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got 'a'",
        ),
        (
            "SELECT * FROM hop(karate, seeds => ['a', 0])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got 'a'",
        ),
        (
            "SELECT * FROM random_walk(karate, start => [1, 2, 'three'], steps => 2)",
            "random_walk(start =>) must be node ids of graph 'karate', which are integers; got \
             'three'",
        ),
        // One value.
        (
            "SELECT * FROM hop(karate, seeds => 'Valjean')",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got \
             'Valjean'",
        ),
        (
            "SELECT * FROM shortest_path(karate, source => 'a', target => 33)",
            "shortest_path(source =>) must be a node id of graph 'karate', which is an \
             integer; got 'a'",
        ),
        (
            "SELECT * FROM shortest_path(karate, source => 0, target => 'b')",
            "shortest_path(target =>) must be a node id of graph 'karate', which is an \
             integer; got 'b'",
        ),
        // A type that does not cast to the id type.
        (
            "SELECT * FROM hop(karate, seeds => [[0, 1]])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got [0, 1], \
             of type List(Int64)",
        ),
        (
            "SELECT * FROM hop(lesmis, seeds => ['Valjean', {'a': 1}])",
            "hop(seeds =>) must be node ids of graph 'lesmis', which are strings; got {'a': 1}, \
             of type Struct(\"a\": Int64)",
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT [0, 1]))",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got a query \
             of type List(Int64)",
        ),
        (
            "SELECT * FROM random_walk(lesmis, start => (SELECT {'a': 1}), steps => 2)",
            "random_walk(start =>) must be node ids of graph 'lesmis', which are strings; got \
             a query of type Struct(\"a\": Int64)",
        ),
        // Floats, booleans and dates would cast, but are refused (#271).
        (
            "SELECT * FROM hop(karate, seeds => [1.5])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got 1.5",
        ),
        (
            "SELECT * FROM hop(karate, seeds => [0, 2.0])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got 2.0",
        ),
        (
            "SELECT * FROM hop(karate, seeds => true)",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got true",
        ),
        (
            "SELECT * FROM shortest_path(karate, source => 0, target => DATE '2020-01-01')",
            "shortest_path(target =>) must be a node id of graph 'karate', which is an \
             integer; got DATE '2020-01-01'",
        ),
        (
            "SELECT * FROM random_walk(karate, start => [CAST(1 AS DOUBLE)], steps => 2)",
            "random_walk(start =>) must be node ids of graph 'karate', which are integers; got \
             CAST(1 AS DOUBLE)",
        ),
        (
            "SELECT * FROM hop(karate, seeds => [random()])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got \
             random()",
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT 1.0))",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got a query \
             of type Float64",
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT id = 0 FROM karate.nodes))",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got a query \
             of type Boolean",
        ),
        (
            "SELECT * FROM hop(karate, seeds => ['1.5'])",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got '1.5'",
        ),
        (
            "SELECT * FROM shortest_path(karate, source => ' 0', target => 33)",
            "shortest_path(source =>) must be a node id of graph 'karate', which is an \
             integer; got ' 0'",
        ),
        // On a graph with string ids too: an integer or a string.
        (
            "SELECT * FROM hop(lesmis, seeds => ['Valjean', 1.5])",
            "hop(seeds =>) must be node ids of graph 'lesmis', which are strings; got 1.5",
        ),
        (
            "SELECT * FROM hop(lesmis, seeds => [false])",
            "hop(seeds =>) must be node ids of graph 'lesmis', which are strings; got false",
        ),
    ];
    for (sql, expected) in at_plan_time {
        let e = ctx
            .sql(sql)
            .await
            .err()
            .unwrap_or_else(|| panic!("{sql} should fail at plan time"));
        assert_eq!(user_message(&e), expected, "{sql}");
    }

    // A subquery's values are known when the node runs.
    for (sql, expected) in [
        (
            "SELECT * FROM hop(karate, seeds => (SELECT 'Valjean'))",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got \
             'Valjean'",
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT * FROM (VALUES ('0'), ('1'), ('x'))))",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got 'x'",
        ),
        (
            "SELECT * FROM shortest_path(karate, source => 0, target => (SELECT 'end'))",
            "shortest_path(target =>) must be a node id of graph 'karate', which is an \
             integer; got 'end'",
        ),
        // A string must parse exactly as an integer (#271).
        (
            "SELECT * FROM hop(karate, seeds => (SELECT '1.5'))",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got '1.5'",
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT * FROM (VALUES ('0'), ('2.0'))))",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got '2.0'",
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT ' 1'))",
            "hop(seeds =>) must be node ids of graph 'karate', which are integers; got ' 1'",
        ),
    ] {
        let frame = ctx.sql(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        let e = frame.collect().await.expect_err(sql);
        assert_eq!(user_message(&e), expected, "{sql}");
    }

    // What still works: an integer of any width, a string that is an
    // integer, an integer for a string graph, NULL, and a mixed list whose
    // values are all ids.
    for (sql, n) in [
        ("SELECT * FROM hop(karate, seeds => ['0'])", 16),
        // A repeated seed gives its rows twice.
        ("SELECT * FROM hop(karate, seeds => [0, '0'])", 32),
        ("SELECT * FROM hop(karate, seeds => [NULL, 0])", 16),
        ("SELECT * FROM hop(karate, seeds => [-1 + 1])", 16),
        ("SELECT * FROM hop(lesmis, seeds => [1])", 0),
        ("SELECT * FROM hop(lesmis, seeds => (SELECT 1))", 0),
        (
            "SELECT * FROM hop(karate, seeds => [CAST(0 AS TINYINT)])",
            16,
        ),
        (
            "SELECT * FROM hop(karate, seeds => (SELECT CAST(0 AS INT UNSIGNED)))",
            16,
        ),
        ("SELECT * FROM hop(karate, seeds => (SELECT '0'))", 16),
        (
            "SELECT * FROM shortest_path(karate, source => '0', target => 33)",
            2,
        ),
    ] {
        assert_eq!(rows(&ctx, sql).await, n, "{sql}");
    }
}
