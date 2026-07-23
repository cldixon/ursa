//! Neighbour aggregation — a per-node segmented reduction over the CSR.
//!
//! `ur.neighbors(edges, direction).agg(ur.col("capacity").mean())` computes, for
//! each node `u`, an aggregate of an attribute over `u`'s neighbours. The
//! attribute arrives already aligned to dense node index (`attr[v]` is node `v`'s
//! value, or `None` if it has no attribute row); the reduction walks each node's
//! adjacency and folds the present neighbour values. No neighbour list is ever
//! materialized — this is a segmented reduction straight over the adjacency.

use crate::topology::{Direction, Topology};

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
pub fn neighbor_aggregate(
    topo: &Topology,
    attr: &[Option<f64>],
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
        .map(|u| {
            let mut fold = Fold::new(agg);
            match direction {
                Direction::Out => {
                    for &v in topo.out().neighbors(u) {
                        fold.push(attr[v as usize]);
                    }
                }
                Direction::In => {
                    for &v in topo.incoming().neighbors(u) {
                        fold.push(attr[v as usize]);
                    }
                }
                Direction::Both => {
                    for &v in topo.out().neighbors(u) {
                        fold.push(attr[v as usize]);
                    }
                    for &v in topo.incoming().neighbors(u) {
                        fold.push(attr[v as usize]);
                    }
                }
            }
            fold.finish()
        })
        .collect()
}

/// Running accumulator for one node's reduction.
struct Fold {
    agg: AggKind,
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    seen: Vec<f64>, // only populated for NUnique
}

impl Fold {
    fn new(agg: AggKind) -> Self {
        Fold {
            agg,
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            seen: Vec::new(),
        }
    }

    fn push(&mut self, value: Option<f64>) {
        let Some(x) = value else { return };
        self.count += 1;
        self.sum += x;
        if x < self.min {
            self.min = x;
        }
        if x > self.max {
            self.max = x;
        }
        if self.agg == AggKind::NUnique && !self.seen.contains(&x) {
            self.seen.push(x);
        }
    }

    fn finish(self) -> Option<f64> {
        match self.agg {
            AggKind::Count => Some(self.count as f64),
            AggKind::NUnique => Some(self.seen.len() as f64),
            AggKind::Sum => Some(self.sum),
            _ if self.count == 0 => None,
            AggKind::Mean => Some(self.sum / self.count as f64),
            AggKind::Min => Some(self.min),
            AggKind::Max => Some(self.max),
        }
    }
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
        let out = neighbor_aggregate(&t, &attr, Direction::In, AggKind::Mean);
        assert_eq!(out[0], Some(20.0)); // mean(10,20,30)
        assert_eq!(out[1], None); // node 1 has no in-neighbours
    }

    #[test]
    fn sum_and_count_default_to_zero_when_empty() {
        let t = hub();
        let attr = vec![Some(1.0); 4];
        let s = neighbor_aggregate(&t, &attr, Direction::In, AggKind::Sum);
        assert_eq!(s[0], Some(3.0));
        assert_eq!(s[1], Some(0.0)); // empty sum -> 0
        let c = neighbor_aggregate(&t, &attr, Direction::In, AggKind::Count);
        assert_eq!(c[0], Some(3.0));
        assert_eq!(c[1], Some(0.0));
    }

    #[test]
    fn skips_missing_attributes() {
        let t = hub();
        // node 2 has no attribute value
        let attr = vec![None, Some(10.0), None, Some(30.0)];
        let out = neighbor_aggregate(&t, &attr, Direction::In, AggKind::Mean);
        assert_eq!(out[0], Some(20.0)); // mean(10, 30), 2 skipped
    }

    #[test]
    fn n_unique_counts_distinct() {
        let t = hub();
        let attr = vec![None, Some(5.0), Some(5.0), Some(9.0)];
        let out = neighbor_aggregate(&t, &attr, Direction::In, AggKind::NUnique);
        assert_eq!(out[0], Some(2.0)); // {5, 9}
    }
}
