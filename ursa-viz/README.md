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
| `model` | the graph-result model, deltas and the UGR wire format (below). No GPU. |
| `scene` | the graph, the live layout, node styles, the camera. No GPU; tested natively. |
| `render` (feature, on by default) | drawing with wgpu: WebGPU, else WebGL2, in the browser; Vulkan, Metal or DX12 natively. |
| `view` (feature `render`) | a scene and its renderer, for a platform binding to drive; offscreen snapshots. |
| `web` (feature) | the wasm-bindgen API: `createView` for a canvas, `Layout` for a worker. |

With `default-features = false` the crate is `model` and `scene` alone, with
no wgpu in its dependencies: what an engine needs to produce a graph, and
nothing to draw one.

## The model: one result, every front end

`model` (#424, [the design](../design/graph-explorer.md) §3 and §6) is the
data everything drawn is made of. The engine will produce it and the shell,
the notebook widget and an exported page will read it; the view above does
not use it yet (#426 and after).

- **`GraphResult`**: nodes, edges, their columns, highlights, encodings and
  provenance.
  - A node is a key in a label (a vertex table). The key is an integer or
    text, by its label, and `(label, key)` is its identity, so two tables may
    reuse a key.
  - An edge is `src`, `dst` (node indexes), a label (an edge table) and,
    where the engine knows it, its row in that table.
  - Columns are properties and metrics, one value a node or an edge: `f32`,
    `u32`, or text as a dictionary and codes.
  - Highlights are paths (ordered nodes, and the edges between), seeds, and a
    distance per node. Encodings say which columns size, colour and group,
    and the layout. Provenance is the SQL and the rule that drew it.
- **`GraphDelta`**: a change, as ops applied all or none: add nodes and
  edges, remove and restore them, set a column, the highlights, the
  encodings, the positions.
  - Indexes are append-only. New nodes and edges get new slots at the end; a
    removed one keeps its slot and its data, and only stops being drawn. So
    an index, once given out, stays good.
  - Every op carries its own inverse (`SetColumn` holds the column it
    replaces), and applies only to the state it was made for. So
    `delta.invert()` reads nothing, and `apply(&delta)` then
    `apply(&delta.invert())` restores the result bit for bit: undo is one
    step and no query.
- **UGR**, the wire format: `to_bytes` and `from_bytes` on both. A JSON
  header, then aligned little-endian arrays, which JavaScript reads as typed
  arrays over the buffer with no copy ([`ui/ugr.js`](ui/ugr.js)).
  [`UGR.md`](UGR.md) specifies it byte for byte. A million edges are 8.5 MB,
  8 bytes an edge plus the keys.
- **`Selection`**: the nodes and edges a user picked, by index.

```rust
use ursa_viz::model::*;

let base = GraphDelta::new(vec![
    Op::AddNodeLabel(NodeLabel::new("members", KeyType::Int)),
    Op::AddEdgeLabel("ties".into()),
    Op::AddNodes(NodeBatch::of_label(0, [1i64, 2, 3])),
    Op::AddEdges(EdgeBatch::of_label(0, vec![0, 1], vec![1, 2])),
]);
let mut result = GraphResult::build(Some("karate".into()), Provenance::default(), &base)?;
let bytes = result.to_bytes();                       // to the page, the widget, a file
assert_eq!(GraphResult::from_bytes(&bytes)?, result);

let step = GraphDelta::new(vec![Op::RemoveNodes(vec![1])]);
result.apply(&step)?;                                // node 1's edges are no longer shown
result.apply(&step.invert())?;                       // undo
assert_eq!(result.to_bytes(), bytes);
```

```js
import { readResult } from "./ui/ugr.js";
const { nodes, edges } = readResult(buffer);         // edges.src is a Uint32Array over `buffer`
```

Its tests: the model's invariants, every op and its inverse, and the format
(round trips, a truncated or malformed buffer refused with a message, any
changed byte never a panic) in `cargo test -p ursa-viz`; the JavaScript
reader against fixtures the Rust encoder wrote, in
`bun test ursa-viz/tests/ugr.test.ts`; and the size and speed at a million
edges in `tests/model_scale.rs`.

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

## Colours

`setTheme({background, edge, node, palette})` takes CSS hex colours, each
optional: the background, the edges' ink (their opacity is `edgeAlpha` times
its alpha), the nodes' colour when not coloured by category, and the
categories' colours (`colorBy(categories)` gives category `c` the colour
`palette[c % palette.length]`). The view keeps the categories, so a new theme
recolours them. `setThemeNamed("light" | "dark")` takes the built-in ones
(`Theme::light()`, `Theme::dark()`): ink on white, and light on near-black,
with an eight-hue palette (`PALETTE`, `DARK_PALETTE`: blue, orange, aqua,
yellow, magenta, green, violet, red) stepped for each, adjacent hues apart
under each kind of colour-vision deficiency. The shell's Graph view passes
its page's `--graph-*` tokens.

## How it lays out

Coarse to fine (`ursa-core`'s `Fa2Multilevel`), as sfdp and OpenOrd do it:
lay out a coarsened graph of a few dozen nodes, then each finer level from
there, in far fewer iterations of the full graph than flat ForceAtlas2 needs.

- **Coarsening** merges nodes, in index order, so the hierarchy is a pure
  function of the graph: heavy-edge matching (each node pairs with the
  neighbour of greatest edge weight over the product of their masses), then
  *hub absorption*: a node left unmatched, all of whose neighbours a maximal
  matching has taken (a star's leaves, an R-MAT hub's), joins the group of
  the neighbour it is most tied to. So a star collapses in one level, and a
  level at least halves the connected graph. A merged node's ForceAtlas2 mass
  is its members' summed `deg + 1`, and a merged edge weighs as many input
  edges as it stands for, so a coarse layout has the forces, and the scale, of
  the graph it stands for. Isolated nodes leave the hierarchy (in 2-D a ring's
  1/d forces cancel inside it, so they do not shape the rest) and come back on
  a ring just outside the drawing. Coarsening stops at 50 nodes.
- **Prolongation**: each node starts at its group's place, its group spread on
  a small golden-angle spiral (radius growing with the root of the group's
  size, scaled to the level's mean edge length).
- **Schedule**: 300 iterations on the coarsest level; on the graph itself,
  about 30M (nodes + adjacency entries) of work, between 10 and 200
  iterations (200 for the 4K- to 16K-node graphs, 15 at 1M edges); each level
  up, 1.5 times as many as the one below, up to 200. The coarse levels are
  cheap in nodes but not always in edges (R-MAT's stay dense), so the growth
  is what keeps them from costing more than the graph itself. Barnes–Hut
  `theta` is 1.0, not flat's 0.5: an iteration at 1M edges is 2.6 times
  faster, and the layout as good, since the levels above have placed the far
  field the approximation blurs.

It is stepped a budget at a time, a budget being worth some iterations of the
full graph, and every node has a position from the start: the current
level's, prolonged down. The schedule is fixed, so the end is the same bits
however the work is split, and on any thread count (`layout_fa2_multilevel`
in one call). `&multilevel=0` runs flat ForceAtlas2 (`Fa2State`) instead,
the same bits as `layout_fa2`.

In the browser it runs in a **Web Worker** (`Layout`), which posts the
positions after each batch of about 100 ms (transferred, not copied), so a
slow iteration never holds up a frame: the page draws at the display's rate
whatever the layout costs. `Layout`'s `level` and `levels` say how far it
is: coarse to fine counts `level` down from `levels - 1` to 0, the graph
itself.

## Measured

Time to a converged layout: coarse to fine to the end of its schedule; flat
until it is as good by the graph's own measure (the cluster ratio, the
grid's fold measure, else edge length over the mean distance between
nodes), measured by `ursa-core/examples/fa2_multilevel.rs` on one core,
release build:

| graph | nodes | edges | flat | coarse to fine |
|---|---|---|---|---|
| clusters | 4,000 | 24K | 1.5 s (270 it) | 0.9 s |
| grid | 10,000 | 20K | not in 1,500 it (23 s) | 2.0 s |
| R-MAT | 16K | 100K | 17 s (530 it) | 3.5 s |
| R-MAT | 131K | 1M | 51 s (160 it); settles by 500 it, 172 s | 5.0 s |

At 1M edges, coarse to fine is as good by edge length as flat after 160
iterations, and looks as flat does after 500: the global shape is the same,
but its edges are longer (mean edge length over mean distance, 0.69 against
flat's 0.50 after 600 iterations; see "Not yet").

In headless Chromium on 4 container cores, release build (`wasm-release`):

| graph | flat, ms per iteration | flat, to converge | coarse to fine |
|---|---|---|---|
| clusters | 7.5 | 2.0 s | 1.4 s |
| grid | 24 | not in 1,500 it (36 s) | 3.2 s |
| R-MAT 16K, 100K edges | 47 | 25 s | 5.3 s |
| R-MAT 131K, 1M edges | 525 | 84 s; settles in 4.4 min | 7.5 s |

Flat's time to converge is its time per iteration times the iterations the
native run needed. Coarse to fine's includes building the hierarchy (0.7 s
at 1M edges) and posting positions every 100 ms.

- **Quality:** on the clustered graph, the mean distance between nodes of
  one cluster is 0.07 of the mean between clusters, flat or coarse to fine.
  On the grid, the correlation of layout distance with grid distance is 0.98
  coarse to fine, an unfolded sheet; flat ForceAtlas2 leaves it folded (0.14
  after 300 iterations, 0.58 after 1,500). `ursa-core/tests/layout_multilevel.rs`
  checks both.
- **Correctness:** frames are read back from the GPU and checked, as the
  harness below does.
- **Size:** the wasm is 4.1 MB raw.
- **Draw cost:** this machine has no GPU, so its frame rates are not a
  measure; a laptop's GPU is.

## Run it

```sh
cargo build -p ursa-viz --features web --target wasm32-unknown-unknown --profile wasm-release
wasm-bindgen --target web --out-dir ursa-viz/pkg target/wasm32-unknown-unknown/wasm-release/ursa_viz.wasm
bunx serve ursa-viz      # then open /demo/?graph=rmat&scale=17&edges=1000000
```

The demo takes `?graph=rmat&scale=&edges=`, `?graph=grid&w=&h=` or
`?graph=clusters&k=&size=`, plus `&multilevel=0` for flat ForceAtlas2 and,
flat, `&iterations=`. Drag to pan, wheel to zoom,
double-click to fit, click a node to pick it.

The browser harness runs the demo on each test graph, reads frames back and
checks them:

```sh
bun ursa-viz/tests/browser/view.ts [screenshot dir]   # WEBGPU=0 for WebGL2; ONLY=<name>; MODE=flat|multilevel
```

## Benchmark

[`BENCH.md`](BENCH.md) has the baseline: time to first frame and to layout
done, frame time while panning and zooming, pick latency, memory and bytes
uploaded, on R-MAT graphs and grids of 1K to 1M edges, under WebGPU and
WebGL2, plus the layout alone natively. Not in CI; to rerun it (after the
build above), from `ursa-viz/`:

```sh
bun add --no-save playwright@1.63.0   # once, if Playwright is not installed
bun run bench                         # writes bench/results.json
SIZES=1k,10k BACKENDS=webgl2 NATIVE=0 bun run bench   # a quick subset
HEADED=1 CHROME=/path/to/chrome bun run bench         # a real GPU
```

## Not yet

- **A 1M-edge R-MAT graph fully converged in seconds.** Coarse to fine
  draws its shape in 7.5 s, and it looks as flat ForceAtlas2 does after
  500 iterations; but by edge length it is where flat is after about 170, as
  its finest level gets only 15 iterations. R-MAT's coarse levels stay dense
  (half the nodes, nearly all the edges), so they cost about what the graph
  does. Cheaper iterations (attraction gathers 2M random reads; repulsion
  could be approximated harder far out) would buy the rest.
- **The view's own layout.** `View::step`, for a page that lays out on its
  own thread, is still flat ForceAtlas2; the worker's `Layout` is coarse to
  fine.
- **Small components out past the ring.** A few nodes of R-MAT's small
  components end up far outside the ring of isolated nodes, as under flat
  ForceAtlas2, and fitting the view to them shrinks the drawing.
- **Snapshots under headless WebGPU.** Reading a frame back works on WebGL2.
  Under headless Chromium's software WebGPU (SwiftShader), the map is
  rejected ("A valid external Instance reference no longer exists"), though
  plain WebGPU reads back there. To check on real hardware, and with wgpu 30,
  which needs wasm-bindgen 0.2.127 or later (the workspace is on 0.2.126).
- **The DataFrame API:** a notebook widget over the same view.
