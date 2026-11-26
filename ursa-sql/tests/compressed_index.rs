//! `SET ursa.index_layout = 'compressed'` (#173): a graph's index is built
//! in the compressed layout, cached and stored under a key of its own, and
//! every graph function and `GRAPH_TABLE` gives the plain layout's rows.
//!
//! What is compared, on a graph large enough to be relabelled and to have
//! its edge rows spilled (#170, #172), and on two small ones:
//! - exactly: components, `hop`, triangles, Louvain and label propagation
//!   (serial and parallel), closeness, degree, shortest-path lengths and
//!   weighted costs, and every `GRAPH_TABLE` row, paths included, in order
//!   (a path search extends a prefix in edge-row order on either layout, so
//!   it finds the same paths: compared in an order of the query's);
//! - within a relative 1e-9: PageRank and betweenness, whose sums run in
//!   neighbour order, which the compressed layout sorts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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

/// The rows of `sql`, each as its cells joined by `|`.
async fn rows(ctx: &SessionContext, sql: &str) -> Vec<String> {
    let batches = run(ctx, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {}", ursa_engine::user_message(&e)));
    pretty_format_batches(&batches)
        .unwrap()
        .to_string()
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

/// 100,000 skewed edges over about 6,000 nodes, with a weight each, written
/// to a CSV file once (a file table, so its index is cached).
fn edges_file() -> PathBuf {
    static FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    FILE.get_or_init(|| {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let mut csv = String::from("src,dst,w\n");
        for _ in 0..100_000 {
            let a = next(6_000).min(next(6_000));
            let b = next(6_000).min(next(6_000)).min(next(6_000));
            let w = 1 + next(9);
            csv.push_str(&format!("{},{},{w}\n", a * 7 + 3, b * 7 + 3));
        }
        let dir = std::env::temp_dir().join(format!("ursa-compressed-173-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edges.csv");
        std::fs::write(&path, csv).unwrap();
        path
    })
    .clone()
}

/// A session with the graphs `g` (the large one), `pg` (DuckPGQ's
/// students) and `karate`, in `layout`.
async fn session(layout: &str) -> SessionContext {
    let ctx = ursa_engine::new_session();
    let setup = format!(
        "SET ursa.index_layout = '{layout}';
         CREATE VIEW e AS SELECT * FROM '{}';
         CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst));
         CREATE VIEW members AS SELECT * FROM '{}';
         CREATE VIEW ties AS SELECT * FROM '{}';
         CREATE PROPERTY GRAPH karate
           VERTEX TABLES (members KEY (id))
           EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id)
                             DESTINATION KEY (dst) REFERENCES members (id));
         CREATE TABLE student AS SELECT * FROM (VALUES
           (0, 'Daniel'), (1, 'Tavneet'), (2, 'Gabor'), (3, 'Peter'), (4, 'David')) AS t(id, name);
         CREATE TABLE know AS SELECT * FROM (VALUES
           (0, 1, 10), (0, 2, 11), (0, 3, 12), (3, 0, 13), (1, 2, 14), (1, 3, 15), (2, 3, 16), (4, 3, 17))
           AS t(src, dst, createdate);
         CREATE PROPERTY GRAPH pg
           VERTEX TABLES (student PROPERTIES (id, name) LABEL person)
           EDGE TABLES (know SOURCE KEY (src) REFERENCES student (id)
                             DESTINATION KEY (dst) REFERENCES student (id) LABEL knows);",
        edges_file().display(),
        data("karate_nodes.csv").display(),
        data("karate_edges.csv").display()
    );
    rows(&ctx, &setup).await;
    ctx
}

/// Queries whose rows must be the same on both layouts.
const EXACT: &[&str] = &[
    "SELECT * FROM connected_components(g) ORDER BY id",
    "SELECT * FROM connected_components(g, mode => 'strong') ORDER BY id",
    "SELECT id, distance FROM hop(g, seeds => [3, 10], steps => 2, direction => 'both') ORDER BY id, distance",
    "SELECT id, distance FROM hop(g, seeds => [3], steps => 3, direction => 'in') ORDER BY id, distance",
    "SELECT * FROM triangle_count(g) ORDER BY id",
    "SELECT * FROM louvain(g, seed => 1) ORDER BY id",
    "SELECT * FROM louvain(g, seed => 1, parallel => true) ORDER BY id",
    "SELECT * FROM louvain(g, seed => 1, weight => w) ORDER BY id",
    "SELECT * FROM label_propagation(g, seed => 1) ORDER BY id",
    "SELECT * FROM label_propagation(g, seed => 1, parallel => true) ORDER BY id",
    "SELECT * FROM closeness(g) ORDER BY id",
    "SELECT * FROM degree(g, direction => 'both') ORDER BY id",
    "SELECT count(*) FROM shortest_path(g, source => 3503, target => 3)",
    "SELECT count(*) FROM shortest_path(g, source => 3, target => 3503, direction => 'both')",
    "SELECT max(cost) FROM shortest_path(g, source => 3503, target => 3, weight => w)",
    "SELECT * FROM GRAPH_TABLE (g
       MATCH p = ANY SHORTEST (a WHERE a.id = 3503)-[e]->{1,3}(b)
       COLUMNS (element_id(p) AS path, b.id AS b)) ORDER BY b, path",
    "SELECT * FROM GRAPH_TABLE (g
       MATCH p = ALL SHORTEST (a WHERE a.id = 10)-[e]-{1,2}(b WHERE b.id = 24)
       COLUMNS (element_id(p) AS path)) ORDER BY path",
    "SELECT * FROM GRAPH_TABLE (pg
       MATCH p = ANY SHORTEST (a:person)-[k:knows]->{1,3}(b:person)
       WHERE a.id <> b.id
       COLUMNS (path_length(p) AS n, element_id(p) AS path, a.name AS name, b.name AS b_name))
     ORDER BY name, b_name, path",
    "SELECT * FROM GRAPH_TABLE (pg
       MATCH p = (a:person)-[k:knows]->{1,2}(b)
       COLUMNS (a.id AS a, b.id AS b, element_id(p) AS path)) ORDER BY a, b, path",
    "SELECT * FROM GRAPH_TABLE (karate
       MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26)
       COLUMNS (element_id(p) AS path))",
    "SELECT * FROM GRAPH_TABLE (karate
       MATCH p = SHORTEST 3 (a WHERE a.id = 0)-[e]-{1,3}(m)
       COLUMNS (m.id AS b, element_id(p) AS path)) ORDER BY b, path",
    "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]->(b) COLUMNS (a.id AS a, b.id AS b))
     ORDER BY a, b",
    "SELECT * FROM shortest_path(karate, source => 16, target => 26, direction => 'both')",
];

/// Queries of `(id, value)` rows whose values are sums in neighbour order.
const SUMS: &[&str] = &[
    "SELECT id, pagerank FROM pagerank(g) ORDER BY id",
    "SELECT id, pagerank FROM pagerank(g, weight => w) ORDER BY id",
    "SELECT id, betweenness FROM betweenness(g, sample => 0.05, seed => 7) ORDER BY id",
    "SELECT id, betweenness FROM betweenness(karate) ORDER BY id",
];

#[tokio::test]
async fn every_query_gives_the_plain_layouts_rows() {
    let plain = session("plain").await;
    let compressed = session("compressed").await;
    assert_eq!(
        rows(&compressed, "SHOW ursa.index_layout").await,
        vec!["ursa.index_layout|compressed"]
    );
    for sql in EXACT {
        let want = rows(&plain, sql).await;
        assert!(!want.is_empty(), "{sql}");
        assert_eq!(rows(&compressed, sql).await, want, "{sql}");
    }
    for sql in SUMS {
        let (want, got) = (rows(&plain, sql).await, rows(&compressed, sql).await);
        assert_eq!(want.len(), got.len(), "{sql}");
        for (w, g) in want.iter().zip(&got) {
            let (wid, wx) = w.split_once('|').unwrap();
            let (gid, gx) = g.split_once('|').unwrap();
            let (wx, gx): (f64, f64) = (wx.parse().unwrap(), gx.parse().unwrap());
            assert_eq!(wid, gid, "{sql}");
            assert!(
                (wx - gx).abs() <= 1e-9 * wx.abs().max(gx.abs()).max(1e-300),
                "{sql}: {wid}: {gx} against {wx}"
            );
        }
    }
}

#[tokio::test]
async fn the_layout_is_part_of_the_index_key() {
    let ctx = session("plain").await;
    let said = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = {
        let said = Arc::clone(&said);
        Arc::new(move |line: &str| said.lock().unwrap().push(line.to_string()))
    };
    ursa_engine::notices::set_sink(&ctx.state(), Some(sink));
    rows(&ctx, "SET ursa.notices = 'all'").await;
    let taken = || std::mem::take(&mut *said.lock().unwrap());
    const COUNT: &str = "SELECT count(*) FROM connected_components(g)";
    let before = ursa_engine::graph::index_builds(&ctx);
    let count = rows(&ctx, COUNT).await;
    assert!(taken()
        .iter()
        .any(|n| n.starts_with("built the index for g")));
    // Another layout builds another index; going back finds the first.
    rows(&ctx, "SET ursa.index_layout = 'compressed'").await;
    assert_eq!(rows(&ctx, COUNT).await, count);
    let said = taken();
    assert!(
        said.iter()
            .any(|n| n.starts_with("built the compressed index for g")),
        "{said:?}"
    );
    rows(&ctx, "SET ursa.index_layout = 'plain'").await;
    assert_eq!(rows(&ctx, COUNT).await, count);
    rows(&ctx, "SET ursa.index_layout = 'compressed'").await;
    assert_eq!(rows(&ctx, COUNT).await, count);
    assert_eq!(ursa_engine::graph::index_builds(&ctx), before + 2);
    rows(&ctx, "RESET ursa.index_layout").await;
    assert_eq!(
        rows(&ctx, "SHOW ursa.index_layout").await,
        vec!["ursa.index_layout|plain"]
    );
}

#[tokio::test]
async fn an_unknown_layout_is_an_error() {
    let ctx = ursa_engine::new_session();
    let e = match run(&ctx, "SET ursa.index_layout = 'zipped'").await {
        Ok(_) => panic!("an unknown layout was accepted"),
        Err(e) => ursa_engine::user_message(&e),
    };
    assert!(
        e.contains("'zipped' is not an index layout; use 'plain' or 'compressed'"),
        "{e}"
    );
}
