//! Variable-length and shortest paths in `GRAPH_TABLE … MATCH` (#297): the
//! quantified edges, selectors, path modes and path functions, on DuckPGQ's
//! own test graph (its `test/sql/path_finding/shortest_path.test`) and on
//! the karate club, checked against `hop` and `shortest_path`.

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

/// The rows of `sql`, each as its cells joined by `|`.
async fn rows(ctx: &SessionContext, sql: &str) -> Vec<String> {
    ok(ctx, sql)
        .await
        .lines()
        .skip(3)
        .filter(|l| !l.starts_with('+'))
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
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

/// DuckPGQ's test graph: five students and who knows whom.
async fn students() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        "CREATE TABLE student AS SELECT * FROM (VALUES
           (0, 'Daniel'), (1, 'Tavneet'), (2, 'Gabor'), (3, 'Peter'), (4, 'David')) AS t(id, name);
         CREATE TABLE know AS SELECT * FROM (VALUES
           (0, 1, 10), (0, 2, 11), (0, 3, 12), (3, 0, 13), (1, 2, 14), (1, 3, 15), (2, 3, 16), (4, 3, 17))
           AS t(src, dst, createdate);
         CREATE TABLE school AS SELECT * FROM (VALUES ('VU', 0), ('UVA', 1)) AS t(name, id);
         CREATE TABLE studyat AS SELECT * FROM (VALUES (0, 0), (1, 0), (2, 1), (3, 1), (4, 1)) AS t(personid, schoolid);
         CREATE PROPERTY GRAPH pg
           VERTEX TABLES (student PROPERTIES (id, name) LABEL person, school LABEL school)
           EDGE TABLES (
             know SOURCE KEY (src) REFERENCES student (id)
                  DESTINATION KEY (dst) REFERENCES student (id) LABEL knows,
             studyat SOURCE KEY (personid) REFERENCES student (id)
                     DESTINATION KEY (schoolid) REFERENCES school (id) LABEL studyat);",
    )
    .await;
    ctx
}

#[tokio::test]
async fn any_shortest_matches_duckpgq() {
    let ctx = students().await;
    // DuckPGQ's expected rows, from shortest_path.test.
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH p = ANY SHORTEST (a:person WHERE a.name = 'Daniel')-[k:knows]->{1,3}(b:person)
               WHERE a.id <> b.id
               COLUMNS (element_id(p), a.name AS name, b.name AS b_name)) study
             ORDER BY name, b_name"
        )
        .await,
        vec![
            "[0, 1, 2]|Daniel|Gabor",
            "[0, 2, 3]|Daniel|Peter",
            "[0, 0, 1]|Daniel|Tavneet",
        ]
    );
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH p = ANY SHORTEST (a:person)-[k:knows]->{1,3}(b:person)
               WHERE a.id <> b.id
               COLUMNS (path_length(p), element_id(p), a.name AS name, b.name AS b_name)) study
             ORDER BY study.name, study.b_name"
        )
        .await,
        vec![
            "1|[0, 1, 2]|Daniel|Gabor",
            "1|[0, 2, 3]|Daniel|Peter",
            "1|[0, 0, 1]|Daniel|Tavneet",
            "2|[4, 7, 3, 3, 0]|David|Daniel",
            "3|[4, 7, 3, 3, 0, 1, 2]|David|Gabor",
            "1|[4, 7, 3]|David|Peter",
            "3|[4, 7, 3, 3, 0, 0, 1]|David|Tavneet",
            "2|[2, 6, 3, 3, 0]|Gabor|Daniel",
            "1|[2, 6, 3]|Gabor|Peter",
            "3|[2, 6, 3, 3, 0, 0, 1]|Gabor|Tavneet",
            "1|[3, 3, 0]|Peter|Daniel",
            "2|[3, 3, 0, 1, 2]|Peter|Gabor",
            "2|[3, 3, 0, 0, 1]|Peter|Tavneet",
            "2|[1, 5, 3, 3, 0]|Tavneet|Daniel",
            "1|[1, 4, 2]|Tavneet|Gabor",
            "1|[1, 5, 3]|Tavneet|Peter",
        ]
    );
    // SQL/PGQ: a shortest path of one edge or more from a node back to
    // itself is a cycle, as Daniel → Peter → Daniel. (DuckPGQ leaves these
    // out; the WHERE above does too, to compare.)
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH p = ANY SHORTEST (a:person WHERE a.id = 0)-[k:knows]->{1,3}(b:person WHERE b.id = 0)
               COLUMNS (vertices(p) AS v, edges(p) AS e))"
        )
        .await,
        vec!["[0, 3, 0]|[2, 3]"]
    );
    // A selector on a fixed edge, between two vertex tables.
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH ANY SHORTEST (a:person)-[s:studyat]->(b:school)
               WHERE a.name = 'Daniel'
               COLUMNS (a.name AS name, b.name AS school))"
        )
        .await,
        vec!["Daniel|VU"]
    );
}

#[tokio::test]
async fn walks_modes_and_selectors() {
    let ctx = students().await;
    // Every walk of one or two edges from Peter, under SQL/PGQ's walk
    // semantics: Peter → Daniel, then on to each of Daniel's three.
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH (a:person)-[k:knows]->{1,2}(b:person)
               WHERE a.name = 'Peter'
               COLUMNS (a.name AS a_name, b.name AS b_name))
             ORDER BY b_name"
        )
        .await,
        vec![
            "Peter|Daniel",
            "Peter|Gabor",
            "Peter|Peter",
            "Peter|Tavneet"
        ]
    );
    // ACYCLIC drops the walk back to Peter.
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH p = ACYCLIC (a:person WHERE a.name = 'Peter')-[k:knows]->+(b:person)
               COLUMNS (vertices(p) AS v))
             ORDER BY v"
        )
        .await,
        vec!["[3, 0]", "[3, 0, 1]", "[3, 0, 1, 2]", "[3, 0, 2]"]
    );
    // SHORTEST 2 and ALL SHORTEST, from Daniel to Peter.
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH p = SHORTEST 2 (a:person WHERE a.id = 0)-[k:knows]->*(b:person WHERE b.id = 3)
               COLUMNS (vertices(p) AS v, edges(p) AS e))"
        )
        .await,
        vec!["[0, 3]|[2]", "[0, 1, 3]|[0, 5]"]
    );
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH p = ALL SHORTEST (a:person WHERE a.id = 0)-[k:knows]->{2,}(b:person WHERE b.id = 3)
               COLUMNS (vertices(p) AS v))
             ORDER BY v"
        )
        .await,
        vec!["[0, 1, 3]", "[0, 2, 3]"]
    );
    // Undirected, from David (as DuckPGQ's undirected_paths.test, without 2-4).
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH o = ANY SHORTEST (a:person WHERE a.id = 4)-[e:knows]- *(b:person)
               COLUMNS (a.id AS a_id, b.id AS b_id, path_length(o)))
             ORDER BY a_id, b_id"
        )
        .await,
        vec!["4|0|2", "4|1|2", "4|2|2", "4|3|1", "4|4|0"]
    );
    // Against the edges.
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH (a:person WHERE a.id = 3)<-[e:knows]-{1,1}(b:person)
               COLUMNS (b.id AS b))
             ORDER BY b"
        )
        .await,
        vec!["0", "1", "2", "4"]
    );
    // A quantified edge between fixed ones.
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM GRAPH_TABLE (pg
               MATCH (s:school)<-[:studyat]-(a:person)-[:knows]->{2,3}(b:person)-[:studyat]->(t:school)
               WHERE a.id = 4
               COLUMNS (b.name AS b, t.name AS school))
             ORDER BY b"
        )
        .await,
        vec!["Daniel|VU", "Gabor|UVA", "Peter|UVA", "Tavneet|VU"]
    );
}

#[tokio::test]
async fn paths_agree_with_hop_and_shortest_path() {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        &format!(
            "CREATE VIEW members AS SELECT * FROM '{}';
             CREATE VIEW ties AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH karate
               VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                 DESTINATION KEY (dst) REFERENCES members (id));",
            data("karate_nodes.csv").display(),
            data("karate_edges.csv").display()
        ),
    )
    .await;
    // Reached in one or two steps either way = hop(n => 2)'s nodes.
    let reached = rows(
        &ctx,
        "SELECT b FROM GRAPH_TABLE (karate
           MATCH ANY SHORTEST (a WHERE a.id = 0)-[e]-{1,2}(m)
           WHERE m.id <> 0
           COLUMNS (m.id AS b))
         ORDER BY b",
    )
    .await;
    let hop = rows(
        &ctx,
        "SELECT DISTINCT id FROM hop(karate, seeds => [0], n => 2, direction => 'both')
         WHERE id <> 0 ORDER BY id",
    )
    .await;
    assert!(reached.len() > 10);
    assert_eq!(reached, hop);
    // Shortest path lengths match shortest_path's.
    let length = rows(
        &ctx,
        "SELECT * FROM GRAPH_TABLE (karate
           MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
           COLUMNS (path_length(p) AS n))",
    )
    .await;
    let edges = rows(
        &ctx,
        "SELECT count(*) FROM shortest_path(karate, source => 16, target => 26, direction => 'both')",
    )
    .await;
    assert_eq!(length, edges);
    // hop's distance is the shortest path's length (#328).
    let lengths = rows(
        &ctx,
        "SELECT b, n FROM GRAPH_TABLE (karate
           MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]-{1,3}(m)
           WHERE m.id <> 0
           COLUMNS (m.id AS b, path_length(p) AS n))
         ORDER BY b",
    )
    .await;
    let distances = rows(
        &ctx,
        "SELECT id, distance FROM hop(karate, seeds => [0], n => 3, direction => 'both')
         ORDER BY id",
    )
    .await;
    assert!(lengths.len() > 20);
    assert_eq!(lengths, distances);
    // The search reuses the graph's index, as the traversals do.
    let explain = ok(
        &ctx,
        "EXPLAIN FROM GRAPH_TABLE (karate
           MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]->*(b) COLUMNS (b.id))",
    )
    .await;
    assert_contains(
        &explain,
        "PathSearchExec: graph=karate, search=ANY SHORTEST WALK -[]->{0,}",
    );
}

#[tokio::test]
async fn refusals() {
    let ctx = students().await;
    for (sql, expected) in [
        (
            "SELECT * FROM GRAPH_TABLE (pg MATCH (a:person)-[k:knows]->*(b:person) COLUMNS (a.id))",
            "has no upper bound, so its walks are endless",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (pg MATCH p = (a:person)-[k:knows]->(b)-[:knows]->(c) COLUMNS (a.id))",
            "must be one edge between two nodes for now",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (pg MATCH (a:person)-[k:knows WHERE k.createdate > 1]->{1,2}(b) COLUMNS (a.id))",
            "a condition on it is not supported yet",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (pg MATCH (a:person)-[s:studyat]->{1,2}(b) COLUMNS (a.id))",
            "joins two vertex tables (student and school)",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (pg MATCH (a:person)-[k:knows]->{1,2}(b) COLUMNS (path_length(a)))",
            "path_length() takes a path variable",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (pg MATCH a = ANY SHORTEST (a:person)-[k:knows]->*(b) COLUMNS (b.id))",
            "'a' names a path and another variable",
        ),
        (
            "SELECT * FROM GRAPH_TABLE (pg MATCH p = (a:person) COLUMNS (a.id))",
            "a path variable needs a path with an edge",
        ),
    ] {
        assert_contains(&err(&ctx, sql).await, expected);
    }
}

/// A complete graph on 50 nodes: 2,450 edges, every pair both ways.
async fn complete() -> SessionContext {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        "CREATE VIEW e AS SELECT a.x AS s, b.x AS d
           FROM generate_series(0, 49) a(x), generate_series(0, 49) b(x) WHERE a.x <> b.x;
         CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
    )
    .await;
    ctx
}

/// `ursa.max_paths` limits a path search, names itself in the error, and
/// each error names the remedy that fits the query (#384, N50).
#[tokio::test]
async fn max_paths_limits_a_search_with_the_right_remedy() {
    let ctx = complete().await;
    assert_eq!(
        rows(&ctx, "SHOW ursa.max_paths").await,
        ["ursa.max_paths|10000000"]
    );
    const WALKS: &str =
        "SELECT count(*) FROM GRAPH_TABLE (g MATCH (a)-[e]->{1,2}(b) COLUMNS (b.id AS b))";
    // 2,450 paths of one edge, and 49 more of two from each.
    assert_eq!(rows(&ctx, WALKS).await, ["122500"]);
    ok(&ctx, "SET ursa.max_paths = 1000").await;
    let message = err(&ctx, WALKS).await;
    assert_contains(
        &message,
        "the path search reached 1000 paths, the limit ursa.max_paths sets",
    );
    assert_contains(&message, "keep fewer paths with ANY SHORTEST");
    assert_contains(
        &message,
        "SET ursa.max_paths to a higher limit (0 for none)",
    );

    // ANY SHORTEST from every node keeps 2,500 paths (one back to the start
    // too, in two edges), over the limit: the
    // remedy is hop, not the selector the query already uses.
    const SHORTEST: &str = "SELECT count(*) FROM GRAPH_TABLE (g
        MATCH p = ANY SHORTEST (a)-[e]->{1,2}(b) COLUMNS (b.id AS b))";
    let message = err(&ctx, SHORTEST).await;
    assert_contains(
        &message,
        "ANY SHORTEST keeps one path per pair of nodes already",
    );
    assert_contains(&message, "hop(");
    assert!(!message.contains("keep fewer paths"), "{message}");
    // ALL SHORTEST holds every prefix: hop, or a lower bound.
    let message = err(
        &ctx,
        "SELECT count(*) FROM GRAPH_TABLE (g
           MATCH p = ALL SHORTEST (a)-[e]->{1,2}(b) COLUMNS (b.id AS b))",
    )
    .await;
    assert_contains(&message, "lower upper bound");
    assert_contains(&message, "hop(");
    assert!(!message.contains("keep fewer paths"), "{message}");

    // ANY SHORTEST from one node keeps one path per node at most, and the
    // limit never refuses those: a limit of 10 lets its 50 through.
    ok(&ctx, "SET ursa.max_paths = 10").await;
    assert_eq!(
        rows(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (g
               MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]->{1,2}(b) COLUMNS (b.id AS b))"
        )
        .await,
        ["50"]
    );

    // 0 is no limit.
    ok(&ctx, "SET ursa.max_paths = 0").await;
    assert_eq!(rows(&ctx, WALKS).await, ["122500"]);
    assert_eq!(rows(&ctx, SHORTEST).await, ["2500"]);
    ok(&ctx, "RESET ursa.max_paths").await;
    assert_eq!(
        rows(&ctx, "SHOW ursa.max_paths").await,
        ["ursa.max_paths|10000000"]
    );
}

/// N50's repro: ANY SHORTEST from one node reaches 12,004,000 nodes within
/// two edges, more than the default limit of paths, and keeps one path to
/// each, as hop counts them (#384).
#[tokio::test(flavor = "multi_thread")]
async fn any_shortest_counts_destinations_not_paths() {
    let ctx = ursa_engine::new_session();
    ok(
        &ctx,
        "CREATE VIEW e AS SELECT 0 AS s, x AS d FROM generate_series(1, 4000) t(x)
           UNION ALL SELECT x AS s, 1000000 + x * 4000 + y AS d
           FROM generate_series(1, 4000) a(x), generate_series(1, 3000) b(y);
         CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (s) DESTINATION KEY (d))",
    )
    .await;
    assert_eq!(
        rows(
            &ctx,
            "SELECT count(*) FROM GRAPH_TABLE (g
               MATCH p = ANY SHORTEST (a WHERE a.id = 0)-[e]->{1,2}(b) COLUMNS (b.id AS b))"
        )
        .await,
        ["12004000"]
    );
    assert_eq!(
        rows(
            &ctx,
            "SELECT distance, count(*) FROM hop(g, seeds => [0], n => 2) GROUP BY 1 ORDER BY 1"
        )
        .await,
        ["1|4000", "2|12000000"]
    );
}
