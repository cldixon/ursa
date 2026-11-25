# Threaded layout: the spike's report (#503)

Part of the Graph Explorer design ([`graph-explorer.md`](graph-explorer.md) §6, "Threads, instances and memory"; decision 7 of §11). The question: is a threaded build of the layout, on a pool of workers over one shared `WebAssembly.Memory`, worth carrying?

**Recommendation: yes, as an optional second build of the layout that the release never depends on.** On four cores it lays out 2.0 to 2.3 times faster in Chromium, with the same bits as the unthreaded build at every thread count, for 33 KB more gzipped and about 25 ms more start-up. The price is a pinned nightly toolchain for that one artifact, for as long as it is carried. Three things this spike could not measure qualify the yes; they are listed under [What was not measured](#what-was-not-measured), and the follow-up issue starts with them.

The spike is `spikes/viz-threads/` (its `README.md` says how to run it). Nothing in the release build changed.

## What was built

- **One source, two wasm builds.** `spikes/viz-threads` is a small crate over `ursa_viz::scene::LiveLayout` (the layout as the shell steps it, `Fa2Multilevel` underneath) with the few methods a measurement calls.
  - *Unthreaded:* the workspace's stable 1.97, no atomics, `ursa-core` without `rayon`. This is the layout as the shell ships it.
  - *Threaded:* `--features threads` on `nightly-2026-11-01`, `-C target-feature=+atomics,+bulk-memory`, `-Z build-std=std,panic_abort`. `ursa-core`'s `rayon` feature is on, and rayon's workers are Web Workers started by `wasm-bindgen-rayon` 1.3 over the module's shared memory.
- **No kernel code changed.** `ursa-core`'s layout already runs on rayon natively with the same bits as its serial path (`ursa-core/src/parallel.rs`, `tests/determinism_threads.rs`). The threaded build turns that feature on for wasm; `ursa-core` and `ursa-viz` are untouched.
- **The choice of build** (`web/pool.js`): the threaded build only where `crossOriginIsolated` is true and `SharedArrayBuffer` exists, on every core but one, at most 8. Otherwise, or if the pool fails to start, the unthreaded build.
- **Kept out of the release.** The crate is a cargo workspace of its own with its own `Cargo.lock`. The nightly is named only on `build.sh`'s command line; there is no `rust-toolchain` file. One CI job builds it (`.github/workflows/viz-threads-spike.yml`), on changes to the spike's own paths or by hand.

## Measurements

**The machine:** Intel Xeon @ 2.10 GHz, 4 cores, 16 GB, Linux 6.18; headless Chrome for Testing 153. The page is served with the shell's own security headers (`web/src/headers.ts`: its CSP, COOP, COEP).

**The load:** the machine was shared with three other agents' builds throughout (load average 2 to 8, at times 15). So each timed run waited for the machine to go idle and recorded how many cores were busy while it ran. A run is *clean* if that was no more than its own threads plus half a core. Each cell below is the median of the clean runs, with the best run in brackets, and (clean/all) runs. Where no run was clean the cell says so. The raw runs are in `spikes/viz-threads/results/`.

**The graphs:** `ursa-viz/BENCH.md`'s R-MAT graphs at 100K and 1M edges, R-MAT at 5M edges and 2M nodes (the design's large case), and the grids at 100K and 1M edges. Layout time is `Layout.step` until done, coarse to fine, default parameters.

### Layout time in Chromium

| graph | unthreaded | 1 thread | 2 threads | 4 threads | 4 threads, speed-up |
|---|---|---|---|---|---|
| R-MAT 100K | 5.72 s (5.37; 2/3) | 5.92 s (1/3) | 3.64 s (3.52; 2/3) | 2.45 s (2.30; 3/3) | **2.3×** |
| R-MAT 1M | 7.88 s (7.71; 2/3) | 8.24 s (8.16; 2/3) | 5.16 s (5.11; 2/3) | 4.03 s (3.44; 3/3) | **2.0×** (best 2.3×) |
| R-MAT 5M | 119 s (116; none clean) | not run | 74.8 s (72.8; none clean) | 53.2 s (39.8; 2/2) | **2.2×** (best 2.9×) |
| grid 100K | 15.7 s (15.6; 2/3) | 17.6 s (1/3) | 10.4 s (9.6; 2/3) | 7.59 s (5.96; 3/3) | **2.1×** (best 2.6×) |
| grid 1M | 32.1 s (1/2) | 41.9 s (none clean) | 22.7 s (19.6; none clean) | 17.3 s (14.9; 2/2) | **1.9×** (best 2.2×) |

- **Four threads halve the layout time, on every graph and size.** Two threads give about 1.5 times.
- **One thread of the threaded build is 4 to 12% slower than the unthreaded build** (atomic instructions, rayon's hand-offs). A pool of one is never worth using: where there is one core, the unthreaded build runs.
- **About three of the four cores were busy at four threads** (2.7 to 3.4 measured). The serial part of each iteration (the quadtree's build, applying the forces) is what caps the speed-up, as it does natively.
- **The 5M and grid 1M rows are weaker evidence**: their unthreaded runs are long, so other agents' work landed on most of them.
- **The design's target** (§9: layout stable at 1M edges in "≤3 s with threads") **is not met on this machine**: 3.4 to 4.0 s. See [What was not measured](#what-was-not-measured).

### Eight threads

This machine has four cores, so eight threads could only be run oversubscribed. That is not the 8-core measurement the issue asks for.

| graph | 4 threads | 8 threads on 4 cores |
|---|---|---|
| R-MAT 100K | 2.45 s | 2.73 s |
| R-MAT 1M | 4.03 s | 4.01 s |
| grid 100K | 7.59 s | 6.91 s |

Asking for more workers than cores costs little or nothing in layout time, 8 MB more memory and 15 to 25 ms more start-up.

### Native, for comparison

The same crate, natively: `ursa-core` without rayon, and with rayon on a pool of each size.

| graph | serial | 1 thread | 2 threads | 4 threads | speed-up |
|---|---|---|---|---|---|
| R-MAT 100K | 3.66 s (1/3) | 3.68 s (1/3) | 2.69 s (2.19; none clean) | 1.61 s (1.45; 3/3) | 2.3× |
| R-MAT 1M | 5.30 s (1/3) | 5.21 s (1/3) | 3.07 s (1/3) | 2.38 s (2.31; 3/3) | 2.2× |

- **The threaded wasm build scales as native rayon does** (2.0 to 2.3 times against 2.2 to 2.3 here). The pool of Web Workers loses nothing measurable to its wake-ups.
- wasm stays about 1.5 times slower than native at every thread count, as `BENCH.md` found for one thread.
- `BENCH.md` measured 2.7 to 2.9 times natively on four quiet cores. So a quiet machine should do somewhat better than these tables.

### Bundle size

The layout alone (no renderer), glue included:

| build | wasm | glue (JS) | raw | gzip -9 |
|---|---|---|---|---|
| unthreaded | 112,527 | 13,139 | 125,666 | 48,062 |
| threaded | 193,495 | 20,502 (with the pool's helper) | 213,997 | 80,820 |
| difference | | | **+88 KB** | **+33 KB** |

For scale: the shell's graph view (`ursa_viz`, with wgpu) is 2.89 MB raw and 0.98 MB gzipped, and the layout worker loads that whole module today. A page would fetch the threaded layout only when it is going to use it.

### Start-up cost

From the worker's first line to a layout that can be made, in clean runs:

| | fetch, compile, instantiate | start the pool | total |
|---|---|---|---|
| unthreaded | 6–11 ms | – | 6–11 ms |
| threaded, 1 worker | 8–12 ms | 11–14 ms | 19–23 ms |
| threaded, 2 workers | 8–10 ms | 13–17 ms | 21–28 ms |
| threaded, 4 workers | 8–9 ms | 21–25 ms | 29–34 ms |
| threaded, 8 workers (4 cores) | 8–10 ms | 36–48 ms | 44–58 ms |

- **About 25 ms more at four workers**, once per layout worker. Each worker instantiates the already-compiled module on the shared memory; nothing is compiled twice.
- Under load the pool took up to 100 ms (518 ms once, for eight workers with the machine's load at 7).
- **Memory:** the threaded build's memory was 3 MB larger than the unthreaded build's at 100K edges, plus about 2 MB a worker (9, 11, 15 and 23 MB at 1, 2, 4 and 8 workers against 6 MB). At 1M edges it was 66 MB against 57 MB at four workers, and at 5M edges 407 MB against 399 MB.
- A shared memory must declare its maximum. The build declares 4 GB, as the unthreaded build can grow to; the browser reserves that address space, not the memory.

### Determinism

**The positions are the same bits as the unthreaded build's at every thread count.** `bun bench.ts check` compares a hash of the positions' bits:

- in Chromium, the unthreaded build against the threaded build on pools of 1, 2, 3, 4 and 8 workers, each run twice, on R-MAT 10K and 100K;
- natively, the serial kernels against rayon on the same pool sizes.

All agree. So did every timed run above: 1M, 5M and both grids gave one hash each across builds and thread counts. The native and wasm builds also agree with each other on the three R-MAT graphs both ran (10K, 100K, 1M), which nothing requires.

The check also serves the page without COOP and COEP, and confirms that the unthreaded build then runs even when the threaded one is asked for. It fails on any console warning or error, so it also covers the CSP.

## What broke

1. **The pool never started under the shell's CSP, silently.** `wasm-bindgen-rayon` 1.3 starts each worker from a `blob:` URL (a copy of its helper, so that a CDN can host it). The shell's `worker-src 'self'` refuses that. Nothing is logged where the page can see it: `initThreadPool` never resolves. `build.sh` patches the generated helper to start workers from its own same-origin URL, and stops with an error if the helper's text has changed. The alternative is `worker-src 'self' blob:`, which this spike did not take: the CSP is not the spike's to loosen.
2. **The link line needs a flag the crate's README does not list.** With the README's flags (`--shared-memory`, `--import-memory`, `--max-memory`, and the exports `__wasm_init_tls`, `__tls_size`, `__tls_align`, `__tls_base`), `wasm-bindgen` 0.2.126 fails with `failed to find __heap_base for injecting thread id`. Exporting `__heap_base` too fixes it. Which of these flags are needed was not narrowed down further; what a given nightly passes by itself has changed before and will again.
3. **The helper calls the glue's initializer the old way**, which `wasm-bindgen` 0.2.126 warns about on every worker. Patched in `build.sh` too.
4. **The hierarchy is built more slowly on threads for the grid at 1M edges**: 240 to 310 ms unthreaded, about 600 ms on two threads and 900 to 1,100 ms on four, in three separate sessions. Natively rayon makes it faster (107 to 75 ms in `BENCH.md`), and for R-MAT the threaded wasm build is faster too (1M: about 590 to 490 ms; 5M: 4.6–5.1 to 3.8–4.3 s). The cause was not found. The coarsening allocates a scratch array per split and a `Vec` per row, and the threaded build's allocator has one lock, so that is the first thing to look at. This matters: the hierarchy is the time to the first frame, which #525 is cutting.
5. **The spike's own `Cargo.lock` went stale within a day.** It records the versions of the path dependencies, so main's bump of `ursa-core` and `ursa-viz` to 0.13.0-dev made `cargo build --locked` fail here until the lock was updated. A crate outside the root workspace will break this way at every release, and its CI job, which runs only on the spike's own paths, will not notice. A productionised build belongs in the root workspace's lock.
6. **Nothing else.** No change to `ursa-core` or `ursa-viz`, no deadlock, no difference in bits. `ursa_core::parallel` is private, so the crate asks rayon for its thread count itself.

## What it costs to maintain

- **A nightly, pinned by date, for one artifact.** `-Z build-std` is unstable and has no date for stabilising. Moving the pin means re-running the determinism check and re-reading items 1 to 3 of "What broke"; each of them is a place where a new nightly, a new `wasm-bindgen` or a new `wasm-bindgen-rayon` can break the build. The three versions move together: the `wasm-bindgen` crate and CLI must match exactly, as they already must for the shell.
- **The pin cannot simply be left alone.** The nightly must be new enough for the workspace's `rust-version` (1.94 today) and for whatever `ursa-core`'s dependencies need (arrow, hashbrown). Each bump of the workspace's stable toolchain may force a bump of the nightly.
- **A different compiler builds the two layouts.** The threaded artifact is compiled by rustc 1.101-nightly and the unthreaded one by 1.97. Their bits agree today; the determinism check is what says so, and it has to run on every change to either toolchain or to the layout.
- **Build time and disk:** the threaded build rebuilds `std` on every cold build or change of flags: 4 minutes cold here (on a loaded machine), 730 MB of target directory against 354 MB for the unthreaded build, and 790 MB for the nightly toolchain with `rust-src`.
- **A patch to a dependency's generated file** (item 1), which fails loudly when the dependency changes.
- **A second code path in the page**: choosing the build, starting the pool, falling back. In the spike it is `web/pool.js`, under 50 lines.
- **The CI job was written but has not run.** It triggers on a pull request that touches the spike's paths, or by hand once the workflow is on the default branch; this branch has no pull request. Its steps are the commands run here.

## What was not measured

- **Eight cores.** This machine has four. The issue asks for 4 and 8; the oversubscribed run above says nothing about real cores. By the measured serial share (about three cores busy of four), eight cores should give around 3 times, not 4 or more. That is an estimate, not a measurement.
- **Firefox and Safari.** Only Chromium 153 ran. Both support shared memory under cross-origin isolation, and `wasm-bindgen-rayon` documents both; neither was tried.
- **A quiet machine, and a laptop.** Every number here was taken on shared cores. The design's 3 s target at 1M edges was missed by 0.4 to 1.0 s on a 2.1 GHz server core; whether a laptop meets it is not known.
- **The real shell.** The spike has its own page and worker. It ran under the shell's headers, but the shell's layout worker (`graph-worker.js`: `stepLevel`, the cut, the edge order, `grow`) was not moved onto the pool.
- **Level-at-a-time stepping.** The timed loop is `step(4)`; the shell uses `stepLevel`. The kernels are the same, and `step` and `stepLevel` give the same bits, but the batch sizes differ.

## Recommendation

**Yes: carry a threaded build of the layout, as an optional artifact.**

- **For:** it halves the layout time on four cores, which is the one number in the design's targets that nothing else improves. It needs no change to the kernels. Determinism holds and is checked. The cost to a user is 33 KB and 25 ms. The fallback is the build the shell ships today, and every host that is not cross-origin isolated (notebooks, exported pages) keeps using it.
- **Against:** a nightly toolchain to maintain indefinitely, a patched dependency, and a speed-up that is 2 times, not 4. The shell is the only host that gains.
- **The condition that makes it a yes:** the threaded artifact must be optional at every level. If its build fails, the release ships without it and the page runs the unthreaded layout. The nightly must never be on the path of the engine, the view or the unthreaded layout. On those terms a broken nightly costs a slower layout, not a release.
- **When to stop carrying it:** if the 8-core measurement shows less than 2.5 times, or if keeping the pin working takes more than an occasional afternoon, drop it. The page loses nothing but speed.

The follow-up issue for productionising is in the spike's final report, to be filed by the maintainer.
