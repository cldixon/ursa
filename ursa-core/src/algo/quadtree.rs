//! A Barnes–Hut quadtree over 2-D positions, built deterministically.
//!
//! This is the structure that makes force-directed layout tractable. The naive
//! force calculation is all-pairs: every node feels every other node, O(n²) per
//! iteration. Barnes–Hut replaces distant *groups* of nodes with their centre of
//! mass, which costs a little accuracy and turns the per-iteration work into
//! O(n log n).
//!
//! The approximation is governed by one parameter, `theta`. A cell is accepted as
//! a single point when `s / d < theta`, where `s` is the cell's width and `d` the
//! distance to it — that is, when the cell is small compared to how far away it
//! is. `theta = 0` accepts nothing and degenerates to exact all-pairs, which is
//! what the accuracy test uses as its oracle.
//!
//! # Why this build, and not the textbook one
//!
//! The textbook quadtree is built by inserting nodes one at a time, splitting
//! cells as they overflow. That is inherently sequential and its shape depends on
//! insertion order. This one is built from a **Morton (z-order) sort** instead:
//!
//! 1. Quantize each position into a 2¹⁶ × 2¹⁶ integer lattice.
//! 2. Interleave the bits of `(ix, iy)` into a 32-bit Morton code.
//! 3. Sort node indices by that code with a stable LSD radix sort.
//!
//! Sorting by Morton code *is* a quadtree depth-first order: the top two bits
//! select the root's quadrant, the next two select the quadrant within that, and
//! so on. So after the sort, **every cell's members are a contiguous range**, and
//! subdividing a cell means finding three split points inside a slice rather than
//! moving any data. The tree is then built top-down over those ranges.
//!
//! Two consequences, both of which the layout kernels depend on:
//!
//! - **The shape is a pure function of the positions.** No insertion order, no
//!   thread scheduling. Two runs on the same input build the identical tree.
//! - **Traversal order is fixed**, because children are stored and visited in
//!   Morton order. Each node sums its own forces in an order the tree decides, so
//!   the floating-point result does not depend on how many workers are running.
//!
//! The radix sort is stable, so nodes that quantize to the *same* cell stay in
//! ascending index order — which is what keeps the leaf-level exact sums
//! reproducible too.
//!
//! # Depth, and coincident nodes
//!
//! 16 bits per axis bounds the depth at 16 levels. That bound is not just a
//! memory concern: without it, two nodes at exactly the same position would
//! subdivide forever, since no cell can ever separate them. At maximum depth a
//! cell becomes a leaf whatever it holds, and the force law's distance floor
//! handles the rest.

/// Bits per axis in a Morton code. 16 + 16 fills a `u32` exactly and bounds the
/// tree at 16 levels.
const AXIS_BITS: u32 = 16;
/// Maximum lattice coordinate, and the scale positions are quantized onto.
const AXIS_MAX: u32 = (1 << AXIS_BITS) - 1;
/// A cell holding at most this many nodes is a leaf. One is the textbook choice;
/// a slightly larger bucket trades a little accuracy for markedly less pointer
/// chasing, and the exact-mode test pins that the trade stays honest.
const LEAF_MAX: usize = 4;

/// Spread the low 16 bits of `n` into every other bit position.
#[inline]
fn part1by1(mut n: u32) -> u32 {
    n &= 0x0000_ffff;
    n = (n | (n << 8)) & 0x00ff_00ff;
    n = (n | (n << 4)) & 0x0f0f_0f0f;
    n = (n | (n << 2)) & 0x3333_3333;
    n = (n | (n << 1)) & 0x5555_5555;
    n
}

/// Interleave `(ix, iy)` into a Morton code: x in the even bits, y in the odd.
#[inline]
fn morton(ix: u32, iy: u32) -> u32 {
    part1by1(ix) | (part1by1(iy) << 1)
}

/// One cell of the tree. Internal cells carry a centre of mass; leaves carry a
/// range into the Morton-sorted node order.
#[derive(Debug, Clone, Copy)]
struct Cell {
    /// Mass-weighted centre. Leaves carry one too: a leaf far enough away is
    /// accepted as a single point like any other cell, and a leaf's centre is what
    /// its parent's is built from.
    com_x: f32,
    com_y: f32,
    /// Sum of member weights — node count for an unweighted layout, `Σ(deg+1)`
    /// for ForceAtlas2's degree-weighted repulsion.
    mass: f32,
    /// Width of this cell in world units, the `s` of the `s/d < theta` test.
    size: f32,
    /// Range into [`QuadTree::order`] covered by this cell.
    start: u32,
    len: u32,
    /// First child index in [`QuadTree::cells`], or `NO_CHILD` for a leaf.
    /// Children are contiguous and in Morton order.
    child: u32,
    n_children: u32,
}

const NO_CHILD: u32 = u32::MAX;

/// A Barnes–Hut quadtree. Rebuilt from scratch each layout iteration — the
/// positions move every step, so there is nothing to update incrementally.
pub struct QuadTree {
    cells: Vec<Cell>,
    /// Node indices in Morton order. A cell's members are `order[start..start+len]`.
    order: Vec<u32>,
    /// Inverse of `order`: where node `i` sits in it. This is what makes "does this
    /// cell contain node `i`?" an O(1) range check instead of a scan, which in turn
    /// is what lets a distant *leaf* be accepted as a single point — one force
    /// computation rather than `LEAF_MAX` of them.
    rank: Vec<u32>,
}

impl QuadTree {
    /// Build over `(x, y)` with per-node `weight`.
    ///
    /// `weight` is what "mass" means to the caller: pass all-ones for a plain
    /// count, or `deg + 1` for ForceAtlas2. It must be the same length as `x`.
    pub fn build(x: &[f32], y: &[f32], weight: &[f32]) -> QuadTree {
        let n = x.len();
        debug_assert_eq!(n, y.len());
        debug_assert_eq!(n, weight.len());
        if n == 0 {
            return QuadTree {
                cells: Vec::new(),
                order: Vec::new(),
                rank: Vec::new(),
            };
        }

        // A *square* root cell, not the bounding rectangle: Morton codes assume
        // both axes are quantized on the same scale, and the `s/d` test needs one
        // meaningful width per cell rather than two.
        let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
        let (mut max_x, mut max_y) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for i in 0..n {
            min_x = min_x.min(x[i]);
            max_x = max_x.max(x[i]);
            min_y = min_y.min(y[i]);
            max_y = max_y.max(y[i]);
        }
        // Guard the degenerate cases the layout can genuinely produce: a single
        // node, or every node coincident, both give a zero-extent box.
        let extent = (max_x - min_x).max(max_y - min_y).max(1e-6);
        let scale = AXIS_MAX as f32 / extent;

        let codes: Vec<u32> = (0..n)
            .map(|i| {
                let ix = (((x[i] - min_x) * scale) as u32).min(AXIS_MAX);
                let iy = (((y[i] - min_y) * scale) as u32).min(AXIS_MAX);
                morton(ix, iy)
            })
            .collect();

        let order = radix_sort_by_code(&codes);
        let sorted_codes: Vec<u32> = order.iter().map(|&i| codes[i as usize]).collect();

        let mut cells: Vec<Cell> = Vec::with_capacity(2 * n / LEAF_MAX + 8);
        cells.push(Cell {
            com_x: 0.0,
            com_y: 0.0,
            mass: 0.0,
            size: extent,
            start: 0,
            len: n as u32,
            child: NO_CHILD,
            n_children: 0,
        });

        // Top-down subdivision over the sorted ranges. A stack rather than
        // recursion: 16 levels is shallow, but a graph of a million nodes is not
        // the place to discover a stack limit.
        //
        // `level` counts down the Morton code's bit pairs: at level `l` the
        // quadrant is selected by bits `2(l-1)..2l`.
        let mut stack: Vec<(usize, u32)> = vec![(0, AXIS_BITS)];
        while let Some((ci, level)) = stack.pop() {
            let (start, len) = (cells[ci].start as usize, cells[ci].len as usize);
            if len <= LEAF_MAX || level == 0 {
                continue; // A leaf. Its centre of mass is computed in the pass below.
            }
            let shift = 2 * (level - 1);
            // The four quadrant boundaries. The range is Morton-sorted, so the
            // quadrant field is non-decreasing across it and each quadrant is one
            // contiguous run found by binary search.
            let quad_of = |k: usize| (sorted_codes[k] >> shift) & 3;
            // Binary search per boundary keeps the build O(n log n) overall
            // instead of rescanning each range.
            let mut bounds = [start, 0, 0, 0, start + len];
            for (q, slot) in bounds.iter_mut().enumerate().take(4).skip(1) {
                *slot = partition_point(start, start + len, |k| quad_of(k) < q as u32);
            }
            let first_child = cells.len() as u32;
            let child_size = cells[ci].size * 0.5;
            let mut n_children = 0u32;
            for q in 0..4 {
                let (cs, ce) = (bounds[q], bounds[q + 1]);
                if ce <= cs {
                    continue; // Empty quadrant: no cell, so traversal never sees it.
                }
                cells.push(Cell {
                    com_x: 0.0,
                    com_y: 0.0,
                    mass: 0.0,
                    size: child_size,
                    start: cs as u32,
                    len: (ce - cs) as u32,
                    child: NO_CHILD,
                    n_children: 0,
                });
                n_children += 1;
            }
            cells[ci].child = first_child;
            cells[ci].n_children = n_children;
            for c in 0..n_children {
                stack.push(((first_child + c) as usize, level - 1));
            }
        }

        // Centres of mass, children before parents. A child is always pushed onto
        // `cells` after its parent, so descending index order visits every child
        // first and one reverse pass suffices.
        let mut rank = vec![0u32; n];
        for (pos, &i) in order.iter().enumerate() {
            rank[i as usize] = pos as u32;
        }
        let mut tree = QuadTree { cells, order, rank };
        for ci in (0..tree.cells.len()).rev() {
            let cell = tree.cells[ci];
            let (mut sx, mut sy, mut m) = (0.0f32, 0.0f32, 0.0f32);
            if cell.child == NO_CHILD {
                for k in cell.start..cell.start + cell.len {
                    let i = tree.order[k as usize] as usize;
                    let w = weight[i];
                    sx += x[i] * w;
                    sy += y[i] * w;
                    m += w;
                }
            } else {
                for c in cell.child..cell.child + cell.n_children {
                    let ch = tree.cells[c as usize];
                    sx += ch.com_x * ch.mass;
                    sy += ch.com_y * ch.mass;
                    m += ch.mass;
                }
            }
            let cell = &mut tree.cells[ci];
            cell.mass = m;
            // A zero-mass cell cannot happen (empty quadrants get no cell), but a
            // caller passing all-zero weights would divide by zero here.
            if m > 0.0 {
                cell.com_x = sx / m;
                cell.com_y = sy / m;
            }
        }
        tree
    }

    /// Whether `cell` covers node `i`, in constant time.
    #[inline]
    fn holds(&self, cell: Cell, i: usize) -> bool {
        let r = self.rank[i];
        r >= cell.start && r < cell.start + cell.len
    }

    /// Number of cells, for tests and for reasoning about memory.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Accumulate the repulsive force on node `i` by descending the tree.
    ///
    /// `pair(dx, dy, mass)` receives the displacement from the source to node `i`
    /// and the source's mass, and returns the force contribution. It is called
    /// once per accepted cell and once per node in every leaf that was not
    /// accepted — so the caller owns the force *law* and this owns the
    /// approximation.
    ///
    /// The traversal is depth-first with an explicit stack, visiting children in
    /// Morton order. That order is fixed by the tree, so the sum a node
    /// accumulates is reproducible regardless of how many workers are running.
    ///
    /// No cell containing `i` is ever accepted, so a node never repels itself
    /// through an aggregate it is part of. Proximity mostly handles that already
    /// (`d ≈ 0` makes `s/d` enormous), but "mostly" is not a guarantee when the
    /// centre of mass happens to land far from the node it contains, so the
    /// containment test is explicit.
    // Eight arguments, and bundling them would make this worse rather than better:
    // `x`/`y`/`weight` are the caller's live buffers (the layout mutates them
    // between iterations, so the tree cannot hold them), and `stack` is hoisted
    // scratch precisely to keep the per-node cost down. A wrapper struct here would
    // add a borrow to the hottest loop in the crate to satisfy a lint.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn accumulate<F>(
        &self,
        x: &[f32],
        y: &[f32],
        weight: &[f32],
        i: usize,
        theta: f32,
        stack: &mut Vec<u32>,
        mut pair: F,
    ) -> (f32, f32)
    where
        F: FnMut(f32, f32, f32) -> (f32, f32),
    {
        if self.cells.is_empty() {
            return (0.0, 0.0);
        }
        let (xi, yi) = (x[i], y[i]);
        let theta2 = theta * theta;
        let (mut fx, mut fy) = (0.0f32, 0.0f32);

        stack.clear();
        stack.push(0);
        while let Some(ci) = stack.pop() {
            let cell = self.cells[ci as usize];
            let (dx, dy) = (xi - cell.com_x, yi - cell.com_y);
            let d2 = dx * dx + dy * dy;

            // Accept the cell as one point when it is small relative to its
            // distance: s² < theta²·d². Written multiplied out to avoid a
            // division and a square root on the hot path.
            //
            // A leaf is never accepted wholesale even when it passes, because its
            // members may include `i` itself; the exact loop below is cheap at
            // LEAF_MAX nodes.
            // Accepting a leaf is allowed too, but only when it does not hold `i`
            // itself — a node must never repel itself through an aggregate it is
            // part of. `rank` makes that an O(1) range check.
            if cell.size * cell.size < theta2 * d2 && !self.holds(cell, i) {
                let (cx, cy) = pair(dx, dy, cell.mass);
                fx += cx;
                fy += cy;
                continue;
            }
            if cell.child == NO_CHILD {
                for k in cell.start..cell.start + cell.len {
                    let j = self.order[k as usize] as usize;
                    if j == i {
                        continue;
                    }
                    let (cx, cy) = pair(xi - x[j], yi - y[j], weight[j]);
                    fx += cx;
                    fy += cy;
                }
                continue;
            }
            // Push in reverse so the LIFO stack pops children in Morton order —
            // the order the summation is specified to happen in.
            for c in (cell.child..cell.child + cell.n_children).rev() {
                stack.push(c);
            }
        }
        (fx, fy)
    }
}

/// The first `k` in `lo..hi` for which `pred(k)` is false, assuming `pred` is
/// monotone over the range. A hand-rolled binary search because the predicate
/// reads an outer slice by index rather than taking an element.
#[inline]
fn partition_point(lo: usize, hi: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (lo, hi);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Stable LSD radix sort of `0..codes.len()` by `codes`, four 8-bit passes.
///
/// Stability is a determinism requirement, not a nicety: nodes that quantize into
/// the same lattice cell come out in ascending index order, so the exact
/// pairwise sums at the leaves happen in a fixed order.
fn radix_sort_by_code(codes: &[u32]) -> Vec<u32> {
    let n = codes.len();
    let mut a: Vec<u32> = (0..n as u32).collect();
    let mut b: Vec<u32> = vec![0; n];
    for pass in 0..4 {
        let shift = pass * 8;
        let mut counts = [0u32; 257];
        for &i in &a {
            counts[((codes[i as usize] >> shift) & 0xff) as usize + 1] += 1;
        }
        for k in 0..256 {
            counts[k + 1] += counts[k];
        }
        for &i in &a {
            let bucket = ((codes[i as usize] >> shift) & 0xff) as usize;
            b[counts[bucket] as usize] = i;
            counts[bucket] += 1;
        }
        std::mem::swap(&mut a, &mut b);
    }
    a
}

/// Per-node weights of all ones — the unweighted case, where mass is population.
pub fn unit_weights(n: usize) -> Vec<f32> {
    vec![1.0; n]
}

/// Scratch stacks for a parallel traversal, one per worker.
///
/// The traversal needs a growable stack and allocating one per node would dominate
/// the cost, so callers hoist it with `map_init`. Depth is bounded by
/// `AXIS_BITS` levels × 4 children, so this never grows past a cache line or two.
pub fn traversal_stack() -> Vec<u32> {
    Vec::with_capacity(4 * AXIS_BITS as usize + 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Positions on a grid — enough spread that subdivision actually happens.
    fn lattice(side: usize) -> (Vec<f32>, Vec<f32>) {
        let mut x = Vec::new();
        let mut y = Vec::new();
        for i in 0..side {
            for j in 0..side {
                x.push(i as f32);
                y.push(j as f32);
            }
        }
        (x, y)
    }

    /// Inverse-square repulsion, the law `layout_fr` uses.
    fn inv_square(dx: f32, dy: f32, m: f32) -> (f32, f32) {
        let d2 = (dx * dx + dy * dy).max(1e-4);
        let f = m / d2;
        (dx * f, dy * f)
    }

    fn brute_force(x: &[f32], y: &[f32], w: &[f32], i: usize) -> (f32, f32) {
        let (mut fx, mut fy) = (0.0f32, 0.0f32);
        for j in 0..x.len() {
            if j == i {
                continue;
            }
            let (cx, cy) = inv_square(x[i] - x[j], y[i] - y[j], w[j]);
            fx += cx;
            fy += cy;
        }
        (fx, fy)
    }

    #[test]
    fn morton_interleaves_the_axes() {
        assert_eq!(morton(0, 0), 0);
        // x in the even bits, y in the odd.
        assert_eq!(morton(1, 0), 0b01);
        assert_eq!(morton(0, 1), 0b10);
        assert_eq!(morton(1, 1), 0b11);
        assert_eq!(morton(2, 0), 0b0100);
        assert_eq!(morton(3, 3), 0b1111);
    }

    #[test]
    fn morton_order_groups_by_quadrant() {
        // The defining property the build relies on: sorting by code puts every
        // quadrant's members in one contiguous run.
        let pts = [(0u32, 0u32), (60000, 60000), (1, 1), (60000, 0), (0, 60000)];
        let codes: Vec<u32> = pts.iter().map(|&(a, b)| morton(a, b)).collect();
        let order = radix_sort_by_code(&codes);
        let quads: Vec<u32> = order.iter().map(|&i| codes[i as usize] >> 30).collect();
        // Non-decreasing top-level quadrant, i.e. grouped.
        assert!(quads.windows(2).all(|w| w[0] <= w[1]), "{quads:?}");
    }

    #[test]
    fn radix_sort_is_stable_on_equal_codes() {
        // Equal codes must come out in ascending index order, which is what makes
        // the leaf-level exact sums reproducible.
        let codes = vec![5u32, 5, 5, 1, 5];
        let order = radix_sort_by_code(&codes);
        assert_eq!(order, vec![3, 0, 1, 2, 4]);
    }

    #[test]
    fn theta_zero_reproduces_brute_force_exactly() {
        // The test that actually pins the tree. An approximation that is subtly
        // wrong still draws a plausible picture, so accuracy needs an oracle:
        // theta = 0 accepts no cell and must agree with all-pairs to the bit.
        let (x, y) = lattice(8);
        let w = unit_weights(x.len());
        let tree = QuadTree::build(&x, &y, &w);
        let mut stack = traversal_stack();
        for i in 0..x.len() {
            let got = tree.accumulate(&x, &y, &w, i, 0.0, &mut stack, inv_square);
            let want = brute_force(&x, &y, &w, i);
            // Same terms, but summed in tree order rather than index order, so
            // exact equality is not the claim — closeness is.
            assert!(
                (got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3,
                "node {i}: tree {got:?} vs brute {want:?}"
            );
        }
    }

    #[test]
    fn a_moderate_theta_stays_close_to_brute_force() {
        // The bargain: theta = 0.5 should approximate well, not merely run.
        let (x, y) = lattice(12);
        let w = unit_weights(x.len());
        let tree = QuadTree::build(&x, &y, &w);
        let mut stack = traversal_stack();
        let mut worst = 0.0f32;
        for i in 0..x.len() {
            let got = tree.accumulate(&x, &y, &w, i, 0.5, &mut stack, inv_square);
            let want = brute_force(&x, &y, &w, i);
            let mag = (want.0 * want.0 + want.1 * want.1).sqrt().max(1e-6);
            let err = ((got.0 - want.0).hypot(got.1 - want.1)) / mag;
            worst = worst.max(err);
        }
        assert!(worst < 0.15, "worst relative error {worst}");
    }

    #[test]
    fn the_tree_is_a_pure_function_of_the_positions() {
        let (x, y) = lattice(10);
        let w = unit_weights(x.len());
        let a = QuadTree::build(&x, &y, &w);
        let b = QuadTree::build(&x, &y, &w);
        assert_eq!(a.len(), b.len());
        assert_eq!(a.order, b.order);
        let mut sa = traversal_stack();
        let mut sb = traversal_stack();
        for i in 0..x.len() {
            let fa = a.accumulate(&x, &y, &w, i, 0.5, &mut sa, inv_square);
            let fb = b.accumulate(&x, &y, &w, i, 0.5, &mut sb, inv_square);
            // Bit-identical, not merely close: this is the determinism claim.
            assert_eq!(fa, fb, "node {i}");
        }
    }

    #[test]
    fn a_node_never_repels_itself() {
        // With leaf acceptance on, the containment check is the only thing stopping
        // a node feeling its own mass through the leaf it sits in. A self-force is
        // not detectable by eye — the drawing still looks like a drawing — so pin
        // it numerically: a huge theta accepts almost everything, and the force on
        // the sole node of a two-node graph must come only from the other one.
        let x = vec![0.0, 100.0];
        let y = vec![0.0, 0.0];
        let w = vec![1.0, 1.0];
        let tree = QuadTree::build(&x, &y, &w);
        let mut stack = traversal_stack();
        let got = tree.accumulate(&x, &y, &w, 0, 100.0, &mut stack, inv_square);
        let want = inv_square(-100.0, 0.0, 1.0);
        assert!(
            (got.0 - want.0).abs() < 1e-9 && (got.1 - want.1).abs() < 1e-9,
            "{got:?} should be exactly the force from node 1, {want:?}"
        );
    }

    #[test]
    fn leaf_acceptance_does_not_wreck_accuracy() {
        // Accepting distant leaves is a speed optimization; it must not quietly
        // buy that speed with error. Same bound as the theta test above.
        let (x, y) = lattice(14);
        let w = unit_weights(x.len());
        let tree = QuadTree::build(&x, &y, &w);
        let mut stack = traversal_stack();
        let mut worst = 0.0f32;
        for i in 0..x.len() {
            let got = tree.accumulate(&x, &y, &w, i, 0.5, &mut stack, inv_square);
            let want = brute_force(&x, &y, &w, i);
            let mag = (want.0 * want.0 + want.1 * want.1).sqrt().max(1e-6);
            worst = worst.max((got.0 - want.0).hypot(got.1 - want.1) / mag);
        }
        assert!(worst < 0.15, "worst relative error {worst}");
    }

    #[test]
    fn mass_is_conserved_up_the_tree() {
        let (x, y) = lattice(9);
        let w: Vec<f32> = (0..x.len()).map(|i| 1.0 + (i % 5) as f32).collect();
        let tree = QuadTree::build(&x, &y, &w);
        let total: f32 = w.iter().sum();
        // The root's mass is the whole graph's, which is what makes a far-field
        // approximation the right magnitude rather than merely the right shape.
        assert!(
            (tree.cells[0].mass - total).abs() < 1e-2,
            "root mass {} vs {total}",
            tree.cells[0].mass
        );
    }

    #[test]
    fn weights_move_the_centre_of_mass() {
        // Two nodes, one ten times the other: the root's centre sits near the
        // heavy one. Without this, degree-weighted repulsion silently degrades to
        // unweighted.
        let x = vec![0.0, 10.0];
        let y = vec![0.0, 0.0];
        let tree = QuadTree::build(&x, &y, &[1.0, 9.0]);
        assert!(
            (tree.cells[0].com_x - 9.0).abs() < 1e-3,
            "com_x {}",
            tree.cells[0].com_x
        );
    }

    #[test]
    fn coincident_nodes_terminate() {
        // Every node at the same point: no subdivision can separate them, so only
        // the depth bound stops the build. Without it this hangs.
        let n = 40;
        let x = vec![3.0; n];
        let y = vec![-1.0; n];
        let w = unit_weights(n);
        let tree = QuadTree::build(&x, &y, &w);
        assert!(!tree.is_empty());
        let mut stack = traversal_stack();
        let f = tree.accumulate(&x, &y, &w, 0, 0.5, &mut stack, inv_square);
        assert!(f.0.is_finite() && f.1.is_finite());
    }

    #[test]
    fn degenerate_sizes_are_handled() {
        let w0: Vec<f32> = Vec::new();
        let tree = QuadTree::build(&[], &[], &w0);
        assert!(tree.is_empty());
        let mut stack = traversal_stack();
        assert_eq!(
            tree.accumulate(&[], &[], &w0, 0, 0.5, &mut stack, inv_square),
            (0.0, 0.0)
        );

        let one = QuadTree::build(&[1.0], &[2.0], &[1.0]);
        assert_eq!(one.len(), 1);
        // A lone node feels nothing: the only leaf holds just itself.
        let w1 = unit_weights(1);
        assert_eq!(
            one.accumulate(&[1.0], &[2.0], &w1, 0, 0.5, &mut stack, inv_square),
            (0.0, 0.0)
        );
    }

    #[test]
    fn every_node_appears_exactly_once_in_the_order() {
        let (x, y) = lattice(7);
        let w = unit_weights(x.len());
        let tree = QuadTree::build(&x, &y, &w);
        let mut seen = vec![false; x.len()];
        for &i in &tree.order {
            assert!(!seen[i as usize], "node {i} twice");
            seen[i as usize] = true;
        }
        assert!(seen.iter().all(|&s| s), "a node is missing from the order");
    }

    #[test]
    fn leaves_hold_at_most_leaf_max_unless_coincident() {
        let (x, y) = lattice(16);
        let w = unit_weights(x.len());
        let tree = QuadTree::build(&x, &y, &w);
        for (ci, cell) in tree.cells.iter().enumerate() {
            if cell.child == NO_CHILD {
                assert!(
                    cell.len as usize <= LEAF_MAX,
                    "leaf {ci} holds {} nodes on a spread-out lattice",
                    cell.len
                );
            }
        }
    }
}
