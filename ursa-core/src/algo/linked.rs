//! The nodes a kernel has to work on: those with at least one edge.
//!
//! A graph with a vertex table has every key as a node, edges or not (#311),
//! and on real data most can have none (37.2M of Bluesky's 39.65M). A node with
//! no kept edge, in either direction and including self-loops, is
//! **isolated**, and every kernel's result for it is closed-form: its own
//! component or community, no triangles, and for PageRank a value shared by
//! every isolated node (see `pagerank`). So the kernels that would otherwise
//! carry them run over the **linked** nodes only, numbered `0..len()` in
//! ascending node order (the compact order), and fill the isolated nodes in
//! when they write their result (#326).
//!
//! The compact order is the node order restricted to the linked nodes, so a
//! kernel that compares node ids (a tie broken toward the smaller id, a sorted
//! neighbour list) compares compact ids the same way. What a kernel must still
//! account for is anything that depends on the *full* node count or order: the
//! node count in PageRank's teleport term, the seeded visit order of the
//! community kernels (a permutation of every node), or labels that are node
//! ids. Each kernel says how it does.
//!
//! [`Linked`] holds the linked nodes as a sorted list (compact to full) and a
//! bitset with a rank directory (full to compact, in O(1)): 4 bytes per
//! linked node and 2 bits per node.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::parallel::*;
use crate::topology::{EdgeMask, Topology};

/// One 64-node block of the bitset: which of its nodes are in the set, and
/// how many set nodes precede the block.
#[derive(Debug, Clone, Copy)]
struct Block {
    bits: u64,
    before: u32,
}

/// A subset of the nodes `0..n` (the linked ones, or a level's non-singleton
/// super-nodes in Louvain), with O(1) membership and rank.
#[derive(Debug, Clone)]
pub(crate) struct Linked {
    n: usize,
    /// The members, ascending: compact id to node.
    nodes: Vec<u32>,
    blocks: Vec<Block>,
}

/// Run a kernel over the linked nodes only when at least this share of the
/// nodes is isolated (`n_isolated * ISOLATED_SHARE >= n`), for a kernel whose
/// compact form looks a node up per edge (PageRank, label propagation,
/// components). Below it the lookup costs more than the isolated nodes do.
/// Results are the same either way.
pub(crate) const ISOLATED_SHARE: usize = 8;

impl Linked {
    /// The nodes of `topo` with at least one edge `mask` keeps (`None`: every
    /// edge), in either direction. One pass over the out-adjacency.
    pub(crate) fn of(topo: &Topology, mask: Option<&EdgeMask>) -> Linked {
        let n = topo.n_nodes();
        let out = topo.out();
        // Unmasked with the transpose at hand, the CSR offsets say.
        if let (None, Some(inc)) = (mask, topo.incoming_if_built()) {
            return Linked::from_fn(n, |u| out.degree(u) > 0 || inc.degree(u) > 0);
        }
        let words: Vec<AtomicU64> = (0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        let set = |v: u32| {
            words[(v >> 6) as usize].fetch_or(1u64 << (v & 63), Ordering::Relaxed);
        };
        (0..n as u32).into_par_iter().for_each(|u| {
            let mut any = false;
            for (&v, &e) in out.neighbors(u).iter().zip(out.edge_ids(u)) {
                if mask.is_none_or(|m| m.keep(e)) {
                    any = true;
                    set(v);
                }
            }
            if any {
                set(u);
            }
        });
        let words: Vec<u64> = words.into_iter().map(AtomicU64::into_inner).collect();
        Linked::from_words(n, words)
    }

    /// The nodes `v` of `0..n` with `linked(v)`.
    pub(crate) fn from_fn(n: usize, linked: impl Fn(u32) -> bool + Sync) -> Linked {
        let words: Vec<u64> = (0..n.div_ceil(64))
            .into_par_iter()
            .map(|w| {
                let lo = w * 64;
                (lo..(lo + 64).min(n)).fold(0u64, |bits, v| {
                    bits | (u64::from(linked(v as u32)) << (v - lo))
                })
            })
            .collect();
        Linked::from_words(n, words)
    }

    /// The subset of `0..n` that `sorted` (ascending, distinct) lists.
    pub(crate) fn from_sorted(n: usize, sorted: Vec<u32>) -> Linked {
        let mut words = vec![0u64; n.div_ceil(64)];
        for &v in &sorted {
            words[(v >> 6) as usize] |= 1u64 << (v & 63);
        }
        let mut linked = Linked::from_words(n, words);
        debug_assert_eq!(
            linked.nodes, sorted,
            "the members must be sorted and distinct"
        );
        linked.nodes = sorted;
        linked
    }

    fn from_words(n: usize, words: Vec<u64>) -> Linked {
        let mut blocks = Vec::with_capacity(words.len());
        let mut before = 0u32;
        for &bits in &words {
            blocks.push(Block { bits, before });
            before += bits.count_ones();
        }
        let mut nodes = Vec::with_capacity(before as usize);
        for (w, &bits) in words.iter().enumerate() {
            let mut b = bits;
            while b != 0 {
                nodes.push((w as u32) << 6 | b.trailing_zeros());
                b &= b - 1;
            }
        }
        Linked { n, nodes, blocks }
    }

    /// The node count of the full graph.
    #[inline]
    pub(crate) fn n(&self) -> usize {
        self.n
    }

    /// How many nodes are linked: the compact node count.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// How many nodes are isolated.
    #[inline]
    pub(crate) fn n_isolated(&self) -> usize {
        self.n - self.nodes.len()
    }

    /// Whether a kernel that looks a node up per edge should run compact: at
    /// least one node in [`ISOLATED_SHARE`] is isolated.
    pub(crate) fn worth_it(&self) -> bool {
        self.n_isolated() > 0 && self.n_isolated() * ISOLATED_SHARE >= self.n
    }

    /// The linked nodes, ascending: `nodes()[c]` is compact node `c`.
    #[inline]
    pub(crate) fn nodes(&self) -> &[u32] {
        &self.nodes
    }

    /// Is node `v` linked?
    #[inline]
    pub(crate) fn contains(&self, v: u32) -> bool {
        (self.blocks[(v >> 6) as usize].bits >> (v & 63)) & 1 == 1
    }

    /// The compact id of node `v`, which must be linked; for any `v`, the
    /// number of linked nodes below it.
    #[inline]
    pub(crate) fn compact(&self, v: u32) -> u32 {
        let b = self.blocks[(v >> 6) as usize];
        b.before + (b.bits & ((1u64 << (v & 63)) - 1)).count_ones()
    }

    /// A per-node vector from a compact one: compact node `c`'s value at
    /// `nodes()[c]`, `fill(v)` at each isolated node `v`.
    pub(crate) fn expand<T: Copy>(&self, compact: &[T], fill: impl Fn(u32) -> T) -> Vec<T> {
        debug_assert_eq!(compact.len(), self.len());
        let mut out = Vec::with_capacity(self.n);
        let mut next = 0u32;
        for (&v, &x) in self.nodes.iter().zip(compact) {
            out.extend((next..v).map(&fill));
            out.push(x);
            next = v + 1;
        }
        out.extend((next..self.n as u32).map(&fill));
        out
    }

    /// Labels that are node ids, computed over the compact nodes (each label a
    /// compact id), as node ids over every node: a linked node's label mapped
    /// back to its node, an isolated node labelled by itself.
    pub(crate) fn expand_labels(&self, compact: &[u32]) -> Vec<u32> {
        let mapped: Vec<u32> = compact.iter().map(|&l| self.nodes[l as usize]).collect();
        self.expand(&mapped, |v| v)
    }

    /// The compact form of `order`, a sequence of nodes (a seeded visit
    /// order): its linked nodes as compact ids, in order, and for each the
    /// number of isolated nodes just before it since the previous linked one;
    /// then the number of isolated nodes after the last.
    pub(crate) fn compact_order(&self, order: &[u32]) -> (Vec<u32>, Vec<u32>, u32) {
        let mut ids = Vec::with_capacity(self.len());
        let mut gaps = Vec::with_capacity(self.len());
        let mut gap = 0u32;
        for &v in order {
            if self.contains(v) {
                ids.push(self.compact(v));
                gaps.push(std::mem::take(&mut gap));
            } else {
                gap += 1;
            }
        }
        (ids, gaps, gap)
    }
}

/// `s + x + x + … + x` (`r` additions, each rounded as `f64` addition rounds),
/// for `s >= 0` and `x >= 0`, finite: exactly the left-to-right sum a loop
/// computes, in O(log) steps rather than `r`.
///
/// While the running sum stays in one binade `[2^e, 2^(e+1))`, it is `M·u`
/// for an integer `M` and the binade's ulp `u`, and `x = t·u` exactly (`u` is
/// a power of two). So `s + x` rounds to `(M + q)·u`, `q` the integer nearest
/// `t`, whatever `M` is; or, when `t` is half an integer `q0 + ½` (common for
/// a difference of two close values, which has few significant bits), to the
/// even one of `M + q0` and `M + q0 + 1`. After a tie `M` is even, and from an
/// even `M` every tie adds the same even step: `q0`, or `q0 + 1` when `q0` is
/// odd. So the additions that stay in the binade add `j·q·u` at once (exactly,
/// as the result is representable), and only the one that crosses into the
/// next binade, a tie from an odd `M`, or a sum not yet normal, is done as a
/// plain addition. Once an addition leaves the sum unchanged, all later ones
/// do too.
///
/// This is how PageRank sums the isolated nodes' rank, all equal, in the same
/// order and to the same bits as summing them one by one (#326).
pub(crate) fn add_repeated(mut s: f64, x: f64, mut r: u64) -> f64 {
    debug_assert!(s >= 0.0 && x >= 0.0 && s.is_finite() && x.is_finite());
    while r > 0 {
        // The biased exponent of s. Outside 53..2046 the binade's ulp or top
        // is not a normal power of two: add one at a time (never met by
        // PageRank's sums).
        let e = (s.to_bits() >> 52) as i64;
        if s.is_normal() && x > 0.0 && (53..2046).contains(&e) {
            let ulp = f64::from_bits(((e - 52) as u64) << 52);
            let top = f64::from_bits(((e + 1) as u64) << 52);
            let t = x / ulp;
            if t < 4.0e15 {
                let q0 = t.trunc();
                let step = if t - q0 != 0.5 {
                    Some(t.round() as u64)
                } else if s.to_bits() & 1 == 0 {
                    // M even: a tie adds the even one of q0 and q0 + 1.
                    Some(q0 as u64 + (q0 as u64 & 1))
                } else {
                    None // M odd: one plain addition makes it even
                };
                if let Some(q) = step {
                    if q == 0 {
                        return s; // every addition leaves s as it is
                    }
                    // Ulps left below the top (exact, by Sterbenz), and how
                    // many steps of q fit while the sum stays at most
                    // top - ulp.
                    let room = ((top - s) / ulp) as u64;
                    let j = ((room - 1) / q).min(r);
                    if j > 0 {
                        s += (j * q) as f64 * ulp;
                        r -= j;
                        continue;
                    }
                }
            }
        }
        let next = s + x;
        r -= 1;
        if next.to_bits() == s.to_bits() {
            return s;
        }
        s = next;
    }
    s
}

/// Graphs with many isolated nodes, for the kernels' tests that their compact
/// runs give the full runs' results.
#[cfg(test)]
pub(crate) mod testing {
    use crate::topology::{EdgeMask, Topology};

    /// splitmix64.
    pub(crate) fn next(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A graph: `m` random edges (self-loops and parallel edges included)
    /// among a random quarter of `n` nodes, the rest isolated, and a mask
    /// keeping about two edges in three.
    pub(crate) fn sparse(seed: u64, n: usize, m: usize) -> (Topology, EdgeMask) {
        let mut st = seed;
        let hubs: Vec<u32> = (0..n.div_ceil(4))
            .map(|_| (next(&mut st) % n as u64) as u32)
            .collect();
        let pick = |st: &mut u64| hubs[(next(st) % hubs.len() as u64) as usize];
        let (src, dst): (Vec<u32>, Vec<u32>) =
            (0..m).map(|_| (pick(&mut st), pick(&mut st))).unzip();
        let keep: Vec<bool> = (0..m).map(|_| !next(&mut st).is_multiple_of(3)).collect();
        (Topology::build(n, src, dst), EdgeMask::from_bools(&keep))
    }

    /// One weight per edge row, inexact so the sums are order-sensitive.
    pub(crate) fn weights(seed: u64, m: usize) -> Vec<f64> {
        let mut st = seed;
        (0..m)
            .map(|_| 0.01 + (next(&mut st) % 1000) as f64 / 7.0)
            .collect()
    }

    /// The seeds and sizes the tests run: small enough to be quick, one with
    /// several delta chunks of PageRank's L1 sum.
    pub(crate) const CASES: &[(u64, usize, usize)] = &[
        (1, 1, 0),
        (2, 7, 3),
        (3, 60, 40),
        (4, 300, 400),
        (5, 1000, 600),
        (6, 20_000, 9_000),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(mut s: f64, x: f64, r: u64) -> f64 {
        for _ in 0..r {
            s += x;
        }
        s
    }

    /// splitmix64, for test values.
    fn rng(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[test]
    fn repeated_addition_matches_the_loop() {
        let mut state = 7u64;
        for case in 0..20_000 {
            let unit = |st: &mut u64| (rng(st) >> 11) as f64 / (1u64 << 53) as f64;
            let x = match case % 5 {
                0 => unit(&mut state) * 1e-7,
                1 => unit(&mut state),
                // Few significant bits: ties and exact steps.
                2 => (rng(&mut state) % 64) as f64 * 2f64.powi(-((rng(&mut state) % 60) as i32)),
                3 => 1.0 / (1 + rng(&mut state) % 40_000_000) as f64,
                // The difference of two close values, as in PageRank's delta.
                _ => {
                    let a = 1.0 / (1 + rng(&mut state) % 40_000_000) as f64;
                    (a - a * (1.0 + unit(&mut state) * 1e-6)).abs()
                }
            };
            let s = match case % 3 {
                0 => 0.0,
                1 => -0.0,
                _ => unit(&mut state) * 10f64.powi(-((rng(&mut state) % 8) as i32)),
            };
            let r = rng(&mut state) % 20_000;
            let (want, got) = (naive(s, x, r), add_repeated(s, x, r));
            assert_eq!(
                want.to_bits(),
                got.to_bits(),
                "s = {s:e}, x = {x:e}, r = {r}: {want:e} != {got:e}"
            );
        }
    }

    #[test]
    fn repeated_addition_matches_the_loop_on_long_runs() {
        for &(s, x, r) in &[
            (0.0, 1.0 / 39_650_000.0, 3_000_000u64),
            (0.1, 3.7e-9, 2_000_000),
            (0.0, 0.1, 1_000_000),
            (1.0, 2f64.powi(-53), 1000), // a tie that rounds to even: stuck
            (1.0 + 2f64.powi(-52), 2f64.powi(-53), 1000), // a tie that moves once
            (0.0, 1.5 * 2f64.powi(-40), 100_000),
        ] {
            assert_eq!(
                naive(s, x, r).to_bits(),
                add_repeated(s, x, r).to_bits(),
                "s = {s:e}, x = {x:e}, r = {r}"
            );
        }
    }

    #[test]
    fn linked_nodes_and_their_ranks() {
        // 0 -> 2, 5 -> 5 (a self-loop), 70 -> 3; the rest are isolated.
        let t = Topology::build(130, vec![0, 5, 70], vec![2, 5, 3]);
        let l = Linked::of(&t, None);
        assert_eq!(l.nodes(), &[0, 2, 3, 5, 70]);
        assert_eq!((l.n(), l.len(), l.n_isolated()), (130, 5, 125));
        assert!(l.contains(70) && !l.contains(69) && !l.contains(129));
        assert_eq!(l.compact(70), 4);
        assert_eq!(l.compact(5), 3);
        // An edge masked out leaves its endpoints isolated.
        let mask = EdgeMask::from_bools(&[true, false, true]);
        assert_eq!(Linked::of(&t, Some(&mask)).nodes(), &[0, 2, 3, 70]);
        let full = l.expand_labels(&[1, 1, 4, 3, 4]);
        assert_eq!(&full[..7], &[2, 1, 2, 70, 4, 5, 6]);
        assert_eq!(full[70], 70);
        let (ids, gaps, tail) = l.compact_order(&[1, 70, 4, 6, 0, 9]);
        assert_eq!((ids, gaps, tail), (vec![4, 0], vec![1, 2], 1));
        let s = Linked::from_sorted(10, vec![1, 4, 9]);
        assert_eq!((s.compact(4), s.compact(9), s.contains(5)), (1, 2, false));
    }
}
