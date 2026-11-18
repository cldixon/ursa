//! Louvain's memory on a large R-MAT graph (#352).
//!
//! Builds one R-MAT edge list over its own nodes, runs Louvain on it once,
//! unweighted or (with `--weighted`) with inexact per-edge weights, and prints
//! the run's wall time, the peak heap it allocated above what the topology
//! already held, and a fingerprint of its result: the community count, the
//! modularity (and its bits) and a hash of the labels, so two builds can be
//! checked to agree bit for bit. Run it under `/usr/bin/time -v` for the
//! process's peak RSS.
//!
//! ```text
//! cargo run --release -p ursa-core --example louvain_memory -- \
//!     --edges 40000000 [--weighted]
//! ```
//!
//! The R-MAT edges are those of the Graph500 generator (quadrants 0.57, 0.19,
//! 0.19, 0.05; `edges / 16` node slots), from a seeded stream, so every run
//! sees the same graph.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ursa_core::algo::{louvain, louvain_weighted};
use ursa_core::Topology;

/// The system allocator, counting the bytes held and their peak.
struct Counting;

static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = HELD.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        HELD.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let grow = new_size - layout.size();
                let now = HELD.fetch_add(grow, Ordering::Relaxed) + grow;
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                HELD.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// splitmix64.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `edges` R-MAT edges over `2^scale` node slots.
fn rmat(edges: usize, scale: u32, seed: u64) -> (Vec<u32>, Vec<u32>) {
    let mut st = seed;
    let (a, ab, abc) = (0.57, 0.76, 0.95);
    (0..edges)
        .map(|_| {
            let (mut s, mut d) = (0u32, 0u32);
            for _ in 0..scale {
                let r = (next(&mut st) >> 11) as f64 / (1u64 << 53) as f64;
                s = s << 1 | u32::from(r >= ab);
                d = d << 1 | u32::from((r >= a && r < ab) || r >= abc);
            }
            (s, d)
        })
        .unzip()
}

/// The R-MAT edges over their own nodes (numbered in slot order), and the
/// node count.
fn graph(edges: usize) -> (usize, Vec<u32>, Vec<u32>) {
    let scale = ((edges / 16).max(2) as f64).log2().round() as u32;
    let (mut src, mut dst) = rmat(edges, scale, 42);
    let mut own = vec![u32::MAX; 1 << scale];
    for &v in src.iter().chain(&dst) {
        own[v as usize] = 0;
    }
    let mut n = 0u32;
    for slot in own.iter_mut().filter(|s| **s == 0) {
        *slot = n;
        n += 1;
    }
    for v in src.iter_mut().chain(dst.iter_mut()) {
        *v = own[*v as usize];
    }
    (n as usize, src, dst)
}

/// Inexact, deterministic per-edge weights in `(0, 3]`.
fn weights(edges: usize) -> Vec<f64> {
    (0..edges)
        .map(|i| ((i % 89) as f64 + 1.0).sqrt() / 3.0)
        .collect()
}

fn arg(args: &[String], name: &str, default: usize) -> usize {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map_or(default, |v| v.replace('_', "").parse().expect("a number"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let edges = arg(&args, "--edges", 5_000_000);
    let weighted = args.iter().any(|a| a == "--weighted");

    let (n, src, dst) = graph(edges);
    let topo = Topology::build(n, src, dst);
    let w = weighted.then(|| weights(edges));
    println!(
        "R-MAT {edges} edges, {n} nodes, {}",
        if weighted { "weighted" } else { "unweighted" }
    );

    let base = HELD.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let start = Instant::now();
    let comm = match &w {
        None => louvain(&topo, None, 1.0, Some(1)),
        Some(w) => louvain_weighted(&topo, w, None, 1.0, Some(1)),
    };
    let secs = start.elapsed().as_secs_f64();
    let peak = PEAK.load(Ordering::Relaxed) - base;
    drop(topo);

    // Modularity of the partition over the symmetrised graph (γ = 1).
    let (_, src, dst) = graph(edges);
    let k = comm.iter().max().map_or(0, |&c| c as usize + 1);
    let (mut tot, mut intra, mut m2) = (vec![0.0f64; k], 0.0f64, 0.0f64);
    for (i, (&s, &d)) in src.iter().zip(&dst).enumerate() {
        let wt = w.as_ref().map_or(1.0, |w| w[i]);
        let (cs, cd) = (comm[s as usize] as usize, comm[d as usize] as usize);
        tot[cs] += wt;
        tot[cd] += wt;
        m2 += 2.0 * wt;
        if cs == cd {
            intra += 2.0 * wt;
        }
    }
    let q = intra / m2 - tot.iter().map(|t| (t / m2) * (t / m2)).sum::<f64>();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &c in &comm {
        hash = (hash ^ u64::from(c)).wrapping_mul(0x100_0000_01b3);
    }
    println!(
        "louvain: {secs:.2} s, peak heap above the topology {:.0} MB",
        peak as f64 / 1e6
    );
    println!(
        "communities {k}, modularity {q:.12} (bits {:#018x}), labels hash {hash:#018x}",
        q.to_bits()
    );
}
