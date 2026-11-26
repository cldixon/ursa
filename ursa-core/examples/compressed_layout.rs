//! What the compressed layout costs and saves (#173): one graph, its index
//! built as the SQL engine builds one (nodes relabelled by in-degree, edge
//! rows spilled so the edge ids are built only when read), in the plain or
//! the compressed layout, then every kernel in turn.
//!
//! ```text
//! cargo run --release -p ursa-core --example compressed_layout -- \
//!     --graph rmat --edges 16000000 --layout compressed
//! ```
//!
//! Graphs: `rmat` (Graph500 quadrants, seed 42, `edges / 16` node slots, in
//! the order generated) and `sorted` (the same edges, node ids shuffled and
//! the rows sorted by source: the shape of an anonymised export, such as
//! the Bluesky snapshots). Prints, one line each: the build (wall time,
//! bytes per edge of the targets and of the whole out-adjacency, resident
//! memory over the input, peak), then per kernel its wall time, the peak
//! resident memory while it ran (`VmHWM`, reset before each), and a hash of
//! its result. `--kernels a,b,c` runs a subset; `closeness` and full
//! `betweenness` are for small graphs (`--edges 1000000`).

use std::sync::Arc;
use std::time::Instant;

use ursa_core::algo::{
    betweenness, bfs_distances, closeness_directed, connected_components_strong,
    connected_components_weak, k_hop_distances_masked, label_propagation, louvain, pagerank,
    shortest_path, triangle_count, PageRankParams,
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

/// The R-MAT edges with node ids shuffled and the rows sorted by source.
fn sorted(edges: usize, scale: u32) -> (Vec<u32>, Vec<u32>) {
    let (src, dst) = rmat(edges, scale, 42);
    let n = 1usize << scale;
    let mut perm: Vec<u32> = (0..n as u32).collect();
    let mut st = 7u64;
    for i in (1..n).rev() {
        let j = (next(&mut st) % (i as u64 + 1)) as usize;
        perm.swap(i, j);
    }
    let mut rows: Vec<u64> = src
        .iter()
        .zip(&dst)
        .map(|(&s, &d)| u64::from(perm[s as usize]) << 32 | u64::from(perm[d as usize]))
        .collect();
    drop((src, dst));
    rows.sort_unstable_by_key(|r| r >> 32);
    rows.iter().map(|&r| ((r >> 32) as u32, r as u32)).unzip()
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

/// Reset the peak resident memory (`VmHWM`) to the current.
fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

fn fnv(words: impl Iterator<Item = u64>) -> u64 {
    words.fold(0xCBF2_9CE4_8422_2325, |h, w| {
        (h ^ w).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

fn arg(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    Some(args.get(i + 1).expect("a value").clone())
}

fn spill(rows: &[u32]) -> Option<Arc<dyn RowSource>> {
    #[cfg(unix)]
    return Some(Arc::new(
        ursa_core::topology::SpilledRows::write(rows).expect("a temporary file"),
    ));
    #[cfg(not(unix))]
    {
        let _ = rows;
        None
    }
}

const KERNELS: &str = "transpose,pagerank,wcc,scc,hop,bfs,shortest_path,triangles,louvain,\
                       label_propagation,betweenness";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let graph = arg(&args, "--graph").unwrap_or_else(|| "rmat".into());
    let edges: usize =
        arg(&args, "--edges").map_or(16_000_000, |v| v.replace('_', "").parse().unwrap());
    let compressed = match arg(&args, "--layout").as_deref() {
        None | Some("plain") => false,
        Some("compressed") => true,
        Some(other) => panic!("--layout is plain or compressed, not {other}"),
    };
    let kernels = arg(&args, "--kernels").unwrap_or_else(|| KERNELS.into());
    let scale = ((edges / 16) as f64).log2().round() as u32;
    let (src, dst) = match graph.as_str() {
        "rmat" => rmat(edges, scale, 42),
        "sorted" => sorted(edges, scale),
        other => panic!("--graph is rmat or sorted, not {other}"),
    };
    let input = status_mib("VmRSS:");
    reset_peak();

    // The engine's build (`relabelled` in ursa-engine's exec.rs).
    let started = Instant::now();
    let targets = compressed.then(|| spill(&dst)).flatten();
    let t = Topology::build_with_rows(1 << scale, src, dst, NodeOrder::InDegree, &mut |s| spill(s));
    let plain_build = started.elapsed().as_secs_f64();
    let t = if compressed { t.compressed(targets) } else { t };
    let build = started.elapsed().as_secs_f64();
    let m = t.n_edges() as f64;
    let out = t.out();
    let target_bytes = match &out.packed {
        Some(p) => p.bytes.len(),
        None => 4 * out.targets.len(),
    };
    println!(
        "{graph} {} edges={edges} nodes={}: build {build:.2} s (plain part {plain_build:.2} s); \
         targets {:.2} B/edge, out-adjacency {:.2} B/edge; rss over input {:.0} MiB; \
         peak {:.0} MiB",
        if compressed { "compressed" } else { "plain" },
        t.n_nodes(),
        target_bytes as f64 / m,
        out.heap_bytes() as f64 / m,
        status_mib("VmRSS:") - input,
        status_mib("VmHWM:"),
    );

    let params = PageRankParams {
        damping: 0.85,
        max_iter: 20,
        tol: 0.0,
    };
    let hub = (0..t.n_nodes() as u32)
        .max_by_key(|&u| (t.out().degree(u), std::cmp::Reverse(u)))
        .unwrap_or(0);
    // The node farthest from the hub along out-edges (no transpose yet).
    let far = {
        let d = bfs_distances(&t, hub, Direction::Out);
        (0..d.len()).max_by_key(|&v| (d[v], v)).unwrap_or(0) as u32
    };
    for kernel in kernels.split(',') {
        reset_peak();
        let before = status_mib("VmRSS:");
        let started = Instant::now();
        let hash = match kernel {
            "transpose" => {
                let inc = t.incoming();
                inc.heap_bytes() as u64
            }
            "pagerank" => fnv(pagerank(&t, None, params).iter().map(|x| x.to_bits())),
            "wcc" => fnv(connected_components_weak(&t, None)
                .iter()
                .map(|&x| u64::from(x))),
            "scc" => fnv(connected_components_strong(&t, None)
                .iter()
                .map(|&x| u64::from(x))),
            "hop" => {
                let (s, r, d) = k_hop_distances_masked(&t, None, &[hub], 2, Direction::Out);
                let mut rows: Vec<(u32, u32, u32)> =
                    (0..s.len()).map(|i| (s[i], r[i], d[i])).collect();
                rows.sort_unstable();
                fnv(rows
                    .iter()
                    .map(|&(s, r, d)| (u64::from(s) << 32 | u64::from(r)) ^ (u64::from(d) << 56)))
            }
            "bfs" => fnv(bfs_distances(&t, hub, Direction::Both)
                .iter()
                .map(|&x| x as i64 as u64)),
            "shortest_path" => {
                shortest_path(&t, far, hub, Direction::Both).map_or(0, |p| p.len() as u64)
            }
            "triangles" => fnv(triangle_count(&t, None).iter().map(|&x| u64::from(x))),
            "louvain" => fnv(louvain(&t, None, 1.0, Some(1), false)
                .iter()
                .map(|&x| u64::from(x))),
            "label_propagation" => fnv(label_propagation(&t, None, 20, Some(1), false)
                .iter()
                .map(|&x| u64::from(x))),
            // 32 sampled sources: the kernel's own sampling.
            "betweenness" => {
                let fraction = (32.0 / t.n_nodes() as f64).min(1.0);
                fnv(betweenness(&t, None, Some(fraction), Some(7))
                    .iter()
                    .map(|x| (x * 1e6).round() as u64))
            }
            "betweenness_full" => fnv(betweenness(&t, None, None, None)
                .iter()
                .map(|x| (x * 1e6).round() as u64)),
            "closeness" => fnv(closeness_directed(&t, None, Direction::Out)
                .iter()
                .map(|x| x.to_bits())),
            other => panic!("unknown kernel {other}"),
        };
        let run = started.elapsed().as_secs_f64();
        let extra = if kernel == "transpose" {
            format!(", in-adjacency {:.2} B/edge", hash as f64 / m)
        } else {
            String::new()
        };
        println!(
            "  {kernel}: {run:.3} s, peak {:.0} MiB (+{:.0} over before){extra}; hash {hash:016x}",
            status_mib("VmHWM:"),
            status_mib("VmHWM:") - before,
        );
    }
    let ids_built =
        t.out().edge_ids.is_built() || t.incoming_if_built().is_some_and(|i| i.edge_ids.is_built());
    println!("  edge ids built: {ids_built}");
}
