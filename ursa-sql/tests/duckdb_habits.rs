//! DuckDB and DuckPGQ habits (#417): the forms Ursa now accepts, and the
//! refusals that say what to write instead. Each input here failed before,
//! with a parser error or a message that did not name the fix.

use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::prelude::SessionContext;

async fn run(ctx: &SessionContext, sql: &str) -> datafusion::error::Result<String> {
    let mut batches = Vec::new();
    for statement in ursa_sql::parse_sql_for(ctx, sql)? {
        batches = ursa_sql::execute(ctx, statement).await?.collect().await?;
    }
    Ok(pretty_format_batches(&batches).unwrap().to_string())
}

async fn ok(ctx: &SessionContext, sql: &str) -> String {
    run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)))
}

async fn err(ctx: &SessionContext, sql: &str) -> String {
    match run(ctx, sql).await {
        Ok(table) => panic!("{sql} should fail; it gave\n{table}"),
        Err(e) => ursa_engine::user_message(&e),
    }
}

/// `sql` and `expected` print the same table, of at least one row.
async fn same(ctx: &SessionContext, sql: &str, expected: &str) {
    let (got, want) = (ok(ctx, sql).await, ok(ctx, expected).await);
    assert_eq!(got, want, "{sql}");
    assert!(got.lines().count() > 4, "no rows: {sql}\n{got}");
}

fn karate() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../python/ursa/datasets/data/karate_edges.csv"
    )
}

/// DuckPGQ's canonical graph (#410), and a graph of one edge table.
async fn graphs() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        &format!(
            "CREATE TABLE Student AS SELECT * FROM (VALUES (0,'Daniel'),(1,'Tavneet'),(2,'Gabor'),(3,'Peter'),(4,'David')) t(id, name);
             CREATE TABLE know AS SELECT * FROM (VALUES (0,1,10),(0,2,11),(0,3,12),(3,0,13),(1,2,14),(1,3,15),(2,3,16),(4,3,17)) t(src, dst, createDate);
             CREATE TABLE School AS SELECT * FROM (VALUES ('VU',0,0),('UVA',1,0)) t(school_name, school_id, school_kind);
             CREATE TABLE StudyAt AS SELECT * FROM (VALUES (0,0),(1,0),(2,1),(3,1),(4,1)) t(personId, schoolId);
             CREATE PROPERTY GRAPH pg
               VERTEX TABLES (Student PROPERTIES (id, name) LABEL Person, School LABEL School)
               EDGE TABLES (
                 know SOURCE KEY (src) REFERENCES Student (id) DESTINATION KEY (dst) REFERENCES Student (id)
                      PROPERTIES (createDate) LABEL Knows,
                 studyAt SOURCE KEY (personId) REFERENCES Student (id) DESTINATION KEY (schoolId) REFERENCES School (school_id));
             CREATE VIEW e AS FROM '{}';
             CREATE PROPERTY GRAPH k EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst));",
            karate()
        ),
    )
    .await;
    ctx
}

/// Part 1: a FROM-first query under DESCRIBE, EXPLAIN, CREATE VIEW and COPY.
#[tokio::test]
async fn from_first_under_describe_explain_view_and_copy() {
    let ctx = graphs().await;
    same(
        &ctx,
        "DESCRIBE FROM Student",
        "DESCRIBE SELECT * FROM Student",
    )
    .await;
    same(
        &ctx,
        &format!("DESCRIBE FROM '{}'", karate()),
        &format!("DESCRIBE '{}'", karate()),
    )
    .await;
    let explain = ok(&ctx, "EXPLAIN FROM Student").await;
    assert!(explain.contains("projection=[id, name]"), "{explain}");
    ok(&ctx, "CREATE VIEW v AS FROM Student WHERE id < 2").await;
    same(
        &ctx,
        "SELECT * FROM v",
        "SELECT * FROM Student WHERE id < 2",
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("s.csv").display().to_string();
    ok(&ctx, &format!("COPY (FROM Student) TO '{out}'")).await;
    same(
        &ctx,
        &format!("SELECT * FROM '{out}' ORDER BY id"),
        "SELECT * FROM Student ORDER BY id",
    )
    .await;
}

/// `COPY … TO 'f' (FORMAT x, HEADER, DELIMITER …, COMPRESSION …)`, and
/// `WITH (…)`, as DuckDB and PostgreSQL write it.
#[tokio::test]
async fn copy_takes_duckdbs_options() {
    let ctx = graphs().await;
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).display().to_string();

    // FORMAT names the format whatever the extension; COMPRESSION zstd gets
    // a level for Parquet.
    let p = path("out.data");
    ok(
        &ctx,
        &format!("COPY (FROM Student) TO '{p}' (FORMAT parquet, COMPRESSION zstd)"),
    )
    .await;
    same(
        &ctx,
        &format!("SELECT * FROM read_parquet('{p}') ORDER BY id"),
        "SELECT * FROM Student ORDER BY id",
    )
    .await;

    // HEADER false and DELIMITER '|'.
    let c = path("out.csv");
    ok(
        &ctx,
        &format!("COPY Student TO '{c}' (HEADER false, DELIMITER '|')"),
    )
    .await;
    let text = std::fs::read_to_string(&c).unwrap();
    assert!(text.lines().all(|l| l.contains('|')), "{text}");
    assert!(!text.contains("name"), "no header: {text}");

    // WITH (…), a bare HEADER, SEP and gzip.
    let g = path("out.csv.gz");
    ok(
        &ctx,
        &format!("COPY Student TO '{g}' WITH (FORMAT csv, HEADER, SEP ';', COMPRESSION gzip)"),
    )
    .await;
    same(
        &ctx,
        &format!("SELECT * FROM read_csv('{g}', delim = ';') ORDER BY id"),
        "SELECT * FROM Student ORDER BY id",
    )
    .await;

    // DataFusion's own form is unchanged.
    let d = path("df.parquet");
    ok(
        &ctx,
        &format!(
            "COPY Student TO '{d}' STORED AS PARQUET OPTIONS ('format.compression' 'zstd(1)')"
        ),
    )
    .await;

    // The rest are refused by name.
    let m = err(
        &ctx,
        &format!(
            "COPY Student TO '{}' (FORMAT parquet, ROW_GROUP_SIZE 1000)",
            path("x.parquet")
        ),
    )
    .await;
    assert!(
        m.starts_with(
            "the COPY option ROW_GROUP_SIZE is not supported; Ursa maps DuckDB's FORMAT, HEADER"
        ),
        "{m}"
    );
    // An option whose value is a list is refused by name too.
    let m = err(
        &ctx,
        &format!(
            "COPY Student TO '{}' (FORMAT csv, PARTITION_BY (id, name))",
            path("x")
        ),
    )
    .await;
    assert!(
        m.starts_with("the COPY option PARTITION_BY is not supported"),
        "{m}"
    );
    let m = err(
        &ctx,
        &format!("COPY Student TO '{}' (FORMAT)", path("x.parquet")),
    )
    .await;
    assert_eq!(m, "the COPY option FORMAT needs a value");
    let m = err(
        &ctx,
        &format!("COPY Student TO '{}' (HEADER maybe)", path("x.csv")),
    )
    .await;
    assert!(m.contains("HEADER is true or false"), "{m}");
    let m = err(
        &ctx,
        &format!(
            "COPY Student TO '{}' (FORMAT csv) STORED AS CSV",
            path("x.csv")
        ),
    )
    .await;
    assert!(
        m.contains("Expected: end of statement or ;, found: STORED at Line: 1"),
        "{m}"
    );
}

#[tokio::test]
async fn set_threads_and_other_duckdb_settings() {
    let ctx = ursa_engine::new_session();
    ok(&ctx, "SET threads = 3").await;
    let shown = ok(&ctx, "SHOW datafusion.execution.target_partitions").await;
    assert!(shown.contains("| 3 "), "{shown}");
    ok(&ctx, "SET threads TO 2").await;
    let shown = ok(&ctx, "SHOW datafusion.execution.target_partitions").await;
    assert!(shown.contains("| 2 "), "{shown}");

    let m = err(&ctx, "SET memory_limit = '1GB'").await;
    assert!(
        m.starts_with(
            "memory_limit is not a setting: Ursa's settings start with ursa. or datafusion., and \
             SHOW ALL lists them; DuckDB's memory_limit is datafusion.runtime.memory_limit"
        ),
        "{m}"
    );
    let m = err(&ctx, "SET enable_progress_bar = true").await;
    assert_eq!(
        m,
        "enable_progress_bar is not a setting: Ursa's settings start with ursa. or datafusion., \
         and SHOW ALL lists them"
    );
}

/// `FROM karate_edges.csv`, unquoted, is schema `karate_edges`, table `csv`.
#[tokio::test]
async fn an_unquoted_file_name_says_to_quote_it() {
    let ctx = ursa_engine::new_session();
    for (sql, path) in [
        ("FROM karate_edges.csv", "karate_edges.csv"),
        (
            "SELECT count(*) FROM data.edges.parquet",
            "data.edges.parquet",
        ),
        ("COPY karate_edges.csv TO 'x.parquet'", "karate_edges.csv"),
    ] {
        let m = err(&ctx, sql).await;
        assert!(
            m.ends_with(&format!(
                " not found; to read the file {path}, quote its path: FROM '{path}'"
            )),
            "{sql}: {m}"
        );
    }
    // A missing two-part table that is not a file name keeps DataFusion's
    // message.
    assert_eq!(
        err(&ctx, "FROM nowhere.t").await,
        "table 'datafusion.nowhere.t' not found"
    );
}

/// `SELECT pagerank(g)`, and every other graph function called as a scalar.
#[tokio::test]
async fn a_graph_function_called_as_a_scalar_is_a_table_function() {
    let ctx = graphs().await;
    assert_eq!(
        err(&ctx, "SELECT pagerank(k)").await,
        "pagerank is a table function, not a scalar one: call it in FROM, as in SELECT * FROM \
         pagerank(k)"
    );
    let mut names: Vec<String> = ursa_engine::graph::functions::names()
        .map(str::to_string)
        .collect();
    names.push("ursa_functions".into());
    for name in names {
        let m = err(&ctx, &format!("SELECT 1 AS one, {name}(k) AS x")).await;
        assert_eq!(
            m,
            format!(
                "{name} is a table function, not a scalar one: call it in FROM, as in SELECT * \
                 FROM {name}(k)"
            ),
        );
    }
    // Other unknown functions keep DataFusion's message.
    let m = err(&ctx, "SELECT nosuchfunction(1)").await;
    assert!(m.starts_with("Invalid function 'nosuchfunction'"), "{m}");
}

/// `SELECT id, pagerank FROM k.nodes` (#300).
#[tokio::test]
async fn a_graph_function_named_as_a_column_says_to_join_it() {
    let ctx = graphs().await;
    let m = err(&ctx, "SELECT id, pagerank FROM k.nodes").await;
    assert!(
        m.ends_with(
            "\npagerank is a graph function, not a column: join its rows on id, as in SELECT * \
             FROM k.nodes JOIN pagerank(k) USING (id)"
        ),
        "{m}"
    );
    // On a vertex table of a graph, the call names the pair, as DuckPGQ's
    // does (#300).
    let m = err(&ctx, "SELECT degree FROM Student").await;
    assert!(
        m.ends_with(
            "join its rows on id, as in SELECT * FROM student JOIN degree(pg, student, know) \
             USING (id)"
        ),
        "{m}"
    );
    // On any other table, the general form.
    let m = err(&ctx, "SELECT degree FROM School s").await;
    assert!(
        m.ends_with("join its rows on id, as in SELECT * FROM t JOIN degree(g) USING (id)"),
        "{m}"
    );
    // Not for other missing columns.
    let m = err(&ctx, "SELECT nosuchcolumn FROM k.nodes").await;
    assert!(!m.contains("graph function"), "{m}");
}

#[tokio::test]
async fn drop_show_summarize_and_alter_graph() {
    let ctx = graphs().await;
    same(&ctx, "SHOW PROPERTY GRAPHS", "SHOW GRAPHS").await;

    // SUMMARIZE PROPERTY GRAPH: describe(g) for one edge table.
    same(
        &ctx,
        "SUMMARIZE PROPERTY GRAPH k",
        "SELECT * FROM describe(k)",
    )
    .await;
    same(&ctx, "summarize graph k", "SELECT * FROM describe(k)").await;
    assert_eq!(
        err(&ctx, "SUMMARIZE PROPERTY GRAPH pg").await,
        "graph 'pg' has 2 edge tables (know, studyat), and SUMMARIZE PROPERTY GRAPH summarizes \
         a graph of one; name the pair to describe: SELECT * FROM describe(pg, <vertex table>, \
         <edge table>)"
    );
    // SUMMARIZE of a table is still refused.
    let m = err(&ctx, "SUMMARIZE Student").await;
    assert!(m.starts_with("SUMMARIZE is not supported;"), "{m}");

    let m = err(&ctx, "ALTER PROPERTY GRAPH pg ADD VERTEX TABLES (x)").await;
    assert_eq!(
        m,
        "ALTER PROPERTY GRAPH is not supported; use CREATE OR REPLACE PROPERTY GRAPH with the \
         whole new definition"
    );
    assert_eq!(err(&ctx, "alter graph pg drop edge tables (know)").await, m);

    // DROP GRAPH, as SHOW GRAPHS and DESCRIBE GRAPH, leaves out PROPERTY.
    ok(&ctx, "DROP GRAPH k").await;
    ok(&ctx, "DROP GRAPH IF EXISTS k").await;
    let shown = ok(&ctx, "SHOW GRAPHS").await;
    assert!(!shown.contains("| k "), "{shown}");
    assert!(err(&ctx, "DROP GRAPH k").await.contains("k"));
}

/// `SHOW 'file'` is `DESCRIBE 'file'`, as in DuckDB.
#[tokio::test]
async fn show_a_file_describes_it() {
    let ctx = ursa_engine::new_session();
    same(
        &ctx,
        &format!("SHOW '{}'", karate()),
        &format!("DESCRIBE '{}'", karate()),
    )
    .await;
}

#[tokio::test]
async fn order_by_all() {
    let ctx = graphs().await;
    same(
        &ctx,
        "SELECT src, count(*) AS n FROM e GROUP BY ALL ORDER BY ALL DESC LIMIT 5",
        "SELECT src, count(*) AS n FROM e GROUP BY src ORDER BY 1 DESC, 2 DESC LIMIT 5",
    )
    .await;
    same(
        &ctx,
        "SELECT name, id FROM Student ORDER BY ALL",
        "SELECT name, id FROM Student ORDER BY name, id",
    )
    .await;
    // A column named `all` is still ordered by.
    same(
        &ctx,
        "SELECT x FROM (VALUES (2, 1), (1, 2)) t(x, \"all\") ORDER BY all",
        "SELECT x FROM (VALUES (2), (1)) t(x)",
    )
    .await;
    // After SELECT *, the hint.
    let m = err(&ctx, "SELECT * FROM Student ORDER BY ALL").await;
    assert!(
        m.ends_with(
            "ORDER BY ALL is supported only where the SELECT list names its columns; after \
             SELECT *, name the columns or their positions: ORDER BY 1, 2, …"
        ),
        "{m}"
    );
}

#[tokio::test]
async fn create_temp_table_and_view() {
    let ctx = ursa_engine::new_session();
    ok(&ctx, "CREATE TEMP TABLE t AS SELECT 1 AS x").await;
    ok(&ctx, "CREATE TEMPORARY VIEW v AS SELECT x + 1 AS y FROM t").await;
    same(&ctx, "SELECT * FROM v", "SELECT 2 AS y").await;
}

/// Part 4: DuckDB's names for DataFusion's functions.
#[tokio::test]
async fn duckdb_function_names() {
    let ctx = graphs().await;
    same(
        &ctx,
        "SELECT list_value(1, 2, 3) AS l",
        "SELECT make_array(1, 2, 3) AS l",
    )
    .await;
    same(
        &ctx,
        "SELECT strftime(TIMESTAMP '2024-03-05 10:11:12', '%Y-%m-%d %H') AS a, \
                strftime('%d/%m', DATE '2024-03-05') AS b",
        "SELECT '2024-03-05 10' AS a, '05/03' AS b",
    )
    .await;
    same(
        &ctx,
        "SELECT epoch_ms(1700000000000) AS t, epoch_ms(TIMESTAMP '2023-11-14 22:13:20') AS ms, \
                epoch_ms(DATE '1970-01-02') AS d",
        "SELECT arrow_cast(TIMESTAMP '2023-11-14 22:13:20', 'Timestamp(Millisecond, None)') AS t, \
                1700000000000 AS ms, 86400000 AS d",
    )
    .await;
    same(
        &ctx,
        "SELECT arg_max(name, id) AS oldest, arg_min(name, id) AS first FROM Student",
        "SELECT 'David' AS oldest, 'Daniel' AS first",
    )
    .await;
    // Grouped, filtered, and rows with a NULL skipped.
    same(
        &ctx,
        "SELECT k, arg_max(x, y) AS a, arg_min(x, y) FILTER (WHERE y > 1) AS b \
         FROM (VALUES (1, 'a', 1), (1, 'b', 3), (1, NULL, 9), (1, 'c', NULL), (2, 'd', 5)) t(k, x, y) \
         GROUP BY k ORDER BY k",
        "SELECT * FROM (VALUES (1, 'b', 'b'), (2, 'd', 'd')) t(k, a, b)",
    )
    .await;
    let m = err(&ctx, "SELECT strftime(1, '%Y')").await;
    assert!(
        m.contains("strftime takes a date or timestamp and a format string"),
        "{m}"
    );
    let m = err(&ctx, "SELECT arg_max(src, dst) OVER () FROM e").await;
    assert!(
        m.contains("arg_max(x, y) is an aggregate, not a window function"),
        "{m}"
    );
}
