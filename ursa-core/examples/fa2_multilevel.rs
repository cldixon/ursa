//! Flat against coarse-to-fine ForceAtlas2 on the graph view's test graphs
//! (the ones `ursa-viz/demo/graphs.js` makes, generated the same way): time to
//! converge, and how good the result is.
//!
//! ```sh
//! cargo run -p ursa-core --release --example fa2_multilevel [clusters|grid|rmat14|rmat17] [flat iterations]
//! ```
//!
//! `ML_COARSEST`, `ML_TOP`, `ML_WORK`, `ML_GROWTH`, `ML_MIN`, `ML_MAX` and
//! `ML_THETA` override the [`MultilevelParams`]; `DUMP=dir` writes the
//! positions out.
//!
//! Build with `--no-default-features` for one core, as the browser runs it.
//!
//! `bench [sizes] [graphs]` instead times the layout alone on the graph view
//! benchmark's graphs (`ursa-viz/tests/browser/bench.ts` runs it), and prints
//! JSON: for each graph, the hierarchy, coarse to fine in one call, and flat
//! ForceAtlas2's time per iteration. Sizes are `1k,10k,100k,1m` (edges),
//! graphs `rmat,grid`:
//!
//! ```sh
//! cargo run -p ursa-core --release --example fa2_multilevel -- bench 1k,10k,100k,1m rmat,grid
//! ```
//!
//! For each graph it prints, every few iterations of flat ForceAtlas2 and over
//! the multilevel schedule:
//!
//! - **move**: the mean distance a node moved in the last iteration, over the
//!   mean edge length.
//! - **edge/pair**: mean edge length over the mean distance between random
//!   pairs of nodes with a neighbour: how much closer neighbours are than
//!   strangers. Lower is better.
//! - **clusters** (clustered graph): mean distance within a cluster over mean
//!   distance between clusters, as the browser harness measures it.
//! - **grid r** (grid): correlation between layout distance and grid distance
//!   over sampled pairs. An unfolded sheet is near 1; a folded one is not.
//!
//! Then flat ForceAtlas2 runs until it is as good as the multilevel layout by
//! the graph's own measure (the cluster ratio, the grid's r, else
//! edge/pair), and says how long that took. Times are for the layout steps
//! alone, not the measuring.
use std::time::Instant;

use ursa_core::algo::{Fa2Multilevel, Fa2Params, Fa2State, MultilevelParams};
use ursa_core::topology::{Topology, UndirectedCsr};

/// xorshift64 → [0, 1), as `graphs.js` has it.
fn rng(seed: u64) -> impl FnMut() -> f64 {
    let mut s = seed;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn rmat(scale: u32, edges: usize) -> Topology {
    let mut next = rng(42);
    let (mut src, mut dst) = (Vec::with_capacity(edges), Vec::with_capacity(edges));
    for _ in 0..edges {
        let (mut u, mut v) = (0u32, 0u32);
        for bit in (0..scale).rev() {
            let r = next();
            if r < 0.57 {
            } else if r < 0.76 {
                v |= 1 << bit;
            } else if r < 0.95 {
                u |= 1 << bit;
            } else {
                u |= 1 << bit;
                v |= 1 << bit;
            }
        }
        src.push(u);
        dst.push(v);
    }
    Topology::build(1 << scale, src, dst)
}

fn grid(w: u32, h: u32) -> Topology {
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if x + 1 < w {
                src.push(i);
                dst.push(i + 1);
            }
            if y + 1 < h {
                src.push(i);
                dst.push(i + w);
            }
        }
    }
    Topology::build((w * h) as usize, src, dst)
}

fn clusters(k: u32, size: u32) -> (Topology, Vec<u32>) {
    let mut next = rng(7);
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    let n = k * size;
    for c in 0..k {
        for i in 0..size {
            let a = c * size + i;
            for _ in 0..6 {
                src.push(a);
                dst.push(c * size + (next() * size as f64) as u32);
            }
            if next() < 0.1 {
                src.push(a);
                dst.push((next() * n as f64) as u32);
            }
        }
    }
    let community = (0..n).map(|i| i / size).collect();
    (Topology::build(n as usize, src, dst), community)
}

enum Extra {
    None,
    Clusters(Vec<u32>),
    Grid(u32),
}

fn edge_pair(adj: &UndirectedCsr, x: &[f32], y: &[f32]) -> (f64, f64) {
    let n = x.len();
    let d = |i: usize, j: usize| f64::from(((x[i] - x[j]).powi(2) + (y[i] - y[j]).powi(2)).sqrt());
    let (mut es, mut en) = (0.0, 0u64);
    for i in 0..n {
        for &j in adj.neighbors(i as u32) {
            es += d(i, j as usize);
            en += 1;
        }
    }
    // Pairs of nodes with a neighbour: isolated nodes are pushed out to a
    // ring around the drawing, and would weigh on the mean for nothing.
    let linked: Vec<usize> = (0..n).filter(|&i| adj.degree(i as u32) > 0).collect();
    let mut next = rng(99);
    let (mut ps, pn) = (0.0, 20_000);
    for _ in 0..pn {
        let i = linked[(next() * linked.len() as f64) as usize];
        let j = linked[(next() * linked.len() as f64) as usize];
        ps += d(i, j);
    }
    let edge = es / en.max(1) as f64;
    (edge, edge / (ps / pn as f64))
}

fn quality(extra: &Extra, x: &[f32], y: &[f32]) -> String {
    let n = x.len();
    let d = |i: usize, j: usize| f64::from(((x[i] - x[j]).powi(2) + (y[i] - y[j]).powi(2)).sqrt());
    match extra {
        Extra::None => String::new(),
        Extra::Clusters(c) => {
            let (mut is, mut inn, mut os, mut on) = (0.0, 0u64, 0.0, 0u64);
            for i in (0..n).step_by(7) {
                for j in (i + 3..n).step_by(11) {
                    if c[i] == c[j] {
                        is += d(i, j);
                        inn += 1;
                    } else {
                        os += d(i, j);
                        on += 1;
                    }
                }
            }
            format!(" clusters {:.3}", (is / inn as f64) / (os / on as f64))
        }
        Extra::Grid(w) => format!(" grid r {:.3}", grid_r(*w as usize, x, y)),
    }
}

/// Pearson correlation of layout distance and grid distance, sampled pairs.
fn grid_r(w: usize, x: &[f32], y: &[f32]) -> f64 {
    let n = x.len();
    let mut next = rng(5);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for _ in 0..20_000 {
        let (i, j) = ((next() * n as f64) as usize, (next() * n as f64) as usize);
        let (gx, gy) = (
            (i % w) as f64 - (j % w) as f64,
            (i / w) as f64 - (j / w) as f64,
        );
        a.push((gx * gx + gy * gy).sqrt());
        b.push(f64::from(
            ((x[i] - x[j]).powi(2) + (y[i] - y[j]).powi(2)).sqrt(),
        ));
    }
    let m = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / m, b.iter().sum::<f64>() / m);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for k in 0..a.len() {
        sab += (a[k] - ma) * (b[k] - mb);
        saa += (a[k] - ma).powi(2);
        sbb += (b[k] - mb).powi(2);
    }
    sab / (saa * sbb).sqrt()
}

fn moved(px: &[f32], py: &[f32], x: &[f32], y: &[f32]) -> f64 {
    let n = x.len();
    let mut s = 0.0f64;
    for i in 0..n {
        s += f64::from(((x[i] - px[i]).powi(2) + (y[i] - py[i]).powi(2)).sqrt());
    }
    s / n as f64
}

/// With `DUMP=dir`, write positions as little-endian f32 `x` then `y`, and
/// the edges as `u32` pairs, for drawing elsewhere.
fn dump(name: &str, x: &[f32], y: &[f32]) {
    if let Ok(dir) = std::env::var("DUMP") {
        let mut out = Vec::with_capacity(x.len() * 8);
        for v in x.iter().chain(y) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(format!("{dir}/{name}.f32"), out).expect("write positions");
    }
}

fn dump_edges(name: &str, adj: &UndirectedCsr) {
    if let Ok(dir) = std::env::var("DUMP") {
        let mut out = Vec::new();
        for i in 0..adj.offsets.len() - 1 {
            for &j in adj.neighbors(i as u32) {
                if (i as u32) < j {
                    out.extend_from_slice(&(i as u32).to_le_bytes());
                    out.extend_from_slice(&j.to_le_bytes());
                }
            }
        }
        std::fs::write(format!("{dir}/{name}-edges.u32"), out).expect("write edges");
    }
}

/// The bench's graphs, as `ursa-viz/tests/browser/bench.ts` names them.
fn bench_graph(graph: &str, size: &str) -> Topology {
    match (graph, size) {
        ("rmat", "1k") => rmat(7, 1_000),
        ("rmat", "10k") => rmat(11, 10_000),
        ("rmat", "100k") => rmat(14, 100_000),
        ("rmat", "1m") => rmat(17, 1_000_000),
        ("grid", "1k") => grid(23, 23),
        ("grid", "10k") => grid(71, 71),
        ("grid", "100k") => grid(224, 224),
        ("grid", "1m") => grid(707, 707),
        other => panic!("no bench graph {other:?}"),
    }
}

/// `bench`: the layout alone, timed, as JSON on stdout.
fn bench(sizes: &str, graphs: &str) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let parallel = cfg!(feature = "rayon");
    let mut rows = Vec::new();
    for graph in graphs.split(',') {
        for size in sizes.split(',') {
            let topo = bench_graph(graph, size);
            let adj = topo.undirected();
            let params = MultilevelParams::default();
            let t = Instant::now();
            let ml = Fa2Multilevel::new(adj, params);
            let hierarchy = t.elapsed().as_secs_f64();
            let (levels, total) = (ml.level_sizes().len(), ml.total_work());
            drop(ml);
            let t = Instant::now();
            let one = ursa_core::algo::layout_fa2_multilevel(&topo, None, params);
            let multilevel = t.elapsed().as_secs_f64();
            std::hint::black_box(one);
            // Flat: enough iterations for a steady time, at most ~2 s.
            let mut flat = Fa2State::new(adj, Fa2Params::default());
            let t = Instant::now();
            flat.step(adj, 1);
            let first = t.elapsed().as_secs_f64();
            let its = ((2.0 / first.max(1e-6)) as u32).clamp(1, 100);
            let t = Instant::now();
            flat.step(adj, its);
            let per = t.elapsed().as_secs_f64() / f64::from(its);
            eprintln!(
                "native {graph} {size}: coarse to fine {multilevel:.3} s, flat {:.2} ms/it",
                per * 1e3
            );
            rows.push(format!(
                "{{\"graph\":\"{graph}\",\"size\":\"{size}\",\"nodes\":{},\"edges\":{},\"levels\":{levels},\"totalWork\":{total:.1},\"hierarchyMs\":{:.1},\"multilevelMs\":{:.1},\"flatMsPerIteration\":{:.3},\"flatIterationsTimed\":{its}}}",
                topo.n_nodes(),
                adj.targets.len() / 2,
                hierarchy * 1e3,
                multilevel * 1e3,
                per * 1e3,
            ));
        }
    }
    println!(
        "{{\"rayon\":{parallel},\"threads\":{},\"graphs\":[{}]}}",
        if parallel { threads } else { 1 },
        rows.join(",")
    );
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("bench") {
        let arg = |i: usize, d: &str| std::env::args().nth(i).unwrap_or_else(|| d.into());
        bench(&arg(2, "1k,10k,100k,1m"), &arg(3, "rmat,grid"));
        return;
    }
    let which = std::env::args().nth(1).unwrap_or_else(|| "clusters".into());
    let flat_max: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let (topo, extra) = match which.as_str() {
        "clusters" => {
            let (t, c) = clusters(8, 500);
            (t, Extra::Clusters(c))
        }
        "grid" => (grid(100, 100), Extra::Grid(100)),
        "rmat14" => (rmat(14, 100_000), Extra::None),
        "rmat17" => (rmat(17, 1_000_000), Extra::None),
        other => panic!("unknown graph {other}"),
    };
    let adj = topo.undirected();
    dump_edges(&which, adj);
    println!(
        "{which}: {} nodes, {} undirected edges",
        topo.n_nodes(),
        adj.targets.len() / 2,
    );
    let report = |label: &str, it: u32, secs: f64, mv: f64, x: &[f32], y: &[f32]| {
        let (edge, ep) = edge_pair(adj, x, y);
        println!(
            "  {label} {it:>5} it {secs:>7.2} s  move {:.4}  edge/pair {ep:.4}{}",
            mv / edge,
            quality(&extra, x, y)
        );
        mv / edge
    };

    // Multilevel.
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
    let mut params = MultilevelParams::default();
    if let Some(v) = env("ML_COARSEST") {
        params.coarsest_nodes = v as usize;
    }
    if let Some(v) = env("ML_TOP") {
        params.coarsest_iterations = v;
    }
    if let Some(v) = std::env::var("ML_THETA")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        params.fa2.theta = v;
    }
    let envf = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
    if let Some(v) = envf("ML_WORK") {
        params.finest_work = v;
    }
    if let Some(v) = envf("ML_GROWTH") {
        params.level_growth = v;
    }
    if let Some(v) = env("ML_MAX") {
        params.max_iterations = v;
    }
    if let Some(v) = env("ML_MIN") {
        params.min_iterations = v;
    }
    let t = Instant::now();
    let mut ml = Fa2Multilevel::new(adj, params);
    let build = t.elapsed().as_secs_f64();
    println!(
        "  multilevel: levels {:?}, entries {:?}, schedule {:?}, total work {:.1} full iterations, hierarchy {:.3} s",
        ml.level_sizes(),
        ml.level_entries(adj),
        ml.schedule(),
        ml.total_work(),
        build
    );
    {
        // What drawing every level costs: a step with no budget only brings
        // the input graph's positions up to date.
        let ts = Instant::now();
        for _ in 0..10 {
            ml.step(adj, 0.0);
        }
        println!(
            "  prolonging to every node: {:.2} ms",
            ts.elapsed().as_secs_f64() * 100.0
        );
    }
    let mut level = ml.level();
    let mut spent = build;
    let mut last_level_time = spent;
    while !ml.is_done() {
        let (px, py) = {
            let (x, y) = ml.positions();
            (x.to_vec(), y.to_vec())
        };
        let ts = Instant::now();
        ml.step(adj, 1.0);
        spent += ts.elapsed().as_secs_f64();
        if ml.level() != level || ml.is_done() {
            let el = spent;
            let (x, y) = ml.positions();
            let (_, ep) = edge_pair(adj, x, y);
            println!(
                "    level {level} done at {el:.3} s ({:.3} s) edge/pair {ep:.4}{}",
                el - last_level_time,
                quality(&extra, x, y)
            );
            last_level_time = el;
            level = ml.level();
        }
        if ml.level() == 0 {
            let (x, y) = ml.positions();
            let mv = moved(&px, &py, x, y);
            let done = ml.is_done();
            let it = ml.work_done().round() as u32;
            if done || it.is_multiple_of(10) {
                report("multilevel", it, spent, mv, x, y);
            }
        }
    }
    println!("  multilevel, stepped: {spent:.2} s");
    let t = Instant::now();
    let one = ursa_core::algo::layout_fa2_multilevel(&topo, None, params);
    println!(
        "  layout_fa2_multilevel, one call: {:.2} s",
        t.elapsed().as_secs_f64()
    );
    let _ = report("one-shot  ", 0, 0.0, 0.0, &one.0, &one.1);
    dump(&format!("{which}-multilevel"), &one.0, &one.1);

    // Flat, until it is as good as multilevel by the graph's measure.
    let target = score(&extra, adj, &one.0, &one.1);
    let mut spent = 0.0;
    let mut flat = Fa2State::new(adj, Fa2Params::default());
    let mut reached = None;
    while flat.iterations() < flat_max {
        let (px, py) = {
            let (x, y) = flat.positions();
            (x.to_vec(), y.to_vec())
        };
        let ts = Instant::now();
        flat.step(adj, 1);
        spent += ts.elapsed().as_secs_f64();
        let it = flat.iterations();
        let (x, y) = flat.positions();
        if reached.is_none() && it.is_multiple_of(10) && score(&extra, adj, x, y) <= target {
            reached = Some((it, spent));
        }
        if it.is_multiple_of(100) {
            dump(&format!("{which}-flat{it}"), x, y);
        }
        if it.is_multiple_of(50) || it <= 10 && it.is_multiple_of(5) {
            report("flat      ", it, spent, moved(&px, &py, x, y), x, y);
        }
    }
    match reached {
        Some((it, secs)) => println!(
            "  flat as good as multilevel ({target:.4}) after {it} iterations, {secs:.2} s"
        ),
        None => println!("  flat not as good as multilevel ({target:.4}) in {flat_max} iterations"),
    }
}

/// The graph's quality measure as a score, lower better: the cluster ratio,
/// the grid's correlation negated, else edge/pair.
fn score(extra: &Extra, adj: &UndirectedCsr, x: &[f32], y: &[f32]) -> f64 {
    match extra {
        Extra::Clusters(_) | Extra::Grid(_) => {
            let q = quality(extra, x, y);
            let v: f64 = q.split_whitespace().last().unwrap().parse().unwrap();
            if matches!(extra, Extra::Grid(_)) {
                -v
            } else {
                v
            }
        }
        Extra::None => edge_pair(adj, x, y).1,
    }
}
