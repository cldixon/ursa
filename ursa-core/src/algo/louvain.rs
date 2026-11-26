//! Louvain community detection — modularity optimisation by local moving and
//! graph aggregation.
//!
//! Two phases alternate until modularity stops improving (Blondel et al.):
//!
//! 1. **Local moving.** Each node, visited in a `seed`-derived order, leaves its
//!    community and joins the neighbouring community that most increases
//!    modularity (staying put if nothing does). A moved node's neighbours are
//!    revisited, until no node can improve or the gain becomes negligible (see
//!    [`one_level`]).
//! 2. **Aggregation.** Each community collapses to a super-node; intra-community
//!    edges become a weighted self-loop and inter-community edges become weighted
//!    super-edges. Local moving then runs on the smaller weighted graph.
//!
//! Edges are treated as **undirected**: a directed input is symmetrised (each
//! directed edge contributes its weight in both directions; parallel/opposite
//! edges accumulate). [`louvain`] uses unit edge weights; [`louvain_weighted`]
//! takes a per-edge weight (gathered via `edge_ids`). `resolution` (γ) scales the
//! null-model term: larger γ favours smaller communities.
//!
//! With `parallel`, local moving runs in fixed sub-rounds instead (#387, see
//! [`one_level_parallel`]): the same objective and the same aggregation, but
//! nodes decide in parallel, so it finds different communities from the serial
//! default. They are still the same for a given seed on any thread count.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;

use crate::parallel::*;

use super::linked::Linked;
use super::rng::{shuffled_order, DEFAULT_SEED};
use super::subrounds::{places, run_of, sub_rounds};
use crate::topology::{EdgeMask, Topology};

/// Run `$body` with `$w` bound to the slice (or vector) inside a [`Weights`],
/// once per form, so the code in it is compiled for each weight type.
macro_rules! by_weights {
    ($weights:expr, $w:ident => $body:expr) => {
        match $weights {
            Weights::Counts($w) => $body,
            Weights::Floats($w) => $body,
        }
    };
}

/// Community label per node (contiguous `0..k`, by first appearance in node
/// order). The visit order, the ties and the aggregation follow the node
/// numbering, so a topology relabelled for locality (#172) can give other
/// communities for a seed than one in first-seen order, of the same quality.
/// Deterministic given `seed`
/// (defaulting to a fixed seed when `None`, so an unseeded run is reproducible).
/// A subgraph `mask` restricts the working graph to kept edges only.
///
/// `parallel` runs local moving in parallel sub-rounds ([`one_level_parallel`]):
/// faster on many cores, and as deterministic (the same labels on any thread
/// count), but not the serial run's labels. It has no NetworkX equivalent.
pub fn louvain(
    topo: &Topology,
    mask: Option<&EdgeMask>,
    resolution: f64,
    seed: Option<u64>,
    parallel: bool,
) -> Vec<u32> {
    louvain_impl(topo, None, mask, resolution, seed, parallel)
}

/// Weighted Louvain: as [`louvain`], but edge weights come from `weights` (per
/// edge row via `edge_ids`; non-negative, `len == n_edges`) instead of unity.
pub fn louvain_weighted(
    topo: &Topology,
    weights: &[f64],
    mask: Option<&EdgeMask>,
    resolution: f64,
    seed: Option<u64>,
    parallel: bool,
) -> Vec<u32> {
    assert_eq!(
        weights.len(),
        topo.n_edges(),
        "weights length must equal the edge count"
    );
    louvain_impl(topo, Some(weights), mask, resolution, seed, parallel)
}

fn louvain_impl(
    topo: &Topology,
    weights: Option<&[f64]>,
    mask: Option<&EdgeMask>,
    resolution: f64,
    seed: Option<u64>,
    parallel: bool,
) -> Vec<u32> {
    let n = topo.n_nodes();
    if n == 0 {
        return Vec::new();
    }
    // No edges: every node is its own community.
    if topo.n_edges() == 0 {
        return (0..n as u32).collect();
    }

    let linked = Linked::of(topo, mask);
    if linked.len() == 0 {
        return (0..n as u32).collect();
    }
    let linked = (linked.n_isolated() > 0).then_some(linked);
    let pace = Pace { parallel, ..PACE };
    louvain_over(topo, weights, mask, resolution, seed, linked, pace)
}

/// Louvain over `topo`, compact over the linked nodes when `linked` is given
/// (#326).
///
/// An isolated node has no neighbour, so it never moves and nothing moves to
/// it: it is a singleton at every level, and its visits only count as work
/// (see [`one_level`]). The compact run works on the linked (super-)nodes, in
/// node order, and keeps, for each level, which positions of the full
/// level's node order they hold (`level`): the full level numbers its
/// communities by first appearance, isolated singletons included, so a
/// linked community first met at compact node `s`, the `r`-th community met,
/// is full node `r + (pos[s] - s)` of the next level. Its labels end up as the
/// full run's: by first appearance over the nodes, isolated ones as their own.
fn louvain_over(
    topo: &Topology,
    weights: Option<&[f64]>,
    mask: Option<&EdgeMask>,
    resolution: f64,
    seed: Option<u64>,
    linked: Option<Linked>,
    pace: Pace,
) -> Vec<u32> {
    let seed = seed.unwrap_or(DEFAULT_SEED);
    let mut graph = Graph::from_topology(topo, weights, mask, linked.as_ref());
    let isolated = linked.as_ref().map_or(0, Linked::n_isolated);
    // Community of each *original* node, expressed in the current graph's node
    // space; updated (composed) at every level.
    let mut node_comm: Vec<u32> = (0..graph.n() as u32).collect();
    let mut level = linked.clone();

    loop {
        let comm = if pace.parallel {
            one_level_parallel(&graph, resolution, seed, level.as_ref(), pace)
        } else {
            one_level(&graph, resolution, seed, level.as_ref(), pace)
        };
        let c = comm.par_iter().copied().max().map_or(0, |m| m as usize + 1);
        // Compose this level's assignment onto the original nodes.
        node_comm
            .par_iter_mut()
            .for_each(|label| *label = comm[*label as usize]);
        // No community merged (every super-node stayed distinct) → converged.
        if c == graph.n() {
            break;
        }
        if let Some(l) = &level {
            let mut pos: Vec<u32> = Vec::with_capacity(c);
            for (s, (&r, &p)) in comm.iter().zip(l.nodes()).enumerate() {
                if r as usize == pos.len() {
                    pos.push(r + (p - s as u32));
                }
            }
            level = Some(Linked::from_sorted(c + isolated, pos));
        }
        graph = graph.aggregate_owned(&comm, c);
        if c + isolated == 1 {
            break;
        }
    }
    match &linked {
        None => node_comm,
        Some(l) => first_appearance(l, &node_comm),
    }
}

/// Labels over every node, by first appearance in node order, from the
/// compact nodes' communities: an isolated node is a community of its own.
fn first_appearance(linked: &Linked, comm: &[u32]) -> Vec<u32> {
    let mut map = vec![u32::MAX; comm.len()];
    let mut next = 0u32;
    let mut label = |slot: Option<&mut u32>| {
        let fresh = next;
        match slot {
            Some(slot) if *slot != u32::MAX => *slot,
            Some(slot) => {
                *slot = fresh;
                next += 1;
                fresh
            }
            None => {
                next += 1;
                fresh
            }
        }
    };
    let nodes = linked.nodes();
    let mut c = 0;
    (0..linked.n() as u32)
        .map(|v| {
            if nodes.get(c) == Some(&v) {
                c += 1;
                label(Some(&mut map[comm[c - 1] as usize]))
            } else {
                label(None)
            }
        })
        .collect()
}

/// Stop a level once a full sweep's worth of node visits (see [`one_level`])
/// raises modularity by less than this. NetworkX's `louvain_communities` default
/// `threshold`; in the same units (a modularity difference, so independent of
/// graph size). On the measured random, scale-free and planted graphs (250k–4M
/// edges) no sweep-equivalent of work gained under 2e-5, so this never trims real
/// progress there; it is a backstop against a level that keeps a few nodes trading
/// places for negligible gains (the shape of #125's tail).
const SWEEP_GAIN_THRESHOLD: f64 = 1e-7;

/// Hard backstop on one level's work, in sweeps of the working graph (one sweep =
/// one visit of every node and every adjacency slot). Measured levels used 2.6–25
/// sweep-equivalents (scale-free and the dense late levels of random graphs at
/// the top), growing only slowly with size, where the former sweep-to-zero-moves
/// loop took up to 1,734 full sweeps (#125). 64 leaves headroom above that while
/// bounding a level at O(64 · (n + m)).
const MAX_SWEEPS: usize = 64;

/// When a level stops: [`SWEEP_GAIN_THRESHOLD`] and [`MAX_SWEEPS`], checked
/// every sweep's worth of work. Tests shorten the sweep to stop levels mid-way.
/// With `parallel`, levels run [`one_level_parallel`], which checks after each
/// real sweep and has its own stop on few moves ([`MIN_MOVED_SHARE`]).
#[derive(Debug, Clone, Copy)]
struct Pace {
    threshold: f64,
    max_sweeps: usize,
    /// The work between checks, if not a sweep of the working graph (serial
    /// local moving only).
    sweep: Option<usize>,
    /// Local moving in parallel sub-rounds (#387).
    parallel: bool,
}

const PACE: Pace = Pace {
    threshold: SWEEP_GAIN_THRESHOLD,
    max_sweeps: MAX_SWEEPS,
    sweep: None,
    parallel: false,
};

/// How many runs [`one_level_parallel`] cuts a level's visit order into. A
/// constant, never the thread count, so the result is the same on any number
/// of threads. On the prototype (#387) quality hardly depended on it (64,
/// 1,024 and 16,384 runs gave modularity within 0.0006 of each other), and
/// fewer, larger runs are faster.
const RUNS: usize = 64;

/// [`one_level_parallel`] stops a level after a sweep that moves fewer than
/// one node in this many (of the full level's nodes). Without it, a level of
/// stale decisions has a long tail of sweeps that each move a handful of nodes
/// for almost nothing (the prototype ran all 64 sweeps on a planted-partition
/// graph).
const MIN_MOVED_SHARE: usize = 1000;

/// A run whose listed active nodes are at least one in this many of its
/// nodes finds them by scanning the run (in order, in parallel); a sparser one
/// sorts its list instead.
const DENSE_RUN: usize = 16;

/// Movers per piece of [`one_level_parallel`]'s marking step. A constant, so
/// the pieces, and the order their gains are summed in, never depend on the
/// thread count.
const MARK_CHUNK: usize = 256;

/// One full local-moving phase over `graph`, returning a contiguous `0..k`
/// community label per node.
///
/// Nodes are visited from a FIFO queue seeded with every node in a `seed`-derived
/// order. When a node moves, its neighbours outside its new community are
/// re-queued (if not already queued), so settled regions are not re-scanned
/// (Traag et al.'s fast local moving, as in Leiden). This is not quite a sweep to
/// zero moves: a move also shifts two community totals, which can tip a
/// non-neighbour, and that node is not revisited. Re-sweeping for those stragglers
/// reintroduces the #125 tail (each pass gains ~1e-3 down to 1e-5 for a full
/// sweep's cost) and measured *lower* final modularity on random, scale-free and
/// planted graphs, so it is not done. The level ends when the queue empties, when
/// a sweep's worth of visits gains less than [`SWEEP_GAIN_THRESHOLD`], or after
/// [`MAX_SWEEPS`] sweeps' worth. Serial and seeded, so deterministic and
/// independent of the thread count.
///
/// With `level`, `graph` is compact (see [`louvain_over`]): its nodes are the
/// `level.nodes()` of a full level of `level.n()` nodes, the rest isolated
/// singletons. The full level visits every node in a permutation of all
/// `level.n()`; visiting an isolated node changes nothing but adds one to the
/// work count, which decides when a level stops. So the compact level visits
/// its own nodes in the order the permutation gives them and, before each,
/// counts the isolated visits the full level makes there, all at once.
fn one_level(
    graph: &Graph,
    resolution: f64,
    seed: u64,
    level: Option<&Linked>,
    pace: Pace,
) -> Vec<u32> {
    by_weights!(&graph.weights, ws => one_level_in(graph, ws, resolution, seed, level, pace))
}

/// [`one_level`] over the graph's weights `ws`, one copy per weight type.
fn one_level_in<W: Weight>(
    graph: &Graph,
    ws: &[W],
    resolution: f64,
    seed: u64,
    level: Option<&Linked>,
    pace: Pace,
) -> Vec<u32> {
    let n = graph.n();
    let n_full = level.map_or(n, Linked::n);
    let mut comm: Vec<u32> = (0..n as u32).collect();
    let mut tot: Vec<f64> = graph.k.clone(); // Σ weighted-degree of each community
                                             // The visit order, and with `level`, the isolated visits before each of its
                                             // nodes and after the last.
    let (order, gaps, tail) = match level {
        None => (shuffled_order(n, seed), Vec::new(), 0),
        Some(l) => l.compact_order(&shuffled_order(n_full, seed)),
    };
    let first_visits = order.len();
    let mut visits = 0usize;
    let mut queue: VecDeque<u32> = order.into();
    let mut queued = vec![true; n];
    // Weight from the current node into each neighbouring community, dense over
    // community ids (`< n`), plus the communities touched for this node. Summed in
    // adjacency order, exactly as a map entry would be, and reset after each node.
    let mut weight_to = vec![0.0f64; n];
    let mut seen = vec![false; n];
    let mut touched: Vec<u32> = Vec::new();
    // The touched communities a move to could beat staying, with their gains.
    let mut better: Vec<(u32, f64)> = Vec::new();

    // Work is counted in adjacency slots visited (plus one per node visit), so a
    // sweep is `n + 2m` slots whatever the order in which nodes are visited.
    let sweep = pace.sweep.unwrap_or(n_full + graph.targets.len()).max(1);
    let budget = sweep.saturating_mul(pace.max_sweeps);
    let (mut work, mut next_check) = (0usize, sweep);
    // Raw gain (in `best_gain - stay` units) accumulated since the last check;
    // `· 2 / m2` converts it to a modularity difference.
    let mut gain = 0.0f64;

    'visits: while let Some(u) = queue.pop_front() {
        // The isolated nodes the full level visits just before this one: each
        // adds one to `work`, and the check below runs after each, so each
        // check falls on the first of them to reach `next_check`.
        if level.is_some() {
            let run = match visits.cmp(&first_visits) {
                std::cmp::Ordering::Less => gaps[visits] as usize,
                std::cmp::Ordering::Equal => tail as usize,
                std::cmp::Ordering::Greater => 0,
            };
            visits += 1;
            let end = work + run;
            while end >= next_check {
                // The visit that brings `work` to `next_check` runs the check.
                let at = next_check;
                if gain * 2.0 / graph.m2 < pace.threshold || at >= budget {
                    break 'visits;
                }
                gain = 0.0;
                next_check = at + sweep;
            }
            work = end;
        }
        queued[u as usize] = false;
        // The visit order is random, so each visit starts with cache misses:
        // ask for the next node's adjacency, and for the one after it its
        // place in the adjacency, community and degree (#268).
        if let Some(&next) = queue.get(1) {
            prefetch(&graph.offsets, next as usize);
            prefetch(&comm, next as usize);
            prefetch(&graph.k, next as usize);
        }
        if let Some(&next) = queue.front() {
            let s = graph.offsets[next as usize];
            if s < graph.targets.len() {
                prefetch(&graph.targets, s);
                prefetch(ws, s);
            }
        }
        let ci = comm[u as usize];
        let ku = graph.k[u as usize];
        let (nbrs, wts) = graph.neighbors(ws, u);
        work += nbrs.len() + 1;

        // Weight from u into each neighbouring community. Each slot reads a
        // neighbour's community, then that community's entries, all at
        // random: ask for the community `PREFETCH` slots ahead, for its
        // entries half as far ahead (by then the community has arrived), and
        // for the total of each community met, which the scan below reads.
        for (i, (&v, &w)) in nbrs.iter().zip(wts).enumerate() {
            if let Some(&ahead) = nbrs.get(i + PREFETCH) {
                prefetch(&comm, ahead as usize);
            }
            if let Some(&ahead) = nbrs.get(i + PREFETCH / 2) {
                let c = comm[ahead as usize] as usize;
                prefetch(&weight_to, c);
                prefetch(&seen, c);
            }
            let c = comm[v as usize];
            if !std::mem::replace(&mut seen[c as usize], true) {
                touched.push(c);
                prefetch(&tot, c as usize);
            }
            weight_to[c as usize] += w.get();
        }

        // Tentatively remove u from its community.
        tot[ci as usize] -= ku;

        // Gain of staying in ci (the baseline every move must strictly beat).
        let stay = weight_to[ci as usize] - resolution * tot[ci as usize] * ku / graph.m2;
        // Neighbour communities are scanned in ascending-id order, never in
        // the order they were met: with the epsilon, "tie" is non-transitive,
        // so scan order could change the partition. Ascending order with a
        // strict improvement (> best + eps) keeps the smallest id on a tie
        // (it's seen first) without letting chained near-ties drag the anchor.
        //
        // The best gain so far never drops below `stay`, so a community whose
        // gain is not above `stay + eps` can never be taken, wherever it falls
        // in the scan, and leaves the scan's state as it was. So only the
        // others (usually none, or one or two) are kept, sorted and scanned:
        // the same comparisons on the same values, in the same order, give
        // the same choice as scanning every touched community (#268).
        let bar = stay + 1e-12;
        for &c in &touched {
            let w_in = std::mem::take(&mut weight_to[c as usize]);
            seen[c as usize] = false;
            if c == ci {
                continue;
            }
            let gain = w_in - resolution * tot[c as usize] * ku / graph.m2;
            if gain > bar {
                better.push((c, gain));
            }
        }
        touched.clear();
        if better.len() > 1 {
            better.sort_unstable_by_key(|&(c, _)| c);
        }
        let mut best_c = ci;
        let mut best_gain = stay;
        for &(c, gain) in &better {
            if gain > best_gain + 1e-12 {
                best_gain = gain;
                best_c = c;
            }
        }
        better.clear();

        // Commit (re-insert into the chosen community).
        tot[best_c as usize] += ku;
        comm[u as usize] = best_c;
        if best_c != ci {
            gain += best_gain - stay;
            for &v in nbrs {
                if !queued[v as usize] && comm[v as usize] != best_c {
                    queued[v as usize] = true;
                    queue.push_back(v);
                }
            }
        }

        if work >= next_check {
            if gain * 2.0 / graph.m2 < pace.threshold || work >= budget {
                break;
            }
            gain = 0.0;
            next_check = work + sweep;
        }
    }
    // The scratch goes before renumbering allocates its own map.
    drop((tot, queue, queued, weight_to, seen));
    renumber(&comm)
}

/// How far ahead in a node's adjacency [`one_level`] asks for a neighbour's
/// community to be fetched into cache.
const PREFETCH: usize = 16;

/// Ask the CPU to bring `xs[i]` into cache (a hint only: it reads nothing and
/// changes nothing, so results cannot depend on it). `i` must be in bounds.
#[inline(always)]
fn prefetch<T>(xs: &[T], i: usize) {
    debug_assert!(i < xs.len());
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch never faults, and the address is inside `xs`.
    unsafe {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        _mm_prefetch(xs.as_ptr().add(i).cast::<i8>(), _MM_HINT_T0);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (xs, i);
}

/// One local-moving phase in parallel sub-rounds (#387), returning a
/// contiguous `0..k` community label per node.
///
/// The level's seeded visit order (the one [`one_level`] starts from) is cut
/// into [`RUNS`] runs of equal length. A sweep takes the runs in turn. In each,
/// every active node of the run picks its best community in parallel against
/// the state (`comm`, `tot`, community sizes) as it was when the run started,
/// writing nothing; the moves are then applied in run order on one thread
/// (`comm`, `tot` and sizes); and, in parallel, the neighbours of the movers
/// outside their new community are marked active (setting a flag is the same
/// in any order) and the run's change in intra-community weight is summed.
/// Each node's arithmetic is [`one_level`]'s: weights summed in adjacency
/// order, candidates scanned in ascending community id, the same `1e-12` rule.
///
/// Nodes of one run decide on stale state, so two single-node communities could
/// swap places for ever: a single node may only join another single node of
/// the same run with a lower community id (the rule of Lu, Halappanavar and
/// Kalyanaraman's Grappolo). A single node in another run decides on the state
/// this run leaves, so it needs no guard.
///
/// Deterministic on any thread count: the runs depend only on the node count,
/// the seed and [`RUNS`]; a decision reads only state frozen at the start of
/// its run; and every float sum has a fixed order (per node; `tot` in run
/// order; the gain in fixed chunks of [`MARK_CHUNK`] movers, in order). Which
/// thread marks a node active does not matter, and each run's active nodes are
/// taken in run order.
///
/// Each run keeps a list of the nodes activated in it, so late sweeps touch
/// only those. Each worker sums a node's weights in a small table sized to the
/// node's degree ([`Tally`]), not in a dense array per thread.
///
/// A level stops after a sweep that moves no node, raises modularity by less
/// than the pace's threshold, or moves fewer than one node in
/// [`MIN_MOVED_SHARE`]; or after the pace's `max_sweeps` sweeps. The gain is
/// the sweep's real change in modularity, not the sum of the movers' gains
/// against stale state: two nodes that each join the other's community (or a
/// handful that keep trading places) estimate a gain every sweep and make none,
/// and the real gain stops them.
///
/// With `level`, `graph` is compact (see [`louvain_over`]): the runs are cut
/// from the full level's order, isolated nodes included, and each holds the
/// linked nodes that fall in it. An isolated node never moves and nothing moves
/// to it, and compact ids keep the order of node ids, so the compact level
/// makes the full level's moves, in the same order.
fn one_level_parallel(
    graph: &Graph,
    resolution: f64,
    seed: u64,
    level: Option<&Linked>,
    pace: Pace,
) -> Vec<u32> {
    by_weights!(&graph.weights, ws => {
        one_level_parallel_in(graph, ws, resolution, seed, level, pace)
    })
}

/// [`one_level_parallel`] over the graph's weights `ws`, one copy per weight
/// type.
fn one_level_parallel_in<W: Weight>(
    graph: &Graph,
    ws: &[W],
    resolution: f64,
    seed: u64,
    level: Option<&Linked>,
    pace: Pace,
) -> Vec<u32> {
    let n = graph.n();
    let n_full = level.map_or(n, Linked::n);
    // The visit order of the working graph's nodes, where each run starts in
    // it (`RUNS + 1` boundaries), and each node's place in it.
    let (order, bounds) = sub_rounds(n_full, seed, level, RUNS);
    let place = places(&order);

    let mut comm: Vec<u32> = (0..n as u32).collect();
    let mut tot: Vec<f64> = graph.k.clone();
    let mut size: Vec<u32> = vec![1; n];
    let active: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(true)).collect();
    // The places (in `order`) of the nodes activated in each run since it
    // last ran.
    let mut listed: Vec<Vec<u32>> = vec![Vec::new(); RUNS];
    let tallies: Vec<Mutex<Tally>> = (0..current_num_threads().max(1))
        .map(|_| Mutex::new(Tally::default()))
        .collect();

    // The community each mover of the current run left, by its place in the
    // run (`EMPTY` for a node that did not move).
    let longest = bounds.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
    let mut left: Vec<u32> = vec![EMPTY; longest];
    let (m2, res) = (graph.m2, resolution);

    let mut sweeps = 0usize;
    loop {
        // The sweep's modularity gain, exactly (not the movers' estimates
        // against stale state): the change in intra-community weight, in
        // ordered pairs, and in the sum of squared community totals.
        let (mut intra, mut squares, mut moved) = (0.0f64, 0.0f64, 0usize);
        for r in 0..RUNS {
            let (lo, hi) = (bounds[r], bounds[r + 1]);
            let mut list = std::mem::take(&mut listed[r]);
            if lo == hi {
                continue;
            }
            // Decide, in parallel, against the state at the start of the run:
            // each active node of the run, in run order, and keep the movers
            // with their new community. A decided node is no longer active
            // (nothing marks nodes while the run decides).
            let movers: Vec<(u32, u32)> = {
                let state = (comm.as_slice(), tot.as_slice(), size.as_slice());
                let pick = |u: u32| {
                    let flag = &active[u as usize];
                    if !flag.load(Relaxed) {
                        return None;
                    }
                    flag.store(false, Relaxed);
                    let mut tally = tallies[worker_index() % tallies.len()]
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let same_run = |v: u32| (lo..hi).contains(&(place[v as usize] as usize));
                    let c = decide(graph, ws, state, res, u, &mut tally, same_run);
                    (c != state.0[u as usize]).then_some((u, c))
                };
                if sweeps == 0 || list.len() * DENSE_RUN >= hi - lo {
                    order[lo..hi].par_iter().filter_map(|&u| pick(u)).collect()
                } else {
                    list.sort_unstable();
                    for i in list.iter_mut() {
                        *i = order[*i as usize];
                    }
                    list.par_iter().filter_map(|&u| pick(u)).collect()
                }
            };
            drop(list);
            if movers.is_empty() {
                continue;
            }

            // Apply, in run order, on one thread.
            for &(u, c) in &movers {
                let ci = comm[u as usize];
                let ku = graph.k[u as usize];
                let (from, to) = (tot[ci as usize], tot[c as usize]);
                tot[ci as usize] = from - ku;
                tot[c as usize] = to + ku;
                squares += (from - ku) * (from - ku) - from * from;
                squares += (to + ku) * (to + ku) - to * to;
                size[ci as usize] -= 1;
                size[c as usize] += 1;
                comm[u as usize] = c;
                left[place[u as usize] as usize - lo] = ci;
            }
            moved += movers.len();

            // Mark the movers' neighbours outside their new community, in
            // parallel (each node woken is listed once, in its own run), and
            // sum the change in intra-community weight over the movers' edges:
            // in fixed chunks of movers, each in adjacency order, the chunks
            // in order.
            let (comm_r, active_r, place_r, left_r) = (&comm, &active, &place, &left);
            // The community a node of this run left, if it moved in it.
            let moved_from = |v: u32| {
                let i = place_r[v as usize] as usize;
                (lo..hi)
                    .contains(&i)
                    .then(|| left_r[i - lo])
                    .filter(|&c| c != EMPTY)
            };
            let marked: Vec<(Vec<u32>, f64)> = movers
                .par_chunks(MARK_CHUNK)
                .map(|chunk| {
                    let (mut woken, mut delta) = (Vec::new(), 0.0f64);
                    for &(u, c) in chunk {
                        let ci = moved_from(u).expect("a mover");
                        let (nbrs, wts) = graph.neighbors(ws, u);
                        for (&v, &w) in nbrs.iter().zip(wts) {
                            let (cv, w) = (comm_r[v as usize], w.get());
                            // An edge to another mover is counted from both
                            // ends; any other, from this one for both.
                            let (bv, w) = match moved_from(v) {
                                Some(bv) => (bv, w),
                                None => (cv, 2.0 * w),
                            };
                            let (now, was) = (cv == c, bv == ci);
                            if now != was {
                                delta += if now { w } else { -w };
                            }
                            let flag = &active_r[v as usize];
                            if cv != c && !flag.load(Relaxed) && !flag.swap(true, Relaxed) {
                                woken.push(place_r[v as usize]);
                            }
                        }
                    }
                    (woken, delta)
                })
                .collect();
            for (woken, delta) in marked {
                intra += delta;
                for i in woken {
                    listed[run_of(&bounds, i as usize)].push(i);
                }
            }
            for &(u, _) in &movers {
                left[place[u as usize] as usize - lo] = EMPTY;
            }
        }
        sweeps += 1;
        let gain = intra / m2 - res * squares / (m2 * m2);
        if moved == 0
            || gain < pace.threshold
            || sweeps >= pace.max_sweeps
            || moved.saturating_mul(MIN_MOVED_SHARE) < n_full
        {
            break;
        }
    }
    renumber(&comm)
}

/// The best community for node `u` against `(comm, tot, size)` (its own when
/// it stays): the arithmetic of [`one_level`], with [`one_level_parallel`]'s
/// single-node guard.
#[inline]
fn decide<W: Weight>(
    graph: &Graph,
    ws: &[W],
    (comm, tot, size): (&[u32], &[f64], &[u32]),
    resolution: f64,
    u: u32,
    tally: &mut Tally,
    same_run: impl Fn(u32) -> bool,
) -> u32 {
    let ci = comm[u as usize];
    let ku = graph.k[u as usize];
    let (nbrs, wts) = graph.neighbors(ws, u);
    tally.begin(nbrs.len(), graph.n());
    for (&v, &w) in nbrs.iter().zip(wts) {
        tally.add(comm[v as usize], w.get(), v);
    }
    tally.touched.sort_unstable_by_key(|&(c, _, _)| c);
    let w_ci = tally
        .touched
        .binary_search_by_key(&ci, |&(c, _, _)| c)
        .map_or(0.0, |i| tally.vals[tally.touched[i].1 as usize]);
    let stay = w_ci - resolution * (tot[ci as usize] - ku) * ku / graph.m2;
    let (mut best_c, mut best_gain) = (ci, stay);
    let single = size[ci as usize] == 1;
    for &(c, slot, member) in &tally.touched {
        // Two single nodes deciding in the same run would swap for ever: only
        // the higher id may join the lower. (A single node's community is met
        // through its one member.)
        if c == ci || (single && c > ci && size[c as usize] == 1 && same_run(member)) {
            continue;
        }
        let g = tally.vals[slot as usize] - resolution * tot[c as usize] * ku / graph.m2;
        if g > best_gain + 1e-12 {
            best_gain = g;
            best_c = c;
        }
    }
    tally.end();
    best_c
}

/// A worker's scratch for [`decide`]: the weight from one node into each
/// neighbouring community. An open-addressing table sized to the node's degree
/// (twice it, to a power of two), or, for a hub whose table would be as large
/// as the graph, a dense array over community ids. Only the slots a node
/// touched are reset after it, so a worker holds the table of the largest
/// degree it has met, not 9 bytes for every node of the graph.
#[derive(Default)]
struct Tally {
    /// The community in each slot (`EMPTY` when free).
    keys: Vec<u32>,
    vals: Vec<f64>,
    /// The communities met, with their slots and the first neighbour met in
    /// each, in the order met.
    touched: Vec<(u32, u32, u32)>,
    /// `64 - log2(capacity)` of the table; 0 when dense.
    shift: u32,
}

const EMPTY: u32 = u32::MAX;

impl Tally {
    /// Ready the table for a node of `degree` neighbours among `n` communities.
    fn begin(&mut self, degree: usize, n: usize) {
        let cap = (2 * degree).next_power_of_two().max(16);
        let (len, shift) = if cap >= n {
            (n, 0)
        } else {
            (cap, 64 - cap.trailing_zeros())
        };
        if self.keys.len() < len {
            self.keys.resize(len, EMPTY);
            self.vals.resize(len, 0.0);
        }
        self.shift = shift;
    }

    /// Add `w`, the weight of the edge to neighbour `v`, to community `c`'s.
    #[inline]
    fn add(&mut self, c: u32, w: f64, v: u32) {
        let slot = if self.shift == 0 {
            c as usize
        } else {
            let mask = (1usize << (64 - self.shift)) - 1;
            let mut i = (u64::from(c).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> self.shift) as usize;
            while self.keys[i] != EMPTY && self.keys[i] != c {
                i = (i + 1) & mask;
            }
            i
        };
        if self.keys[slot] == EMPTY {
            self.keys[slot] = c;
            self.vals[slot] = w;
            self.touched.push((c, slot as u32, v));
        } else {
            self.vals[slot] += w;
        }
    }

    /// Free the slots the node touched.
    fn end(&mut self) {
        for &(_, slot, _) in &self.touched {
            self.keys[slot as usize] = EMPTY;
        }
        self.touched.clear();
    }
}

/// An undirected, weighted working graph in flat CSR form (`u32` targets and
/// weights in parallel arrays). Self-loops are kept apart from the adjacency
/// so the weighted degree `k` can count them (twice, per convention) without
/// polluting the neighbour scan.
struct Graph {
    /// Symmetric adjacency: edge `{u, v}` of weight `w` appears as `(v, w)` in
    /// `u`'s segment and `(u, w)` in `v`'s. Segment `u` is
    /// `offsets[u]..offsets[u + 1]`, sorted by neighbour id with no duplicates.
    offsets: Vec<usize>,
    targets: Vec<u32>,
    weights: Weights,
    self_loop: Vec<f64>,
    /// Weighted degree: `Σ weights[segment i] + 2 · self_loop[i]`.
    k: Vec<f64>,
    /// `2m` — total weighted degree over all nodes.
    m2: f64,
    /// Every weight and self-loop is a non-negative integer and `m2 < 2^52`,
    /// so every sum [`Graph::aggregate`] forms (of these values and their
    /// halves) is exact, and the same in any order (#359).
    exact: bool,
}

/// The weights of a [`Graph`]'s adjacency slots (#450). An unweighted level 0
/// stores each slot's count of parallel edges in one byte, not eight: it is
/// 1, or 2 for a reciprocal pair, and rarely more. Every other graph (weighted,
/// any aggregated level, or a level 0 with more than 255 parallel edges
/// between two nodes) stores `f64`s.
///
/// A count reads as `count as f64`, the value an `f64` store holds for it, so
/// every sum sees the same operands in the same order: results are the same
/// bit for bit. The loops that read weights are generic over [`Weight`] and
/// chosen once per pass (`by_weights!`), so they stay monomorphic.
enum Weights {
    Counts(Vec<u8>),
    Floats(Vec<f64>),
}

/// A slot weight as a [`Weights`] store holds it.
trait Weight: Copy + Send + Sync + 'static {
    /// The weight, as every sum uses it.
    fn get(self) -> f64;
    /// The weight of a slot coalescing `run` parallel unit edges (`run` fits).
    fn run(run: usize) -> Self;
    /// Add `w` into `self` if the sum fits the store; `false` (and `self`
    /// unchanged) if it does not.
    fn merge(&mut self, w: Self) -> bool;
}

impl Weight for u8 {
    #[inline(always)]
    fn get(self) -> f64 {
        f64::from(self)
    }
    #[inline(always)]
    fn run(run: usize) -> Self {
        debug_assert!(run <= u8::MAX as usize);
        run as u8
    }
    #[inline(always)]
    fn merge(&mut self, w: Self) -> bool {
        match self.checked_add(w) {
            Some(sum) => {
                *self = sum;
                true
            }
            None => false,
        }
    }
}

impl Weight for f64 {
    #[inline(always)]
    fn get(self) -> f64 {
        self
    }
    #[inline(always)]
    fn run(run: usize) -> Self {
        run as f64
    }
    #[inline(always)]
    fn merge(&mut self, w: Self) -> bool {
        *self += w;
        true
    }
}

/// Split `0..n` into at most `parts` contiguous node ranges of roughly equal
/// segment volume under `offsets` (length `n + 1`), returned as `parts + 1`
/// boundaries. Only the work split depends on this; every node's result is
/// computed independently, so output never depends on `parts`.
fn node_blocks(offsets: &[usize], parts: usize) -> Vec<usize> {
    let n = offsets.len() - 1;
    let total = offsets[n];
    let mut bounds = vec![0usize];
    for p in 1..parts {
        let target = total / parts * p;
        let b = offsets.partition_point(|&o| o < target).min(n);
        if b > *bounds.last().unwrap() {
            bounds.push(b);
        }
    }
    if *bounds.last().unwrap() < n || bounds.len() == 1 {
        bounds.push(n);
    }
    bounds
}

/// Carve `buf` into the consecutive pieces `[offsets[bounds[i]]..offsets[bounds[i + 1]]]`.
fn split_blocks<'a, T>(
    mut buf: &'a mut [T],
    offsets: &[usize],
    bounds: &[usize],
) -> Vec<&'a mut [T]> {
    let mut pieces = Vec::with_capacity(bounds.len() - 1);
    for w in bounds.windows(2) {
        let (head, rest) = buf.split_at_mut(offsets[w[1]] - offsets[w[0]]);
        pieces.push(head);
        buf = rest;
    }
    pieces
}

/// `from_topology`'s unweighted coalescing: each sorted segment of `targets`
/// (under `offsets`) has its runs of equal neighbours written over its front,
/// one entry per run, with the run's length as its weight in `ws` (laid out
/// under `new_offsets`). Blocks of nodes (`blocks`) run in parallel.
fn write_runs<W: Weight>(
    targets: &mut [u32],
    ws: &mut [W],
    (offsets, new_offsets): (&[usize], &[usize]),
    blocks: &[usize],
) {
    let block_ranges: Vec<(usize, usize)> = blocks.windows(2).map(|w| (w[0], w[1])).collect();
    split_blocks(targets, offsets, blocks)
        .into_par_iter()
        .zip(split_blocks(ws, new_offsets, blocks))
        .zip(block_ranges.par_iter())
        .for_each(|((ts, ws), &(lo, hi))| {
            let base = offsets[lo];
            let mut k = 0;
            for u in lo..hi {
                let (mut i, end) = (offsets[u] - base, offsets[u + 1] - base);
                while i < end {
                    let v = ts[i];
                    let run = ts[i..end].iter().take_while(|&&x| x == v).count();
                    // `k <= i`: the entry is read before it is written.
                    ts[k] = v;
                    ws[k] = W::run(run);
                    k += 1;
                    i += run;
                }
            }
        });
}

/// [`Graph::aggregate_owned`]'s first step, in place: each node's segment
/// (under `offsets`) has its neighbours relabelled to their communities,
/// edges inside its own community folded into its self-loop, and the rest
/// sorted and coalesced over the segment's front; the segments are then moved
/// down and the arrays shrunk. Returns the new offsets.
///
/// Coalesced weights are summed in the store's own type. A one-byte count
/// that would pass 255 starts another entry for the same community instead
/// (#450): a node's segment may then list a community more than once, which
/// [`Graph::collapse`] sums like any other entry, and since every weight is
/// a whole number (the graph is [`exact`](Graph::exact)) the sums come out the
/// same. A segment never grows: each entry written takes one read before it.
fn fold_segments<W: Weight>(
    offsets: &[usize],
    targets: &mut Vec<u32>,
    weights: &mut Vec<W>,
    self_loop: &mut [f64],
    comm: &[u32],
) -> Vec<usize> {
    let n = self_loop.len();
    let bounds = node_blocks(offsets, 4 * current_num_threads());
    let ranges: Vec<(usize, usize)> = bounds.windows(2).map(|w| (w[0], w[1])).collect();
    let lens: Vec<usize> = split_blocks(targets, offsets, &bounds)
        .into_par_iter()
        .zip(split_blocks(weights, offsets, &bounds))
        .zip(split_at_bounds(self_loop, &bounds))
        .zip(ranges.par_iter())
        .map_init(
            Vec::new,
            |scratch: &mut Vec<(u32, W)>, (((ts, ws), loops), &(lo, hi))| {
                let base = offsets[lo];
                let mut lens = Vec::with_capacity(hi - lo);
                let mut k = 0;
                for u in lo..hi {
                    let cu = comm[u];
                    let seg = offsets[u] - base..offsets[u + 1] - base;
                    scratch.clear();
                    for (&v, &w) in ts[seg.clone()].iter().zip(&ws[seg]) {
                        let cv = comm[v as usize];
                        if cv == cu {
                            loops[u - lo] += w.get() / 2.0;
                        } else {
                            scratch.push((cv, w));
                        }
                    }
                    scratch.sort_unstable_by_key(|&(cv, _)| cv);
                    let first = k;
                    for &(cv, w) in scratch.iter() {
                        // `k` never passes the segment's start, which is read.
                        if !(k > first && ts[k - 1] == cv && ws[k - 1].merge(w)) {
                            ts[k] = cv;
                            ws[k] = w;
                            k += 1;
                        }
                    }
                    lens.push(k - first);
                }
                lens
            },
        )
        .collect::<Vec<_>>()
        .concat();
    let mut new_offsets = vec![0usize; n + 1];
    for u in 0..n {
        new_offsets[u + 1] = new_offsets[u] + lens[u];
    }
    drop(lens);
    for w in bounds.windows(2) {
        let (from, to) = (offsets[w[0]], new_offsets[w[0]]);
        let len = new_offsets[w[1]] - to;
        targets.copy_within(from..from + len, to);
        weights.copy_within(from..from + len, to);
    }
    let total = new_offsets[n];
    targets.truncate(total);
    targets.shrink_to_fit();
    weights.truncate(total);
    weights.shrink_to_fit();
    new_offsets
}

/// Carve `buf` into the consecutive pieces `[bounds[i]..bounds[i + 1]]`.
fn split_at_bounds<'a, T>(mut buf: &'a mut [T], bounds: &[usize]) -> Vec<&'a mut [T]> {
    let mut pieces = Vec::with_capacity(bounds.len() - 1);
    for w in bounds.windows(2) {
        let (head, rest) = buf.split_at_mut(w[1] - w[0]);
        pieces.push(head);
        buf = rest;
    }
    pieces
}

impl Graph {
    fn n(&self) -> usize {
        self.self_loop.len()
    }

    /// Node `u`'s neighbours and their weights, read from `ws` (this graph's
    /// weight store, as `by_weights!` gives it).
    #[inline]
    fn neighbors<'a, W>(&'a self, ws: &'a [W], u: u32) -> (&'a [u32], &'a [W]) {
        let (s, e) = (self.offsets[u as usize], self.offsets[u as usize + 1]);
        (&self.targets[s..e], &ws[s..e])
    }

    /// Every slot's weight as an `f64` (tests compare graphs by them).
    #[cfg(test)]
    fn weight_values(&self) -> Vec<f64> {
        by_weights!(&self.weights, ws => ws.iter().map(|w| w.get()).collect())
    }

    /// Build the initial undirected working graph. `weights` (per edge row, gathered
    /// via `edge_ids`) sets each edge's weight; `None` uses unit weights.
    ///
    /// The dominant (level-0) graph is built by counting sort rather than one
    /// `HashMap` per node: each non-self edge `(u, v, w)` scatters `(v, w)` into
    /// `u`'s segment and `(u, w)` into `v`'s, then each segment is sorted by
    /// neighbour id and parallel edges coalesced (weights summed) in place. Sorted
    /// segments make the neighbour order, and with it every weighted sum,
    /// deterministic.
    ///
    /// Memory (#352): the scatter buffers become the final CSR arrays, compacted
    /// in place, so the adjacency is never held twice. Targets and weights are
    /// separate arrays (12 bytes an entry, not a padded 16), and an unweighted
    /// run scatters targets alone (4 bytes) and derives each weight from the
    /// count of parallel edges, which it stores in one byte (#450, see
    /// [`Weights`]): 5 bytes an entry instead of 12.
    ///
    /// The histogram and scatter run over contiguous chunks of source nodes in
    /// parallel. Each chunk's per-node counts become its base cursor into every
    /// segment, so a segment receives its entries in exactly the order a serial
    /// row-order pass would produce, whatever the chunking (the same scheme as
    /// `Adjacency::transpose`).
    ///
    /// With `linked`, the graph is over the linked nodes only, in node order
    /// (compact ids keep the order of node ids, so every segment sorts and sums
    /// as it would in the full graph).
    fn from_topology(
        topo: &Topology,
        weights: Option<&[f64]>,
        mask: Option<&EdgeMask>,
        linked: Option<&Linked>,
    ) -> Graph {
        let n = linked.map_or(topo.n_nodes(), Linked::len);
        // The node of compact node `u`, and the compact node of node `v`.
        let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
        let at = |v: u32| linked.map_or(v, |l| l.compact(v));
        let out = topo.out();
        let m = out.targets.len();
        // Under a subgraph mask, only kept edges enter the working graph. The
        // histogram and scatter below share this predicate so segment sizes match.
        let keep = |e: u32| mask.is_none_or(|m| m.keep(e));
        // Each out-slot of node `uo`: its neighbour, and its edge row when a
        // mask or a weight reads it. Without either, the edge ids are never
        // read, so never built (#170); the row then reads as 0, unused.
        let ids = (mask.is_some() || weights.is_some()).then(|| &out.edge_ids[..]);
        let slots = |uo: u32| {
            let lo = out.offsets[uo as usize] as usize;
            out.neighbors(uo)
                .iter()
                .enumerate()
                .map(move |(i, &v)| (v, ids.map_or(0, |ids| ids[lo + i])))
        };

        // Source-node chunks of roughly equal out-slot count. Chunk count is capped
        // by mean degree, as in `transpose`, so the per-chunk histograms (n u32s
        // each) stay within a small multiple of the edge list.
        const PARALLEL_MIN_EDGES: usize = 1 << 16;
        let threads = current_num_threads();
        let n_chunks = if m < PARALLEL_MIN_EDGES || threads <= 1 || in_worker() {
            1
        } else {
            threads.min((m / n.max(1)).max(4))
        };
        let out_offsets: Vec<usize> = match linked {
            None => out.offsets.iter().map(|&o| o as usize).collect(),
            Some(l) => std::iter::once(0)
                .chain(l.nodes().iter().scan(0usize, |acc, &v| {
                    *acc += out.degree(v) as usize;
                    Some(*acc)
                }))
                .collect(),
        };
        let chunks = node_blocks(&out_offsets, n_chunks);
        let ranges: Vec<(usize, usize)> = chunks.windows(2).map(|w| (w[0], w[1])).collect();

        // Pass 1: per-chunk degree histogram; every non-self edge counts toward
        // both endpoints.
        let hist = |&(lo, hi): &(usize, usize)| {
            let mut h = vec![0u32; n];
            for u in lo as u32..hi as u32 {
                let uo = node(u);
                for (v, e) in slots(uo) {
                    let v = at(v);
                    if keep(e) && u != v {
                        h[u as usize] += 1;
                        h[v as usize] += 1;
                    }
                }
            }
            h
        };
        let mut hists: Vec<Vec<u32>> = ranges.par_iter().map(hist).collect();
        let mut offsets = vec![0usize; n + 1];
        for u in 0..n {
            offsets[u + 1] = offsets[u] + hists.iter().map(|h| h[u] as usize).sum::<usize>();
        }
        // Each chunk's histogram becomes its cursor into every segment, relative
        // to the segment start (a node's degree is at most `m`, so `u32` holds it).
        let mut running = vec![0u32; n];
        for h in hists.iter_mut() {
            for (slot, run) in h.iter_mut().zip(running.iter_mut()) {
                let cnt = *slot;
                *slot = *run;
                *run += cnt;
            }
        }
        drop(running);

        // Pass 2: scatter each edge into both endpoints' segments, and sum
        // self-loops (a node's own, so chunk-local). Positions are disjoint across
        // chunks (the cursors partition each segment), so writes never overlap;
        // the pointers cross threads as plain addresses, as in `transpose`.
        //
        // Targets and weights go to separate arrays (#352): a `(u32, f64)` pair
        // pads to 16 bytes, two arrays take 12. An unweighted run stores no
        // weights at all: every weight is 1.0, so a coalesced weight is the
        // number of entries a segment has for that neighbour, counted in pass 3.
        let total = offsets[n];
        let mut targets = vec![0u32; total];
        let mut wts: Vec<f64> = vec![0.0; if weights.is_some() { total } else { 0 }];
        let (t_addr, w_addr) = (targets.as_mut_ptr() as usize, wts.as_mut_ptr() as usize);
        let scatter = |(&(lo, hi), mut cur): (&(usize, usize), Vec<u32>)| {
            let mut loops = vec![0.0f64; hi - lo];
            for u in lo as u32..hi as u32 {
                let uo = node(u);
                for (v, e) in slots(uo) {
                    if !keep(e) {
                        continue;
                    }
                    let v = at(v);
                    let w = weights.map_or(1.0, |ws| ws[e as usize]);
                    if u == v {
                        loops[u as usize - lo] += w;
                        continue;
                    }
                    for (a, b) in [(u, v), (v, u)] {
                        let pos = offsets[a as usize] + cur[a as usize] as usize;
                        cur[a as usize] += 1;
                        // SAFETY: `pos < total` and is claimed by exactly one
                        // (chunk, slot); `targets` and `wts` (of length
                        // `total` when weighted) outlive the loop.
                        unsafe {
                            *(t_addr as *mut u32).add(pos) = b;
                            if weights.is_some() {
                                *(w_addr as *mut f64).add(pos) = w;
                            }
                        }
                    }
                }
            }
            loops
        };
        let self_loop: Vec<f64> = ranges
            .par_iter()
            .zip(hists)
            .map(scatter)
            .collect::<Vec<_>>()
            .concat();

        // Pass 3: sort each segment by neighbour id and coalesce parallel edges,
        // writing the coalesced entries to the front of their block, in place.
        let blocks = node_blocks(&offsets, 4 * threads);
        let block_ranges: Vec<(usize, usize)> = blocks.windows(2).map(|w| (w[0], w[1])).collect();
        let lens: Vec<usize>;
        let new_offsets: Vec<usize>;
        // An unweighted run's weights, written at their final places.
        let mut counted: Option<Weights> = None;
        let lens_to_offsets = |lens: &[usize]| {
            let mut o = vec![0usize; n + 1];
            for u in 0..n {
                o[u + 1] = o[u] + lens[u];
            }
            o
        };
        if weights.is_some() {
            // Each segment is sorted as `(u32, f64)` pairs in a per-worker
            // scratch buffer (a segment's worth, not the edge list's), with the
            // same unstable sort as ever: the order it leaves parallel edges in
            // is the order their weights are summed, so this keeps every sum
            // bit for bit.
            lens = split_blocks(&mut targets, &offsets, &blocks)
                .into_par_iter()
                .zip(split_blocks(&mut wts, &offsets, &blocks))
                .zip(block_ranges.par_iter())
                .map_init(
                    Vec::new,
                    |scratch: &mut Vec<(u32, f64)>, ((ts, ws), &(lo, hi))| {
                        let base = offsets[lo];
                        let mut lens = Vec::with_capacity(hi - lo);
                        let mut k = 0;
                        for u in lo..hi {
                            let seg = offsets[u] - base..offsets[u + 1] - base;
                            scratch.clear();
                            scratch.extend(
                                ts[seg.clone()].iter().copied().zip(ws[seg].iter().copied()),
                            );
                            scratch.sort_unstable_by_key(|&(v, _)| v);
                            let mut len = 0;
                            for i in 0..scratch.len() {
                                if len > 0 && scratch[len - 1].0 == scratch[i].0 {
                                    scratch[len - 1].1 += scratch[i].1;
                                } else {
                                    scratch[len] = scratch[i];
                                    len += 1;
                                }
                            }
                            // `k` never passes the segment's start, which is read.
                            for &(v, w) in &scratch[..len] {
                                ts[k] = v;
                                ws[k] = w;
                                k += 1;
                            }
                            lens.push(len);
                        }
                        lens
                    },
                )
                .collect::<Vec<_>>()
                .concat();
            new_offsets = lens_to_offsets(&lens);
        } else {
            // Sort, and count the distinct neighbours, which sizes the weights,
            // and the longest run of one neighbour, which picks their store.
            let counts: Vec<(Vec<usize>, usize)> = split_blocks(&mut targets, &offsets, &blocks)
                .into_par_iter()
                .zip(block_ranges.par_iter())
                .map(|(ts, &(lo, hi))| {
                    let base = offsets[lo];
                    let mut longest = 0;
                    let lens = (lo..hi)
                        .map(|u| {
                            let seg = &mut ts[offsets[u] - base..offsets[u + 1] - base];
                            seg.sort_unstable();
                            let mut len = 0;
                            for run in seg.chunk_by(|a, b| a == b) {
                                longest = longest.max(run.len());
                                len += 1;
                            }
                            len
                        })
                        .collect::<Vec<_>>();
                    (lens, longest)
                })
                .collect();
            let longest = counts.iter().map(|c| c.1).max().unwrap_or(0);
            lens = counts.into_iter().flat_map(|c| c.0).collect();
            new_offsets = lens_to_offsets(&lens);
            // Each run of equal neighbours becomes one entry whose weight is
            // the run's length: the sum of that many 1.0s, exactly. One byte
            // holds it unless some run is longer than 255 (#450).
            let (old, new) = ((offsets.as_slice(), new_offsets.as_slice()), &blocks);
            counted = Some(if longest <= usize::from(u8::MAX) {
                let mut ws = vec![0u8; new_offsets[n]];
                write_runs(&mut targets, &mut ws, old, new);
                Weights::Counts(ws)
            } else {
                let mut ws = vec![0.0f64; new_offsets[n]];
                write_runs(&mut targets, &mut ws, old, new);
                Weights::Floats(ws)
            });
        }
        drop(lens);

        // Pass 4: move each block's coalesced entries down to their place in
        // the final CSR, in block order (a block's destination never passes
        // its own start, so nothing unread is overwritten), and give back the
        // tail. The final arrays are the scatter buffers themselves, so the
        // build never holds a second copy of the adjacency.
        let new_total = new_offsets[n];
        for w in blocks.windows(2) {
            let (from, to) = (offsets[w[0]], new_offsets[w[0]]);
            let len = new_offsets[w[1]] - to;
            targets.copy_within(from..from + len, to);
            if weights.is_some() {
                wts.copy_within(from..from + len, to);
            }
        }
        targets.truncate(new_total);
        targets.shrink_to_fit();
        wts.truncate(new_total);
        wts.shrink_to_fit();
        drop(offsets);
        let store = counted.unwrap_or(Weights::Floats(wts));
        let mut graph = Graph::finalize(new_offsets, targets, store, self_loop, false);
        graph.exact = graph.is_exact();
        graph
    }

    /// Whether every weight and self-loop is a non-negative integer and `m2 <
    /// 2^52`: then each sum `aggregate` forms is of multiples of ½ below
    /// `2^52`, all exact, so its order cannot change a bit. An unweighted
    /// graph always is (its weights count parallel edges).
    fn is_exact(&self) -> bool {
        let whole = |x: &f64| *x >= 0.0 && x.fract() == 0.0;
        self.m2 < (1u64 << 52) as f64
            && match &self.weights {
                Weights::Counts(_) => true,
                Weights::Floats(ws) => ws.par_iter().all(whole),
            }
            && self.self_loop.par_iter().all(whole)
    }

    /// Collapse each community into a super-node.
    ///
    /// Each community's members are visited in ascending node order, summing edge
    /// weights into a dense per-community accumulator; its neighbours come out
    /// sorted by community id. Every float sum therefore runs in a fixed order.
    /// Communities are independent, so contiguous blocks of them are built in
    /// parallel, each worker with one scratch accumulator reused across its
    /// communities.
    ///
    /// Memory (#359): this runs while `self` is still held, so it is sized
    /// exactly rather than grown. A counting pass finds each community's number
    /// of distinct neighbouring communities (and its self-loop), which gives
    /// the final offsets; the adjacency is then written straight into its final
    /// arrays, allocated once at their exact size, with no per-block buffers
    /// and no concatenation. The counting pass costs a second scan of `self`.
    fn aggregate(&self, comm: &[u32], c: usize) -> Graph {
        self.collapse(comm, c, |v| comm[v as usize])
    }

    /// [`Graph::aggregate`], consuming the graph so that, when it is
    /// [`exact`](Graph::exact), most of it is given back before the
    /// aggregated graph is allocated (#359).
    ///
    /// Each node's segment is first collapsed in place: its neighbours are
    /// relabelled to their communities, edges inside its own community are
    /// folded into its self-loop, and the rest are sorted and coalesced, one
    /// entry per neighbouring community, written over the segment's front.
    /// The segments are then moved down (as in `from_topology`'s pass 4) and
    /// the arrays shrunk, so only one entry per (node, neighbouring
    /// community) is held while the aggregated graph is built from them (one
    /// per 255 of weight, for one-byte counts: see [`fold_segments`]). The
    /// sums are grouped differently (per node first), which changes nothing
    /// when every one of them is exact; an inexact graph takes the plain
    /// path, which sums in the one fixed order.
    fn aggregate_owned(mut self, comm: &[u32], c: usize) -> Graph {
        if !self.exact {
            // Only the degrees can go first: aggregating never reads them.
            self.k = Vec::new();
            return self.aggregate(comm, c);
        }
        let Graph {
            offsets,
            mut targets,
            mut weights,
            mut self_loop,
            k,
            ..
        } = self;
        drop(k);
        let new_offsets = by_weights!(&mut weights, ws => {
            fold_segments(&offsets, &mut targets, ws, &mut self_loop, comm)
        });
        drop(offsets);
        // Neighbours are communities now, and no edge is inside one. (With
        // one-byte counts a community can be listed more than once; see
        // `fold_segments`.)
        let by_node = Graph {
            offsets: new_offsets,
            targets,
            weights,
            self_loop,
            k: Vec::new(),
            m2: 0.0,
            exact: true,
        };
        by_node.collapse(comm, c, |cv| cv)
    }

    /// The aggregation behind [`Graph::aggregate`]: node `u` joins community
    /// `comm[u]`, and a neighbour `v` counts toward community `label(v)`. The
    /// aggregated graph always stores `f64` weights.
    fn collapse(&self, comm: &[u32], c: usize, label: impl Fn(u32) -> u32 + Sync) -> Graph {
        by_weights!(&self.weights, ws => self.collapse_in(ws, comm, c, label))
    }

    /// [`Graph::collapse`] over this graph's weights `store`, one copy per weight
    /// type.
    fn collapse_in<W: Weight>(
        &self,
        store: &[W],
        comm: &[u32],
        c: usize,
        label: impl Fn(u32) -> u32 + Sync,
    ) -> Graph {
        let n = self.n();
        // Members of each community, ascending, via a counting sort on `comm`.
        let mut start = vec![0usize; c + 1];
        for &cu in comm {
            start[cu as usize + 1] += 1;
        }
        for i in 0..c {
            start[i + 1] += start[i];
        }
        let mut members = vec![0u32; n];
        let mut cursor = start[..c].to_vec();
        for (u, &cu) in comm.iter().enumerate() {
            members[cursor[cu as usize]] = u as u32;
            cursor[cu as usize] += 1;
        }
        drop(cursor);
        // Contiguous community blocks of roughly equal member count; the dense
        // scratch is per worker, not per block. Only the work split depends on
        // this: every community's result is computed on its own.
        let bounds = node_blocks(&start, 4 * current_num_threads());
        let blocks: Vec<(usize, usize)> = bounds.windows(2).map(|w| (w[0], w[1])).collect();

        // Pass 1: count each community's distinct neighbouring communities, and
        // sum its self-loop (in the same member and adjacency order as ever).
        let counted: Vec<(Vec<usize>, Vec<f64>)> = blocks
            .par_iter()
            .map_init(
                || (vec![false; c], Vec::<u32>::new()),
                |(seen, touched), &(lo, hi)| {
                    let mut lens = Vec::with_capacity(hi - lo);
                    let mut loops = Vec::with_capacity(hi - lo);
                    for cu in lo..hi {
                        let mut self_loop = 0.0f64;
                        for &u in &members[start[cu]..start[cu + 1]] {
                            self_loop += self.self_loop[u as usize];
                            let (nbrs, wts) = self.neighbors(store, u);
                            for (&v, &wt) in nbrs.iter().zip(wts) {
                                let cv = label(v) as usize;
                                if cv == cu {
                                    // Each intra-community edge is seen from both
                                    // endpoints, so halve to land one loop-weight
                                    // per undirected edge.
                                    self_loop += wt.get() / 2.0;
                                } else if !std::mem::replace(&mut seen[cv], true) {
                                    touched.push(cv as u32);
                                }
                            }
                        }
                        for &cv in touched.iter() {
                            seen[cv as usize] = false;
                        }
                        lens.push(touched.len());
                        touched.clear();
                        loops.push(self_loop);
                    }
                    (lens, loops)
                },
            )
            .collect();
        let mut offsets = Vec::with_capacity(c + 1);
        offsets.push(0usize);
        let mut self_loop = Vec::with_capacity(c);
        for (lens, loops) in counted {
            for len in lens {
                offsets.push(offsets.last().unwrap() + len);
            }
            self_loop.extend(loops);
        }

        // Pass 2: sum each community's edge weights per neighbouring community
        // and write them, sorted by community id, into its exact place.
        let total = offsets[c];
        let mut targets = vec![0u32; total];
        let mut weights = vec![0.0f64; total];
        split_blocks(&mut targets, &offsets, &bounds)
            .into_par_iter()
            .zip(split_blocks(&mut weights, &offsets, &bounds))
            .zip(blocks.par_iter())
            .for_each_init(
                || (vec![0.0f64; c], vec![false; c], Vec::<u32>::new()),
                |(acc, seen, touched), ((ts, ws), &(lo, hi))| {
                    let mut k = 0;
                    for cu in lo..hi {
                        for &u in &members[start[cu]..start[cu + 1]] {
                            let (nbrs, wts) = self.neighbors(store, u);
                            for (&v, &wt) in nbrs.iter().zip(wts) {
                                let cv = label(v) as usize;
                                if cv != cu {
                                    if !std::mem::replace(&mut seen[cv], true) {
                                        touched.push(cv as u32);
                                    }
                                    acc[cv] += wt.get();
                                }
                            }
                        }
                        touched.sort_unstable();
                        for &cv in touched.iter() {
                            seen[cv as usize] = false;
                            ts[k] = cv;
                            ws[k] = std::mem::take(&mut acc[cv as usize]);
                            k += 1;
                        }
                        touched.clear();
                    }
                    debug_assert_eq!(k, ts.len());
                },
            );
        let weights = Weights::Floats(weights);
        Graph::finalize(offsets, targets, weights, self_loop, self.exact)
    }

    fn finalize(
        offsets: Vec<usize>,
        targets: Vec<u32>,
        weights: Weights,
        self_loop: Vec<f64>,
        exact: bool,
    ) -> Graph {
        let k: Vec<f64> = by_weights!(&weights, ws => {
            (0..self_loop.len())
                .into_par_iter()
                .map(|i| {
                    let seg = &ws[offsets[i]..offsets[i + 1]];
                    seg.iter().map(|w| w.get()).sum::<f64>() + 2.0 * self_loop[i]
                })
                .collect()
        });
        let m2 = k.iter().sum();
        Graph {
            offsets,
            targets,
            weights,
            self_loop,
            k,
            m2,
            exact,
        }
    }
}

/// Relabel community ids (each `< comm.len()`) to a contiguous `0..k` by first
/// appearance.
fn renumber(comm: &[u32]) -> Vec<u32> {
    let mut map = vec![u32::MAX; comm.len()];
    let mut next = 0u32;
    comm.iter()
        .map(|&c| {
            let slot = &mut map[c as usize];
            if *slot == u32::MAX {
                *slot = next;
                next += 1;
            }
            *slot
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compact run (#326) gives the full run's labels, values and order:
    /// unweighted and weighted, masked and not, over several seeds.
    #[test]
    fn compact_runs_give_the_full_labels() {
        use crate::algo::linked::testing::{sparse, weights, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m);
            let w = weights(seed, m);
            for mask in [None, Some(&mask)] {
                let linked = Linked::of(&t, mask);
                if linked.len() == 0 || linked.n_isolated() == 0 {
                    continue;
                }
                // Levels that end with the queue, and levels stopped mid-way
                // by the gain threshold or the work budget, whose stopping
                // point counts the isolated nodes' visits. Then parallel
                // levels (#387), whole and stopped after one or two sweeps.
                let short = |threshold, max_sweeps, sweep| Pace {
                    threshold,
                    max_sweeps,
                    sweep: Some(sweep),
                    parallel: false,
                };
                let par = |threshold, max_sweeps| Pace {
                    threshold,
                    max_sweeps,
                    sweep: None,
                    parallel: true,
                };
                let paces = [
                    PACE,
                    par(SWEEP_GAIN_THRESHOLD, MAX_SWEEPS),
                    short(1e-2, 64, n / 3 + 1),
                    short(-1.0, 2, n / 2 + 3),
                    short(-1.0, 3, 7),
                    par(1e-2, 64),
                    par(-1.0, 1),
                    par(-1.0, 2),
                ];
                for s in [None, Some(1), Some(77)] {
                    for (ws, res) in [(None, 1.0), (Some(w.as_slice()), 1.0), (None, 0.3)] {
                        for pace in paces.into_iter().take(if n > 5000 { 2 } else { 8 }) {
                            let full = louvain_over(&t, ws, mask, res, s, None, pace);
                            let compact =
                                louvain_over(&t, ws, mask, res, s, Some(linked.clone()), pace);
                            assert_eq!(
                                compact,
                                full,
                                "graph {seed}, masked {}, seed {s:?}, weighted {}, {pace:?}",
                                mask.is_some(),
                                ws.is_some()
                            );
                        }
                    }
                }
            }
        }
    }

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
    fn weighted_runs_are_reproducible_with_inexact_weights() {
        // Irrational-ish weights make float sums order-sensitive, so this pins the
        // aggregation order: every run must agree exactly.
        let n = 600u32;
        let (mut src, mut dst, mut w) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..6000u32 {
            let a = (i * 7) % n;
            let b = (a + 1 + (i * 13) % 40) % n; // local structure -> real communities
            src.push(a);
            dst.push(b);
            w.push(((i % 97) as f64 + 1.0).sqrt() / 3.0);
        }
        let t = Topology::build(n as usize, src, dst);
        let first = louvain_weighted(&t, &w, None, 1.0, Some(5), false);
        assert!(
            first.iter().max().unwrap() > &0,
            "found more than one community"
        );
        for _ in 0..5 {
            assert_eq!(louvain_weighted(&t, &w, None, 1.0, Some(5), false), first);
        }
    }

    #[test]
    fn recovers_two_communities() {
        let t = two_cliques();
        let comm = louvain(&t, None, 1.0, Some(1), false);
        assert!(comm[0] == comm[1] && comm[1] == comm[2] && comm[2] == comm[3]);
        assert!(comm[4] == comm[5] && comm[5] == comm[6] && comm[6] == comm[7]);
        assert_ne!(comm[0], comm[4]);
        // Contiguous labels: exactly two communities.
        let max = *comm.iter().max().unwrap();
        assert_eq!(max, 1);
    }

    #[test]
    fn deterministic_given_a_seed() {
        let t = two_cliques();
        assert_eq!(
            louvain(&t, None, 1.0, Some(5), false),
            louvain(&t, None, 1.0, Some(5), false)
        );
    }

    #[test]
    fn no_edges_is_all_singletons() {
        let t = Topology::build(3, vec![], vec![]);
        assert_eq!(louvain(&t, None, 1.0, None, false), vec![0, 1, 2]);
    }

    #[test]
    fn empty_graph_is_empty() {
        let t = Topology::build(0, vec![], vec![]);
        assert!(louvain(&t, None, 1.0, None, false).is_empty());
    }

    #[test]
    fn weighted_uniform_matches_unweighted() {
        // Unit weights on every edge reproduce the unweighted partition.
        let t = two_cliques();
        let ones = vec![1.0; t.n_edges()];
        assert_eq!(
            louvain_weighted(&t, &ones, None, 1.0, Some(1), false),
            louvain(&t, None, 1.0, Some(1), false)
        );
    }

    #[test]
    fn weighted_can_reassign_the_bridge_endpoints() {
        // Two triangles {0,1,2} and {3,4,5} joined by 2-3. With the bridge weighted
        // far heavier than the intra-triangle edges, 2 and 3 bind together.
        // edges: 0-1,1-2,2-0 (triangle A), 3-4,4-5,5-3 (triangle B), 2-3 (bridge)
        let src = vec![0, 1, 2, 3, 4, 5, 2];
        let dst = vec![1, 2, 0, 4, 5, 3, 3];
        let t = Topology::build(6, src, dst);
        let heavy_bridge = vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 50.0];
        let comm = louvain_weighted(&t, &heavy_bridge, None, 1.0, Some(1), false);
        assert_eq!(comm[2], comm[3], "a heavy bridge binds its endpoints");
    }

    /// A deterministic pseudo-random graph with inexact weights (an LCG, so no RNG
    /// dependency): enough edges to take `from_topology`'s parallel path, with
    /// parallel edges, opposite edges and self-loops.
    #[cfg(feature = "rayon")]
    fn random_weighted(n: u32, m: usize) -> (Topology, Vec<f64>) {
        let (src, dst, w) = random_edges(n, m);
        (Topology::build(n as usize, src, dst), w)
    }

    /// Every array of a graph, floats as bits.
    type Bits = (Vec<usize>, Vec<u32>, Vec<u64>, Vec<u64>, Vec<u64>, u64);
    fn graph_bits(g: &Graph) -> Bits {
        let b = |xs: &[f64]| xs.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        (
            g.offsets.clone(),
            g.targets.clone(),
            b(&g.weight_values()),
            b(&g.self_loop),
            b(&g.k),
            g.m2.to_bits(),
        )
    }

    /// The consuming aggregation (#359) collapses each node's segment in place
    /// before aggregating, which regroups the sums: for an exact graph
    /// (integer weights) it must give the plain aggregation's graph bit for
    /// bit, over Louvain's own partition and over coarse and fine ones, at
    /// every level; an inexact graph must take the plain path.
    #[test]
    fn aggregating_in_place_is_exact() {
        let n = 5000u32;
        let (src, dst, w) = random_edges(n, 120_000);
        let t = Topology::build(n as usize, src.clone(), dst.clone());
        let whole: Vec<f64> = w.iter().map(|x| (x * 7.0).round()).collect();
        let keep: Vec<bool> = (0..src.len()).map(|e| e % 5 != 2).collect();
        let mask = EdgeMask::from_bools(&keep);
        assert!(!Graph::from_topology(&t, Some(&w), None, None).exact);
        for weights in [None, Some(whole.as_slice())] {
            for mask in [None, Some(&mask)] {
                let build = || Graph::from_topology(&t, weights, mask, None);
                assert!(build().exact);
                let louvain = one_level(&build(), 1.0, 1, None, PACE);
                let partitions: [Vec<u32>; 3] = [
                    louvain,
                    (0..n).map(|u| u / 7).collect(),
                    (0..n).map(|u| u % 3).collect(),
                ];
                for comm in partitions {
                    let c = *comm.iter().max().unwrap() as usize + 1;
                    let want = build().aggregate(&comm, c);
                    let got = build().aggregate_owned(&comm, c);
                    assert!(got.exact);
                    assert!(graph_bits(&got) == graph_bits(&want));
                    // The next level down, too.
                    let next = one_level(&want, 1.0, 1, None, PACE);
                    let c2 = *next.iter().max().unwrap() as usize + 1;
                    let want2 = want.aggregate(&next, c2);
                    assert!(graph_bits(&got.aggregate_owned(&next, c2)) == graph_bits(&want2));
                }
            }
        }
    }

    /// An unweighted level 0 stores its parallel-edge counts in one byte, and
    /// falls back to `f64`s when a count passes 255 (#450); either way the
    /// run is the one unit weights give.
    #[test]
    fn unweighted_counts_take_one_byte() {
        let is_counts = |t: &Topology| {
            let g = Graph::from_topology(t, None, None, None);
            matches!(g.weights, Weights::Counts(_))
        };
        assert!(is_counts(&two_cliques()));
        // A ring of 40 nodes, with `extra` more edges between nodes 0 and 1,
        // some of them reversed.
        let ring = |extra: u32| {
            let (mut src, mut dst): (Vec<u32>, Vec<u32>) = ((0..40).collect(), Vec::new());
            dst.extend((1..40).chain([0]));
            for i in 0..extra {
                src.push(i % 2);
                dst.push(1 - i % 2);
            }
            Topology::build(40, src, dst)
        };
        for (extra, counts) in [(254, true), (255, false), (600, false)] {
            let t = ring(extra);
            assert_eq!(is_counts(&t), counts, "{extra} parallel edges");
            let ones = vec![1.0; t.n_edges()];
            for parallel in [false, true] {
                assert_eq!(
                    louvain(&t, None, 1.0, Some(3), parallel),
                    louvain_weighted(&t, &ones, None, 1.0, Some(3), parallel)
                );
            }
        }
    }

    /// Aggregating one-byte counts in place: a node with more than 255 edges
    /// into one community lists it more than once (#450), and the aggregated
    /// graph is still the plain aggregation's, bit for bit, as is the run.
    #[test]
    fn aggregating_counts_past_a_byte_is_exact() {
        // A hub joined to 1,000 nodes (to some twice), which form a path.
        let n = 1001u32;
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for i in 1..n {
            src.push(0);
            dst.push(i);
            if i % 7 == 0 {
                src.push(i);
                dst.push(0);
            }
            if i + 1 < n {
                src.push(i);
                dst.push(i + 1);
            }
        }
        let t = Topology::build(n as usize, src, dst);
        let build = || Graph::from_topology(&t, None, None, None);
        assert!(matches!(build().weights, Weights::Counts(_)));
        let partitions: [Vec<u32>; 3] = [
            (0..n).map(|u| u32::from(u > 0)).collect(),
            (0..n).map(|u| u32::from(u > 0) * (1 + u % 2)).collect(),
            one_level(&build(), 1.0, 1, None, PACE),
        ];
        for comm in partitions {
            let c = *comm.iter().max().unwrap() as usize + 1;
            let want = build().aggregate(&comm, c);
            let got = build().aggregate_owned(&comm, c);
            assert!(matches!(got.weights, Weights::Floats(_)));
            assert!(graph_bits(&got) == graph_bits(&want));
        }
        let ones = vec![1.0; t.n_edges()];
        for parallel in [false, true] {
            assert_eq!(
                louvain(&t, None, 1.0, Some(2), parallel),
                louvain_weighted(&t, &ones, None, 1.0, Some(2), parallel)
            );
        }
    }

    /// The edge list behind [`random_weighted`]: sources, targets, weights.
    fn random_edges(n: u32, m: usize) -> (Vec<u32>, Vec<u32>, Vec<f64>) {
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let (mut src, mut dst, mut w) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..m {
            let a = next() % n;
            // Mostly local edges (real communities), some long-range.
            let b = if i % 4 == 0 {
                next() % n
            } else {
                (a + next() % 30) % n
            };
            src.push(a);
            dst.push(b);
            w.push(((i % 89) as f64 + 1.0).sqrt() / 7.0);
        }
        (src, dst, w)
    }

    /// Modularity (γ = 1) of `comm` over the symmetrised edge list, computed
    /// from the edges themselves, not from Louvain's working graph.
    fn modularity(src: &[u32], dst: &[u32], w: Option<&[f64]>, comm: &[u32]) -> f64 {
        let k = comm.iter().max().map_or(0, |&c| c as usize + 1);
        let (mut tot, mut intra, mut m2) = (vec![0.0f64; k], 0.0f64, 0.0f64);
        for (i, (&s, &d)) in src.iter().zip(dst).enumerate() {
            let wt = w.map_or(1.0, |w| w[i]);
            let (cs, cd) = (comm[s as usize] as usize, comm[d as usize] as usize);
            tot[cs] += wt;
            tot[cd] += wt;
            m2 += 2.0 * wt;
            if cs == cd {
                intra += 2.0 * wt;
            }
        }
        intra / m2 - tot.iter().map(|t| (t / m2) * (t / m2)).sum::<f64>()
    }

    /// FNV-1a over a sequence of words.
    fn fnv(words: impl IntoIterator<Item = u64>) -> u64 {
        words.into_iter().fold(0xcbf2_9ce4_8422_2325, |h, x| {
            (h ^ x).wrapping_mul(0x100_0000_01b3)
        })
    }

    /// Exact results, pinned (#352): the community count, modularity bits and
    /// a hash of the labels for fixed seeds, unweighted and weighted, masked
    /// and not, on a graph big enough for the parallel build; and a hash of
    /// the level-0 working graph. A change to how the working graph is built
    /// or stored must leave every one of these bit for bit as it is.
    #[test]
    fn results_are_pinned() {
        let n = 5000u32;
        let (src, dst, w) = random_edges(n, 120_000);
        let t = Topology::build(n as usize, src.clone(), dst.clone());
        let keep: Vec<bool> = (0..src.len()).map(|e| e % 5 != 2).collect();
        let mask = EdgeMask::from_bools(&keep);
        fn kept<T: Copy>(xs: &[T], keep: &[bool]) -> Vec<T> {
            xs.iter().zip(keep).filter(|p| *p.1).map(|p| *p.0).collect()
        }
        let masked_edges = (kept(&src, &keep), kept(&dst, &keep), kept(&w, &keep));
        let mut got = Vec::new();
        for masked in [false, true] {
            let (s, d, ww) = if masked {
                (&masked_edges.0, &masked_edges.1, &masked_edges.2)
            } else {
                (&src, &dst, &w)
            };
            let mask = masked.then_some(&mask);
            for weighted in [false, true] {
                for seed in [1u64, 9] {
                    let comm = if weighted {
                        louvain_weighted(&t, &w, mask, 1.0, Some(seed), false)
                    } else {
                        louvain(&t, mask, 1.0, Some(seed), false)
                    };
                    let q = modularity(s, d, weighted.then_some(ww.as_slice()), &comm);
                    let k = comm.iter().max().unwrap() + 1;
                    let h = fnv(comm.iter().map(|&c| u64::from(c)));
                    got.push((masked, weighted, seed, k, q.to_bits(), h));
                }
                let g = Graph::from_topology(&t, weighted.then_some(w.as_slice()), mask, None);
                let h = fnv(g
                    .offsets
                    .iter()
                    .map(|&o| o as u64)
                    .chain(g.targets.iter().map(|&v| u64::from(v)))
                    .chain(g.weight_values().iter().map(|x| x.to_bits()))
                    .chain(g.self_loop.iter().map(|x| x.to_bits()))
                    .chain(g.k.iter().map(|x| x.to_bits()))
                    .chain([g.m2.to_bits()]));
                got.push((masked, weighted, 0, 0, 0, h));
            }
        }
        // (masked, weighted, seed, communities, modularity bits, labels hash);
        // seed 0 rows are the level-0 graph's hash.
        let want: &[(bool, bool, u64, u32, u64, u64)] = &[
            (
                false,
                false,
                1,
                20,
                4604229447551481594,
                2248463203008460393,
            ),
            (false, false, 9, 21, 4604228329304253869, 613740474174045293),
            (false, false, 0, 0, 0, 5059561701017657901),
            (false, true, 1, 20, 4604229012191403382, 6018945348175921665),
            (
                false,
                true,
                9,
                21,
                4604228104913520882,
                17737549763898365559,
            ),
            (false, true, 0, 0, 0, 6124457408852198281),
            (
                true,
                false,
                1,
                21,
                4604241415289548926,
                16938039130672030744,
            ),
            (true, false, 9, 21, 4604237435839331312, 9774245703022877507),
            (true, false, 0, 0, 0, 318761951090822375),
            (true, true, 1, 21, 4604240838659378926, 753696038131262450),
            (true, true, 9, 21, 4604232695396470159, 7947206719753406432),
            (true, true, 0, 0, 0, 8722660127737123802),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn recovers_a_ring_of_cliques() {
        // Eight 5-cliques, each joined to the next by one edge: the textbook
        // modularity optimum is one community per clique.
        let (k, size) = (8u32, 5u32);
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for c in 0..k {
            for a in 0..size {
                for b in a + 1..size {
                    src.push(c * size + a);
                    dst.push(c * size + b);
                }
            }
            src.push(c * size);
            dst.push(((c + 1) % k) * size + 1);
        }
        let t = Topology::build((k * size) as usize, src, dst);
        for seed in 0..5 {
            let comm = louvain(&t, None, 1.0, Some(seed), false);
            for u in 0..(k * size) as usize {
                assert_eq!(comm[u], comm[u - u % size as usize], "seed {seed}");
            }
            assert_eq!(*comm.iter().max().unwrap(), k - 1, "seed {seed}");
        }
    }

    #[test]
    fn parallel_recovers_cliques() {
        let t = two_cliques();
        for seed in 0..5 {
            let comm = louvain(&t, None, 1.0, Some(seed), true);
            assert_eq!(comm, vec![0, 0, 0, 0, 1, 1, 1, 1], "seed {seed}");
        }
        // A ring of eight 5-cliques, as above.
        let (k, size) = (8u32, 5u32);
        let (mut src, mut dst) = (Vec::new(), Vec::new());
        for c in 0..k {
            for a in 0..size {
                for b in a + 1..size {
                    src.push(c * size + a);
                    dst.push(c * size + b);
                }
            }
            src.push(c * size);
            dst.push(((c + 1) % k) * size + 1);
        }
        let t = Topology::build((k * size) as usize, src, dst);
        for seed in 0..5 {
            let comm = louvain(&t, None, 1.0, Some(seed), true);
            for u in 0..(k * size) as usize {
                assert_eq!(comm[u], comm[u - u % size as usize], "seed {seed}");
            }
            assert_eq!(*comm.iter().max().unwrap(), k - 1, "seed {seed}");
        }
    }

    /// Two single nodes deciding in the same run must not swap: on many
    /// disjoint edges, every pair of nodes still ends up together, and a
    /// level stops (the work stays bounded) rather than running every sweep.
    #[test]
    fn parallel_single_nodes_do_not_swap() {
        let pairs = 10_000u32;
        let src: Vec<u32> = (0..pairs).map(|i| 2 * i).collect();
        let dst: Vec<u32> = (0..pairs).map(|i| 2 * i + 1).collect();
        let t = Topology::build(2 * pairs as usize, src, dst);
        let comm = louvain(&t, None, 1.0, Some(3), true);
        for i in 0..pairs as usize {
            assert_eq!(comm[2 * i], comm[2 * i + 1]);
        }
        assert_eq!(*comm.iter().max().unwrap(), pairs - 1);
        // One level of local moving: each pair merges in the first sweep, as
        // the guard lets exactly one of its nodes move.
        let g = Graph::from_topology(&t, None, None, None);
        let pace = Pace {
            max_sweeps: 1,
            parallel: true,
            ..PACE
        };
        let level = one_level_parallel(&g, 1.0, 3, None, pace);
        assert_eq!(*level.iter().max().unwrap(), pairs - 1);
    }

    /// The table and the dense array sum the same weights to the same bits,
    /// and leave no trace for the next node.
    #[test]
    fn tally_table_and_dense_agree() {
        let sums = |tally: &mut Tally, n: usize| {
            let mut out = Vec::new();
            for round in 0..3u32 {
                tally.begin(30, n);
                for i in 0..30u32 {
                    let c = (i * 7 + round) % 23;
                    tally.add(c, f64::from(i + 1).sqrt() / 3.0, i);
                }
                tally.touched.sort_unstable_by_key(|&(c, _, _)| c);
                out.push(
                    tally
                        .touched
                        .iter()
                        .map(|&(c, slot, v)| (c, tally.vals[slot as usize].to_bits(), v))
                        .collect::<Vec<_>>(),
                );
                tally.end();
            }
            out
        };
        let (mut table, mut dense) = (Tally::default(), Tally::default());
        let hashed = sums(&mut table, 1 << 20);
        assert_ne!(table.shift, 0, "a table");
        assert_eq!(hashed, sums(&mut dense, 50));
        assert_eq!(dense.shift, 0, "dense");
        assert!(table.keys.iter().chain(&dense.keys).all(|&k| k == EMPTY));
    }

    /// The parallel run's scratch is a hash table for most nodes and a dense
    /// array for a hub; both sum in adjacency order, so a star (one hub) with
    /// inexact weights gives the same result as ever, run after run.
    #[test]
    fn parallel_runs_are_reproducible_with_hubs() {
        let n = 3000u32;
        let (mut src, mut dst, mut w) = (Vec::new(), Vec::new(), Vec::new());
        for i in 1..n {
            src.push(0);
            dst.push(i);
            w.push((f64::from(i % 97) + 1.0).sqrt() / 3.0);
            src.push(i);
            dst.push(1 + (i * 7 + 3) % (n - 1));
            w.push((f64::from(i % 89) + 1.0).sqrt() / 7.0);
        }
        let t = Topology::build(n as usize, src, dst);
        let first = louvain_weighted(&t, &w, None, 1.0, Some(5), true);
        assert!(*first.iter().max().unwrap() > 0);
        for _ in 0..3 {
            assert_eq!(louvain_weighted(&t, &w, None, 1.0, Some(5), true), first);
        }
    }

    #[cfg(feature = "rayon")]
    #[test]
    fn working_graph_is_identical_on_any_thread_count() {
        // The chunked histogram/scatter must hand every segment its entries in row
        // order whatever the chunking, so the coalesced weights match bit for bit.
        let (t, w) = random_weighted(5000, 120_000);
        let build = |threads: usize| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let g = Graph::from_topology(&t, Some(&w), None, None);
                let comm: Vec<u32> = (0..g.n() as u32).map(|u| u / 7).collect();
                let agg = g.aggregate(&comm, g.n().div_ceil(7));
                (g, agg)
            })
        };
        let bits = |g: &Graph| {
            (
                g.offsets.clone(),
                g.targets.clone(),
                g.weight_values()
                    .iter()
                    .map(|x| x.to_bits())
                    .collect::<Vec<_>>(),
                g.self_loop.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                g.k.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                g.m2.to_bits(),
            )
        };
        let (g1, a1) = build(1);
        for threads in [2, 3, 8] {
            let (g, a) = build(threads);
            assert!(
                bits(&g) == bits(&g1),
                "level-0 graph diverged at {threads} threads"
            );
            assert!(
                bits(&a) == bits(&a1),
                "aggregate diverged at {threads} threads"
            );
        }
        // Coalescing: segments are strictly sorted (parallel edges merged).
        for u in 0..g1.n() as u32 {
            let seg = &g1.targets[g1.offsets[u as usize]..g1.offsets[u as usize + 1]];
            assert!(seg.windows(2).all(|p| p[0] < p[1]));
        }
    }
}
