//! SQL/PGQ pattern matching, `GRAPH_TABLE (g MATCH … COLUMNS (…))` (#296).
//! Each match is checked against the SQL join it means.

use std::path::{Path, PathBuf};

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::prelude::SessionContext;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data")
        .join(file)
}

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

async fn err(ctx: &SessionContext, sql: &str) -> String {
    match run(ctx, sql).await {
        Ok(batches) => panic!(
            "{sql} should fail; got {}",
            pretty_format_batches(&batches).unwrap()
        ),
        Err(e) => ursa_engine::user_message(&e),
    }
}

fn assert_contains(message: &str, part: &str) {
    assert!(message.contains(part), "expected '{part}' in:\n{message}");
}

/// Both queries return the same rows (as sorted multisets).
async fn same_rows(ctx: &SessionContext, matched: &str, joined: &str) {
    let sorted = |q: &str| format!("SELECT * FROM ({q}) ORDER BY 1, 2");
    // The rows, without the header (the column names differ).
    let rows = |table: String| {
        table
            .lines()
            .skip(3)
            .filter(|l| !l.starts_with('+'))
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (a, b) = (
        rows(ok(ctx, &sorted(matched)).await),
        rows(ok(ctx, &sorted(joined)).await),
    );
    assert_eq!(a, b, "\n{matched}\n{joined}");
    assert!(a.lines().count() > 1, "no rows: {matched}");
}

/// The Getting Started session: `karate` (with its members) and
/// `karate_edges_only` (endpoints only).
async fn karate() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        &format!(
            "CREATE VIEW members AS SELECT * FROM '{}';
             CREATE VIEW ties AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH karate
               VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                 DESTINATION KEY (dst) REFERENCES members (id));
             CREATE PROPERTY GRAPH karate_edges_only
               EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));",
            data("karate_nodes.csv").display(),
            data("karate_edges.csv").display()
        ),
    )
    .await;
    ctx
}

#[tokio::test]
async fn an_edge_pattern_is_the_edge_table_joined_to_its_nodes() {
    let ctx = karate().await;
    same_rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]->(b) COLUMNS (a.id AS a, b.id AS b))",
        "SELECT src, dst FROM ties",
    )
    .await;
    // Pointing left reverses it; abbreviated edges are the same edges.
    same_rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate MATCH (a)<-[e]-(b) COLUMNS (a.id AS a, b.id AS b))",
        "SELECT dst, src FROM ties",
    )
    .await;
    same_rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate MATCH (a)->(b) COLUMNS (a.id AS a, b.id AS b))",
        "SELECT src, dst FROM ties",
    )
    .await;
    // Undirected: both orientations of every edge.
    same_rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]-(b) COLUMNS (a.id AS a, b.id AS b))",
        "SELECT src, dst FROM ties UNION ALL SELECT dst, src FROM ties",
    )
    .await;
    // A graph without vertex tables matches its endpoints.
    same_rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate_edges_only MATCH (a)-[e]->(b) COLUMNS (a.id AS a, b.id AS b))",
        "SELECT src, dst FROM ties",
    )
    .await;
}

#[tokio::test]
async fn paths_conditions_and_properties() {
    let ctx = karate().await;
    // Two steps from member 0, by a condition on a node.
    same_rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate
           MATCH (a WHERE a.id = 0)-[e]->(b)-[f]->(c)
           COLUMNS (b.id AS b, c.id AS c))",
        "SELECT t1.dst, t2.dst FROM ties t1 JOIN ties t2 ON t1.dst = t2.src WHERE t1.src = 0",
    )
    .await;
    // Officers' ties to Mr. Hi's members, with a pattern WHERE and properties.
    same_rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate
           MATCH (a)-[e]-(b)
           WHERE a.club = 'Officer' AND b.club = 'Mr. Hi'
           COLUMNS (a.id AS officer, b.id AS member))",
        "SELECT m1.id, m2.id FROM members m1, members m2, ties t
         WHERE ((t.src = m1.id AND t.dst = m2.id) OR (t.src = m2.id AND t.dst = m1.id))
           AND m1.club = 'Officer' AND m2.club = 'Mr. Hi'",
    )
    .await;
    // Two path patterns sharing a node: a triangle a -> b -> c, a -> c.
    same_rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate
           MATCH (a)-[]->(b)-[]->(c), (a)-[]->(c)
           COLUMNS (a.id AS a, c.id AS c))",
        "SELECT t1.src, t2.dst FROM ties t1 JOIN ties t2 ON t1.dst = t2.src
         JOIN ties t3 ON t3.src = t1.src AND t3.dst = t2.dst",
    )
    .await;
    // Quotes inside the pattern survive the rewrite.
    assert_contains(
        &ok(
            &ctx,
            "SELECT count(*) AS n FROM GRAPH_TABLE (karate
               MATCH (a WHERE a.club = 'Mr. Hi' AND 'it''s' = 'it''s') COLUMNS (a.id))",
        )
        .await,
        "| 17 |",
    );
}

#[tokio::test]
async fn a_graph_table_is_a_table_anywhere_sql_takes_one() {
    let ctx = karate().await;
    // Joined to an algorithm, aliased, in a CTE, and in a view.
    let ranked = ok(
        &ctx,
        "WITH ties_of_zero AS (
           SELECT * FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[e]-(b) COLUMNS (b.id AS id)) t
         )
         SELECT id, round(pagerank, 4) AS pagerank
         FROM ties_of_zero JOIN pagerank(karate, direction => 'both') USING (id)
         ORDER BY pagerank DESC LIMIT 2",
    )
    .await;
    assert_contains(&ranked, "| 2  | 0.0571   |");
    ok(
        &ctx,
        "CREATE VIEW friendships AS SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]-(b) COLUMNS (a.id AS a, b.id AS b))",
    )
    .await;
    assert_contains(
        &ok(&ctx, "SELECT count(*) AS n FROM friendships").await,
        "| 156 |",
    );
    assert_contains(
        &ok(
            &ctx,
            "SELECT g.a FROM GRAPH TABLE (karate MATCH (a)-[e]->(b) COLUMNS (a.id AS a)) AS g WHERE g.a = 32 LIMIT 1",
        )
        .await,
        "| 32 |",
    );
}

/// People who follow each other and write posts; a self-loop, for counting.
async fn social() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        "CREATE TABLE people AS SELECT * FROM (VALUES (1, 'Ann'), (2, 'Bo'), (3, 'Cy')) t(id, name);
         CREATE TABLE posts AS SELECT * FROM (VALUES (10, 'hello', 5), (11, 'graphs', 9)) t(pid, title, likes);
         CREATE TABLE follows AS SELECT * FROM (VALUES (1, 2, 2020), (2, 3, 2021), (3, 3, 2022)) t(a, b, since);
         CREATE TABLE wrote AS SELECT * FROM (VALUES (1, 10), (3, 11)) t(author, post);
         CREATE PROPERTY GRAPH social
           VERTEX TABLES (
             people AS person KEY (id) LABEL person PROPERTIES (id, name AS full_name),
             posts KEY (pid) PROPERTIES (pid, title)
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
async fn labels_and_properties_across_several_tables() {
    let ctx = social().await;
    // The post's table is inferred from `wrote`'s REFERENCES.
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social
               MATCH (p:person)-[w:wrote]->(post)
               COLUMNS (p.full_name AS author, post.title AS title))
             ORDER BY author",
        )
        .await,
        "\
+--------+--------+
| author | title  |
+--------+--------+
| Ann    | hello  |
| Cy     | graphs |
+--------+--------+"
    );
    // An edge label, an edge property, and a label written with IS.
    assert_eq!(
        ok(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social
               MATCH (a IS person)-[k:knows WHERE k.since >= 2021]->(b)
               COLUMNS (a.full_name AS a, b.full_name AS b, k.since AS since))
             ORDER BY since",
        )
        .await,
        "\
+----+----+-------+
| a  | b  | since |
+----+----+-------+
| Bo | Cy | 2021  |
| Cy | Cy | 2022  |
+----+----+-------+"
    );
    // An undirected edge matches a self-loop once, and others both ways.
    assert_contains(
        &ok(
            &ctx,
            "SELECT count(*) AS n FROM GRAPH_TABLE (social MATCH (a:person)-[k:knows]-(b) COLUMNS (a.id))",
        )
        .await,
        "| 5 |",
    );
    // Properties are what the labels expose, by their names.
    assert_contains(
        &err(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social MATCH (a:person) COLUMNS (a.name))",
        )
        .await,
        "name",
    );
    assert_contains(
        &err(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (social MATCH (p:posts) COLUMNS (p.likes))",
        )
        .await,
        "likes",
    );
}

#[tokio::test]
async fn patterns_that_cannot_be_matched_are_refused() {
    let ctx = social().await;
    for (sql, expected) in [
        (
            "SELECT * FROM GRAPH_TABLE (social MATCH (a)-[e]->(b) COLUMNS (a.id))",
            "which edge table edge 'e' is cannot be told from the pattern; give it a label",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (social MATCH (a) COLUMNS (a.id))",
            "which vertex table node 'a' is cannot be told from the pattern; give it a label",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (social MATCH (a:robot) COLUMNS (a.id))",
            "graph 'social' has no vertex table or vertex label 'robot' (for 'a'); its vertex \
             tables are person, posts",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (social MATCH (a:person)-[e:likes]->(b) COLUMNS (a.id))",
            "graph 'social' has no edge table or edge label 'likes' (for 'e')",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (social MATCH (a:person)-[e:knows]->*(b) COLUMNS (a.id))",
            "has no upper bound, so its walks are endless",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (social MATCH (a:person))",
            "a GRAPH_TABLE needs COLUMNS (…)",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (nope MATCH (a) COLUMNS (a.id))",
            "graph 'nope' does not exist",
        ),
    ] {
        assert_contains(&err(&ctx, sql).await, expected);
    }
}
