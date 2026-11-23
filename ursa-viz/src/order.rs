//! The order the edges are drawn in.
//!
//! A view draws its edges in importance order, so that the prefix it draws
//! while it moves ([`crate::budget`]) is a backbone of the drawing. The
//! order comes from the layout, which has the hierarchy
//! ([`crate::scene::LiveLayout::edge_order`]), as a permutation: `order[i]`
//! is the edge to draw `i`-th.
//!
//! Only the first edges are ordered: the ones there were when the order was
//! computed. An edge added later is drawn after them, in the slot of its
//! own index, so adding edges and taking them off again (an undo) are as
//! they were without an order: a write at the end, and a shorter draw range.
//!
//! The view keeps no table of where each edge went (four bytes an edge, for
//! as long as the view lives): it keeps how many are ordered, and that is
//! enough for everything a front end does today. What it cannot do without
//! the table is rewrite one ordered edge in place, so a delta that removes
//! or restores an ordered edge, or takes ordered edges off the end, puts
//! every edge back in the slot of its index ([`Ordered::keeps`]): the
//! frames at rest do not change, and the frames that move draw a prefix by
//! index until an order is set again.

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

/// An edge order, wherever it is held: a vector here, or an array of a
/// page's that a view reads a run at a time (#501), so that a million
/// edges' order is not copied into the view's memory to be written to the
/// GPU from there.
pub trait EdgeOrder {
    /// How many edges it orders.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy the edges of the slots from `start` on into `out`, all of it.
    fn read(&self, start: usize, out: &mut [u32]);
}

impl EdgeOrder for Vec<u32> {
    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn read(&self, start: usize, out: &mut [u32]) {
        out.copy_from_slice(&self[start..start + out.len()]);
    }
}

/// How many slots of an order are read at a time.
const ORDER_RUN: usize = 1 << 16;

/// Check that `order` is a permutation of the first `order.len()` of
/// `n_edges` edges, in an eighth of a byte an edge.
pub fn check_order(order: &[u32], n_edges: usize) -> Result<(), BadOrder> {
    let mut seen = Seen::new(order.len(), n_edges)?;
    seen.check(0, order)
}

/// [`check_order`] of an order read a run at a time.
pub fn check_order_from(order: &dyn EdgeOrder, n_edges: usize) -> Result<(), BadOrder> {
    let len = order.len();
    let mut seen = Seen::new(len, n_edges)?;
    let mut run = vec![0u32; ORDER_RUN.min(len)];
    let mut start = 0;
    while start < len {
        let run = &mut run[..ORDER_RUN.min(len - start)];
        order.read(start, run);
        seen.check(start, run)?;
        start += run.len();
    }
    Ok(())
}

/// The edges of an order met so far: a bit each.
struct Seen {
    len: usize,
    bits: Vec<u64>,
}

impl Seen {
    fn new(len: usize, n_edges: usize) -> Result<Self, BadOrder> {
        if len > n_edges {
            return Err(BadOrder::TooLong { len, n_edges });
        }
        Ok(Self {
            len,
            bits: vec![0u64; len.div_ceil(64)],
        })
    }

    /// The slots from `start` on hold `edges`.
    fn check(&mut self, start: usize, edges: &[u32]) -> Result<(), BadOrder> {
        for (i, &edge) in edges.iter().enumerate() {
            let at = start + i;
            if edge as usize >= self.len {
                return Err(BadOrder::OutOfRange { at, edge });
            }
            let (word, bit) = (edge as usize / 64, 1u64 << (edge % 64));
            if self.bits[word] & bit != 0 {
                return Err(BadOrder::Repeated { at, edge });
            }
            self.bits[word] |= bit;
        }
        Ok(())
    }
}

/// How many of a view's edges are drawn in an order of their own: the
/// first ones. The rest are in the slots of their indexes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ordered(pub usize);

impl Ordered {
    /// Whether the order survives a delta that leaves edges from
    /// `new_edges_from` on to write (the first edge added, or the edge
    /// count if edges were taken off) and changes the edges of `runs` in
    /// place: it does if none of that touches an ordered edge.
    pub fn keeps(self, new_edges_from: usize, runs: &[std::ops::Range<usize>]) -> bool {
        new_edges_from >= self.0 && runs.iter().all(|r| r.is_empty() || r.start >= self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_is_a_permutation_of_the_first_edges() {
        assert_eq!(check_order(&[2, 0, 3, 1], 6), Ok(()));
        assert_eq!(check_order(&[], 3), Ok(()));
        assert_eq!(check_order(&[0], 1), Ok(()));
        assert_eq!(
            check_order(&[0, 1, 2], 2),
            Err(BadOrder::TooLong { len: 3, n_edges: 2 })
        );
        assert_eq!(
            check_order(&[0, 3, 1], 3),
            Err(BadOrder::OutOfRange { at: 1, edge: 3 })
        );
        assert_eq!(
            check_order(&[1, 0, 1], 3),
            Err(BadOrder::Repeated { at: 2, edge: 1 })
        );
        // Past a word of the bitmap.
        let mut long: Vec<u32> = (0..200).rev().collect();
        assert_eq!(check_order(&long, 200), Ok(()));
        long[7] = 150;
        assert_eq!(
            check_order(&long, 200),
            Err(BadOrder::Repeated { at: 49, edge: 150 })
        );
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn an_order_survives_what_does_not_touch_its_edges() {
        let ordered = Ordered(100);
        // Edges added after it, and taken off again.
        assert!(ordered.keeps(100, &[]));
        assert!(ordered.keeps(130, &[]));
        // An edge added since is removed in place.
        assert!(ordered.keeps(130, &[110..111, 120..125]));
        // An ordered edge is removed, or ordered edges are taken off.
        assert!(!ordered.keeps(130, &[99..100]));
        assert!(!ordered.keeps(130, &[110..111, 3..4]));
        assert!(!ordered.keeps(90, &[]));
        // No order: nothing to lose.
        assert!(Ordered(0).keeps(0, &[0..5]));
        assert!(ordered.keeps(100, &[5..5]));
    }
}
