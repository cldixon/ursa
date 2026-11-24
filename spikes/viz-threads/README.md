# Threaded layout spike (#503)

A time-boxed measurement, not a product: `ursa-viz`'s layout
(`Fa2Multilevel`, through `ursa_viz::scene::LiveLayout`) on a pool of Web
Workers over one shared `WebAssembly.Memory`. The report, with the numbers
and the recommendation, is `design/threaded-layout-spike.md`.

Nothing here is in the release build. This directory is a cargo workspace of
its own with its own `Cargo.lock`; the root workspace does not list it; the
nightly toolchain is named only on `build.sh`'s command line; and the one CI
job that builds it (`.github/workflows/viz-threads-spike.yml`) runs only when
this directory changes, or by hand.

| | |
|---|---|
| `src/lib.rs` | the layout behind the smallest wasm API that can time it; the `threads` feature turns on `ursa-core`'s rayon kernels and `wasm-bindgen-rayon`'s pool |
| `src/bin/bench.rs` | the same layout natively |
| `build.sh` | the two wasm builds (`web/pkg-serial` on stable, `web/pkg-threads` on the pinned nightly with `-Z build-std`) and the native benches |
| `web/pool.js` | which build a worker loads: threaded only where `crossOriginIsolated`, else the unthreaded one |
| `web/worker.js`, `web/page.js` | the layout worker and page of the measurement |
| `bench.ts` | `check`: the determinism check; `bench`: the measurement |
| `results/` | the runs the report quotes |

```sh
./build.sh toolchain     # once: the pinned nightly, with rust-src
./build.sh               # both wasm builds and the native benches
bun add --no-save playwright@1.63.0
bun bench.ts check       # the same bits at every thread count, or exit 1
SIZES=100k,1m THREADS=1,2,4 REPEATS=3 bun bench.ts bench
```

`CHROME=/path/to/chrome` uses that browser and not Playwright's Chromium.
Set `CARGO_TARGET_DIR` as for any build here; the script keeps its four
builds apart under it (about 1.5 GB in all).
