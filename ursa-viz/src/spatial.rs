//! Where things are, so that pointing does not read every node (#500): a
//! uniform grid over the node positions for picking, hover and culling, and
//! a grid of edges by length for picking an edge. No GPU.
//!
//! Both are indexes over positions as they were when built. They only
//! narrow down which nodes or edges to test: every test is the one a linear
//! pass makes ([`Discs::hit`], `interact::edge_distance2`), and ties go to
//! the lower index as they do there, so a pick with a grid is the pick
//! without it ([`Discs::pick_linear`], [`nearest_edge`]), always.
//!
//! [`nearest_edge`]: crate::interact::nearest_edge

use crate::highlight::{flag, MIN_FRONT_RADIUS};
use crate::interact::edge_distance2;
use crate::scene::Camera;

/// How many positions the grid's box is found from.
const SAMPLE: usize = 4096;
/// The most cells along a side.
const MAX_SIDE: usize = 4096;
/// The most cells in all.
const MAX_CELLS: usize = 1 << 22;

/// A range of cells: columns `c0..=c1` of rows `r0..=r1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cells {
    pub c0: usize,
    pub c1: usize,
    pub r0: usize,
    pub r1: usize,
}

impl Cells {
    /// How many cells.
    pub fn len(&self) -> usize {
        (self.c1 + 1 - self.c0) * (self.r1 + 1 - self.r0)
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

/// A uniform grid over node positions: which nodes are in each cell.
///
/// The cells tile the box that holds the middle 98% of the positions on
/// each axis (so one stray node does not stretch them), about two nodes to
/// a cell; a node outside the box is in the nearest cell of the border, as
/// is a point asked about. A cell's nodes are in index order.
#[derive(Debug, Clone)]
pub struct NodeGrid {
    /// The box's corner, and cells per world unit.
    x0: f32,
    y0: f32,
    inv: f32,
    cols: usize,
    rows: usize,
    /// Cell `c`'s nodes are `order[start[c]..start[c + 1]]`.
    start: Vec<u32>,
    order: Vec<u32>,
}

impl NodeGrid {
    /// The grid over these positions, in two passes over them.
    pub fn build(x: &[f32], y: &[f32]) -> Self {
        let n = x.len().min(y.len());
        let (x0, y0, w, h) = middle_box(&x[..n], &y[..n]);
        let target = (n / 2).clamp(1, MAX_CELLS) as f64;
        let (w, h) = (f64::from(w), f64::from(h));
        let side = if w > 0.0 && h > 0.0 {
            (w * h / target).sqrt()
        } else {
            w.max(h) / target
        };
        let dim = |len: f64| {
            if side > 0.0 && len > 0.0 {
                ((len / side).ceil() as usize).clamp(1, MAX_SIDE)
            } else {
                1
            }
        };
        let (cols, rows) = (dim(w), dim(h));
        // The cells cover the box: a side the caps made longer still does.
        let cell = (w / cols as f64).max(h / rows as f64);
        let inv = if cell > 0.0 { (1.0 / cell) as f32 } else { 0.0 };
        let mut grid = Self {
            x0,
            y0,
            inv,
            cols,
            rows,
            start: vec![0; cols * rows + 1],
            order: vec![0; n],
        };
        // Sort the nodes by row, then each row's by column: two counting
        // sorts, which leave a cell's nodes in index order. Each reads the
        // positions in order and writes to a few hundred places at a time,
        // where one sort by cell would write all over a million.
        let mut row_start = vec![0u32; rows + 1];
        for &py in &y[..n] {
            row_start[grid.row(py) + 1] += 1;
        }
        for r in 0..rows {
            row_start[r + 1] += row_start[r];
        }
        let mut next = row_start.clone();
        let mut by_row = vec![(0u32, 0u16); n];
        for i in 0..n {
            let at = &mut next[grid.row(y[i])];
            by_row[*at as usize] = (i as u32, grid.col(x[i]) as u16);
            *at += 1;
        }
        let mut next = vec![0u32; cols];
        for r in 0..rows {
            let (from, to) = (row_start[r] as usize, row_start[r + 1] as usize);
            let start = &mut grid.start[r * cols..(r + 1) * cols + 1];
            for &(_, c) in &by_row[from..to] {
                start[usize::from(c) + 1] += 1;
            }
            start[0] = from as u32;
            for c in 0..cols {
                start[c + 1] += start[c];
            }
            next.copy_from_slice(&start[..cols]);
            for &(i, c) in &by_row[from..to] {
                let at = &mut next[usize::from(c)];
                grid.order[*at as usize] = i;
                *at += 1;
            }
        }
        grid
    }

    /// How many nodes the grid was built over.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Columns and rows.
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// The column of `x`: never decreasing in `x`, and 0 for a NaN.
    #[inline]
    fn col(&self, x: f32) -> usize {
        // A float to an integer saturates, and a NaN is 0.
        (((x - self.x0) * self.inv) as usize).min(self.cols - 1)
    }

    #[inline]
    fn row(&self, y: f32) -> usize {
        (((y - self.y0) * self.inv) as usize).min(self.rows - 1)
    }

    /// The cells that hold every node in the box from `(x0, y0)` to
    /// `(x1, y1)`, corners included.
    pub fn cells(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> Cells {
        // Rounding to `f32` keeps the order, and so does the column.
        let (c0, c1) = (self.col(x0 as f32), self.col(x1 as f32));
        let (r0, r1) = (self.row(y0 as f32), self.row(y1 as f32));
        // A NaN corner is every cell.
        let all = !(x0 <= x1 && y0 <= y1);
        Cells {
            c0: if all { 0 } else { c0.min(c1) },
            c1: if all { self.cols - 1 } else { c0.max(c1) },
            r0: if all { 0 } else { r0.min(r1) },
            r1: if all { self.rows - 1 } else { r0.max(r1) },
        }
    }

    /// The nodes in `cells`, a row of cells at a time: each slice is in
    /// cell order, and a cell's nodes in index order.
    pub fn runs(&self, cells: Cells) -> impl Iterator<Item = &[u32]> + '_ {
        (cells.r0..=cells.r1).map(move |r| {
            let from = self.start[r * self.cols + cells.c0] as usize;
            let to = self.start[r * self.cols + cells.c1 + 1] as usize;
            &self.order[from..to]
        })
    }

    /// How many nodes are in `cells`.
    pub fn count(&self, cells: Cells) -> usize {
        self.runs(cells).map(<[u32]>::len).sum()
    }
}

/// The box holding the middle 98% of the positions on each axis, grown a
/// little: `(x0, y0, width, height)`. Found from a sample of them, the
/// same for the same positions.
fn middle_box(x: &[f32], y: &[f32]) -> (f32, f32, f32, f32) {
    let n = x.len();
    let step = n.div_ceil(SAMPLE).max(1);
    let sample = |v: &[f32]| {
        let mut s: Vec<f32> = v
            .iter()
            .step_by(step)
            .copied()
            .filter(|p| p.is_finite())
            .collect();
        s.sort_unstable_by(f32::total_cmp);
        s
    };
    let span = |s: &[f32]| -> (f32, f32) {
        let Some(last) = s.len().checked_sub(1) else {
            return (0.0, 0.0);
        };
        // Every position, for a few: there is no stray to leave out.
        let cut = if s.len() < 256 { 0 } else { s.len() / 100 };
        let (lo, hi) = (s[cut], s[last - cut]);
        let pad = (hi - lo) * 0.05;
        let (lo, hi) = (lo - pad, hi + pad);
        if (hi - lo).is_finite() {
            (lo, hi - lo)
        } else {
            (lo.max(f32::MIN), 0.0)
        }
    };
    let ((x0, w), (y0, h)) = (span(&sample(x)), span(&sample(y)));
    (x0, y0, w, h)
}

/// The nodes as discs on the canvas: what a pick tests.
#[derive(Debug, Clone, Copy)]
pub struct Discs<'a> {
    pub x: &'a [f32],
    pub y: &'a [f32],
    /// Each node's radius in pixels, before the view's shrinking.
    pub radius: &'a [f32],
    /// Each node's flags ([`flag`]).
    pub flags: &'a [u8],
    pub camera: &'a Camera,
    /// How much nodes are shrunk.
    pub scale: f32,
}

/// How far past its disc a node is still picked, in pixels.
pub const PICK_SLACK: f32 = 2.0;

impl Discs<'_> {
    /// Node `i`'s radius as drawn, in pixels: a node in front is never
    /// tiny.
    pub fn drawn_radius(&self, i: usize) -> f32 {
        let r = self.radius[i] * self.scale;
        if self.flags[i] & flag::FRONT != 0 {
            r.max(MIN_FRONT_RADIUS)
        } else {
            r
        }
    }

    /// The squared distance, in pixels, from the pixel `(px, py)` to node
    /// `i`'s centre, if its disc (and [`PICK_SLACK`] around it) covers the
    /// pixel. A hidden node covers nothing.
    pub fn hit(&self, i: usize, px: f32, py: f32) -> Option<f32> {
        if self.flags[i] & flag::HIDDEN != 0 {
            return None;
        }
        let (qx, qy) = self.camera.to_pixel(self.x[i], self.y[i]);
        let d2 = (qx - px).powi(2) + (qy - py).powi(2);
        let r = self.drawn_radius(i) + PICK_SLACK;
        (d2 <= r * r).then_some(d2)
    }

    /// The node drawn under the pixel `(px, py)`, if any: the nearest node
    /// whose disc covers it and, of nodes equally near, the first. Every
    /// node is tested.
    pub fn pick_linear(&self, px: f32, py: f32) -> Option<u32> {
        let mut best = None;
        for i in 0..self.x.len() {
            nearer(&mut best, i as u32, self.hit(i, px, py));
        }
        best.map(|(i, _)| i)
    }

    /// [`Self::pick_linear`], testing only the nodes `grid` has near the
    /// pixel, and every node from `fresh` on: the nodes before `fresh` are
    /// where they were when the grid was built, and the rest (moved since,
    /// or new) are not. `max_radius` is at least every node's radius.
    pub fn pick(
        &self,
        grid: &NodeGrid,
        fresh: usize,
        max_radius: f32,
        px: f32,
        py: f32,
    ) -> Option<u32> {
        let n = self.x.len();
        let fresh = fresh.min(n).min(grid.len());
        let reach = (max_radius * self.scale).max(MIN_FRONT_RADIUS) + PICK_SLACK;
        let cells = self.cells_near(grid, px, py, reach);
        // More cells than it is worth: the linear pass.
        if fresh == 0 || !reach.is_finite() || cells.len() > n / 4 + 16 {
            return self.pick_linear(px, py);
        }
        let mut best = None;
        for run in grid.runs(cells) {
            for &i in run {
                if (i as usize) < fresh {
                    nearer(&mut best, i, self.hit(i as usize, px, py));
                }
            }
        }
        for i in fresh..n {
            nearer(&mut best, i as u32, self.hit(i, px, py));
        }
        best.map(|(i, _)| i)
    }

    /// The cells that hold every node whose centre is within `reach`
    /// pixels of the pixel `(px, py)`, as [`Camera::to_pixel`] places it.
    pub fn cells_near(&self, grid: &NodeGrid, px: f32, py: f32, reach: f32) -> Cells {
        let (w, h) = self.camera.viewport;
        // `to_pixel` rounds: a little further than `reach`, for that.
        let (px, py, w, h) = (f64::from(px), f64::from(py), f64::from(w), f64::from(h));
        let reach = f64::from(reach) * 1.0001 + 1e-4 * (px.abs() + py.abs() + w + h) + 1e-3;
        self.cells_in(grid, px - reach, py - reach, px + reach, py + reach)
    }

    /// The cells that hold every node drawn at a pixel of the box from
    /// `(px0, py0)` to `(px1, py1)`.
    pub fn cells_in(&self, grid: &NodeGrid, px0: f64, py0: f64, px1: f64, py1: f64) -> Cells {
        let (w, h) = self.camera.viewport;
        let (cx, cy) = self.camera.center;
        let s = f64::from(self.camera.pixels_per_unit);
        let x = |px: f64| f64::from(cx) + (px - f64::from(w) * 0.5) / s;
        let y = |py: f64| f64::from(cy) - (py - f64::from(h) * 0.5) / s;
        // Up the canvas is down the world's y.
        grid.cells(x(px0), y(py1), x(px1), y(py0))
    }
}

/// Keep the nearer of `best` and node or edge `i` at squared distance
/// `d2`; of two equally near, the lower index.
fn nearer(best: &mut Option<(u32, f32)>, i: u32, d2: Option<f32>) {
    let Some(d2) = d2 else { return };
    if best.is_none_or(|(b, bd)| d2 < bd || (d2 == bd && i < b)) {
        *best = Some((i, d2));
    }
}

/// The edges by where their middles are, a grid for each length: an edge
/// is in the level whose cells are just longer than it, in the cell its
/// middle is in. Every point of an edge is within half a cell of its
/// middle, so the edges near a point are in the cells around it on each
/// level, wherever their ends are.
#[derive(Debug, Clone)]
pub struct EdgeGrid {
    x0: f64,
    y0: f64,
    /// The largest coordinate, for the rounding a test may have made.
    extent: f64,
    levels: Vec<Level>,
    /// Cell `c`'s edges are `order[start[c]..start[c + 1]]`.
    start: Vec<u32>,
    order: Vec<u32>,
    n_edges: usize,
}

#[derive(Debug, Clone, Copy)]
struct Level {
    /// A cell's side.
    side: f64,
    cols: usize,
    rows: usize,
    /// Where its cells start in `start`.
    first: usize,
}

impl Level {
    fn col(&self, x: f64) -> usize {
        ((x / self.side) as usize).min(self.cols - 1)
    }

    fn row(&self, y: f64) -> usize {
        ((y / self.side) as usize).min(self.rows - 1)
    }
}

impl EdgeGrid {
    /// The grid over the edges `(src[i], dst[i])` between nodes at
    /// `(x[v], y[v])`, in two passes over the edges. An edge with an end
    /// that is not a node, or at no finite position, is in no cell: it is
    /// never the nearest.
    pub fn build(src: &[u32], dst: &[u32], x: &[f32], y: &[f32]) -> Self {
        let m = src.len().min(dst.len());
        let ends = |i: usize| -> Option<(f64, f64, f64, f64)> {
            let (s, d) = (src[i] as usize, dst[i] as usize);
            let (ax, ay, bx, by) = (*x.get(s)?, *y.get(s)?, *x.get(d)?, *y.get(d)?);
            (ax.is_finite() && ay.is_finite() && bx.is_finite() && by.is_finite())
                .then(|| (f64::from(ax), f64::from(ay), f64::from(bx), f64::from(by)))
        };
        // The box around the nodes, which holds every edge.
        let (x0, y0, x1, y1) = match crate::scene::bounds(x, y) {
            Some(b) => (
                f64::from(b.0),
                f64::from(b.1),
                f64::from(b.2),
                f64::from(b.3),
            ),
            None => (0.0, 0.0, 0.0, 0.0),
        };
        let (w, h) = (x1 - x0, y1 - y0);
        let longest = w.max(h);
        // The finest cells: about four edges to a cell, were they spread
        // evenly, and at most 4096 to a side.
        let finest = if w > 0.0 && h > 0.0 {
            (w * h / (m as f64 / 4.0).max(1.0)).sqrt()
        } else {
            0.0
        };
        let mut side = finest.max(longest / MAX_SIDE as f64);
        if side <= 0.0 || !side.is_finite() {
            side = 1.0;
        }
        // Each level's cells are twice the last's, up to one cell that
        // holds whatever is left.
        let mut levels = Vec::new();
        let mut first = 0;
        loop {
            let dim = |len: f64| ((len / side).ceil() as usize).max(1);
            let (cols, rows) = (dim(w), dim(h));
            levels.push(Level {
                side,
                cols,
                rows,
                first,
            });
            first += cols * rows;
            if cols * rows == 1 {
                break;
            }
            side *= 2.0;
        }
        // Each edge's cell, then a counting sort by it.
        const NONE: u32 = u32::MAX;
        let top = levels.len() - 1;
        let mut start = vec![0u32; first + 1];
        let mut placed = 0;
        let keys: Vec<u32> = (0..m)
            .map(|i| {
                let Some((ax, ay, bx, by)) = ends(i) else {
                    return NONE;
                };
                let length = (bx - ax).abs().max((by - ay).abs());
                let level = &levels[levels.iter().position(|l| l.side >= length).unwrap_or(top)];
                let (mx, my) = ((ax + bx) * 0.5 - x0, (ay + by) * 0.5 - y0);
                let k = level.first + level.row(my) * level.cols + level.col(mx);
                start[k + 1] += 1;
                placed += 1;
                k as u32
            })
            .collect();
        for c in 0..first {
            start[c + 1] += start[c];
        }
        let mut next = start.clone();
        let mut order = vec![0u32; placed];
        for (i, &k) in keys.iter().enumerate() {
            if k != NONE {
                let at = &mut next[k as usize];
                order[*at as usize] = i as u32;
                *at += 1;
            }
        }
        Self {
            x0,
            y0,
            extent: x0.abs().max(y0.abs()).max(x1.abs()).max(y1.abs()),
            levels,
            start,
            order,
            n_edges: m,
        }
    }

    /// How many edges the grid was built over.
    pub fn len(&self) -> usize {
        self.n_edges
    }

    pub fn is_empty(&self) -> bool {
        self.n_edges == 0
    }

    /// The edges that may be within `tolerance` of the point `at`: every
    /// edge that is, and others. Each is given once.
    pub fn near(&self, at: (f32, f32), tolerance: f32) -> impl Iterator<Item = u32> + '_ {
        let (px, py) = (f64::from(at.0), f64::from(at.1));
        // What a test's rounding could add to a distance.
        let slack = 1e-5 * self.extent.max(px.abs()).max(py.abs());
        let tolerance = f64::from(tolerance);
        let (px, py) = (px - self.x0, py - self.y0);
        self.levels.iter().flat_map(move |level| {
            let reach = (level.side * 0.5 + tolerance) * 1.0001 + slack;
            // A NaN is every cell.
            let all = !(reach.is_finite() && px.is_finite() && py.is_finite());
            let (c0, c1, r0, r1) = if all {
                (0, level.cols - 1, 0, level.rows - 1)
            } else {
                (
                    level.col(px - reach),
                    level.col(px + reach),
                    level.row(py - reach),
                    level.row(py + reach),
                )
            };
            (r0..=r1).flat_map(move |r| {
                let from = self.start[level.first + r * level.cols + c0] as usize;
                let to = self.start[level.first + r * level.cols + c1 + 1] as usize;
                self.order[from..to].iter().copied()
            })
        })
    }

    /// [`nearest_edge`], testing only the edges the grid has near the
    /// point. The edges and positions are those the grid was built over.
    ///
    /// [`nearest_edge`]: crate::interact::nearest_edge
    #[allow(clippy::too_many_arguments)]
    pub fn nearest(
        &self,
        src: &[u32],
        dst: &[u32],
        x: &[f32],
        y: &[f32],
        skip: impl Fn(u32) -> bool,
        at: (f32, f32),
        tolerance: f32,
    ) -> Option<u32> {
        let mut best: Option<(u32, f32)> = None;
        for i in self.near(at, tolerance) {
            let e = i as usize;
            let Some(d2) = edge_distance2(src[e], dst[e], x, y, at, tolerance) else {
                continue;
            };
            let better = best.is_none_or(|(b, bd)| d2 < bd || (d2 == bd && i < b));
            if better && !skip(i) {
                best = Some((i, d2));
            }
        }
        best.map(|(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interact::nearest_edge;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        /// From 0 to 1.
        fn unit(&mut self) -> f32 {
            (self.next() % (1 << 24)) as f32 / (1 << 24) as f32
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Positions spread evenly over a box.
    fn random(n: usize, seed: u64) -> (Vec<f32>, Vec<f32>) {
        let mut rng = Rng(seed);
        let x = (0..n).map(|_| rng.unit() * 2000.0 - 700.0).collect();
        let y = (0..n).map(|_| rng.unit() * 900.0 + 50.0).collect();
        (x, y)
    }

    /// Positions in a few tight clusters, some on top of each other, with
    /// a few strays far away and a few that are not numbers.
    fn clustered(n: usize, seed: u64) -> (Vec<f32>, Vec<f32>) {
        let mut rng = Rng(seed);
        let centres: Vec<(f32, f32, f32)> = (0..6)
            .map(|k| {
                (
                    rng.unit() * 1000.0,
                    rng.unit() * 1000.0,
                    0.5 * 4f32.powi(k % 4),
                )
            })
            .collect();
        let (mut x, mut y) = (Vec::new(), Vec::new());
        for i in 0..n {
            let (cx, cy, spread) = centres[rng.below(centres.len())];
            // The sum of three is denser in the middle.
            let mut around = |c: f32| c + (rng.unit() + rng.unit() + rng.unit() - 1.5) * spread;
            let (px, py) = (around(cx), around(cy));
            match i % 97 {
                // On top of the node before.
                13 if i > 0 => {
                    x.push(x[i - 1]);
                    y.push(y[i - 1]);
                }
                29 => {
                    x.push(px * 50.0 - 9000.0);
                    y.push(py * -40.0);
                }
                31 if i % 2 == 0 => {
                    x.push(f32::NAN);
                    y.push(py);
                }
                _ => {
                    x.push(px);
                    y.push(py);
                }
            }
        }
        (x, y)
    }

    fn styles(n: usize, seed: u64) -> (Vec<f32>, Vec<u8>) {
        let mut rng = Rng(seed);
        let radius = (0..n)
            .map(|i| match i % 50 {
                0 => 24.0,
                _ => 2.0 + rng.unit() * 6.0,
            })
            .collect();
        let flags = (0..n)
            .map(|_| match rng.below(20) {
                0 => flag::HIDDEN,
                1 => flag::SELECTED,
                2 => flag::HIGHLIGHTED | flag::PINNED,
                3 => flag::DIMMED,
                _ => 0,
            })
            .collect();
        (radius, flags)
    }

    fn max_of(radius: &[f32]) -> f32 {
        radius.iter().copied().fold(0.0, f32::max)
    }

    /// Cameras from fitted to far in and far out, some off the drawing.
    fn cameras(x: &[f32], y: &[f32]) -> Vec<Camera> {
        let b = crate::scene::bounds(x, y).unwrap_or((0.0, 0.0, 1.0, 1.0));
        let mut fitted = Camera::new(1000.0, 700.0);
        fitted.fit(b);
        let mut all = vec![fitted];
        for (zoom, dx, dy) in [
            (0.05, 0.0, 0.0),
            (0.4, 300.0, -100.0),
            (3.0, -200.0, 50.0),
            (40.0, 10.0, 10.0),
            (2000.0, 0.0, 0.0),
        ] {
            let mut c = fitted;
            c.zoom(zoom, 500.0, 350.0);
            c.pan(dx, dy);
            all.push(c);
        }
        all
    }

    /// Pixels to point at: some anywhere, most on or beside a node.
    fn pixels(discs: &Discs, rng: &mut Rng, count: usize) -> Vec<(f32, f32)> {
        let n = discs.x.len();
        (0..count)
            .map(|k| {
                if k % 4 == 0 || n == 0 {
                    (rng.unit() * 1200.0 - 100.0, rng.unit() * 900.0 - 100.0)
                } else {
                    let i = rng.below(n);
                    let (qx, qy) = discs.camera.to_pixel(discs.x[i], discs.y[i]);
                    let off = discs.drawn_radius(i) + PICK_SLACK;
                    match k % 4 {
                        // Dead on, at the rim, and anywhere around it.
                        1 => (qx, qy),
                        2 => (qx + off, qy),
                        _ => (
                            qx + (rng.unit() * 2.4 - 1.2) * off,
                            qy + (rng.unit() * 2.4 - 1.2) * off,
                        ),
                    }
                }
            })
            .collect()
    }

    fn check_picks(x: &[f32], y: &[f32], seed: u64) -> usize {
        let n = x.len();
        let (radius, flags) = styles(n, seed);
        let grid = NodeGrid::build(x, y);
        assert_eq!(grid.len(), n);
        let mut rng = Rng(seed ^ 0xabcdef);
        let mut hits = 0;
        for camera in cameras(x, y) {
            for scale in [1.0, 0.15] {
                let discs = Discs {
                    x,
                    y,
                    radius: &radius,
                    flags: &flags,
                    camera: &camera,
                    scale,
                };
                for (px, py) in pixels(&discs, &mut rng, 300) {
                    let want = discs.pick_linear(px, py);
                    let got = discs.pick(&grid, n, max_of(&radius), px, py);
                    assert_eq!(got, want, "at ({px}, {py}) with {camera:?} x{scale}");
                    hits += usize::from(want.is_some());
                }
            }
        }
        hits
    }

    #[test]
    fn a_pick_with_the_grid_is_the_linear_pick_on_random_positions() {
        for (n, seed) in [(0, 1), (1, 2), (2, 3), (37, 4), (1000, 5), (20_000, 6)] {
            let (x, y) = random(n, seed);
            let hits = check_picks(&x, &y, seed);
            assert!(n < 37 || hits > 200, "{hits} hits of {n} nodes");
        }
    }

    #[test]
    fn a_pick_with_the_grid_is_the_linear_pick_on_clustered_positions() {
        for (n, seed) in [(50, 11), (3000, 12), (30_000, 13)] {
            let (x, y) = clustered(n, seed);
            let hits = check_picks(&x, &y, seed);
            assert!(hits > 200, "{hits} hits of {n} nodes");
        }
    }

    #[test]
    fn nodes_at_one_place_pick_the_first_that_is_not_hidden() {
        // Every node at one point, and all on one line.
        for (x, y) in [
            (vec![3.0f32; 500], vec![-2.0f32; 500]),
            ((0..500).map(|i| (i / 5) as f32).collect(), vec![7.0; 500]),
        ] {
            let radius = vec![4.0; 500];
            let mut flags = vec![0u8; 500];
            flags[0] = flag::HIDDEN;
            let grid = NodeGrid::build(&x, &y);
            let mut camera = Camera::new(800.0, 600.0);
            camera.center = (3.0, 0.0);
            camera.pixels_per_unit = 20.0;
            let discs = Discs {
                x: &x,
                y: &y,
                radius: &radius,
                flags: &flags,
                camera: &camera,
                scale: 1.0,
            };
            let (px, py) = camera.to_pixel(3.0, y[0]);
            let want = discs.pick_linear(px, py);
            assert!(want.is_some_and(|i| i > 0 && flags[i as usize] == 0));
            assert_eq!(discs.pick(&grid, 500, 4.0, px, py), want);
            assert!(check_picks(&x, &y, 21) > 0);
        }
    }

    #[test]
    fn nodes_moved_or_added_since_the_grid_are_found_by_the_linear_pass() {
        let (mut x, mut y) = clustered(4000, 31);
        let grid = NodeGrid::build(&x, &y);
        // The last 500 move, 300 are new, and then 3900 are left.
        let mut rng = Rng(32);
        for i in 3500..4000 {
            x[i] = rng.unit() * 1000.0;
            y[i] = rng.unit() * 1000.0;
        }
        for _ in 0..300 {
            x.push(rng.unit() * 1000.0);
            y.push(rng.unit() * 1000.0);
        }
        for (n, fresh) in [(4300, 3500), (3900, 3500), (3000, 3500), (4300, 0)] {
            let (x, y) = (&x[..n], &y[..n]);
            let (radius, flags) = styles(n, 33);
            for camera in cameras(x, y) {
                let discs = Discs {
                    x,
                    y,
                    radius: &radius,
                    flags: &flags,
                    camera: &camera,
                    scale: 0.5,
                };
                for (px, py) in pixels(&discs, &mut rng, 400) {
                    assert_eq!(
                        discs.pick(&grid, fresh, max_of(&radius), px, py),
                        discs.pick_linear(px, py),
                        "at ({px}, {py}), {n} nodes, {fresh} fresh"
                    );
                }
            }
        }
    }

    #[test]
    fn the_cells_of_a_box_hold_every_node_in_it() {
        for (x, y) in [random(5000, 41), clustered(5000, 42)] {
            let grid = NodeGrid::build(&x, &y);
            let (cols, rows) = grid.size();
            assert!(cols * rows <= 5000 && cols * rows >= 100, "{cols} x {rows}");
            let all = Cells {
                c0: 0,
                c1: cols - 1,
                r0: 0,
                r1: rows - 1,
            };
            assert_eq!(grid.count(all), 5000);
            let mut rng = Rng(43);
            for _ in 0..200 {
                let (ax, ay) = (rng.unit() * 2400.0 - 900.0, rng.unit() * 1200.0 - 100.0);
                let (w, h) = (rng.unit().powi(3) * 800.0, rng.unit().powi(3) * 800.0);
                let (x0, y0, x1, y1) = (ax, ay, ax + w, ay + h);
                let cells = grid.cells(x0.into(), y0.into(), x1.into(), y1.into());
                let mut found: Vec<u32> = grid.runs(cells).flatten().copied().collect();
                assert_eq!(found.len(), grid.count(cells));
                found.sort_unstable();
                found.dedup();
                assert_eq!(found.len(), grid.count(cells), "a node is in one cell");
                for i in 0..5000 {
                    if x[i] >= x0 && x[i] <= x1 && y[i] >= y0 && y[i] <= y1 {
                        assert!(found.binary_search(&(i as u32)).is_ok(), "node {i}");
                    }
                }
            }
        }
    }

    /// Edges: most between nodes near each other in index (short, in a
    /// cluster), some between any two, some loops, repeats and edges to
    /// nodes that are not there.
    fn edges(n: usize, m: usize, seed: u64) -> (Vec<u32>, Vec<u32>) {
        let mut rng = Rng(seed);
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for e in 0..m {
            let s = rng.below(n);
            let d = match e % 10 {
                0 => rng.below(n),
                1 => s,
                2 => n + 3,
                4..=6 => (s + 1024 + rng.below(3)) % n,
                _ => (s + 1 + rng.below(8)) % n,
            };
            src.push(s as u32);
            dst.push(d as u32);
            if e % 10 == 3 {
                // The same edge again, the other way.
                src.push(d as u32);
                dst.push(s as u32);
            }
        }
        (src, dst)
    }

    fn check_edges(x: &[f32], y: &[f32], m: usize, seed: u64) -> usize {
        let n = x.len();
        let (src, dst) = edges(n, m, seed);
        let grid = EdgeGrid::build(&src, &dst, x, y);
        assert_eq!(grid.len(), src.len());
        let mut rng = Rng(seed ^ 0x5555);
        let mut hits = 0;
        for k in 0..3000 {
            // On an edge, beside one, or anywhere.
            let e = rng.below(src.len());
            let (s, d) = (src[e] as usize, dst[e] as usize % n);
            let t = rng.unit();
            let tolerance = [0.01, 0.5, 4.0, 60.0, 3000.0][k % 5];
            let at = match k % 3 {
                0 => (rng.unit() * 2400.0 - 900.0, rng.unit() * 1200.0 - 100.0),
                1 => (x[s] + (x[d] - x[s]) * t, y[s] + (y[d] - y[s]) * t),
                _ => (
                    x[s] + (x[d] - x[s]) * t + (rng.unit() - 0.5) * 2.0 * tolerance,
                    y[s] + (y[d] - y[s]) * t + (rng.unit() - 0.5) * 2.0 * tolerance,
                ),
            };
            for skip_some in [false, true] {
                let skip = |e: u32| skip_some && e.is_multiple_of(3);
                let want = nearest_edge(&src, &dst, x, y, skip, at, tolerance);
                let got = grid.nearest(&src, &dst, x, y, skip, at, tolerance);
                assert_eq!(got, want, "at {at:?} within {tolerance}");
                hits += usize::from(want.is_some());
            }
        }
        hits
    }

    #[test]
    fn an_edge_pick_with_the_grid_is_the_linear_pick() {
        for (n, m, seed) in [(2, 1, 51), (40, 100, 52), (3000, 12_000, 53)] {
            let (x, y) = random(n, seed);
            let hits = check_edges(&x, &y, m, seed);
            assert!(hits > 1000, "{hits} hits, random, {n} nodes");
            let (x, y) = clustered(n, seed);
            let hits = check_edges(&x, &y, m, seed);
            assert!(hits > 1000, "{hits} hits, clustered, {n} nodes");
        }
    }

    #[test]
    fn edges_on_a_point_or_a_line_and_no_edges_at_all() {
        let none = EdgeGrid::build(&[], &[], &[0.0], &[0.0]);
        assert!(none.is_empty());
        assert_eq!(
            none.nearest(&[], &[], &[0.0], &[0.0], |_| false, (0.0, 0.0), 1.0),
            None
        );
        // Every node at one point: every edge is a point there.
        let (x, y) = (vec![5.0f32; 10], vec![5.0f32; 10]);
        assert!(check_edges(&x, &y, 30, 61) > 0);
        // All on one line.
        let x: Vec<f32> = (0..200).map(|i| i as f32 * 3.0).collect();
        let y = vec![-4.0f32; 200];
        assert!(check_edges(&x, &y, 600, 62) > 0);
    }

    #[test]
    fn a_long_edge_is_found_far_from_its_ends() {
        // A thousand short edges in a corner, and one edge across.
        let mut rng = Rng(71);
        let n = 2002;
        let mut x: Vec<f32> = (0..n).map(|_| rng.unit() * 10.0).collect();
        let mut y: Vec<f32> = (0..n).map(|_| rng.unit() * 10.0).collect();
        (x[2000], y[2000], x[2001], y[2001]) = (-5000.0, -5000.0, 5000.0, 5000.0);
        let mut src: Vec<u32> = (0..1000).map(|i| 2 * i).collect();
        let mut dst: Vec<u32> = (0..1000).map(|i| 2 * i + 1).collect();
        src.push(2000);
        dst.push(2001);
        let grid = EdgeGrid::build(&src, &dst, &x, &y);
        let at = (3000.0, 3000.5);
        assert_eq!(
            grid.nearest(&src, &dst, &x, &y, |_| false, at, 1.0),
            Some(1000)
        );
        // Far from the corner, the short edges are not even looked at.
        assert!(grid.near(at, 1.0).count() < 20);
        assert_eq!(grid.near((5.0, 5.0), 20_000.0).count(), 1001);
    }

    /// The acceptance numbers, at a million nodes: run it in a release
    /// build, where the times mean something.
    ///
    /// ```sh
    /// cargo test -p ursa-viz --no-default-features --release --lib spatial -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a timing, for a release build"]
    fn a_million_nodes_build_and_pick_in_time() {
        use std::time::Instant;
        let n = 1 << 20;
        // A layout: nodes on a jittered lattice, in index order.
        let mut rng = Rng(80);
        let lattice: (Vec<f32>, Vec<f32>) = (0..n)
            .map(|i| {
                let (c, r) = ((i % 1024) as f32, (i / 1024) as f32);
                (c + rng.unit() * 0.8, r + rng.unit() * 0.8)
            })
            .unzip();
        for (name, (x, y)) in [
            ("random", random(n, 81)),
            ("clustered", clustered(n, 82)),
            ("lattice", lattice),
        ] {
            let (radius, flags) = styles(n, 83);
            let build = (0..5)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(NodeGrid::build(&x, &y));
                    t.elapsed()
                })
                .min()
                .expect("five runs");
            let grid = NodeGrid::build(&x, &y);
            let camera = cameras(&x, &y)[0];
            let discs = Discs {
                x: &x,
                y: &y,
                radius: &radius,
                flags: &flags,
                camera: &camera,
                scale: 0.15,
            };
            let mut rng = Rng(84);
            let at: Vec<(f32, f32)> = (0..200)
                .map(|_| (rng.unit() * 1000.0, rng.unit() * 700.0))
                .collect();
            let t = Instant::now();
            let with: Vec<_> = at
                .iter()
                .map(|&(px, py)| discs.pick(&grid, n, 24.0, px, py))
                .collect();
            let pick = t.elapsed() / 200;
            let t = Instant::now();
            let without: Vec<_> = at
                .iter()
                .map(|&(px, py)| discs.pick_linear(px, py))
                .collect();
            let linear = t.elapsed() / 200;
            assert_eq!(with, without);
            // Edges to nodes near in index (near on the lattice, on the
            // row or the one above), a tenth of them to any node.
            let (m_src, m_dst) = edges(n, 1_000_000, 85);
            let t = Instant::now();
            let edge_grid = EdgeGrid::build(&m_src, &m_dst, &x, &y);
            let edge_build = t.elapsed();
            let world: Vec<(f32, f32)> =
                at.iter().map(|&(px, py)| camera.to_world(px, py)).collect();
            let reach = 4.0 / camera.pixels_per_unit;
            let t = Instant::now();
            let a: Vec<_> = world
                .iter()
                .map(|&p| edge_grid.nearest(&m_src, &m_dst, &x, &y, |_| false, p, reach))
                .collect();
            let edge_pick = t.elapsed() / 200;
            let t = Instant::now();
            let b: Vec<_> = world
                .iter()
                .map(|&p| nearest_edge(&m_src, &m_dst, &x, &y, |_| false, p, reach))
                .collect();
            let edge_linear = t.elapsed() / 200;
            assert_eq!(a, b);
            println!(
                "{name}: grid {build:?}, pick {pick:?} (linear {linear:?}); \
                 {} edges: grid {edge_build:?}, pick {edge_pick:?} (linear {edge_linear:?})",
                m_src.len()
            );
            if !cfg!(debug_assertions) {
                assert!(build.as_secs_f64() <= 0.015, "grid: {build:?}");
                assert!(pick.as_secs_f64() <= 0.0002, "pick: {pick:?}");
            }
        }
    }
}
