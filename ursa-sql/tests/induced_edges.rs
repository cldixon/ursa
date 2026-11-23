//! `induced_edges(g, nodes => …)`: the graph's edges among a set of nodes,
//! read from the index (#432). Held to the scan of the edge table it
//! replaces.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datafusion::arrow::array::{Int64Array, RecordBatch};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::SessionContext;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("python/ursa/datasets/data")
        .join(file)
}

/// Run every statement in `sql`; the last one's schema and rows, as CSV
/// lines.
async fn run(
    ctx: &SessionContext,
    sql: &str,
) -> datafusion::error::Result<(Arc<Schema>, Vec<String>)> {
    let mut last = (Arc::new(Schema::empty()), Vec::new());
    for statement in ursa_sql::parse_sql_for(ctx, sql)? {
        let stream = ursa_sql::execute(ctx, statement)
            .await?
            .execute_stream()
            .await?;
        let schema = stream.schema();
        let batches = datafusion::physical_plan::common::collect(stream).await?;
        let options = FormatOptions::default();
        let mut lines = Vec::new();
        for batch in &batches {
            let formatters: Vec<_> = batch
                .columns()
                .iter()
                .map(|c| ArrayFormatter::try_new(c.as_ref(), &options).unwrap())
                .collect();
            for row in 0..batch.num_rows() {
                let cells: Vec<_> = formatters
                    .iter()
                    .map(|f| f.value(row).to_string())
                    .collect();
                lines.push(cells.join(","));
            }
        }
        last = (schema, lines);
    }
    Ok(last)
}

async fn rows(ctx: &SessionContext, sql: &str) -> Vec<String> {
    run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)))
        .1
}

async fn err(ctx: &SessionContext, sql: &str) -> String {
    match run(ctx, sql).await {
        Ok((_, rows)) => panic!("{sql} should fail; got {rows:?}"),
        Err(e) => ursa_engine::user_message(&e),
    }
}

/// A seeded R-MAT edge list (a = 0.57, b = c = 0.19): `edges` edges over
/// `2^scale` nodes, with the hubs, parallel edges and self-loops R-MAT makes.
fn rmat(scale: u32, edges: usize, seed: u64) -> (Vec<i64>, Vec<i64>) {
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let (mut src, mut dst) = (Vec::with_capacity(edges), Vec::with_capacity(edges));
    for _ in 0..edges {
        let (mut u, mut v) = (0i64, 0i64);
        for _ in 0..scale {
            let r = next();
            let (right, down) = match r {
                r if r < 0.57 => (0, 0),
                r if r < 0.76 => (1, 0),
                r if r < 0.95 => (0, 1),
                _ => (1, 1),
            };
            u = (u << 1) | down;
            v = (v << 1) | right;
        }
        src.push(u);
        dst.push(v);
    }
    (src, dst)
}

const RMAT_SCALE: u32 = 12;
const RMAT_EDGES: usize = 30_000;

/// `karate` (a vertex table, integer ids), `lesmis` (string ids) and `rmat`
/// (a table in memory, so an edge's row is its position).
async fn session() -> SessionContext {
    let ctx = ursa_engine::new_session();
    let (src, dst) = rmat(RMAT_SCALE, RMAT_EDGES, 7);
    let schema = Arc::new(Schema::new(vec![
        Field::new("src", DataType::Int64, false),
        Field::new("dst", DataType::Int64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(src)),
            Arc::new(Int64Array::from(dst)),
        ],
    )
    .unwrap();
    ctx.register_batch("rmat_edges", batch).unwrap();
    rows(
        &ctx,
        &format!(
            "CREATE VIEW members AS SELECT * FROM '{}';
             CREATE VIEW ties AS SELECT * FROM '{}';
             CREATE VIEW lesmis_edges AS SELECT * FROM '{}';
             CREATE PROPERTY GRAPH karate
               VERTEX TABLES (members KEY (id))
               EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                                 DESTINATION KEY (dst) REFERENCES members (id));
             CREATE PROPERTY GRAPH lesmis
               EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));
             CREATE PROPERTY GRAPH rmat
               EDGE TABLES (rmat_edges SOURCE KEY (src) DESTINATION KEY (dst));",
            data("karate_nodes.csv").display(),
            data("karate_edges.csv").display(),
            data("lesmis_edges.csv").display(),
        ),
    )
    .await;
    ctx
}

/// The acceptance: on each graph, for a list, a subquery and an empty set,
/// the rows are the edge table's whose two ends are both in the set.
#[tokio::test]
async fn equals_the_edge_table_filtered_to_the_nodes() {
    let ctx = session().await;
    // (graph, edge table, node sets as a list and as the same query)
    let cases = [
        (
            "karate",
            "ties",
            vec![
                (
                    "[0, 1, 2, 3, 7, 13, 33]",
                    "SELECT * FROM (VALUES (0), (1), (2), (3), (7), (13), (33))",
                ),
                ("[32, 33]", "SELECT * FROM (VALUES (32), (33))"),
                (
                    "(SELECT id FROM members WHERE club = 'Officer')",
                    "SELECT id FROM members WHERE club = 'Officer'",
                ),
                ("(SELECT id FROM members)", "SELECT id FROM members"),
            ],
        ),
        (
            "lesmis",
            "lesmis_edges",
            vec![
                (
                    "['Valjean', 'Javert', 'Cosette', 'Marius', 'Fantine', 'Nobody']",
                    "SELECT * FROM (VALUES ('Valjean'), ('Javert'), ('Cosette'), ('Marius'), ('Fantine'))",
                ),
                (
                    "(SELECT id FROM degree(lesmis) WHERE degree >= 10)",
                    "SELECT id FROM degree(lesmis) WHERE degree >= 10",
                ),
            ],
        ),
        (
            "rmat",
            "rmat_edges",
            vec![
                (
                    "[0, 1, 2, 3, 4, 5, 6, 7, 8, 16, 32, 64]",
                    "SELECT * FROM generate_series(0, 8) UNION ALL \
                     SELECT * FROM (VALUES (16), (32), (64))",
                ),
                (
                    "(SELECT id FROM degree(rmat) ORDER BY degree DESC, id LIMIT 300)",
                    "SELECT id FROM degree(rmat) ORDER BY degree DESC, id LIMIT 300",
                ),
                (
                    "(SELECT id FROM hop(rmat, seeds => [1], steps => 2, direction => 'both') \
                     UNION SELECT 1)",
                    "SELECT id FROM hop(rmat, seeds => [1], steps => 2, direction => 'both') \
                     UNION SELECT 1",
                ),
            ],
        ),
    ];
    for (graph, table, sets) in cases {
        for (nodes, query) in sets {
            let got = rows(
                &ctx,
                &format!(
                    "SELECT src, dst FROM induced_edges({graph}, nodes => {nodes}) \
                     ORDER BY src, dst"
                ),
            )
            .await;
            let expected = rows(
                &ctx,
                &format!(
                    "SELECT src, dst FROM {table} WHERE src IN ({query}) AND dst IN ({query}) \
                     ORDER BY src, dst"
                ),
            )
            .await;
            assert_eq!(got, expected, "{graph}: {nodes}");
            assert!(!got.is_empty(), "{graph}: {nodes}");
        }
        // An empty set, as a list and as a query: no rows.
        for nodes in ["[]", &format!("(SELECT src FROM {table} WHERE false)")] {
            let sql = format!("SELECT * FROM induced_edges({graph}, nodes => {nodes})");
            assert_eq!(rows(&ctx, &sql).await, Vec::<String>::new(), "{sql}");
        }
    }
}

/// `edge` is the edge's row in the edge table, the rows come in that order,
/// and parallel edges and self-loops are rows of their own.
#[tokio::test]
async fn rows_are_the_edge_tables_rows_in_order() {
    let ctx = session().await;
    let (src, dst) = rmat(RMAT_SCALE, RMAT_EDGES, 7);
    let in_set = |v: i64| v % 3 == 0 && v < 600;
    let expected: Vec<String> = src
        .iter()
        .zip(&dst)
        .enumerate()
        .filter(|(_, (u, v))| in_set(**u) && in_set(**v))
        .map(|(row, (u, v))| format!("{u},{v},{row}"))
        .collect();
    let sql = "SELECT * FROM induced_edges(rmat, nodes => \
               (SELECT * FROM generate_series(0, 599, 3)))";
    let got = rows(&ctx, sql).await;
    assert_eq!(got, expected);
    // R-MAT repeats edges and loops: both are in the answer.
    let pairs: std::collections::HashSet<&str> = got
        .iter()
        .map(|line| line.rsplit_once(',').unwrap().0)
        .collect();
    assert!(pairs.len() < got.len(), "no parallel edges in the case");
    assert!(got.iter().any(|line| {
        let mut cells = line.split(',');
        cells.next() == cells.next()
    }));
    // The same rows however the set is listed: backwards, with repeats, a
    // NULL and ids that are no node.
    let listed = "SELECT * FROM induced_edges(rmat, nodes => \
                  (SELECT * FROM generate_series(599, 0, -1) AS t(v) WHERE v % 3 = 0 \
                   UNION ALL VALUES (0), (3), (NULL), (-1), (99999999)))";
    assert_eq!(rows(&ctx, listed).await, expected);
    // And on every run.
    assert_eq!(rows(&ctx, sql).await, expected);
}

/// `edge` numbers an edge as `edges(p)` does in `GRAPH_TABLE`.
#[tokio::test]
async fn edge_is_the_row_edges_of_a_path_names() {
    let ctx = session().await;
    let matched = rows(
        &ctx,
        "SELECT a, b, e[1] FROM GRAPH_TABLE (karate MATCH p = (a)-[x]->{1,1}(b) \
         COLUMNS (a.id AS a, b.id AS b, edges(p) AS e)) \
         WHERE a IN (0, 1, 2, 3, 7, 13) AND b IN (0, 1, 2, 3, 7, 13) ORDER BY 3",
    )
    .await;
    let induced = rows(
        &ctx,
        "SELECT * FROM induced_edges(karate, nodes => [0, 1, 2, 3, 7, 13])",
    )
    .await;
    assert_eq!(induced, matched);
    assert_eq!(induced.len(), 14);
    assert_eq!(induced[0], "0,1,0");
}

/// The direction is the one the graph stores: an edge is not also its
/// reverse. One id is a set of one; a node with no loop has no edge.
#[tokio::test]
async fn the_direction_is_the_stored_one() {
    let ctx = session().await;
    assert_eq!(
        rows(
            &ctx,
            "SELECT src, dst FROM induced_edges(karate, nodes => [33, 32, 0])"
        )
        .await,
        ["32,33"]
    );
    assert_eq!(
        rows(&ctx, "SELECT * FROM induced_edges(karate, nodes => 0)").await,
        Vec::<String>::new()
    );
    assert_eq!(
        rows(
            &ctx,
            "SELECT * FROM induced_edges(karate, nodes => [99, 100])"
        )
        .await,
        Vec::<String>::new()
    );
}

/// A graph whose vertex table leaves some endpoints out drops their edges
/// (#311): `edge` is still the row in the edge table, not a count of the
/// edges kept.
#[tokio::test]
async fn edge_is_the_row_when_the_index_dropped_edges() {
    let ctx = session().await;
    rows(
        &ctx,
        "CREATE VIEW some AS SELECT * FROM generate_series(0, 4095) AS t(id) WHERE id % 2 = 0;
         CREATE PROPERTY GRAPH evens
           VERTEX TABLES (some KEY (id))
           EDGE TABLES (rmat_edges SOURCE KEY (src) REFERENCES some (id)
                                   DESTINATION KEY (dst) REFERENCES some (id));",
    )
    .await;
    let (src, dst) = rmat(RMAT_SCALE, RMAT_EDGES, 7);
    let in_set = |v: i64| v % 2 == 0 && v < 200;
    let expected: Vec<String> = src
        .iter()
        .zip(&dst)
        .enumerate()
        .filter(|(_, (u, v))| in_set(**u) && in_set(**v))
        .map(|(row, (u, v))| format!("{u},{v},{row}"))
        .collect();
    assert!(expected.len() > 100);
    // Odd ids are in the list but are not nodes of the graph.
    let got = rows(
        &ctx,
        "SELECT * FROM induced_edges(evens, nodes => (SELECT * FROM generate_series(0, 199)))",
    )
    .await;
    assert_eq!(got, expected);
}

/// The columns say what they are (#431): the ends are node keys of the
/// vertex table, `edge` a row of the edge table.
#[tokio::test]
async fn the_columns_say_what_they_are() {
    let ctx = session().await;
    let describe = |schema: &Schema| -> Vec<String> {
        schema
            .fields()
            .iter()
            .map(|f| {
                let get = |key: &str| f.metadata().get(key).map_or("-", String::as_str);
                format!(
                    "{} {} {} {} {}",
                    f.name(),
                    f.data_type(),
                    get("ursa.role"),
                    get("ursa.graph"),
                    get("ursa.label")
                )
            })
            .collect()
    };
    let (schema, _) = run(&ctx, "SELECT * FROM induced_edges(karate, nodes => [0, 1])")
        .await
        .unwrap();
    assert_eq!(
        describe(&schema),
        [
            "src Int64 src karate members",
            "dst Int64 dst karate members",
            "edge Int64 edge karate ties"
        ]
    );
    let (schema, _) = run(
        &ctx,
        "SELECT * FROM induced_edges(lesmis, nodes => ['Valjean'])",
    )
    .await
    .unwrap();
    assert_eq!(
        describe(&schema),
        [
            "src Utf8 src lesmis -",
            "dst Utf8 dst lesmis -",
            "edge Int64 edge lesmis lesmis_edges"
        ]
    );
}

/// It composes: the nodes from another function, the edges joined back to
/// the edge table's other columns by row.
#[tokio::test]
async fn composes_with_other_functions() {
    let ctx = session().await;
    let top = rows(
        &ctx,
        "SELECT count(*) FROM induced_edges(karate, nodes => \
         (SELECT id FROM pagerank(karate, direction => 'both') ORDER BY pagerank DESC LIMIT 10))",
    )
    .await;
    let expected = rows(
        &ctx,
        "WITH top AS (SELECT id FROM pagerank(karate, direction => 'both') \
                      ORDER BY pagerank DESC LIMIT 10) \
         SELECT count(*) FROM ties WHERE src IN (SELECT id FROM top) \
         AND dst IN (SELECT id FROM top)",
    )
    .await;
    assert_eq!(top, expected);
    // A hop neighbourhood and its edges: what the Graph view draws.
    let ring = rows(
        &ctx,
        "WITH near AS (SELECT id FROM hop(lesmis, seeds => ['Gavroche'], direction => 'both') \
                       UNION SELECT 'Gavroche') \
         SELECT count(*), count(DISTINCT edge) FROM induced_edges(lesmis, nodes => \
         (SELECT id FROM near))",
    )
    .await;
    let scanned = rows(
        &ctx,
        "WITH near AS (SELECT id FROM hop(lesmis, seeds => ['Gavroche'], direction => 'both') \
                       UNION SELECT 'Gavroche') \
         SELECT count(*) AS a, count(*) AS b FROM lesmis_edges WHERE src IN (SELECT id FROM near) \
         AND dst IN (SELECT id FROM near)",
    )
    .await;
    assert_eq!(ring, scanned);
    let plan = rows(
        &ctx,
        "EXPLAIN SELECT * FROM induced_edges(karate, nodes => [0, 1])",
    )
    .await
    .join("\n");
    assert!(
        plan.contains("GraphTraversalExec: graph=karate, call=induced_edges(nodes => [0, 1])"),
        "{plan}"
    );
}

#[tokio::test]
async fn explains_bad_calls() {
    let ctx = session().await;
    assert_eq!(
        err(&ctx, "SELECT * FROM induced_edges(karate)").await,
        "induced_edges() needs 'nodes': induced_edges(<graph>, nodes => [<id>, ...])"
    );
    assert_eq!(
        err(&ctx, "SELECT * FROM induced_edges(karate, seeds => [0])").await,
        "induced_edges() has no parameter 'seeds'. Its parameters are: nodes"
    );
    assert_eq!(
        err(&ctx, "SELECT * FROM induced_edges(karate, nodes => ['a'])").await,
        "induced_edges(nodes =>) must be node ids of graph 'karate', which are integers; got 'a'"
    );
    assert_eq!(
        err(&ctx, "SELECT * FROM induced_edges(karate, nodes => [1.5])").await,
        "induced_edges(nodes =>) must be node ids of graph 'karate', which are integers; got 1.5"
    );
    let two = err(
        &ctx,
        "SELECT * FROM induced_edges(karate, nodes => (SELECT src, dst FROM ties))",
    )
    .await;
    assert!(two.contains("one column"), "{two}");
}

/// A long list of literal ids is planned as a column, not as an expression
/// per id (the Graph view sends thousands): the same rows as the query form,
/// for integers, negative and quoted ones, and strings, and the same errors.
#[tokio::test]
async fn a_long_list_is_the_same_set() {
    let ctx = session().await;
    let ids: Vec<i64> = (0..5000).map(|i| (i * 7919 + 13) % 4096).collect();
    let list = |cell: &dyn Fn(i64) -> String| {
        let cells: Vec<String> = ids.iter().map(|&i| cell(i)).collect();
        format!("[{}]", cells.join(", "))
    };
    let from_query = rows(
        &ctx,
        "SELECT * FROM induced_edges(rmat, nodes => \
         (SELECT (i * 7919 + 13) % 4096 FROM generate_series(0, 4999) AS t(i)))",
    )
    .await;
    assert!(from_query.len() > 10_000, "{}", from_query.len());
    for (what, nodes) in [
        ("integers", list(&|i| i.to_string())),
        ("quoted integers", list(&|i| format!("'{i}'"))),
        // Ids that are no node, and a negative one, among them.
        (
            "with strangers",
            format!("[-7, 99999999, {}", &list(&|i| i.to_string())[1..]),
        ),
    ] {
        let sql = format!("SELECT * FROM induced_edges(rmat, nodes => {nodes})");
        assert_eq!(rows(&ctx, &sql).await, from_query, "{what}");
    }
    // `hop`'s seeds are planned the same way.
    let seeds = list(&|i| i.to_string());
    assert_eq!(
        rows(
            &ctx,
            &format!("SELECT count(*) FROM hop(rmat, seeds => {seeds})")
        )
        .await,
        rows(
            &ctx,
            "SELECT count(*) FROM hop(rmat, seeds => \
             (SELECT (i * 7919 + 13) % 4096 FROM generate_series(0, 4999) AS t(i)))"
        )
        .await
    );
    // String ids, and integers read as their strings.
    let names = rows(&ctx, "SELECT id FROM degree(lesmis) ORDER BY id").await;
    assert!(names.len() >= 65, "{}", names.len());
    let quoted: Vec<String> = names
        .iter()
        .map(|n| format!("'{}'", n.replace('\'', "''")))
        .collect();
    let sql = format!(
        "SELECT * FROM induced_edges(lesmis, nodes => [{}, 7, -1])",
        quoted.join(", ")
    );
    assert_eq!(
        rows(&ctx, &sql).await,
        rows(
            &ctx,
            "SELECT * FROM induced_edges(lesmis, nodes => (SELECT id FROM degree(lesmis)))"
        )
        .await
    );
    // One bad value in a long list is named as in a short one.
    let hundred = (0..100)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    for (bad, got) in [
        ("'a'", "'a'"),
        ("1.5", "1.5"),
        ("true", "true"),
        ("' 1'", "' 1'"),
    ] {
        assert_eq!(
            err(
                &ctx,
                &format!("SELECT * FROM induced_edges(rmat, nodes => [{hundred}, {bad}])")
            )
            .await,
            format!(
                "induced_edges(nodes =>) must be node ids of graph 'rmat', which are integers; \
                 got {got}"
            )
        );
    }
    // NULL and an expression still work in a long list: no node, and its value.
    assert_eq!(
        rows(
            &ctx,
            &format!(
                "SELECT count(*) FROM induced_edges(rmat, nodes => [{hundred}, NULL, 50 + 50])"
            )
        )
        .await,
        rows(
            &ctx,
            "SELECT count(*) FROM induced_edges(rmat, nodes => \
             (SELECT * FROM generate_series(0, 100)))"
        )
        .await
    );
    let plan = rows(
        &ctx,
        &format!("EXPLAIN SELECT * FROM induced_edges(rmat, nodes => [{hundred}])"),
    )
    .await
    .join("\n");
    assert!(plan.contains("TableScan: node_ids"), "{plan}");
}
