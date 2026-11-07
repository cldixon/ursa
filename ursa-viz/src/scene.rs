//! What the view shows, apart from how it is drawn: the graph, its live
//! layout, each node's size and colour, and the camera.
//!
//! None of this touches the GPU, so it is tested natively and is the same on
//! every platform. [`crate::render`] draws it.

use std::sync::Arc;

use ursa_core::algo::{Fa2Params, Fa2State};
use ursa_core::topology::Topology;

/// A graph to show: `n_nodes` nodes, and one edge per `(src[i], dst[i])`, as
/// dense node indexes below `n_nodes`.
///
/// Dense indexes keep the view independent of where the graph came from: the
/// SQL shell maps its result's `src`/`dst` ids to them, and the DataFrame API
/// already has them in its topology.
#[derive(Debug)]
pub struct Graph {
    topology: Arc<Topology>,
    src: Vec<u32>,
    dst: Vec<u32>,
}

/// An edge that names a node index at or past the node count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadEdge {
    pub edge: usize,
    pub node: u32,
    pub n_nodes: usize,
}

impl std::fmt::Display for BadEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "edge {} names node {}, but the graph has {} nodes",
            self.edge, self.node, self.n_nodes
        )
    }
}

impl std::error::Error for BadEdge {}

impl Graph {
    pub fn new(n_nodes: usize, src: Vec<u32>, dst: Vec<u32>) -> Result<Self, BadEdge> {
        assert_eq!(
            src.len(),
            dst.len(),
            "src and dst must have one entry per edge"
        );
        for (edge, (&s, &d)) in src.iter().zip(&dst).enumerate() {
            for node in [s, d] {
                if node as usize >= n_nodes {
                    return Err(BadEdge {
                        edge,
                        node,
                        n_nodes,
                    });
                }
            }
        }
        let topology = Arc::new(Topology::build(n_nodes, src.clone(), dst.clone()));
        Ok(Self { topology, src, dst })
    }

    pub fn n_nodes(&self) -> usize {
        self.topology.n_nodes()
    }

    pub fn n_edges(&self) -> usize {
        self.src.len()
    }

    /// The edges as one index list, `src, dst` per edge: a line list over the
    /// node positions.
    pub fn line_indices(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.src.len() * 2);
        for (&s, &d) in self.src.iter().zip(&self.dst) {
            out.push(s);
            out.push(d);
        }
        out
    }

    /// Each node's undirected degree (distinct neighbours, self-loops left out).
    pub fn degrees(&self) -> Vec<u32> {
        let adj = self.topology.undirected();
        (0..self.n_nodes() as u32)
            .map(|u| adj.degree(u) as u32)
            .collect()
    }

    pub fn topology(&self) -> &Arc<Topology> {
        &self.topology
    }
}

/// ForceAtlas2, run a few iterations at a time so the view can draw it as it
/// converges. The positions are `Fa2State`'s, so a layout stepped here is the
/// same as `layout_fa2` with as many iterations.
#[derive(Debug)]
pub struct LiveLayout {
    topology: Arc<Topology>,
    state: Fa2State,
}

impl LiveLayout {
    pub fn new(graph: &Graph, params: Fa2Params) -> Self {
        let topology = Arc::clone(graph.topology());
        let state = Fa2State::new(topology.undirected(), params);
        Self { topology, state }
    }

    /// Run `iterations` more iterations.
    pub fn step(&mut self, iterations: u32) {
        self.state.step(self.topology.undirected(), iterations);
    }

    pub fn iterations(&self) -> u32 {
        self.state.iterations()
    }

    /// The positions now: `x` and `y`, one per node.
    pub fn positions(&self) -> (&[f32], &[f32]) {
        self.state.positions()
    }
}

/// The smallest box holding every position: `(min_x, min_y, max_x, max_y)`,
/// or `None` for no nodes (or none finite).
pub fn bounds(x: &[f32], y: &[f32]) -> Option<(f32, f32, f32, f32)> {
    let mut b: Option<(f32, f32, f32, f32)> = None;
    for (&px, &py) in x.iter().zip(y) {
        if !(px.is_finite() && py.is_finite()) {
            continue;
        }
        b = Some(match b {
            None => (px, py, px, py),
            Some((a, c, d, e)) => (a.min(px), c.min(py), d.max(px), e.max(py)),
        });
    }
    b
}

/// A 2-D camera: the world point at the centre of the canvas, and how many
/// pixels a world unit spans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub center: (f32, f32),
    pub pixels_per_unit: f32,
    /// The canvas size, in pixels.
    pub viewport: (f32, f32),
}

impl Camera {
    pub fn new(width: f32, height: f32) -> Self {
        Self {
            center: (0.0, 0.0),
            pixels_per_unit: 1.0,
            viewport: (width.max(1.0), height.max(1.0)),
        }
    }

    /// Show the whole box `(min_x, min_y, max_x, max_y)`, with a margin.
    pub fn fit(&mut self, (x0, y0, x1, y1): (f32, f32, f32, f32)) {
        self.center = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
        let (w, h) = ((x1 - x0).max(1e-6), (y1 - y0).max(1e-6));
        let margin = 0.92;
        self.pixels_per_unit = (self.viewport.0 / w).min(self.viewport.1 / h) * margin;
    }

    /// Move the view by a drag of `(dx, dy)` pixels (y down, as on screen).
    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.center.0 -= dx / self.pixels_per_unit;
        self.center.1 += dy / self.pixels_per_unit;
    }

    /// Zoom by `factor` (above 1 is in) around the pixel `(px, py)`, which
    /// stays over the same world point.
    pub fn zoom(&mut self, factor: f32, px: f32, py: f32) {
        let before = self.to_world(px, py);
        self.pixels_per_unit = (self.pixels_per_unit * factor).clamp(1e-6, 1e9);
        let after = self.to_world(px, py);
        self.center.0 += before.0 - after.0;
        self.center.1 += before.1 - after.1;
    }

    /// The world point under the pixel `(px, py)` (origin top left).
    pub fn to_world(&self, px: f32, py: f32) -> (f32, f32) {
        (
            self.center.0 + (px - self.viewport.0 * 0.5) / self.pixels_per_unit,
            self.center.1 - (py - self.viewport.1 * 0.5) / self.pixels_per_unit,
        )
    }

    /// The pixel over the world point `(x, y)`.
    pub fn to_pixel(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (x - self.center.0) * self.pixels_per_unit + self.viewport.0 * 0.5,
            self.viewport.1 * 0.5 - (y - self.center.1) * self.pixels_per_unit,
        )
    }
}

/// How each node is drawn: a radius in pixels and a colour (`0xRRGGBBAA`).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeStyle {
    pub radius: Vec<f32>,
    pub color: Vec<u32>,
}

/// The default node colour: near-black, opaque.
pub const NODE_COLOR: u32 = 0x2a_2a_2a_ff;

impl NodeStyle {
    /// Every node the same: radius `r` pixels, colour [`NODE_COLOR`].
    pub fn uniform(n: usize, r: f32) -> Self {
        Self {
            radius: vec![r; n],
            color: vec![NODE_COLOR; n],
        }
    }

    /// Radius from degree, so hubs read as hubs: `1.5 + sqrt(degree)` pixels,
    /// capped at 12.
    pub fn by_degree(degrees: &[u32]) -> Self {
        Self {
            radius: degrees
                .iter()
                .map(|&d| (1.5 + (d as f32).sqrt()).min(12.0))
                .collect(),
            color: vec![NODE_COLOR; degrees.len()],
        }
    }

    /// Radii from a numeric column (`pagerank`, say): its range mapped to
    /// `min_r..max_r` pixels by square root, so area tracks value. Values that
    /// are not finite get `min_r`.
    pub fn sizes_from(values: &[f32], min_r: f32, max_r: f32) -> Vec<f32> {
        let finite = values.iter().copied().filter(|v| v.is_finite());
        let (lo, hi) = finite.fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), v| {
            (a.min(v), b.max(v))
        });
        values
            .iter()
            .map(|&v| {
                // No spread (or nothing finite): every node the same size.
                if !v.is_finite() || hi <= lo || !hi.is_finite() {
                    return min_r;
                }
                let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0).sqrt();
                min_r + t * (max_r - min_r)
            })
            .collect()
    }

    /// Colours from categories (`louvain`'s community, say): category `c` gets
    /// the `c`-th colour of a small fixed palette, repeating past its end.
    pub fn colors_from(categories: &[u32]) -> Vec<u32> {
        categories
            .iter()
            .map(|&c| PALETTE[c as usize % PALETTE.len()])
            .collect()
    }
}

/// A plain categorical palette (`0xRRGGBBAA`), distinct on white.
pub const PALETTE: [u32; 10] = [
    0x1f77b4ff, 0xff7f0eff, 0x2ca02cff, 0xd62728ff, 0x9467bdff, 0x8c564bff, 0xe377c2ff, 0x7f7f7fff,
    0xbcbd22ff, 0x17becfff,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Graph {
        Graph::new(4, vec![0, 1, 2, 3], vec![1, 2, 3, 0]).unwrap()
    }

    #[test]
    fn a_graph_checks_its_edges() {
        let err = Graph::new(2, vec![0, 1], vec![1, 2]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "edge 1 names node 2, but the graph has 2 nodes"
        );
        let g = square();
        assert_eq!((g.n_nodes(), g.n_edges()), (4, 4));
        assert_eq!(g.line_indices(), vec![0, 1, 1, 2, 2, 3, 3, 0]);
        assert_eq!(g.degrees(), vec![2, 2, 2, 2]);
    }

    #[test]
    fn a_live_layout_is_layout_fa2() {
        let g = square();
        let params = Fa2Params::default();
        let mut live = LiveLayout::new(&g, params);
        live.step(3);
        live.step(7);
        assert_eq!(live.iterations(), 10);
        let mut state = Fa2State::new(g.topology().undirected(), params);
        state.step(g.topology().undirected(), 10);
        let (x, y) = live.positions();
        let (ex, ey) = state.positions();
        assert_eq!((x, y), (ex, ey));
    }

    #[test]
    fn the_camera_fits_pans_and_zooms_around_the_cursor() {
        let mut c = Camera::new(200.0, 100.0);
        c.fit((-10.0, -5.0, 10.0, 5.0));
        assert_eq!(c.center, (0.0, 0.0));
        assert!((c.pixels_per_unit - 9.2).abs() < 1e-4);
        let (px, py) = c.to_pixel(10.0, 5.0);
        assert!(px < 200.0 && py > 0.0, "the corner is inside the canvas");

        // The world point under the cursor stays under it.
        let before = c.to_world(150.0, 20.0);
        c.zoom(2.5, 150.0, 20.0);
        let after = c.to_world(150.0, 20.0);
        assert!((before.0 - after.0).abs() < 1e-4 && (before.1 - after.1).abs() < 1e-4);

        // Dragging right moves the world right with the cursor.
        let p = c.to_pixel(1.0, 1.0);
        c.pan(30.0, -10.0);
        let q = c.to_pixel(1.0, 1.0);
        assert!((q.0 - p.0 - 30.0).abs() < 1e-3 && (q.1 - p.1 + 10.0).abs() < 1e-3);
    }

    #[test]
    fn bounds_skip_non_finite_positions() {
        assert_eq!(bounds(&[], &[]), None);
        assert_eq!(
            bounds(&[1.0, f32::NAN, -2.0], &[0.5, 9.0, 3.0]),
            Some((-2.0, 0.5, 1.0, 3.0))
        );
    }

    #[test]
    fn styles_map_columns_to_sizes_and_colours() {
        assert_eq!(
            NodeStyle::sizes_from(&[0.0, 1.0, 0.25, f32::NAN], 2.0, 10.0),
            vec![2.0, 10.0, 6.0, 2.0]
        );
        assert_eq!(
            NodeStyle::sizes_from(&[3.0, 3.0], 2.0, 10.0),
            vec![2.0, 2.0]
        );
        assert_eq!(
            NodeStyle::colors_from(&[0, 1, 10]),
            vec![PALETTE[0], PALETTE[1], PALETTE[0]]
        );
        let s = NodeStyle::by_degree(&[0, 4, 1000]);
        assert_eq!(s.radius, vec![1.5, 3.5, 12.0]);
    }
}
