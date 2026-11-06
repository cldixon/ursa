//! Deterministic results when a graph's edges, or a traversal's seeds, come
//! from a plan that does not preserve row order (#263).
//!
//! Over a join, an aggregate or a union, the engine sorts the edges by
//! `(source, destination)` (and a traversal's seeds by value) before building
//! the index, so results are the same on any number of partitions, and the
//! same as over the edges in that order. Plans that preserve order are not
//! sorted.

use std::path::{Path, PathBuf};

use datafusion::arrow::util::pretty::pretty_format_batches;
use datafusion::common::TableReference;
use datafusion::prelude::{SessionConfig, SessionContext};
use ursa_engine::graph::{registry_of, EdgeTable, GraphDef};
use ursa_engine::new_session_with_config;

fn data(file: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file);
    path.display().to_string()
}

/// A session with `partitions` target partitions, and graphs over a join, an
/// aggregate, a union and the sorted file.
async fn session(partitions: usize) -> SessionContext {
    let ctx = new_session_with_config(SessionConfig::new().with_target_partitions(partitions));
    let statements = [
        format!(
            "CREATE VIEW ties AS SELECT * FROM '{}'",
            data("karate_edges.csv")
        ),
        format!(
            "CREATE VIEW members AS SELECT * FROM '{}'",
            data("karate_nodes.csv")
        ),
        format!(
            "CREATE VIEW scenes AS SELECT * FROM '{}'",
            data("lesmis_edges.csv")
        ),
        // Every tie, through a join with the members on both ends.
        "CREATE VIEW joined AS SELECT t.src, t.dst FROM ties t \
         JOIN members a ON t.src = a.id JOIN members b ON t.dst = b.id"
            .into(),
        "CREATE VIEW sorted_ties AS SELECT * FROM ties ORDER BY src, dst".into(),
        // Weighted edges through an aggregate.
        "CREATE VIEW summed AS SELECT src, dst, sum(weight) AS weight FROM scenes \
         GROUP BY src, dst"
            .into(),
        "CREATE VIEW both_ways AS SELECT src, dst FROM ties UNION ALL SELECT dst, src FROM ties"
            .into(),
    ];
    for sql in statements {
        ctx.sql(&sql).await.unwrap().collect().await.unwrap();
    }
    for (graph, edges) in [
        ("g_join", "joined"),
        ("g_sorted", "sorted_ties"),
        ("g_file", "ties"),
        ("g_sum", "summed"),
        ("g_union", "both_ways"),
    ] {
        registry_of(&ctx).put(GraphDef {
            name: graph.into(),
            edges: EdgeTable::new(TableReference::bare(edges), "src", "dst"),
            vertices: None,
        });
    }
    ctx
}

/// The rows of `sql`, in the order the query returns them.
async fn rows(ctx: &SessionContext, sql: &str) -> String {
    let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
    pretty_format_batches(&batches).unwrap().to_string()
}

/// The queries whose results must not depend on the partition count: label
/// values, float bits, and the order of a traversal's rows.
const QUERIES: &[&str] = &[
    "SELECT * FROM louvain(g_join, seed => 1)",
    "SELECT * FROM connected_components(g_join)",
    "SELECT id, arrow_cast(pagerank, 'Utf8') AS bits FROM pagerank(g_join)",
    "SELECT * FROM louvain(g_sum, seed => 1, weight => weight)",
    "SELECT id, arrow_cast(pagerank, 'Utf8') AS bits FROM pagerank(g_sum, weight => weight)",
    "SELECT * FROM louvain(g_union, seed => 1)",
    "SELECT * FROM hop(g_file, seeds => (SELECT src FROM ties GROUP BY src), n => 1)",
    "SELECT * FROM hop(g_join, seeds => (SELECT dst FROM joined GROUP BY dst), n => 1)",
];

#[tokio::test]
async fn a_graph_over_a_join_gives_the_same_results_on_any_partition_count() {
    let baseline = session(1).await;
    let mut expected = Vec::new();
    for sql in QUERIES {
        expected.push(rows(&baseline, sql).await);
    }
    for partitions in [2, 3, 4, 8, 16] {
        let ctx = session(partitions).await;
        for (sql, expected) in QUERIES.iter().zip(&expected) {
            assert_eq!(
                &rows(&ctx, sql).await,
                expected,
                "{sql} on {partitions} partitions"
            );
        }
    }
}

#[tokio::test]
async fn a_graph_over_a_join_is_ordered_by_source_and_destination() {
    for partitions in [1, 4] {
        let ctx = session(partitions).await;
        for algorithm in [
            "louvain(GRAPH, seed => 1)",
            "connected_components(GRAPH)",
            "pagerank(GRAPH)",
        ] {
            let joined = rows(
                &ctx,
                &format!("SELECT * FROM {}", algorithm.replace("GRAPH", "g_join")),
            )
            .await;
            let sorted = rows(
                &ctx,
                &format!("SELECT * FROM {}", algorithm.replace("GRAPH", "g_sorted")),
            )
            .await;
            assert_eq!(joined, sorted, "{algorithm} on {partitions} partitions");
        }
    }
}

/// The physical plan of `sql`, as `EXPLAIN` shows it.
async fn physical_plan(ctx: &SessionContext, sql: &str) -> String {
    let out = rows(ctx, &format!("EXPLAIN {sql}")).await;
    let start = out.find("physical_plan").expect("a physical plan");
    out[start..].to_string()
}

#[tokio::test]
async fn explain_shows_the_sort_only_where_order_is_not_preserved() {
    let ctx = session(4).await;
    let plan = physical_plan(&ctx, "SELECT * FROM pagerank(g_join)").await;
    assert!(
        plan.contains("SortExec: expr=[src@0 ASC NULLS LAST, dst@1 ASC NULLS LAST]"),
        "{plan}"
    );
    let plan = physical_plan(&ctx, "SELECT * FROM pagerank(g_sum, weight => weight)").await;
    assert!(plan.contains("SortExec"), "{plan}");
    // Over the file (and a view that filters it), no sort.
    for sql in [
        "SELECT * FROM pagerank(g_file)",
        "SELECT * FROM hop(g_file, seeds => [0], n => 1)",
    ] {
        let plan = physical_plan(&ctx, sql).await;
        assert!(!plan.contains("SortExec"), "{plan}");
    }
    // Seeds from an aggregate are sorted; the edges from the file are not.
    let plan = physical_plan(
        &ctx,
        "SELECT * FROM hop(g_file, seeds => (SELECT src FROM ties GROUP BY src))",
    )
    .await;
    assert_eq!(plan.matches("SortExec").count(), 1, "{plan}");
}
