//! Queries that used to return a wrong answer without an error, and are now
//! refused (the SQL surface review, #408): text left over after a `WHERE`
//! condition or a `COLUMNS` item (#409), a node label that contradicts its
//! edge's `REFERENCES` (#410), and a vertex table whose key repeats or is
//! NULL (#411). Each query is the repro of its issue.

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::prelude::SessionContext;

/// Run every statement in `sql`; the batches of the last one.
async fn run(ctx: &SessionContext, sql: &str) -> datafusion::error::Result<Vec<RecordBatch>> {
    let mut batches = Vec::new();
    for statement in ursa_sql::parse_sql_for(ctx, sql)? {
        batches = ursa_sql::execute(ctx, statement).await?.collect().await?;
    }
    Ok(batches)
}

async fn ok(ctx: &SessionContext, sql: &str) -> String {
    let batches = run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)));
    pretty_format_batches(&batches).unwrap().to_string()
}

/// The user-facing error `sql` fails with.
async fn err(ctx: &SessionContext, sql: &str) -> String {
    match run(ctx, sql).await {
        Ok(batches) => panic!(
            "{sql} should fail; got\n{}",
            pretty_format_batches(&batches).unwrap()
        ),
        Err(e) => ursa_engine::user_message(&e),
    }
}

#[track_caller]
fn assert_contains(message: &str, part: &str) {
    assert!(message.contains(part), "expected '{part}' in:\n{message}");
}

/// DuckPGQ's test graph: students who know each other and study at schools.
const SCHOOL: &str = "
CREATE TABLE Student AS SELECT * FROM (VALUES
  (0,'Daniel'),(1,'Tavneet'),(2,'Gabor'),(3,'Peter'),(4,'David')) t(id, name);
CREATE TABLE know AS SELECT * FROM (VALUES
  (0,1,10),(0,2,11),(0,3,12),(3,0,13),(1,2,14),(1,3,15),(2,3,16),(4,3,17)) t(src, dst, createDate);
CREATE TABLE School AS SELECT * FROM (VALUES ('VU',0,0),('UVA',1,0))
  t(school_name, school_id, school_kind);
CREATE TABLE StudyAt AS SELECT * FROM (VALUES (0,0),(1,0),(2,1),(3,1),(4,1)) t(personId, schoolId);
CREATE PROPERTY GRAPH pg
  VERTEX TABLES (Student PROPERTIES (id, name) LABEL Person, School LABEL School)
  EDGE TABLES (
    know    SOURCE KEY (src) REFERENCES Student (id) DESTINATION KEY (dst) REFERENCES Student (id)
            PROPERTIES (createDate) LABEL Knows,
    studyAt SOURCE KEY (personId) REFERENCES Student (id)
            DESTINATION KEY (schoolId) REFERENCES School (school_id));";

async fn school() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(&ctx, SCHOOL).await;
    ctx
}

/// `GRAPH_TABLE (pg MATCH (a:Person)-[k:knows]->(b:Person) <rest>)`.
fn knows(rest: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (pg MATCH (a:Person)-[k:knows]->(b:Person) {rest})")
}

// ---- #409 ----------------------------------------------------------------

#[tokio::test]
async fn text_after_a_where_condition_is_an_error() {
    let ctx = school().await;
    // A missing AND: the second condition used to be dropped (3 rows).
    let message = err(
        &ctx,
        &knows("WHERE a.id = 0 b.id = 3 COLUMNS (a.id AS a, b.id AS b)"),
    )
    .await;
    assert_eq!(
        message,
        "in GRAPH_TABLE: unexpected 'b' (at character 58 of the pattern) after the WHERE \
         condition 'a.id = 0'; join conditions with AND or OR"
    );
    // Any words were accepted.
    let message = err(
        &ctx,
        &knows("WHERE a.id = 0 garbage words here COLUMNS (a.id AS a, b.id AS b)"),
    )
    .await;
    assert_contains(
        &message,
        "unexpected 'garbage' (at character 58 of the pattern)",
    );
    assert_contains(&message, "after the WHERE condition 'a.id = 0'");

    // The condition as it was meant runs.
    assert_eq!(
        ok(
            &ctx,
            &knows("WHERE a.id = 0 AND b.id = 3 COLUMNS (a.id AS a, b.id AS b)")
        )
        .await,
        ok(&ctx, "SELECT 0 AS a, 3 AS b").await
    );
    // So do conditions that end in a keyword, a bracket or a string.
    for condition in [
        "a.id = 0 AND b.name IS NOT NULL",
        "(a.id = 0)",
        "a.id IN (0, 1) AND b.name <> 'COLUMNS (x'",
        "a.id BETWEEN 0 AND 1 OR b.id = CASE WHEN a.id = 0 THEN 3 ELSE 2 END",
    ] {
        ok(
            &ctx,
            &knows(&format!("WHERE {condition} COLUMNS (a.id AS a, b.id AS b)")),
        )
        .await;
    }
}

#[tokio::test]
async fn text_after_a_node_or_edge_condition_is_an_error() {
    let ctx = school().await;
    let message = err(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (pg
           MATCH (a:Person WHERE a.id = 0 nonsense)-[k:knows]->(b:Person)
           COLUMNS (a.id AS a, b.id AS b))",
    )
    .await;
    assert_contains(&message, "unexpected 'nonsense' (at character ");
    assert_contains(&message, "after the WHERE condition 'a.id = 0'");

    let message = err(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (pg
           MATCH (a:Person)-[k:knows WHERE k.createDate > 12 k.createDate < 15]->(b:Person)
           COLUMNS (a.id AS a, b.id AS b))",
    )
    .await;
    assert_contains(&message, "unexpected 'k' (at character ");
    assert_contains(&message, "after the WHERE condition 'k.createDate > 12'");

    // As they were meant.
    assert_eq!(
        ok(
            &ctx,
            "SELECT count(*) AS n FROM GRAPH_TABLE (pg
               MATCH (a:Person WHERE a.id = 0)
                     -[k:knows WHERE k.createDate > 10 AND k.createDate < 15]->(b:Person)
               COLUMNS (a.id AS a, b.id AS b))"
        )
        .await,
        ok(&ctx, "SELECT 2 AS n").await
    );
}

#[tokio::test]
async fn text_after_a_columns_item_is_an_error() {
    let ctx = school().await;
    // This ran, and returned one column: `b` was dropped too.
    let message = err(&ctx, &knows("COLUMNS (a.id AS a nonsense, b.id AS b)")).await;
    assert_eq!(
        message,
        "in GRAPH_TABLE: unexpected 'nonsense' (at character 62 of the pattern) after the \
         COLUMNS item 'a.id AS a'; separate items with commas"
    );
    // A missing comma between two items.
    let message = err(&ctx, &knows("COLUMNS (a.id AS a b.id AS b)")).await;
    assert_contains(&message, "unexpected 'b' (at character 62 of the pattern)");

    // Every form of item still runs: an alias with and without AS, an
    // expression, a variable's columns.
    ok(
        &ctx,
        &knows("COLUMNS (a.id AS a, b.id b, a.id + b.id, upper(a.name) AS n, k.*)"),
    )
    .await;
}

#[tokio::test]
async fn keep_and_one_row_per_are_refused_not_ignored() {
    let ctx = school().await;
    let columns = "COLUMNS (a.id AS a, b.id AS b)";
    for clause in [
        "WHERE a.id = 0 KEEP TRAIL",
        "KEEP TRAIL",
        "KEEP ANY SHORTEST WHERE a.id = 0",
    ] {
        let message = err(&ctx, &knows(&format!("{clause} {columns}"))).await;
        assert_contains(&message, "in GRAPH_TABLE: KEEP is not supported yet");
    }
    for clause in [
        "WHERE a.id = 0 ONE ROW PER MATCH",
        "ONE ROW PER MATCH",
        "ONE ROW PER VERTEX (v)",
    ] {
        let message = err(&ctx, &knows(&format!("{clause} {columns}"))).await;
        assert_contains(&message, "in GRAPH_TABLE: ONE ROW PER is not supported yet");
    }
}

// ---- #410 ----------------------------------------------------------------

#[tokio::test]
async fn a_node_label_that_contradicts_the_edges_references_is_refused() {
    let ctx = school().await;
    // No `knows` edge ends at a school: this used to join know.dst to
    // School.school_id and return (0, UVA) and (3, VU).
    let message = err(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (pg
           MATCH (a:Person)-[k:knows]->(b:School)
           COLUMNS (a.id, b.school_name)) ORDER BY 1",
    )
    .await;
    assert_eq!(
        message,
        "in GRAPH_TABLE: edge table 'know' of graph 'pg' goes from vertex table 'student' to \
         'student', but edge 'k' is written (a:person)->(b:school), from 'student' to 'school'; \
         no edge of 'know' connects them, so the pattern can match nothing"
    );
    // The other end: five rows before.
    let message = err(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (pg
           MATCH (a:School)-[k:knows]->(b:Person)
           COLUMNS (a.school_name, b.id))",
    )
    .await;
    assert_contains(
        &message,
        "from 'school' to 'student'; no edge of 'know' connects them",
    );

    // Written backwards, without a direction, and with an unnamed edge the
    // nodes' labels cannot both fit.
    for (pattern, part) in [
        (
            "(a:School)<-[k:knows]-(b:Person)",
            "written (a:school)<-(b:person), from 'student' to 'school'",
        ),
        (
            "(a:Person)-[k:knows]-(b:School)",
            "written (a:person)-(b:school), between 'student' and 'school'; no edge of 'know' \
             connects them in either direction",
        ),
        (
            "(a:School)-[:studyAt]->(b:Person)",
            "an unnamed edge is written (a:school)->(b:person), from 'school' to 'student'",
        ),
        (
            "(a:Person)-[k:knows]->(b:Person)-[j:knows]->(:School)",
            "edge 'j' is written (b:person)->(:school)",
        ),
    ] {
        let message = err(
            &ctx,
            &format!("SELECT * FROM GRAPH_TABLE (pg MATCH {pattern} COLUMNS (a.*))"),
        )
        .await;
        assert_contains(&message, part);
    }

    // Labels that agree with the edge still match, either way round.
    for pattern in [
        "(a:Person)-[s:studyAt]->(b:School)",
        "(b:School)<-[s:studyAt]-(a:Person)",
        "(a:Person)-[s:studyAt]->(b)",
        "(a)-[s:studyAt]->(b:School)",
    ] {
        assert_eq!(
            ok(
                &ctx,
                &format!(
                    "SELECT count(*) AS n FROM GRAPH_TABLE (pg MATCH {pattern}
                       WHERE b.school_name = 'UVA' COLUMNS (a.id))"
                )
            )
            .await,
            ok(&ctx, "SELECT 3 AS n").await,
            "{pattern}"
        );
    }
    assert_eq!(
        ok(
            &ctx,
            "SELECT count(*) AS n FROM GRAPH_TABLE (pg
               MATCH (a:Person)-[k:knows]->(b:Person)-[s:studyAt]->(c:School)
               COLUMNS (a.id))"
        )
        .await,
        ok(&ctx, "SELECT 8 AS n").await
    );
}

// ---- #411 ----------------------------------------------------------------

/// The issue's graph: key 1 twice, a NULL key, and an edge to a node (3)
/// that is no key.
const DIRTY: &str = "
CREATE TABLE v AS SELECT * FROM (VALUES (1,'x'), (1,'y'), (NULL,'z'), (2,'w')) t(id, name);
CREATE TABLE e AS SELECT * FROM (VALUES (1,2), (2,3)) t(src, dst);
CREATE PROPERTY GRAPH gs
  VERTEX TABLES (v KEY (id))
  EDGE TABLES (e SOURCE KEY (src) REFERENCES v (id) DESTINATION KEY (dst) REFERENCES v (id));";

const DIRTY_KEYS: &str = "graph 'gs': vertex table 'v' has 1 row with a duplicate key (id = 1) \
                          and 1 row with a NULL key; a vertex table's KEY must identify one row. \
                          Deduplicate it in a view and define the graph over the view, for \
                          example: CREATE VIEW v_nodes AS SELECT DISTINCT ON (id) * FROM v WHERE \
                          id IS NOT NULL ORDER BY id";

#[tokio::test]
async fn a_vertex_table_whose_key_repeats_or_is_null_is_refused_everywhere() {
    let ctx = ursa_engine::new_session();
    // The definition is accepted: the tables are read when a query runs.
    ok(&ctx, DIRTY).await;
    // The algorithms saw nodes 1 and 2; `gs.nodes` and GRAPH_TABLE four rows,
    // so one node was two matches and one edge two paths. Only neighbors_agg
    // noticed, naming `values =>`, which the query did not use.
    for sql in [
        "SELECT * FROM degree(gs) ORDER BY id",
        "SELECT * FROM describe(gs)",
        "SELECT * FROM pagerank(gs)",
        "SELECT * FROM hop(gs, seeds => [1], n => 1)",
        "SELECT * FROM gs.nodes",
        "SELECT count(*) FROM gs.nodes",
        "SELECT name FROM gs.nodes WHERE id = 2",
        "SELECT count(*) FROM GRAPH_TABLE (gs MATCH (a) COLUMNS (a.id))",
        "SELECT * FROM GRAPH_TABLE (gs MATCH (a)-[]->(b) COLUMNS (a.id, a.name))",
        "SELECT * FROM GRAPH_TABLE (gs MATCH (a WHERE a.id = 2)-[]->(b) COLUMNS (b.name))",
        "SELECT * FROM GRAPH_TABLE (gs MATCH p = ANY SHORTEST (a)-[]->*(b) COLUMNS (a.id, b.id))",
        "SELECT * FROM gs.nodes JOIN degree(gs) USING (id)",
        "SELECT * FROM neighbors_agg(gs, attribute => name, agg => 'count', direction => 'in')",
    ] {
        assert_eq!(err(&ctx, sql).await, DIRTY_KEYS, "{sql}");
    }
    // The edge table is still a table, and `gs.edges` is the edge table.
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n FROM gs.edges").await,
        ok(&ctx, "SELECT 2 AS n").await
    );

    // The view the message suggests: one row per key, and every surface
    // agrees on the two nodes and the one edge between them.
    ok(
        &ctx,
        "CREATE VIEW v_nodes AS SELECT DISTINCT ON (id) * FROM v WHERE id IS NOT NULL ORDER BY id;
         CREATE PROPERTY GRAPH clean
           VERTEX TABLES (v_nodes KEY (id))
           EDGE TABLES (e SOURCE KEY (src) REFERENCES v_nodes (id)
                          DESTINATION KEY (dst) REFERENCES v_nodes (id))",
    )
    .await;
    let two = ok(&ctx, "SELECT 2 AS n").await;
    for sql in [
        "SELECT count(*) AS n FROM degree(clean)",
        "SELECT n_nodes AS n FROM describe(clean)",
        "SELECT count(*) AS n FROM clean.nodes",
        "SELECT count(*) AS n FROM GRAPH_TABLE (clean MATCH (a) COLUMNS (a.id))",
        "SELECT count(*) AS n FROM clean.nodes JOIN degree(clean) USING (id)",
    ] {
        assert_eq!(ok(&ctx, sql).await, two, "{sql}");
    }
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (clean MATCH (a)-[]->(b) COLUMNS (a.id AS a, b.id AS b))"
        )
        .await,
        ok(&ctx, "SELECT 1 AS a, 2 AS b").await
    );
}

#[tokio::test]
async fn the_refusal_counts_the_rows_and_names_the_smallest_duplicate() {
    let ctx = ursa_engine::new_session();
    // String keys; 'b' three times and 'a' twice: three rows repeat a key.
    ok(
        &ctx,
        "CREATE TABLE people AS SELECT * FROM (VALUES
           ('b', 1), ('a', 2), ('b', 3), ('c', 4), ('a', 5), ('b', 6)) t(name, n);
         CREATE TABLE likes AS SELECT * FROM (VALUES ('a', 'b'), ('b', 'c')) t(who, whom);
         CREATE PROPERTY GRAPH likers
           VERTEX TABLES (people KEY (name))
           EDGE TABLES (likes SOURCE KEY (who) REFERENCES people (name)
                              DESTINATION KEY (whom) REFERENCES people (name))",
    )
    .await;
    let expected = "graph 'likers': vertex table 'people' has 3 rows with a duplicate key \
                    (name = 'a'); a vertex table's KEY must identify one row.";
    for sql in [
        "SELECT * FROM degree(likers)",
        "SELECT * FROM likers.nodes",
        "SELECT * FROM GRAPH_TABLE (likers MATCH (a)-[]->(b) COLUMNS (a.n))",
    ] {
        assert_contains(&err(&ctx, sql).await, expected);
    }

    // NULL keys alone.
    ok(
        &ctx,
        "CREATE TABLE some AS SELECT * FROM (VALUES (1), (NULL), (2), (NULL)) t(id);
         CREATE TABLE pairs AS SELECT * FROM (VALUES (1, 2)) t(src, dst);
         CREATE PROPERTY GRAPH holes
           VERTEX TABLES (some KEY (id))
           EDGE TABLES (pairs SOURCE KEY (src) REFERENCES some (id)
                              DESTINATION KEY (dst) REFERENCES some (id))",
    )
    .await;
    let expected = "graph 'holes': vertex table 'some' has 2 rows with a NULL key; a vertex \
                    table's KEY must identify one row.";
    for sql in [
        "SELECT * FROM degree(holes)",
        "SELECT * FROM holes.nodes",
        "SELECT * FROM GRAPH_TABLE (holes MATCH (a) COLUMNS (a.id))",
    ] {
        assert_contains(&err(&ctx, sql).await, expected);
    }
}

#[tokio::test]
async fn a_graph_of_several_vertex_tables_refuses_the_one_with_a_dirty_key() {
    let ctx = school().await;
    // Two schools with one id. Students are still one row each.
    ok(&ctx, "INSERT INTO School VALUES ('TUD', 1, 0)").await;
    let expected = "graph 'pg': vertex table 'school' has 1 row with a duplicate key \
                    (school_id = 1)";
    for sql in [
        "SELECT * FROM GRAPH_TABLE (pg MATCH (s:School) COLUMNS (s.school_name))",
        "SELECT * FROM GRAPH_TABLE (pg MATCH (a:Person)-[:studyAt]->(s:School)
           COLUMNS (a.name, s.school_name))",
    ] {
        assert_contains(&err(&ctx, sql).await, expected);
    }
    // What reads only the students is not refused.
    assert_eq!(
        ok(
            &ctx,
            "SELECT count(*) AS n FROM GRAPH_TABLE (pg
               MATCH (a:Person)-[k:knows]->(b:Person) COLUMNS (a.id))"
        )
        .await,
        ok(&ctx, "SELECT 8 AS n").await
    );
    ok(&ctx, "SELECT * FROM pagerank(pg, student, know)").await;
}

#[tokio::test]
async fn a_vertex_file_is_counted_again_when_it_changes() {
    let dir = tempfile::tempdir().unwrap();
    let nodes = dir.path().join("nodes.csv");
    let edges = dir.path().join("edges.csv");
    std::fs::write(&nodes, "id,name\n1,x\n2,w\n3,q\n").unwrap();
    std::fs::write(&edges, "src,dst\n1,2\n2,3\n").unwrap();
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        &format!(
            "CREATE PROPERTY GRAPH files
               VERTEX TABLES ('{}' AS nodes KEY (id))
               EDGE TABLES ('{}' AS edges SOURCE KEY (src) REFERENCES nodes (id)
                                          DESTINATION KEY (dst) REFERENCES nodes (id))",
            nodes.display(),
            edges.display()
        ),
    )
    .await;
    let three = ok(&ctx, "SELECT 3 AS n").await;
    let queries = [
        "SELECT count(*) AS n FROM degree(files)",
        "SELECT count(*) AS n FROM files.nodes",
        "SELECT count(*) AS n FROM GRAPH_TABLE (files MATCH (a) COLUMNS (a.id))",
    ];
    // Twice: the second time from what the session keeps.
    for _ in 0..2 {
        for sql in queries {
            assert_eq!(ok(&ctx, sql).await, three, "{sql}");
        }
    }
    // The file gains a row with a key it already has (and another size).
    std::fs::write(&nodes, "id,name\n1,x\n2,w\n3,q\n2,again\n").unwrap();
    for sql in queries {
        assert_contains(
            &err(&ctx, sql).await,
            "graph 'files': vertex table 'nodes' has 1 row with a duplicate key (id = 2)",
        );
    }
    // And is clean again.
    std::fs::write(&nodes, "id,name\n1,x\n2,w\n3,q\n4,new\n").unwrap();
    let four = ok(&ctx, "SELECT 4 AS n").await;
    for sql in queries {
        assert_eq!(ok(&ctx, sql).await, four, "{sql}");
    }
}
