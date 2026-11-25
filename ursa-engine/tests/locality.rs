//! The locality relabel round-trips through SQL (#172): an index of 65,536
//! edges or more numbers its nodes by in-degree, and every result, read by
//! user id, is the one an index in first-seen order gives (the Python
//! library's build, `ursa_io::build_topology_batches`): the same component
//! labels, communities, counts and reached sets, and PageRank to within its
//! last bits. Per-node rows come out in first-seen order, as before.

use std::collections::HashMap;
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, Float64Array, Int64Array, UInt32Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::TableReference;
use datafusion::prelude::SessionContext;
use ursa_core::algo::{
    connected_components_strong, connected_components_weak, label_propagation, louvain, pagerank,
    triangle_count, PageRankParams,
};
use ursa_engine::graph::{registry_of, EdgeTable, GraphDef};
use ursa_engine::new_session;

/// 100,000 skewed edges over about 6,000 nodes with gappy ids, the low ones
/// drawn most, so the in-degree order is far from the first-seen one.
fn edges() -> (Vec<i64>, Vec<i64>) {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    (0..100_000)
        .map(|_| {
            let a = next(6_000).min(next(6_000));
            let b = next(6_000).min(next(6_000)).min(next(6_000));
            (a as i64 * 7 + 3, b as i64 * 7 + 3)
        })
        .unzip()
}

/// Each row's `id` and column `column`, from `sql`.
async fn by_id<T: Copy>(
    ctx: &SessionContext,
    sql: &str,
    column: &str,
    value: impl Fn(&ArrayRef, usize) -> T,
) -> Vec<(i64, T)> {
    let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
    let mut rows = Vec::new();
    for b in &batches {
        let ids = b.column_by_name("id").unwrap();
        let ids = ids.as_any().downcast_ref::<Int64Array>().unwrap();
        let values = b.column_by_name(column).unwrap();
        for i in 0..b.num_rows() {
            rows.push((ids.value(i), value(values, i)));
        }
    }
    rows
}

fn label(a: &ArrayRef, i: usize) -> u32 {
    a.as_any().downcast_ref::<UInt32Array>().unwrap().value(i)
}

#[tokio::test]
async fn results_by_user_id_are_those_of_an_unrelabelled_index() {
    let (src, dst) = edges();
    let (src, dst): (ArrayRef, ArrayRef) = (
        Arc::new(Int64Array::from(src)),
        Arc::new(Int64Array::from(dst)),
    );
    let ctx = new_session();
    let schema = Arc::new(Schema::new(vec![
        Field::new("src", DataType::Int64, false),
        Field::new("dst", DataType::Int64, false),
    ]));
    let batch = RecordBatch::try_new(schema, vec![src.clone(), dst.clone()]).unwrap();
    ctx.register_batch("e", batch).unwrap();
    registry_of(&ctx).put(GraphDef {
        name: "g".into(),
        edges: EdgeTable::new(TableReference::bare("e"), "src", "dst"),
        vertices: None,
    });

    // The reference: the first-seen build, and its kernels.
    let (plain, ids) = ursa_io::build_topology_batches(&[(src.as_ref(), dst.as_ref())]).unwrap();
    assert!(plain.n_edges() >= 1 << 16);
    let user = ids.user_id_array();
    let user = user.as_any().downcast_ref::<Int64Array>().unwrap();
    let keyed = |values: Vec<u32>| -> Vec<(i64, u32)> {
        (0..values.len())
            .map(|v| (user.value(v), values[v]))
            .collect()
    };

    // Per-node rows in first-seen order, and the same labels by user id.
    for (sql, column, expected) in [
        (
            "SELECT * FROM connected_components(g)",
            "connected_components",
            keyed(connected_components_weak(&plain, None)),
        ),
        (
            "SELECT * FROM connected_components(g, mode => 'strong')",
            "connected_components",
            keyed(connected_components_strong(&plain, None)),
        ),
        (
            "SELECT * FROM louvain(g, seed => 1)",
            "louvain",
            keyed(louvain(&plain, None, 1.0, Some(1), false)),
        ),
        (
            "SELECT * FROM louvain(g, seed => 1, parallel => true)",
            "louvain",
            keyed(louvain(&plain, None, 1.0, Some(1), true)),
        ),
        (
            "SELECT * FROM label_propagation(g, seed => 1)",
            "label_propagation",
            keyed(label_propagation(&plain, None, 20, Some(1), false)),
        ),
        (
            "SELECT * FROM triangle_count(g)",
            "triangle_count",
            keyed(triangle_count(&plain, None)),
        ),
    ] {
        assert_eq!(by_id(&ctx, sql, column, label).await, expected, "{sql}");
    }
    // The ranks, to within their last bits.
    let ranks = pagerank(&plain, None, PageRankParams::default());
    let got = by_id(&ctx, "SELECT * FROM pagerank(g)", "pagerank", |a, i| {
        a.as_any().downcast_ref::<Float64Array>().unwrap().value(i)
    })
    .await;
    assert_eq!(got.len(), ranks.len());
    for (v, (id, x)) in got.into_iter().enumerate() {
        assert_eq!(id, user.value(v));
        assert!(
            (x - ranks[v]).abs() <= 1e-12 * ranks[v],
            "{id}: {x} vs {}",
            ranks[v]
        );
    }
    // A traversal reaches the same nodes, at the same distances.
    let hub = user.value(0);
    let sql = format!("SELECT id, distance FROM hop(g, seeds => [{hub}], steps => 2)");
    let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
    let mut got = HashMap::new();
    for b in &batches {
        let ids = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        let d = b.column(1).as_any().downcast_ref::<Int64Array>().unwrap();
        for i in 0..b.num_rows() {
            assert!(got.insert(ids.value(i), d.value(i)).is_none());
        }
    }
    let (_, reached, distance) =
        ursa_core::algo::k_hop_distances_masked(&plain, None, &[0], 2, ursa_core::Direction::Out);
    let expected: HashMap<i64, i64> = reached
        .iter()
        .zip(&distance)
        .map(|(&r, &d)| (user.value(r as usize), i64::from(d)))
        .collect();
    assert_eq!(got, expected);
}
