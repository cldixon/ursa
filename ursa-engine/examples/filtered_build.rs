//! How long a graph index takes to build from a filtered view over a large
//! Parquet file (#351).
//!
//! Writes a synthetic edge list (zstd, many row groups) with a column `t`
//! scattered over every row group, so a filter on it is one no file or
//! row-group statistics prune, as `date_followed < 2024` was on Bluesky. Then
//! it times `count(*) FROM degree(g)` over `WHERE t < 5000` (5% of the rows)
//! in fresh sessions, so each one builds the index, and prints the plain
//! parallel SQL scan of the same view for comparison.
//!
//! ```sh
//! cargo run --release -p ursa-engine --example filtered_build -- [ROWS] [DIR]
//! ```
//!
//! `ROWS` defaults to 40 million; `DIR` (default: a temporary directory)
//! keeps the file between runs.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use datafusion::arrow::array::Int64Array;
use datafusion::common::TableReference;
use datafusion::prelude::{SessionConfig, SessionContext};
use ursa_engine::graph::{registry_of, EdgeTable, GraphDef};

const RUNS: usize = 3;

async fn run(ctx: &SessionContext, sql: &str) -> Duration {
    let started = Instant::now();
    ctx.sql(sql).await.unwrap().collect().await.unwrap();
    started.elapsed()
}

/// A fresh session with the view and the graph over it.
async fn session(file: &str) -> SessionContext {
    let ctx = ursa_engine::new_session_with_config(SessionConfig::new());
    run(
        &ctx,
        &format!("CREATE VIEW few AS SELECT s, d FROM '{file}' WHERE t < 5000"),
    )
    .await;
    registry_of(&ctx).put(GraphDef {
        name: "g".into(),
        edges: EdgeTable::new(TableReference::bare("few"), "s", "d"),
        vertices: None,
    });
    ctx
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let rows: u64 = args
        .next()
        .map(|a| a.parse().expect("ROWS is a number"))
        .unwrap_or(40_000_000);
    let temp = tempfile::tempdir().unwrap();
    let dir = args.next().map(PathBuf::from).unwrap_or(temp.path().into());
    let file = dir.join(format!("edges_{rows}.parquet"));
    let file_name = file.display().to_string();

    if !file.exists() {
        let ctx = SessionContext::new();
        let started = Instant::now();
        for sql in [
            "SET datafusion.execution.parquet.compression = 'zstd(3)'".to_string(),
            "SET datafusion.execution.parquet.max_row_group_size = 1048576".to_string(),
            // Ids and `t` scattered by multiplicative hashing: every row
            // group spans all of `t`, and node ids repeat across groups.
            format!(
                "COPY (SELECT (value * 7919) % 5000011 AS s, \
                 (value * 104729 + 13) % 5000011 AS d, \
                 (value * 2654435761) % 100000 AS t \
                 FROM generate_series(0, {}) ORDER BY value) TO '{file_name}'",
                rows - 1
            ),
        ] {
            run(&ctx, &sql).await;
        }
        let size = std::fs::metadata(&file).unwrap().len();
        println!(
            "wrote {rows} rows ({:.0} MiB) in {:.1} s",
            size as f64 / (1 << 20) as f64,
            started.elapsed().as_secs_f64()
        );
    }

    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!("{cores} cores; {rows} rows, filter keeps 5%; median of {RUNS} runs");

    let mut scans = Vec::new();
    let mut builds = Vec::new();
    for _ in 0..RUNS {
        let ctx = session(&file_name).await;
        scans.push(run(&ctx, "SELECT count(*), sum(s), sum(d) FROM few").await);
        let ctx = session(&file_name).await;
        builds.push(run(&ctx, "SELECT count(*) FROM degree(g)").await);
    }
    let ctx = session(&file_name).await;
    let counted = ctx
        .sql("SELECT count(*) FROM few")
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    let edges = counted[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap()
        .value(0);
    println!(
        "parallel SQL scan of the view: {:.2} s",
        median(scans).as_secs_f64()
    );
    println!(
        "index build ({edges} edges):    {:.2} s",
        median(builds).as_secs_f64()
    );
}
