//! The order the edges are drawn in: where each edge is in the GPU's edge
//! buffer.
//!
//! A view draws its edges in importance order, so that the prefix it draws
//! while it moves ([`crate::budget`]) is a backbone of the drawing. The
//! order comes from the layout, which has the hierarchy
//! ([`crate::scene::LiveLayout::edge_order`]); the view keeps where it put
//! each edge ([`EdgeSlots`]), because everything else names an edge by its
//! index: a delta removes edge 7, and the view has to find its slot.
//!
//! Only the first edges are ordered: the ones there were when the order was
//! computed. An edge added later is in the slot of its own index, after
//! them, so adding edges and taking them off again (an undo) are as they
//! were without an order: a write at the end, and a shorter draw range.

/// An order that is not one: what [`EdgeSlots::from_order`] refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadOrder {
    /// The order is of more edges than there are.
    TooLong { len: usize, n_edges: usize },
    /// It names an edge at or past its own length.
    OutOfRange { at: usize, edge: u32 },
    /// It names an edge twice.
    Repeated { at: usize, edge: u32 },
}

impl std::fmt::Display for BadOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::TooLong { len, n_edges } => {
                write!(f, "an order of {len} edges, but there are {n_edges}")
            }
            Self::OutOfRange { at, edge } => {
                write!(f, "the order names edge {edge} at {at}, past its own end")
            }
            Self::Repeated { at, edge } => write!(f, "the order names edge {edge} twice (at {at})"),
        }
    }
}

impl std::error::Error for BadOrder {}

/// Where each edge is in the edge buffer: four bytes for each ordered edge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EdgeSlots {
    /// The slot of each of the first edges; an edge past them is in the
    /// slot of its index.
    slot_of: Vec<u32>,
}

const UNSET: u32 = u32::MAX;

impl EdgeSlots {
    /// Every edge in the slot of its index.
    pub fn identity() -> Self {
        Self::default()
    }

    /// The slots for `order`, where `order[slot]` is the edge to draw in
    /// that slot: a permutation of the first `order.len()` of the `n_edges`
    /// edges there are.
    pub fn from_order(order: &[u32], n_edges: usize) -> Result<Self, BadOrder> {
        let len = order.len();
        if len > n_edges {
            return Err(BadOrder::TooLong { len, n_edges });
        }
        let mut slot_of = vec![UNSET; len];
        for (at, &edge) in order.iter().enumerate() {
            match slot_of.get_mut(edge as usize) {
                None => return Err(BadOrder::OutOfRange { at, edge }),
                Some(slot) if *slot != UNSET => return Err(BadOrder::Repeated { at, edge }),
                Some(slot) => *slot = at as u32,
            }
        }
        Ok(Self { slot_of })
    }

    /// How many edges are ordered: the first ones. The rest are in the
    /// slots of their indexes.
    pub fn ordered(&self) -> usize {
        self.slot_of.len()
    }

    /// The slot edge `e` is in.
    pub fn slot(&self, e: usize) -> usize {
        self.slot_of.get(e).map_or(e, |&s| s as usize)
    }

    /// The slots of the edges `run`, as runs of adjacent slots with the
    /// edges in them in slot order: what to write when those edges change.
    /// Past the ordered edges that is the run itself.
    pub fn slot_runs(&self, run: std::ops::Range<usize>) -> Vec<(usize, Vec<u32>)> {
        let ordered = self.ordered();
        let mut runs: Vec<(usize, Vec<u32>)> = Vec::new();
        if run.start < ordered {
            let mut slots: Vec<(u32, u32)> = (run.start..run.end.min(ordered))
                .map(|e| (self.slot_of[e], e as u32))
                .collect();
            slots.sort_unstable();
            for (slot, e) in slots {
                match runs.last_mut() {
                    Some((first, edges)) if *first + edges.len() == slot as usize => edges.push(e),
                    _ => runs.push((slot as usize, vec![e])),
                }
            }
        }
        let rest = run.start.max(ordered)..run.end;
        if !rest.is_empty() {
            runs.push((rest.start, (rest.start as u32..rest.end as u32).collect()));
        }
        runs
    }

    /// Whether a graph of `n_edges` edges still has every ordered edge: if
    /// not (edges were taken off past the ones the order was made for), the
    /// order no longer fills the first slots, and the caller starts again
    /// from [`EdgeSlots::identity`].
    pub fn fits(&self, n_edges: usize) -> bool {
        self.ordered() <= n_edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_is_a_permutation_of_the_first_edges() {
        let slots = EdgeSlots::from_order(&[2, 0, 3, 1], 6).unwrap();
        assert_eq!(slots.ordered(), 4);
        let at: Vec<usize> = (0..6).map(|e| slots.slot(e)).collect();
        // Edge 2 is drawn first; 4 and 5, added since, where they were.
        assert_eq!(at, [1, 3, 0, 2, 4, 5]);
        assert!(slots.fits(4) && !slots.fits(3));

        assert_eq!(
            EdgeSlots::from_order(&[0, 1, 2], 2),
            Err(BadOrder::TooLong { len: 3, n_edges: 2 })
        );
        assert_eq!(
            EdgeSlots::from_order(&[0, 3, 1], 3),
            Err(BadOrder::OutOfRange { at: 1, edge: 3 })
        );
        assert_eq!(
            EdgeSlots::from_order(&[1, 0, 1], 3),
            Err(BadOrder::Repeated { at: 2, edge: 1 })
        );
        let none = EdgeSlots::from_order(&[], 3).unwrap();
        assert_eq!(none, EdgeSlots::identity());
        assert_eq!(none.slot(2), 2);
    }

    #[test]
    fn edges_that_change_are_written_as_runs_of_slots() {
        // Slots: edge 3, 2, 0, 1; then 4, 5, 6 as they are.
        let slots = EdgeSlots::from_order(&[3, 2, 0, 1], 7).unwrap();
        // Edges 0..2 are in slots 2 and 3: one run, edge 0 first.
        assert_eq!(slots.slot_runs(0..2), [(2, vec![0, 1])]);
        // Edges 2..4 are in slots 1 and 0: one run, edge 3 first.
        assert_eq!(slots.slot_runs(2..4), [(0, vec![3, 2])]);
        // Edges 1..6: slots 0, 1 and 3, then the unordered run.
        assert_eq!(
            slots.slot_runs(1..6),
            [(0, vec![3, 2]), (3, vec![1]), (4, vec![4, 5])]
        );
        assert_eq!(slots.slot_runs(5..7), [(5, vec![5, 6])]);
        assert!(slots.slot_runs(3..3).is_empty());
        assert_eq!(EdgeSlots::identity().slot_runs(1..3), [(1, vec![1, 2])]);
    }
}
