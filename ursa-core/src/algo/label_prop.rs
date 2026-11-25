//! Label propagation — community detection by iterated neighbour-majority voting.
//!
//! Each node adopts the label held by the plurality of its neighbours; iterated
//! to a fixpoint. Updates are *asynchronous* in a `seed`-derived sweep order (a
//! node sees its neighbours' already-updated labels within a sweep), which is the
//! Raghavan et al. formulation — it avoids the two-colour oscillation that a
//! fully-synchronous update suffers on bipartite structure. Ties are broken
//! toward the smallest label id so the result is deterministic regardless of hash
//! iteration order.
//!
//! **Multiplicity:** votes are tallied per CSR slot, so parallel edges cast
//! parallel votes (multiplicity-is-rows) — a heavily-parallel neighbour pulls
//! proportionally harder.
//!
//! With `parallel`, sweeps run in fixed sub-rounds instead (#453, see
//! [`label_propagation_parallel`]), the frame of Louvain's parallel local
//! moving (#387): deterministic on any thread count, but not the serial run's
//! labels.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;

use crate::parallel::*;

use super::linked::Linked;
use super::rng::{shuffled_order, DEFAULT_SEED};
use super::subrounds::{places, run_of, sub_rounds};
use crate::topology::{Adjacency, EdgeMask, Topology};

/// Community label per node. Communities are treated as undirected (a node votes
/// over both its out- and in-neighbours). Iterates until a full sweep changes no
/// label or `max_iter` sweeps have run. Labels are original node ids (one
/// representative per community; see [`Topology::original_ids`]), not
/// necessarily a contiguous `0..k`. A subgraph `mask` restricts voting to kept
/// edges.
///
/// The visit order and the ties are defined on the original ids: a topology
/// whose nodes were relabelled for locality (#172) runs over
/// [`Topology::in_original_order`], so a seed gives the same communities, and
/// the same labels, as on an index that was not relabelled.
///
/// An isolated node never votes and is never voted for, so it keeps its own
/// label; a graph with many runs over its linked nodes only (#326), visited in
/// the order the seeded permutation of every node gives them, with the same
/// labels.
///
/// `parallel` runs each sweep in fixed sub-rounds whose nodes vote in
/// parallel ([`label_propagation_parallel`]): faster on many cores, and as
/// deterministic (the same labels on any thread count), but not the serial
/// run's labels. It has no NetworkX equivalent.
pub fn label_propagation(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    max_iter: u32,
    seed: Option<u64>,
    parallel: bool,
) -> Vec<u32> {
    if topo.n_nodes() == 0 {
        return Vec::new();
    }
    // Relabelled nodes (#172): vote in the original numbering, whose visit
    // order and ties give the labels an unrelabelled index gives.
    if let Some(original) = topo.in_original_order() {
        let labels = label_propagation(&original, mask, max_iter, seed, parallel);
        return in_node_order(topo, &labels);
    }
    topo.incoming();
    let linked = Linked::of(topo, mask);
    let linked = linked.worth_it().then_some(&linked);
    if parallel {
        label_propagation_parallel(topo, mask, max_iter, seed, linked)
    } else {
        label_propagation_over(topo, mask, max_iter, seed, linked)
    }
}

/// Labels computed over [`Topology::in_original_order`], back in `topo`'s
/// node order: node `v` takes the label of its original id. The label values
/// are original ids, as an unrelabelled run gives them.
pub(crate) fn in_node_order(topo: &Topology, labels: &[u32]) -> Vec<u32> {
    (0..topo.n_nodes() as u32)
        .map(|v| labels[topo.original_id(v) as usize])
        .collect()
}

/// [`label_propagation`], compact over `linked` when given.
///
/// A node votes again only once a neighbour's label has changed since its
/// last vote (#453): otherwise it would vote for the label it holds. So late
/// sweeps, which change few labels, read few adjacencies, and the labels are
/// those of voting every node every sweep.
fn label_propagation_over(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    max_iter: u32,
    seed: Option<u64>,
    linked: Option<&Linked>,
) -> Vec<u32> {
    let n = topo.n_nodes();
    let width = linked.map_or(n, Linked::len);
    // The node of compact node `u`, and the compact node of node `v`.
    let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
    let at = |v: u32| linked.map_or(v, |l| l.compact(v));
    let mut label: Vec<u32> = (0..width as u32).collect();
    let out = topo.out();
    let inc = topo.incoming();
    let order = shuffled_order(n, seed.unwrap_or(DEFAULT_SEED));
    let order = match linked {
        None => order,
        Some(l) => l.compact_order(&order).0,
    };
    // Vote tally, dense over label ids (labels are node ids, so `< n`), plus the
    // labels touched for the current node. Counting into an array and resetting
    // only the touched slots replaces a hash map probe per neighbour; the winner
    // depends only on the counts, never on the order labels were seen.
    let mut counts = vec![0u32; width];
    let mut touched: Vec<u32> = Vec::new();

    // Tally each kept-edge neighbour's label for node `u` in one adjacency.
    let tally =
        |adj: &Adjacency, u: u32, label: &[u32], counts: &mut [u32], touched: &mut Vec<u32>| {
            each_kept(adj, node(u), mask, |v| {
                let lab = label[at(v) as usize];
                let c = &mut counts[lab as usize];
                if *c == 0 {
                    touched.push(lab);
                }
                *c += 1;
            });
        };

    // Whether a neighbour's label has changed since the node last voted (every
    // node before its first vote). A vote depends only on the neighbours'
    // labels, and a node holds the label it last voted for, so a node whose
    // neighbours have not changed would vote for the label it holds: it is
    // skipped, with the same labels.
    let mut stale = vec![true; width];

    for _ in 0..max_iter {
        let mut changed = false;
        for &u in &order {
            if !std::mem::replace(&mut stale[u as usize], false) {
                continue;
            }
            tally(out, u, &label, &mut counts, &mut touched);
            tally(inc, u, &label, &mut counts, &mut touched);
            if touched.is_empty() {
                continue; // isolated node keeps its own label
            }
            // Plurality label; ties resolved toward the smallest label id so the
            // choice does not depend on the order labels were tallied.
            let cur = label[u as usize];
            let mut best_label = cur;
            let mut best_count = 0u32;
            for &lab in &touched {
                let c = std::mem::take(&mut counts[lab as usize]);
                if c > best_count || (c == best_count && lab < best_label) {
                    best_count = c;
                    best_label = lab;
                }
            }
            touched.clear();
            if best_label != cur {
                label[u as usize] = best_label;
                changed = true;
                // A neighbour that holds the new label still votes for it: it
                // gains a vote and its winner does not change.
                let x = node(u);
                let mut touch = |v: u32| {
                    let v = at(v) as usize;
                    if label[v] != best_label {
                        stale[v] = true;
                    }
                };
                each_kept(out, x, mask, &mut touch);
                each_kept(inc, x, mask, &mut touch);
            }
        }
        if !changed {
            break;
        }
    }
    match linked {
        None => label,
        Some(l) => l.expand_labels(&label),
    }
}

/// How many runs [`label_propagation_parallel`] cuts the visit order into. A
/// constant, never the thread count, so the labels are the same on any number
/// of threads. Louvain's parallel local moving uses the same number (#387).
const RUNS: usize = 64;

/// A run whose listed active nodes are at least one in this many of its nodes
/// finds them by scanning the run; a sparser one sorts its list instead.
const DENSE_RUN: usize = 16;

/// Nodes per piece of a run's vote in [`label_propagation_parallel`]: each
/// piece locks a worker's scratch once and keeps its changes and the nodes they
/// wake.
const VOTE_CHUNK: usize = 256;

/// What a piece of a run's vote keeps: its nodes whose label changes, with
/// their new label, and the nodes those changes wake.
type Voted = (Vec<(u32, u32)>, Vec<u32>);

/// Label propagation in parallel sub-rounds (#453), compact over `linked` when
/// given.
///
/// The seeded visit order of the serial run is cut into [`RUNS`] runs of equal
/// length. A sweep takes the runs in turn. In each, every active node of the
/// run takes the plurality label of its neighbours, in parallel, against the
/// labels as they were when the run started (the serial tie rule: the smallest
/// label among the most frequent); then the new labels are written in run
/// order, and the nodes whose vote they may change are marked active.
///
/// **Oscillation.** Nodes of one run vote on labels that may change under
/// them. Two neighbours of one run would trade labels: each takes the other's,
/// for ever (the two-colouring of fully synchronous label propagation on
/// bipartite structure). So a node does not read the label of a neighbour of
/// its own run when that label is above its own: of two such neighbours, only
/// the one with the higher label moves, to the lower, as in Louvain's guard
/// for single nodes. A neighbour in another run votes as in the serial run,
/// since its label does not change while this run votes. The run stops like
/// the serial one: after a sweep that changes no label, or after `max_iter`
/// sweeps. Measured on R-MAT (32M edges): without the guard, 14 nodes still
/// traded labels in the 20th sweep; with it, the fifth sweep changed nothing,
/// as in the serial run.
///
/// **Skipping.** A node votes again only when its vote may have changed: when
/// a neighbour took a label other than the one the node holds (a neighbour
/// that took the node's own label only adds a vote to its winner), when a
/// neighbour of its own run changed (the node's label may change in the same
/// run, so which label it holds is not known yet), or when its own label rose (the guard then reads more of
/// its run). Every other node would vote for the label it holds, so the
/// labels are those of every node voting in every sweep; late sweeps, which
/// change few labels, read few adjacencies.
///
/// Deterministic on any thread count: the runs depend only on the node count,
/// the seed and [`RUNS`]; a vote reads only labels frozen at the start of its
/// run, and a vote's winner does not depend on the order it was counted in.
/// Each worker counts a node's votes in a small table sized to the node's
/// degree ([`Votes`]), not in an array over every label.
///
/// With `linked`, the runs are cut from the full order, isolated nodes
/// included, and each holds the linked nodes that fall in it; an isolated node
/// never votes and is never voted for, so the compact run makes the full run's
/// changes. On a graph of at most [`RUNS`] nodes every run holds one node, and
/// the labels are the serial run's.
fn label_propagation_parallel(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    max_iter: u32,
    seed: Option<u64>,
    linked: Option<&Linked>,
) -> Vec<u32> {
    let n = topo.n_nodes();
    let width = linked.map_or(n, Linked::len);
    let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
    let at = |v: u32| linked.map_or(v, |l| l.compact(v));
    let (out, inc) = (topo.out(), topo.incoming());

    let (order, bounds) = sub_rounds(n, seed.unwrap_or(DEFAULT_SEED), linked, RUNS);
    let place = places(&order);
    // The run of each node, for the guard: one byte a node, read per vote.
    let mut run = vec![0u8; width];
    for r in 0..RUNS {
        for &u in &order[bounds[r]..bounds[r + 1]] {
            run[u as usize] = r as u8;
        }
    }
    let mut label: Vec<u32> = (0..width as u32).collect();
    let active: Vec<AtomicBool> = (0..width).map(|_| AtomicBool::new(true)).collect();
    // The places (in `order`) of the nodes activated in each run since it last
    // ran.
    let mut listed: Vec<Vec<u32>> = vec![Vec::new(); RUNS];
    let votes: Vec<Mutex<Votes>> = (0..current_num_threads().max(1))
        .map(|_| Mutex::new(Votes::default()))
        .collect();

    for sweep in 0..max_iter {
        let mut changed = 0usize;
        for r in 0..RUNS {
            let (lo, hi) = (bounds[r], bounds[r + 1]);
            let mut list = std::mem::take(&mut listed[r]);
            if lo == hi {
                continue;
            }
            // The run's active nodes, in run order: all of the run in the
            // first sweep, or when many are listed; else the listed ones.
            let nodes: &[u32] = if sweep == 0 || list.len() * DENSE_RUN >= hi - lo {
                &order[lo..hi]
            } else {
                list.sort_unstable();
                for i in list.iter_mut() {
                    *i = order[*i as usize];
                }
                &list
            };
            // Vote, in parallel, against the labels at the start of the run,
            // in fixed chunks of the run's nodes: each chunk keeps its nodes
            // whose label changes, with their new label, and the nodes their
            // change may wake. A node that has voted is no longer active
            // (nothing marks nodes while the run votes).
            let voted: Vec<Voted> = {
                let (label, run) = (label.as_slice(), run.as_slice());
                nodes
                    .par_chunks(VOTE_CHUNK)
                    .map(|chunk| {
                        let mut votes = votes[worker_index() % votes.len()]
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let (mut movers, mut wake) = (Vec::new(), Vec::new());
                        for &u in chunk {
                            let flag = &active[u as usize];
                            if !flag.load(Relaxed) {
                                continue;
                            }
                            flag.store(false, Relaxed);
                            let cur = label[u as usize];
                            let x = node(u);
                            votes.begin(out.degree(x) as usize + inc.degree(x) as usize, width);
                            let mut vote = |v: u32| {
                                let v = at(v) as usize;
                                let lv = label[v];
                                // The guard: a higher label of a node of this
                                // run is not read.
                                if lv <= cur || usize::from(run[v]) != r {
                                    votes.add(lv);
                                }
                            };
                            each_kept(out, x, mask, &mut vote);
                            each_kept(inc, x, mask, &mut vote);
                            let best = votes.take_best().unwrap_or(cur);
                            if best == cur {
                                continue;
                            }
                            movers.push((u, best));
                            // The node's own vote changes only if its label
                            // rose: the guard then reads more of its run.
                            if best > cur {
                                wake.push(u);
                            }
                            // A neighbour in another run that holds the new
                            // label still votes for it (it gains a vote, and
                            // its winner does not change); one of this run
                            // may change its label in this run, so it is woken
                            // whatever it holds now.
                            let mut touch = |v: u32| {
                                let v = at(v);
                                if label[v as usize] != best || usize::from(run[v as usize]) == r {
                                    wake.push(v);
                                }
                            };
                            each_kept(out, x, mask, &mut touch);
                            each_kept(inc, x, mask, &mut touch);
                        }
                        (movers, wake)
                    })
                    .collect()
            };
            drop(list);

            // Write the new labels.
            for (movers, _) in &voted {
                for &(u, l) in movers {
                    label[u as usize] = l;
                }
                changed += movers.len();
            }

            // Mark the woken nodes active, in parallel; each is listed once,
            // in its own run.
            let (active, place) = (&active, &place);
            let marked: Vec<Vec<u32>> = voted
                .par_iter()
                .map(|(_, wake)| {
                    let mut woken = Vec::new();
                    for &v in wake {
                        let flag = &active[v as usize];
                        if !flag.load(Relaxed) && !flag.swap(true, Relaxed) {
                            woken.push(place[v as usize]);
                        }
                    }
                    woken
                })
                .collect();
            for i in marked.into_iter().flatten() {
                listed[run_of(&bounds, i as usize)].push(i);
            }
        }
        if changed == 0 {
            break;
        }
    }
    match linked {
        None => label,
        Some(l) => l.expand_labels(&label),
    }
}

/// Apply `f` to the neighbour across each edge of node `u` in `adj` that
/// `mask` keeps. Without a mask the edge ids are not read.
#[inline(always)]
fn each_kept(adj: &Adjacency, u: u32, mask: Option<&EdgeMask>, mut f: impl FnMut(u32)) {
    match mask {
        None => adj.neighbors(u).iter().for_each(|&v| f(v)),
        Some(m) => {
            for (&v, &e) in adj.neighbors(u).iter().zip(adj.edge_ids(u)) {
                if m.keep(e) {
                    f(v);
                }
            }
        }
    }
}

const EMPTY: u32 = u32::MAX;

/// A worker's scratch for one node's vote in [`label_propagation_parallel`]:
/// the count of each neighbouring label. An open-addressing table sized to
/// the node's degree (twice it, to a power of two), or, for a hub whose table
/// would be as large as the graph, a dense array over labels. Only the slots
/// a node touched are reset after it, so a worker holds the table of the
/// largest degree it has met, not 8 bytes for every node of the graph.
#[derive(Default)]
struct Votes {
    /// The label in each slot (`EMPTY` when free).
    keys: Vec<u32>,
    counts: Vec<u32>,
    /// The slots used, in the order met.
    touched: Vec<u32>,
    /// `64 - log2(capacity)` of the table; 0 when dense.
    shift: u32,
}

impl Votes {
    /// Ready the table for a node of `degree` neighbours among `n` labels.
    fn begin(&mut self, degree: usize, n: usize) {
        let cap = (2 * degree).next_power_of_two().max(16);
        let (len, shift) = if cap >= n {
            (n, 0)
        } else {
            (cap, 64 - cap.trailing_zeros())
        };
        if self.keys.len() < len {
            self.keys.resize(len, EMPTY);
            self.counts.resize(len, 0);
        }
        self.shift = shift;
    }

    /// Count one vote for label `l`.
    #[inline]
    fn add(&mut self, l: u32) {
        let slot = if self.shift == 0 {
            l as usize
        } else {
            let mask = (1usize << (64 - self.shift)) - 1;
            let mut i = (u64::from(l).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> self.shift) as usize;
            while self.keys[i] != EMPTY && self.keys[i] != l {
                i = (i + 1) & mask;
            }
            i
        };
        if self.keys[slot] == EMPTY {
            self.keys[slot] = l;
            self.counts[slot] = 1;
            self.touched.push(slot as u32);
        } else {
            self.counts[slot] += 1;
        }
    }

    /// The most frequent label, the smallest of those tied (the serial rule,
    /// which does not depend on the order the votes were counted in), or
    /// `None` without votes; and free the slots.
    fn take_best(&mut self) -> Option<u32> {
        let mut best: Option<(u32, u32)> = None;
        for &slot in &self.touched {
            let (l, c) = (self.keys[slot as usize], self.counts[slot as usize]);
            self.keys[slot as usize] = EMPTY;
            if best.is_none_or(|(bl, bc)| c > bc || (c == bc && l < bl)) {
                best = Some((l, c));
            }
        }
        self.touched.clear();
        best.map(|(l, _)| l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two 4-cliques {0,1,2,3} and {4,5,6,7} joined by a single bridge 3-4.
    fn two_cliques() -> Topology {
        let mut src = Vec::new();
        let mut dst = Vec::new();
        for &(a, b) in &[
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 2),
            (1, 3),
            (2, 3),
            (4, 5),
            (4, 6),
            (4, 7),
            (5, 6),
            (5, 7),
            (6, 7),
            (3, 4), // bridge
        ] {
            src.push(a);
            dst.push(b);
        }
        Topology::build(8, src, dst)
    }

    #[test]
    fn recovers_two_communities() {
        let t = two_cliques();
        let lab = label_propagation(&t, None, 20, Some(7), false);
        // Each clique collapses to one label; the two labels differ.
        assert!(lab[0] == lab[1] && lab[1] == lab[2] && lab[2] == lab[3]);
        assert!(lab[4] == lab[5] && lab[5] == lab[6] && lab[6] == lab[7]);
        assert_ne!(lab[0], lab[4]);
    }

    #[test]
    fn deterministic_given_a_seed() {
        let t = two_cliques();
        assert_eq!(
            label_propagation(&t, None, 20, Some(3), false),
            label_propagation(&t, None, 20, Some(3), false)
        );
    }

    /// The compact run (#326) gives the full run's labels.
    #[test]
    fn compact_runs_give_the_full_labels() {
        use crate::algo::linked::testing::{sparse, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m);
            for mask in [None, Some(&mask)] {
                let linked = Linked::of(&t, mask);
                for s in [None, Some(5)] {
                    for iters in [1, 3, 30] {
                        assert_eq!(
                            label_propagation_over(&t, mask, iters, s, Some(&linked)),
                            label_propagation_over(&t, mask, iters, s, None),
                            "graph {seed}, masked {}, seed {s:?}, {iters} iterations",
                            mask.is_some()
                        );
                        assert_eq!(
                            label_propagation_parallel(&t, mask, iters, s, Some(&linked)),
                            label_propagation_parallel(&t, mask, iters, s, None),
                            "parallel: graph {seed}, masked {}, seed {s:?}, {iters} iterations",
                            mask.is_some()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(label_propagation(&t, None, 20, None, false).is_empty());
    }

    /// Label propagation as written down, with no skipping: every node votes
    /// in every sweep. Serially, each vote reads the labels as they are; with
    /// `runs`, in the parallel frame: the votes of a run read the labels at
    /// its start, with the guard, and are written after.
    fn every_node_votes(
        t: &Topology,
        mask: Option<&EdgeMask>,
        max_iter: u32,
        seed: u64,
        runs: Option<usize>,
    ) -> Vec<u32> {
        let n = t.n_nodes();
        let (order, bounds) = sub_rounds(n, seed, None, runs.unwrap_or(1));
        let mut run = vec![0usize; n];
        for r in 0..bounds.len() - 1 {
            for &u in &order[bounds[r]..bounds[r + 1]] {
                run[u as usize] = r;
            }
        }
        let vote = |label: &[u32], u: u32| {
            let mut counts = std::collections::BTreeMap::new();
            let cur = label[u as usize];
            let mut add = |v: u32| {
                let guarded = runs.is_some() && run[v as usize] == run[u as usize];
                if !guarded || label[v as usize] <= cur {
                    *counts.entry(label[v as usize]).or_insert(0u32) += 1;
                }
            };
            each_kept(t.out(), u, mask, &mut add);
            each_kept(t.incoming(), u, mask, &mut add);
            // The most votes, the smallest label of those tied.
            counts
                .into_iter()
                .max_by_key(|&(l, c)| (c, std::cmp::Reverse(l)))
                .map_or(cur, |(l, _)| l)
        };
        let mut label: Vec<u32> = (0..n as u32).collect();
        for _ in 0..max_iter {
            let mut changed = false;
            for r in 0..bounds.len() - 1 {
                let nodes = &order[bounds[r]..bounds[r + 1]];
                let new: Vec<u32> = if runs.is_some() {
                    nodes.iter().map(|&u| vote(&label, u)).collect()
                } else {
                    // Serial: one run, each vote written at once.
                    for &u in nodes {
                        let l = vote(&label, u);
                        changed |= l != label[u as usize];
                        label[u as usize] = l;
                    }
                    continue;
                };
                for (&u, l) in nodes.iter().zip(new) {
                    changed |= l != label[u as usize];
                    label[u as usize] = l;
                }
            }
            if !changed {
                break;
            }
        }
        label
    }

    /// Skipping the nodes none of whose neighbours changed (#453), serial and
    /// parallel, gives the labels of voting every node in every sweep.
    #[test]
    fn skipping_settled_nodes_changes_no_label() {
        use crate::algo::linked::testing::{sparse, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m);
            t.incoming();
            for mask in [None, Some(&mask)] {
                for s in [1u64, 5] {
                    for iters in [1, 3, 30] {
                        assert_eq!(
                            label_propagation_over(&t, mask, iters, Some(s), None),
                            every_node_votes(&t, mask, iters, s, None),
                            "serial: graph {seed}, masked {}, seed {s}, {iters} iterations",
                            mask.is_some()
                        );
                        assert_eq!(
                            label_propagation_parallel(&t, mask, iters, Some(s), None),
                            every_node_votes(&t, mask, iters, s, Some(RUNS)),
                            "parallel: graph {seed}, masked {}, seed {s}, {iters} iterations",
                            mask.is_some()
                        );
                    }
                }
            }
        }
    }
}
