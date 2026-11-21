//! The force-directed layouts in SQL (#395): `layout_fa2` (coarse to fine by
//! default, flat with `multilevel => false`) and `layout_fr`, as table
//! functions of `(id, x, y)` and inside `algorithms()`.
//!
//! Positions must be `ursa-core`'s, bit for bit: the same kernel over the
//! index the graph node builds, with the parameters the call names. They are
//! the same bits on every run and every thread count (`ursa-core`'s
//! `determinism_threads.rs` checks the kernels across pools; here, the SQL
//! path end to end).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use datafusion::arrow::array::{Array, ArrayRef, Float32Array, Int64Array};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::DataType;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::TableReference;
use datafusion::prelude::SessionContext;
use ursa_core::algo::{
    layout_fa2, layout_fa2_multilevel, layout_fr, Fa2Params, LayoutParams, MultilevelParams,
    Positions,
};
use ursa_core::Topology;
use ursa_engine::graph::{registry_of, EdgeTable, GraphDef, VertexTable};
use ursa_engine::{new_session, user_message};

fn data(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../python/ursa/datasets/data")
        .join(file)
}

/// A session with `karate` (integer ids) and `lesmis` (string ids), and
/// `karate_v`: karate's edges with node 0's dropped, over a vertex table of
/// every node, so node 0 is isolated.
async fn session() -> SessionContext {
    let ctx = new_session();
    for (view, file) in [("ties", "karate_edges.csv"), ("scenes", "lesmis_edges.csv")] {
        ctx.sql(&format!(
            "CREATE OR REPLACE VIEW {view} AS SELECT * FROM '{}'",
            data(file).display()
        ))
        .await
        .unwrap();
    }
    ctx.sql("CREATE VIEW ties_v AS SELECT * FROM ties WHERE src <> 0 AND dst <> 0")
        .await
        .unwrap();
    ctx.sql(
        "CREATE VIEW people AS SELECT DISTINCT id FROM \
         (SELECT src AS id FROM ties UNION ALL SELECT dst AS id FROM ties)",
    )
    .await
    .unwrap();
    let registry = registry_of(&ctx);
    for (graph, table) in [("karate", "ties"), ("lesmis", "scenes")] {
        registry.put(GraphDef {
            name: graph.into(),
            edges: EdgeTable::new(TableReference::bare(table), "src", "dst"),
            vertices: None,
        });
    }
    registry.put(GraphDef {
        name: "karate_v".into(),
        edges: EdgeTable::new(TableReference::bare("ties_v"), "src", "dst"),
        vertices: Some(VertexTable::new(TableReference::bare("people"), "id")),
    });
    ctx
}

async fn batches(ctx: &SessionContext, sql: &str) -> Vec<RecordBatch> {
    ctx.sql(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .collect()
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn error(ctx: &SessionContext, sql: &str) -> String {
    match ctx.sql(sql).await {
        Ok(frame) => match frame.collect().await {
            Ok(_) => panic!("{sql} should have failed"),
            Err(e) => user_message(&e),
        },
        Err(e) => user_message(&e),
    }
}

/// Rows of `(id, x bits, y bits)` from a query's columns `id`, `x` and `y`
/// (or the names given), ids as strings, sorted by id.
async fn positions(ctx: &SessionContext, sql: &str, x: &str, y: &str) -> Vec<(String, u32, u32)> {
    let mut rows = Vec::new();
    for b in batches(ctx, sql).await {
        let schema = b.schema();
        let col = |name: &str| b.column(schema.index_of(name).unwrap()).clone();
        let ids = cast(&col("id"), &DataType::Utf8).unwrap();
        let ids = ids
            .as_any()
            .downcast_ref::<datafusion::arrow::array::StringArray>()
            .unwrap();
        let (xs, ys) = (col(x), col(y));
        assert_eq!(xs.data_type(), &DataType::Float32, "{sql}: {x}");
        assert_eq!(ys.data_type(), &DataType::Float32, "{sql}: {y}");
        let xs = xs.as_any().downcast_ref::<Float32Array>().unwrap();
        let ys = ys.as_any().downcast_ref::<Float32Array>().unwrap();
        for i in 0..b.num_rows() {
            assert!(xs.is_valid(i) && ys.is_valid(i), "{sql}: a NULL position");
            rows.push((
                ids.value(i).to_string(),
                xs.value(i).to_bits(),
                ys.value(i).to_bits(),
            ));
        }
    }
    rows.sort();
    rows
}

/// The topology the engine builds for the edge view `edges` (no vertex
/// table), and its node ids in dense order, as strings.
async fn topology(
    ctx: &SessionContext,
    edges: &str,
    ids: &DataType,
) -> (std::sync::Arc<Topology>, Vec<String>) {
    let batches = batches(ctx, &format!("SELECT src, dst FROM {edges}")).await;
    let endpoints: Vec<(ArrayRef, ArrayRef)> = batches
        .iter()
        .map(|b| {
            (
                cast(b.column(0), ids).unwrap(),
                cast(b.column(1), ids).unwrap(),
            )
        })
        .collect();
    let chunks: Vec<(&dyn Array, &dyn Array)> = endpoints
        .iter()
        .map(|(s, d)| (s.as_ref(), d.as_ref()))
        .collect();
    let (topology, map) = ursa_io::build_topology_batches(&chunks).unwrap();
    let ids = cast(&map.user_id_array(), &DataType::Utf8).unwrap();
    let ids = ids
        .as_any()
        .downcast_ref::<datafusion::arrow::array::StringArray>()
        .unwrap();
    let ids = (0..ids.len()).map(|i| ids.value(i).to_string()).collect();
    (topology, ids)
}

fn rows(ids: &[String], (x, y): &Positions) -> Vec<(String, u32, u32)> {
    let mut rows: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), x[i].to_bits(), y[i].to_bits()))
        .collect();
    rows.sort();
    rows
}

fn multilevel(fa2: Fa2Params) -> MultilevelParams {
    MultilevelParams {
        fa2,
        ..MultilevelParams::default()
    }
}

/// Every call's positions are ursa-core's, bit for bit, on integer and
/// string ids.
#[tokio::test]
async fn positions_are_ursa_cores() {
    let ctx = session().await;
    let ml = MultilevelParams::default();
    let flat = Fa2Params::default();
    for (graph, edges, id_type) in [
        ("karate", "ties", DataType::Int64),
        ("lesmis", "scenes", DataType::Utf8),
    ] {
        let (topo, ids) = topology(&ctx, edges, &id_type).await;
        let cases: Vec<(String, Positions)> = vec![
            (
                format!("layout_fa2({graph})"),
                layout_fa2_multilevel(&topo, None, ml),
            ),
            (
                format!("layout_fa2({graph}, seed => 7)"),
                layout_fa2_multilevel(
                    &topo,
                    None,
                    multilevel(Fa2Params {
                        seed: Some(7),
                        ..ml.fa2
                    }),
                ),
            ),
            (
                format!("layout_fa2({graph}, multilevel => false)"),
                layout_fa2(&topo, None, flat),
            ),
            (
                format!(
                    "layout_fa2({graph}, multilevel => false, iterations => 40, k => 2, \
                     gravity => 0.5, strong_gravity => true, lin_log => true, theta => 0.7, \
                     jitter_tolerance => 0.5, seed => 3)"
                ),
                layout_fa2(
                    &topo,
                    None,
                    Fa2Params {
                        iterations: 40,
                        k: 2.0,
                        gravity: 0.5,
                        strong_gravity: true,
                        lin_log: true,
                        theta: 0.7,
                        jitter_tolerance: 0.5,
                        seed: Some(3),
                    },
                ),
            ),
            (
                // Coarse to fine, `iterations` is the graph's own: no work
                // budget, a floor of 25, each level up up to 200. Karate (34
                // nodes) is too small to coarsen: it is its own coarsest
                // level, and gets the 25 too.
                format!("layout_fa2({graph}, iterations => 25, theta => 0.5, gravity => 2.0)"),
                layout_fa2_multilevel(
                    &topo,
                    None,
                    MultilevelParams {
                        fa2: Fa2Params {
                            theta: 0.5,
                            gravity: 2.0,
                            ..ml.fa2
                        },
                        finest_work: 0.0,
                        min_iterations: 25,
                        coarsest_iterations: if graph == "karate" { 25 } else { 300 },
                        ..ml
                    },
                ),
            ),
            (
                format!("layout_fr({graph})"),
                layout_fr(&topo, None, LayoutParams::default()),
            ),
            (
                format!(
                    "layout_fr({graph}, iterations => 50, k => 2.0, gravity => 0.1, seed => 5)"
                ),
                layout_fr(
                    &topo,
                    None,
                    LayoutParams {
                        iterations: 50,
                        k: 2.0,
                        gravity: 0.1,
                        seed: Some(5),
                        ..LayoutParams::default()
                    },
                ),
            ),
        ];
        for (call, want) in cases {
            let sql = format!("SELECT * FROM {call}");
            let got = positions(&ctx, &sql, "x", "y").await;
            assert_eq!(got.len(), topo.n_nodes(), "{sql}");
            assert_eq!(got, rows(&ids, &want), "{sql}");
        }
    }
}

/// `iterations => 25` coarse to fine gives the graph itself 25 iterations,
/// not the default schedule's.
#[tokio::test]
async fn coarse_to_fine_iterations_change_the_layout() {
    let ctx = session().await;
    let default = positions(&ctx, "SELECT * FROM layout_fa2(lesmis)", "x", "y").await;
    let short = positions(
        &ctx,
        "SELECT * FROM layout_fa2(lesmis, iterations => 25)",
        "x",
        "y",
    )
    .await;
    let flat = positions(
        &ctx,
        "SELECT * FROM layout_fa2(lesmis, multilevel => false)",
        "x",
        "y",
    )
    .await;
    assert_ne!(default, short);
    assert_ne!(default, flat);
    // A graph too small to coarsen (karate, 34 nodes) gets them too.
    assert_ne!(
        positions(&ctx, "SELECT * FROM layout_fa2(karate)", "x", "y").await,
        positions(
            &ctx,
            "SELECT * FROM layout_fa2(karate, iterations => 25)",
            "x",
            "y"
        )
        .await
    );
}

/// The same call gives the same bits: twice in one session, in a fresh
/// session, and with the engine splitting work over one partition or many.
#[tokio::test]
async fn positions_are_deterministic() {
    let ctx = session().await;
    for sql in [
        "SELECT * FROM layout_fa2(lesmis)",
        "SELECT * FROM layout_fa2(lesmis, multilevel => false, iterations => 60)",
        "SELECT * FROM layout_fr(karate, seed => 2)",
    ] {
        let first = positions(&ctx, sql, "x", "y").await;
        assert_eq!(first, positions(&ctx, sql, "x", "y").await, "{sql}");
        let fresh = session().await;
        assert_eq!(first, positions(&fresh, sql, "x", "y").await, "{sql}");
        for partitions in [1, 7] {
            let other = session().await;
            other
                .sql(&format!(
                    "SET datafusion.execution.target_partitions = {partitions}"
                ))
                .await
                .unwrap();
            assert_eq!(first, positions(&other, sql, "x", "y").await, "{sql}");
        }
        // A different seed is a different layout.
        let reseeded = sql.replace("lesmis)", "lesmis, seed => 99)");
        if reseeded != sql {
            assert_ne!(
                first,
                positions(&ctx, &reseeded, "x", "y").await,
                "{reseeded}"
            );
        }
    }
}

/// Inside `algorithms()`: a layout's two columns are `x` and `y`, or
/// `<key>_x` and `<key>_y`, beside the other functions' columns, with the
/// standalone calls' values.
#[tokio::test]
async fn layouts_compose_in_algorithms() {
    let ctx = session().await;
    let sql = "SELECT * FROM algorithms(lesmis, degree(), fa2 => layout_fa2(seed => 1), \
               flat => layout_fa2(multilevel => false, iterations => 30), layout_fr(seed => 2))";
    let batches = batches(&ctx, sql).await;
    let names: Vec<String> = batches[0]
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    assert_eq!(
        names,
        ["id", "degree", "fa2_x", "fa2_y", "flat_x", "flat_y", "x", "y"]
    );
    for (columns, alone) in [
        (("fa2_x", "fa2_y"), "layout_fa2(lesmis, seed => 1)"),
        (
            ("flat_x", "flat_y"),
            "layout_fa2(lesmis, multilevel => false, iterations => 30)",
        ),
        (("x", "y"), "layout_fr(lesmis, seed => 2)"),
    ] {
        assert_eq!(
            positions(&ctx, sql, columns.0, columns.1).await,
            positions(&ctx, &format!("SELECT * FROM {alone}"), "x", "y").await,
            "{alone}"
        );
    }
    // Joined to the node table, as a figure would read it.
    let joined = batches_len(
        &ctx,
        "SELECT p.id, l.x, l.y FROM people p JOIN layout_fa2(karate) l USING (id)",
    )
    .await;
    assert_eq!(joined, 34);
    // Two layouts with no key would both be x and y.
    let message = error(
        &ctx,
        "SELECT * FROM algorithms(karate, layout_fa2(), layout_fr())",
    )
    .await;
    assert!(message.contains("'x'"), "{message}");
}

async fn batches_len(ctx: &SessionContext, sql: &str) -> usize {
    batches(ctx, sql).await.iter().map(|b| b.num_rows()).sum()
}

/// A graph with a vertex table: every vertex gets a finite position, the
/// isolated one included. (Values on such graphs are checked against the
/// Python library in `parity.rs`'s views.)
#[tokio::test]
async fn isolated_vertices_get_positions() {
    let ctx = session().await;
    for call in [
        "layout_fa2(karate_v)",
        "layout_fa2(karate_v, multilevel => false)",
        "layout_fr(karate_v)",
    ] {
        let got = positions(&ctx, &format!("SELECT * FROM {call}"), "x", "y").await;
        assert_eq!(got.len(), 34, "{call}");
        let ids: Vec<&str> = got.iter().map(|r| r.0.as_str()).collect();
        assert!(ids.contains(&"0"), "{call}: {ids:?}");
        assert!(
            got.iter()
                .all(|r| f32::from_bits(r.1).is_finite() && f32::from_bits(r.2).is_finite()),
            "{call}"
        );
    }
    // The ids are karate's, as integers.
    let b = batches(
        &ctx,
        "SELECT id FROM layout_fa2(karate_v) ORDER BY id LIMIT 1",
    )
    .await;
    let first = b[0]
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(first.value(0), 0);
}

/// The layouts take no weight, and a bad argument is named.
#[tokio::test]
async fn layout_errors() {
    let ctx = session().await;
    let cases: HashMap<&str, &str> = HashMap::from([
        (
            "SELECT * FROM layout_fa2(karate, multilevel => 'yes')",
            "layout_fa2(multilevel =>) must be a boolean; got 'yes'",
        ),
        (
            "SELECT * FROM layout_fa2(karate, iterations => -1)",
            "layout_fa2(iterations =>) must be a non-negative integer below 2^32, or NULL; got -1",
        ),
        (
            "SELECT * FROM layout_fa2(karate, iterations => 2.5)",
            "layout_fa2(iterations =>) must be a non-negative integer below 2^32, or NULL; got 2.5",
        ),
        (
            "SELECT * FROM layout_fa2(karate, theta => 'wide')",
            "layout_fa2(theta =>) must be a number or NULL; got 'wide'",
        ),
        (
            "SELECT * FROM layout_fa2(karate, seed => -3)",
            "layout_fa2(seed =>) must be a non-negative integer or NULL; got -3",
        ),
        (
            "SELECT * FROM layout_fr(karate, iterations => NULL)",
            "layout_fr(iterations =>) must be a non-negative integer below 2^32; got NULL",
        ),
        // Numbers with no meaning for a force (#416).
        (
            "SELECT * FROM layout_fa2(karate, k => 0)",
            "layout_fa2(k =>) must be above 0; got 0",
        ),
        (
            "SELECT * FROM layout_fa2(karate, gravity => -1)",
            "layout_fa2(gravity =>) must be at least 0; got -1",
        ),
        (
            "SELECT * FROM layout_fa2(karate, theta => -0.5)",
            "layout_fa2(theta =>) must be at least 0; got -0.5",
        ),
        (
            "SELECT * FROM layout_fa2(karate, jitter_tolerance => 0)",
            "layout_fa2(jitter_tolerance =>) must be above 0; got 0",
        ),
        (
            "SELECT * FROM layout_fr(karate, k => -2)",
            "layout_fr(k =>) must be above 0; got -2",
        ),
    ]);
    for (sql, want) in cases {
        assert_eq!(error(&ctx, sql).await, want, "{sql}");
    }
    // Unknown parameters, a weight among them, name the call's parameters.
    for (sql, starts) in [
        (
            "SELECT * FROM layout_fa2(karate, weight => weight)",
            "layout_fa2() has no parameter 'weight'",
        ),
        (
            "SELECT * FROM layout_fa2(karate, iteration => 10)",
            "layout_fa2() has no parameter 'iteration'; did you mean 'iterations'?",
        ),
        (
            "SELECT * FROM layout_fr(karate, multilevel => false)",
            "layout_fr() has no parameter 'multilevel'",
        ),
        (
            "SELECT * FROM algorithms(karate, layout_fa2(weight => weight))",
            "algorithms(): layout_fa2() has no parameter 'weight'",
        ),
    ] {
        let message = error(&ctx, sql).await;
        assert!(message.starts_with(starts), "{sql}: {message}");
    }
}
