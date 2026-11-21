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
| `highlight` | each node's flags (selected, pinned, hidden, …) and the highlighted edges. No GPU. |
| `live` | a result as the view draws it, kept in step with its deltas: which slots a delta changes. No GPU. |
| `render` (feature, on by default) | drawing with wgpu: WebGPU, else WebGL2, in the browser; Vulkan, Metal or DX12 natively. |
| `view` (feature `render`) | a scene and its renderer, for a platform binding to drive; offscreen snapshots. |
| `web` (feature) | the wasm-bindgen API: `createView` and `createViewFromResult` for a canvas (`applyDelta`), `Layout` for a worker (`step`, `grow`). |

With `default-features = false` the crate is `model`, `scene`, `highlight` and `live`, with
no wgpu in its dependencies: what an engine needs to produce a graph, and
nothing to draw one.

## The model: one result, every front end

`model` (#424, [the design](../design/graph-explorer.md) §3 and §6) is the
data everything drawn is made of. The engine will produce it and the shell,
the notebook widget and an exported page will read it. A view can be made
from a result and changed by its deltas ([below](#scenes-that-change-in-place));
the shell's `.graph` does not do so yet (#443).

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
- **Highlights** are a second layer (below): flags in a buffer of their own,
  and the highlighted edges as instanced quads over the line list.

## Highlights: paths, selection, pins, hidden nodes

A path, a selection or a hover is drawn by a layer of its own (#435, the
design's §4 and §6), so that pointing at something never uploads the graph.

Each node has a **flags** byte (`highlight::flag`, and `NodeFlag` in
JavaScript):

| bit | flag | drawn as |
|---|---|---|
| 1 | selected | a 2 px ring in the selection colour |
| 2 | hovered | a 1 px ring in the selection colour |
| 4 | pinned | a small ring in the pin colour, hugging the disc |
| 8 | dimmed | the node at 35%, whatever else is set |
| 16 | highlighted | a halo in the highlight colour |
| 32 | hidden | nothing: not the node, not its edges; it cannot be picked |
| 64 | seed | a ring in the highlight colour: a seed, or a route's end |

**Highlighted edges** are drawn wide (3 px, round ends) in the highlight
colour, over the other edges and under the nodes in front.

**While a highlight is set** (a node is highlighted or a seed, or an edge is
highlighted), everything else draws at 35%: the edges' ink that much
fainter, and the nodes that far from the background's colour to their own
(still opaque, so the edges under them do not show through). The nodes that
are selected, hovered, highlighted or a seed stay at full strength, are
drawn last, over everything, and are never smaller than 3 px in radius, so
a path shows in a graph whose nodes have shrunk to dots. A selection or a
hover alone dims nothing.

```js
view.setHighlights(offsets, nodes, seeds);  // UGR's highlights.paths.offsets, .nodes and highlights.seeds
view.clearHighlights();
view.setSelection(Uint32Array.of(33));      // exactly these nodes; an empty array clears
view.setHovered(view.pick(x, y));           // or undefined
view.setPinned(nodes); view.setHidden(nodes);
view.setFlags(flags);                       // or every node's byte at once (view.flags() reads them)
view.setHighlightEdges(pairs);              // and any edges: src, dst node indexes
```

`setHighlights` highlights the paths' nodes, draws the steps between their
consecutive nodes wide, and rings each path's two ends and each seed. In
Rust, `View::set_result_highlights(&GraphResult)` does the same from a
result's highlights (its paths' own edges, where it names them) and hides
its removed nodes; `View::set_highlight` takes any change to the
`HighlightState`.

How it is drawn, on WebGL2 as on WebGPU:

- The flags are a vertex buffer of their own, four bytes a node (the flags
  byte, then three unused: a vertex buffer's stride is a multiple of four).
  Changing flags uploads only the runs of nodes that changed: 4 bytes for a
  selection or a hover of one node (and 20 to draw it in front), never the
  styles or the edges.
- A highlighted edge is an instance of a quad: its two ends' positions, 16
  bytes, in a small buffer of their own, written again when the positions
  change (an instance cannot index the position buffers without a storage
  buffer). The line list's index buffer is not written for a highlight.
- The nodes in front are a small buffer too (position, radius, colour and
  flags, 20 bytes a node), drawn by the node shader after the highlighted
  edges; the pass over every node leaves them out by their flags. So a
  frame with a highlight draws a few more instances, not every node twice.
- The dimming is one uniform. The edges' shader reads the flags only while a
  node is hidden (a second pipeline), so a frame with nothing hidden costs
  what it did.

`ursa-viz/tests/browser/view.ts` reads frames back on WebGL2 in both themes
and checks all of it: the path's width and colour, the dimming, the rings,
the hidden node and the bytes uploaded. Under WebGPU, whose frames cannot be
read back headless, it checks that the same calls draw with no error from
the device.

## Scenes that change in place

A scene is a base result and the deltas applied to it (#441, the design's
§3 and §6). A view made from a result applies a delta **in time
proportional to the delta, not to the graph**: an expansion of 100 nodes
and 300 edges on a view of a million edges uploads 6 KB and takes a tenth
of a millisecond outside the frame ([`BENCH.md`](BENCH.md#deltas)).

```js
const view = await createViewFromResult(canvas, resultBytes);  // a GraphResult as UGR
const before = view.positions();                // an undo's positions: saved before the step
const applied = view.applyDelta(deltaBytes);    // a GraphDelta as UGR; throws, changing nothing, if it does not fit
applied.nodes; applied.edges;                   // the slots there are now
applied.bytesUploaded; applied.grew;            // what it wrote to the GPU, and whether a buffer doubled
view.info();                                    // what is shown: counts, what sizes the nodes, the colour legend

view.applyDelta(invertDelta(deltaBytes), before);  // undo: the inverse delta, and every node back where it was
```

In Rust, `View::from_result`, `View::apply(&GraphDelta)` and
`View::apply_at` (with positions), `View::info` and `View::result`.

What a delta does to what is drawn:

- **New nodes and edges take the slots after the last.** The node and edge
  buffers are append-only: they grow by doubling, what they held is copied
  on the GPU (`copyBufferSubData` on WebGL2; nothing is uploaded again),
  and a delta writes only its own slots: 16 bytes a node (position, radius,
  colour) and 4 of flags, 8 an edge. A view starts with buffers the size of
  its graph, so its first addition doubles them; a million-edge view then
  takes 48,000 more edges before the next.
- **A removed node keeps its slot, hidden by its flag**: 4 bytes. Its edges
  are left out by the pipeline that reads the flags (the highlight layer's,
  used only while a node is hidden), so none of them is written.
- **A removed edge keeps its slot as a line from its source to itself**,
  which draws nothing: 8 bytes, and the same to restore it.
- **Taking slots off the end** (a step undone) is a shorter draw range. It
  writes nothing.
- **Styles follow the result's encodings.** With `size_by`, a node's radius
  is its value's place in the column's range (3 to 14 px, by square root);
  without, its degree's (`1.5 + sqrt(degree)`, at most 12). With `color_by`,
  its colour is its category's in the theme's palette (a text column's
  values in order of appearance; a null gets the node colour). A delta
  restyles what it has to and writes only the nodes whose radius or colour
  came out different, in runs: the two ends of a new edge between nodes
  that were not neighbours, when nodes are sized by degree; every node,
  when a new node's value widens the column's range, or a column that
  sizes or colours is set.
- **Highlights and positions:** `SetHighlights` redraws the highlight layer
  from the result's paths and seeds. `SetPositions`, or the positions given
  with the delta, move every node (8 bytes a node, as a layout step does).
- **A new node the delta gives no position** starts beside a node a new
  edge joins it to, on a golden-angle spiral, so that it is drawn at once
  where it belongs; the layout (`Layout.grow`, then `setPositions`) then
  places it properly.

**Undo is the inverse delta** (`invertDelta(bytes)`, or
`GraphDelta::invert`), with no query: no index changes, and everything
drawn is a function of the result and the positions, so a delta and its
inverse leave the frame as it was, bit for bit. The positions are the
page's to keep: a layout moves the nodes after a step, so the page saves
`view.positions()` before each step and gives them back with the inverse.

`info()` follows every delta, for the status line and the legend:

```js
{ nodes: 37, edges: 83,                  // shown: not removed, and for an edge, neither end removed
  hiddenNodes: 0, hiddenEdges: 0,
  nodeSlots: 37, edgeSlots: 83,          // every slot: the next node's and the next edge's index
  size: { column: null, range: null },   // column null: by degree
  color: { column: "kind", entries: [{ category: 0, label: "a", color: "#2a78d6", count: 20 }, …], other: 0, none: 2 } }  // or null
```

The colour legend lists the twelve categories with the most shown nodes;
`other` counts the rest and `none` the nodes with no value. It is counted on
the first call after a delta (in time proportional to the graph) and kept.

How it is built: `live::LiveResult` holds the result and, for each delta,
works out which slots changed (`live::Changes`), with no GPU, so the
bookkeeping is tested natively: against a stand-in for the buffers that is
written only as `Changes` says, after every step of a run of random deltas
and undos, what it holds must be what a new view of the result would draw.
`View::apply` then writes those slots. Degrees are kept as edges come and
go, from the adjacency the view started with and a count of the pairs added
since, so a delta never builds the adjacency again; `scene::Graph` builds
its own again only when a layout on this thread next asks for it. The
model answers "does an edge still name one of the last nodes?" without
reading the edges, so undoing an expansion costs what the expansion did.

`ursa-viz/tests/browser/view.ts` reads frames back on WebGL2 and checks it
(`ONLY=delta`): a growth changes only the pixels around what it added,
though the buffers doubled under it; removed nodes and edges are gone; a
delta that does not fit changes nothing; and after each inverse the frame
is the first frame, byte for byte. For a page with no engine (the demo, the
tests) the wasm has `resultFromEdges` and a small `DeltaBuilder`.

## Colours

`setTheme({background, edge, node, palette, highlight, selection, pin})`
takes CSS hex colours, each optional: the background, the edges' ink (their
opacity is `edgeAlpha` times its alpha), the nodes' colour when not coloured
by category, the categories' colours (`colorBy(categories)` gives category
`c` the colour `palette[c % palette.length]`), and the highlight layer's:
`highlight` for highlighted edges, halos and a route's ends, `selection` for
the rings of selected and hovered nodes, `pin` for the pins' rings. The
built-in themes' three are at least 3:1 against their background (light:
`#d9480f`, `#0b63ce`, `#7a3fc0`; dark: `#ffa94d`, `#6cb2ff`, `#c9a3ff`). The view keeps the categories, so a new theme
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

## Growing a layout

A view that expands a node gains a few neighbours, and what is drawn must
not jump (#442). `LiveLayout::grow` (the worker's `Layout.grow`) takes the
grown graph, with the same node indexes and the new nodes after them:

```js
// 500 nodes drawn; node 33 gains 16 neighbours, nodes 500..515.
layout.grow(516, src, dst, parents /* Uint32Array(16).fill(33), or undefined */, pinned);
while (!layout.done) layout.step(10);   // 60 iterations in all; layout.total
```

- **Placement**: each new node starts on a golden-angle spiral around its
  parent, as prolongation places a group's members. The parent is the node
  named, or the new node's lowest-indexed neighbour already drawn, or, for a
  new node tied only to new ones, theirs. A new node tied to nothing goes on
  a ring around the drawing.
- **What is held**: pinned nodes are fixed (`Fa2State::set_fixed`): they do
  not move by a bit, and still repel and attract. So are the expanded nodes
  themselves, nodes with no edge, and old nodes more than two hops from a new
  one. The old nodes within two hops have ten times a new node's inertia
  (`Fa2State::set_inertia`), so they make room without rearranging. A drawing
  of more than 5,000 nodes is held whole. `WarmParams` has each of these as a
  knob.
- **Schedule**: 60 iterations of flat ForceAtlas2 for up to 100 new nodes
  (or a tenth of the drawing), 200 for more.
- **Cost**: the fixed nodes have a quadtree of their own, built once, and no
  force is computed for them, so an iteration costs what the free nodes do.
  On one thread, 100 new neighbours in a drawing of 131K nodes and 929K edges
  take 2 ms to place and 10 ms for all 60 iterations; rebuilding the
  adjacency for the grown graph takes 52 ms, and is the rest of the click
  ([`BENCH.md`](BENCH.md#growing-a-layout)).
- The same layout grown by the same additions in the same order has the same
  bits, on any thread count and however the iterations are split
  (`ursa-core/tests/determinism_threads.rs`). A layout that never grows has
  the bits it always had (`ursa-core/tests/layout_golden.rs`).

`LiveLayout::from_positions` (`Layout.fromPositions`) makes a layout from
positions already known (saved ones, another layout's, an undo's), to grow
from. `tests/grow.rs` holds the acceptance: expanding a node to degree 16 in
a 500-node view moves the old nodes 0.003% of the view's width on average
and 0.4% at most, and pinned nodes not at all.

What it does not do: keep discs from touching. ForceAtlas2 has no notion of
a node's size, and the leaves of one hub settle about a third as far apart
as the average pair of nodes in the drawing; the test bounds that ratio, not
pixels.

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

The demo takes `?graph=rmat&scale=&edges=`, `?graph=grid&w=&h=`,
`?graph=clusters&k=&size=` or `?graph=karate`, plus `&multilevel=0` for flat
ForceAtlas2 (and, flat, `&iterations=`), `&theme=dark` and
`&highlight=path` to start with a path highlighted. Drag to pan, wheel to
zoom, double-click to fit. The pointer's node is hovered; a click selects
it, shift-click adds to the selection, and two selected nodes draw the
shortest path between them. `p` pins the selection, `h` hides it, `u` shows
everything again and Escape clears. With `&result=1` the view is made from a
graph result, and once the layout is done `e` expands the selected node (one
delta: five new nodes and their edges) and `z` undoes the last expansion.

The browser harness runs the demo on each test graph, reads frames back and
checks them:

```sh
bun ursa-viz/tests/browser/view.ts [screenshot dir]   # WEBGPU=0 for WebGL2; ONLY=<name>; MODE=flat|multilevel
ONLY=highlight bun ursa-viz/tests/browser/view.ts     # the highlight layer alone (always on WebGL2)
ONLY=delta bun ursa-viz/tests/browser/view.ts         # deltas alone; ONLY="delta karate" without the 1M-edge timing
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
- **Growing from the shell.** `Layout.grow` is in the wasm API; the
  shell's layout worker does not call it yet (`{type: "grow"}`, with #443).
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
- **Highlights from the shell.** The layer and its API are in the view;
  `.graph`, `.path` and the pointer do not drive it yet (#433, #438).
- **Deltas from the shell.** `applyDelta` is in the view; `.graph` still
  makes its view from `src`/`dst` arrays and never changes it (#443, with
  the explore session that emits the deltas). `graph.js` keeps its own
  status line and legend until then; `info()` is what they will read.
- **A history in the view.** Undo is the inverse delta and the positions
  the page saved; the stack of steps is the explore session's (#443).
- **Room to grow from the start.** A view's buffers are the size of its
  graph, so the first addition doubles them (a copy on the GPU of what they
  held). Making them with headroom would move that cost to the first frame.
- **Edge styles.** A delta's edge columns are kept in the result; edges are
  all drawn alike, so nothing of them is uploaded.
- **Encodings in physical pixels.** A result's sizes are 3 to 14 px (or by
  degree, 1.5 to 12) whatever the device's pixel ratio; the shell scales its
  own by the ratio today.
- **Highlights' own widths and colours.** Every highlighted edge is 3 px in
  the one highlight colour (`Renderer::highlight_width` sets the width for
  all); a width or a colour per edge would be 8 more bytes an instance.
- **A group flag.** The design lists "group" among the per-node flags; it
  comes with groups (Phase 3). Three bytes a node are free for it.
- **The DataFrame API:** a notebook widget over the same view.
