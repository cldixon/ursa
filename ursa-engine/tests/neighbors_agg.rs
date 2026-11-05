//! Neighbour aggregation in SQL (#262): `neighbors_agg(g, values => (SELECT
//! id, value FROM ...), agg => 'mean', direction => 'both')`, and its
//! `attribute =>` form over the graph's node table. Its values against the
//! Python library are in `parity.rs`.

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

async fn run(ctx: &SessionContext, sql: &str) {
    ctx.sql(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .collect()
        .await
        .unwrap();
}

/// Getting Started's `karate`, with its `members` vertex table, and a hub:
/// `1`, `2` and `3` point at `0`, which points at `1`; `people` gives each a
/// capacity and a region, and has a row for `9`, which is not in the graph.
async fn session() -> SessionContext {
    let ctx = new_session();
    for (view, file) in [
        ("members", "karate_nodes.csv"),
        ("ties", "karate_edges.csv"),
    ] {
        run(
            &ctx,
            &format!(
                "CREATE VIEW {view} AS SELECT * FROM '{}'",
                data(file).display()
            ),
        )
        .await;
    }
    create_graph(
        &ctx,
        def("karate", "ties", Some(("members", "id"))),
        false,
        false,
    )
    .await
    .unwrap();
    run(
        &ctx,
        "CREATE VIEW hub_edges AS SELECT * FROM (VALUES (1, 0), (2, 0), (3, 0), (0, 1)) \
         AS t(src, dst)",
    )
    .await;
    run(
        &ctx,
        "CREATE VIEW people AS SELECT * FROM (VALUES \
         (0, 0.0, 'x'), (1, 10.0, 'us'), (2, 20.0, 'us'), (3, 30.0, 'eu'), (9, 90.0, 'zz')) \
         AS t(id, capacity, region)",
    )
    .await;
    create_graph(&ctx, def("hub", "hub_edges", None), false, false)
        .await
        .unwrap();
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

/// `SELECT id, neighbors_agg FROM neighbors_agg(hub, <args>) ORDER BY id`.
async fn hub(ctx: &SessionContext, args: &str) -> String {
    let sql = format!("SELECT * FROM neighbors_agg(hub, {args}) ORDER BY id");
    table(ctx, &sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

#[tokio::test]
async fn aggregates_over_each_nodes_neighbours() {
    let ctx = session().await;
    let values = "values => (SELECT id, capacity FROM people)";
    // Node 0's in-neighbours are 1, 2 and 3; the others have one or none.
    assert_eq!(
        hub(&ctx, &format!("{values}, agg => 'mean', direction => 'in'")).await,
        "\
+----+---------------+
| id | neighbors_agg |
+----+---------------+
| 0  | 20.0          |
| 1  | 0.0           |
| 2  |               |
| 3  |               |
+----+---------------+"
    );
    // The defaults: mean over out-neighbours.
    assert_eq!(
        hub(&ctx, values).await,
        "\
+----+---------------+
| id | neighbors_agg |
+----+---------------+
| 0  | 10.0          |
| 1  | 0.0           |
| 2  | 0.0           |
| 3  | 0.0           |
+----+---------------+"
    );
    // sum and count are 0 with no neighbours; min and max are NULL.
    for (agg, expected) in [
        ("sum", ["60.0", "0.0", "0.0", "0.0"]),
        ("count", ["3.0", "1.0", "0.0", "0.0"]),
        ("min", ["10.0", "0.0", "", ""]),
        ("max", ["30.0", "0.0", "", ""]),
        ("n_unique", ["3.0", "1.0", "0.0", "0.0"]),
    ] {
        let out = hub(
            &ctx,
            &format!("{values}, agg => '{agg}', direction => 'in'"),
        )
        .await;
        let got: Vec<&str> = out
            .lines()
            .skip(3)
            .filter(|l| l.starts_with("| "))
            .map(|l| l.split('|').nth(2).unwrap().trim())
            .collect();
        assert_eq!(got, expected, "{agg}");
    }
    // 'both' unions in- and out-neighbours: 0's are 1, 2, 3 and 1 again.
    assert_eq!(
        hub(
            &ctx,
            &format!("{values}, agg => 'sum', direction => 'both'")
        )
        .await
        .lines()
        .nth(3)
        .unwrap(),
        "| 0  | 70.0          |"
    );
}

#[tokio::test]
async fn strings_count_and_n_unique() {
    let ctx = session().await;
    let out = hub(
        &ctx,
        "values => (SELECT id, region FROM people), agg => 'n_unique', direction => 'in'",
    )
    .await;
    // {us, us, eu} is two regions.
    assert_eq!(out.lines().nth(3).unwrap(), "| 0  | 2.0           |");
    let out = hub(
        &ctx,
        "values => (SELECT id, region FROM people), agg => 'count', direction => 'in'",
    )
    .await;
    assert_eq!(out.lines().nth(3).unwrap(), "| 0  | 3.0           |");
}

#[tokio::test]
async fn nulls_and_missing_rows_are_no_value() {
    let ctx = session().await;
    // Node 2 has a NULL value, node 3 no row, and 9 is not in the graph: the
    // mean of node 0's in-neighbours is 1's value alone.
    let out = hub(
        &ctx,
        "values => (SELECT * FROM (VALUES (1, 10.0), (2, NULL), (9, 99.0), (NULL, 5.0))), \
         agg => 'mean', direction => 'in'",
    )
    .await;
    assert_eq!(out.lines().nth(3).unwrap(), "| 0  | 10.0          |");
    // Integer, boolean and string ids of the right kind are all values.
    let out = hub(
        &ctx,
        "values => (SELECT CAST(id AS TINYINT), id > 1 FROM people WHERE id < 9), \
         agg => 'sum', direction => 'in'",
    )
    .await;
    assert_eq!(out.lines().nth(3).unwrap(), "| 0  | 2.0           |");
}

#[tokio::test]
async fn attribute_reads_the_node_table() {
    let ctx = session().await;
    let by_values = table(
        &ctx,
        "SELECT * FROM neighbors_agg(karate, values => (SELECT id, club FROM karate.nodes), \
         agg => 'n_unique', direction => 'both') ORDER BY id",
    )
    .await
    .unwrap();
    let by_attribute = table(
        &ctx,
        "SELECT * FROM neighbors_agg(karate, attribute => club, agg => 'n_unique', \
         direction => 'both') ORDER BY id",
    )
    .await
    .unwrap();
    assert_eq!(by_values, by_attribute);
    // Any expression over the node table's columns: the share of each
    // member's friends in the Officer's club (14 of 33's 17).
    let out = table(
        &ctx,
        "SELECT id, neighbors_agg FROM neighbors_agg(karate, \
         attribute => CASE WHEN club = 'Officer' THEN 1 ELSE 0 END, agg => 'mean', \
         direction => 'both') WHERE id IN (0, 33) ORDER BY id",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+----+--------------------+
| id | neighbors_agg      |
+----+--------------------+
| 0  | 0.0625             |
| 33 | 0.8235294117647058 |
+----+--------------------+"
    );
}

#[tokio::test]
async fn values_from_another_graph_function_share_the_index() {
    let ctx = session().await;
    // The mean degree of each member's friends.
    let out = table(
        &ctx,
        "SELECT id, round(neighbors_agg, 3) AS friends_degree \
         FROM neighbors_agg(karate, \
                            values => (SELECT id, degree FROM degree(karate, direction => 'both')), \
                            direction => 'both') \
         ORDER BY friends_degree DESC, id LIMIT 3",
    )
    .await
    .unwrap();
    assert!(out.contains("friends_degree"), "{out}");
    assert_eq!(index_builds(&ctx), 1);
}

#[tokio::test]
async fn schema_and_explain() {
    let ctx = session().await;
    let frame = ctx
        .sql(
            "SELECT * FROM neighbors_agg(hub, values => (SELECT id, capacity FROM people), \
             agg => 'max')",
        )
        .await
        .unwrap();
    let schema = frame.schema().as_arrow().clone();
    let fields: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| format!("{} {} {}", f.name(), f.data_type(), f.is_nullable()))
        .collect();
    assert_eq!(fields, ["id Int64 false", "neighbors_agg Float64 true"]);
    let plan = table(
        &ctx,
        "EXPLAIN SELECT * FROM neighbors_agg(hub, values => (SELECT id, capacity FROM people \
         WHERE capacity > 5))",
    )
    .await
    .unwrap();
    assert!(
        plan.contains("outputs=[neighbors_agg(direction => 'out', agg => 'mean')]"),
        "{plan}"
    );
}

#[tokio::test]
async fn errors_say_what_to_write() {
    let ctx = session().await;
    for (sql, expected) in [
        (
            "SELECT * FROM neighbors_agg(hub)",
            "neighbors_agg() needs the value of each node: values => (SELECT id, value FROM \
             ...), a query of node ids and values, or attribute => <expr>, an expression over \
             the graph's node table. Write neighbors_agg(<graph>, values => (SELECT id, value \
             FROM ...), direction => 'out', agg => 'mean')",
        ),
        (
            "SELECT * FROM neighbors_agg(karate, values => (SELECT id, club FROM members), \
             attribute => club, agg => 'count')",
            "neighbors_agg() takes 'values' or 'attribute', not both: values => (SELECT id, \
             value FROM ...) is a query of node ids and values, attribute => <expr> an \
             expression over the graph's node table",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => 3)",
            "neighbors_agg(values =>) must be a query of two columns, node ids then values, as \
             values => (SELECT id, value FROM ...); got 3",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT id FROM people))",
            "neighbors_agg(values =>) must be a query of two columns, node ids then values; it \
             returns 1 columns",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT capacity, id FROM people))",
            "neighbors_agg(values =>): the first column must be node ids of graph 'hub', which \
             are integers; got a column of type Float64",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT id, region FROM people))",
            "neighbors_agg(agg => 'mean') needs numbers, and the values are strings; only \
             'count' and 'n_unique' take strings",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT id, DATE '2020-01-01' FROM \
             people), agg => 'max')",
            "neighbors_agg(values =>): the values must be numbers, or strings for 'count' and \
             'n_unique'; got a column of type Date32",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT id, capacity FROM nowhere))",
            "neighbors_agg(values =>): table 'datafusion.public.nowhere' not found",
        ),
        (
            "SELECT * FROM neighbors_agg(karate, attribute => age)",
            "neighbors_agg(attribute =>): No field named age. Valid fields are karate.nodes.id, \
             karate.nodes.club.",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT id, capacity FROM people), \
             agg => 'median')",
            "neighbors_agg(agg => 'median') is not supported; use one of 'mean', 'sum', 'min', \
             'max', 'count', 'n_unique'",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT id, capacity FROM people), \
             dtype => 'f32')",
            "neighbors_agg() has no parameter 'dtype' in SQL; cast its column instead, for \
             example CAST(neighbors_agg AS REAL)",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, value => (SELECT id, capacity FROM people))",
            "neighbors_agg() has no parameter 'value'; did you mean 'values'? Its parameters \
             are: values, direction, attribute, agg",
        ),
        (
            "SELECT * FROM algorithms(hub, degree(), neighbors_agg(agg => 'sum'))",
            "algorithms(): neighbors_agg() reads node values, so it runs on its own: SELECT * \
             FROM neighbors_agg(<graph>, values => (SELECT id, value FROM ...), direction => \
             'out', agg => 'mean'); join its result on id",
        ),
        // A node given twice: which value counts would depend on row order.
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT * FROM (VALUES (1, 1.0), \
             (1, 2.0))))",
            "neighbors_agg(values =>) gives node 1 more than one value; give each node one row",
        ),
        (
            "SELECT * FROM neighbors_agg(hub, values => (SELECT * FROM (VALUES ('1', 1.0), \
             ('x', 2.0))))",
            "neighbors_agg(values =>): the first column must be node ids of graph 'hub', which \
             are integers; got 'x'",
        ),
    ] {
        assert_eq!(error(&ctx, sql).await, expected, "{sql}");
    }
}

#[tokio::test]
async fn listed_by_ursa_functions() {
    let ctx = session().await;
    let out = table(
        &ctx,
        "SELECT family, p['name'] AS name, p['type'] AS type, p['default'] AS default \
         FROM (SELECT family, unnest(parameters) AS p FROM ursa_functions() \
               WHERE name = 'neighbors_agg')",
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        "\
+-------------+-----------+------------+---------+
| family      | name      | type       | default |
+-------------+-----------+------------+---------+
| aggregation | values    | query      | NULL    |
| aggregation | direction | string     | 'out'   |
| aggregation | attribute | expression | NULL    |
| aggregation | agg       | string     | 'mean'  |
+-------------+-----------+------------+---------+"
    );
}
