//! `GRAPH_TABLE` through the graph index (#325): a pattern with a bound
//! node expands its fixed edges on the index, a node read only for its key
//! is not joined to its table, and a search with a known last node stops at
//! it. Each such match is checked against the same match planned as joins
//! (its bound condition written so it binds nothing: `OR false`), and
//! `EXPLAIN` shows which plan each one is.

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

/// `table` (a pretty-printed table) with its rows sorted.
fn sorted(table: String) -> String {
    let mut lines: Vec<&str> = table.lines().collect();
    let last = lines.pop();
    if let Some(rows) = lines.get_mut(3..) {
        rows.sort();
    }
    lines.extend(last);
    lines.join("\n")
}

/// The physical plan of `query`.
async fn physical(ctx: &SessionContext, query: &str) -> String {
    let explain = ok(ctx, &format!("EXPLAIN {query}")).await;
    let at = explain.find("physical_plan").expect("a physical plan");
    explain[at..].to_string()
}

/// Whether the plan joins an edge table: a hash join on an edge's endpoint.
fn joins_edges(plan: &str) -> bool {
    plan.lines()
        .any(|l| l.contains("HashJoinExec") && (l.contains("src@") || l.contains("dst@")))
}

/// `query` and `joined` (the same match, planned as joins) give the same
/// table, header and row order included once both are sorted; `query`
/// expands edges through the index (`{1,1}` searches) and joins no edge
/// table, and `joined` does neither. The sorted table.
async fn same_as_joins(ctx: &SessionContext, query: &str, joined: &str) -> String {
    let (a, b) = (sorted(ok(ctx, query).await), sorted(ok(ctx, joined).await));
    assert_eq!(a, b, "\n{query}\n{joined}");
    assert!(a.lines().count() > 4, "no rows: {query}\n{a}");
    let plan = physical(ctx, query).await;
    assert_contains(&plan, "PathSearchExec");
    assert!(!joins_edges(&plan), "{query} joins an edge table:\n{plan}");
    let plan = physical(ctx, joined).await;
    assert!(!plan.contains("{1,1}"), "{joined} is expanded:\n{plan}");
    a
}

/// The Getting Started session: `karate` (with its members) and
/// `karate_edges_only` (endpoints only), over the CSV files.
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
async fn bound_patterns_expand_through_the_index() {
    let ctx = karate().await;
    for (query, joined) in [
        // Two steps out, as stored and either way.
        (
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[e]->(b)-[f]->(c)
               COLUMNS (b.id AS b, c.id AS c))",
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0 OR false)-[e]->(b)-[f]->(c)
               COLUMNS (b.id AS b, c.id AS c))",
        ),
        (
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[]-(b)-[]-(c) WHERE c.id <> 0
               COLUMNS (b.id, c.id, c.club))",
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0 OR false)-[]-(b)-[]-(c) WHERE c.id <> 0
               COLUMNS (b.id, c.id, c.club))",
        ),
        // Bound in the middle: the edge to its left is followed backward.
        (
            "FROM GRAPH_TABLE (karate MATCH (a)-[]->(b WHERE b.id IN (2, 33))<-[]-(c)
               COLUMNS (a.id AS a, b.id AS b, c.club AS club))",
            "FROM GRAPH_TABLE (karate MATCH (a)-[]->(b WHERE b.id IN (2, 33) OR false)<-[]-(c)
               COLUMNS (a.id AS a, b.id AS b, c.club AS club))",
        ),
        // Bound by a subquery, in the pattern's WHERE. (A table a pattern's
        // subquery reads must be named in the statement too.)
        (
            "SELECT g.* FROM GRAPH_TABLE (karate MATCH (a)-[]->(b)
               WHERE a.id IN (SELECT id FROM members WHERE club = 'Mr. Hi') AND b.club = 'Officer'
               COLUMNS (a.id AS a, b.id AS b)) g, (SELECT count(*) FROM members) m",
            "SELECT g.* FROM GRAPH_TABLE (karate MATCH (a)-[]->(b)
               WHERE (a.id IN (SELECT id FROM members WHERE club = 'Mr. Hi') OR false)
                 AND b.club = 'Officer'
               COLUMNS (a.id AS a, b.id AS b)) g, (SELECT count(*) FROM members) m",
        ),
        // Two paths: a triangle through a bound node, and a path that goes on
        // from where another ended.
        (
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[]->(b)-[]->(c), (a)-[]->(c)
               COLUMNS (b.id AS b, c.id AS c))",
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0 OR false)-[]->(b)-[]->(c), (a)-[]->(c)
               COLUMNS (b.id AS b, c.id AS c))",
        ),
        (
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 33)-[]-(b), (b)-[]->(c)
               COLUMNS (b.id AS b, c.id AS c))",
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 33 OR false)-[]-(b), (b)-[]->(c)
               COLUMNS (b.id AS b, c.id AS c))",
        ),
        // A fixed edge after a quantified one, from the keys it reached.
        (
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 5)-[]->{1,2}(b)-[]->(c)
               COLUMNS (a.id AS a, b.id AS b, c.id AS c))",
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 5 OR false)-[]->{1,2}(b)-[]->(c)
               COLUMNS (a.id AS a, b.id AS b, c.id AS c))",
        ),
        // A graph with no vertex table: its nodes are the endpoints.
        (
            "FROM GRAPH_TABLE (karate_edges_only MATCH (a WHERE a.id IN (0, 1))-[]-(b)
               COLUMNS (a.id AS a, b.id AS b))",
            "FROM GRAPH_TABLE (karate_edges_only MATCH (a WHERE a.id IN (0, 1) OR false)-[]-(b)
               COLUMNS (a.id AS a, b.id AS b))",
        ),
    ] {
        same_as_joins(&ctx, query, joined).await;
    }
    // A node read only for its key is not scanned: the match reads no
    // vertex table but for the index's nodes.
    let plan = physical(
        &ctx,
        "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[]->(b) COLUMNS (b.id))",
    )
    .await;
    assert!(!plan.contains("HashJoinExec"), "{plan}");
    // An edge whose columns the query reads is joined, as before.
    let plan = physical(
        &ctx,
        "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[e]->(b) COLUMNS (e.src, b.id))",
    )
    .await;
    assert!(joins_edges(&plan), "{plan}");
    // So is a pattern with no bound node.
    let plan = physical(
        &ctx,
        "FROM GRAPH_TABLE (karate MATCH (a WHERE a.club = 'Officer')-[]->(b) COLUMNS (b.id))",
    )
    .await;
    assert!(joins_edges(&plan), "{plan}");
}

#[tokio::test]
async fn a_bound_key_that_is_no_node_matches_nothing() {
    let ctx = karate().await;
    for graph in ["karate", "karate_edges_only"] {
        for pattern in [
            "(a WHERE a.id = 99)-[]->(b)",
            "(a WHERE a.id = 99)-[]->{0,2}(b)",
            "ANY SHORTEST (a WHERE a.id = 99)-[]->*(b)",
            "ANY SHORTEST (a WHERE a.id = 0)-[]-*(b WHERE b.id = 99)",
        ] {
            let query = format!(
                "SELECT count(*) AS n FROM GRAPH_TABLE ({graph} MATCH {pattern} COLUMNS (a.id))"
            );
            assert_contains(&ok(&ctx, &query).await, "| 0 |");
        }
    }
    // A bound key that is a node on no edge has its path of no edges.
    let ctx = ursa_engine::new_session();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("v.csv"), "id\n1\n2\n3\n").unwrap();
    std::fs::write(dir.path().join("e.csv"), "s,d\n1,2\n").unwrap();
    ok(
        &ctx,
        &format!(
            "CREATE VIEW v AS SELECT * FROM '{}';
             CREATE VIEW e AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id))
               EDGE TABLES (e SOURCE KEY (s) REFERENCES v (id) DESTINATION KEY (d) REFERENCES v (id));",
            dir.path().join("v.csv").display(),
            dir.path().join("e.csv").display()
        ),
    )
    .await;
    assert_eq!(
        ok(
            &ctx,
            "FROM GRAPH_TABLE (g MATCH p = ANY SHORTEST (a WHERE a.id IN (3, 7))-[]->*(b)
               COLUMNS (a.id AS a, b.id AS b, path_length(p) AS n))"
        )
        .await,
        "\
+---+---+---+
| a | b | n |
+---+---+---+
| 3 | 3 | 0 |
+---+---+---+"
    );
}

/// Self-loops, parallel edges, an edge to a value that is no key, and a key
/// of another type than the endpoints', in files.
#[tokio::test]
async fn expanded_edges_keep_the_joins_multiset() {
    let ctx = ursa_engine::new_session();
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    };
    let people = file("people.csv", "pid,name\n1,Ann\n2,Bo\n3,Cy\n4,Di\n");
    let follows = file(
        "follows.csv",
        "a,b,since\n1,2,2020\n1,2,2021\n2,2,2022\n2,3,2023\n3,1,2024\n3,9,2025\n4,4,2026\n",
    );
    ok(
        &ctx,
        &format!(
            "CREATE VIEW people AS SELECT CAST(pid AS INT) AS pid, name FROM '{people}';
             CREATE VIEW follows AS SELECT * FROM '{follows}';
             CREATE PROPERTY GRAPH social
               VERTEX TABLES (people KEY (pid) LABEL person PROPERTIES (pid AS id, pid, name))
               EDGE TABLES (follows SOURCE KEY (a) REFERENCES people (pid)
                                    DESTINATION KEY (b) REFERENCES people (pid));"
        ),
    )
    .await;
    for (pattern, columns) in [
        (
            "(x WHERE x.id IN (1, 2, 3, 4))-[]->(y)",
            "x.id AS x, y.pid AS y",
        ),
        (
            "(x WHERE x.id IN (1, 2, 3, 4))-[]-(y)",
            "x.pid AS x, y.id AS y",
        ),
        (
            "(x WHERE x.pid IN (2, 4))-[]-(y)-[]-(z)",
            "y.id AS y, z.name AS z",
        ),
        ("(x)-[]->(y WHERE y.id = 2)", "x.name AS x, y.id AS y"),
    ] {
        let query = format!("FROM GRAPH_TABLE (social MATCH {pattern} COLUMNS ({columns}))");
        let joined = query
            .replace("IN (1, 2, 3, 4))", "IN (1, 2, 3, 4) OR false)")
            .replace("IN (2, 4))", "IN (2, 4) OR false)")
            .replace("y.id = 2)", "y.id = 2 OR false)");
        same_as_joins(&ctx, &query, &joined).await;
    }
    // Keys keep their type: the key is an INT, the endpoints BIGINT.
    assert_contains(
        &ok(
            &ctx,
            "SELECT arrow_typeof(y) AS t FROM GRAPH_TABLE (social
               MATCH (x WHERE x.id = 1)-[]->(y) COLUMNS (y.id AS y)) LIMIT 1",
        )
        .await,
        "| Int32 |",
    );
}

#[tokio::test]
async fn a_search_with_a_known_last_node_stops_at_it() {
    let ctx = karate().await;
    let expanded = |analyzed: &str| -> u64 {
        let at = analyzed.find("expanded=").expect("the search's metrics");
        analyzed[at + "expanded=".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap()
    };
    for (query, joined) in [
        (
            "FROM GRAPH_TABLE (karate
               MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
               COLUMNS (vertices(p) AS v, edges(p) AS e))",
            "FROM GRAPH_TABLE (karate
               MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26 OR false)
               COLUMNS (vertices(p) AS v, edges(p) AS e))",
        ),
        (
            "FROM GRAPH_TABLE (karate
               MATCH p = ALL SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id IN (0, 6))
               COLUMNS (vertices(p) AS v))",
            "FROM GRAPH_TABLE (karate
               MATCH p = ALL SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id IN (0, 6) OR false)
               COLUMNS (vertices(p) AS v))",
        ),
        (
            "FROM GRAPH_TABLE (karate_edges_only
               MATCH p = SHORTEST 3 (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 33)
               COLUMNS (vertices(p) AS v))",
            "FROM GRAPH_TABLE (karate_edges_only
               MATCH p = SHORTEST 3 (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 33 OR false)
               COLUMNS (vertices(p) AS v))",
        ),
    ] {
        assert_eq!(
            sorted(ok(&ctx, query).await),
            sorted(ok(&ctx, joined).await)
        );
        let early = ok(&ctx, &format!("EXPLAIN ANALYZE {query}")).await;
        assert_contains(&early, ", targets");
        let all = ok(&ctx, &format!("EXPLAIN ANALYZE {joined}")).await;
        assert!(!all.contains(", targets"), "{all}");
        assert!(
            expanded(&early) < expanded(&all),
            "{query}: {} expanded, {} without targets",
            expanded(&early),
            expanded(&all)
        );
    }
    // Neither end is joined to the members: the path search alone.
    let plan = physical(
        &ctx,
        "FROM GRAPH_TABLE (karate
           MATCH ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) COLUMNS (a.id, b.id))",
    )
    .await;
    assert!(!plan.contains("HashJoinExec"), "{plan}");
    assert_contains(
        &plan,
        "PathSearchExec: graph=karate, search=ANY SHORTEST WALK -[]-{0,}, targets",
    );
}

#[tokio::test]
async fn a_missing_property_names_the_variables_properties() {
    let ctx = karate().await;
    let message = err(
        &ctx,
        "FROM GRAPH_TABLE (karate MATCH (a)-[]->(b) COLUMNS (a.s AS x))",
    )
    .await;
    assert_contains(
        &message,
        "node 'a' has no property 's'; its properties are club, id. To read an edge's columns, name the edge, as -[e]->, and write e.s",
    );
    assert!(!message.contains("__ursa"), "{message}");
    assert_contains(
        &err(
            &ctx,
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[e]->(b) COLUMNS (e.weight))",
        )
        .await,
        "edge 'e' has no property 'weight'; its properties are dst, src",
    );
    assert_contains(
        &err(
            &ctx,
            "FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[e]->(b) COLUMNS (x.id))",
        )
        .await,
        "'x' is not a variable of the pattern; its variables are a, b, e",
    );
}
