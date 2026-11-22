//! Rings and layers: layouts for a result whose nodes already have a level.
//!
//! A `hop` result gives every node its distance from a seed, and a set of
//! shortest paths gives every node its position along the path. Both say
//! where a node belongs before any force is computed, so these two layouts
//! are done in one pass and need no iteration to converge:
//!
//! - [`layout_rings`]: the seed at the centre and distance `d` on the circle
//!   of radius `d`. Several seeds give one ring system each, side by side.
//! - [`layout_layers`]: level `l` on the vertical line at `x = l`, so a path
//!   reads left to right. One path is a straight line.
//!
//! # The order on a ring or a layer
//!
//! A level only fixes one coordinate. The other, where a node sits on its
//! ring or its line, is chosen to shorten edges, by a fixed number of
//! barycentre sweeps (Sugiyama, Tagawa and Toda's heuristic, which is also
//! what keeps crossings low): out from the first level and back, each level
//! is ordered by the mean place of its nodes' neighbours on the level just
//! visited. Neighbours on the same level count for half; edges that skip a
//! level do not count. Equal means are ordered by node index.
//!
//! Each node then goes as near its mean as it can while keeping its distance
//! from the nodes beside it, so a node with one neighbour sits in line with
//! it and a full level is spaced evenly. That step is exact (pool adjacent
//! violators), not iterated.
//!
//! # Determinism
//!
//! Both are serial and read nothing but their arguments: the same input
//! gives the same bits on any thread count, with or without `rayon`.
//! [`layout_layers`] uses only `+ - * /`, so its bits are the same on every
//! platform too. [`layout_rings`] needs `sin`, `cos`, `atan2` and `asin`,
//! whose last bit belongs to the platform's libm: reproducible on one
//! machine, not promised across them.

use std::f64::consts::TAU;

use super::layout::Positions;
use crate::topology::UndirectedCsr;

/// The level of a node that has none: it is drawn after the last level
/// (outside the outermost ring, or on a line after the last layer).
pub const NO_LEVEL: u32 = u32::MAX;

/// Parameters for [`layout_rings`].
#[derive(Debug, Clone, Copy)]
pub struct RingsParams {
    /// The radius of the ring at distance 1, and the least step from one ring
    /// to the next. Distance `d` is at radius `d * spacing` unless a ring
    /// needs more room for its nodes.
    pub spacing: f32,
    /// The least distance between two nodes on a ring. A ring too short for
    /// its nodes at that distance gets a larger radius, and the rings outside
    /// it move out with it.
    pub gap: f32,
    /// Barycentre sweeps: each is one pass out from the centre and one back.
    pub sweeps: u32,
}

impl Default for RingsParams {
    fn default() -> Self {
        RingsParams {
            spacing: 1.0,
            gap: 0.25,
            sweeps: 4,
        }
    }
}

/// Parameters for [`layout_layers`].
#[derive(Debug, Clone, Copy)]
pub struct LayersParams {
    /// The least distance from one layer to the next. Layers are spread
    /// further when the tallest is more than twice as high as the drawing is
    /// wide.
    pub spacing: f32,
    /// The least distance between two nodes on a layer.
    pub gap: f32,
    /// Barycentre sweeps: each is one pass from the first layer to the last
    /// and one back.
    pub sweeps: u32,
}

impl Default for LayersParams {
    fn default() -> Self {
        LayersParams {
            spacing: 1.0,
            gap: 0.5,
            sweeps: 4,
        }
    }
}

/// Each node's level with [`NO_LEVEL`] replaced by the level after the last
/// one (0 if no node has a level).
fn effective_levels(level: &[u32]) -> Vec<u32> {
    let last = level.iter().copied().filter(|&l| l != NO_LEVEL).max();
    // `NO_LEVEL - 1` is the largest real level, so this cannot overflow.
    let after = last.map_or(0, |l| l + 1);
    level
        .iter()
        .map(|&l| if l == NO_LEVEL { after } else { l })
        .collect()
}

/// `x` minimising `Σ (x[i] - want[i])²` subject to `x[i + 1] - x[i] >= sep`,
/// appended to `out`: every node as near its wanted place as the spacing
/// allows, in the order given. Pool adjacent violators on `want[i] - i * sep`.
fn separate(want: &[f64], sep: f64, blocks: &mut Vec<(f64, u32)>, out: &mut Vec<f64>) {
    blocks.clear();
    for (i, &w) in want.iter().enumerate() {
        blocks.push((w - i as f64 * sep, 1));
        while blocks.len() >= 2 {
            let (s1, c1) = blocks[blocks.len() - 1];
            let (s0, c0) = blocks[blocks.len() - 2];
            if s0 / f64::from(c0) <= s1 / f64::from(c1) {
                break;
            }
            blocks.pop();
            *blocks.last_mut().expect("two blocks") = (s0 + s1, c0 + c1);
        }
    }
    let mut i = 0usize;
    for &(sum, count) in blocks.iter() {
        let mean = sum / f64::from(count);
        for _ in 0..count {
            out.push(mean + i as f64 * sep);
            i += 1;
        }
    }
}

/// Concentric rings by distance from a seed.
///
/// `distance[v]` is node `v`'s distance: 0 for a seed, [`NO_LEVEL`] for a
/// node that has none. `adj` is the drawn graph, whose edges decide the
/// order on each ring (see the [module docs](self)); it must have one node
/// per entry of `distance`.
///
/// - One seed is at the origin, and distance `d` is on the circle of radius
///   `d * spacing` around it. A ring with more nodes than fit `gap` apart
///   gets the radius it needs, and every ring outside it is at least
///   `spacing` further out. So two nodes are never nearer than `gap`.
/// - Several seeds give one ring system each, laid side by side in rows in
///   seed order. A node belongs to the system of the seed it is nearest
///   through the drawn edges: that of its neighbour one step nearer a seed,
///   the lowest-numbered seed when several tie. A node with a distance and
///   no such neighbour joins the first seed's system.
/// - No seed at all leaves the centre empty.
/// - Nodes without a distance are on one more ring outside the first
///   system's last.
///
/// Deterministic: see the [module docs](self).
pub fn layout_rings(adj: &UndirectedCsr, distance: &[u32], params: RingsParams) -> Positions {
    let n = distance.len();
    assert_eq!(
        adj.offsets.len(),
        n + 1,
        "one distance per node of the graph"
    );
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let spacing = f64::from(params.spacing.max(1e-4));
    let gap = f64::from(params.gap.max(1e-4));
    let level = effective_levels(distance);

    // Each node's ring system: a seed's own, or the lowest among its
    // neighbours one step in. Nodes are visited nearest first, so those are
    // set by then.
    const UNSET: u32 = u32::MAX;
    let mut by_level: Vec<u32> = (0..n as u32).collect();
    by_level.sort_unstable_by_key(|&v| (level[v as usize], v));
    let mut system = vec![UNSET; n];
    let mut n_systems = 0u32;
    for &v in &by_level {
        let l = level[v as usize];
        system[v as usize] = if l == 0 {
            n_systems += 1;
            n_systems - 1
        } else if distance[v as usize] == NO_LEVEL {
            0
        } else {
            adj.neighbors(v)
                .iter()
                .filter(|&&u| level[u as usize] == l - 1 && distance[u as usize] != NO_LEVEL)
                .map(|&u| system[u as usize])
                .min()
                .unwrap_or(0)
        };
    }
    let n_systems = n_systems.max(1) as usize;

    // The rings: runs of `order` with one system and one level, a system's
    // rings together and from the centre out.
    let mut order = by_level;
    order.sort_unstable_by_key(|&v| (system[v as usize], level[v as usize], v));
    let key_of = |v: u32| (system[v as usize], level[v as usize]);
    let mut starts = vec![0usize];
    for i in 1..n {
        if key_of(order[i]) != key_of(order[i - 1]) {
            starts.push(i);
        }
    }
    starts.push(n);
    let n_rings = starts.len() - 1;

    // Radii, and each system's outer radius.
    let mut radius = vec![0.0f64; n_rings];
    let mut outer = vec![0.0f64; n_systems];
    for r in 0..n_rings {
        let first = order[starts[r]];
        let (s, l) = key_of(first);
        let m = starts[r + 1] - starts[r];
        let inner = (r > 0 && system[order[starts[r - 1]] as usize] == s).then(|| radius[r - 1]);
        let fits = if m >= 2 {
            gap / (2.0 * (TAU / 2.0 / m as f64).sin())
        } else {
            0.0
        };
        let mut rad = (f64::from(l) * spacing).max(fits);
        if let Some(inner) = inner {
            rad = rad.max(inner + spacing);
        }
        radius[r] = rad;
        outer[s as usize] = rad;
    }

    // Every node starts evenly spaced on its ring, in index order.
    let mut angle = vec![0.0f64; n];
    let mut cos = vec![0.0f64; n];
    let mut sin = vec![0.0f64; n];
    // Whether the node has a direction from its system's centre: not the
    // seed in the middle.
    let mut placed = vec![false; n];
    for r in 0..n_rings {
        let ring = &order[starts[r]..starts[r + 1]];
        if radius[r] == 0.0 {
            continue;
        }
        for (i, &v) in ring.iter().enumerate() {
            let a = TAU * i as f64 / ring.len() as f64;
            angle[v as usize] = a;
            (sin[v as usize], cos[v as usize]) = a.sin_cos();
            placed[v as usize] = true;
        }
    }

    let mut scratch = RingScratch::default();
    let mut reorder = |r: usize, outward: bool, order: &mut [u32]| {
        let rad = radius[r];
        let ring = &mut order[starts[r]..starts[r + 1]];
        let m = ring.len();
        if rad == 0.0 {
            return;
        }
        // Each node's wanted angle: the direction of the sum of its
        // neighbours' unit vectors, from the old angles of them all.
        scratch.keyed.clear();
        let mut moved = false;
        for &v in ring.iter() {
            let (s, l) = key_of(v);
            let (mut sx, mut sy) = (0.0f64, 0.0f64);
            for &u in adj.neighbors(v) {
                if !placed[u as usize] || system[u as usize] != s {
                    continue;
                }
                let lu = level[u as usize];
                let w = if lu == l {
                    0.5
                } else if (outward && lu + 1 == l) || (!outward && lu == l + 1) {
                    1.0
                } else {
                    continue;
                };
                sx += w * cos[u as usize];
                sy += w * sin[u as usize];
            }
            let want = if sx * sx + sy * sy > 1e-18 {
                moved = true;
                let a = sy.atan2(sx);
                if a < 0.0 {
                    a + TAU
                } else {
                    a
                }
            } else {
                angle[v as usize]
            };
            scratch.keyed.push((want, v));
        }
        if !moved {
            return;
        }
        scratch
            .keyed
            .sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        // Cut the circle at the widest gap between wanted angles, so nodes
        // that belong together are not split by where the angle wraps.
        let mut cut = 0usize;
        let mut widest = scratch.keyed[0].0 + TAU - scratch.keyed[m - 1].0;
        for i in 1..m {
            let g = scratch.keyed[i].0 - scratch.keyed[i - 1].0;
            if g > widest {
                widest = g;
                cut = i;
            }
        }
        scratch.want.clear();
        for (i, slot) in ring.iter_mut().enumerate() {
            let j = (cut + i) % m;
            let (a, v) = scratch.keyed[j];
            scratch.want.push(if j < cut { a + TAU } else { a });
            *slot = v;
        }
        // Nodes two gaps apart where there is room, evenly spaced where
        // there is not.
        let even = TAU / m as f64;
        let least = 2.0 * (gap / (2.0 * rad)).min(1.0).asin();
        let sep = even.min(2.0 * least);
        scratch.at.clear();
        separate(&scratch.want, sep, &mut scratch.blocks, &mut scratch.at);
        if m >= 2 && scratch.at[m - 1] - scratch.at[0] > TAU - sep + 1e-12 {
            // They do not fit with the wrap between the last and the first:
            // evenly spaced, turned to be as near the wanted angles as can be.
            let mut turn = 0.0f64;
            for (i, &a) in scratch.want.iter().enumerate() {
                turn += a - i as f64 * even;
            }
            turn /= m as f64;
            for (i, at) in scratch.at.iter_mut().enumerate() {
                *at = turn + i as f64 * even;
            }
        }
        for (&v, &a) in ring.iter().zip(&scratch.at) {
            let a = a.rem_euclid(TAU);
            angle[v as usize] = a;
            (sin[v as usize], cos[v as usize]) = a.sin_cos();
        }
    };
    for _ in 0..params.sweeps {
        for r in 0..n_rings {
            reorder(r, true, &mut order);
        }
        for r in (0..n_rings).rev() {
            reorder(r, false, &mut order);
        }
    }

    let half: Vec<f64> = outer.iter().map(|&r| r + spacing * 0.5).collect();
    let centre = pack(&half);

    let mut x = vec![0.0f32; n];
    let mut y = vec![0.0f32; n];
    for r in 0..n_rings {
        let rad = radius[r];
        for &v in &order[starts[r]..starts[r + 1]] {
            let (cx, cy) = centre[system[v as usize] as usize];
            x[v as usize] = (cx + rad * cos[v as usize]) as f32;
            y[v as usize] = (cy + rad * sin[v as usize]) as f32;
        }
    }
    (x, y)
}

/// Where each ring system's centre goes: the systems side by side in rows,
/// in seed order, each in a square of half-width `half[s]`, the whole
/// centred on the origin. As few rows as leave the drawing no more than two
/// and a half times as wide as it is high: two systems side by side, four in
/// two rows.
fn pack(half: &[f64]) -> Vec<(f64, f64)> {
    let n = half.len();
    let mut centre = vec![(0.0f64, 0.0f64); n];
    if n <= 1 {
        return centre;
    }
    let total: f64 = half.iter().map(|&h| 2.0 * h).sum();
    let widest = half.iter().fold(0.0f64, |a, &h| a.max(2.0 * h));
    // Rows of at most `limit` (a system may reach half past it): each row's
    // end, and the drawing's size.
    let rows_within = |limit: f64, ends: &mut Vec<usize>| -> (f64, f64) {
        ends.clear();
        let (mut x, mut row_height) = (0.0f64, 0.0f64);
        let (mut width, mut height) = (0.0f64, 0.0f64);
        for (s, &h) in half.iter().enumerate() {
            if x > 0.0 && x + h > limit {
                ends.push(s);
                width = width.max(x);
                height += row_height;
                (x, row_height) = (0.0, 0.0);
            }
            x += 2.0 * h;
            row_height = row_height.max(2.0 * h);
        }
        ends.push(n);
        (width.max(x), height + row_height)
    };
    let mut ends = Vec::new();
    for rows in 1..=n {
        let (width, height) = rows_within(widest.max(total / rows as f64), &mut ends);
        if width <= 2.5 * height || ends.len() == n {
            break;
        }
    }
    let (mut y, mut from) = (0.0f64, 0usize);
    let (mut x0, mut x1) = (f64::MAX, f64::MIN);
    for &to in &ends {
        let height = half[from..to].iter().fold(0.0f64, |a, &h| a.max(2.0 * h));
        let mut x = 0.0f64;
        for s in from..to {
            centre[s] = (x + half[s], y - height * 0.5);
            x += 2.0 * half[s];
        }
        x0 = x0.min(0.0);
        x1 = x1.max(x);
        y -= height;
        from = to;
    }
    let (mx, my) = ((x0 + x1) * 0.5, y * 0.5);
    for c in &mut centre {
        c.0 -= mx;
        c.1 -= my;
    }
    centre
}

/// Buffers one ring or layer at a time is ordered in.
#[derive(Default)]
struct RingScratch {
    keyed: Vec<(f64, u32)>,
    want: Vec<f64>,
    at: Vec<f64>,
    blocks: Vec<(f64, u32)>,
}

/// Layers on parallel lines, left to right.
///
/// `layer[v]` is node `v`'s layer: its position along a path, or its
/// distance from the nearest of several seeds; [`NO_LEVEL`] for a node that
/// has none, which goes on a line after the last. `adj` is the drawn graph,
/// whose edges decide the order on each line (see the [module docs](self));
/// it must have one node per entry of `layer`.
///
/// Layer `l` is the vertical line at `x = l * spacing`, the first at the
/// left, and the nodes on it are at least `gap` apart. The drawing is centred
/// on the origin. When the tallest layer is more than twice as high as the
/// drawing is wide, the lines are spread until it is not, so a wide result
/// is not drawn as one column.
///
/// A single path (one node a layer) is a straight horizontal line.
///
/// Deterministic, and the same bits on every platform: see the
/// [module docs](self).
pub fn layout_layers(adj: &UndirectedCsr, layer: &[u32], params: LayersParams) -> Positions {
    let n = layer.len();
    assert_eq!(adj.offsets.len(), n + 1, "one layer per node of the graph");
    if n == 0 {
        return (Vec::new(), Vec::new());
    }
    let spacing = f64::from(params.spacing.max(1e-4));
    let gap = f64::from(params.gap.max(1e-4));
    let level = effective_levels(layer);

    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_unstable_by_key(|&v| (level[v as usize], v));
    let mut starts = vec![0usize];
    for i in 1..n {
        if level[order[i] as usize] != level[order[i - 1] as usize] {
            starts.push(i);
        }
    }
    starts.push(n);
    let n_layers = starts.len() - 1;

    // Every node starts in index order, its layer centred on the axis.
    let mut at = vec![0.0f64; n];
    for r in 0..n_layers {
        let line = &order[starts[r]..starts[r + 1]];
        let mid = (line.len() - 1) as f64 * 0.5;
        for (i, &v) in line.iter().enumerate() {
            at[v as usize] = (i as f64 - mid) * gap;
        }
    }

    let mut scratch = RingScratch::default();
    let mut reorder = |r: usize, forward: bool, order: &mut [u32]| {
        let line = &mut order[starts[r]..starts[r + 1]];
        scratch.keyed.clear();
        let mut moved = false;
        for &v in line.iter() {
            let l = level[v as usize];
            let (mut sum, mut weight) = (0.0f64, 0.0f64);
            for &u in adj.neighbors(v) {
                let lu = level[u as usize];
                let w = if lu == l {
                    0.5
                } else if (forward && lu.checked_add(1) == Some(l))
                    || (!forward && l.checked_add(1) == Some(lu))
                {
                    1.0
                } else {
                    continue;
                };
                sum += w * at[u as usize];
                weight += w;
            }
            let want = if weight > 0.0 {
                moved = true;
                sum / weight
            } else {
                at[v as usize]
            };
            scratch.keyed.push((want, v));
        }
        if !moved {
            return;
        }
        scratch
            .keyed
            .sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        scratch.want.clear();
        for (slot, &(want, v)) in line.iter_mut().zip(&scratch.keyed) {
            scratch.want.push(want);
            *slot = v;
        }
        scratch.at.clear();
        separate(&scratch.want, gap, &mut scratch.blocks, &mut scratch.at);
        for (&v, &a) in line.iter().zip(&scratch.at) {
            at[v as usize] = a;
        }
    };
    for _ in 0..params.sweeps {
        for r in 0..n_layers {
            reorder(r, true, &mut order);
        }
        for r in (0..n_layers).rev() {
            reorder(r, false, &mut order);
        }
    }

    let (lo, hi) = at
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let first = level[order[0] as usize];
    let span = f64::from(level[order[n - 1] as usize] - first);
    // No more than twice as high as wide.
    let step = if span > 0.0 {
        spacing.max((hi - lo) / (2.0 * span))
    } else {
        spacing
    };
    let mid = (lo + hi) * 0.5;
    let mut x = vec![0.0f32; n];
    let mut y = vec![0.0f32; n];
    for v in 0..n {
        x[v] = ((f64::from(level[v] - first) - span * 0.5) * step) as f32;
        // The first node of a layer at the top.
        y[v] = (mid - at[v]) as f32;
    }
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::Topology;

    /// Zachary's karate club: 34 nodes, 78 edges.
    const KARATE: [(u32, u32); 78] = [
        (0, 1),
        (0, 2),
        (0, 3),
        (0, 4),
        (0, 5),
        (0, 6),
        (0, 7),
        (0, 8),
        (0, 10),
        (0, 11),
        (0, 12),
        (0, 13),
        (0, 17),
        (0, 19),
        (0, 21),
        (0, 31),
        (1, 2),
        (1, 3),
        (1, 7),
        (1, 13),
        (1, 17),
        (1, 19),
        (1, 21),
        (1, 30),
        (2, 3),
        (2, 7),
        (2, 8),
        (2, 9),
        (2, 13),
        (2, 27),
        (2, 28),
        (2, 32),
        (3, 7),
        (3, 12),
        (3, 13),
        (4, 6),
        (4, 10),
        (5, 6),
        (5, 10),
        (5, 16),
        (6, 16),
        (8, 30),
        (8, 32),
        (8, 33),
        (9, 33),
        (13, 33),
        (14, 32),
        (14, 33),
        (15, 32),
        (15, 33),
        (18, 32),
        (18, 33),
        (19, 33),
        (20, 32),
        (20, 33),
        (22, 32),
        (22, 33),
        (23, 25),
        (23, 27),
        (23, 29),
        (23, 32),
        (23, 33),
        (24, 25),
        (24, 27),
        (24, 31),
        (25, 31),
        (26, 29),
        (26, 33),
        (27, 33),
        (28, 31),
        (28, 33),
        (29, 32),
        (29, 33),
        (30, 32),
        (30, 33),
        (31, 32),
        (31, 33),
        (32, 33),
    ];

    fn graph(n: usize, edges: &[(u32, u32)]) -> Topology {
        Topology::build(
            n,
            edges.iter().map(|e| e.0).collect(),
            edges.iter().map(|e| e.1).collect(),
        )
    }

    /// BFS distances over the undirected graph, `NO_LEVEL` past `max`.
    fn distances(t: &Topology, seeds: &[u32], max: u32) -> Vec<u32> {
        let adj = t.undirected();
        let mut d = vec![NO_LEVEL; t.n_nodes()];
        let mut frontier: Vec<u32> = seeds.to_vec();
        for &s in seeds {
            d[s as usize] = 0;
        }
        for step in 1..=max {
            let mut next = Vec::new();
            for &v in &frontier {
                for &u in adj.neighbors(v) {
                    if d[u as usize] == NO_LEVEL {
                        d[u as usize] = step;
                        next.push(u);
                    }
                }
            }
            frontier = next;
        }
        d
    }

    /// The nodes with a level, and the edges among them, renumbered: what a
    /// `hop` result draws. Returns the sub-topology, its levels and each
    /// node's original index.
    fn induced(t: &Topology, level: &[u32]) -> (Topology, Vec<u32>, Vec<u32>) {
        let keep: Vec<u32> = (0..t.n_nodes() as u32)
            .filter(|&v| level[v as usize] != NO_LEVEL)
            .collect();
        let mut dense = vec![u32::MAX; t.n_nodes()];
        for (i, &v) in keep.iter().enumerate() {
            dense[v as usize] = i as u32;
        }
        let adj = t.undirected();
        let mut edges = Vec::new();
        for &v in &keep {
            for &u in adj.neighbors(v) {
                if v < u && dense[u as usize] != u32::MAX {
                    edges.push((dense[v as usize], dense[u as usize]));
                }
            }
        }
        let levels = keep.iter().map(|&v| level[v as usize]).collect();
        (graph(keep.len(), &edges), levels, keep)
    }

    fn min_distance(x: &[f32], y: &[f32]) -> f32 {
        let mut least = f32::MAX;
        for i in 0..x.len() {
            for j in i + 1..x.len() {
                least = least.min((x[i] - x[j]).hypot(y[i] - y[j]));
            }
        }
        least
    }

    /// How many pairs of edges between consecutive layers cross.
    fn crossings(edges: &[(u32, u32)], x: &[f32], y: &[f32]) -> usize {
        let mut count = 0;
        for (i, &(a, b)) in edges.iter().enumerate() {
            let (a, b) = if x[a as usize] <= x[b as usize] {
                (a, b)
            } else {
                (b, a)
            };
            for &(c, d) in &edges[i + 1..] {
                let (c, d) = if x[c as usize] <= x[d as usize] {
                    (c, d)
                } else {
                    (d, c)
                };
                if x[a as usize] != x[c as usize] || x[b as usize] != x[d as usize] {
                    continue;
                }
                let left = y[a as usize] - y[c as usize];
                let right = y[b as usize] - y[d as usize];
                if left * right < 0.0 {
                    count += 1;
                }
            }
        }
        count
    }

    fn bits(p: &Positions) -> (Vec<u32>, Vec<u32>) {
        (
            p.0.iter().map(|v| v.to_bits()).collect(),
            p.1.iter().map(|v| v.to_bits()).collect(),
        )
    }

    /// `f` on a pool of `threads` workers (without `rayon`, serially).
    fn on_pool<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
        #[cfg(feature = "rayon")]
        {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("thread pool builds")
                .install(f)
        }
        #[cfg(not(feature = "rayon"))]
        {
            let _ = threads;
            f()
        }
    }

    #[test]
    fn karate_two_hops_from_node_0_is_two_rings() {
        let t = graph(34, &KARATE);
        let d = distances(&t, &[0], 2);
        let (sub, level, keep) = induced(&t, &d);
        assert_eq!(keep.len(), 26, "node 0, 16 neighbours and 9 at two hops");
        let params = RingsParams::default();
        let (x, y) = layout_rings(sub.undirected(), &level, params);
        // Ring membership: distance d is at radius d.
        let mut on_ring = [0usize; 3];
        for v in 0..keep.len() {
            let r = x[v].hypot(y[v]);
            assert!(
                (r - level[v] as f32).abs() < 1e-5,
                "node {} at distance {} has radius {r}",
                keep[v],
                level[v]
            );
            on_ring[level[v] as usize] += 1;
        }
        assert_eq!(on_ring, [1, 16, 9]);
        // No node on another: never nearer than the gap.
        let least = min_distance(&x, &y);
        assert!(least >= params.gap - 1e-5, "nearest pair {least} apart");
    }

    #[test]
    fn karate_all_shortest_paths_from_16_to_26_are_six_layers() {
        let t = graph(34, &KARATE);
        // The shortest-path DAG: nodes with d(16, v) + d(v, 26) = d(16, 26).
        let from = distances(&t, &[16], 34);
        let to = distances(&t, &[26], 34);
        let total = from[26];
        assert_eq!(total, 5);
        let layer: Vec<u32> = (0..34)
            .map(|v| {
                if from[v] + to[v] == total {
                    from[v]
                } else {
                    NO_LEVEL
                }
            })
            .collect();
        let (full, level, keep) = induced(&t, &layer);
        // Only the edges of the paths: from one layer to the next.
        let mut edges = Vec::new();
        for v in 0..keep.len() as u32 {
            for &u in full.undirected().neighbors(v) {
                if level[u as usize] == level[v as usize] + 1 {
                    edges.push((v, u));
                }
            }
        }
        let sub = graph(keep.len(), &edges);
        let params = LayersParams::default();
        let (x, y) = layout_layers(sub.undirected(), &level, params);
        let mut columns: Vec<u32> = x.iter().map(|v| v.to_bits()).collect();
        columns.sort_unstable();
        columns.dedup();
        assert_eq!(columns.len(), 6, "six layers: {x:?}");
        for v in 0..keep.len() {
            assert_eq!(
                x[v],
                level[v] as f32 - 2.5,
                "layer {} of node {}",
                level[v],
                keep[v]
            );
        }
        // The ends are alone on their layers, and on the axis's side of the
        // nodes they lead to.
        let at = |node: u32| keep.iter().position(|&k| k == node).unwrap();
        assert_eq!((level[at(16)], level[at(26)]), (0, 5));
        assert!(min_distance(&x, &y) >= params.gap - 1e-5);
        assert_eq!(crossings(&edges, &x, &y), 0, "the DAG is drawn planar");
    }

    #[test]
    fn a_route_is_a_straight_line_left_to_right() {
        // One path, its nodes in no index order: 3 - 0 - 4 - 1 - 2.
        let path = [3u32, 0, 4, 1, 2];
        let edges: Vec<(u32, u32)> = path.windows(2).map(|w| (w[0], w[1])).collect();
        let t = graph(5, &edges);
        let mut layer = vec![0u32; 5];
        for (i, &v) in path.iter().enumerate() {
            layer[v as usize] = i as u32;
        }
        let (x, y) = layout_layers(t.undirected(), &layer, LayersParams::default());
        assert_eq!(y, vec![0.0; 5]);
        let along: Vec<f32> = path.iter().map(|&v| x[v as usize]).collect();
        assert_eq!(along, vec![-2.0, -1.0, 0.0, 1.0, 2.0]);
    }

    #[test]
    fn the_same_input_gives_the_same_bits_on_any_thread_count() {
        let t = graph(34, &KARATE);
        let d = distances(&t, &[0, 33], 3);
        let rings = bits(&layout_rings(t.undirected(), &d, RingsParams::default()));
        let layers = bits(&layout_layers(t.undirected(), &d, LayersParams::default()));
        for threads in [1, 2, 8] {
            let (r, l) = on_pool(threads, || {
                (
                    bits(&layout_rings(t.undirected(), &d, RingsParams::default())),
                    bits(&layout_layers(t.undirected(), &d, LayersParams::default())),
                )
            });
            assert_eq!(r, rings, "rings on {threads} threads");
            assert_eq!(l, layers, "layers on {threads} threads");
        }
    }

    /// Pinned: a change to either layout shows here. Layers are exact on
    /// every platform; rings to the last bit of this one's libm, so they are
    /// compared to a rounding.
    #[test]
    fn the_layouts_are_pinned() {
        // A seed, three neighbours, and two children of the middle one.
        let t = graph(6, &[(0, 1), (0, 2), (0, 3), (2, 4), (2, 5)]);
        let level = [0, 1, 1, 1, 2, 2];
        let (x, y) = layout_layers(t.undirected(), &level, LayersParams::default());
        assert_eq!(x, vec![-1.0, 0.0, 0.0, 0.0, 1.0, 1.0]);
        assert_eq!(y, vec![0.0, 0.5, 0.0, -0.5, 0.25, -0.25]);
        let (x, y) = layout_rings(t.undirected(), &level, RingsParams::default());
        let round = |v: &[f32]| -> Vec<f32> { v.iter().map(|a| (a * 1e4).round() / 1e4).collect() };
        // Ring 1 evenly spaced from angle 0; node 2's children on ring 2,
        // either side of its direction (120°), two gaps apart.
        assert_eq!(round(&x[..4]), vec![0.0, 1.0, -0.5, -0.5]);
        assert_eq!(round(&y[..4]), vec![0.0, 0.0, 0.866, -0.866]);
        let mid = ((x[4] + x[5]) / 2.0, (y[4] + y[5]) / 2.0);
        let dir = mid.1.atan2(mid.0).to_degrees();
        assert!((dir - 120.0).abs() < 1e-3, "children at {dir}°");
        let apart = (x[4] - x[5]).hypot(y[4] - y[5]);
        assert!((apart - 0.5).abs() < 2e-3, "children {apart} apart");
        for v in 4..6 {
            assert!((x[v].hypot(y[v]) - 2.0).abs() < 1e-5);
        }
    }

    #[test]
    fn barycentre_sweeps_remove_the_crossings_of_a_shuffled_ladder() {
        // Four layers of 12; node i of each layer tied to node p(i) of the
        // next by a fixed shuffle, so index order crosses heavily.
        let (layers, width) = (4u32, 12u32);
        let shuffle = |i: u32, l: u32| (i * 5 + 3 * l + 1) % width;
        let mut edges = Vec::new();
        for l in 0..layers - 1 {
            for i in 0..width {
                edges.push((l * width + i, (l + 1) * width + shuffle(i, l)));
            }
        }
        let t = graph((layers * width) as usize, &edges);
        let level: Vec<u32> = (0..layers * width).map(|v| v / width).collect();
        let unswept = LayersParams {
            sweeps: 0,
            ..LayersParams::default()
        };
        let (x, y) = layout_layers(t.undirected(), &level, unswept);
        assert!(crossings(&edges, &x, &y) > 50, "index order is tangled");
        let (x, y) = layout_layers(t.undirected(), &level, LayersParams::default());
        assert_eq!(crossings(&edges, &x, &y), 0);
        // Disjoint paths: every one is drawn straight.
        for &(a, b) in &edges {
            assert_eq!(y[a as usize], y[b as usize]);
        }
        assert!(min_distance(&x, &y) >= 0.5 - 1e-6);
    }

    #[test]
    fn a_crowded_ring_grows_and_pushes_the_rings_outside_it() {
        // A star of 200 leaves, and one node two hops out.
        let mut edges: Vec<(u32, u32)> = (1..=200).map(|v| (0, v)).collect();
        edges.push((200, 201));
        let t = graph(202, &edges);
        let mut level = vec![1u32; 202];
        level[0] = 0;
        level[201] = 2;
        let params = RingsParams::default();
        let (x, y) = layout_rings(t.undirected(), &level, params);
        let r1 = x[1].hypot(y[1]);
        assert!(
            r1 > 7.9 && r1 < 8.0,
            "200 nodes a gap apart need radius {r1}"
        );
        for v in 1..=200 {
            assert!((x[v].hypot(y[v]) - r1).abs() < 1e-4);
        }
        let r2 = x[201].hypot(y[201]);
        assert!((r2 - (r1 + params.spacing)).abs() < 1e-4);
        assert!(min_distance(&x, &y) >= params.gap - 1e-4);
        // The far node is in line with the leaf it hangs from.
        let cross = x[200] * y[201] - y[200] * x[201];
        assert!(cross.abs() < 1e-3, "not in line: {cross}");
    }

    #[test]
    fn several_seeds_get_a_ring_system_each_side_by_side() {
        let t = graph(34, &KARATE);
        let d = distances(&t, &[0, 33, 16], 1);
        let (sub, level, keep) = induced(&t, &d);
        let params = RingsParams::default();
        let (x, y) = layout_rings(sub.undirected(), &level, params);
        let at = |node: u32| keep.iter().position(|&k| k == node).unwrap();
        let seeds = [at(0), at(16), at(33)];
        // Every other node is on the unit circle of exactly one seed, and
        // that seed is a neighbour.
        for v in 0..keep.len() {
            if level[v] == 0 {
                continue;
            }
            let on: Vec<usize> = seeds
                .iter()
                .copied()
                .filter(|&s| ((x[v] - x[s]).hypot(y[v] - y[s]) - 1.0).abs() < 1e-4)
                .collect();
            assert_eq!(on.len(), 1, "node {} is on {on:?}", keep[v]);
            assert!(sub
                .undirected()
                .neighbors(v as u32)
                .contains(&(on[0] as u32)));
        }
        // The systems do not overlap: seeds at least their radii and a
        // spacing apart. Node 16's neighbours are nearer node 0's system, so
        // it is alone.
        let radius = [1.0f32, 0.0, 1.0];
        for (i, &a) in seeds.iter().enumerate() {
            for (j, &b) in seeds.iter().enumerate().skip(i + 1) {
                let apart = (x[a] - x[b]).hypot(y[a] - y[b]);
                assert!(
                    apart >= radius[i] + radius[j] + 1.0 - 1e-4,
                    "seeds {apart} apart"
                );
            }
        }
        assert!(min_distance(&x, &y) >= params.gap - 1e-5);
        // Side by side, in seed order: node 0's, node 16 alone, node 33's.
        assert!(x[seeds[0]] < x[seeds[1]] && x[seeds[1]] < x[seeds[2]]);
        assert_eq!(y[seeds[0]], y[seeds[2]]);
        // Centred on the origin.
        let (lo, hi) = x
            .iter()
            .fold((f32::MAX, f32::MIN), |(a, b), &v| (a.min(v), b.max(v)));
        assert!((lo + hi).abs() < 1.0 + 1e-4);
    }

    #[test]
    fn ring_systems_pack_in_rows() {
        // Two side by side; four in two rows of two; a row's systems share
        // its middle line.
        assert_eq!(pack(&[1.5, 1.5]), vec![(-1.5, 0.0), (1.5, 0.0)]);
        assert_eq!(
            pack(&[1.5; 4]),
            vec![(-1.5, 1.5), (1.5, 1.5), (-1.5, -1.5), (1.5, -1.5)]
        );
        assert_eq!(pack(&[2.0, 0.5]), vec![(-0.5, 0.0), (2.0, 0.0)]);
        assert_eq!(pack(&[1.0]), vec![(0.0, 0.0)]);
        // Many: no wider than two and a half times its height, nor a column.
        let c = pack(&[1.0; 40]);
        let (w, h) = c.iter().fold((0.0f64, 0.0f64), |(w, h), p| {
            (w.max(p.0.abs() * 2.0 + 2.0), h.max(p.1.abs() * 2.0 + 2.0))
        });
        assert!(w <= 2.5 * h && w >= h, "{w} by {h}");
    }

    #[test]
    fn nodes_without_a_level_go_after_the_last() {
        let t = graph(5, &[(0, 1), (1, 2), (3, 4)]);
        let level = [0, 1, 2, NO_LEVEL, NO_LEVEL];
        let (x, y) = layout_rings(t.undirected(), &level, RingsParams::default());
        for v in 3..5 {
            assert!((x[v].hypot(y[v]) - 3.0).abs() < 1e-5);
        }
        let (x, _) = layout_layers(t.undirected(), &level, LayersParams::default());
        assert_eq!(x, vec![-1.5, -0.5, 0.5, 1.5, 1.5]);
        // Nothing has a level: one ring, one layer.
        let none = [NO_LEVEL; 5];
        let (x, y) = layout_rings(t.undirected(), &none, RingsParams::default());
        assert!(min_distance(&x, &y) >= 0.25 - 1e-5);
        let (x, y) = layout_layers(t.undirected(), &none, LayersParams::default());
        assert_eq!(x, vec![0.0; 5]);
        assert!(min_distance(&x, &y) >= 0.5 - 1e-6);
    }

    #[test]
    fn empty_and_single_node_graphs() {
        let t = graph(0, &[]);
        assert_eq!(
            layout_rings(t.undirected(), &[], RingsParams::default()),
            (vec![], vec![])
        );
        assert_eq!(
            layout_layers(t.undirected(), &[], LayersParams::default()),
            (vec![], vec![])
        );
        let t = graph(1, &[]);
        assert_eq!(
            layout_rings(t.undirected(), &[0], RingsParams::default()),
            (vec![0.0], vec![0.0])
        );
        assert_eq!(
            layout_layers(t.undirected(), &[7], LayersParams::default()),
            (vec![0.0], vec![0.0])
        );
    }

    #[test]
    fn a_tall_layer_spreads_the_layers() {
        // Two layers: one node, then 41 a gap of 0.5 apart: 20 high.
        let edges: Vec<(u32, u32)> = (1..=41).map(|v| (0, v)).collect();
        let t = graph(42, &edges);
        let mut level = vec![1u32; 42];
        level[0] = 0;
        let (x, y) = layout_layers(t.undirected(), &level, LayersParams::default());
        let height = y.iter().fold(0.0f32, |a, &v| a.max(v.abs())) * 2.0;
        assert_eq!(height, 20.0);
        assert_eq!(x[1] - x[0], 10.0, "half as wide as it is high");
        assert_eq!(
            y[0], 0.0,
            "the root is level with the middle of its children"
        );
    }
}
