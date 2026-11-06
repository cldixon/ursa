//! DuckDB's FROM-first queries (#306): `FROM t` is `SELECT * FROM t`.

use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::prelude::SessionContext;

async fn ok(ctx: &SessionContext, sql: &str) -> String {
    let mut batches = Vec::new();
    for statement in ursa_sql::parse_sql_for(ctx, sql).unwrap() {
        batches = ursa_sql::execute(ctx, statement)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)))
            .collect()
            .await
            .unwrap();
    }
    pretty_format_batches(&batches).unwrap().to_string()
}

/// `sql` and `expected` print the same table.
async fn same(ctx: &SessionContext, sql: &str, expected: &str) {
    let (got, want) = (ok(ctx, sql).await, ok(ctx, expected).await);
    assert_eq!(got, want, "{sql}");
    assert!(got.lines().count() > 4, "no rows: {sql}\n{got}");
}

#[tokio::test]
async fn from_first_queries_select_every_column() {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        "CREATE TABLE t AS SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c')) AS v(x, y)",
    )
    .await;
    for (sql, expected) in [
        ("FROM t", "SELECT * FROM t"),
        (
            "FROM t WHERE x > 1 ORDER BY x DESC LIMIT 1",
            "SELECT * FROM t WHERE x > 1 ORDER BY x DESC LIMIT 1",
        ),
        ("FROM t SELECT y WHERE x = 2", "SELECT y FROM t WHERE x = 2"),
        ("SELECT count(*) FROM (FROM t)", "SELECT 3 AS \"count(*)\""),
        (
            "WITH s AS (FROM t WHERE x < 3) FROM s ORDER BY x",
            "SELECT * FROM t WHERE x < 3 ORDER BY x",
        ),
        (
            "FROM t WHERE x = 1 UNION ALL FROM t WHERE x = 3",
            "SELECT * FROM t WHERE x IN (1, 3)",
        ),
    ] {
        same(
            &ctx,
            &format!("SELECT * FROM ({sql}) ORDER BY 1"),
            &format!("SELECT * FROM ({expected}) ORDER BY 1"),
        )
        .await;
    }
    ok(&ctx, "CREATE VIEW v AS FROM t WHERE x = 2").await;
    same(&ctx, "SELECT * FROM v", "SELECT * FROM t WHERE x = 2").await;
    let explain = ok(&ctx, "EXPLAIN FROM t").await;
    assert!(explain.contains("projection=[x, y]"), "{explain}");
}

#[tokio::test]
async fn from_first_graph_table_as_duckpgq_writes_it() {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        "CREATE TABLE person AS SELECT * FROM (VALUES (0, 'Ann'), (1, 'Bo')) AS v(id, name);
         CREATE TABLE knows AS SELECT * FROM (VALUES (0, 1)) AS v(src, dst);
         CREATE PROPERTY GRAPH g VERTEX TABLES (person)
           EDGE TABLES (knows SOURCE KEY (src) REFERENCES person (id)
                              DESTINATION KEY (dst) REFERENCES person (id));",
    )
    .await;
    same(
        &ctx,
        "FROM GRAPH_TABLE (g MATCH (a)-[k]->(b) COLUMNS (a.name AS a, b.name AS b))",
        "SELECT 'Ann' AS a, 'Bo' AS b",
    )
    .await;
}
