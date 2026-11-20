//! Weak and strong components on a large R-MAT graph (#383): the kernel's wall
//! time, how many cores it kept busy, and the most heap it held beyond the
//! graph (run under `/usr/bin/time -v` for the process's peak RSS, which the
//! graph's build sets).
//!
//! ```text
//! cargo run --release -p ursa-core --example components_scaling -- \
//!     --edges 256000000 --kernel weak
//! ```
//!
//! `--kernel` is `weak`, `strong` or `none` (build the graph only: the RSS the
//! kernel adds is the difference). Linux only for the cores figure, which reads
//! the process's CPU time from `/proc/self/stat`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use ursa_core::algo::{connected_components_strong, connected_components_weak};
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

/// `edges` R-MAT edges over `2^scale` node slots, generated in parallel blocks
/// with one seed each (the same graph on any thread count).
fn rmat(edges: usize, scale: u32, seed: u64) -> (Vec<u32>, Vec<u32>) {
    use rayon::prelude::*;
    const BLOCK: usize = 1 << 20;
    let (a, ab, abc) = (0.57, 0.76, 0.95);
    let blocks: Vec<(Vec<u32>, Vec<u32>)> = (0..edges.div_ceil(BLOCK))
        .into_par_iter()
        .map(|b| {
            let mut st = seed ^ (b as u64).wrapping_mul(0xA24B_AED4_963E_E407);
            (b * BLOCK..((b + 1) * BLOCK).min(edges))
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
        })
        .collect();
    let mut src = Vec::with_capacity(edges);
    let mut dst = Vec::with_capacity(edges);
    for (s, d) in blocks {
        src.extend(s);
        dst.extend(d);
    }
    (src, dst)
}

/// The process's user + system CPU seconds, from `/proc/self/stat`.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // Fields after the parenthesised command name; utime and stime are 14 and 15.
    let rest = stat.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks: f64 = f.get(11).and_then(|x| x.parse().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|x| x.parse().ok()).unwrap_or(0.0);
    ticks / 100.0 // USER_HZ
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let edges: usize = arg(&args, "--edges").map_or(16_000_000, |s| s.parse().unwrap());
    let kernel = arg(&args, "--kernel").unwrap_or_else(|| "weak".into());
    // 16 edges per node slot, as Graph500 does.
    let scale = ((edges / 16).max(2) as f64).log2().round() as u32;

    let t = Instant::now();
    let (src, dst) = rmat(edges, scale, 42);
    let topo = Topology::build(1 << scale, src, dst);
    eprintln!(
        "graph: 2^{scale} nodes, {} edges, built in {:.1} s",
        topo.n_edges(),
        t.elapsed().as_secs_f64()
    );

    let held = HELD.load(Ordering::Relaxed);
    PEAK.store(held, Ordering::Relaxed);
    let (cpu0, t) = (cpu_seconds(), Instant::now());
    let labels = match kernel.as_str() {
        "weak" => connected_components_weak(&topo, None),
        "strong" => connected_components_strong(&topo, None),
        _ => return,
    };
    let (wall, cpu) = (t.elapsed().as_secs_f64(), cpu_seconds() - cpu0);
    let extra = (PEAK.load(Ordering::Relaxed) - held) as f64 / (1 << 20) as f64;
    let mut roots: Vec<u32> = labels
        .iter()
        .enumerate()
        .filter(|&(v, &l)| v as u32 == l)
        .map(|(v, _)| v as u32)
        .collect();
    roots.dedup();
    let hash = labels.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &l| {
        (h ^ u64::from(l)).wrapping_mul(0x0100_0000_01b3)
    });
    println!(
        "{kernel}: {wall:.2} s wall, {cpu:.2} s cpu, {:.2} cores busy, {extra:.0} MiB peak heap \
         beyond the graph ({:.0} MiB), {} components, labels {hash:#018x}",
        cpu / wall,
        held as f64 / (1 << 20) as f64,
        roots.len(),
    );
}
