//! What building edge ids late saves (#170): one kernel on one R-MAT graph,
//! its index built as the SQL engine builds one (nodes relabelled by
//! in-degree), with the edge ids built eagerly or read back from a spilled
//! file of the rows' sources when first read.
//!
//! Prints the build's wall time, the kernel's, the resident memory after the
//! build and at the end (`VmRSS`), the peak (`VmHWM`), whether the kernel
//! built the edge ids, and a hash of the result, so the two modes can be
//! shown to agree:
//!
//! ```text
//! cargo run --release -p ursa-core --example edge_ids_memory -- \
//!     --kernel pagerank --edges 16000000 --ids eager
//! cargo run --release -p ursa-core --example edge_ids_memory -- \
//!     --kernel pagerank --edges 16000000 --ids late
//! ```
//!
//! Kernels: `none` (the index alone), `pagerank`, `components` (weak),
//! `hop` (2 hops out of the hub), `bfs` (from the hub, both directions),
//! `louvain` (serial, seed 1), and `weighted-pagerank` (which reads the edge
//! ids, so pays the rebuild). The R-MAT edges are rmat_kernel's (Graph500
//! quadrants, seed 42), generated in memory, `edges / 16` node slots.

use std::sync::Arc;
use std::time::Instant;

use ursa_core::algo::{
    bfs_distances, connected_components_weak, k_hop_distances_masked, louvain, pagerank,
    pagerank_weighted, PageRankParams,
};
use ursa_core::topology::{Direction, NodeOrder, RowSource, Topology};

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

/// A `/proc/self/status` field, in MiB.
fn status_mib(field: &str) -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
        })
        .map_or(f64::NAN, |kib| kib / 1024.0)
}

fn fnv(words: impl Iterator<Item = u64>) -> u64 {
    words.fold(0xCBF2_9CE4_8422_2325, |h, w| {
        (h ^ w).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

fn arg(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    Some(args.get(i + 1).expect("a value").replace('_', ""))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let kernel = arg(&args, "--kernel").unwrap_or_else(|| "none".into());
    let edges: usize = arg(&args, "--edges").map_or(16_000_000, |v| v.parse().unwrap());
    let late = match arg(&args, "--ids").as_deref() {
        None | Some("late") => true,
        Some("eager") => false,
        Some(other) => panic!("--ids is eager or late, not {other}"),
    };
    let scale = ((edges / 16) as f64).log2().round() as u32;
    let (src, dst) = rmat(edges, scale, 42);
    let base = status_mib("VmRSS:");

    let started = Instant::now();
    let mut rows = |sources: &[u32]| -> Option<Arc<dyn RowSource>> {
        if !late {
            return None;
        }
        #[cfg(unix)]
        return Some(Arc::new(
            ursa_core::topology::SpilledRows::write(sources).expect("a temporary file"),
        ));
        #[cfg(not(unix))]
        None
    };
    let t = Topology::build_with_rows(1 << scale, src, dst, NodeOrder::InDegree, &mut rows);
    let build = started.elapsed().as_secs_f64();
    let after_build = status_mib("VmRSS:");

    let params = PageRankParams {
        damping: 0.85,
        max_iter: 20,
        tol: 0.0,
    };
    let hub = (0..t.n_nodes() as u32)
        .max_by_key(|&u| (t.out().degree(u), std::cmp::Reverse(u)))
        .unwrap_or(0);
    let started = Instant::now();
    let hash = match kernel.as_str() {
        "none" => 0,
        "pagerank" => fnv(pagerank(&t, None, params).iter().map(|x| x.to_bits())),
        "weighted-pagerank" => {
            let w: Vec<f64> = (0..t.n_edges()).map(|e| (e % 7) as f64 + 1.0).collect();
            fnv(pagerank_weighted(&t, &w, None, params)
                .iter()
                .map(|x| x.to_bits()))
        }
        "components" => fnv(connected_components_weak(&t, None)
            .iter()
            .map(|&x| u64::from(x))),
        "hop" => {
            let (s, r, d) = k_hop_distances_masked(&t, None, &[hub], 2, Direction::Out);
            let mut rows: Vec<(u32, u32, u32)> = (0..s.len()).map(|i| (s[i], r[i], d[i])).collect();
            rows.sort_unstable();
            fnv(rows
                .iter()
                .map(|&(s, r, d)| (u64::from(s) << 32 | u64::from(r)) ^ (u64::from(d) << 56)))
        }
        "bfs" => fnv(bfs_distances(&t, hub, Direction::Both)
            .iter()
            .map(|&x| x as i64 as u64)),
        "louvain" => fnv(louvain(&t, None, 1.0, Some(1), false)
            .iter()
            .map(|&x| u64::from(x))),
        other => panic!("unknown --kernel {other}"),
    };
    let run = started.elapsed().as_secs_f64();
    let built =
        t.out().edge_ids.is_built() || t.incoming_if_built().is_some_and(|i| i.edge_ids.is_built());
    println!(
        "{kernel} ids={} edges={edges} nodes={}: build {build:.2} s, kernel {run:.2} s, \
         rss over input: after build {:.0} MiB, end {:.0} MiB; peak {:.0} MiB; \
         edge ids held: {built}; hash {hash:016x}",
        if late { "late" } else { "eager" },
        t.n_nodes(),
        after_build - base,
        status_mib("VmRSS:") - base,
        status_mib("VmHWM:"),
    );
}
