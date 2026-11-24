//! A level of the layout's hierarchy, drawn as a graph of its own (#497).
//!
//! Coarse-to-fine ForceAtlas2 lays out a pyramid of the graph from the top
//! down (`ursa_core::algo::Fa2Multilevel`): a few dozen nodes first, each
//! standing for thousands, then twice as many, and so on down to the graph
//! itself. Until the layout reaches the graph, drawing the graph is the
//! most expensive way to show the least: a million edges between points
//! that coincide. So the view draws the level being laid out instead:
//!
//! - **a disc for each of its nodes**, its area in proportion to the node's
//!   mass (the summed `degree + 1` of the nodes it stands for). All the
//!   discs together cover [`COVERAGE`] of the canvas, at every level: a
//!   node's disc is as large as its children's together will be;
//! - **its heaviest merged edges**, each as wide and as dark as the number
//!   of the graph's edges it stands for says ([`MIN_WIDTH`] to
//!   [`MAX_WIDTH`] pixels, by the square root of its weight over the
//!   level's heaviest).
//!
//! While the view moves (and a layout that is running is movement) a frame
//! draws what fits the frame budget ([`crate::budget`]): the heaviest discs
//! and the heaviest edges, which is the order both are kept in here.
//!
//! This module is the level as the view holds it and the instances it
//! draws, with no GPU in it; `level_render` (feature `render`) draws them.
//! What a level is made of comes from [`crate::scene::LiveLayout::level_graph`]
//! as a [`LevelGraph`], which is also what a cut through the hierarchy
//! (#498) would be made of: every level's masses and parents, and its
//! heaviest edges, in the level's own node numbering.

use bytemuck::{Pod, Zeroable};
use ursa_core::algo::LevelEdges;

use crate::scene::bounds;

/// The share of the canvas the discs of a level cover together, fitted.
pub const COVERAGE: f32 = 0.06;
/// The least radius a disc is drawn with, in pixels.
pub const MIN_RADIUS: f32 = 1.5;
/// The greatest, as a share of the canvas's shorter side.
pub const MAX_RADIUS_SHARE: f32 = 0.12;
/// How wide the ring around a marked disc is, in pixels
/// ([`LevelScene::discs_by`]).
pub const RING_WIDTH: f32 = 3.0;
/// How wide the rim of an open disc is, in pixels ([`LevelScene::set_kinds`]).
pub const OPEN_RIM: f32 = 2.0;
/// How much of its own colour the inside of an open disc keeps over the
/// background.
pub const OPEN_TINT: f32 = 0.14;
/// A point is picked within this many pixels of its middle, if its own
/// radius is less.
pub const POINT_REACH: f32 = 4.0;
/// A merged edge's width, in pixels: the lightest and the heaviest.
pub const MIN_WIDTH: f32 = 1.0;
pub const MAX_WIDTH: f32 = 5.0;
/// A merged edge's opacity: the lightest and the heaviest.
pub const MIN_ALPHA: f32 = 0.12;
pub const MAX_ALPHA: f32 = 0.6;

/// One level of a coarse-to-fine layout, as a graph of its own: what
/// [`crate::scene::LiveLayout::level_graph`] gives.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelGraph {
    /// Which level: 0 is the graph itself.
    pub level: usize,
    /// How many levels there are, the graph itself included.
    pub levels: usize,
    /// Each node's mass: the summed `degree + 1` of the graph's nodes it
    /// stands for.
    pub mass: Vec<f32>,
    /// Each node's parent, a node of the level above (`u32::MAX` for a
    /// node that has none: it left the hierarchy below that level). `None`
    /// at the coarsest level, or if the layout was not stepped down to
    /// this one a level at a time.
    pub parents: Option<Vec<u32>>,
    /// The heaviest merged edges, between this level's nodes, heaviest
    /// first. None at level 0: they are the graph's own.
    pub edges: LevelEdges,
}

/// A disc on the GPU: a node of a level.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Disc {
    pub x: f32,
    pub y: f32,
    /// In pixels.
    pub radius: f32,
    /// R, G, B, A.
    pub color: [u8; 4],
}

/// A merged edge on the GPU: its two ends, its width in pixels and its
/// opacity.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Link {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub width: f32,
    pub alpha: f32,
}

/// The level a view draws: its nodes where the layout has them now, and
/// its heaviest edges.
#[derive(Debug, Clone)]
pub struct LevelScene {
    level: usize,
    levels: usize,
    x: Vec<f32>,
    y: Vec<f32>,
    mass: Vec<f32>,
    /// The nodes, heaviest first (then by index).
    by_mass: Vec<u32>,
    total_mass: f32,
    /// The edges, heaviest first.
    src: Vec<u32>,
    dst: Vec<u32>,
    weight: Vec<f32>,
    /// How many merged edges the level has: `src.len()` of them are held.
    total_edges: usize,
    /// The greatest radius a disc is drawn with, in pixels, if less than
    /// [`MAX_RADIUS_SHARE`] allows ([`LevelScene::set_max_radius`]).
    max_radius: f32,
    /// How each node is drawn where it is not by its mass
    /// ([`LevelScene::set_kinds`]); empty when every one is.
    kinds: Vec<f32>,
}

/// How a node of a scene is drawn ([`LevelScene::set_kinds`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// A disc, its area by the node's mass.
    Mass,
    /// An open disc of this radius in the layout's units: a rim, and the
    /// nodes inside it drawn over it.
    Open(f32),
    /// A point of this radius in pixels, drawn over the discs.
    Point(f32),
    /// Not drawn, nor its edges.
    Hidden,
}

impl LevelScene {
    /// The level `level` of `levels`, of `mass.len()` nodes, every one at
    /// the origin until [`LevelScene::set_positions`], with the merged
    /// edges `(src[i], dst[i])` of weight `weight[i]`, heaviest first, of
    /// the `total_edges` the level has. Or why that is not a level.
    pub fn new(
        level: usize,
        levels: usize,
        mass: Vec<f32>,
        src: Vec<u32>,
        dst: Vec<u32>,
        weight: Vec<f32>,
        total_edges: usize,
    ) -> Result<Self, String> {
        let n = mass.len();
        if src.len() != dst.len() || src.len() != weight.len() {
            return Err(format!(
                "a level's edges are {} sources, {} destinations and {} weights",
                src.len(),
                dst.len(),
                weight.len()
            ));
        }
        if let Some(&node) = src.iter().chain(&dst).find(|&&v| v as usize >= n) {
            return Err(format!("a level of {n} nodes has an edge at node {node}"));
        }
        let mut by_mass: Vec<u32> = (0..n as u32).collect();
        by_mass.sort_unstable_by(|&a, &b| {
            mass[b as usize]
                .total_cmp(&mass[a as usize])
                .then(a.cmp(&b))
        });
        let total_mass = mass.iter().map(|m| m.max(0.0)).sum();
        Ok(Self {
            level,
            levels,
            x: vec![0.0; n],
            y: vec![0.0; n],
            mass,
            by_mass,
            total_mass,
            total_edges: total_edges.max(src.len()),
            max_radius: f32::INFINITY,
            kinds: Vec::new(),
            src,
            dst,
            weight,
        })
    }

    /// Which level this is: 0 would be the graph itself.
    pub fn level(&self) -> usize {
        self.level
    }

    /// How many levels the layout has, the graph itself included.
    pub fn levels(&self) -> usize {
        self.levels
    }

    /// How many nodes the level has.
    pub fn n_nodes(&self) -> usize {
        self.mass.len()
    }

    /// How many of the level's merged edges are held: the heaviest.
    pub fn n_edges(&self) -> usize {
        self.src.len()
    }

    /// How many merged edges the level has.
    pub fn total_edges(&self) -> usize {
        self.total_edges
    }

    /// Draw no disc with a radius over `pixels`, however heavy and however
    /// far the view is zoomed in: for discs that stand a known distance
    /// apart (a cut's, `crate::cut`), which larger ones would cover.
    pub fn set_max_radius(&mut self, pixels: f32) {
        self.max_radius = if pixels > 0.0 { pixels } else { f32::INFINITY };
    }

    /// Draw some nodes otherwise than by their mass (`GROUP BY`'s groups
    /// opened in place, #499), one value per node: `0` for a disc by its
    /// mass; above 0, an open disc of that radius in the layout's units,
    /// drawn under everything as a rim [`OPEN_RIM`] pixels wide around a
    /// tint of its colour; below 0, a point of that many pixels' radius,
    /// drawn over everything; NaN, a node that is not drawn, nor its
    /// edges. An empty list is every node by its mass. Or why that is not
    /// one per node.
    pub fn set_kinds(&mut self, kinds: Vec<f32>) -> Result<(), String> {
        if !kinds.is_empty() && kinds.len() != self.n_nodes() {
            return Err(format!(
                "a scene of {} nodes has {} kinds",
                self.n_nodes(),
                kinds.len()
            ));
        }
        self.kinds = kinds;
        Ok(())
    }

    /// How node `node` is drawn.
    pub fn kind(&self, node: usize) -> Kind {
        match self.kinds.get(node) {
            None => Kind::Mass,
            Some(k) if k.is_nan() => Kind::Hidden,
            Some(&k) if k > 0.0 => Kind::Open(k),
            Some(&k) if k < 0.0 => Kind::Point(-k),
            Some(_) => Kind::Mass,
        }
    }

    /// A node's radius in pixels as it is drawn at `pixels_per_unit`:
    /// [`LevelScene::radius`] for a disc by its mass.
    pub fn radius_at(
        &self,
        node: usize,
        width: f32,
        height: f32,
        zoom: f32,
        pixels_per_unit: f32,
    ) -> f32 {
        match self.kind(node) {
            Kind::Mass => self.radius_of(self.mass[node], width, height, zoom),
            Kind::Open(r) => r * pixels_per_unit,
            Kind::Point(r) => r,
            Kind::Hidden => 0.0,
        }
    }

    /// The nodes' positions, to write: `x` and `y`, one per node each.
    pub fn positions_mut(&mut self) -> (&mut [f32], &mut [f32]) {
        (&mut self.x, &mut self.y)
    }

    /// Put the nodes at `x`, `y` (one per node each).
    pub fn set_positions(&mut self, x: &[f32], y: &[f32]) {
        self.x.copy_from_slice(x);
        self.y.copy_from_slice(y);
    }

    /// The nodes' positions.
    pub fn positions(&self) -> (&[f32], &[f32]) {
        (&self.x, &self.y)
    }

    /// The smallest box holding every node, if any is anywhere finite.
    pub fn bounds(&self) -> Option<(f32, f32, f32, f32)> {
        bounds(&self.x, &self.y)
    }

    /// A node's radius in pixels on a `width` x `height` canvas: its area
    /// is its share of the level's mass, of [`COVERAGE`] of the canvas,
    /// times `zoom` (1 when the level is fitted; more as the view zooms
    /// in). Never under [`MIN_RADIUS`], nor over [`MAX_RADIUS_SHARE`] of
    /// the shorter side.
    pub fn radius(&self, node: usize, width: f32, height: f32, zoom: f32) -> f32 {
        self.radius_of(self.mass[node], width, height, zoom)
    }

    fn radius_of(&self, mass: f32, width: f32, height: f32, zoom: f32) -> f32 {
        let share = if self.total_mass > 0.0 {
            mass.max(0.0) / self.total_mass
        } else {
            0.0
        };
        let area = COVERAGE * width * height * share;
        let most = (MAX_RADIUS_SHARE * width.min(height))
            .min(self.max_radius)
            .max(MIN_RADIUS);
        ((area / std::f32::consts::PI).sqrt() * zoom).clamp(MIN_RADIUS, most)
    }

    /// The `count` heaviest nodes as discs of `color` (`0xRRGGBBAA`), the
    /// lightest of them first, so that the heaviest are drawn over them.
    pub fn discs(&self, count: usize, width: f32, height: f32, zoom: f32, color: u32) -> Vec<Disc> {
        self.discs_by(count, width, height, zoom, |_| (color, None))
    }

    /// [`LevelScene::discs`], each node `i` in the colour `ink(i)` gives
    /// first, and with a ring [`RING_WIDTH`] pixels wide around it in the
    /// colour it gives second, if any: a disc that much larger, under the
    /// node's own. Of a scene whose nodes are not all drawn by their mass
    /// ([`LevelScene::set_kinds`]): the discs by mass, then the points
    /// over them; the open discs are [`LevelScene::open_discs`].
    pub fn discs_by(
        &self,
        count: usize,
        width: f32,
        height: f32,
        zoom: f32,
        ink: impl Fn(usize) -> (u32, Option<u32>),
    ) -> Vec<Disc> {
        let count = count.min(self.n_nodes());
        let mut discs = Vec::with_capacity(count);
        let mut push = |i: usize, radius: f32| {
            let (color, ring) = ink(i);
            let (x, y) = (self.x[i], self.y[i]);
            if let Some(ring) = ring {
                discs.push(Disc {
                    x,
                    y,
                    radius: radius + RING_WIDTH,
                    color: ring.to_be_bytes(),
                });
            }
            discs.push(Disc {
                x,
                y,
                radius,
                color: color.to_be_bytes(),
            });
        };
        for &i in self.by_mass[..count].iter().rev() {
            let i = i as usize;
            if self.kind(i) == Kind::Mass {
                push(i, self.radius_of(self.mass[i], width, height, zoom));
            }
        }
        if !self.kinds.is_empty() {
            for &i in &self.by_mass[..count] {
                if let Kind::Point(r) = self.kind(i as usize) {
                    push(i as usize, r);
                }
            }
        }
        discs
    }

    /// The open discs of a scene ([`LevelScene::set_kinds`]) at
    /// `pixels_per_unit`, on `background` (`0xRRGGBBAA`), the largest
    /// first: each a rim in its colour (and a ring, as
    /// [`LevelScene::discs_by`]) around a tint of it. They are drawn under
    /// the edges, which [`LevelScene::discs_by`]'s discs are drawn over.
    pub fn open_discs(
        &self,
        pixels_per_unit: f32,
        background: u32,
        ink: impl Fn(usize) -> (u32, Option<u32>),
    ) -> Vec<Disc> {
        let mut discs = Vec::new();
        if self.kinds.is_empty() {
            return discs;
        }
        let mut open: Vec<(f32, usize)> = (0..self.n_nodes())
            .filter_map(|i| match self.kind(i) {
                Kind::Open(r) => Some((r, i)),
                _ => None,
            })
            .collect();
        open.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        for (r, i) in open {
            let radius = r * pixels_per_unit;
            let (color, ring) = ink(i);
            let (x, y) = (self.x[i], self.y[i]);
            let mut push = |radius: f32, color: u32| {
                discs.push(Disc {
                    x,
                    y,
                    radius,
                    color: color.to_be_bytes(),
                })
            };
            if let Some(ring) = ring {
                push(radius + RING_WIDTH, ring);
            }
            push(radius, color);
            if radius > OPEN_RIM {
                push(radius - OPEN_RIM, tint(background, color));
            }
        }
        discs
    }

    /// The node whose disc covers the point `(x, y)` of the layout, on a
    /// `width` x `height` canvas at `pixels_per_unit` (and `zoom`, as
    /// [`LevelScene::radius`] has it): the one drawn last of those that
    /// do, which is the heaviest. With [`LevelScene::set_kinds`]: a point
    /// within [`POINT_REACH`] pixels first, the nearest; then a disc; then
    /// the smallest open disc.
    pub fn pick(
        &self,
        (x, y): (f32, f32),
        pixels_per_unit: f32,
        width: f32,
        height: f32,
        zoom: f32,
    ) -> Option<u32> {
        let far = |i: usize| {
            let (dx, dy) = (self.x[i] - x, self.y[i] - y);
            dx * dx + dy * dy
        };
        if self.kinds.is_empty() {
            return self.by_mass.iter().copied().find(|&i| {
                let i = i as usize;
                let r = self.radius_of(self.mass[i], width, height, zoom) / pixels_per_unit;
                far(i) <= r * r
            });
        }
        let mut point: Option<(f32, u32)> = None;
        let mut open: Option<(f32, u32)> = None;
        for i in 0..self.n_nodes() {
            match self.kind(i) {
                Kind::Point(r) => {
                    let (r, d) = (r.max(POINT_REACH) / pixels_per_unit, far(i));
                    if d <= r * r && point.is_none_or(|(best, _)| d < best) {
                        point = Some((d, i as u32));
                    }
                }
                Kind::Open(r) if far(i) <= r * r && open.is_none_or(|(best, _)| r < best) => {
                    open = Some((r, i as u32));
                }
                _ => {}
            }
        }
        if let Some((_, i)) = point {
            return Some(i);
        }
        let disc = self.by_mass.iter().copied().find(|&i| {
            let i = i as usize;
            let r = self.radius_of(self.mass[i], width, height, zoom) / pixels_per_unit;
            self.kind(i) == Kind::Mass && far(i) <= r * r
        });
        disc.or(open.map(|(_, i)| i))
    }

    /// The nodes, heaviest first (then by index).
    pub fn by_mass(&self) -> &[u32] {
        &self.by_mass
    }

    /// The `count` heaviest edges, the lightest of them first.
    pub fn links(&self, count: usize) -> Vec<Link> {
        let count = count.min(self.n_edges());
        let heaviest = self.weight.first().copied().unwrap_or(1.0).max(1.0);
        (0..count)
            .rev()
            .filter(|&e| {
                self.kinds.is_empty()
                    || (self.kind(self.src[e] as usize) != Kind::Hidden
                        && self.kind(self.dst[e] as usize) != Kind::Hidden)
            })
            .map(|e| {
                let (a, b) = (self.src[e] as usize, self.dst[e] as usize);
                // By the square root: a weight is a count of edges, and
                // the heaviest of a level is hundreds of times the rest.
                let t = (self.weight[e].max(0.0) / heaviest).sqrt().min(1.0);
                Link {
                    a: [self.x[a], self.y[a]],
                    b: [self.x[b], self.y[b]],
                    width: MIN_WIDTH + (MAX_WIDTH - MIN_WIDTH) * t,
                    alpha: MIN_ALPHA + (MAX_ALPHA - MIN_ALPHA) * t,
                }
            })
            .collect()
    }
}

/// `color` (`0xRRGGBBAA`) over `background`, [`OPEN_TINT`] of it: the
/// inside of an open disc.
fn tint(background: u32, color: u32) -> u32 {
    let (b, c) = (background.to_be_bytes(), color.to_be_bytes());
    let mix = |i: usize| {
        (f32::from(b[i]) + (f32::from(c[i]) - f32::from(b[i])) * OPEN_TINT).round() as u8
    };
    u32::from_be_bytes([mix(0), mix(1), mix(2), 0xff])
}

/// What a view is drawing of a level, for a status line and for tests
/// (`View::level_info`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LevelInfo {
    /// Which level: counted down to 1, the last before the graph itself.
    pub level: usize,
    /// How many levels the layout has, the graph itself included.
    pub levels: usize,
    /// The level's nodes, and how many the last frame drew: the heaviest.
    pub nodes: usize,
    pub nodes_drawn: usize,
    /// The merged edges the view holds (the heaviest of `total_edges`),
    /// and how many the last frame drew: the heaviest of those.
    pub edges: usize,
    pub edges_drawn: usize,
    pub total_edges: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> LevelScene {
        // Four nodes of mass 1, 9, 4, 2 and three edges, heaviest first.
        let mut s = LevelScene::new(
            3,
            5,
            vec![1.0, 9.0, 4.0, 2.0],
            vec![1, 0, 2],
            vec![2, 1, 3],
            vec![16.0, 4.0, 1.0],
            7,
        )
        .unwrap();
        s.set_positions(&[0.0, 10.0, 20.0, 30.0], &[0.0, 1.0, 2.0, 3.0]);
        s
    }

    #[test]
    fn a_disc_s_area_is_its_share_of_the_mass() {
        let s = scene();
        let (w, h) = (1000.0, 800.0);
        let area = |i: usize| std::f32::consts::PI * s.radius(i, w, h, 1.0).powi(2);
        let all: f32 = (0..4).map(area).sum();
        assert!((all - COVERAGE * w * h).abs() < 1.0, "{all}");
        assert!((area(1) / area(0) - 9.0).abs() < 1e-3);
        // A parent's disc is as large as its children's together: masses
        // add up, and so do areas.
        assert!((area(1) - (area(2) + area(2) + area(0))).abs() < 1.0);
        // Zoomed in, larger; never under the least, nor over the most.
        assert!(s.radius(1, w, h, 2.0) > s.radius(1, w, h, 1.0));
        assert_eq!(s.radius(1, w, h, 1e6), MAX_RADIUS_SHARE * h);
        assert_eq!(s.radius(0, 10.0, 10.0, 1.0), MIN_RADIUS);
        // Nor over a most of its own.
        let mut capped = scene();
        capped.set_max_radius(12.0);
        assert_eq!(capped.radius(1, w, h, 1.0), 12.0);
        assert_eq!(capped.radius(1, w, h, 1e6), 12.0);
        assert_eq!(
            capped.radius(0, 100.0, 100.0, 1.0),
            s.radius(0, 100.0, 100.0, 1.0)
        );
        capped.set_max_radius(0.0);
        assert_eq!(capped.radius(1, w, h, 1.0), s.radius(1, w, h, 1.0));
    }

    #[test]
    fn a_prefix_is_the_heaviest_drawn_last() {
        let s = scene();
        let discs = s.discs(2, 1000.0, 800.0, 1.0, 0x11223344);
        // Nodes 1 (mass 9) and 2 (mass 4): the heavier last, on top.
        assert_eq!(discs.len(), 2);
        assert_eq!((discs[0].x, discs[1].x), (20.0, 10.0));
        assert!(discs[1].radius > discs[0].radius);
        assert_eq!(discs[0].color, [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(s.discs(99, 1000.0, 800.0, 1.0, 0).len(), 4);
        let links = s.links(2);
        assert_eq!(links.len(), 2);
        // The heaviest (1 to 2) last, at full width; the next at half way
        // by the square root of a quarter.
        assert_eq!((links[1].a, links[1].b), ([10.0, 1.0], [20.0, 2.0]));
        assert_eq!(links[1].width, MAX_WIDTH);
        assert_eq!(links[1].alpha, MAX_ALPHA);
        assert_eq!(links[0].width, MIN_WIDTH + (MAX_WIDTH - MIN_WIDTH) * 0.5);
        assert_eq!(s.links(99).len(), 3);
        assert_eq!(
            s.links(99)[0].width,
            MIN_WIDTH + (MAX_WIDTH - MIN_WIDTH) * 0.25
        );
        assert_eq!((s.n_edges(), s.total_edges()), (3, 7));
    }

    #[test]
    fn discs_take_their_own_colours_and_rings() {
        let s = scene();
        let discs = s.discs_by(2, 1000.0, 800.0, 1.0, |i| {
            (i as u32, (i == 1).then_some(0xff))
        });
        // Node 2, then node 1's ring and node 1 over it.
        assert_eq!(discs.len(), 3);
        assert_eq!(discs[0].color, [0, 0, 0, 2]);
        assert_eq!(
            (discs[1].color, discs[2].color),
            ([0, 0, 0, 0xff], [0, 0, 0, 1])
        );
        assert_eq!(discs[1].radius, discs[2].radius + RING_WIDTH);
        assert_eq!((discs[1].x, discs[1].y), (discs[2].x, discs[2].y));
    }

    #[test]
    fn a_point_picks_the_heaviest_disc_over_it() {
        let s = scene();
        let (w, h) = (1000.0, 800.0);
        // On node 1 (at 10, 1), and a pixel inside its edge.
        assert_eq!(s.pick((10.0, 1.0), 1.0, w, h, 1.0), Some(1));
        let r = s.radius(1, w, h, 1.0);
        assert_eq!(s.pick((10.0 + r - 1.0, 1.0), 1.0, w, h, 1.0), Some(1));
        // Node 2 is ten units away: under node 1's disc at this zoom,
        // and its own disc once the view is zoomed in.
        assert!(r > 10.0);
        assert_eq!(s.pick((20.0, 2.0), 1.0, w, h, 1.0), Some(1));
        assert_eq!(s.pick((20.0, 2.0), 100.0, w, h, 1.0), Some(2));
        assert_eq!(s.pick((500.0, 500.0), 1.0, w, h, 1.0), None);
    }

    #[test]
    fn an_open_disc_is_under_the_others_and_points_are_over_them() {
        let mut s = scene();
        let (w, h) = (1000.0, 800.0);
        assert!(s.set_kinds(vec![0.0; 3]).is_err());
        // Node 1 open, 5 units wide; node 0 a point of 3 pixels; node 3
        // hidden.
        s.set_kinds(vec![-3.0, 5.0, 0.0, f32::NAN]).unwrap();
        assert_eq!(
            [s.kind(0), s.kind(1), s.kind(2), s.kind(3)],
            [Kind::Point(3.0), Kind::Open(5.0), Kind::Mass, Kind::Hidden]
        );
        let ink = |i: usize| {
            (
                0x000000ff + ((i as u32) << 24),
                (i == 1).then_some(0x11111111),
            )
        };
        // The open disc, under the edges: its ring, its rim (5 units at 4
        // pixels a unit), its inside.
        let open = s.open_discs(4.0, 0xffffffff, ink);
        let radii: Vec<f32> = open.iter().map(|d| d.radius).collect();
        assert_eq!(radii, [20.0 + RING_WIDTH, 20.0, 20.0 - OPEN_RIM]);
        assert_eq!(open[0].color, [0x11; 4]);
        assert_eq!(open[1].color, [1, 0, 0, 0xff]);
        // The inside is mostly the background.
        assert_eq!(open[2].color, [219, 219, 219, 0xff]);
        // Over them: node 2 by its mass, then the point.
        let discs = s.discs_by(4, w, h, 1.0, ink);
        let radii: Vec<f32> = discs.iter().map(|d| d.radius).collect();
        assert_eq!(radii, [s.radius(2, w, h, 1.0), 3.0]);
        assert_eq!((discs[1].x, discs[1].color), (0.0, [0, 0, 0, 0xff]));
        assert_eq!(s.radius_at(1, w, h, 1.0, 4.0), 20.0);
        assert_eq!(s.radius_at(0, w, h, 1.0, 4.0), 3.0);
        assert_eq!(s.radius_at(3, w, h, 1.0, 4.0), 0.0);
        // The edges at the hidden node are not drawn.
        assert_eq!(s.links(3).len(), 2);

        // A click: the point within its reach, then a disc, then the open
        // disc; never the hidden node.
        assert_eq!(s.pick((0.05, 0.0), 40.0, w, h, 1.0), Some(0));
        assert_eq!(s.pick((10.0, 4.0), 40.0, w, h, 1.0), Some(1));
        assert_eq!(s.pick((20.0, 2.0), 40.0, w, h, 1.0), Some(2));
        assert_eq!(s.pick((30.0, 3.0), 40.0, w, h, 1.0), None);
        // With no kinds, as before.
        s.set_kinds(Vec::new()).unwrap();
        assert_eq!(s.kind(1), Kind::Mass);
        assert!(s.open_discs(4.0, 0, |_| (7, None)).is_empty());
        assert_eq!(s.discs(4, w, h, 1.0, 7).len(), 4);
    }

    #[test]
    fn a_level_that_is_not_one_is_refused() {
        let level = |src: Vec<u32>, dst: Vec<u32>, weight: Vec<f32>| {
            LevelScene::new(1, 2, vec![1.0; 3], src, dst, weight, 0)
        };
        assert!(level(vec![0], vec![1, 2], vec![1.0]).is_err());
        assert!(level(vec![0], vec![1], vec![]).is_err());
        assert!(level(vec![0], vec![3], vec![1.0]).is_err());
        let empty = level(vec![], vec![], vec![]).unwrap();
        assert!(empty.links(5).is_empty());
        assert_eq!(empty.bounds(), Some((0.0, 0.0, 0.0, 0.0)));
        // No nodes at all: nothing to draw, and nothing to fit.
        let none = LevelScene::new(1, 2, vec![], vec![], vec![], vec![], 0).unwrap();
        assert!(none.discs(5, 100.0, 100.0, 1.0, 0).is_empty());
        assert_eq!(none.bounds(), None);
    }

    #[test]
    fn the_instances_are_laid_out_as_their_pipelines_read_them() {
        assert_eq!(std::mem::size_of::<Disc>(), 16);
        assert_eq!(std::mem::size_of::<Link>(), 24);
    }
}
