//! Refinement at rest: everything, drawn in slices and kept.
//!
//! While the view moves, a frame draws what fits in it ([`crate::budget`]).
//! Once it rests, everything is drawn again, and where everything does not
//! fit in a frame that is seconds of a software renderer's time. So it is
//! drawn in slices, into a texture the size of the canvas: the edges a run
//! of the buffer at a time, then the nodes a run at a time, each slice sent
//! only when the GPU has finished the last. A pan, a zoom or any other
//! change stops it: nothing waits for more than one slice. When the last
//! slice is done the texture is put on the canvas, under the highlighted
//! edges and the nodes in front, and it stays:
//!
//! - **At rest**, a frame is the texture and those two small layers, so a
//!   hover or a selection costs a frame of next to nothing.
//! - **While the view moves**, the texture is drawn under the frame's
//!   prefix of the edges and its largest nodes, moved and scaled as the
//!   camera has been since: the whole picture, a little soft where it is
//!   zoomed, until the view rests and it is drawn again.
//!
//! This module is what of that needs no GPU: how large a slice is
//! ([`SlicePace`]) and what a change of flags does to the texture
//! ([`change`]). The renderer draws the slices ([`crate::render`]) and the
//! view keeps the state ([`crate::view`]).
//!
//! # The same pixels
//!
//! The texture is what a frame that draws everything at once puts under
//! its highlighted edges: the background, every edge, and every node not
//! in front, in the same order, with the same blending, into a target of
//! the same format. A slice is a run of the same draw; the blending of a
//! fragment reads what the fragments before it left in the target, whether
//! the same pass drew them or an earlier one. So the frame made of it is
//! the frame drawn at once, byte for byte, and it is put on the canvas
//! texel for texel.
//!
//! A node that comes to the front afterwards (hovered, selected) is drawn
//! in front, as ever, and the copy of it in the texture stays under it: the
//! same disc, covered, but for the rim's pixels, which are blended twice
//! (at most a quarter of the way further from what is under the node to
//! its colour). When it leaves the front again the frame is exact again.
//! A node that was in front when the texture was drawn and no longer is
//! is drawn over the texture, after every other node and not among them.
//! Any other change of flags (a highlight set, a node hidden or pinned)
//! draws the texture again.

use crate::budget::{MIN_UNITS, NODE_UNITS};
use crate::highlight::flag;

/// How long the GPU is to take over a slice, in milliseconds: what a pan
/// or a zoom begun during the refinement waits for at most, and, where
/// drawing holds the page's thread (a software renderer), how long one
/// task holds it. Short of the 50 ms a task may take, with room for a
/// slice that costs twice what the last did.
pub const SLICE_MS: f64 = 16.0;

/// The most a slice grows by from one to the next: the cost of an edge
/// changes along the buffer (the long edges are first), so one slice says
/// only roughly what the next will cost.
const GROW: f64 = 2.0;
/// The most it shrinks by.
const SHRINK: f64 = 0.25;
/// A slice whose end is not seen for this long is taken as done: a
/// backend that never says when the GPU is.
pub const SLICE_TIMEOUT_MS: f64 = 1000.0;

/// What a slice draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Edges,
    Nodes,
}

/// How many edges, and how many nodes, a slice draws: as many as the last
/// slices' costs say take [`SLICE_MS`]. It is kept from one refinement to
/// the next.
#[derive(Debug, Clone, PartialEq)]
pub struct SlicePace {
    edges: f64,
    nodes: f64,
}

impl SlicePace {
    /// Starting from slices of `units` ([`crate::budget`]'s: what a moving
    /// frame draws now), which is little: the slices double until one
    /// takes [`SLICE_MS`].
    pub fn new(units: usize) -> Self {
        let units = units.max(MIN_UNITS) as f64;
        Self {
            edges: units,
            nodes: (units / NODE_UNITS as f64).max(1.0),
        }
    }

    /// How many of `left` a slice of `kind` draws now: never none.
    pub fn slice(&self, kind: Kind, left: usize) -> usize {
        let size = match kind {
            Kind::Edges => self.edges,
            Kind::Nodes => self.nodes,
        };
        (size as usize).max(1).min(left)
    }

    /// A slice of `drawn` of `kind` took the GPU `ms`; `whole` if it was as
    /// large as [`SlicePace::slice`] allowed (the last of a kind is what
    /// was left, and says nothing of a larger one unless it was slow).
    pub fn observe(&mut self, kind: Kind, drawn: usize, whole: bool, ms: f64) {
        if !ms.is_finite() || drawn == 0 {
            return;
        }
        let factor = (SLICE_MS / ms.max(0.25)).clamp(SHRINK, GROW);
        if factor > 1.0 && !whole {
            return;
        }
        let size = (drawn as f64 * factor).max(64.0);
        match kind {
            Kind::Edges => self.edges = size,
            Kind::Nodes => self.nodes = size,
        }
    }
}

/// Where a node with these flags is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Not at all.
    Hidden,
    /// In the pass of the nodes in front, over the texture.
    Front,
    /// In the pass over every node, which the texture holds: as these
    /// flags draw it.
    Base(u8),
}

fn standing(flags: u8) -> Standing {
    if flags & flag::HIDDEN != 0 {
        Standing::Hidden
    } else if flags & flag::FRONT != 0 {
        Standing::Front
    } else {
        Standing::Base(flags)
    }
}

/// What a node's change of flags asks of a texture drawn when its flags
/// were `was`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Nothing: the texture has the node as it is drawn now, or the node is
    /// in neither.
    Same,
    /// The node is in front now: its copy in the texture is under it.
    Covered,
    /// The node was in front and is not: the texture has no copy of it,
    /// and it is drawn over the texture.
    Missing,
    /// The texture is wrong: draw it again.
    Redraw,
}

/// What it means for the texture that a node's flags, `was` when the
/// texture was drawn, are `now`.
pub fn change(was: u8, now: u8) -> Change {
    match (standing(was), standing(now)) {
        (a, b) if a == b => Change::Same,
        (Standing::Base(_), Standing::Front) => Change::Covered,
        (Standing::Front, Standing::Base(_)) => Change::Missing,
        _ => Change::Redraw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_double_until_one_takes_its_time_and_then_hold() {
        let mut pace = SlicePace::new(0);
        assert_eq!(pace.slice(Kind::Edges, usize::MAX), MIN_UNITS);
        // A device that draws 10,000 edges a millisecond.
        let cost = |edges: usize| edges as f64 / 10_000.0;
        let mut sizes = Vec::new();
        for _ in 0..12 {
            let k = pace.slice(Kind::Edges, usize::MAX);
            sizes.push(k);
            pace.observe(Kind::Edges, k, true, cost(k));
        }
        assert_eq!(sizes[1], 2 * sizes[0]);
        assert_eq!(sizes[2], 4 * sizes[0]);
        let last = *sizes.last().expect("twelve");
        assert!((150_000..=170_000).contains(&last), "{sizes:?}");
        // The nodes have a pace of their own.
        assert_eq!(pace.slice(Kind::Nodes, usize::MAX), MIN_UNITS / NODE_UNITS);
    }

    #[test]
    fn a_slow_slice_shrinks_the_next_and_a_short_last_one_grows_nothing() {
        let mut pace = SlicePace::new(1 << 16);
        pace.observe(Kind::Edges, 1 << 16, true, 4.0 * SLICE_MS);
        assert_eq!(pace.slice(Kind::Edges, usize::MAX), 1 << 14);
        // However slow, by a quarter at most.
        pace.observe(Kind::Edges, 1 << 14, true, 100.0 * SLICE_MS);
        assert_eq!(pace.slice(Kind::Edges, usize::MAX), 1 << 12);
        // The last 100 edges, done at once: the next refinement's slices
        // are not 200.
        pace.observe(Kind::Edges, 100, false, 0.1);
        assert_eq!(pace.slice(Kind::Edges, usize::MAX), 1 << 12);
        // But a slow last one counts.
        pace.observe(Kind::Edges, 1000, false, 2.0 * SLICE_MS);
        assert_eq!(pace.slice(Kind::Edges, usize::MAX), 500);
        // Never none, never more than are left.
        assert_eq!(pace.slice(Kind::Edges, 3), 3);
        pace.observe(Kind::Nodes, 1, true, f64::NAN);
        pace.observe(Kind::Nodes, 0, true, 1.0);
        assert!(pace.slice(Kind::Nodes, usize::MAX) >= 1);
    }

    #[test]
    fn a_change_of_flags_is_read_against_what_the_texture_holds() {
        use flag::*;
        // Hovered and selected nodes come to the front and go back.
        assert_eq!(change(0, HOVERED), Change::Covered);
        assert_eq!(change(PINNED, SELECTED | PINNED), Change::Covered);
        assert_eq!(change(0, 0), Change::Same);
        assert_eq!(change(SELECTED, SELECTED | HOVERED), Change::Same);
        assert_eq!(change(HOVERED, 0), Change::Missing);
        assert_eq!(change(SELECTED, DIMMED), Change::Missing);
        // What the pass over every node draws differently, or not at all.
        assert_eq!(change(0, PINNED), Change::Redraw);
        assert_eq!(change(0, DIMMED), Change::Redraw);
        assert_eq!(change(0, HIDDEN), Change::Redraw);
        assert_eq!(change(HIDDEN, 0), Change::Redraw);
        assert_eq!(change(SELECTED, SELECTED | HIDDEN), Change::Redraw);
        assert_eq!(change(HIDDEN, HIDDEN | SELECTED), Change::Same);
    }
}
