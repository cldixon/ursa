//! Connected components: weak (union-find over the undirected view) and strong
//! (mutual reachability over out-edges).
//!
//! Both kernels have a serial form, which defines the labels, and a parallel
//! form (#383) that returns exactly the same labels, run when there is a pool
//! of threads and enough edges to share. The serial forms are the reference:
//!
//! - **Weak**: union-find with path halving and union by rank, over the edges
//!   in CSR order (`union(u, v)` for each out-edge `u -> v`). A component's
//!   label is its union-find root.
//! - **Strong**: iterative Tarjan (an explicit DFS stack, so deep graphs don't
//!   overflow the native call stack), from each unvisited node in id order. A
//!   component's label is its Tarjan root: the member the DFS reached first.
//!
//! Neither label is the component's smallest id, and both depend on the order
//! of the work, so each parallel form splits the job into a part that runs in
//! parallel and a short serial part that fixes the labels the serial form
//! would give:
//!
//! - **Weak** ([`weak_parallel`]): a union only changes the union-find roots
//!   and ranks when it joins two sets, and which unions do is fixed by the edge
//!   order alone: an edge joins two sets iff no earlier edge already connects
//!   its ends (the edges of the minimum spanning forest, each edge weighted by
//!   its CSR slot). Path halving moves no root. So replaying only those edges,
//!   in order, through the same union-find gives the same roots. Each thread
//!   finds the joining edges of its own range of slots (an edge that joins
//!   nothing within its range joins nothing in the whole graph either);
//!   neighbouring ranges are then merged pairwise, which keeps only edges that
//!   still join, until two remain; the serial replay runs over at most two
//!   spanning forests' worth of edges rather than every edge.
//! - **Strong** ([`strong_parallel`]): the components themselves are found in
//!   parallel, by trimming (a node with no in-edge, or no out-edge, from an
//!   unassigned node is a component of its own) and colouring (each node takes
//!   the smallest id that reaches it; the nodes of a colour that reach back to
//!   the node of that id are its component), round after round. Then a serial
//!   DFS in Tarjan's order, which only tests a visited bit per edge, finds the
//!   member of each component of two or more nodes that it reaches first (a
//!   single node is its own label), and stops once it has found all of them.
//!   The DFS is the part that cannot be parallelised: which member of a
//!   component a depth-first search reaches first is a question about the
//!   whole search's order. A colouring round that would take too many passes
//!   (a long cycle) hands what is left to serial Tarjan, which only finds the
//!   components: the labels still come from the DFS.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

use super::linked::Linked;
use crate::parallel::*;
use crate::topology::{Adjacency, EdgeMask, Topology};

/// The fewest edges a components kernel takes its parallel form for. Below it
/// the fixed costs (a bitset over the edges, per-thread scratch) are not worth
/// it. Results are the same either way.
const PARALLEL_MIN_EDGES: usize = 1 << 16;

/// Whether to take the parallel forms: a pool of more than one thread and
/// enough edges to share. Never without the `rayon` feature.
fn parallel(n_edges: usize) -> bool {
    current_num_threads() > 1 && n_edges >= PARALLEL_MIN_EDGES
}

/// The linked nodes a components kernel runs over, when enough nodes are
/// isolated to be worth it (#326). An isolated node is a component of its own,
/// labelled by itself, in either kernel: with the compact ids in node order,
/// every union (weak) and every discovery-order comparison (strong) is the
/// full run's, so a compact run's labels, mapped back to nodes, are too.
fn linked_nodes(topo: &Topology, mask: Option<&EdgeMask>) -> Option<Linked> {
    let linked = Linked::of(topo, mask);
    linked.worth_it().then_some(linked)
}

struct DisjointSet {
    parent: Vec<u32>,
    rank: Vec<u8>,
}

impl DisjointSet {
    fn new(n: usize) -> Self {
        DisjointSet {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: u32) -> u32 {
        // Path compression by halving.
        while self.parent[x as usize] != x {
            let gp = self.parent[self.parent[x as usize] as usize];
            self.parent[x as usize] = gp;
            x = gp;
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (ra_rank, rb_rank) = (self.rank[ra as usize], self.rank[rb as usize]);
        let (small, big) = if ra_rank < rb_rank {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small as usize] = big;
        if ra_rank == rb_rank {
            self.rank[big as usize] += 1;
        }
    }
}

/// The graph a parallel components kernel walks: the out-CSR under `mask`, in
/// compact ids over `linked` when given.
#[derive(Clone, Copy)]
struct View<'a> {
    out: &'a Adjacency,
    mask: Option<&'a EdgeMask>,
    linked: Option<&'a Linked>,
    /// The node count: compact when `linked` is given.
    n: usize,
}

impl<'a> View<'a> {
    fn new(topo: &'a Topology, mask: Option<&'a EdgeMask>, linked: Option<&'a Linked>) -> Self {
        View {
            out: topo.out(),
            mask,
            linked,
            n: linked.map_or(topo.n_nodes(), Linked::len),
        }
    }

    /// The node of (compact) node `u`.
    #[inline]
    fn node(&self, u: u32) -> u32 {
        self.linked.map_or(u, |l| l.nodes()[u as usize])
    }

    /// The (compact) node of node `v`.
    #[inline]
    fn at(&self, v: u32) -> u32 {
        self.linked.map_or(v, |l| l.compact(v))
    }

    /// The first CSR slot of (compact) node `u`; for `u == n`, the slot count.
    /// The slots of `u` are `slot(u)..slot(u + 1)`: an isolated node between
    /// two compact ones has none.
    #[inline]
    fn slot(&self, u: u32) -> usize {
        if u as usize == self.n {
            *self.out.offsets.last().unwrap_or(&0) as usize
        } else {
            self.out.offsets[self.node(u) as usize] as usize
        }
    }

    /// Calls `f` with each (compact) out-neighbour of `u` along an edge the
    /// mask keeps, self-loops included, in CSR order.
    #[inline]
    fn each(&self, u: u32, mut f: impl FnMut(u32)) {
        let uo = self.node(u);
        let nb = self.out.neighbors(uo);
        match self.mask {
            None => nb.iter().for_each(|&v| f(self.at(v))),
            Some(m) => nb
                .iter()
                .zip(self.out.edge_ids(uo))
                .filter(|&(_, &e)| m.keep(e))
                .for_each(|(&v, _)| f(self.at(v))),
        }
    }

    /// Whether `f` holds for some out-neighbour of `u`, as [`View::each`].
    #[inline]
    fn any(&self, u: u32, mut f: impl FnMut(u32) -> bool) -> bool {
        let uo = self.node(u);
        let nb = self.out.neighbors(uo);
        match self.mask {
            None => nb.iter().any(|&v| f(self.at(v))),
            Some(m) => nb
                .iter()
                .zip(self.out.edge_ids(uo))
                .any(|(&v, &e)| m.keep(e) && f(self.at(v))),
        }
    }
}

/// A bitset shared between threads.
struct Bits(Vec<AtomicU64>);

impl Bits {
    fn new(n: usize) -> Self {
        Bits((0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect())
    }

    #[inline]
    fn get(&self, i: usize) -> bool {
        self.0[i >> 6].load(Relaxed) >> (i & 63) & 1 == 1
    }

    /// Sets bit `i`; whether it was set before.
    #[inline]
    fn set(&self, i: usize) -> bool {
        let bit = 1u64 << (i & 63);
        self.0[i >> 6].fetch_or(bit, Relaxed) & bit != 0
    }

    #[inline]
    fn clear(&self, i: usize) {
        self.0[i >> 6].fetch_and(!(1u64 << (i & 63)), Relaxed);
    }
}

/// Weak connected component label per node, dense-indexed. Labels are the dense
/// index of each component's representative (not necessarily contiguous `0..k`);
/// callers that want canonical small ids can densify the output.
pub fn connected_components_weak(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<u32> {
    let linked = linked_nodes(topo, mask);
    if parallel(topo.n_edges()) {
        weak_parallel(topo, mask, linked.as_ref())
    } else {
        weak_over(topo, mask, linked.as_ref())
    }
}

/// [`connected_components_weak`], serially, compact over `linked` when given.
fn weak_over(topo: &Topology, mask: Option<&EdgeMask>, linked: Option<&Linked>) -> Vec<u32> {
    let width = linked.map_or(topo.n_nodes(), Linked::len);
    let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
    let at = |v: u32| linked.map_or(v, |l| l.compact(v));
    let mut ds = DisjointSet::new(width);
    let out = topo.out();
    for u in 0..width as u32 {
        let uo = node(u);
        match mask {
            None => {
                for &v in out.neighbors(uo) {
                    ds.union(u, at(v));
                }
            }
            // Subgraph: only kept edges connect their endpoints.
            Some(m) => {
                for (&v, &e) in out.neighbors(uo).iter().zip(out.edge_ids(uo)) {
                    if m.keep(e) {
                        ds.union(u, at(v));
                    }
                }
            }
        }
    }
    let labels: Vec<u32> = (0..width as u32).map(|u| ds.find(u)).collect();
    match linked {
        None => labels,
        Some(l) => l.expand_labels(&labels),
    }
}

/// Union-find scratch for one thread: which edges of a range join two sets.
/// Any union rule finds the same edges, so this one keeps no ranks: it links
/// the larger root under the smaller (in a power-law graph the hubs tend to
/// have small ids, so the trees stay shallow). It records the roots it links,
/// so that [`Forest::reset`] restores identity in time proportional to the
/// work done, not to the node count.
struct Forest {
    parent: Vec<u32>,
    linked: Vec<u32>,
}

impl Forest {
    fn root(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let gp = self.parent[self.parent[x as usize] as usize];
            self.parent[x as usize] = gp;
            x = gp;
        }
        x
    }

    /// Joins the sets of `a` and `b`; whether they were apart.
    #[inline]
    fn join(&mut self, a: u32, b: u32) -> bool {
        let (ra, rb) = (self.root(a), self.root(b));
        if ra == rb {
            return false;
        }
        let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
        self.parent[hi as usize] = lo;
        self.linked.push(hi);
        true
    }

    /// Back to every node its own set. Only a linked root's parent ever
    /// leaves itself (path halving rewrites only non-roots).
    fn reset(&mut self) {
        for &x in &self.linked {
            self.parent[x as usize] = x;
        }
        self.linked.clear();
    }
}

/// [`Forest`]s to reuse across tasks: at most one per thread is ever made.
struct Pool {
    n: usize,
    free: Mutex<Vec<Forest>>,
}

impl Pool {
    fn take(&self) -> Forest {
        let spare = self.free.lock().unwrap_or_else(|e| e.into_inner()).pop();
        spare.unwrap_or_else(|| Forest {
            parent: (0..self.n as u32).collect(),
            linked: Vec::new(),
        })
    }

    fn give(&self, mut f: Forest) {
        f.reset();
        self.free.lock().unwrap_or_else(|e| e.into_inner()).push(f);
    }
}

/// Calls `f(u, slot, v)` for each candidate edge (a set bit of `cand`) of the
/// (compact) sources `a..b`, in CSR order, `v` its (compact) target.
fn each_candidate(view: &View, cand: &Bits, a: u32, b: u32, mut f: impl FnMut(u32, usize, u32)) {
    let (lo, hi) = (view.slot(a), view.slot(b));
    if lo >= hi {
        return;
    }
    let mut u = a;
    for w in lo >> 6..=(hi - 1) >> 6 {
        let mut bits = cand.0[w].load(Relaxed);
        // Only this range's bits: a word at either end is shared.
        if w == lo >> 6 {
            bits &= !0u64 << (lo & 63);
        }
        if w == (hi - 1) >> 6 && hi & 63 != 0 {
            bits &= (1u64 << (hi & 63)) - 1;
        }
        while bits != 0 {
            let p = (w << 6) | bits.trailing_zeros() as usize;
            bits &= bits - 1;
            while view.slot(u + 1) <= p {
                u += 1;
            }
            f(u, p, view.at(view.out.targets[p]));
        }
    }
}

/// [`connected_components_weak`] in parallel, compact over `linked` when
/// given: the same labels as [`weak_over`] (see the module docs for why).
fn weak_parallel(topo: &Topology, mask: Option<&EdgeMask>, linked: Option<&Linked>) -> Vec<u32> {
    let view = View::new(topo, mask, linked);
    let n = view.n;
    let m = view.slot(n as u32);
    // One range of sources per thread, of about equal edge counts.
    let parts = current_num_threads().max(2);
    let mut bounds: Vec<u32> = (0..=parts)
        .map(|i| {
            let target = m / parts * i + (m % parts) * i / parts;
            // The first source whose slots start at or after `target`.
            let (mut lo, mut hi) = (0u32, n as u32);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if view.slot(mid) < target {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo
        })
        .collect();
    bounds[parts] = n as u32;
    bounds.dedup();
    let mut ranges: Vec<(u32, u32)> = bounds.windows(2).map(|w| (w[0], w[1])).collect();

    // Each range's own joining edges, marked by slot.
    let cand = Bits::new(m);
    let pool = Pool {
        n,
        free: Mutex::new(Vec::new()),
    };
    ranges.par_iter().for_each(|&(a, b)| {
        let mut f = pool.take();
        for u in a..b {
            let base = view.slot(u);
            let mut k = 0usize;
            // Masked-out edges keep their slot, so count every slot.
            let uo = view.node(u);
            let nb = view.out.neighbors(uo);
            match mask {
                None => {
                    for &v in nb {
                        if f.join(u, view.at(v)) {
                            cand.set(base + k);
                        }
                        k += 1;
                    }
                }
                Some(mk) => {
                    for (&v, &e) in nb.iter().zip(view.out.edge_ids(uo)) {
                        if mk.keep(e) && f.join(u, view.at(v)) {
                            cand.set(base + k);
                        }
                        k += 1;
                    }
                }
            }
        }
        pool.give(f);
    });

    // Merge neighbouring ranges pairwise: of the two ranges' joining edges, in
    // order, keep those that still join.
    while ranges.len() > 2 {
        let pairs: Vec<&[(u32, u32)]> = ranges.chunks(2).collect();
        pairs.par_iter().filter(|p| p.len() == 2).for_each(|p| {
            let mut f = pool.take();
            each_candidate(&view, &cand, p[0].0, p[1].1, |u, slot, v| {
                if !f.join(u, v) {
                    cand.clear(slot);
                }
            });
            pool.give(f);
        });
        ranges = pairs.iter().map(|p| (p[0].0, p[p.len() - 1].1)).collect();
    }
    drop(pool);

    // The serial replay: the serial kernel's union-find, over the edges that
    // can still join, in the same order.
    let mut ds = DisjointSet::new(n);
    each_candidate(&view, &cand, 0, n as u32, |u, _, v| ds.union(u, v));
    drop(cand);
    // Each node's root, read in parallel without compressing paths.
    let parent = &ds.parent;
    let labels: Vec<u32> = (0..n as u32)
        .into_par_iter()
        .map(|mut u| {
            while parent[u as usize] != u {
                u = parent[u as usize];
            }
            u
        })
        .collect();
    match linked {
        None => labels,
        Some(l) => l.expand_labels(&labels),
    }
}

/// Strongly connected component label per node, dense-indexed. Two nodes share
/// a label iff each is reachable from the other. Follows out-edges only — the
/// transpose is not needed. Labels are the dense index of each SCC's Tarjan
/// root (the member an iterative Tarjan from each unvisited node in id order
/// reaches first): an arbitrary but stable representative, matching the
/// weak-component labeling convention. `O(n + m)` serially; the parallel form
/// returns the same labels.
pub fn connected_components_strong(topo: &Topology, mask: Option<&EdgeMask>) -> Vec<u32> {
    let linked = linked_nodes(topo, mask);
    if parallel(topo.n_edges()) {
        strong_parallel(topo, mask, linked.as_ref())
    } else {
        strong_over(topo, mask, linked.as_ref())
    }
}

/// [`connected_components_strong`], serially, compact over `linked` when given.
fn strong_over(topo: &Topology, mask: Option<&EdgeMask>, linked: Option<&Linked>) -> Vec<u32> {
    // Discovery indices are `< n`, which fits `u32` (dense ids are `u32`), so the
    // max value is free to mark an unvisited node.
    const UNVISITED: u32 = u32::MAX;

    let n = linked.map_or(topo.n_nodes(), Linked::len);
    let node = |u: u32| linked.map_or(u, |l| l.nodes()[u as usize]);
    let at = |v: u32| linked.map_or(v, |l| l.compact(v));
    let out = topo.out();

    let mut index = vec![UNVISITED; n]; // discovery order per node
    let mut lowlink = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut labels = vec![0u32; n];
    let mut scc_stack: Vec<u32> = Vec::new();
    let mut next_index: u32 = 0;

    // Explicit DFS: each frame is (node, cursor into its out-neighbours). Replaces
    // recursion so a long path can't blow the native stack. A cursor fits `u32`
    // because a node's degree is at most the edge count, capped at `u32::MAX`.
    let mut call_stack: Vec<(u32, u32)> = Vec::new();

    for start in 0..n as u32 {
        if index[start as usize] != UNVISITED {
            continue;
        }
        call_stack.push((start, 0));
        while let Some(&(v, cursor)) = call_stack.last() {
            if cursor == 0 {
                // First time we reach v: stamp its index/lowlink, push to SCC stack.
                index[v as usize] = next_index;
                lowlink[v as usize] = next_index;
                next_index += 1;
                scc_stack.push(v);
                on_stack[v as usize] = true;
            }
            let neighbors = out.neighbors(node(v));
            if (cursor as usize) < neighbors.len() {
                call_stack.last_mut().unwrap().1 = cursor + 1;
                // Subgraph: an edge masked out is not traversed (advance past it).
                if mask.is_some_and(|m| !m.keep(out.edge_ids(node(v))[cursor as usize])) {
                    continue;
                }
                let w = at(neighbors[cursor as usize]);
                if index[w as usize] == UNVISITED {
                    call_stack.push((w, 0)); // descend into w
                } else if on_stack[w as usize] {
                    // Edge to a node still on the SCC stack: relax v's lowlink.
                    lowlink[v as usize] = lowlink[v as usize].min(index[w as usize]);
                }
            } else {
                // v's out-edges are exhausted; it roots an SCC iff lowlink == index.
                if lowlink[v as usize] == index[v as usize] {
                    loop {
                        let w = scc_stack.pop().expect("scc stack non-empty at a root");
                        on_stack[w as usize] = false;
                        labels[w as usize] = v; // representative = the root v
                        if w == v {
                            break;
                        }
                    }
                }
                call_stack.pop();
                // Propagate v's lowlink up the DFS-tree edge to its parent.
                if let Some(&(parent, _)) = call_stack.last() {
                    lowlink[parent as usize] = lowlink[parent as usize].min(lowlink[v as usize]);
                }
            }
        }
    }
    match linked {
        None => labels,
        Some(l) => l.expand_labels(&labels),
    }
}

/// No component yet, or no colour.
const NONE: u32 = u32::MAX;

/// A frontier shorter than this is propagated serially, until it grows past
/// [`FRONTIER_GROWN`]: a long chain then costs no pool round-trip per step.
/// (The tests use small thresholds, so that small graphs take every path.)
const FRONTIER_SERIAL: usize = if cfg!(test) { 8 } else { 1 << 10 };
const FRONTIER_GROWN: usize = if cfg!(test) { 64 } else { 1 << 14 };

/// Colouring rounds, and backward passes within a round, before what is left
/// goes to serial Tarjan: a long cycle needs a pass per step around it.
const MAX_ROUNDS: usize = 64;
const MAX_PASSES: usize = 64;

/// Nodes per task of a colouring sweep.
const SWEEP_CHUNK: usize = if cfg!(test) { 64 } else { 1 << 12 };

/// Fewer unassigned nodes than this go straight to serial Tarjan.
const SERIAL_REST: usize = if cfg!(test) { 16 } else { 1 << 12 };

/// [`connected_components_strong`] in parallel, compact over `linked` when
/// given: the same labels as [`strong_over`] (see the module docs for how).
fn strong_parallel(topo: &Topology, mask: Option<&EdgeMask>, linked: Option<&Linked>) -> Vec<u32> {
    let view = View::new(topo, mask, linked);
    let scc = strong_partition(&view);
    let labels = tarjan_roots(&view, scc);
    match linked {
        None => labels,
        Some(l) => l.expand_labels(&labels),
    }
}

/// The strong components, in parallel: each node's component, named by one of
/// its members (which one is not Tarjan's choice; [`tarjan_roots`] makes it).
fn strong_partition(view: &View) -> Vec<u32> {
    let n = view.n;
    let scc: Vec<AtomicU32> = (0..n)
        .into_par_iter()
        .map(|_| AtomicU32::new(NONE))
        .collect();
    let open = |v: u32| scc[v as usize].load(Relaxed) == NONE;

    // In-degree from unassigned nodes, self-loops aside (a self-loop puts no
    // second node in a component).
    let indeg: Vec<AtomicU32> = (0..n).into_par_iter().map(|_| AtomicU32::new(0)).collect();
    (0..n as u32).into_par_iter().for_each(|u| {
        view.each(u, |v| {
            if v != u {
                indeg[v as usize].fetch_add(1, Relaxed);
            }
        })
    });

    // `batch` has just been assigned: take its edges out of the in-degrees,
    // and assign each node left with none to a component of its own (nothing
    // unassigned reaches it, so nothing it reaches can reach back), until no
    // more are.
    let settle = |mut batch: Vec<u32>| {
        while !batch.is_empty() {
            let next: Vec<u32> = batch
                .par_iter()
                .flat_map(|&u| {
                    let mut freed = Vec::new();
                    view.each(u, |v| {
                        if v != u && open(v) && indeg[v as usize].fetch_sub(1, Relaxed) == 1 {
                            freed.push(v);
                        }
                    });
                    freed
                })
                .collect();
            next.par_iter()
                .for_each(|&v| scc[v as usize].store(v, Relaxed));
            batch = next;
        }
    };
    let sources: Vec<u32> = (0..n as u32)
        .into_par_iter()
        .filter(|&v| indeg[v as usize].load(Relaxed) == 0)
        .collect();
    sources
        .par_iter()
        .for_each(|&v| scc[v as usize].store(v, Relaxed));
    settle(sources);
    let mut rest: Vec<u32> = (0..n as u32).into_par_iter().filter(|&v| open(v)).collect();

    let color: Vec<AtomicU32> = (0..n)
        .into_par_iter()
        .map(|_| AtomicU32::new(NONE))
        .collect();
    let queued = Bits::new(n);
    let reached = Bits::new(n);
    let mut rounds = 0;
    loop {
        // Trim sinks: a node none of whose edges leads to an unassigned node
        // (but itself) is a component of its own. A few passes, while they
        // pay.
        for _ in 0..4 {
            let sinks: Vec<u32> = rest
                .par_iter()
                .copied()
                .filter(|&u| !view.any(u, |v| v != u && open(v)))
                .collect();
            sinks
                .par_iter()
                .for_each(|&v| scc[v as usize].store(v, Relaxed));
            let trimmed = sinks.len();
            settle(sinks);
            rest = rest.par_iter().copied().filter(|&v| open(v)).collect();
            if trimmed * 64 < rest.len() {
                break;
            }
        }
        if rest.is_empty() {
            break;
        }
        if rest.len() < SERIAL_REST || rounds == MAX_ROUNDS {
            tarjan_rest(view, &scc, &rest);
            break;
        }
        rounds += 1;

        // Colour: each unassigned node takes the smallest id that reaches it.
        rest.par_iter()
            .for_each(|&v| color[v as usize].store(v, Relaxed));
        // Lowers `v`'s colour to `c` if `v` is unassigned and its colour is
        // higher; whether it did. (A plain load first: most edges change
        // nothing, and the atomic update costs far more.)
        let lower = |v: u32, c: u32| {
            open(v)
                && color[v as usize].load(Relaxed) > c
                && color[v as usize].fetch_min(c, Relaxed) > c
        };
        let push = |u: u32, to: &mut Vec<u32>| {
            let c = color[u as usize].load(Relaxed);
            view.each(u, |v| {
                if v != u && lower(v, c) && !queued.set(v as usize) {
                    to.push(v);
                }
            });
        };
        // First, sweeps in id order: a node pushes its colour on when it has
        // changed since it last did (its `queued` bit), and a smaller id met
        // early in a sweep goes on in the same sweep. Each sweep does less;
        // once one changes little, a frontier takes over.
        rest.par_iter().for_each(|&v| {
            queued.set(v as usize);
        });
        loop {
            let changed: usize = rest
                .par_chunks(SWEEP_CHUNK)
                .map(|chunk| {
                    let mut changed = 0;
                    for &u in chunk {
                        if !queued.get(u as usize) {
                            continue;
                        }
                        queued.clear(u as usize);
                        let c = color[u as usize].load(Relaxed);
                        view.each(u, |v| {
                            if v != u && lower(v, c) && !queued.set(v as usize) {
                                changed += 1;
                            }
                        });
                    }
                    changed
                })
                .sum();
            if changed * 64 < rest.len() {
                break;
            }
        }
        // Invariant: a node in `frontier` has its `queued` bit clear.
        let mut frontier: Vec<u32> = rest
            .par_iter()
            .copied()
            .filter(|&v| queued.get(v as usize))
            .collect();
        frontier.par_iter().for_each(|&v| queued.clear(v as usize));
        while !frontier.is_empty() {
            if frontier.len() < FRONTIER_SERIAL {
                let mut stack = frontier;
                stack.iter().for_each(|&v| {
                    queued.set(v as usize);
                });
                while let Some(u) = stack.pop() {
                    queued.clear(u as usize);
                    push(u, &mut stack);
                    if stack.len() > FRONTIER_GROWN {
                        break;
                    }
                }
                stack.iter().for_each(|&v| queued.clear(v as usize));
                frontier = stack;
            } else {
                let next: Vec<u32> = frontier
                    .par_iter()
                    .flat_map(|&u| {
                        let mut to = Vec::new();
                        push(u, &mut to);
                        to
                    })
                    .collect();
                next.par_iter().for_each(|&v| queued.clear(v as usize));
                frontier = next;
            }
        }

        // The component of each colour's own node: the nodes of that colour
        // that reach it, found by passes that mark a node once one of its
        // out-neighbours of the same colour is marked.
        rest.par_iter().for_each(|&v| {
            if color[v as usize].load(Relaxed) == v {
                reached.set(v as usize);
            }
        });
        let mut pending: Vec<u32> = rest
            .par_iter()
            .copied()
            .filter(|&v| !reached.get(v as usize))
            .collect();
        let mut passes = 0;
        let converged = loop {
            let before = pending.len();
            pending = pending
                .par_iter()
                .copied()
                .filter(|&v| {
                    let c = color[v as usize].load(Relaxed);
                    let back = view.any(v, |w| {
                        w != v
                            && reached.get(w as usize)
                            && open(w)
                            && color[w as usize].load(Relaxed) == c
                    });
                    if back {
                        reached.set(v as usize);
                    }
                    !back
                })
                .collect();
            if pending.len() == before {
                break true;
            }
            passes += 1;
            if passes == MAX_PASSES {
                break false;
            }
        };
        if !converged {
            rest.par_iter().for_each(|&v| reached.clear(v as usize));
            tarjan_rest(view, &scc, &rest);
            break;
        }
        let found: Vec<u32> = rest
            .par_iter()
            .copied()
            .filter(|&v| reached.get(v as usize))
            .collect();
        found.par_iter().for_each(|&v| {
            reached.clear(v as usize);
            scc[v as usize].store(color[v as usize].load(Relaxed), Relaxed);
        });
        rest = rest.par_iter().copied().filter(|&v| open(v)).collect();
        if rest.is_empty() {
            break;
        }
        // (Unless nothing is left, when no in-degree matters any more.)
        settle(found);
        rest = rest.par_iter().copied().filter(|&v| open(v)).collect();
    }
    scc.into_iter().map(AtomicU32::into_inner).collect()
}

/// Serial Tarjan over the unassigned nodes `rest` (ascending), ignoring
/// assigned ones: assigns each a component, named by its Tarjan root here.
fn tarjan_rest(view: &View, scc: &[AtomicU32], rest: &[u32]) {
    let n = view.n;
    let out = view.out;
    let open = |v: u32| scc[v as usize].load(Relaxed) == NONE;
    let mut index = vec![NONE; n];
    let mut lowlink = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut scc_stack: Vec<u32> = Vec::new();
    // (node, next edge slot, its end).
    let mut call_stack: Vec<(u32, usize, usize)> = Vec::new();
    let mut next_index = 0u32;
    for &start in rest {
        if index[start as usize] != NONE {
            continue;
        }
        index[start as usize] = next_index;
        lowlink[start as usize] = next_index;
        next_index += 1;
        scc_stack.push(start);
        on_stack[start as usize] = true;
        call_stack.push((start, view.slot(start), view.slot(start + 1)));
        while let Some(top) = call_stack.last_mut() {
            let (v, k, end) = *top;
            if k < end {
                top.1 = k + 1;
                if view.mask.is_some_and(|m| !m.keep(out.edge_ids[k])) {
                    continue;
                }
                let w = view.at(out.targets[k]);
                if !open(w) {
                    continue;
                }
                if index[w as usize] == NONE {
                    index[w as usize] = next_index;
                    lowlink[w as usize] = next_index;
                    next_index += 1;
                    scc_stack.push(w);
                    on_stack[w as usize] = true;
                    call_stack.push((w, view.slot(w), view.slot(w + 1)));
                } else if on_stack[w as usize] {
                    lowlink[v as usize] = lowlink[v as usize].min(index[w as usize]);
                }
            } else {
                if lowlink[v as usize] == index[v as usize] {
                    loop {
                        let w = scc_stack.pop().expect("scc stack non-empty at a root");
                        on_stack[w as usize] = false;
                        scc[w as usize].store(v, Relaxed);
                        if w == v {
                            break;
                        }
                    }
                }
                call_stack.pop();
                if let Some(&(parent, _, _)) = call_stack.last() {
                    lowlink[parent as usize] = lowlink[parent as usize].min(lowlink[v as usize]);
                }
            }
        }
    }
}

/// Tarjan's labels for the components `scc`: a node alone in its component is
/// its own label; a component of two or more nodes is labelled by the member
/// that a DFS in Tarjan's order (from each unvisited node in id order, edges
/// in CSR order) reaches first, which is Tarjan's root. The DFS stops once it
/// has reached every such component.
fn tarjan_roots(view: &View, scc: Vec<u32>) -> Vec<u32> {
    let n = view.n;
    // Each component's size, then its label: `NONE` until the DFS enters a
    // component of two or more, a lone node's own id.
    let size: Vec<AtomicU32> = (0..n).into_par_iter().map(|_| AtomicU32::new(0)).collect();
    scc.par_iter()
        .for_each(|&s| _ = size[s as usize].fetch_add(1, Relaxed));
    let mut label: Vec<u32> = size
        .into_par_iter()
        .enumerate()
        .map(|(s, k)| if k.into_inner() >= 2 { NONE } else { s as u32 })
        .collect();
    let mut left = label.iter().filter(|&&l| l == NONE).count();

    let mut seen = vec![0u64; n.div_ceil(64)];
    // (node, next edge slot, its end).
    let mut stack: Vec<(u32, usize, usize)> = Vec::new();
    let out = view.out;
    // Marks `v` visited; whether that was the last component to enter.
    let mut visit = |v: u32, seen: &mut [u64], left: &mut usize| -> bool {
        seen[v as usize >> 6] |= 1 << (v & 63);
        let s = scc[v as usize] as usize;
        if label[s] == NONE {
            label[s] = v;
            *left -= 1;
        }
        *left == 0
    };
    'all: for start in 0..n as u32 {
        if left == 0 {
            break;
        }
        if seen[start as usize >> 6] >> (start & 63) & 1 == 1 {
            continue;
        }
        if visit(start, &mut seen, &mut left) {
            break;
        }
        stack.push((start, view.slot(start), view.slot(start + 1)));
        while let Some(top) = stack.last_mut() {
            // The next edge to an unvisited node, if any.
            let (mut k, end) = (top.1, top.2);
            let mut next = None;
            while k < end {
                let keep = view.mask.is_none_or(|m| m.keep(out.edge_ids[k]));
                let w = view.at(out.targets[k]);
                k += 1;
                if keep && seen[w as usize >> 6] >> (w & 63) & 1 == 0 {
                    next = Some(w);
                    break;
                }
            }
            top.1 = k;
            match next {
                Some(w) => {
                    if visit(w, &mut seen, &mut left) {
                        break 'all;
                    }
                    stack.push((w, view.slot(w), view.slot(w + 1)));
                }
                None => {
                    stack.pop();
                }
            }
        }
    }
    scc.par_iter().map(|&s| label[s as usize]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compact runs (#326) give the full runs' labels.
    #[test]
    fn compact_runs_give_the_full_labels() {
        use crate::algo::linked::testing::{sparse, CASES};
        for &(seed, n, m) in CASES {
            let (t, mask) = sparse(seed, n, m);
            for mask in [None, Some(&mask)] {
                let linked = Linked::of(&t, mask);
                assert_eq!(
                    weak_over(&t, mask, Some(&linked)),
                    weak_over(&t, mask, None),
                    "weak: graph {seed}, masked {}",
                    mask.is_some()
                );
                assert_eq!(
                    strong_over(&t, mask, Some(&linked)),
                    strong_over(&t, mask, None),
                    "strong: graph {seed}, masked {}",
                    mask.is_some()
                );
            }
        }
    }

    #[test]
    fn two_components() {
        // {0,1,2} connected; {3,4} connected; edges directed but weak conn.
        let t = Topology::build(5, vec![0, 1, 3], vec![1, 2, 4]);
        let cc = connected_components_weak(&t, None);
        assert_eq!(cc[0], cc[1]);
        assert_eq!(cc[1], cc[2]);
        assert_eq!(cc[3], cc[4]);
        assert_ne!(cc[0], cc[3]);
    }

    #[test]
    fn strong_components_partition_directed_cycles() {
        // SCCs: {0,1,2} (a 3-cycle), {3,4} (a 2-cycle), {5} (isolated).
        // 2->3 links the first two but only one way, so they stay distinct.
        let t = Topology::build(6, vec![0, 1, 2, 2, 3, 4], vec![1, 2, 0, 3, 4, 3]);
        let scc = connected_components_strong(&t, None);
        assert_eq!(scc[0], scc[1]);
        assert_eq!(scc[1], scc[2]);
        assert_eq!(scc[3], scc[4]);
        // The three components are mutually distinct.
        assert_ne!(scc[0], scc[3]);
        assert_ne!(scc[0], scc[5]);
        assert_ne!(scc[3], scc[5]);
    }

    #[test]
    fn strong_differs_from_weak_on_a_directed_chain() {
        // 0->1->2: weakly one component, but strongly three singletons (no cycle).
        let t = Topology::build(3, vec![0, 1], vec![1, 2]);
        let weak = connected_components_weak(&t, None);
        assert_eq!(weak[0], weak[1]);
        assert_eq!(weak[1], weak[2]);
        let strong = connected_components_strong(&t, None);
        assert_ne!(strong[0], strong[1]);
        assert_ne!(strong[1], strong[2]);
        assert_ne!(strong[0], strong[2]);
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

    /// The parallel forms give the serial forms' labels exactly, on every
    /// pinned graph, at several pool sizes, run compact (#326) or not.
    #[test]
    fn parallel_forms_give_the_serial_labels() {
        for (name, t, m) in pinned::graphs() {
            let m = m.as_ref();
            let linked = Linked::of(&t, m);
            for l in [None, Some(&linked)] {
                let weak = weak_over(&t, m, l);
                let strong = strong_over(&t, m, l);
                for threads in [1, 2, 3, 8] {
                    let (w, s) = on_pool(threads, || {
                        (weak_parallel(&t, m, l), strong_parallel(&t, m, l))
                    });
                    let compact = l.is_some();
                    assert_eq!(
                        w, weak,
                        "weak: {name}, {threads} threads, compact {compact}"
                    );
                    assert_eq!(
                        s, strong,
                        "strong: {name}, {threads} threads, compact {compact}"
                    );
                }
            }
        }
    }

    /// Long cycles, in id order, against it, and with chords, through the
    /// colouring's serial frontier and its fallback to serial Tarjan.
    #[test]
    fn parallel_strong_on_long_cycles() {
        let n = 5_000u32;
        let graphs: [(Vec<u32>, Vec<u32>); 3] = [
            ((0..n).collect(), (0..n).map(|i| (i + 1) % n).collect()),
            ((0..n).collect(), (0..n).map(|i| (i + n - 1) % n).collect()),
            (
                (0..n).chain((0..n).step_by(7)).collect(),
                (0..n)
                    .map(|i| (i + 1) % n)
                    .chain((0..n).step_by(7).map(|i| (i * 31) % n))
                    .collect(),
            ),
        ];
        for (src, dst) in graphs {
            let t = Topology::build(n as usize, src, dst);
            for threads in [1, 4] {
                let s = on_pool(threads, || strong_parallel(&t, None, None));
                assert_eq!(s, strong_over(&t, None, None));
            }
        }
    }

    /// Exact outputs pinned against the serial kernels as they were before the
    /// parallel versions (#383): a label per node on small graphs, and an FNV-1a
    /// hash of the whole label vector on generated ones. Any change to a label,
    /// not only to the partition, fails here.
    mod pinned {
        use super::*;
        use crate::algo::linked::testing::{next, sparse};

        pub(crate) fn fnv(labels: &[u32]) -> u64 {
            labels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &l| {
                l.to_le_bytes()
                    .iter()
                    .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
            })
        }

        /// A random permutation of `0..n`.
        fn perm(st: &mut u64, n: usize) -> Vec<u32> {
            let mut p: Vec<u32> = (0..n as u32).collect();
            for i in (1..n).rev() {
                p.swap(i, (next(st) % (i as u64 + 1)) as usize);
            }
            p
        }

        /// `m` uniform random edges over `n` nodes.
        fn uniform(seed: u64, n: usize, m: usize) -> Topology {
            let mut st = seed;
            let (s, d): (Vec<u32>, Vec<u32>) = (0..m)
                .map(|_| {
                    let a = (next(&mut st) % n as u64) as u32;
                    (a, (next(&mut st) % n as u64) as u32)
                })
                .unzip();
            Topology::build(n, s, d)
        }

        /// `m` R-MAT edges over `2^scale` nodes (many left isolated).
        fn rmat(seed: u64, scale: u32, m: usize) -> Topology {
            let mut st = seed;
            let (a, ab, abc) = (0.57, 0.76, 0.95);
            let (s, d): (Vec<u32>, Vec<u32>) = (0..m)
                .map(|_| {
                    let (mut s, mut d) = (0u32, 0u32);
                    for _ in 0..scale {
                        let r = (next(&mut st) >> 11) as f64 / (1u64 << 53) as f64;
                        s = s << 1 | u32::from(r >= ab);
                        d = d << 1 | u32::from((r >= a && r < ab) || r >= abc);
                    }
                    (s, d)
                })
                .unzip();
            Topology::build(1 << scale, s, d)
        }

        /// Many small components (paths, cycles, stars, cliques, self-loops,
        /// lone nodes) of 1 to 9 nodes, their ids scattered by a permutation.
        fn small_components(seed: u64, n: usize) -> Topology {
            let mut st = seed;
            let p = perm(&mut st, n);
            let (mut s, mut d) = (Vec::new(), Vec::new());
            let mut at = 0usize;
            while at < n {
                let k = (1 + next(&mut st) % 9).min((n - at) as u64) as usize;
                let g = &p[at..at + k];
                match next(&mut st) % 5 {
                    0 => (1..k).for_each(|i| {
                        s.push(g[i - 1]);
                        d.push(g[i]);
                    }),
                    1 => (0..k).for_each(|i| {
                        s.push(g[i]);
                        d.push(g[(i + 1) % k]);
                    }),
                    2 => (1..k).for_each(|i| {
                        s.push(g[i]);
                        d.push(g[0]);
                    }),
                    3 => (0..k).for_each(|i| {
                        (0..k).filter(|&j| j != i).for_each(|j| {
                            s.push(g[i]);
                            d.push(g[j]);
                        })
                    }),
                    _ => g.iter().for_each(|&v| {
                        s.push(v);
                        d.push(v);
                    }),
                }
                at += k;
            }
            Topology::build(n, s, d)
        }

        /// One giant component: a directed cycle through every node in a random
        /// order, plus `extra` random edges.
        fn giant(seed: u64, n: usize, extra: usize) -> Topology {
            let mut st = seed;
            let p = perm(&mut st, n);
            let (mut s, mut d): (Vec<u32>, Vec<u32>) =
                (0..n).map(|i| (p[i], p[(i + 1) % n])).unzip();
            for _ in 0..extra {
                s.push((next(&mut st) % n as u64) as u32);
                d.push((next(&mut st) % n as u64) as u32);
            }
            Topology::build(n, s, d)
        }

        /// Directed cycles of 2 to 20 nodes (and some paths), scattered, each
        /// linked from one earlier group so the groups form a DAG, with a
        /// self-loop now and then: many strong components in few weak ones.
        fn cycles(seed: u64, n: usize) -> Topology {
            let mut st = seed;
            let p = perm(&mut st, n);
            let (mut s, mut d) = (Vec::new(), Vec::new());
            let mut heads: Vec<u32> = Vec::new();
            let mut at = 0usize;
            while at < n {
                let k = (2 + next(&mut st) % 19).min((n - at) as u64) as usize;
                let g = &p[at..at + k];
                if k > 1 && !next(&mut st).is_multiple_of(4) {
                    (0..k).for_each(|i| {
                        s.push(g[i]);
                        d.push(g[(i + 1) % k]);
                    });
                } else {
                    (1..k).for_each(|i| {
                        s.push(g[i - 1]);
                        d.push(g[i]);
                    });
                }
                if next(&mut st).is_multiple_of(7) {
                    s.push(g[0]);
                    d.push(g[0]);
                }
                let pick = (next(&mut st) % (heads.len() as u64 + 1)) as usize;
                if let Some(&h) = heads.get(pick) {
                    s.push(h);
                    d.push(g[(next(&mut st) % k as u64) as usize]);
                }
                heads.push(g[(next(&mut st) % k as u64) as usize]);
                at += k;
            }
            Topology::build(n, s, d)
        }

        /// A random graph with a self-loop on about every third node.
        fn self_loops(seed: u64, n: usize, m: usize) -> Topology {
            let mut st = seed;
            let (mut s, mut d) = (Vec::new(), Vec::new());
            for _ in 0..m {
                s.push((next(&mut st) % n as u64) as u32);
                d.push((next(&mut st) % n as u64) as u32);
            }
            for v in 0..n as u32 {
                if next(&mut st).is_multiple_of(3) {
                    s.push(v);
                    d.push(v);
                }
            }
            Topology::build(n, s, d)
        }

        /// A mask keeping about `keep` edges in 8.
        fn mask(seed: u64, m: usize, keep: u64) -> EdgeMask {
            let mut st = seed;
            let k: Vec<bool> = (0..m).map(|_| next(&mut st) % 8 < keep).collect();
            EdgeMask::from_bools(&k)
        }

        /// The pinned graphs: a name, the topology and a mask.
        pub(crate) fn graphs() -> Vec<(&'static str, Topology, Option<EdgeMask>)> {
            let mut g: Vec<(&'static str, Topology, Option<EdgeMask>)> = vec![
                ("uniform sparse", uniform(11, 5_000, 4_000), None),
                ("uniform dense", uniform(12, 4_000, 40_000), None),
                ("rmat", rmat(13, 14, 60_000), None),
                ("small components", small_components(14, 20_000), None),
                ("giant", giant(15, 12_000, 30_000), None),
                ("giant, a path", giant(16, 30_000, 0), None),
                ("cycles", cycles(17, 20_000), None),
                ("self-loops", self_loops(18, 3_000, 2_500), None),
            ];
            let t = rmat(19, 13, 30_000);
            let m = mask(20, t.n_edges(), 5);
            g.push(("rmat, masked", t, Some(m)));
            let t = cycles(21, 10_000);
            let m = mask(22, t.n_edges(), 7);
            g.push(("cycles, masked", t, Some(m)));
            // Isolated vertices, as #326 has them: the compact runs.
            let (t, m) = sparse(23, 40_000, 9_000);
            g.push(("isolated", sparse(23, 40_000, 9_000).0, None));
            g.push(("isolated, masked", t, Some(m)));
            g
        }

        /// (weak hash, strong hash) per graph of [`graphs`], from the serial
        /// kernels before #383.
        const PINNED: &[(&str, u64, u64)] = &[
            ("uniform sparse", 0xd969d63f3e302fac, 0x5b35d2f40009d715),
            ("uniform dense", 0x35da70bdb324c525, 0x2c36c2471ceec525),
            ("rmat", 0x389a40a3b0ca34bf, 0x9ec683b86cb6f611),
            ("small components", 0x0608503661e8db16, 0xa3506ef85af28733),
            ("giant", 0x207327a55dadd625, 0x810efd6d3c240925),
            ("giant, a path", 0x792fc743ff1f1025, 0xe94d157b19346225),
            ("cycles", 0x5f935d30b79ecd4e, 0x2612d677587752c1),
            ("self-loops", 0xa99d05d5b6074213, 0xc0d981f1dd370ef0),
            ("rmat, masked", 0x7cb3611586dd14d9, 0xffabc2fe691bf0af),
            ("cycles, masked", 0x39efc3fc11cf1a36, 0xa21f5009fec93113),
            ("isolated", 0x4fce32e9c714b779, 0xe166896181a7b329),
            ("isolated, masked", 0x5228bbcd6fb3536d, 0xe70c68a66c38d11d),
        ];

        #[test]
        fn generated_graphs_keep_their_labels() {
            let graphs = graphs();
            assert_eq!(graphs.len(), PINNED.len());
            for ((name, t, m), &(pname, weak, strong)) in graphs.iter().zip(PINNED) {
                assert_eq!(*name, pname);
                let w = connected_components_weak(t, m.as_ref());
                let s = connected_components_strong(t, m.as_ref());
                assert_eq!(fnv(&w), weak, "weak labels changed: {name}");
                assert_eq!(fnv(&s), strong, "strong labels changed: {name}");
                // The parallel forms, whatever the graph's size.
                let l = linked_nodes(t, m.as_ref());
                let w = weak_parallel(t, m.as_ref(), l.as_ref());
                let s = strong_parallel(t, m.as_ref(), l.as_ref());
                assert_eq!(fnv(&w), weak, "parallel weak labels differ: {name}");
                assert_eq!(fnv(&s), strong, "parallel strong labels differ: {name}");
            }
        }

        #[test]
        fn small_graphs_keep_their_labels() {
            // 0->1->2->0, 3->4->6<->7, 8->3, 5 with a self-loop.
            let t = Topology::build(
                9,
                vec![0, 1, 2, 3, 4, 6, 7, 8, 5],
                vec![1, 2, 0, 4, 6, 7, 6, 3, 5],
            );
            assert_eq!(
                connected_components_weak(&t, None),
                vec![0, 0, 0, 3, 3, 5, 3, 3, 3]
            );
            assert_eq!(
                connected_components_strong(&t, None),
                vec![0, 0, 0, 3, 4, 5, 6, 6, 8]
            );
            // A star into the hub 3, and the hub's own self-loop.
            let t = Topology::build(6, vec![0, 1, 2, 4, 5, 3], vec![3, 3, 3, 3, 3, 3]);
            assert_eq!(connected_components_weak(&t, None), vec![0, 0, 0, 0, 0, 0]);
            assert_eq!(
                connected_components_strong(&t, None),
                vec![0, 1, 2, 3, 4, 5]
            );
            // A DFS that enters the cycle {2,4,5} at 4, not at its smallest id.
            let t = Topology::build(6, vec![0, 1, 4, 5, 2, 1], vec![1, 4, 5, 2, 4, 3]);
            assert_eq!(connected_components_weak(&t, None), vec![0, 0, 0, 0, 0, 0]);
            assert_eq!(
                connected_components_strong(&t, None),
                vec![0, 1, 4, 3, 4, 4]
            );
        }
    }
}
