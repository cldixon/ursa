//! Standard and DuckPGQ forms of `GRAPH_TABLE` and `CREATE PROPERTY GRAPH`
//! that used to be refused (#413), and clear errors for SQL/PGQ path
//! features Ursa does not have yet (#414). The graph is DuckPGQ's canonical
//! `pg` (setup in #410).

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

/// The rows `sql` returns, as a table, its rows sorted.
async fn ok(ctx: &SessionContext, sql: &str) -> String {
    let batches = run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)));
    let table = pretty_format_batches(&batches).unwrap().to_string();
    let mut lines: Vec<&str> = table.lines().collect();
    if lines.len() > 4 {
        let end = lines.len() - 1;
        lines[3..end].sort();
    }
    lines.join("\n")
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

/// `SELECT * FROM GRAPH_TABLE (pg MATCH <rest>)`.
fn pg(rest: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (pg MATCH {rest})")
}

// ---- #413 ----------------------------------------------------------------

#[tokio::test]
async fn columns_left_out_or_star_return_every_named_variable() {
    let ctx = school().await;
    let expected = ok(
        &ctx,
        &pg("(a:Person WHERE a.id = 0)-[k:knows]->(b:Person) COLUMNS (a.*, k.*, b.*)"),
    )
    .await;
    assert_eq!(
        expected,
        "+----+--------+------------+-----+-----+------+---------+
| id | name   | createdate | src | dst | id:1 | name:1  |
+----+--------+------------+-----+-----+------+---------+
| 0  | Daniel | 10         | 0   | 1   | 1    | Tavneet |
| 0  | Daniel | 11         | 0   | 2   | 2    | Gabor   |
| 0  | Daniel | 12         | 0   | 3   | 3    | Peter   |
+----+--------+------------+-----+-----+------+---------+"
    );
    for rest in [
        "(a:Person WHERE a.id = 0)-[k:knows]->(b:Person)",
        "(a:Person WHERE a.id = 0)-[k:knows]->(b:Person) COLUMNS (*)",
        "(a:Person)-[k:knows]->(b:Person) WHERE a.id = 0",
    ] {
        assert_eq!(ok(&ctx, &pg(rest)).await, expected, "{rest}");
    }
    // A quantified edge has no properties of its own: it is left out.
    assert_eq!(
        ok(
            &ctx,
            &pg("(a:Person WHERE a.id = 4)-[k:knows]->{1,1}(b:Person)")
        )
        .await,
        ok(
            &ctx,
            &pg("(a:Person WHERE a.id = 4)-[k:knows]->{1,1}(b:Person) COLUMNS (a.*, b.*)")
        )
        .await
    );
    assert_contains(
        &err(&ctx, &pg("(:Person)-[:knows]->(:Person)")).await,
        "a GRAPH_TABLE without COLUMNS returns the properties of the pattern's named nodes and \
         edges, and it names none",
    );
}

#[tokio::test]
async fn left_or_right_edges_are_either_way_and_tilde_edges_are_refused() {
    let ctx = school().await;
    let either = ok(&ctx, &pg("(a:Person)-[k:knows]-(b:Person) COLUMNS (a.id, b.id)")).await;
    assert_eq!(
        ok(&ctx, &pg("(a:Person)<-[k:knows]->(b:Person) COLUMNS (a.id, b.id)")).await,
        either
    );
    assert_eq!(
        ok(&ctx, &pg("(a:Person)<->(b:Person) COLUMNS (a.id, b.id)")).await,
        ok(&ctx, &pg("(a:Person)-(b:Person) COLUMNS (a.id, b.id)")).await,
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT count(*) AS n FROM GRAPH_TABLE (pg MATCH (a:Person)<-[k:knows]->(b:Person) \
             COLUMNS (a.id))"
        )
        .await,
        ok(&ctx, "SELECT 16 AS n").await
    );
    for edge in ["~[k:knows]~", "<~[k:knows]~", "~[k:knows]~>", "~", "<~"] {
        let message = err(
            &ctx,
            &pg(&format!("(a:Person){edge}(b:Person) COLUMNS (a.id)")),
        )
        .await;
        assert_eq!(
            message,
            "in GRAPH_TABLE: Ursa's edges are directed; use -[e]- to follow them either way \
             (~[e]~, <~[e]~, ~[e]~> and ~ are for undirected edges)",
            "{edge}"
        );
    }
}

#[tokio::test]
async fn element_id_of_a_node_is_its_key_and_of_an_edge_its_row() {
    let ctx = school().await;
    assert_eq!(
        ok(
            &ctx,
            &pg("(a:Person WHERE a.id = 3)-[s:studyAt]->(b:School) \
                 COLUMNS (element_id(a) AS ea, element_id(b))")
        )
        .await,
        "+----+---------------+
| ea | element_id(b) |
+----+---------------+
| 3  | 1             |
+----+---------------+"
    );
    // An edge's is the row edges(p) lists: the one edge of a path.
    assert_eq!(
        ok(
            &ctx,
            &pg("p = (a:Person WHERE a.id = 1)-[k:knows]->(b:Person) \
                 COLUMNS (b.id, element_id(k) AS ek, edges(p) AS e)")
        )
        .await,
        "+----+----+-----+
| id | ek | e   |
+----+----+-----+
| 2  | 4  | [4] |
| 3  | 5  | [5] |
+----+----+-----+"
    );
    for (rest, expected) in [
        (
            "(a:Person)-[k:knows]->(b:Person) COLUMNS (element_id(k))",
            "element_id(k) of an edge is its row in the edge table, which only a path search \
             reads for now; name the path, as p = (a)-[k]->(b), or return edges(p)",
        ),
        (
            "p = (a:Person WHERE a.id = 0)-[k:knows]->{1,2}(b:Person) COLUMNS (element_id(k))",
            "edge 'k' is quantified, so it stands for several edges",
        ),
    ] {
        assert_contains(&err(&ctx, &pg(rest)).await, expected);
    }
}

#[tokio::test]
async fn same_all_different_is_labeled_and_is_source_of() {
    let ctx = school().await;
    let count = |rest: &str| {
        format!("SELECT count(*) AS n FROM GRAPH_TABLE (pg MATCH {rest} COLUMNS (a.id))")
    };
    let chain = "(a:Person)-[k:knows]->(b:Person)-[l:knows]->(c:Person)";
    for (condition, n) in [
        ("TRUE", 11),
        ("all_different(a, b, c)", 9),
        ("NOT same(a, c)", 9),
        ("same(a, c)", 2),
        ("SAME(a, c, a)", 2),
        (
            "a IS SOURCE OF k AND b IS DESTINATION OF k AND b IS SOURCE OF l",
            11,
        ),
        ("a IS DESTINATION OF l", 2),
        ("a IS NOT DESTINATION OF l", 9),
        (
            "k IS LABELED Knows AND a IS LABELED person AND b IS NOT LABELED School",
            11,
        ),
        ("k IS LABELED studyAt", 0),
    ] {
        assert_eq!(
            ok(&ctx, &count(&format!("{chain} WHERE {condition}"))).await,
            ok(&ctx, &format!("SELECT {n} AS n")).await,
            "{condition}"
        );
    }
    // Nodes of two tables are two nodes; an edge that does not start at a
    // node's table does not start at the node.
    let study = "(a:Person)-[s:studyAt]->(b:School)";
    for (condition, n) in [
        ("all_different(a, b)", 5),
        ("same(a, b)", 0),
        ("b IS SOURCE OF s", 0),
        ("a IS SOURCE OF s AND b IS DESTINATION OF s", 5),
    ] {
        assert_eq!(
            ok(&ctx, &count(&format!("{study} WHERE {condition}"))).await,
            ok(&ctx, &format!("SELECT {n} AS n")).await,
            "{condition}"
        );
    }
    // Either way: the orientation each row matched.
    assert_eq!(
        ok(
            &ctx,
            &pg("(a:Person WHERE a.id = 0)-[k:knows]-(b:Person) \
                 COLUMNS (b.id, a IS SOURCE OF k AS out, same(a, b), k IS LABELED knows AS l)")
        )
        .await,
        "+----+-------+------------+------+
| id | out   | same(a, b) | l    |
+----+-------+------------+------+
| 1  | true  | false      | true |
| 2  | true  | false      | true |
| 3  | false | false      | true |
| 3  | true  | false      | true |
+----+-------+------------+------+"
    );
    for (rest, expected) in [
        (
            "(a:Person)-[k:knows]->(b:Person) WHERE same(a, k) COLUMNS (a.id)",
            "SAME compares nodes for now; 'k' is an edge",
        ),
        (
            "(a:Person)-[k:knows]->(b:Person) WHERE all_different(a) COLUMNS (a.id)",
            "ALL_DIFFERENT takes two node variables or more",
        ),
        (
            "(a:Person)-[k:knows]->(b:Person) WHERE same(a, x) COLUMNS (a.id)",
            "'x' is not a node or edge variable of the pattern; its variables are a, k, b",
        ),
        (
            "(a:Person)-[k:knows]->(b:Person) WHERE a IS LABELED Teacher COLUMNS (a.id)",
            "graph 'pg' has no label 'teacher'",
        ),
        (
            "(a:Person)-[k:knows]->(b:Person) WHERE k IS SOURCE OF a COLUMNS (a.id)",
            "k IS SOURCE OF a takes a node variable before IS",
        ),
        (
            "(a:Person WHERE a.id = 0)-[k:knows]->{1,2}(b:Person) WHERE a IS SOURCE OF k \
             COLUMNS (a.id)",
            "edge 'k' is quantified, so it stands for several edges",
        ),
    ] {
        assert_contains(&err(&ctx, &pg(rest)).await, expected);
    }
}

#[tokio::test]
async fn a_quantified_edge_variable_says_it_stands_for_several_edges() {
    let ctx = school().await;
    for columns in ["k.createDate", "k.*"] {
        assert_eq!(
            err(
                &ctx,
                &pg(&format!(
                    "(a:Person WHERE a.id = 0)-[k:knows]->{{1,2}}(b:Person) COLUMNS ({columns})"
                ))
            )
            .await,
            "in GRAPH_TABLE: edge 'k' is quantified, so it stands for several edges; use \
             edges(p), or an aggregate once group variables are supported",
        );
    }
    assert_contains(
        &err(
            &ctx,
            &pg("p = ANY SHORTEST (a:Person WHERE a.id = 0)-[k:knows]->(b:Person) \
                 COLUMNS (k.createDate)"),
        )
        .await,
        "edge 'k' is the edge of a searched path",
    );
}

#[tokio::test]
async fn references_without_a_column_names_the_vertex_tables_key() {
    let ctx = school().await;
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH g2 VERTEX TABLES (Student KEY (id))
           EDGE TABLES (know SOURCE KEY (src) REFERENCES Student
                             DESTINATION KEY (dst) REFERENCES Student);
         CREATE PROPERTY GRAPH g3
           VERTEX TABLES (Student AS Person KEY (id), School KEY (school_id))
           EDGE TABLES (studyAt SOURCE KEY (personId) REFERENCES Person
                                DESTINATION KEY (schoolId) REFERENCES School);",
    )
    .await;
    assert_eq!(
        ok(
            &ctx,
            "SELECT count(*) AS n FROM GRAPH_TABLE (g2 MATCH (a)-[k]->(b) COLUMNS (a.id))"
        )
        .await,
        ok(&ctx, "SELECT 8 AS n").await
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (g3 MATCH (a)-[s]->(b WHERE b.school_name = 'VU') \
             COLUMNS (a.name))"
        )
        .await,
        ok(
            &ctx,
            "SELECT * FROM (VALUES ('Daniel'), ('Tavneet')) t(name)"
        )
        .await
    );
    // A key named by another edge's REFERENCES is known too.
    ok(
        &ctx,
        "CREATE PROPERTY GRAPH g4 VERTEX TABLES (Student)
           EDGE TABLES (know SOURCE KEY (src) REFERENCES Student (id)
                             DESTINATION KEY (dst) REFERENCES Student);",
    )
    .await;
    // Otherwise the key is not known.
    assert_contains(
        &err(
            &ctx,
            "CREATE PROPERTY GRAPH g5 VERTEX TABLES (Student)
               EDGE TABLES (know SOURCE KEY (src) REFERENCES Student
                                 DESTINATION KEY (dst) REFERENCES Student)",
        )
        .await,
        "SOURCE KEY (src) REFERENCES student names no column, so it references the vertex \
         table's key, and vertex table 'student' has none declared",
    );
    assert_contains(
        &err(
            &ctx,
            "CREATE PROPERTY GRAPH g5 VERTEX TABLES (Student KEY (id))
               EDGE TABLES (know SOURCE KEY (src) REFERENCES Teacher
                                 DESTINATION KEY (dst) REFERENCES Student)",
        )
        .await,
        "SOURCE KEY (src) REFERENCES teacher, but the vertex table is student KEY (id)",
    );
}

// ---- #414 ----------------------------------------------------------------

#[tokio::test]
async fn keep_on_one_path_is_its_prefix_form() {
    let ctx = school().await;
    let path = "p = (a:Person WHERE a.id = 0)-[k:knows]->*(b:Person)";
    let columns = "COLUMNS (b.id, path_length(p) AS n)";
    for (keep, prefix) in [
        ("KEEP ANY SHORTEST", "ANY SHORTEST"),
        ("KEEP ALL SHORTEST", "ALL SHORTEST"),
        ("KEEP SHORTEST 2 PATHS", "SHORTEST 2"),
        ("KEEP TRAIL", "TRAIL"),
        ("KEEP ANY SHORTEST ACYCLIC PATH", "ANY SHORTEST ACYCLIC"),
    ] {
        let prefixed = path.replace("p = ", &format!("p = {prefix} "));
        assert_eq!(
            ok(&ctx, &pg(&format!("{path} {keep} {columns}"))).await,
            ok(&ctx, &pg(&format!("{prefixed} {columns}"))).await,
            "{keep}"
        );
    }
    assert_contains(
        &err(
            &ctx,
            &pg(
                "(a:Person)-[k:knows]->*(b:Person), (c:Person)-[l:knows]->*(a) KEEP ANY SHORTEST \
                 COLUMNS (a.id)",
            ),
        )
        .await,
        "KEEP over several path patterns is not supported yet; put the selector or path mode \
         before each path",
    );
}

#[tokio::test]
async fn path_features_not_supported_yet_say_what_to_write() {
    let ctx = school().await;
    let knows = "(a:Person)-[k:knows]->(b:Person)";
    for (rest, expected) in [
        (
            format!("{knows} COLUMNS (count(*) AS n)"),
            "in GRAPH_TABLE: aggregates in COLUMNS need a quantified edge variable, which is not \
             supported yet; aggregate outside: SELECT count(*) FROM GRAPH_TABLE (…)",
        ),
        (
            format!("{knows} COLUMNS (a.id, sum(k.createDate))"),
            "aggregates in COLUMNS need a quantified edge variable",
        ),
        (
            "p = (a:Person WHERE a.id = 0)-[k:knows]->{1,2}(b:Person) \
             ONE ROW PER STEP (v1, e, v2) COLUMNS (v1.id)"
                .to_string(),
            "in GRAPH_TABLE: ONE ROW PER VERTEX and ONE ROW PER STEP are not supported yet; \
             return vertices(p) or edges(p) in COLUMNS and unnest them",
        ),
        (
            format!("{knows} WHERE a.id = (SELECT min(id) FROM Student) COLUMNS (b.id)"),
            "in GRAPH_TABLE: a subquery inside the pattern reads table 'student', which the \
             statement does not name outside the GRAPH_TABLE",
        ),
        (
            "(a:Person {name: 'Daniel'}) COLUMNS (a.id)".to_string(),
            "in GRAPH_TABLE: property maps ({…}) are Cypher; write the condition with WHERE, as \
             (a:Person WHERE a.name = 'Daniel')",
        ),
        (
            "(a:Person)-[k:knows*1..3]->(b:Person) COLUMNS (a.id)".to_string(),
            "in GRAPH_TABLE: *1..3 inside an edge's brackets is Cypher's range; in SQL/PGQ the \
             quantifier follows the edge: write -[k:knows]->{1,3}",
        ),
        (
            "(a:Person|School) COLUMNS (a.id)".to_string(),
            "in GRAPH_TABLE: label expressions (|, &, !, %) are not supported yet; use one label \
             (#412)",
        ),
    ] {
        assert_contains(&err(&ctx, &pg(&rest)).await, expected);
    }
    // A table outside the GRAPH_TABLE.
    for sql in [
        "SELECT * FROM Student s, LATERAL GRAPH_TABLE (pg MATCH (a:Person)-[k:knows]->(b:Person) \
         WHERE a.id = s.id COLUMNS (b.id))",
        "SELECT * FROM Student s, GRAPH_TABLE (pg MATCH (a:Person)-[k:knows]->(b:Person) \
         WHERE a.id = s.id COLUMNS (b.id))",
    ] {
        assert_contains(
            &err(&ctx, sql).await,
            "GRAPH_TABLE cannot refer to tables outside it yet",
        );
    }
    // With the table named outside, the subquery runs (as documented).
    ok(
        &ctx,
        "SELECT g.id FROM GRAPH_TABLE (pg MATCH (a:Person)-[k:knows]->(b:Person) \
         WHERE a.id = (SELECT min(id) FROM Student) COLUMNS (b.id)) g \
         JOIN Student s ON s.id = g.id",
    )
    .await;
}
