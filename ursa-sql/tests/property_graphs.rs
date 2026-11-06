//! SQL/PGQ graph definitions beyond one edge table (#295): several vertex and
//! edge tables, `AS` names, labels, properties, and vertex keys inferred as
//! DuckPGQ infers them. Algorithms run on one (vertex table, edge table) pair,
//! as DuckPGQ's `pagerank(g, vertex, edge)` does.

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
            "{sql} should fail; got {}",
            pretty_format_batches(&batches).unwrap()
        ),
        Err(e) => ursa_engine::user_message(&e),
    }
}

/// Whether `message` contains `part`, with runs of spaces (table padding)
/// compared as one.
fn assert_contains(message: &str, part: &str) {
    let squeeze = |s: &str| {
        s.split(' ')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(
        squeeze(message).contains(&squeeze(part)),
        "expected '{part}' in:\n{message}"
    );
}

/// DuckPGQ's PageRank test data (test/sql/scalar/pagerank.test).
const STUDENTS: &str = "
CREATE TABLE Student AS SELECT * FROM (VALUES
  (0, 'Daniel'), (1, 'Tavneet'), (2, 'Gabor'), (3, 'Peter'), (4, 'David')) t(id, name);
CREATE TABLE know AS SELECT * FROM (VALUES
  (0, 1, 10), (0, 2, 11), (0, 3, 12), (3, 0, 13), (1, 2, 14), (1, 3, 15), (2, 3, 16), (4, 3, 17))
  t(src, dst, edge);";

#[tokio::test]
async fn duckpgqs_graph_runs_as_written() {
    let ctx = ursa_engine::new_session();
    ok(&ctx, STUDENTS).await;
    // DuckPGQ's definition: no KEY; the key is the column the edges REFERENCES.
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH pg
           VERTEX TABLES (Student)
           EDGE TABLES (know SOURCE KEY (src) REFERENCES Student (id)
                             DESTINATION KEY (dst) REFERENCES Student (id))",
    )
    .await;
    // DuckPGQ's call, with the pair named; and Ursa's, with it implied.
    let named = ok(
        &ctx,
        "SELECT id, pagerank FROM pagerank(pg, student, know) ORDER BY id",
    )
    .await;
    let implied = ok(&ctx, "SELECT id, pagerank FROM pagerank(pg) ORDER BY id").await;
    assert_eq!(named, implied);
    // Named parameters still follow the pair.
    ok(
        &ctx,
        "SELECT * FROM pagerank(pg, student, know, damping => 0.9, direction => 'both')",
    )
    .await;
    // A one-pair graph keeps its nodes and edges tables.
    assert_eq!(
        ok(&ctx, "SELECT count(*) AS n FROM pg.nodes").await,
        ok(&ctx, "SELECT 5 AS n").await
    );
}

#[tokio::test]
async fn duckpgqs_properties_before_label_run_as_written() {
    let ctx = ursa_engine::new_session();
    ok(&ctx, STUDENTS).await;
    ok(
        &ctx,
        "CREATE TABLE School AS SELECT * FROM (VALUES ('VU', 0, 'University'), ('UVA', 1, 'University'))
           t(name, Id, Kind);
         CREATE TABLE StudyAt AS SELECT * FROM (VALUES (0, 0), (1, 0), (2, 1), (3, 1), (4, 1))
           t(personId, schoolId)",
    )
    .await;
    // DuckPGQ's shortest_path.test and subpath_match.test, as written (#307).
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH pg
         VERTEX TABLES (
             Student PROPERTIES ( id, name ) LABEL Person,
             School LABEL SCHOOL
             )
         EDGE TABLES (
             know    SOURCE KEY ( src ) REFERENCES Student ( id )
                     DESTINATION KEY ( dst ) REFERENCES Student ( id )
                     PROPERTIES ( edge ) LABEL Knows,
             studyAt SOURCE KEY ( personId ) REFERENCES Student ( id )
                     DESTINATION KEY ( SchoolId ) REFERENCES School ( id )
                     LABEL StudyAt
             )",
    )
    .await;
    // Person has the properties written before it, and only those.
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg MATCH (a:Person WHERE a.id = 0) COLUMNS (a.*))"
        )
        .await,
        ok(&ctx, "SELECT 0 AS id, 'Daniel' AS name").await
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH (a:Person)-[k:Knows WHERE k.edge = 10]->(b:Person)
               COLUMNS (a.id AS a_id, b.id AS b_id))"
        )
        .await,
        ok(&ctx, "SELECT 0 AS a_id, 1 AS b_id").await
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH (a:Person)-[s:StudyAt]->(b:School)
               WHERE a.name = 'Daniel'
               COLUMNS (a.name AS name, b.name AS school))"
        )
        .await,
        ok(&ctx, "SELECT 'Daniel' AS name, 'VU' AS school").await
    );
}

#[tokio::test]
async fn a_vertex_key_comes_from_key_then_the_primary_key_then_references() {
    let ctx = ursa_engine::new_session();
    ok(&ctx, STUDENTS).await;
    ok(
        &ctx,
        "CREATE TABLE pk (code BIGINT PRIMARY KEY, name VARCHAR);
         INSERT INTO pk VALUES (0, 'a'), (1, 'b');
         CREATE TABLE pk_edges AS SELECT * FROM (VALUES (0, 1)) t(a, b);",
    )
    .await;
    // The primary key, with no KEY and no REFERENCES.
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH by_pk VERTEX TABLES (pk)
           EDGE TABLES (pk_edges SOURCE KEY (a) DESTINATION KEY (b))",
    )
    .await;
    assert_contains(
        &ok(&ctx, "SHOW GRAPHS").await,
        "| by_pk | pk_edges   | a      | b           | pk           | code       |",
    );
    // Neither: refused, saying how to give one.
    let message = err(
        &ctx,
        "CREATE PROPERTY GRAPH keyless VERTEX TABLES (Student)
           EDGE TABLES (know SOURCE KEY (src) DESTINATION KEY (dst))",
    )
    .await;
    assert_contains(
        &message,
        "vertex table 'student' needs a key: write KEY (column), declare a primary key, or \
         have an edge table REFERENCES it",
    );
    // REFERENCES must name the key.
    let message = err(
        &ctx,
        "CREATE PROPERTY GRAPH wrong VERTEX TABLES (Student KEY (id))
           EDGE TABLES (know SOURCE KEY (src) REFERENCES Student (name)
                             DESTINATION KEY (dst) REFERENCES Student (id))",
    )
    .await;
    assert_contains(
        &message,
        "SOURCE KEY (src) REFERENCES student (name), but the vertex table is student KEY (id); \
         REFERENCES must name the vertex table and its key",
    );
}

/// A social graph: people who follow each other and write posts.
async fn social() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        "CREATE TABLE people AS SELECT * FROM (VALUES (1, 'Ann'), (2, 'Bo'), (3, 'Cy'), (4, 'Di'))
           t(id, name);
         CREATE TABLE posts AS SELECT * FROM (VALUES (10, 'hello'), (11, 'graphs'), (12, 'sql'))
           t(pid, title);
         CREATE TABLE follows AS SELECT * FROM (VALUES (1, 2), (2, 3), (3, 1), (4, 1), (1, 4))
           t(a, b);
         CREATE TABLE wrote AS SELECT * FROM (VALUES (1, 10), (1, 11), (3, 12)) t(author, post);
         CREATE PROPERTY GRAPH social
           VERTEX TABLES (
             people AS person KEY (id) LABEL person PROPERTIES (id, name),
             posts KEY (pid)
           )
           EDGE TABLES (
             follows SOURCE KEY (a) REFERENCES person (id)
                     DESTINATION KEY (b) REFERENCES person (id) LABEL knows,
             wrote SOURCE KEY (author) REFERENCES person (id)
                   DESTINATION KEY (post) REFERENCES posts (pid)
           );",
    )
    .await;
    ctx
}

#[tokio::test]
async fn a_pair_of_a_larger_graph_is_the_one_pair_graph_of_its_tables() {
    let ctx = social().await;
    // The same two tables as a graph of their own.
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH people_only
           VERTEX TABLES (people KEY (id))
           EDGE TABLES (follows SOURCE KEY (a) REFERENCES people (id)
                                DESTINATION KEY (b) REFERENCES people (id))",
    )
    .await;
    let alone = ok(
        &ctx,
        "SELECT * FROM pagerank(people_only, direction => 'both') ORDER BY id",
    )
    .await;
    // Named by element name, or by a label.
    for call in [
        "pagerank(social, person, follows, direction => 'both')",
        "pagerank(social, person, knows, direction => 'both')",
        "pagerank(social, PERSON, Knows, direction => 'both')",
    ] {
        let paired = ok(&ctx, &format!("SELECT * FROM {call} ORDER BY id")).await;
        assert_eq!(paired, alone, "{call}");
    }
    // algorithms() and traversals take the pair the same way.
    ok(
        &ctx,
        "SELECT * FROM algorithms(social, person, knows, degree(direction => 'both'), louvain(seed => 1))",
    )
    .await;
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM shortest_path(social, person, knows, source => 4, target => 3)"
        )
        .await,
        ok(
            &ctx,
            "SELECT * FROM shortest_path(people_only, source => 4, target => 3)"
        )
        .await
    );
}

#[tokio::test]
async fn which_pair_an_algorithm_runs_on_is_never_guessed() {
    let ctx = social().await;
    for (sql, expected) in [
        (
            "SELECT * FROM pagerank(social)",
            "graph 'social' has 2 edge tables (follows, wrote); name the vertex and edge table \
             to run on, as in pagerank(social, <vertex table>, <edge table>)",
        ),
        (
            "SELECT * FROM pagerank(social, person, wrote)",
            "edge table 'wrote' of graph 'social' goes from vertex table 'person' to 'posts'; \
             an algorithm runs on edges between one vertex table",
        ),
        (
            "SELECT * FROM pagerank(social, posts, follows)",
            "edge table 'follows' of graph 'social' connects vertex table 'person', not 'posts'",
        ),
        (
            "SELECT * FROM pagerank(social, person, likes)",
            "graph 'social' has no edge table 'likes'; its edge tables are follows, wrote",
        ),
        (
            "SELECT * FROM pagerank(social, people_x, knows)",
            "graph 'social' has no vertex table 'people_x'; its vertex tables are person, posts",
        ),
        (
            "SELECT * FROM pagerank(social, person)",
            "pagerank() takes a graph, optionally followed by a vertex table and an edge table",
        ),
    ] {
        assert_contains(&err(&ctx, sql).await, expected);
    }
    // No implied pair, so no nodes and edges tables; the element tables are
    // the session's own.
    assert_contains(
        &err(&ctx, "SELECT * FROM social.nodes").await,
        "social.nodes",
    );
    assert_contains(&ok(&ctx, "SELECT count(*) AS n FROM wrote").await, "| 3 |");
}

#[tokio::test]
async fn show_and_describe_a_graph_of_several_tables() {
    let ctx = social().await;
    assert_eq!(
        ok(&ctx, "SHOW GRAPHS").await,
        "\
+--------+------------+--------+-------------+--------------+------------+
| name   | edge_table | source | destination | vertex_table | vertex_key |
+--------+------------+--------+-------------+--------------+------------+
| social | follows    | a      | b           | people       | id         |
| social | wrote      | author | post        | people       | id         |
+--------+------------+--------+-------------+--------------+------------+"
    );
    let described = ok(&ctx, "DESCRIBE GRAPH social").await;
    for row in [
        "| person     | id          | Int64     | key                    | people.id     |",
        "| posts      | pid         | Int64     | key                    | posts.pid     |",
        "| follows    | a           | Int64     | source → person        | follows.a     |",
        "| wrote      | post        | Int64     | destination → posts    | wrote.post    |",
    ] {
        assert_contains(&described, row);
    }
    // DuckPGQ's spelling.
    assert_eq!(ok(&ctx, "DESCRIBE PROPERTY GRAPH social").await, described);
}

#[tokio::test]
async fn definitions_of_several_tables_are_checked() {
    let ctx = social().await;
    for (sql, expected) in [
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (people KEY (id), people KEY (id))",
            "graph 'g' has two element tables named 'people'; give one a different name with AS",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (people KEY (id))
               EDGE TABLES (follows SOURCE KEY (a) REFERENCES users (id)
                                    DESTINATION KEY (b) REFERENCES people (id))",
            "SOURCE KEY (a) REFERENCES users (id), but the vertex table is people KEY (id); \
             REFERENCES must name the vertex table and its key",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (people KEY (id), posts KEY (pid))
               EDGE TABLES (follows SOURCE KEY (a) DESTINATION KEY (b) REFERENCES people (id))",
            "the SOURCE of edge table 'follows' needs REFERENCES: graph 'g' has several vertex \
             tables, so say which one it refers to",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (people KEY (id) PROPERTIES (id, age))",
            "label people property column 'age' not found in table 'people'",
        ),
        (
            "CREATE PROPERTY GRAPH g VERTEX TABLES (posts KEY (title))
               EDGE TABLES (wrote SOURCE KEY (author) DESTINATION KEY (post) REFERENCES posts (title))",
            "the key of vertex table 'posts' (title) is Utf8, but the endpoints of edge table \
             'wrote' are Int64; both must be integers or both strings",
        ),
    ] {
        assert_contains(&err(&ctx, sql).await, expected);
    }
}

#[tokio::test]
async fn every_element_table_is_protected_from_drop() {
    let ctx = social().await;
    for (sql, expected) in [
        (
            "DROP TABLE posts",
            "cannot drop table 'posts': graph 'social' uses it",
        ),
        (
            "DROP TABLE wrote",
            "cannot drop table 'wrote': graph 'social' uses it",
        ),
    ] {
        assert_contains(&err(&ctx, sql).await, expected);
    }
    ok(&ctx, "DROP PROPERTY GRAPH social; DROP TABLE posts").await;
}

#[tokio::test]
async fn a_graph_of_vertex_tables_only() {
    let ctx = ursa_engine::new_session();
    ok(&ctx, STUDENTS).await;
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH roster VERTEX TABLES (Student KEY (id))",
    )
    .await;
    assert_contains(
        &ok(&ctx, "SHOW GRAPHS").await,
        "| roster |            |        |             | student      | id         |",
    );
    assert_contains(
        &err(&ctx, "SELECT * FROM pagerank(roster)").await,
        "graph 'roster' has no edge tables, so there is nothing for an algorithm to run on",
    );
}
