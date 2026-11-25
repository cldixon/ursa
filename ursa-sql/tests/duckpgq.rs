//! DuckPGQ's algorithm calls (#298) and the errors that point to the
//! standard form (#300).
//!
//! DuckPGQ's call is positional, `pagerank(pg, Student, know)`: the graph, a
//! vertex table, an edge table. Its names that differ from Ursa's run Ursa's
//! function with DuckPGQ's column: `weakly_connected_component` is
//! `connected_components(mode => 'weak')` with `componentid`, and
//! `local_clustering_coefficient` is `clustering_coefficient` with
//! `local_clustering_coefficient`. The values are Ursa's (NetworkX's).

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
    // The header names the columns, which differ on purpose; compare rows.
    let rows = |t: &str| {
        t.lines()
            .filter(|l| l.starts_with('|'))
            .skip(1)
            .map(|l| l.split('|').map(str::trim).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
    };
    assert_eq!(rows(&got), rows(&want), "{sql}");
    assert!(got.lines().count() > 4, "no rows: {sql}\n{got}");
}

/// DuckPGQ's canonical graph, and a graph of one edge table.
async fn graphs() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
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
         CREATE VIEW e AS SELECT * FROM (VALUES (1,2),(2,3),(4,5)) t(src, dst);
         CREATE PROPERTY GRAPH k EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst));",
    )
    .await;
    ctx
}

#[tokio::test]
async fn duckpgq_calls_run_unchanged() {
    let ctx = graphs().await;
    // DuckPGQ's own queries, as its documentation writes them.
    for sql in [
        "SELECT id, pagerank FROM pagerank(pg, Student, know)",
        "SELECT id, componentId FROM weakly_connected_component(pg, Student, know)",
        "SELECT id, local_clustering_coefficient FROM local_clustering_coefficient(pg, Student, know)",
    ] {
        let table = ok(&ctx, sql).await;
        assert_eq!(table.lines().count(), 4 + 5, "{sql}\n{table}");
    }
    // The pair by label, and named parameters after it.
    same(
        &ctx,
        "SELECT * FROM weakly_connected_component(pg, Person, Knows) ORDER BY id",
        "SELECT * FROM connected_components(pg, student, know, mode => 'weak') ORDER BY id",
    )
    .await;
    same(
        &ctx,
        "SELECT * FROM local_clustering_coefficient(pg, Student, know) ORDER BY id",
        "SELECT * FROM clustering_coefficient(pg, student, know) ORDER BY id",
    )
    .await;
    // On a graph of one edge table, the pair is implied, as for Ursa's names.
    same(
        &ctx,
        "SELECT * FROM weakly_connected_component(k) ORDER BY id",
        "SELECT * FROM connected_components(k) ORDER BY id",
    )
    .await;
}

#[tokio::test]
async fn duckpgq_names_keep_duckpgqs_columns() {
    let ctx = graphs().await;
    let wcc = ok(
        &ctx,
        "SELECT * FROM weakly_connected_component(pg, Student, know) ORDER BY id",
    )
    .await;
    assert!(wcc.contains("| id | componentid |"), "{wcc}");
    let lcc = ok(
        &ctx,
        "SELECT * FROM local_clustering_coefficient(pg, Student, know) ORDER BY id",
    )
    .await;
    assert!(
        lcc.contains("| id | local_clustering_coefficient |"),
        "{lcc}"
    );
    // Undirected local clustering on DuckPGQ's graph.
    for row in ["| 0  | 1.0", "| 3  | 0.5", "| 4  | 0.0"] {
        assert!(lcc.contains(row), "{row}\n{lcc}");
    }
    // Unaliased, the columns are qualified by DuckPGQ's name.
    ok(
        &ctx,
        "SELECT weakly_connected_component.componentId FROM weakly_connected_component(pg, Student, know)",
    )
    .await;
    // In algorithms(), too, with the same column names.
    let both = ok(
        &ctx,
        "SELECT * FROM algorithms(pg, Student, know, weakly_connected_component(), \
         local_clustering_coefficient(), c => weakly_connected_component()) ORDER BY id",
    )
    .await;
    assert!(
        both.contains("| id | componentid | local_clustering_coefficient | c |"),
        "{both}"
    );
}

#[tokio::test]
async fn duckpgq_names_refuse_what_they_fix() {
    let ctx = graphs().await;
    assert_eq!(
        err(
            &ctx,
            "SELECT * FROM weakly_connected_component(pg, Student, know, mode => 'strong')"
        )
        .await,
        "weakly_connected_component() is DuckPGQ's name for connected_components(<graph>, \
         mode => 'weak'), so it takes no 'mode'; for another mode, call \
         connected_components(mode => …)"
    );
    assert_eq!(
        err(
            &ctx,
            "SELECT * FROM algorithms(pg, Student, know, weakly_connected_component(mode => 'weak'))"
        )
        .await,
        "algorithms(): weakly_connected_component() is DuckPGQ's name for \
         connected_components(<graph>, mode => 'weak'), so it takes no 'mode'; for another \
         mode, call connected_components(mode => …)"
    );
    // Other messages say which function the name runs.
    assert_eq!(
        err(
            &ctx,
            "SELECT * FROM local_clustering_coefficient(pg, Student, know, damping => 1)"
        )
        .await,
        "local_clustering_coefficient() is DuckPGQ's name for clustering_coefficient(<graph>): \
         clustering_coefficient() takes no parameters besides the graph; got 'damping'"
    );
    // The pair's errors are the same as for Ursa's names.
    let message = err(
        &ctx,
        "SELECT * FROM weakly_connected_component(pg, School, know)",
    )
    .await;
    assert!(
        message.contains("connects vertex table 'student', not 'school'"),
        "{message}"
    );
}

#[tokio::test]
async fn the_pair_is_positional() {
    let ctx = graphs().await;
    for (sql, param) in [
        (
            "SELECT * FROM pagerank(pg, vertex => Student, edge => know)",
            "vertex",
        ),
        (
            "SELECT * FROM weakly_connected_component(pg, edge_table => know)",
            "edge_table",
        ),
    ] {
        let message = err(&ctx, sql).await;
        assert!(
            message.contains(&format!(
                "takes the vertex table and the edge table by position, right after the \
                 graph, not as '{param} =>': write"
            )) && message.contains("(pg, <vertex table>, <edge table>)"),
            "{sql}: {message}"
        );
    }
}

#[tokio::test]
async fn duckpgq_path_functions_point_to_graph_table() {
    let ctx = graphs().await;
    for name in [
        "iterativelength",
        "shortestpath",
        "reachability",
        "cheapest_path_length",
    ] {
        for sql in [
            format!("SELECT {name}(0, 5, 0, 3)"),
            format!("SELECT * FROM {name}(pg, 0, 3)"),
        ] {
            let message = err(&ctx, &sql).await;
            assert!(
                message.starts_with(&format!(
                    "{name}() is DuckPGQ's internal path function over its CSR, which Ursa \
                     does not have. Find paths with GRAPH_TABLE"
                )) && message.contains("ANY SHORTEST")
                    && message.contains("shortest_path(g, source => …, target => …)"),
                "{sql}: {message}"
            );
        }
    }
    // A DuckPGQ name called as a scalar is a table function, as Ursa's are.
    assert_eq!(
        err(&ctx, "SELECT weakly_connected_component(pg)").await,
        "weakly_connected_component is a table function, not a scalar one: call it in FROM, \
         as in SELECT * FROM weakly_connected_component(pg)"
    );
}

#[tokio::test]
async fn ursa_functions_lists_duckpgqs_names() {
    let ctx = graphs().await;
    let table = ok(
        &ctx,
        "SELECT name, family, same_as, duckpgq FROM ursa_functions() WHERE duckpgq ORDER BY name",
    )
    .await;
    let expected = "\
+------------------------------+-----------+------------------------+---------+
| name                         | family    | same_as                | duckpgq |
+------------------------------+-----------+------------------------+---------+
| local_clustering_coefficient | algorithm | clustering_coefficient | true    |
| pagerank                     | algorithm |                        | true    |
| weakly_connected_component   | algorithm | connected_components   | true    |
+------------------------------+-----------+------------------------+---------+";
    assert_eq!(table, expected);
    // Its parameters are the function's, less the ones its name fixes; its
    // output is DuckPGQ's column.
    let wcc = ok(
        &ctx,
        "SELECT array_length(parameters) AS n, outputs[1]['name'] AS column \
         FROM ursa_functions() WHERE name = 'weakly_connected_component'",
    )
    .await;
    assert!(wcc.contains("| 0 | componentid |"), "{wcc}");
}

#[tokio::test]
async fn non_standard_forms_name_the_standard_one() {
    let ctx = graphs().await;
    // Cypher's MATCH … RETURN.
    for sql in [
        "MATCH (a)-[e]->(b) RETURN a.id",
        "match (a) return a",
        "OPTIONAL MATCH (a)-[e]->(b) RETURN a.id",
    ] {
        let message = err(&ctx, sql).await;
        assert!(
            message.starts_with(
                "a statement cannot start with MATCH: that is Cypher's or GQL's form. In \
                 SQL/PGQ a pattern goes in GRAPH_TABLE"
            ),
            "{sql}: {message}"
        );
    }
    // A graph read as a table: one pair, then several element tables.
    assert_eq!(
        err(&ctx, "SELECT * FROM k").await,
        "table 'datafusion.public.k' not found; 'k' is a graph, not a table: read its nodes or \
         edges with FROM k.nodes or FROM k.edges, match a pattern with SELECT * FROM \
         GRAPH_TABLE (k MATCH (a)-[e]->(b) COLUMNS (a.id, b.id)), or call a graph function, \
         as in SELECT * FROM pagerank(k)"
    );
    let message = err(&ctx, "FROM pg").await;
    assert!(
        message.contains(
            "'pg' is a graph, not a table: its vertex tables are student, school and its edge \
             tables know, studyat."
        ) && message.contains("pagerank(pg, <vertex table>, <edge table>)"),
        "{message}"
    );
    let message = err(&ctx, "SELECT * FROM pg.nodes").await;
    assert!(
        message.contains("graph 'pg' has several element tables, so it has no pg.nodes"),
        "{message}"
    );
    // A table named like a graph is still read.
    ok(&ctx, "SELECT * FROM k.nodes").await;
}
