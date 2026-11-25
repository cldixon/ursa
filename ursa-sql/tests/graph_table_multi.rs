//! A `GRAPH_TABLE` variable over several element tables (#412): an
//! unlabelled variable, a label several tables carry, and a disjunction of
//! labels (`:a|b`). The match is a `UNION ALL` of one plan per combination of
//! tables the edges' `REFERENCES` allow; one combination is planned as it
//! always was. On DuckPGQ's canonical graph `pg` (setup in #410), and on a
//! graph where one label is on two tables.

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

/// The one value `sql` returns, as text.
async fn value(ctx: &SessionContext, sql: &str) -> String {
    let batches = run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)));
    let table = pretty_format_batches(&batches).unwrap().to_string();
    table
        .lines()
        .nth(3)
        .unwrap()
        .trim_matches(['|', ' '])
        .to_string()
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

/// The logical plan `EXPLAIN` gives for `sql`.
async fn logical_plan(ctx: &SessionContext, sql: &str) -> String {
    use datafusion::arrow::array::{Array, StringArray};
    let batches = run(ctx, &format!("EXPLAIN {sql}")).await.unwrap();
    let batch = &batches[0];
    let column = |i: usize| {
        batch
            .column(i)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("text")
            .clone()
    };
    let (types, plans) = (column(0), column(1));
    (0..types.len())
        .find(|&i| types.value(i) == "logical_plan")
        .map(|i| plans.value(i).to_string())
        .expect("a logical plan")
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

/// LDBC's shape: `Message` is the label of two vertex tables, `Likes` of two
/// edge tables. A comment's `id` is an Int32 where a post's is an Int64, and
/// `len` is a number in one and text in the other.
async fn messages() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        "CREATE TABLE Person AS SELECT * FROM (VALUES (100,'Ann'),(101,'Bob')) t(id, name);
         CREATE TABLE Post AS SELECT * FROM (VALUES (1,'hello',10),(2,'world',11)) t(id, content, len);
         CREATE TABLE Comment AS SELECT CAST(id AS INT) AS id, content, len
           FROM (VALUES (10,'nice','4'),(11,'meh','x')) t(id, content, len);
         CREATE TABLE likesPost AS SELECT * FROM (VALUES (100,1),(101,2)) t(p, m);
         CREATE TABLE likesComment AS SELECT * FROM (VALUES (100,10),(100,11)) t(p, m);
         CREATE PROPERTY GRAPH social
           VERTEX TABLES (Person, Post LABEL Message, Comment LABEL Message)
           EDGE TABLES (
             likesPost SOURCE KEY (p) REFERENCES Person (id)
                       DESTINATION KEY (m) REFERENCES Post (id) LABEL Likes,
             likesComment SOURCE KEY (p) REFERENCES Person (id)
                          DESTINATION KEY (m) REFERENCES Comment (id) LABEL Likes);",
    )
    .await;
    ctx
}

/// `SELECT * FROM GRAPH_TABLE (pg MATCH <rest>`: `rest` closes the bracket.
fn pg(rest: &str) -> String {
    format!("SELECT * FROM GRAPH_TABLE (pg MATCH {rest}")
}

#[tokio::test]
async fn an_unlabelled_variable_is_a_row_of_every_table_that_fits() {
    let ctx = school().await;
    // Every vertex table: five students and two schools.
    assert_eq!(
        value(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (pg MATCH (a) COLUMNS (element_id(a)))"
        )
        .await,
        "7"
    );
    // An unlabelled edge from a Person: know or studyAt; and its other end,
    // a student or a school, as the edge table's REFERENCES say.
    assert_eq!(
        ok(
            &ctx,
            &pg("(a:Person WHERE a.id < 2)-[e]->(b)
                 COLUMNS (a.id, element_id(b) AS b, e IS LABELED knows AS knows,
                          b IS LABELED School AS school))"),
        )
        .await,
        "\
+----+---+-------+--------+
| id | b | knows | school |
+----+---+-------+--------+
| 0  | 0 | false | true   |
| 0  | 1 | true  | false  |
| 0  | 2 | true  | false  |
| 0  | 3 | true  | false  |
| 1  | 0 | false | true   |
| 1  | 2 | true  | false  |
| 1  | 3 | true  | false  |
+----+---+-------+--------+"
    );
    // Nothing labelled, either direction: know both ways (16), studyAt from
    // each of its ends (5 + 5).
    assert_eq!(
        value(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (pg MATCH (a)-[e]-(b) COLUMNS (element_id(a)))"
        )
        .await,
        "26"
    );
    // Two chains: know then know, know then studyAt; nothing leaves a school.
    assert_eq!(
        value(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (pg MATCH (a)-[]->(b)-[]->(c) COLUMNS (element_id(c)))"
        )
        .await,
        (11 + 8).to_string()
    );
    // SAME is per table: student 0 is not school 0.
    assert_eq!(
        value(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (pg MATCH (a), (b) WHERE SAME(a, b) COLUMNS (element_id(a)))"
        )
        .await,
        "7"
    );
}

#[tokio::test]
async fn a_label_disjunction_and_any_label() {
    let ctx = school().await;
    assert_eq!(
        ok(
            &ctx,
            &pg("(a:Person|School) COLUMNS (element_id(a) AS id, a IS LABELED Person AS person))"),
        )
        .await,
        "\
+----+--------+
| id | person |
+----+--------+
| 0  | false  |
| 0  | true   |
| 1  | false  |
| 1  | true   |
| 2  | true   |
| 3  | true   |
| 4  | true   |
+----+--------+"
    );
    for pattern in [
        "(a IS %)",
        "(a:School|%)",
        "(a) WHERE a IS LABELED Person|School",
    ] {
        assert_eq!(
            value(
                &ctx,
                &format!(
                    "SELECT count(*) FROM GRAPH_TABLE (pg MATCH {pattern} COLUMNS (element_id(a)))"
                )
            )
            .await,
            "7",
            "{pattern}"
        );
    }
    assert_contains(
        &err(&ctx, &pg("(a:Person&School) COLUMNS (element_id(a)))")).await,
        "label expressions with &, ! or parentheses are not supported yet; use one label, or \
         several joined with |, as (a:person|school) (#412)",
    );
    assert_contains(
        &err(&ctx, &pg("(a:Person|Robot) COLUMNS (element_id(a)))")).await,
        "graph 'pg' has no vertex table or vertex label 'robot' (for 'a')",
    );
}

#[tokio::test]
async fn an_undirected_edge_between_two_tables_follows_its_one_orientation() {
    // It used to join the reverse orientation too, on key values that
    // happened to be equal: student 0 at school 1 (from studyAt's (1, 0)).
    let ctx = school().await;
    assert_eq!(
        ok(
            &ctx,
            &pg("(a:Person)-[s:studyAt]-(b:School) COLUMNS (a.id, b.school_id))"),
        )
        .await,
        "\
+----+-----------+
| id | school_id |
+----+-----------+
| 0  | 0         |
| 1  | 0         |
| 2  | 1         |
| 3  | 1         |
| 4  | 1         |
+----+-----------+"
    );
}

#[tokio::test]
async fn a_label_on_two_tables() {
    let ctx = messages().await;
    // Properties every table has; Comment's Int32 ids read as Int64.
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social MATCH (m:Message) COLUMNS (m.id, m.content))",
        )
        .await,
        "\
+----+---------+
| id | content |
+----+---------+
| 1  | hello   |
| 10 | nice    |
| 11 | meh     |
| 2  | world   |
+----+---------+"
    );
    let batches = run(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (social MATCH (m:Message) COLUMNS (m.id))",
    )
    .await
    .unwrap();
    assert_eq!(
        batches[0].schema().field(0).data_type(),
        &datafusion::arrow::datatypes::DataType::Int64
    );
    // An edge label on two tables, and an unlabelled pattern over them.
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social MATCH (p:Person)-[l:Likes]->(m:Message)
               COLUMNS (p.name, m.id, m.content, l.p))",
        )
        .await,
        "\
+------+----+---------+-----+
| name | id | content | p   |
+------+----+---------+-----+
| Ann  | 1  | hello   | 100 |
| Ann  | 10 | nice    | 100 |
| Ann  | 11 | meh     | 100 |
| Bob  | 2  | world   | 101 |
+------+----+---------+-----+"
    );
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social MATCH (p)-[]->(m) WHERE m.content <> 'meh'
               COLUMNS (p.name, m.content))",
        )
        .await,
        "\
+------+---------+
| name | content |
+------+---------+
| Ann  | hello   |
| Ann  | nice    |
| Bob  | world   |
+------+---------+"
    );
    // COLUMNS left out: the properties every table of each variable has.
    assert_eq!(
        ok(&ctx, "SELECT * FROM GRAPH_TABLE (social MATCH (m:Message))").await,
        "\
+----+---------+
| id | content |
+----+---------+
| 1  | hello   |
| 10 | nice    |
| 11 | meh     |
| 2  | world   |
+----+---------+"
    );
}

#[tokio::test]
async fn a_property_some_tables_lack_is_null_in_their_rows() {
    let ctx = school().await;
    // Schools have no name: their rows read NULL, as the standard says.
    assert_eq!(
        ok(&ctx, &pg("(a) COLUMNS (element_id(a) AS id, a.name))")).await,
        "\
+----+---------+
| id | name    |
+----+---------+
| 0  |         |
| 0  | Daniel  |
| 1  |         |
| 1  | Tavneet |
| 2  | Gabor   |
| 3  | Peter   |
| 4  | David   |
+----+---------+"
    );
    // The standard's own case (match/11): a.id is a student's, NULL for a school.
    assert_eq!(
        value(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (pg MATCH (a) COLUMNS (a.id)) WHERE id IS NULL"
        )
        .await,
        "2"
    );
    // An edge property, and in a condition: NULL is not > 10.
    assert_eq!(
        value(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (pg MATCH (a:Person)-[e]->(b)
               WHERE e.createDate > 10 COLUMNS (e.createDate))"
        )
        .await,
        "7"
    );
    // A property no table has is a mistake, and the error names the tables.
    assert_contains(
        &err(&ctx, &pg("(a) COLUMNS (a.nmae))")).await,
        "in GRAPH_TABLE: node 'a' can be a row of vertex tables student and school, and none \
         of them has property 'nmae'; their properties are id, name, school_name, school_id, \
         school_kind (#412)",
    );
    // a.* is what every table has.
    assert_contains(
        &err(&ctx, &pg("(a))")).await,
        "a.* returns the properties every table 'a' can be a row of has, and its tables have \
         none in common; list what to return, as COLUMNS (element_id(a)), or give 'a' a label \
         (#412)",
    );
}

#[tokio::test]
async fn a_property_must_have_one_type_where_it_is() {
    let ctx = messages().await;
    assert_contains(
        &err(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social MATCH (m:Message) COLUMNS (m.len))",
        )
        .await,
        "in GRAPH_TABLE: node 'm' can be a row of vertex tables post and comment, and property \
         'len' is Int64 in post but Utf8 in comment, which do not mix; give 'm' a label (#412)",
    );
    // In a condition too.
    assert_contains(
        &err(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social MATCH (m:Message WHERE m.len > 0) COLUMNS (m.id))",
        )
        .await,
        "property 'len' is Int64 in post but Utf8 in comment",
    );
}

#[tokio::test]
async fn one_table_per_variable_is_planned_as_before() {
    let ctx = school().await;
    // A node whose table the edge's REFERENCES tell, and the same node
    // labelled: one combination, the same plan, and no UNION.
    for (inferred, labelled) in [
        (
            "(a)-[k:knows]->(b) WHERE a.id = 0 COLUMNS (b.id))",
            "(a:Person)-[k:knows]->(b:Person) WHERE a.id = 0 COLUMNS (b.id))",
        ),
        (
            "(a:Person)-[e]->(b:School) COLUMNS (a.name, b.school_name))",
            "(a:Person)-[e:studyAt]->(b:School) COLUMNS (a.name, b.school_name))",
        ),
    ] {
        let plan = logical_plan(&ctx, &pg(inferred)).await;
        assert_eq!(plan, logical_plan(&ctx, &pg(labelled)).await);
        assert!(!plan.contains("Union"), "{plan}");
    }
    // Several combinations: a UNION.
    let plan = logical_plan(&ctx, &pg("(a:Person)-[e]->(b) COLUMNS (a.id))")).await;
    assert!(plan.contains("Union"), "{plan}");
}

#[tokio::test]
async fn too_many_combinations_and_searches_over_several_edge_tables_are_refused() {
    let ctx = school().await;
    // Six unlabelled nodes: 2^6 = 64 combinations, the most a pattern may have.
    assert_eq!(
        value(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (pg MATCH (a), (b), (c), (d), (e), (f)
               COLUMNS (element_id(a)))"
        )
        .await,
        7usize.pow(6).to_string()
    );
    assert_contains(
        &err(
            &ctx,
            &pg("(a), (b), (c), (d), (e), (f), (g) COLUMNS (element_id(a)))"),
        )
        .await,
        "in GRAPH_TABLE: the pattern can match more than 64 combinations of element tables, one \
         plan each; give its variables labels so fewer tables fit, as (a:person) or \
         -[e:knows]-> (variables over several tables: a (2), b (2), c (2), d (2), e (2), f (2), \
         g (2)) (#412)",
    );
    assert_contains(
        &err(
            &ctx,
            &pg("(a:Person)-[e]->{1,2}(b) COLUMNS (element_id(b)))"),
        )
        .await,
        "in GRAPH_TABLE: edge 'e' is quantified or in a searched path, and it can be an edge of \
         know or studyat; a path search runs over one edge table for now, so give it a label, \
         as -[e:know]->* (#412)",
    );
    // A quantified edge of one table, between unlabelled nodes, still runs.
    assert_eq!(
        value(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (pg MATCH (a)-[k:knows]->{1,2}(b) COLUMNS (b.id))"
        )
        .await,
        "19"
    );
}

#[tokio::test]
async fn a_pattern_no_combination_fits_is_refused() {
    let ctx = school().await;
    assert_contains(
        &err(&ctx, &pg("(a:School)-[e]->(b) COLUMNS (element_id(a)))")).await,
        "no edge table of graph 'pg' fits edge 'e' as written, (a:school)->(b): the edge can be \
         a row of know or studyat, (a:school) of school and (b) of student or school, and none \
         of those edge tables connects them that way, so the pattern can match nothing",
    );
    // Every variable on one table: the #410 refusal, as before.
    assert_contains(
        &err(
            &ctx,
            &pg("(a:Person)-[k:knows]->(b:School) COLUMNS (a.id))"),
        )
        .await,
        "no edge of 'know' connects them",
    );
}
