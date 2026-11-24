//! The threaded-layout spike (#503): `ursa-viz`'s layout
//! ([`ursa_viz::scene::LiveLayout`], `Fa2Multilevel` underneath) behind the
//! smallest wasm API that can time it, built twice from this one source:
//!
//! - without the `threads` feature, on the workspace's stable toolchain:
//!   the layout as the shell ships it, one thread;
//! - with it, on a pinned nightly with `+atomics,+bulk-memory` and a rebuilt
//!   `std`: `ursa-core`'s kernels on rayon, whose workers are Web Workers
//!   over the module's one shared `WebAssembly.Memory`
//!   (`wasm-bindgen-rayon`).
//!
//! The same source also builds natively (`src/bin/bench.rs`), for the
//! native numbers and the determinism check.

use std::sync::Arc;

use ursa_core::algo::MultilevelParams;
use ursa_core::topology::UndirectedCsr;
use ursa_viz::scene::{Graph, LiveLayout};

/// xorshift64 → [0, 1), as `ursa-viz/demo/graphs.js` and `ursa-core`'s
/// `fa2_multilevel` example have it.
fn rng(seed: u64) -> impl FnMut() -> f64 {
    let mut s = seed;
    move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The benchmark's R-MAT graph: `1 << scale` nodes, `edges` edges.
pub fn rmat_edges(scale: u32, edges: usize) -> (Vec<u32>, Vec<u32>) {
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
    (src, dst)
}

/// A `w` by `h` grid: the structured graph, which coarsens into more levels
/// than R-MAT and costs several times as much an edge (`ursa-viz/BENCH.md`).
pub fn grid_edges(w: u32, h: u32) -> (Vec<u32>, Vec<u32>) {
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
    (src, dst)
}

/// The benchmark's graphs by name, as `(nodes, src, dst)`: R-MAT at `10k`,
/// `100k`, `1m` (`ursa-viz/BENCH.md`'s) and `5m` (the design's "5M edges and
/// 2M nodes"), and the grids `grid100k` and `grid1m`.
pub fn graph(name: &str) -> Option<(usize, Vec<u32>, Vec<u32>)> {
    let rmat = |scale: u32, edges: usize| {
        let (src, dst) = rmat_edges(scale, edges);
        Some((1usize << scale, src, dst))
    };
    let grid = |w: u32| {
        let (src, dst) = grid_edges(w, w);
        Some(((w * w) as usize, src, dst))
    };
    match name {
        "10k" => rmat(11, 10_000),
        "100k" => rmat(14, 100_000),
        "1m" => rmat(17, 1_000_000),
        "5m" => rmat(21, 5_000_000),
        "grid100k" => grid(224),
        "grid1m" => grid(707),
        _ => None,
    }
}

/// The graph's undirected adjacency, as `Layout::new` makes it in the shell.
pub fn adjacency(n_nodes: usize, src: Vec<u32>, dst: Vec<u32>) -> Arc<UndirectedCsr> {
    let graph = Graph::new(n_nodes, src, dst).expect("edges within the graph");
    Arc::clone(graph.adjacency())
}

/// The layout the shell runs: coarse to fine, default parameters.
pub fn layout(adjacency: Arc<UndirectedCsr>) -> LiveLayout {
    LiveLayout::multilevel_on(adjacency, MultilevelParams::default())
}

/// FNV-1a over the positions' bits, `x` then `y`: equal hashes are equal
/// bits (to one in 2^64).
pub fn hash(x: &[f32], y: &[f32]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for v in x.iter().chain(y) {
        for b in v.to_bits().to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

#[cfg(target_arch = "wasm32")]
pub mod web {
    use wasm_bindgen::prelude::*;

    /// `initThreadPool(n)`: start rayon's `n` workers. Await it before the
    /// first layout; it must be called from a worker, since a rayon caller
    /// blocks (`Atomics.wait`), which the page's thread may not.
    #[cfg(feature = "threads")]
    pub use wasm_bindgen_rayon::init_thread_pool;

    /// Whether this build has a pool.
    #[wasm_bindgen]
    pub fn threaded() -> bool {
        cfg!(feature = "threads")
    }

    /// The threads the kernels run on.
    #[wasm_bindgen]
    pub fn threads() -> u32 {
        #[cfg(feature = "threads")]
        return rayon::current_num_threads() as u32;
        #[cfg(not(feature = "threads"))]
        1
    }

    /// The benchmark's graph `name`: `[nodes, src..., dst...]`.
    #[wasm_bindgen]
    pub fn graph(name: &str) -> Result<Vec<u32>, JsValue> {
        let (n, src, dst) = super::graph(name).ok_or_else(|| JsValue::from_str("no such graph"))?;
        let mut out = Vec::with_capacity(1 + 2 * src.len());
        out.push(n as u32);
        out.extend_from_slice(&src);
        out.extend_from_slice(&dst);
        Ok(out)
    }

    /// `ursa-viz`'s `Layout`, cut down to what the measurement calls.
    #[wasm_bindgen]
    pub struct Layout {
        layout: super::LiveLayout,
    }

    #[wasm_bindgen]
    impl Layout {
        #[wasm_bindgen(constructor)]
        pub fn new(n_nodes: u32, src: Vec<u32>, dst: Vec<u32>) -> Layout {
            console_error_panic_hook::set_once();
            let adjacency = super::adjacency(n_nodes as usize, src, dst);
            Layout {
                layout: super::layout(adjacency),
            }
        }

        pub fn step(&mut self, iterations: u32) {
            self.layout.step(iterations);
        }

        #[wasm_bindgen(js_name = stepLevel)]
        pub fn step_level(&mut self, iterations: u32) {
            self.layout.step_level(iterations);
        }

        #[wasm_bindgen(getter)]
        pub fn done(&self) -> bool {
            self.layout.is_done()
        }

        #[wasm_bindgen(getter)]
        pub fn iterations(&self) -> u32 {
            self.layout.iterations()
        }

        #[wasm_bindgen(getter)]
        pub fn total(&self) -> Option<u32> {
            self.layout.total()
        }

        #[wasm_bindgen(getter)]
        pub fn levels(&self) -> u32 {
            self.layout.levels() as u32
        }

        /// `x` then `y`, as `ursa-viz`'s `Layout.positions` gives them.
        pub fn positions(&mut self) -> js_sys::Float32Array {
            self.layout.prolong();
            let (x, y) = self.layout.positions();
            let n = x.len() as u32;
            let out = js_sys::Float32Array::new_with_length(2 * n);
            out.subarray(0, n).copy_from(x);
            out.subarray(n, 2 * n).copy_from(y);
            out
        }

        /// [`crate::hash`] of the positions, in hex.
        #[wasm_bindgen(js_name = positionsHash)]
        pub fn positions_hash(&mut self) -> String {
            self.layout.prolong();
            let (x, y) = self.layout.positions();
            format!("{:016x}", super::hash(x, y))
        }
    }
}
