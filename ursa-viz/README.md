# ursa-viz

Ursa's graph view (#280): a graph drawn as its ForceAtlas2 layout converges,
with pan, zoom and picking. Bare by design for now: the work so far is the
capability, its performance and its correctness, not its looks.

## One view, any front end

The crate takes a graph as a node count and dense `src`/`dst` node indexes,
plus optional per-node values for size and colour. It knows nothing of SQL,
DataFusion or Python:

- the **SQL shell** maps a result's `src`/`dst` ids to dense indexes;
- the **DataFrame API** already has them in its topology.

Both can drive the same view: in the browser through the `web` feature, and,
later, in a notebook widget or a native window through the same `View`.

| module | what |
|---|---|
| `scene` | the graph, the live layout, node styles, the camera. No GPU; tested natively. |
| `render` | drawing with wgpu: WebGPU, else WebGL2, in the browser; Vulkan, Metal or DX12 natively. |
| `view` | a scene and its renderer, for a platform binding to drive; offscreen snapshots. |
| `web` (feature) | the wasm-bindgen API: `createView` for a canvas, `Layout` for a worker. |

## How it draws

- **Edges** are a line list: an index buffer of `src, dst` pairs into the node
  position buffers. The edges go to the GPU once; a layout step uploads only
  the positions, 8 bytes a node.
- **Nodes** are instanced quads shaded as discs of a radius in pixels, so they
  keep their size as the view zooms. When the nodes would cover more than 8%
  of the canvas (131K nodes on a laptop screen), every radius shrinks to fit,
  and eases back as the view zooms in.
- Positions are two vertex buffers, `x` and `y`, exactly as the layout holds
  them: no interleaving on upload. Everything is a vertex or index buffer,
  never a storage buffer, because WebGL2 has none.

## How it lays out

The layout is `ursa-core`'s ForceAtlas2 (`Fa2State`), stepped a few
iterations at a time: stepping 10 then 290 iterations gives the same bits as
`layout_fa2` with 300. In the browser it runs in a **Web Worker** (`Layout`),
which posts the positions after each batch (transferred, not copied), so a
slow iteration never holds up a frame: the page draws at the display's rate
whatever the layout costs.

## Measured

Headless Chromium on 4 container cores, release build (`wasm-release`):

| graph | nodes | edges | layout, ms per iteration (worker) |
|---|---|---|---|
| clusters | 4,000 | 24K | 5.5 |
| grid | 10,000 | 20K | 26 |
| R-MAT | 16K | 100K | 49 |
| R-MAT | 131K | 1M | 519 |

The same 1M-edge iteration takes about 330 ms natively on one core.

- **Correctness:** frames are read back from the GPU and checked, as the
  harness below does. On the clustered graph, the mean distance between nodes
  of one cluster is 0.07 of the mean between clusters.
- **Size:** the wasm is 4.0 MB raw, about 0.96 MB gzipped.
- **Draw cost:** this machine has no GPU, so its frame rates are not a
  measure; a laptop's GPU is.

## Run it

```sh
cargo build -p ursa-viz --features web --target wasm32-unknown-unknown --profile wasm-release
wasm-bindgen --target web --out-dir ursa-viz/pkg target/wasm32-unknown-unknown/wasm-release/ursa_viz.wasm
bunx serve ursa-viz      # then open /demo/?graph=rmat&scale=17&edges=1000000
```

The demo takes `?graph=rmat&scale=&edges=`, `?graph=grid&w=&h=` or
`?graph=clusters&k=&size=`, plus `&iterations=`. Drag to pan, wheel to zoom,
double-click to fit, click a node to pick it.

The browser harness runs the demo on each test graph, reads frames back and
checks them:

```sh
bun ursa-viz/tests/browser/view.ts [screenshot dir]   # WEBGPU=0 for WebGL2; ONLY=<name>
```

## Not yet

- **Converging large graphs in seconds.** Flat ForceAtlas2 needs hundreds of
  iterations, which at 1M edges is minutes in the browser. The next step is
  coarse to fine (multilevel, as sfdp and OpenOrd do): lay out a coarsened
  graph of a few thousand nodes, then refine at full size in far fewer
  iterations. That is also what straightens out a grid's folds, which flat
  ForceAtlas2 leaves.
- **Snapshots under headless WebGPU.** Reading a frame back works on WebGL2.
  Under headless Chromium's software WebGPU (SwiftShader), the map is
  rejected ("A valid external Instance reference no longer exists"), though
  plain WebGPU reads back there. To check on real hardware, and with wgpu 30,
  which needs wasm-bindgen 0.2.127 or later (the workspace is on 0.2.126).
- **The shell's Graph view:** results with `src`/`dst` columns, sized and
  coloured by their other columns.
- **The DataFrame API:** a notebook widget over the same view.
