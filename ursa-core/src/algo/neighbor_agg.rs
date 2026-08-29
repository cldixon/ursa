//! Neighbour aggregation — a per-node segmented reduction over the CSR.
//!
//! `ur.neighbors(edges, direction).agg(ur.col("capacity").mean())` computes, for
//! each node `u`, an aggregate of an attribute over `u`'s neighbours. The
//! attribute arrives already aligned to dense node index (`attr[v]` is node `v`'s
//! value, or `None` if it has no attribute row); the reduction walks each node's
//! adjacency and folds the present neighbour values. No neighbour list is ever
//! materialized — this is a segmented reduction straight over the adjacency.

use crate::parallel::*;

use crate::topology::{Direction, EdgeMask, Topology};

/// The supported neighbour aggregations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    Mean,
    Sum,
    Min,
    Max,
    Count,
    NUnique,
}

/// For each node, aggregate `attr` over its neighbours in `direction`.
///
/// `result[u]` is `None` where the aggregate is undefined (mean/min/max over a
/// node with no attributed neighbours); `sum`/`count`/`n_unique` return `0` in
/// that case. `Both` unions out- and in-neighbours (a neighbour reachable via
/// both directions is folded twice, matching multiplicity-is-rows).
///
/// Each node's reduction is independent, so the outer loop runs in parallel
/// (rayon); results collect positionally, so the output order is identical to a
/// serial run. `n_unique` sorts the gathered values (O(d log d)) instead of the
/// former O(d²) linear-scan dedup — decisive on the power-law hubs that are the
/// target workload. The sort scratch buffer is reused per worker via `map_init`.
pub fn neighbor_aggregate(
    topo: &Topology,
    attr: &[Option<f64>],
    mask: Option<&EdgeMask>,
    direction: Direction,
    agg: AggKind,
) -> Vec<Option<f64>> {
    assert_eq!(
        attr.len(),
        topo.n_nodes(),
        "attr length must equal the node count"
    );
    let n = topo.n_nodes();
    (0..n as u32)
        .into_par_iter()
        .map_init(Vec::<f64>::new, |scratch, u| {
            aggregate_node(topo, attr, mask, direction, agg, u, scratch)
        })
        .collect()
}

/// Aggregate one node's neighbour attribute values. `scratch` is a reusable
/// buffer (only `NUnique` fills it) so the hot path allocates nothing per node.
/// Iterates `(neighbour, edge_row)` pairs so a subgraph `mask` can drop masked-out
/// incident edges.
fn aggregate_node(
    topo: &Topology,
    attr: &[Option<f64>],
    mask: Option<&EdgeMask>,
    direction: Direction,
    agg: AggKind,
    u: u32,
    scratch: &mut Vec<f64>,
) -> Option<f64> {
    let out = topo.out();
    let out_pairs = || out.neighbors(u).iter().zip(out.edge_ids(u));
    match direction {
        Direction::Out => fold_neighbors(out_pairs(), attr, mask, agg, scratch),
        Direction::In => {
            let inc = topo.incoming();
            fold_neighbors(
                inc.neighbors(u).iter().zip(inc.edge_ids(u)),
                attr,
                mask,
                agg,
                scratch,
            )
        }
        Direction::Both => {
            let inc = topo.incoming();
            fold_neighbors(
                out_pairs().chain(inc.neighbors(u).iter().zip(inc.edge_ids(u))),
                attr,
                mask,
                agg,
                scratch,
            )
        }
    }
}

/// Fold the present attribute values of a `(neighbour, edge_row)` iterator into one
/// aggregate. Generic over the iterator so `Out`/`In` (a slice zip) and `Both`
/// (a chained zip) monomorphize without boxing. A masked-out edge row is skipped.
fn fold_neighbors<'a, I: Iterator<Item = (&'a u32, &'a u32)>>(
    neighbors: I,
    attr: &[Option<f64>],
    mask: Option<&EdgeMask>,
    agg: AggKind,
    scratch: &mut Vec<f64>,
) -> Option<f64> {
    let mut count: u64 = 0;
    let mut sum = 0.0;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    if agg == AggKind::NUnique {
        scratch.clear();
    }
    for (&v, &e) in neighbors {
        if !mask.is_none_or(|m| m.keep(e)) {
            continue;
        }
        let Some(x) = attr[v as usize] else { continue };
        count += 1;
        sum += x;
        if x < min {
            min = x;
        }
        if x > max {
            max = x;
        }
        if agg == AggKind::NUnique {
            scratch.push(x);
        }
    }
    match agg {
        AggKind::Count => Some(count as f64),
        AggKind::NUnique => Some(count_distinct(scratch) as f64),
        AggKind::Sum => Some(sum),
        _ if count == 0 => None,
        AggKind::Mean => Some(sum / count as f64),
        AggKind::Min => Some(min),
        AggKind::Max => Some(max),
    }
}

/// Count distinct values by sorting and counting runs — O(d log d) and cache
/// friendly, replacing the former O(d²) `Vec::contains` scan.
///
/// Values are canonicalized first so `-0.0` counts as `0.0` and every NaN
/// collapses to one, matching SQL `COUNT(DISTINCT)` (and fixing the old
/// `==`-dedup, which — since `NaN != NaN` — counted each NaN separately). After
/// canonicalization, bit-equality on the `total_cmp`-sorted values is exact.
fn count_distinct(values: &mut [f64]) -> u64 {
    for x in values.iter_mut() {
        if x.is_nan() {
            *x = f64::NAN;
        } else if *x == 0.0 {
            *x = 0.0; // normalize -0.0 to +0.0
        }
    }
    values.sort_unstable_by(f64::total_cmp);
    let mut distinct: u64 = 0;
    let mut prev_bits: Option<u64> = None;
    for &x in values.iter() {
        let bits = x.to_bits();
        if prev_bits != Some(bits) {
            distinct += 1;
            prev_bits = Some(bits);
        }
    }
    distinct
}

#[cfg(test)]
mod tests {
    use super::*;

    // hub graph: 1->0, 2->0, 3->0  (node 0's in-neighbours are 1,2,3)
    fn hub() -> Topology {
        Topology::build(4, vec![1, 2, 3], vec![0, 0, 0])
    }

    #[test]
    fn mean_over_in_neighbours() {
        let t = hub();
        let attr = vec![Some(0.0), Some(10.0), Some(20.0), Some(30.0)];
        let out = neighbor_aggregate(&t, &attr, None, Direction::In, AggKind::Mean);
        assert_eq!(out[0], Some(20.0)); // mean(10,20,30)
        assert_eq!(out[1], None); // node 1 has no in-neighbours
    }

    #[test]
    fn sum_and_count_default_to_zero_when_empty() {
        let t = hub();
        let attr = vec![Some(1.0); 4];
        let s = neighbor_aggregate(&t, &attr, None, Direction::In, AggKind::Sum);
        assert_eq!(s[0], Some(3.0));
        assert_eq!(s[1], Some(0.0)); // empty sum -> 0
        let c = neighbor_aggregate(&t, &attr, None, Direction::In, AggKind::Count);
        assert_eq!(c[0], Some(3.0));
        assert_eq!(c[1], Some(0.0));
    }

    #[test]
    fn skips_missing_attributes() {
        let t = hub();
        // node 2 has no attribute value
        let attr = vec![None, Some(10.0), None, Some(30.0)];
        let out = neighbor_aggregate(&t, &attr, None, Direction::In, AggKind::Mean);
        assert_eq!(out[0], Some(20.0)); // mean(10, 30), 2 skipped
    }

    #[test]
    fn n_unique_counts_distinct() {
        let t = hub();
        let attr = vec![None, Some(5.0), Some(5.0), Some(9.0)];
        let out = neighbor_aggregate(&t, &attr, None, Direction::In, AggKind::NUnique);
        assert_eq!(out[0], Some(2.0)); // {5, 9}
    }

    #[test]
    fn n_unique_canonicalizes_zero_and_nan() {
        // -0.0 counts as 0.0, and every NaN collapses to one (SQL COUNT(DISTINCT)).
        // hub: 1,2,3 -> 0. Give node 0 four in-neighbours (extend the hub).
        let t = Topology::build(5, vec![1, 2, 3, 4], vec![0, 0, 0, 0]);
        let attr = vec![
            Some(0.0),
            Some(-0.0),
            Some(0.0),
            Some(f64::NAN),
            Some(f64::NAN),
        ];
        // in-neighbours of 0 carry {-0.0, 0.0, NaN, NaN} -> distinct {0.0, NaN} = 2
        let out = neighbor_aggregate(&t, &attr, None, Direction::In, AggKind::NUnique);
        assert_eq!(out[0], Some(2.0));
    }

    #[test]
    fn parallel_matches_serial_on_a_larger_hub() {
        // A wider hub with repeats exercises the sort-based n_unique path.
        let src: Vec<u32> = (1..50).collect();
        let dst: Vec<u32> = vec![0; 49];
        let t = Topology::build(50, src, dst);
        let mut attr = vec![Some(0.0); 50];
        for (i, a) in attr.iter_mut().enumerate().skip(1) {
            *a = Some((i % 7) as f64); // 7 distinct values among the 49 neighbours
        }
        let out = neighbor_aggregate(&t, &attr, None, Direction::In, AggKind::NUnique);
        assert_eq!(out[0], Some(7.0));
    }
}
