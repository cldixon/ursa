//! What the view points at: a flags byte per node (selected, hovered,
//! pinned, dimmed, highlighted, hidden, a route's end) and the highlighted
//! edges.
//!
//! None of this touches the GPU. [`crate::render`] draws it: a halo or a ring
//! from each node's flags, the highlighted edges as wide lines over the
//! others, and, while anything is highlighted, everything else dimmed.
//!
//! The flags are the view's, not the model's: [`HighlightState::set_result`]
//! derives them from a [`GraphResult`]'s highlights and removed nodes.

use std::ops::Range;

use crate::model::GraphResult;

/// The bits of a node's flags byte.
pub mod flag {
    /// The user picked it: a ring in the selection colour.
    pub const SELECTED: u8 = 1 << 0;
    /// The pointer is on it: a thin ring in the selection colour.
    pub const HOVERED: u8 = 1 << 1;
    /// It keeps its place through layouts: a small ring in the pin colour.
    pub const PINNED: u8 = 1 << 2;
    /// Drawn at the dim level whether or not anything is highlighted.
    pub const DIMMED: u8 = 1 << 3;
    /// On a highlighted path, or reached from the seeds: a halo in the
    /// highlight colour.
    pub const HIGHLIGHTED: u8 = 1 << 4;
    /// Not drawn, nor are its edges. It keeps its slot.
    pub const HIDDEN: u8 = 1 << 5;
    /// A seed, or the first or last node of a path: a ring in the highlight
    /// colour.
    pub const SEED: u8 = 1 << 6;

    /// Drawn over everything else, at full strength while the rest is
    /// dimmed, and never smaller than [`super::MIN_FRONT_RADIUS`].
    pub const FRONT: u8 = SELECTED | HOVERED | HIGHLIGHTED | SEED;
    /// A highlight is set: everything not in [`FRONT`] is dimmed.
    pub const DIMS_THE_REST: u8 = HIGHLIGHTED | SEED;
}

/// How strongly the rest is drawn while a highlight is set: 35%.
pub const DIM_LEVEL: f32 = 0.35;

/// How wide a highlighted edge is drawn, in pixels.
pub const HIGHLIGHT_WIDTH: f32 = 3.0;

/// The least radius a node in front ([`flag::FRONT`]) is drawn with, in
/// pixels: a path stays visible where a huge graph's nodes shrink to dots.
pub const MIN_FRONT_RADIUS: f32 = 3.0;

/// How far a node's marks (halo and rings) reach past its disc, in pixels.
pub const MARK_REACH: f32 = 8.0;

/// Changed nodes closer than this are uploaded as one run.
const MERGE_GAP: usize = 8;
/// More runs than this are uploaded as one, from the first to the last.
const MAX_RUNS: usize = 1024;

/// Why a highlight could not be set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadHighlight {
    /// The flags are not one byte per node.
    Flags { got: usize, nodes: usize },
    /// The edges are not `src, dst` pairs.
    OddEdges { len: usize },
    /// A node index past the graph's nodes.
    Node { node: u32, nodes: usize },
    /// The path offsets are not `paths + 1` rising offsets into the nodes.
    Offsets,
}

impl std::fmt::Display for BadHighlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flags { got, nodes } => {
                write!(f, "expected one flags byte per node ({nodes}); got {got}")
            }
            Self::OddEdges { len } => write!(
                f,
                "highlighted edges are src, dst pairs; got {len} values, an odd number"
            ),
            Self::Node { node, nodes } => {
                write!(f, "node {node} is not in a graph of {nodes} nodes")
            }
            Self::Offsets => write!(
                f,
                "path offsets must start at 0, never fall, and end at the number of path nodes"
            ),
        }
    }
}

impl std::error::Error for BadHighlight {}

/// A graph's flags and highlighted edges, and which flags have changed since
/// the renderer last took them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightState {
    flags: Vec<u8>,
    /// The flags as the renderer has them ([`HighlightState::take_dirty`]).
    drawn: Vec<u8>,
    /// `src, dst` per highlighted edge.
    edges: Vec<u32>,
    /// The nodes from here on are new ([`HighlightState::resize`]): the
    /// renderer has no flags for them yet.
    unsent_from: usize,
}

impl HighlightState {
    /// Nothing flagged, nothing highlighted, for a graph of `n_nodes` nodes.
    pub fn new(n_nodes: usize) -> Self {
        Self {
            flags: vec![0; n_nodes],
            drawn: vec![0; n_nodes],
            edges: Vec::new(),
            unsent_from: n_nodes,
        }
    }

    /// As many nodes as `n_nodes`: new ones at the end, unflagged, or the
    /// last ones taken off, and with them the highlighted edges that name
    /// them. The other nodes keep their flags. A new node's flags are
    /// uploaded ([`HighlightState::take_dirty`]) whatever they are.
    ///
    /// Returns whether anything drawn went with the nodes taken off: one of
    /// them was flagged, or a highlighted edge named one.
    pub fn resize(&mut self, n_nodes: usize) -> bool {
        let mut lost = false;
        if n_nodes < self.flags.len() {
            lost = self.flags[n_nodes..].iter().any(|&f| f != 0);
            let gone = |v: u32| v as usize >= n_nodes;
            if self.edges.iter().any(|&v| gone(v)) {
                let kept: Vec<u32> = self
                    .edges
                    .chunks_exact(2)
                    .filter(|e| !gone(e[0]) && !gone(e[1]))
                    .flatten()
                    .copied()
                    .collect();
                self.edges = kept;
                lost = true;
            }
        }
        self.flags.resize(n_nodes, 0);
        self.drawn.resize(n_nodes, 0);
        self.unsent_from = self.unsent_from.min(n_nodes);
        lost
    }

    /// The new nodes ([`HighlightState::resize`]) the renderer has no flags
    /// for yet, as a range, taken as sent: what [`take_dirty`] would give
    /// for them, without reading the other nodes. For when no other flag
    /// changed.
    ///
    /// [`take_dirty`]: HighlightState::take_dirty
    pub fn take_unsent(&mut self) -> Range<usize> {
        let new = std::mem::replace(&mut self.unsent_from, self.flags.len())..self.flags.len();
        self.drawn[new.clone()].copy_from_slice(&self.flags[new.clone()]);
        new
    }

    pub fn n_nodes(&self) -> usize {
        self.flags.len()
    }

    /// Each node's flags ([`flag`]).
    pub fn flags(&self) -> &[u8] {
        &self.flags
    }

    /// The highlighted edges, `src, dst` each, hidden ends included.
    pub fn edges(&self) -> &[u32] {
        &self.edges
    }

    /// The highlighted edges to draw: those with neither end hidden.
    pub fn shown_edges(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.edges
            .chunks_exact(2)
            .map(|e| (e[0], e[1]))
            .filter(|&(s, d)| !self.is(s, flag::HIDDEN) && !self.is(d, flag::HIDDEN))
    }

    fn is(&self, node: u32, bits: u8) -> bool {
        self.flags[node as usize] & bits != 0
    }

    fn any(&self, bits: u8) -> bool {
        self.flags.iter().any(|f| f & bits != 0)
    }

    /// Whether a highlight is set, so the rest is dimmed: a node is
    /// highlighted or a seed, or an edge is highlighted.
    pub fn dims_the_rest(&self) -> bool {
        !self.edges.is_empty() || self.any(flag::DIMS_THE_REST)
    }

    /// Whether any node is drawn in front ([`flag::FRONT`]).
    pub fn any_front(&self) -> bool {
        self.any(flag::FRONT)
    }

    /// The nodes drawn in front: flagged [`flag::FRONT`] and not hidden, in
    /// index order.
    pub fn front_nodes(&self) -> impl Iterator<Item = u32> + '_ {
        self.flags
            .iter()
            .enumerate()
            .filter(|(_, &f)| f & flag::FRONT != 0 && f & flag::HIDDEN == 0)
            .map(|(i, _)| i as u32)
    }

    /// Whether any node is hidden, so its edges must be too.
    pub fn any_hidden(&self) -> bool {
        self.any(flag::HIDDEN)
    }

    fn check_nodes(&self, nodes: &[u32]) -> Result<(), BadHighlight> {
        let n = self.flags.len();
        match nodes.iter().find(|&&v| v as usize >= n) {
            Some(&node) => Err(BadHighlight::Node { node, nodes: n }),
            None => Ok(()),
        }
    }

    /// Every node's flags at once.
    pub fn set_flags(&mut self, flags: &[u8]) -> Result<(), BadHighlight> {
        if flags.len() != self.flags.len() {
            return Err(BadHighlight::Flags {
                got: flags.len(),
                nodes: self.flags.len(),
            });
        }
        self.flags.copy_from_slice(flags);
        Ok(())
    }

    /// Set `bits` on exactly `nodes`: cleared everywhere else. The other
    /// bits are kept.
    pub fn set_only(&mut self, bits: u8, nodes: &[u32]) -> Result<(), BadHighlight> {
        self.check_nodes(nodes)?;
        for f in &mut self.flags {
            *f &= !bits;
        }
        for &v in nodes {
            self.flags[v as usize] |= bits;
        }
        Ok(())
    }

    /// Set or clear `bits` on `nodes`, leaving every other node as it is.
    pub fn set_bits(&mut self, bits: u8, nodes: &[u32], on: bool) -> Result<(), BadHighlight> {
        self.check_nodes(nodes)?;
        for &v in nodes {
            let f = &mut self.flags[v as usize];
            *f = if on { *f | bits } else { *f & !bits };
        }
        Ok(())
    }

    /// The highlighted edges: `src, dst` per edge, in any order. They need
    /// not be edges of the graph.
    pub fn set_edges(&mut self, pairs: &[u32]) -> Result<(), BadHighlight> {
        if !pairs.len().is_multiple_of(2) {
            return Err(BadHighlight::OddEdges { len: pairs.len() });
        }
        self.check_nodes(pairs)?;
        self.edges.clear();
        self.edges.extend_from_slice(pairs);
        Ok(())
    }

    /// Highlight paths and seeds, replacing the highlight there was: the
    /// paths' nodes are highlighted and the edges between consecutive nodes
    /// drawn wide; each path's two ends, and each seed, carry a ring.
    ///
    /// The paths are as UGR holds them: path `p`'s nodes are
    /// `nodes[offsets[p]..offsets[p + 1]]`. No paths is `offsets` empty (or
    /// `[0]`). Selection, hover, pins and hidden nodes are kept.
    pub fn set_paths(
        &mut self,
        offsets: &[u32],
        nodes: &[u32],
        seeds: &[u32],
    ) -> Result<(), BadHighlight> {
        let ends: &[u32] = if offsets.is_empty() { &[0] } else { offsets };
        let rising = ends.windows(2).all(|w| w[0] <= w[1]);
        if ends[0] != 0 || !rising || ends[ends.len() - 1] as usize != nodes.len() {
            return Err(BadHighlight::Offsets);
        }
        self.check_nodes(nodes)?;
        self.check_nodes(seeds)?;
        self.clear_highlight();
        for w in ends.windows(2) {
            let path = &nodes[w[0] as usize..w[1] as usize];
            self.mark_path(path);
            for step in path.windows(2) {
                self.edges.extend_from_slice(step);
            }
        }
        for &s in seeds {
            self.flags[s as usize] |= flag::SEED;
        }
        Ok(())
    }

    fn mark_path(&mut self, path: &[u32]) {
        for &v in path {
            self.flags[v as usize] |= flag::HIGHLIGHTED;
        }
        if let (Some(&first), Some(&last)) = (path.first(), path.last()) {
            self.flags[first as usize] |= flag::SEED;
            self.flags[last as usize] |= flag::SEED;
        }
    }

    /// No node highlighted or a seed, and no edge highlighted. Selection,
    /// hover, pins and hidden nodes are kept.
    pub fn clear_highlight(&mut self) {
        for f in &mut self.flags {
            *f &= !(flag::HIGHLIGHTED | flag::SEED);
        }
        self.edges.clear();
    }

    /// The highlight and the hidden nodes of `result`, replacing those there
    /// were: its paths (their own edges where it names them, else the steps
    /// between consecutive nodes) and seeds as [`HighlightState::set_paths`]
    /// draws them, and its removed nodes hidden. Removed edges are not
    /// highlighted. Selection, hover and pins are kept.
    ///
    /// `result` must have as many node slots as the graph drawn, in the
    /// same order.
    pub fn set_result(&mut self, result: &GraphResult) -> Result<(), BadHighlight> {
        if result.n_nodes() != self.flags.len() {
            return Err(BadHighlight::Flags {
                got: result.n_nodes(),
                nodes: self.flags.len(),
            });
        }
        self.clear_highlight();
        for (i, f) in self.flags.iter_mut().enumerate() {
            *f = if result.node_is_removed(i) {
                *f | flag::HIDDEN
            } else {
                *f & !flag::HIDDEN
            };
        }
        let highlights = result.highlights();
        for path in &highlights.paths {
            self.mark_path(&path.nodes);
            if path.edges.is_empty() {
                for step in path.nodes.windows(2) {
                    self.edges.extend_from_slice(step);
                }
            } else {
                for &e in &path.edges {
                    let e = e as usize;
                    if result.edge_is_shown(e) {
                        self.edges.extend([result.src()[e], result.dst()[e]]);
                    }
                }
            }
        }
        for &s in &highlights.seeds {
            self.flags[s as usize] |= flag::SEED;
        }
        Ok(())
    }

    /// The runs of nodes whose flags changed since this was last called:
    /// what the renderer uploads. Changes a few nodes apart are one run, and
    /// a change scattered over many runs is one run from first to last, so a
    /// selection is a few bytes and a new highlight a few writes.
    pub fn take_dirty(&mut self) -> Vec<Range<usize>> {
        let mut runs: Vec<Range<usize>> = Vec::new();
        let unsent_from = std::mem::replace(&mut self.unsent_from, self.flags.len());
        for (i, (new, old)) in self.flags.iter().zip(&mut self.drawn).enumerate() {
            if new == old && i < unsent_from {
                continue;
            }
            *old = *new;
            match runs.last_mut() {
                Some(run) if i - run.end <= MERGE_GAP => run.end = i + 1,
                _ => runs.push(i..i + 1),
            }
        }
        if runs.len() > MAX_RUNS {
            let whole = runs[0].start..runs[runs.len() - 1].end;
            runs.clear();
            runs.push(whole);
        }
        runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    #[test]
    fn new_nodes_are_unflagged_and_uploaded_and_nodes_taken_off_take_their_edges() {
        let mut h = HighlightState::new(4);
        h.set_bits(flag::SELECTED, &[1], true).unwrap();
        h.set_edges(&[0, 1, 2, 3]).unwrap();
        assert_eq!(h.take_dirty(), vec![1..2]);

        h.resize(6);
        assert_eq!(h.flags(), [0, flag::SELECTED, 0, 0, 0, 0]);
        // The renderer has nothing for the new slots: they are sent, though 0.
        assert_eq!(h.take_dirty(), vec![4..6]);
        assert!(h.take_dirty().is_empty());
        h.set_bits(flag::PINNED, &[5], true).unwrap();
        h.set_edges(&[0, 1, 4, 5, 2, 3]).unwrap();
        assert_eq!(h.take_dirty(), vec![5..6]);

        // Off again: the edge that named them goes, the others stay.
        assert!(h.resize(4), "a pinned node and an edge went");
        assert_eq!(h.flags(), [0, flag::SELECTED, 0, 0]);
        assert_eq!(h.edges(), [0, 1, 2, 3]);
        assert!(h.take_dirty().is_empty());
        // The slots come back unflagged, and are sent again.
        assert!(!h.resize(6));
        assert_eq!(h.flags()[4..], [0, 0]);
        assert_eq!(h.take_unsent(), 4..6);
        assert_eq!(h.take_unsent(), 6..6);
        assert!(h.take_dirty().is_empty());
        // Unflagged nodes go without a trace.
        assert!(!h.resize(5));
    }

    #[test]
    fn the_themes_marks_stand_out_from_the_background() {
        use crate::scene::{contrast, Theme};
        assert!((contrast(0x000000ff, 0xffffffff) - 21.0).abs() < 0.01);
        assert!((contrast(0x777777ff, 0x777777ff) - 1.0).abs() < 1e-6);
        for theme in [Theme::light(), Theme::dark()] {
            for (name, color) in [
                ("highlight", theme.highlight),
                ("selection", theme.selection),
                ("pin", theme.pin),
            ] {
                let ratio = contrast(color, theme.background);
                assert!(ratio >= 3.0, "{name} {color:08x}: {ratio:.2}:1");
            }
            // The three are told apart from each other and from the nodes.
            assert_ne!(theme.highlight, theme.selection);
            assert_ne!(theme.selection, theme.pin);
            assert_ne!(theme.highlight, theme.node);
        }
    }

    #[test]
    fn nothing_is_flagged_at_first() {
        let mut h = HighlightState::new(5);
        assert_eq!(h.flags(), &[0; 5]);
        assert!(!h.dims_the_rest() && !h.any_front() && !h.any_hidden());
        assert!(h.take_dirty().is_empty());
        assert_eq!(h.shown_edges().count(), 0);
    }

    #[test]
    fn a_selection_of_one_node_is_one_dirty_byte() {
        let mut h = HighlightState::new(1000);
        h.set_only(flag::SELECTED, &[700]).unwrap();
        assert_eq!(h.take_dirty(), vec![700..701]);
        // Nothing changed since: nothing to upload.
        assert!(h.take_dirty().is_empty());
        // Moving it uploads the old node and the new, not what lies between.
        h.set_only(flag::SELECTED, &[3]).unwrap();
        assert_eq!(h.take_dirty(), vec![3..4, 700..701]);
        assert!(h.any_front());
        assert_eq!(h.front_nodes().collect::<Vec<_>>(), vec![3]);
        // Hidden, it is not drawn in front either.
        h.set_bits(flag::HIDDEN, &[3], true).unwrap();
        assert_eq!(h.front_nodes().count(), 0);
        h.set_bits(flag::HIDDEN, &[3], false).unwrap();
        h.take_dirty();
        // A selection alone does not dim the rest.
        assert!(!h.dims_the_rest());
    }

    #[test]
    fn near_changes_merge_and_scattered_ones_fall_back_to_one_run() {
        let mut h = HighlightState::new(10_000);
        h.set_bits(flag::PINNED, &[10, 14, 40], true).unwrap();
        assert_eq!(h.take_dirty(), vec![10..15, 40..41]);
        // A path's worth of scattered nodes is still a write each.
        let mut h = HighlightState::new(1_000_000);
        let path: Vec<u32> = (0..100).map(|i| i * 10_000).collect();
        h.set_bits(flag::HIGHLIGHTED, &path, true).unwrap();
        assert_eq!(h.take_dirty().len(), 100);
        // Thousands are one write, from the first to the last.
        let scattered: Vec<u32> = (0..2000).map(|i| i * 500).collect();
        h.set_bits(flag::PINNED, &scattered, true).unwrap();
        assert_eq!(h.take_dirty(), vec![0..999_501]);
    }

    #[test]
    fn setting_bits_keeps_the_others() {
        let mut h = HighlightState::new(4);
        h.set_bits(flag::PINNED, &[1, 2], true).unwrap();
        h.set_only(flag::SELECTED, &[2, 3]).unwrap();
        assert_eq!(
            h.flags(),
            &[
                0,
                flag::PINNED,
                flag::PINNED | flag::SELECTED,
                flag::SELECTED
            ]
        );
        h.set_only(flag::SELECTED, &[]).unwrap();
        h.set_bits(flag::PINNED, &[1], false).unwrap();
        assert_eq!(h.flags(), &[0, 0, flag::PINNED, 0]);
    }

    #[test]
    fn a_path_highlights_its_nodes_rings_its_ends_and_dims_the_rest() {
        let mut h = HighlightState::new(8);
        h.set_only(flag::SELECTED, &[7]).unwrap();
        h.set_paths(&[0, 3, 5], &[0, 1, 2, 4, 5], &[6]).unwrap();
        let (hl, seed) = (flag::HIGHLIGHTED, flag::SEED);
        assert_eq!(
            h.flags(),
            &[
                hl | seed,
                hl,
                hl | seed,
                0,
                hl | seed,
                hl | seed,
                seed,
                flag::SELECTED
            ]
        );
        assert_eq!(h.edges(), &[0, 1, 1, 2, 4, 5]);
        assert!(h.dims_the_rest());
        // A new highlight replaces the old one; the selection stays.
        h.set_paths(&[], &[], &[3]).unwrap();
        assert_eq!(h.flags(), &[0, 0, 0, seed, 0, 0, 0, flag::SELECTED]);
        assert!(h.edges().is_empty() && h.dims_the_rest());
        h.clear_highlight();
        assert!(!h.dims_the_rest());
        assert_eq!(h.flags()[7], flag::SELECTED);
    }

    #[test]
    fn a_highlighted_edge_alone_dims_the_rest() {
        let mut h = HighlightState::new(3);
        h.set_edges(&[0, 2]).unwrap();
        assert!(h.dims_the_rest());
        assert_eq!(h.flags(), &[0; 3]);
    }

    #[test]
    fn an_edge_with_a_hidden_end_is_not_drawn() {
        let mut h = HighlightState::new(4);
        h.set_edges(&[0, 1, 1, 2, 2, 3]).unwrap();
        h.set_only(flag::HIDDEN, &[1]).unwrap();
        assert!(h.any_hidden());
        assert_eq!(h.shown_edges().collect::<Vec<_>>(), vec![(2, 3)]);
        // Shown again, its edges are back: they were kept.
        h.set_only(flag::HIDDEN, &[]).unwrap();
        assert_eq!(h.shown_edges().count(), 3);
    }

    #[test]
    fn bad_input_is_refused_and_changes_nothing() {
        let mut h = HighlightState::new(3);
        let before = h.clone();
        let text = |r: Result<(), BadHighlight>| r.unwrap_err().to_string();
        assert_eq!(
            text(h.set_flags(&[0; 2])),
            "expected one flags byte per node (3); got 2"
        );
        assert_eq!(
            text(h.set_edges(&[0, 1, 2])),
            "highlighted edges are src, dst pairs; got 3 values, an odd number"
        );
        assert_eq!(
            text(h.set_edges(&[0, 3])),
            "node 3 is not in a graph of 3 nodes"
        );
        assert_eq!(
            text(h.set_only(flag::SELECTED, &[9])),
            "node 9 is not in a graph of 3 nodes"
        );
        for (offsets, nodes) in [
            (&[1u32, 2][..], &[0u32, 1][..]),
            (&[0, 2, 1, 2], &[0, 1]),
            (&[0, 1], &[0, 1]),
            (&[], &[0]),
        ] {
            assert_eq!(
                text(h.set_paths(offsets, nodes, &[])),
                "path offsets must start at 0, never fall, and end at the number of path nodes"
            );
        }
        assert_eq!(
            text(h.set_paths(&[0, 1], &[5], &[])),
            "node 5 is not in a graph of 3 nodes"
        );
        assert_eq!(
            text(h.set_paths(&[], &[], &[4])),
            "node 4 is not in a graph of 3 nodes"
        );
        assert_eq!(h, before);
    }

    /// Five nodes in a line, 0-1-2-3-4, plus an edge 0-4.
    fn line() -> GraphResult {
        GraphResult::build(
            None,
            Provenance::default(),
            &GraphDelta::new(vec![
                Op::AddNodeLabel(NodeLabel::new("n", KeyType::Int)),
                Op::AddEdgeLabel("e".into()),
                Op::AddNodes(NodeBatch::of_label(0, [0i64, 1, 2, 3, 4])),
                Op::AddEdges(EdgeBatch::of_label(
                    0,
                    vec![0, 1, 2, 3, 0],
                    vec![1, 2, 3, 4, 4],
                )),
            ]),
        )
        .unwrap()
    }

    fn with_highlights(result: &mut GraphResult, new: Highlights) {
        let old = result.highlights().clone();
        result
            .apply(&GraphDelta::new(vec![Op::SetHighlights { old, new }]))
            .unwrap();
    }

    #[test]
    fn a_results_highlights_and_removed_nodes_set_the_flags() {
        let mut result = line();
        with_highlights(
            &mut result,
            Highlights {
                // One path by its nodes alone, one naming its edges (0-4).
                paths: vec![
                    Path {
                        nodes: vec![1, 2, 3],
                        edges: vec![],
                    },
                    Path {
                        nodes: vec![0, 4],
                        edges: vec![4],
                    },
                ],
                seeds: vec![2],
                distance: None,
            },
        );
        let mut h = HighlightState::new(5);
        h.set_bits(flag::PINNED, &[3], true).unwrap();
        h.set_result(&result).unwrap();
        let (hl, seed) = (flag::HIGHLIGHTED, flag::SEED);
        assert_eq!(
            h.flags(),
            &[
                hl | seed,
                hl | seed,
                hl | seed,
                hl | seed | flag::PINNED,
                hl | seed
            ]
        );
        assert_eq!(h.edges(), &[1, 2, 2, 3, 0, 4]);

        // A removed node is hidden, and a path edge at it is not drawn.
        result
            .apply(&GraphDelta::new(vec![Op::RemoveNodes(vec![4])]))
            .unwrap();
        h.set_result(&result).unwrap();
        assert_eq!(h.flags()[4] & flag::HIDDEN, flag::HIDDEN);
        assert_eq!(h.edges(), &[1, 2, 2, 3]);
        // Restored, it shows again; a result with no highlight clears it.
        result
            .apply(&GraphDelta::new(vec![Op::RestoreNodes(vec![4])]))
            .unwrap();
        with_highlights(&mut result, Highlights::default());
        h.set_result(&result).unwrap();
        assert_eq!(h.flags(), &[0, 0, 0, flag::PINNED, 0]);
        assert!(!h.dims_the_rest());

        let text = HighlightState::new(4)
            .set_result(&result)
            .unwrap_err()
            .to_string();
        assert_eq!(text, "expected one flags byte per node (4); got 5");
    }
}
