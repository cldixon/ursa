//! What isolated vertices cost the kernels (#326).
//!
//! A graph with a vertex table has every key as a node, edges or not, and on
//! real data most keys can have none (37.2M of the Bluesky follows graph's
//! 39.65M). This runs each kernel over one R-MAT edge list twice: over the
//! edges' own nodes, and with those nodes scattered among `--vertices` nodes,
//! the rest isolated, as a vertex table would number them. Each line is the
//! kernel's wall time and the peak heap it allocated above what the topology
//! already held (its working memory plus its result, one value per node).
//! The topology's transpose and undirected view are built before timing,
//! as an index read from storage has them.
//!
//! ```text
//! cargo run --release -p ursa-core --example isolated_vertices -- \
//!     --edges 5000000 --vertices 20000000
//! ```
//!
//! The R-MAT edges are those of the Graph500 generator (quadrants 0.57, 0.19,
//! 0.19, 0.05; `edges / 16` node slots), from a seeded stream, so every run
//! sees the same graph.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ursa_core::algo::{
    connected_components_strong, connected_components_weak, label_propagation, louvain, pagerank,
    triangle_count, PageRankParams,
};
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

fn arg(args: &[String], name: &str, default: usize) -> usize {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map_or(default, |v| v.replace('_', "").parse().expect("a number"))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let edges = arg(&args, "--edges", 5_000_000);
    let vertices = arg(&args, "--vertices", 20_000_000);
    let scale = ((edges / 16).max(2) as f64).log2().round() as u32;
    let (src, dst) = rmat(edges, scale, 42);

    // The slots that are nodes, numbered in a random order (the edges' own
    // node set), and the same nodes at random places among `vertices` nodes.
    let slots = 1usize << scale;
    let mut used = vec![false; slots];
    for &v in src.iter().chain(&dst) {
        used[v as usize] = true;
    }
    let mut st = 7u64;
    let mut nodes: Vec<u32> = (0..slots as u32).filter(|&v| used[v as usize]).collect();
    for i in (1..nodes.len()).rev() {
        nodes.swap(i, (next(&mut st) % (i as u64 + 1)) as usize);
    }
    let n = nodes.len();
    assert!(
        vertices >= n,
        "--vertices is below the {n} nodes of the edges"
    );
    let mut own = vec![u32::MAX; slots];
    for (i, &v) in nodes.iter().enumerate() {
        own[v as usize] = i as u32;
    }
    // A partial Fisher-Yates over 0..vertices: the first n entries.
    let mut at: Vec<u32> = (0..vertices as u32).collect();
    for i in 0..n {
        let j = i + (next(&mut st) % (vertices - i) as u64) as usize;
        at.swap(i, j);
    }
    at.truncate(n);
    let wide = |v: u32| at[own[v as usize] as usize];

    println!(
        "R-MAT {edges} edges, {n} nodes with edges; with the vertex table, {vertices} nodes \
         ({:.1}% isolated)",
        100.0 * (vertices - n) as f64 / vertices as f64
    );
    println!("| kernel | edges only | + vertex table |");
    println!("|---|---|---|");

    let graphs = [
        Topology::build(
            n,
            src.iter().map(|&v| own[v as usize]).collect(),
            dst.iter().map(|&v| own[v as usize]).collect(),
        ),
        Topology::build(
            vertices,
            src.iter().map(|&v| wide(v)).collect(),
            dst.iter().map(|&v| wide(v)).collect(),
        ),
    ];
    drop((src, dst, used, own, at, nodes));
    for t in &graphs {
        t.incoming();
        t.undirected();
    }

    type Kernel = fn(&Topology) -> usize;
    let kernels: [(&str, Kernel); 6] = [
        ("pagerank", |t| {
            pagerank(t, None, PageRankParams::default()).len()
        }),
        ("louvain", |t| louvain(t, None, 1.0, Some(1)).len()),
        ("label_propagation", |t| {
            label_propagation(t, None, 20, Some(1)).len()
        }),
        ("weak components", |t| {
            connected_components_weak(t, None).len()
        }),
        ("strong components", |t| {
            connected_components_strong(t, None).len()
        }),
        ("triangle_count", |t| triangle_count(t, None).len()),
    ];
    for (name, kernel) in kernels {
        let cells: Vec<String> = graphs
            .iter()
            .map(|t| {
                let base = HELD.load(Ordering::Relaxed);
                PEAK.store(base, Ordering::Relaxed);
                let start = Instant::now();
                let rows = kernel(t);
                let secs = start.elapsed().as_secs_f64();
                let peak = PEAK.load(Ordering::Relaxed) - base;
                assert_eq!(rows, t.n_nodes());
                format!("{secs:.2} s / {:.0} MB", peak as f64 / 1e6)
            })
            .collect();
        println!("| {name} | {} | {} |", cells[0], cells[1]);
    }
}
