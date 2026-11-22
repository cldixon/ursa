//! What the user has pointed at and picked: the selection set, the selected
//! edge and the hovered node. No GPU and no engine: a front end tells it
//! what was clicked, and it says what is selected now, which the view draws
//! ([`flag::SELECTED`], [`flag::HOVERED`] and the selection's edges).
//!
//! An edge is picked by [`nearest_edge`], when no node is under the pointer.
//!
//! [`flag::SELECTED`]: crate::highlight::flag::SELECTED
//! [`flag::HOVERED`]: crate::highlight::flag::HOVERED

use std::collections::HashSet;

/// The selected nodes, as dense node indexes, in the order they were
/// selected. No node is in it twice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// In the order selected: the last is the one selected last.
    order: Vec<u32>,
    members: HashSet<u32>,
}

impl Selection {
    /// Nothing selected.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many nodes are selected.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn contains(&self, node: u32) -> bool {
        self.members.contains(&node)
    }

    /// The selected nodes, in the order they were selected.
    pub fn nodes(&self) -> &[u32] {
        &self.order
    }

    /// The node selected last, if any.
    pub fn last(&self) -> Option<u32> {
        self.order.last().copied()
    }

    /// Select exactly `nodes`, in that order (a node named twice counts
    /// once, where it is first). Returns whether the selection changed;
    /// the same nodes in another order is a change.
    pub fn set(&mut self, nodes: &[u32]) -> bool {
        let mut members = HashSet::with_capacity(nodes.len());
        let order: Vec<u32> = nodes
            .iter()
            .copied()
            .filter(|&v| members.insert(v))
            .collect();
        let changed = order != self.order;
        self.order = order;
        self.members = members;
        changed
    }

    /// Add `node` to the selection. Returns whether it was not in it.
    pub fn add(&mut self, node: u32) -> bool {
        let new = self.members.insert(node);
        if new {
            self.order.push(node);
        }
        new
    }

    /// Take `node` out of the selection. Returns whether it was in it.
    pub fn remove(&mut self, node: u32) -> bool {
        let was = self.members.remove(&node);
        if was {
            self.order.retain(|&v| v != node);
        }
        was
    }

    /// Add `node` if it is not selected, take it out if it is. Returns
    /// whether it is selected now.
    pub fn toggle(&mut self, node: u32) -> bool {
        if self.remove(node) {
            false
        } else {
            self.add(node)
        }
    }

    /// Select nothing. Returns whether anything was selected.
    pub fn clear(&mut self) -> bool {
        let any = !self.order.is_empty();
        self.order.clear();
        self.members.clear();
        any
    }

    /// Keep only the nodes `keep` says to, in their order. Returns whether
    /// any went: for when nodes leave the view (hidden, or removed by a
    /// delta).
    pub fn retain(&mut self, mut keep: impl FnMut(u32) -> bool) -> bool {
        let before = self.order.len();
        self.order.retain(|&v| keep(v));
        if self.order.len() == before {
            return false;
        }
        self.members = self.order.iter().copied().collect();
        true
    }

    /// A click: on a node (`hit`) or on empty space (`None`), plain or with
    /// Shift held (`extend`).
    ///
    /// - A plain click on a node selects it alone.
    /// - A plain click on empty space clears the selection.
    /// - With `extend`, a click on a node adds it, or takes it out if it
    ///   was selected; on empty space it changes nothing, so a slip of the
    ///   pointer does not lose a selection built up one node at a time.
    ///
    /// Returns whether the selection changed.
    pub fn click(&mut self, hit: Option<u32>, extend: bool) -> bool {
        match (hit, extend) {
            (Some(node), false) => self.set(&[node]),
            (Some(node), true) => {
                self.toggle(node);
                true
            }
            (None, false) => self.clear(),
            (None, true) => false,
        }
    }

    /// The edges with a selected end, of the edges `(src[i], dst[i])`: `src,
    /// dst` for each, in edge order, as the highlight layer takes them. An
    /// edge between two selected nodes is given once. It reads every edge
    /// once, and nothing when the selection is empty.
    pub fn incident_edges(&self, src: &[u32], dst: &[u32]) -> Vec<u32> {
        if self.is_empty() {
            return Vec::new();
        }
        src.iter()
            .zip(dst)
            .filter(|&(s, d)| self.contains(*s) || self.contains(*d))
            .flat_map(|(&s, &d)| [s, d])
            .collect()
    }
}

/// The edge nearest the point `at`, of the edges `(src[i], dst[i])` between
/// nodes at `(x[v], y[v])`: its index, if the point is within `tolerance` of
/// its segment. Of edges equally near (parallel edges lie on one segment),
/// the first. `skip(i)` says edge `i` is not drawn (it was removed, or a node
/// it joins is hidden), and is asked only of an edge that would be picked.
/// The point, the positions and the tolerance are in one unit: a view gives
/// the pointer and 4 px in world units.
///
/// A linear pass over the edges, each a few multiplications, after a test
/// against the edge's bounding box that most edges fail: a click's cost, not
/// a pointer move's.
pub fn nearest_edge(
    src: &[u32],
    dst: &[u32],
    x: &[f32],
    y: &[f32],
    skip: impl Fn(u32) -> bool,
    at: (f32, f32),
    tolerance: f32,
) -> Option<u32> {
    let (px, py) = at;
    let mut best: Option<(u32, f32)> = None;
    let mut reach = tolerance * tolerance;
    for (i, (&s, &d)) in src.iter().zip(dst).enumerate() {
        let (Some(&ax), Some(&ay), Some(&bx), Some(&by)) = (
            x.get(s as usize),
            y.get(s as usize),
            x.get(d as usize),
            y.get(d as usize),
        ) else {
            continue;
        };
        // Outside the segment's box grown by the tolerance: too far.
        if px < ax.min(bx) - tolerance
            || px > ax.max(bx) + tolerance
            || py < ay.min(by) - tolerance
            || py > ay.max(by) + tolerance
        {
            continue;
        }
        let (ex, ey) = (bx - ax, by - ay);
        let len2 = ex * ex + ey * ey;
        // How far along the segment the nearest point is; a loop is a point.
        let t = if len2 > 0.0 {
            (((px - ax) * ex + (py - ay) * ey) / len2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (qx, qy) = (ax + t * ex - px, ay + t * ey - py);
        let d2 = qx * qx + qy * qy;
        // NaN positions compare false, and are never picked.
        let nearer = match best {
            None => d2 <= reach,
            Some(_) => d2 < reach,
        };
        if nearer && !skip(i as u32) {
            best = Some((i as u32, d2));
            reach = d2;
        }
    }
    best.map(|(i, _)| i)
}

/// A selected edge: its index, and the nodes it joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgePick {
    pub edge: u32,
    pub src: u32,
    pub dst: u32,
}

/// The selection, the selected edge and the node under the pointer. Nodes
/// are selected or an edge is, never both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interact {
    selection: Selection,
    edge: Option<EdgePick>,
    hovered: Option<u32>,
}

impl Interact {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// The selection, to change. The caller draws what it changed. A
    /// selected edge stays: [`Self::click`] and [`Self::select_edge`] keep
    /// the two apart.
    pub fn selection_mut(&mut self) -> &mut Selection {
        &mut self.selection
    }

    /// The selected edge, if any.
    pub fn edge(&self) -> Option<EdgePick> {
        self.edge
    }

    /// A click on a node or on empty space, as [`Selection::click`] reads
    /// it; the selected edge is deselected, unless the click is on empty
    /// space with Shift held, which changes nothing. Returns whether
    /// anything changed.
    pub fn click(&mut self, hit: Option<u32>, extend: bool) -> bool {
        let edge = (hit.is_some() || !extend) && self.edge.take().is_some();
        self.selection.click(hit, extend) || edge
    }

    /// Select `edge` alone, the selected nodes with it no longer, or no
    /// edge (`None`, which leaves the nodes). Returns whether anything
    /// changed.
    pub fn select_edge(&mut self, edge: Option<EdgePick>) -> bool {
        let changed = self.edge != edge;
        self.edge = edge;
        (edge.is_some() && self.selection.clear()) || changed
    }

    /// Select nothing: no node and no edge. Returns whether anything was
    /// selected.
    pub fn clear(&mut self) -> bool {
        let edge = self.edge.take().is_some();
        self.selection.clear() || edge
    }

    /// The node under the pointer, if any.
    pub fn hovered(&self) -> Option<u32> {
        self.hovered
    }

    /// The pointer is over `node`, or over none. Returns whether that is
    /// news, so a pointer moving within one node costs nothing more.
    pub fn hover(&mut self, node: Option<u32>) -> bool {
        let changed = self.hovered != node;
        self.hovered = node;
        changed
    }

    /// Drop the nodes `keep` does not keep from the selection and the
    /// hover, and the selected edge if either of its ends goes. Returns
    /// whether (the selection or the edge, the hover) changed.
    pub fn retain(&mut self, mut keep: impl FnMut(u32) -> bool) -> (bool, bool) {
        let mut selection = self.selection.retain(&mut keep);
        if self.edge.is_some_and(|e| !keep(e.src) || !keep(e.dst)) {
            self.edge = None;
            selection = true;
        }
        let hover = self.hovered.is_some_and(|v| !keep(v));
        if hover {
            self.hovered = None;
        }
        (selection, hover)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_selects_one_node_and_another_click_replaces_it() {
        let mut s = Selection::new();
        assert!(s.click(Some(33), false));
        assert_eq!(s.nodes(), [33]);
        assert!(s.click(Some(0), false));
        assert_eq!(s.nodes(), [0]);
        assert!(!s.contains(33));
        // The same node again is no change.
        assert!(!s.click(Some(0), false));
        assert_eq!(s.last(), Some(0));
    }

    #[test]
    fn shift_click_adds_and_takes_out() {
        let mut s = Selection::new();
        s.click(Some(33), false);
        assert!(s.click(Some(0), true));
        assert_eq!(s.nodes(), [33, 0]);
        assert_eq!(s.len(), 2);
        assert!(s.click(Some(2), true));
        assert_eq!(s.nodes(), [33, 0, 2]);
        // Shift+click on a selected node takes it out, keeping the order.
        assert!(s.click(Some(0), true));
        assert_eq!(s.nodes(), [33, 2]);
        assert!(!s.contains(0));
        // A plain click on one of several selects it alone.
        assert!(s.click(Some(2), false));
        assert_eq!(s.nodes(), [2]);
    }

    #[test]
    fn a_click_on_empty_space_clears_but_not_with_shift() {
        let mut s = Selection::new();
        assert!(!s.click(None, false), "nothing to clear");
        s.click(Some(1), false);
        s.click(Some(2), true);
        assert!(!s.click(None, true));
        assert_eq!(s.nodes(), [1, 2]);
        assert!(s.click(None, false));
        assert!(s.is_empty());
        assert_eq!(s.last(), None);
        assert!(!s.contains(1));
    }

    #[test]
    fn set_keeps_order_and_drops_repeats() {
        let mut s = Selection::new();
        assert!(s.set(&[5, 3, 5, 9, 3]));
        assert_eq!(s.nodes(), [5, 3, 9]);
        assert!(!s.set(&[5, 3, 9]));
        assert!(s.set(&[3, 5, 9]), "another order is a change");
        assert!(s.set(&[]));
        assert!(!s.clear());
    }

    #[test]
    fn add_remove_and_toggle_say_what_happened() {
        let mut s = Selection::new();
        assert!(s.add(4));
        assert!(!s.add(4));
        assert_eq!(s.len(), 1);
        assert!(!s.remove(7));
        assert!(s.remove(4));
        assert!(s.toggle(4));
        assert!(!s.toggle(4));
        assert!(s.is_empty());
    }

    #[test]
    fn retain_drops_nodes_that_left() {
        let mut s = Selection::new();
        s.set(&[1, 8, 3, 9]);
        assert!(!s.retain(|v| v < 10));
        assert!(s.retain(|v| v < 8));
        assert_eq!(s.nodes(), [1, 3]);
        assert!(!s.contains(8) && s.contains(3));
        // A node that left can be selected again.
        assert!(s.add(8));
    }

    #[test]
    fn incident_edges_are_each_edge_with_a_selected_end_once() {
        // A path 0-1-2-3 and a chord 0-2.
        let (src, dst) = ([0, 1, 2, 0], [1, 2, 3, 2]);
        let mut s = Selection::new();
        assert!(s.incident_edges(&src, &dst).is_empty());
        s.set(&[1]);
        assert_eq!(s.incident_edges(&src, &dst), [0, 1, 1, 2]);
        // Both ends selected: the edge 1-2 is still given once.
        s.add(2);
        assert_eq!(s.incident_edges(&src, &dst), [0, 1, 1, 2, 2, 3, 0, 2]);
        // A node with no edges has none.
        s.set(&[7]);
        assert!(s.incident_edges(&src, &dst).is_empty());
    }

    #[test]
    fn hover_reports_only_a_change() {
        let mut i = Interact::new();
        assert!(!i.hover(None));
        assert!(i.hover(Some(3)));
        assert!(!i.hover(Some(3)));
        assert!(i.hover(Some(4)));
        assert!(i.hover(None));
        assert_eq!(i.hovered(), None);
    }

    #[test]
    fn retain_drops_the_hover_with_its_node() {
        let mut i = Interact::new();
        i.selection_mut().set(&[2, 6]);
        i.hover(Some(6));
        assert_eq!(i.retain(|v| v < 9), (false, false));
        assert_eq!(i.retain(|v| v < 6), (true, true));
        assert_eq!(i.hovered(), None);
        assert_eq!(i.selection().nodes(), [2]);
    }

    /// A square 0-1-2-3 with a diagonal 0-2, a unit a side.
    fn square() -> ([u32; 5], [u32; 5], [f32; 4], [f32; 4]) {
        (
            [0, 1, 2, 3, 0],
            [1, 2, 3, 0, 2],
            [0.0, 1.0, 1.0, 0.0],
            [0.0, 0.0, 1.0, 1.0],
        )
    }

    #[test]
    fn the_nearest_edge_within_the_tolerance_is_picked() {
        let (src, dst, x, y) = square();
        let pick = |at, tolerance| nearest_edge(&src, &dst, &x, &y, |_| false, at, tolerance);
        // Just under the bottom side.
        assert_eq!(pick((0.5, -0.03), 0.04), Some(0));
        // On the diagonal.
        assert_eq!(pick((0.25, 0.25), 0.04), Some(4));
        // Between the bottom side and the diagonal, nearer the diagonal.
        assert_eq!(pick((0.5, 0.3), 0.5), Some(4));
        assert_eq!(pick((0.5, 0.1), 0.5), Some(0));
        // Too far from every edge.
        assert_eq!(pick((0.75, 0.25), 0.04), None);
        // Exactly at the tolerance counts.
        assert_eq!(pick((0.5, -0.25), 0.25), Some(0));
        assert_eq!(pick((0.5, -0.26), 0.25), None);
    }

    #[test]
    fn an_edge_is_a_segment_not_a_line() {
        let (src, dst, x, y) = ([0u32], [1u32], [0.0f32, 1.0], [0.0f32, 0.0]);
        let pick = |at| nearest_edge(&src, &dst, &x, &y, |_| false, at, 0.1);
        // Past its end, on its line: only the end itself is near.
        assert_eq!(pick((1.5, 0.0)), None);
        assert_eq!(pick((1.05, 0.0)), Some(0));
        assert_eq!(pick((-0.05, 0.05)), Some(0));
    }

    #[test]
    fn parallel_edges_pick_the_first_that_is_drawn() {
        // Two edges 0-1, one each way, and a loop at 2.
        let (src, dst) = ([0u32, 1, 2], [1u32, 0, 2]);
        let (x, y) = ([0.0f32, 1.0, 5.0], [0.0f32, 0.0, 5.0]);
        let at = (0.5, 0.0);
        assert_eq!(
            nearest_edge(&src, &dst, &x, &y, |_| false, at, 0.1),
            Some(0)
        );
        // The first is not drawn: the second, as near, is picked.
        assert_eq!(
            nearest_edge(&src, &dst, &x, &y, |e| e == 0, at, 0.1),
            Some(1)
        );
        assert_eq!(nearest_edge(&src, &dst, &x, &y, |e| e < 2, at, 0.1), None);
        // A loop is picked at its node.
        assert_eq!(
            nearest_edge(&src, &dst, &x, &y, |_| false, (5.0, 5.05), 0.1),
            Some(2)
        );
        // An end past the positions, or a position that is not a number, is
        // not there to pick.
        assert_eq!(
            nearest_edge(&[0, 0], &[9, 1], &x, &y, |_| false, at, 0.1),
            Some(1)
        );
        let nan = [0.0f32, f32::NAN, 5.0];
        assert_eq!(nearest_edge(&src, &dst, &nan, &y, |_| false, at, 0.1), None);
        assert_eq!(nearest_edge(&[], &[], &x, &y, |_| false, at, 0.1), None);
    }

    #[test]
    fn nodes_are_selected_or_an_edge_is() {
        let edge = EdgePick {
            edge: 4,
            src: 0,
            dst: 2,
        };
        let mut i = Interact::new();
        i.click(Some(1), false);
        i.click(Some(3), true);
        // An edge replaces the nodes.
        assert!(i.select_edge(Some(edge)));
        assert!(i.selection().is_empty());
        assert_eq!(i.edge(), Some(edge));
        assert!(!i.select_edge(Some(edge)), "the same edge is no change");
        // Shift on empty space changes nothing; a node replaces the edge.
        assert!(!i.click(None, true));
        assert_eq!(i.edge(), Some(edge));
        assert!(i.click(Some(1), true));
        assert_eq!(i.edge(), None);
        assert_eq!(i.selection().nodes(), [1]);
        // A click on empty space deselects an edge.
        i.select_edge(Some(edge));
        assert!(i.click(None, false));
        assert_eq!(i.edge(), None);
        assert!(!i.click(None, false));
        // Clear drops both kinds.
        i.select_edge(Some(edge));
        assert!(i.clear());
        assert!(!i.clear());
        i.click(Some(2), false);
        assert!(i.clear());
        // No edge leaves the nodes.
        i.click(Some(2), false);
        assert!(!i.select_edge(None));
        assert_eq!(i.selection().nodes(), [2]);
    }

    #[test]
    fn retain_drops_an_edge_with_an_end_that_left() {
        let mut i = Interact::new();
        i.select_edge(Some(EdgePick {
            edge: 0,
            src: 2,
            dst: 6,
        }));
        assert_eq!(i.retain(|v| v < 9), (false, false));
        assert_eq!(i.retain(|v| v < 6), (true, false));
        assert_eq!(i.edge(), None);
    }
}
