//! A node order for locality, measured (#172): build one graph in an order,
//! then time one kernel on it, best of `--reps` runs.
//!
//! ```text
//! cargo run --release -p ursa-core --example locality_order -- \
//!     --graph rmat --edges 16000000 --order degree --kernel pagerank
//! ```
//!
//! **Graphs.** `--graph rmat` is the Graph500 R-MAT generator (quadrants 0.57,
//! 0.19, 0.19, 0.05; `edges / 16` node slots), from seeded streams. `--graph
//! skewed` is the same edges with the node ids shuffled and the rows sorted by
//! source, as an export of anonymised ids sorted by its first column is (the
//! Bluesky follows graph, #172). Either way the endpoints are then numbered in
//! first-seen order, as the SQL engine's index build numbers them.
//!
//! **Orders.** `first-seen` builds as the engine did before #172 (no relabel);
//! `degree` is [`NodeOrder::InDegree`], the default since; `out-degree` and
//! `total-degree` sort by those degrees instead; `bfs` numbers the nodes in
//! breadth-first order over the undirected graph from the node of highest
//! degree (then from each node not yet reached, in id order); `rcm` is reverse
//! Cuthill–McKee (breadth-first from a node of least degree, neighbours by
//! increasing degree, the order reversed). The last four are computed here,
//! serially, over a first-seen build, so their build time is that build plus
//! the order plus a relabelled build.
//!
//! **Kernels.** `none` (the build alone), `pagerank` (20 iterations, `tol = 0`,
//! per iteration), `triangles`, `wcc`, `scc`, `louvain`, `label-propagation`
//! (20 sweeps), `hop` (2 steps out from the hub), `bfs` (from 3 sources, both
//! directions) and `closeness` (exact, out-edges). `--parallel` takes the
//! parallel form of Louvain and label propagation. The transpose (and, for
//! triangles, the undirected view) is built before the clock starts, as a
//! stored index has it.
//!
//! Every result is printed as a hash over the nodes in original-id order, so
//! the orders can be checked to agree: exactly for counts, labels and
//! partitions (a partition is hashed by its canonical labels), and for floats
//! after rounding to nine significant digits. Run it under `/usr/bin/time -v`
//! for the peak resident memory.

use std::time::Instant;

use ursa_core::algo::{
    bfs_distances, closeness_directed, connected_components_strong, connected_components_weak,
    k_hop_distances_masked, label_propagation, louvain, pagerank, triangle_count, PageRankParams,
};
use ursa_core::{Direction, NodeOrder, Topology};

/// splitmix64.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `edges` R-MAT edges over `2^scale` node slots, generated in parallel
/// chunks of 2^20 edges, each from its own seeded stream.
fn rmat(edges: usize, scale: u32) -> (Vec<u32>, Vec<u32>) {
    use rayon::prelude::*;
    const CHUNK: usize = 1 << 20;
    let (a, ab, abc) = (0.57, 0.76, 0.95);
    let pairs: Vec<Vec<(u32, u32)>> = (0..edges.div_ceil(CHUNK))
        .into_par_iter()
        .map(|c| {
            let mut st = 42 ^ (c as u64).wrapping_mul(0xA24B_AED4_963E_E407);
            let len = CHUNK.min(edges - c * CHUNK);
            (0..len)
                .map(|_| {
                    let (mut s, mut d) = (0u32, 0u32);
                    for _ in 0..scale {
                        let r = (next(&mut st) >> 11) as f64 / (1u64 << 53) as f64;
                        s = s << 1 | u32::from(r >= ab);
                        d = d << 1 | u32::from((r >= a && r < ab) || r >= abc);
                    }
                    (s, d)
                })
                .collect()
        })
        .collect();
    pairs.into_iter().flatten().unzip()
}

/// Shuffle the ids of `src`/`dst` over `n` slots, then sort the rows by
/// source (stable), as an anonymised export sorted by its first column.
fn skew(n: usize, src: &mut Vec<u32>, dst: &mut Vec<u32>) {
    use rayon::prelude::*;
    let mut perm: Vec<u32> = (0..n as u32).collect();
    let mut st = 7u64;
    for i in (1..n).rev() {
        let j = (next(&mut st) % (i as u64 + 1)) as usize;
        perm.swap(i, j);
    }
    let mut rows: Vec<(u32, u32)> = src
        .par_iter()
        .zip(dst.par_iter())
        .map(|(&s, &d)| (perm[s as usize], perm[d as usize]))
        .collect();
    rows.par_sort_by_key(|&(s, _)| s);
    (*src, *dst) = rows.into_iter().unzip();
}

/// Number the endpoints in first-seen order (source before destination, row
/// by row), as the engine's interner does; returns the node count.
fn first_seen(src: &mut [u32], dst: &mut [u32], slots: usize) -> usize {
    let mut id = vec![u32::MAX; slots];
    let mut n = 0u32;
    for (s, d) in src.iter_mut().zip(dst.iter_mut()) {
        for x in [s, d] {
            let slot = &mut id[*x as usize];
            if *slot == u32::MAX {
                *slot = n;
                n += 1;
            }
            *x = *slot;
        }
    }
    n as usize
}

/// The undirected degree of every node.
fn total_degree(t: &Topology) -> Vec<u64> {
    let (out, inc) = (t.out(), t.incoming());
    (0..t.n_nodes() as u32)
        .map(|v| u64::from(out.degree(v)) + u64::from(inc.degree(v)))
        .collect()
}

/// Nodes sorted by `key` descending, ties by id: the original id of each new
/// node.
fn by_key(key: &[u64]) -> Vec<u32> {
    let mut nodes: Vec<u32> = (0..key.len() as u32).collect();
    nodes.sort_by_key(|&v| (std::cmp::Reverse(key[v as usize]), v));
    nodes
}

/// Breadth-first order over the undirected graph, from each start in turn
/// (skipping reached ones); `sorted` visits each node's neighbours by
/// increasing degree (Cuthill–McKee).
fn bfs_order(t: &Topology, starts: &[u32], degree: &[u64], sorted: bool) -> Vec<u32> {
    let n = t.n_nodes();
    let mut seen = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut scratch = Vec::new();
    for &s in starts {
        if seen[s as usize] {
            continue;
        }
        seen[s as usize] = true;
        let mut head = order.len();
        order.push(s);
        while head < order.len() {
            let u = order[head];
            head += 1;
            scratch.clear();
            t.for_each_neighbor(u, Direction::Both, None, |v| {
                if !seen[v as usize] {
                    seen[v as usize] = true;
                    scratch.push(v);
                }
            });
            if sorted {
                scratch.sort_by_key(|&v| (degree[v as usize], v));
            }
            order.extend_from_slice(&scratch);
        }
    }
    order
}

fn arg(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    Some(args.get(i + 1).expect("a value").replace('_', ""))
}

/// CPU seconds this process has used, on every thread (Linux).
fn cpu_seconds() -> f64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return f64::NAN;
    };
    let rest = stat.rsplit_once(')').map_or("", |r| r.1);
    let mut fields = rest.split_whitespace().skip(11);
    let mut tick = || fields.next().and_then(|f| f.parse::<f64>().ok());
    match (tick(), tick()) {
        (Some(u), Some(s)) => (u + s) / 100.0,
        _ => f64::NAN,
    }
}

/// FNV-1a over 64-bit words.
fn fnv(words: impl Iterator<Item = u64>) -> u64 {
    words.fold(0xCBF2_9CE4_8422_2325, |h, w| {
        (h ^ w).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// A float rounded to nine significant digits, as bits.
fn round9(x: f64) -> u64 {
    if x == 0.0 || !x.is_finite() {
        return x.to_bits();
    }
    let e = 8 - x.abs().log10().floor() as i32;
    let p = 10f64.powi(e);
    ((x * p).round() / p).to_bits()
}

/// Per-node values in original-id order.
fn in_original<T: Copy>(t: &Topology, values: &[T]) -> Vec<T> {
    (0..t.n_nodes() as u32)
        .map(|o| values[t.node_of_original(o) as usize])
        .collect()
}

/// A partition's hash, by labels made canonical (the smallest original id
/// of each part), and its part count.
fn partition(t: &Topology, labels: &[u32]) -> String {
    let labels = in_original(t, labels);
    let mut least = std::collections::HashMap::new();
    let canon: Vec<u64> = labels
        .iter()
        .enumerate()
        .map(|(o, &l)| *least.entry(l).or_insert(o as u64))
        .collect();
    format!(
        "{} parts, hash {:016x}",
        least.len(),
        fnv(canon.into_iter())
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let number = |name: &str| arg(&args, name).map(|v| v.parse::<usize>().expect("a number"));
    let graph = arg(&args, "--graph").unwrap_or_else(|| "rmat".into());
    let order = arg(&args, "--order").unwrap_or_else(|| "degree".into());
    let kernel = arg(&args, "--kernel").unwrap_or_else(|| "none".into());
    let edges = number("--edges").unwrap_or(16_000_000);
    let reps = number("--reps").unwrap_or(3);
    let parallel = args.iter().any(|a| a == "--parallel");
    let scale = ((edges / 16).max(2) as f64).log2().round() as u32;

    let (mut src, mut dst) = rmat(edges, scale);
    if graph == "skewed" {
        skew(1 << scale, &mut src, &mut dst);
    } else {
        assert_eq!(graph, "rmat", "--graph is rmat or skewed");
    }
    let n = first_seen(&mut src, &mut dst, 1 << scale);

    let (build_start, build_cpu) = (Instant::now(), cpu_seconds());
    let t = match order.as_str() {
        "first-seen" => Topology::build(n, src, dst),
        "degree" => Topology::build_ordered(n, src, dst, NodeOrder::InDegree),
        other => {
            let first = Topology::build(n, src.clone(), dst.clone());
            let original = match other {
                "out-degree" => {
                    let out = first.out();
                    let key: Vec<u64> = (0..n as u32).map(|v| u64::from(out.degree(v))).collect();
                    by_key(&key)
                }
                "total-degree" => by_key(&total_degree(&first)),
                "bfs" | "rcm" => {
                    let degree = total_degree(&first);
                    let mut starts: Vec<u32> = (0..n as u32).collect();
                    if other == "bfs" {
                        starts.sort_by_key(|&v| (std::cmp::Reverse(degree[v as usize]), v));
                    } else {
                        starts.sort_by_key(|&v| (degree[v as usize], v));
                    }
                    // Hub (or least) first, the rest in id order.
                    let lead = starts[0];
                    let mut rest: Vec<u32> = (0..n as u32).filter(|&v| v != lead).collect();
                    rest.insert(0, lead);
                    let mut o = bfs_order(&first, &rest, &degree, other == "rcm");
                    if other == "rcm" {
                        o.reverse();
                    }
                    o
                }
                _ => panic!("unknown --order {other}"),
            };
            drop(first);
            Topology::build_relabelled(n, src, dst, original)
        }
    };
    let (build_wall, build_cpu) = (
        build_start.elapsed().as_secs_f64(),
        cpu_seconds() - build_cpu,
    );
    t.incoming();
    if kernel == "triangles" {
        t.undirected();
    }
    let hub = (0..n as u32)
        .max_by_key(|&v| (t.out().degree(v), std::cmp::Reverse(t.original_id(v))))
        .unwrap_or(0);

    let mut best = (f64::INFINITY, f64::INFINITY);
    let mut result = String::new();
    for _ in 0..reps.max(1) {
        let (start, cpu) = (Instant::now(), cpu_seconds());
        result = match kernel.as_str() {
            "none" => String::new(),
            "pagerank" => {
                let params = PageRankParams {
                    max_iter: 20,
                    tol: 0.0,
                    ..PageRankParams::default()
                };
                let pr = pagerank(&t, None, params);
                let pr = in_original(&t, &pr);
                format!("hash {:016x}", fnv(pr.iter().map(|&x| round9(x))))
            }
            "triangles" => {
                let c = in_original(&t, &triangle_count(&t, None));
                let total: u64 = c.iter().map(|&x| u64::from(x)).sum();
                format!(
                    "{} triangles, hash {:016x}",
                    total / 3,
                    fnv(c.iter().map(|&x| u64::from(x)))
                )
            }
            "wcc" => partition(&t, &connected_components_weak(&t, None)),
            "scc" => partition(&t, &connected_components_strong(&t, None)),
            "louvain" => partition(&t, &louvain(&t, None, 1.0, Some(1), parallel)),
            "label-propagation" => {
                partition(&t, &label_propagation(&t, None, 20, Some(1), parallel))
            }
            "hop" => {
                let (s, r, d) = k_hop_distances_masked(&t, None, &[hub], 2, Direction::Out);
                let mut rows: Vec<(u32, u32, u32)> = (0..s.len())
                    .map(|i| (t.original_id(s[i]), t.original_id(r[i]), d[i]))
                    .collect();
                rows.sort_unstable();
                let h = fnv(rows
                    .iter()
                    .map(|&(s, r, d)| (u64::from(s) << 32 | u64::from(r)) ^ (u64::from(d) << 56)));
                format!("{} rows, hash {h:016x}", rows.len())
            }
            "bfs" => {
                let sources = [hub, t.node_of_original(1), t.node_of_original(n as u32 / 2)];
                let d: Vec<i32> = sources
                    .iter()
                    .flat_map(|&s| in_original(&t, &bfs_distances(&t, s, Direction::Both)))
                    .collect();
                let reached = d.iter().filter(|&&x| x > 0).count();
                format!(
                    "{reached} reached, hash {:016x}",
                    fnv(d.iter().map(|&x| x as i64 as u64))
                )
            }
            "closeness" => {
                let c = in_original(&t, &closeness_directed(&t, None, Direction::Out));
                format!("hash {:016x}", fnv(c.iter().map(|&x| round9(x))))
            }
            other => panic!("unknown --kernel {other}"),
        };
        let (wall, cpu) = (start.elapsed().as_secs_f64(), cpu_seconds() - cpu);
        if wall < best.0 {
            best = (wall, cpu);
        }
    }
    let per = if kernel == "pagerank" { 20.0 } else { 1.0 };
    println!(
        "{graph} {edges} edges {n} nodes, order {order}, kernel {kernel}{}: build {build_wall:.3} s wall {build_cpu:.2} CPU-s; \
         kernel best of {reps} {:.4} s wall {:.2} CPU-s{}; {result}",
        if parallel { " (parallel)" } else { "" },
        best.0 / per,
        best.1 / per,
        if per > 1.0 { " per iteration" } else { "" },
    );
}
