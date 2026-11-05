//! Evaluated weights stored with a graph's index (#260): a weighted query
//! over a stored index in a later session reads no edge rows, and gives the
//! same results.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::common::TableReference;
use datafusion::prelude::SessionContext;
use ursa_engine::database::{self, Database, OpenOptions};
use ursa_engine::graph::{index_builds, index_loads, registry_of, EdgeTable, GraphDef};
use ursa_engine::new_session;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

async fn rows(ctx: &SessionContext, sql: &str) -> String {
    let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
    pretty_format_batches(&batches).unwrap().to_string()
}

/// The size of every index file in the database's index directory.
fn index_files(path: &Path) -> Vec<u64> {
    let dir = path.with_extension("ursa.d").join("indexes");
    let mut sizes: Vec<u64> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".idx"))
        .map(|e| e.metadata().unwrap().len())
        .collect();
    sizes.sort();
    sizes
}

/// A session over the database at `path`, with graph `w` over the Les
/// Misérables co-appearances, weighted by `weight`.
async fn weighted_session(path: &Path, csv: &Path) -> SessionContext {
    let ctx = new_session();
    let db = Database::open(path, &OpenOptions::default()).unwrap();
    database::slot(&ctx.state()).set(Arc::new(db)).unwrap();
    ctx.sql(&format!(
        "CREATE VIEW scenes AS SELECT * FROM '{}'",
        csv.display()
    ))
    .await
    .unwrap();
    registry_of(&ctx).put(GraphDef {
        name: "w".into(),
        edges: EdgeTable {
            table: TableReference::bare("scenes"),
            source: "src".into(),
            destination: "dst".into(),
        },
        vertices: None,
    });
    ctx
}

/// The rows each file scan of `sql` produced, from `EXPLAIN ANALYZE`.
async fn rows_scanned(ctx: &SessionContext, sql: &str) -> u64 {
    let plan = rows(ctx, &format!("EXPLAIN ANALYZE {sql}")).await;
    plan.lines()
        .filter(|l| l.contains("DataSourceExec"))
        .filter_map(|l| {
            let at = l.find("output_rows=")? + "output_rows=".len();
            let digits: String = l[at..].chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().ok()
        })
        .sum()
}

#[tokio::test]
async fn evaluated_weights_are_stored_and_a_later_session_reads_no_edges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("club.ursa");
    let csv = dir.path().join("scenes.csv");
    std::fs::copy(data("lesmis_edges.csv"), &csv).unwrap();
    let weighted = "SELECT id, arrow_cast(pagerank, 'Utf8') AS bits \
                    FROM pagerank(w, weight => weight) ORDER BY id";
    let doubled = "SELECT id, arrow_cast(pagerank, 'Utf8') AS bits \
                   FROM pagerank(w, weight => weight * 2 + 1) ORDER BY id";

    let (expected, expected_doubled) = {
        let ctx = weighted_session(&path, &csv).await;
        let ranks = rows(&ctx, weighted).await;
        assert!(rows_scanned(&ctx, weighted).await == 0, "cached in memory");
        let doubled = rows(&ctx, doubled).await;
        database::flush(&ctx.state());
        (ranks, doubled)
    };
    let stored = index_files(&path);
    assert_eq!(stored.len(), 1);

    // A later session loads the index and its weights, and reads no edges.
    let ctx = weighted_session(&path, &csv).await;
    assert_eq!(rows_scanned(&ctx, weighted).await, 0);
    assert_eq!(rows(&ctx, weighted).await, expected);
    assert_eq!(rows(&ctx, doubled).await, expected_doubled);
    assert_eq!((index_builds(&ctx), index_loads(&ctx)), (0, 1));
    // Weights not stored are read and evaluated from the edges, and stored
    // when the database closes.
    let squared = "SELECT id, arrow_cast(pagerank, 'Utf8') AS bits \
                   FROM pagerank(w, weight => weight * weight) ORDER BY id";
    let ranks = rows(&ctx, squared).await;
    assert!(rows_scanned(&ctx, squared).await == 0, "cached in memory");
    database::flush(&ctx.state());
    let grown = index_files(&path);
    assert!(grown[0] > stored[0], "{stored:?} -> {grown:?}");
    drop(ctx);
    let ctx = weighted_session(&path, &csv).await;
    assert_eq!(rows_scanned(&ctx, squared).await, 0);
    assert_eq!(rows(&ctx, squared).await, ranks);
    drop(ctx);

    // Weights stored with a stale index are never used: a changed edge file
    // changes the index key, so the next session builds and reads afresh.
    let mut text = std::fs::read_to_string(&csv).unwrap();
    text.push_str("Valjean,Napoleon,3\n");
    std::fs::write(&csv, text).unwrap();
    let ctx = weighted_session(&path, &csv).await;
    let changed = rows(&ctx, weighted).await;
    assert_ne!(changed, expected);
    assert_eq!((index_builds(&ctx), index_loads(&ctx)), (1, 0));
    assert!(rows_scanned(&ctx, doubled).await > 0);
}
