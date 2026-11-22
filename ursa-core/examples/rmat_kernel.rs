//! One kernel on one R-MAT graph, for measuring a change to it (#420, #454,
//! #422).
//!
//! Prints the kernel's wall time, the CPU time it used (all threads) and a
//! hash of its result, so two builds can be compared and shown to agree. Run
//! it under `/usr/bin/time -v` for the process's peak resident memory; with
//! `--kernel none` that is the cost of the graph alone.
//!
//! ```text
//! cargo run --release -p ursa-core --example rmat_kernel -- \
//!     --kernel triangles --edges 16000000 --scale 20
//! cargo run --release -p ursa-core --example rmat_kernel -- \
//!     --kernel betweenness --edges 32000000 --scale 21 --sources 105
//! cargo run --release -p ursa-core --example rmat_kernel -- \
//!     --kernel label-propagation --edges 32000000 --scale 21 [--parallel]
//! cargo run --release -p ursa-core --example rmat_kernel -- \
//!     --kernel closeness --edges 1000000 --scale 16 [--direction both]
//! ```
//!
//! `--scale` is the log2 of the node slots (default: `edges / 16` slots, as
//! Graph500). `--vertices V` spreads the slots evenly among `V` nodes, the
//! rest isolated, as a vertex table would (#326). `--sources K` is the size of
//! the betweenness sample. The R-MAT edges are those of the Graph500 generator
//! (quadrants 0.57, 0.19, 0.19, 0.05) from a seeded stream, so every run sees
//! the same graph. The thread count is rayon's (`RAYON_NUM_THREADS`).
//!
//! `--kernel label-propagation` runs 20 sweeps (seed 1), serially or, with
//! `--parallel`, in parallel sub-rounds (#453), and prints the community count
//! and modularity with the hash. `--planted P` replaces the R-MAT graph with a
//! planted-partition one: `edges / 16` nodes in communities of 16 to 79 nodes,
//! with P% of the edges drawn between communities.
//!
//! `--kernel closeness` is exact unweighted closeness from every node, along
//! `--direction` (`out`, the default, `in` or `both`).

use std::time::Instant;

use ursa_core::algo::{betweenness, closeness_directed, label_propagation, triangle_count};
use ursa_core::{Direction, Topology};

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

/// A planted-partition graph: `edges / 16` nodes cut into consecutive
/// communities of 16 to 79 nodes; each edge's source drawn uniformly, and its
/// destination from the whole graph for `percent`% of the edges, from the
/// source's community otherwise.
fn planted(edges: usize, percent: usize) -> (usize, Vec<u32>, Vec<u32>) {
    let n = (edges / 16).max(16);
    let mut st = 7u64;
    let mut start = vec![0u32; n];
    let mut size = vec![0u32; n];
    let mut lo = 0usize;
    while lo < n {
        let len = (16 + (next(&mut st) % 64) as usize).min(n - lo);
        for u in lo..lo + len {
            start[u] = lo as u32;
            size[u] = len as u32;
        }
        lo += len;
    }
    let (src, dst) = (0..edges)
        .map(|_| {
            let s = (next(&mut st) % n as u64) as usize;
            let d = if (next(&mut st) % 100) < percent as u64 {
                (next(&mut st) % n as u64) as u32
            } else {
                start[s] + (next(&mut st) % u64::from(size[s])) as u32
            };
            (s as u32, d)
        })
        .unzip();
    (n, src, dst)
}

/// Modularity (γ = 1) of the communities `labels` over the edges, taken as
/// undirected.
fn modularity(t: &Topology, labels: &[u32]) -> f64 {
    let out = t.out();
    let (mut tot, mut intra) = (vec![0.0f64; labels.len()], 0.0f64);
    for u in 0..t.n_nodes() as u32 {
        for &v in out.neighbors(u) {
            let (a, b) = (labels[u as usize], labels[v as usize]);
            tot[a as usize] += 1.0;
            tot[b as usize] += 1.0;
            if a == b {
                intra += 2.0;
            }
        }
    }
    let m2 = 2.0 * t.n_edges() as f64;
    intra / m2 - tot.iter().map(|x| (x / m2) * (x / m2)).sum::<f64>()
}

fn arg(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    Some(args.get(i + 1).expect("a value").replace('_', ""))
}

/// CPU seconds this process has used, user plus system, on every thread
/// (Linux: `/proc/self/stat`, in ticks of 1/100 s; elsewhere not known).
fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The fields after the command name, which may itself hold spaces.
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace().skip(11);
    let utime: f64 = fields.next()?.parse().ok()?;
    let stime: f64 = fields.next()?.parse().ok()?;
    Some((utime + stime) / 100.0)
}

/// FNV-1a over 64-bit words.
fn fnv(words: impl Iterator<Item = u64>) -> u64 {
    words.fold(0xCBF2_9CE4_8422_2325, |h, w| {
        (h ^ w).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let number = |name: &str| arg(&args, name).map(|v| v.parse::<usize>().expect("a number"));
    let kernel = arg(&args, "--kernel").unwrap_or_else(|| "triangles".into());
    let edges = number("--edges").unwrap_or(5_000_000);
    let scale = number("--scale").map_or_else(
        || ((edges / 16).max(2) as f64).log2().round() as u32,
        |s| s as u32,
    );
    let slots = 1usize << scale;
    let vertices = number("--vertices").unwrap_or(slots);
    assert!(
        vertices >= slots,
        "--vertices is below the {slots} node slots"
    );
    let sources = number("--sources").unwrap_or(105);
    let parallel = args.iter().any(|a| a == "--parallel");

    let t = if let Some(percent) = number("--planted") {
        let (n, src, dst) = planted(edges, percent);
        Topology::build(n, src, dst)
    } else {
        let (mut src, mut dst) = rmat(edges, scale, 42);
        let step = (vertices / slots) as u32;
        if step > 1 {
            src.iter_mut()
                .chain(dst.iter_mut())
                .for_each(|v| *v *= step);
        }
        Topology::build(vertices, src, dst)
    };
    let vertices = t.n_nodes();
    // Built before timing, as an index read from storage has them.
    t.incoming();
    if kernel == "triangles" {
        t.undirected();
    }
    let mut communities = None;
    let (start, cpu) = (Instant::now(), cpu_seconds());
    let mut result = match kernel.as_str() {
        "none" => String::new(),
        "triangles" => {
            let counts = triangle_count(&t, None);
            let total: u64 = counts.iter().map(|&c| u64::from(c)).sum();
            let hash = fnv(counts.iter().map(|&c| u64::from(c)));
            format!("{} triangles, result hash {hash:016x}", total / 3)
        }
        "betweenness" => {
            // A fraction that samples exactly `sources` nodes.
            let frac = (sources as f64 - 0.5) / vertices as f64;
            let bc = betweenness(&t, None, Some(frac), Some(42));
            let hash = fnv(bc.iter().map(|x| x.to_bits()));
            format!("{sources} sources, result hash {hash:016x}")
        }
        "label-propagation" => {
            // Described after the clock stops: the modularity takes a pass.
            communities = Some(label_propagation(&t, None, 20, Some(1), parallel));
            String::new()
        }
        "closeness" => {
            let dir = match arg(&args, "--direction").as_deref() {
                None | Some("out") => Direction::Out,
                Some("in") => Direction::In,
                Some("both") => Direction::Both,
                Some(other) => panic!("--direction is out, in or both, not {other}"),
            };
            let c = closeness_directed(&t, None, dir);
            let hash = fnv(c.iter().map(|x| x.to_bits()));
            format!("{dir:?}, result hash {hash:016x}")
        }
        other => {
            panic!(
                "--kernel is triangles, betweenness, label-propagation, closeness or none, \
                 not {other}"
            )
        }
    };
    let wall = start.elapsed().as_secs_f64();
    let cpu = cpu
        .zip(cpu_seconds())
        .map_or_else(|| "?".into(), |(a, b)| format!("{:.1}", b - a));
    if let Some(labels) = communities {
        let hash = fnv(labels.iter().map(|&l| u64::from(l)));
        let mut distinct = labels.clone();
        distinct.sort_unstable();
        distinct.dedup();
        result = format!(
            "{}, {} communities, modularity {:.5}, result hash {hash:016x}",
            if parallel { "parallel" } else { "serial" },
            distinct.len(),
            modularity(&t, &labels)
        );
    }
    println!("{kernel}: {edges} edges, {vertices} nodes: {wall:.2} s wall, {cpu} CPU-s");
    println!("{result}");
}
