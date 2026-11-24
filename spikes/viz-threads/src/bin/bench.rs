//! The layout natively, timed, one JSON line a run:
//!
//! ```sh
//! cargo run --release --features threads --bin bench -- 100k,1m 1,2,4 [repeats]
//! cargo run --release --bin bench -- 100k,1m          # no rayon: the serial kernels
//! ```
use std::time::Instant;

use ursa_viz_threads::{adjacency, graph, hash, layout};

fn run(name: &str) -> (f64, f64, u32, u64) {
    let (n, src, dst) = graph(name).unwrap_or_else(|| panic!("no graph {name}"));
    let t = Instant::now();
    let mut l = layout(adjacency(n, src, dst));
    let build = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    while !l.is_done() {
        l.step(4);
    }
    let ms = t.elapsed().as_secs_f64() * 1e3;
    l.prolong();
    let (x, y) = l.positions();
    (build, ms, l.iterations(), hash(x, y))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sizes = args.first().map_or("100k", String::as_str);
    let threads = args.get(1).map_or("1", String::as_str);
    let repeats: usize = args.get(2).map_or(1, |r| r.parse().expect("repeats"));
    for name in sizes.split(',') {
        for t in threads.split(',') {
            let t: usize = t.parse().expect("threads");
            for _ in 0..repeats {
                #[cfg(feature = "threads")]
                let (build, ms, iterations, h) = rayon::ThreadPoolBuilder::new()
                    .num_threads(t)
                    .build()
                    .expect("a pool")
                    .install(|| run(name));
                #[cfg(not(feature = "threads"))]
                let (build, ms, iterations, h) = run(name);
                println!(
                    "{{\"where\":\"native\",\"build\":\"{}\",\"graph\":\"{name}\",\"threads\":{},\"buildMs\":{build:.1},\"layoutMs\":{ms:.1},\"iterations\":{iterations},\"hash\":\"{h:016x}\"}}",
                    if cfg!(feature = "threads") { "threads" } else { "serial" },
                    if cfg!(feature = "threads") { t } else { 1 },
                );
            }
            if cfg!(not(feature = "threads")) {
                break;
            }
        }
    }
}
