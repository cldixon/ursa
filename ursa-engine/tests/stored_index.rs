//! Stored graph indexes across sessions: the in-adjacency is stored once it
//! is built, and a later session reads it only when an algorithm needs it
//! (#259).
//!
//! One test in this binary, as it counts the process-wide reads of stored
//! in-adjacencies.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::common::TableReference;
use datafusion::prelude::SessionContext;
use ursa_engine::database::index_file::incoming_reads;
use ursa_engine::database::{self, Database, OpenOptions};
use ursa_engine::graph::{index_builds, index_loads, registry_of, EdgeTable, GraphDef};
use ursa_engine::new_session;

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

/// A session over the database at `path`, with graph `g` over the karate
/// club's edges.
async fn session(path: &Path) -> SessionContext {
    let ctx = new_session();
    let db = Database::open(path, &OpenOptions::default()).unwrap();
    database::slot(&ctx.state()).set(Arc::new(db)).unwrap();
    ctx.sql(&format!(
        "CREATE VIEW ties AS SELECT * FROM '{}'",
        data("karate_edges.csv").display()
    ))
    .await
    .unwrap();
    registry_of(&ctx).put(GraphDef {
        name: "g".into(),
        edges: EdgeTable {
            table: TableReference::bare("ties"),
            source: "src".into(),
            destination: "dst".into(),
        },
        vertices: None,
    });
    ctx
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

#[tokio::test]
async fn the_in_adjacency_is_stored_once_built_and_read_only_when_needed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("club.ursa");
    let pagerank = "SELECT id, arrow_cast(pagerank, 'Utf8') AS bits FROM pagerank(g) ORDER BY id";
    let out_degree = "SELECT * FROM degree(g, direction => 'out') ORDER BY id";

    // The first session builds the index (stored in the background, before
    // pagerank builds the in-adjacency), then appends the in-adjacency when
    // it closes.
    let (expected, expected_degree, before) = {
        let ctx = session(&path).await;
        let degree = rows(&ctx, out_degree).await;
        database::flush(&ctx.state());
        let before = index_files(&path);
        assert_eq!(before.len(), 1);
        let ranks = rows(&ctx, pagerank).await;
        assert_eq!(index_builds(&ctx), 1);
        database::flush(&ctx.state());
        (ranks, degree, before)
    };
    let after = index_files(&path);
    assert_eq!(after.len(), 1);
    assert!(after[0] > before[0], "{before:?} -> {after:?}");

    // A second session loads the index; a query that needs only the
    // out-adjacency does not read the in-adjacency.
    let reads = incoming_reads();
    let ctx = session(&path).await;
    assert_eq!(rows(&ctx, out_degree).await, expected_degree);
    assert_eq!((index_builds(&ctx), index_loads(&ctx)), (0, 1));
    assert_eq!(incoming_reads(), reads);
    // PageRank reads it rather than building it, with the same results.
    assert_eq!(rows(&ctx, pagerank).await, expected);
    assert_eq!(incoming_reads(), reads + 1);
    // Closing does not write it again.
    database::flush(&ctx.state());
    assert_eq!(index_files(&path), after);
    drop(ctx);

    // A third session that builds it before reading it (no stored chunk)
    // gives the same results too: truncate the file to its main part.
    let file = std::fs::read_dir(path.with_extension("ursa.d").join("indexes"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".idx"))
        .unwrap()
        .path();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(before[0])
        .unwrap();
    let ctx = session(&path).await;
    assert_eq!(rows(&ctx, pagerank).await, expected);
    assert_eq!(index_loads(&ctx), 1);
    assert_eq!(incoming_reads(), reads + 1);
    database::flush(&ctx.state());
    assert_eq!(index_files(&path), after);
}
