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
| `spatial` | a grid over the node positions, and one over the edges: a pick reads what is near the pointer, and a zoomed-in frame draws the nodes in view. No GPU. |
| `live` | a result as the view draws it, kept in step with its deltas: which slots a delta changes. No GPU. |
| `budget` | the frame budget: how much a frame draws while the view moves, from what the last frames cost. No GPU. |
| `order` | the edges' drawing order, most important first: what a prefix of them is. No GPU. |
| `pace` (feature `render`) | making a view in pieces, with a turn of the event loop between them. |
| `render` (feature, on by default) | drawing with wgpu: WebGPU, else WebGL2, in the browser; Vulkan, Metal or DX12 natively. |
| `view` (feature `render`) | a scene and its renderer, for a platform binding to drive; offscreen snapshots. |
| `web` (feature) | the wasm-bindgen API: `createView` and `createViewFromResult` for a canvas (`applyDelta`), `Layout` for a worker (`step`, `grow`, `rings`, `layers`, `edgeOrder`). |

With `default-features = false` the crate is `model`, `scene`, `highlight`, `interact`, `spatial` and `live`, with
no wgpu in its dependencies: what an engine needs to produce a graph, and
nothing to draw one.

## The model: one result, every front end

`model` (#424, [the design](../design/graph-explorer.md) §3 and §6) is the
data everything drawn is made of. The engine produces it
(`ursa_engine::graph::view`, #425: what `.graph` draws), and the web shell
reads it as UGR with `ui/ugr.js`; the notebook widget and an exported page
will read it the same way. A view can be made from a result and changed by
its deltas ([below](#scenes-that-change-in-place)); the shell does not do
so yet (#443): it hands the view the arrays it read.

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

## The frame budget

A frame has 16 ms. A million edges cost an integrated GPU more than that,
and a software renderer seconds (4 s here; the 131K nodes alone, 1.4 s). So
the view keeps to a budget by drawing less while it moves, and everything
when it rests (#495, the design's §5).

- **The edges are in importance order**, so that any prefix of the edge
  buffer is a backbone of the drawing and not a corner of it. The layout
  computes the order from its hierarchy (`LiveLayout::edge_order`,
  `Layout.edgeOrder(src, dst)` in the worker; `Fa2Multilevel::edge_order` in
  `ursa-core`), by:
  1. the level at which the edge's two ends merge, coarsest first: an edge
     whose ends are still apart at the coarsest level joins two of the
     drawing's far groups, and one whose ends merge at the first level is
     local fill;
  2. a round: each merged edge (the tie between two groups, just below that
     level) gives one of the input edges it stands for before any gives a
     second, so a short prefix shows every tie, not all of the heaviest;
  3. the merged edge's weight within a round, heaviest first;
  4. a hash of the edge's index.

  With no hierarchy (rings, layers, a flat or grown layout, a graph too
  small to coarsen) it is by the larger degree of the edge's two ends, then
  the hash. The order is a permutation, `order[i]` the edge to draw `i`-th;
  the view takes it with `setEdgeOrder(order)` and writes the edges to a new
  buffer a piece at a time (`orderEdges()`, until it returns `false`), the
  frames drawing the old buffer until the new one is whole. Every edge has
  the same ink, so a frame with all of them is the same pixels in any
  order. Edges added later (a delta) go after the ordered ones.
- **While the view moves** (the camera or the positions changed since the
  last frame, or less than 200 ms ago), `renderFrame(now)` draws the first
  `k` edges and, if the nodes are too many as well, the largest of them
  (gathered into a small buffer of their own, 20 bytes each: instances
  cannot be drawn by index). A budget is in units of one edge; a node is
  three (six vertices to an edge's two), and half the budget goes to the
  nodes until they are all drawn. The edges are drawn deeper to make up for
  the ones left out (`1 - (1 - alpha)^(m / k)`, up to 0.6), so that where
  edges pile up the ink is as deep as with all of them. Highlighted edges
  and the nodes in front are always drawn.
- **`k` follows what the last frames cost.** The page measures a frame from
  its start to the next animation frame and says (`frameCost(ms)`). That is
  a whole number of the display's refresh periods, so it says whether a
  budget fits (12 ms, or 1.3 periods: 22 ms at 60 Hz), never by how much,
  and the controller searches: a new view doubles its budget every frame
  until one is late; then it holds the largest budget known to fit and now
  and then tries 4% more (bolder if that holds, rarer if it does not); and
  a budget that held and then is late twice running is cut in proportion.
  One late frame is a hiccup and changes nothing. Under a steady load the
  budget moves by 4%.
- **At rest** (200 ms after the last movement; `frameRestMs`) a frame draws
  everything: at once, as `render()` always does, if that fits in a frame,
  and else in slices ([below](#refinement-at-rest)). The page draws that
  frame when `framePartial` says the last one left something out; a layout
  that is still running counts as movement (`keepMoving()`).
- `frameInfo()` (and `info().frame`) says what the budget is doing:
  `{k, nodes, edges, ordered, p95, frames, limit, refining, refined,
  refinement}`: the edges and nodes the last moving frame drew, the edges
  there are, how many are in importance order, the p95 of the last 120
  moving frames' costs, how many were measured, the cost a frame is held
  to, and the refinement at rest.

The view keeps no table of where each edge went (it would be four bytes an
edge for as long as the view lives), only how many are ordered. So a delta
that removes or restores an ordered edge, or takes ordered edges off the
end, puts every edge back in index order: the frame at rest is unchanged,
and the frames that move draw a prefix by index until an order is set again.
Hiding a node does not: that is a flag, and the edges stay where they are.

`tests/browser/view.ts` (`ONLY=budget`) reads frames back on WebGL2: with
the edges in importance order the frame at rest is the frame before, byte
for byte, on karate (through deltas and their inverses, with the order set
or half written) and on R-MAT with 100K edges; a moving frame draws a
prefix (8,192 edges and 2,730 nodes of 100,000 and 16,384 at first), less
ink than everything and never none (512 edges and 170 nodes at the least);
and the budget grows back to everything, the same pixels as `render()`. The
controller's unit tests (`budget.rs`) run it against model devices (an
integrated GPU at 60 Hz, 120 Hz, a software renderer): it converges on what
fits, never draws nothing, moves by under a tenth under a steady load, and
follows a load that changes.

Measured in the shell on `.graph rmat` (1M edges, headless Chromium,
software WebGL2: [`BENCH.md`](BENCH.md#the-frame-budget)): frames of a
scripted wheel zoom are 26 to 31 ms apart at the 95th percentile over three
runs, where they were 3.3 s; they draw 512 to 1,081 edges and 170 to 360
nodes, which is all this renderer shows in a frame. Ordering the million edges takes the layout
worker 0.3 s. It has not been measured on a GPU.

## Refinement at rest

Everything, at rest, is seconds of a software renderer's time, and more
than a frame of an integrated GPU's. Drawn at once it holds up whatever
comes next: a zoom begun under it waits for it. So where everything does
not fit in a frame (the budget says: it is what a moving frame leaves
something out of), the frame at rest is drawn in slices into a texture,
and the texture is kept (#496, the design's §5; `refine.rs`).

- **The slices.** `renderFrame` at rest leaves the canvas as it is (the
  last moving frame) and sets `refining`. The page then calls
  `refine(performance.now())` a task at a time. Each call sends the GPU a
  run of the edge buffer, and after the edges a run of the node pass, drawn
  into a texture the size of the canvas over what the runs before left
  there; the first clears it. A slice is sized to take the GPU 16 ms: the
  first is what a moving frame draws, and each is at most twice the last,
  or a quarter of it, by what the last took (`SlicePace`). The GPU says
  when it has finished one (`queue.onSubmittedWorkDone`: a fence under
  WebGL2), and is kept one slice ahead so that it is never idle; `refine`
  sends no more than that, so nothing queues behind seconds of drawing.
  When it returns `false` the page draws a frame.
- **The frame.** That frame, and every frame at rest after it, is the
  texture, copied texel for texel, and over it the highlighted edges and
  the nodes in front: `framePartial` is false again. A hover or a click on
  a million edges is a frame of next to nothing (in the shell with no GPU,
  0.1 s from the click to its frame, where it was 2.5 to 4 s).
- **Any movement stops the slices**: a pan, a zoom, a resize, new
  positions. It waits for the slices the GPU has (two at most), not for
  the rest. A change that only a frame can show (a hover's ring) while
  they are drawn stops them too: a frame of what fits shows it, and they
  begin again.
- **While the view moves**, the texture is drawn under the moving frame's
  own edges and nodes, moved and scaled as the camera has been since it
  was drawn: the whole picture stays on screen through a pan, as a map's
  tiles do, a little soft through a zoom, and what the texture does not
  cover is the prefix alone. A texture whose slices were stopped is drawn
  the same way, with what it has. At the next rest it is drawn again.
- **One render target**, the size of the canvas (4 bytes a pixel), made
  when a refinement first needs it, and destroyed when the view is freed
  (which is what the page does when the pane closes or the context is
  lost), when the canvas changes size, and when everything fits in a frame
  again. It needs nothing WebGL2 lacks: a node slice is drawn from where
  its buffers are bound, not from a first instance.

**The same pixels.** The texture is what `render()` puts under its
highlighted edges: the background, every edge, every node not in front, in
the same order, with the same blending, in the canvas's format. A slice is
a run of the same draw, and blending reads what is in the target whichever
pass put it there, so the refined frame is the frame drawn at once **byte
for byte: the tolerance is 0**, and `tests/browser/view.ts` holds it to
that on WebGL2 (`snapshotRefined()` against `snapshot()`). Two cases are
not byte for byte, both after the fact:

- A node that comes to the front **after** the texture was drawn (hovered,
  selected) is drawn in front as ever, over its copy in the texture. Where
  the two discs are the same size the rim's pixels are blended twice: at
  most a quarter of the way further from what is under the node to its
  colour, on the rim of that node alone. Nodes under 3 px (every node of a
  large graph, fitted) are drawn at 3 px in front and cover their copy
  whole: no pixel differs. When the node leaves the front the frame is
  exact again.
- A node that was in front **when** the texture was drawn and no longer is
  has no copy in it: it is drawn over the texture, after every other node
  and not among them, so it differs where another node overlaps it.

Any other change of flags (a highlight, which dims the rest; a hidden or a
pinned node), a delta, new positions, a style, a theme or the edges'
opacity draws the texture again; an edge order written under the slices,
or the grid's being built, begins them again.

`tests/browser/view.ts` (`ONLY=refine`) checks, on WebGL2: karate fits in
a frame and is never refined; on R-MAT with 100K edges the frame at rest
leaves the canvas alone, is drawn in 25 to 40 slices with no call of
`refine` over 5 ms, and is then the frame drawn at once, 0 bytes apart,
read back and on the canvas: fitted, zoomed in eight times (the nodes in
view alone), in the dark theme, with a path highlighted, a node hidden,
after a delta, and after an order written under the slices; a hover and a
selection draw no slices; a pan stops them, and its frame over the texture
inks as much as everything does. `ONLY="refine rmat 1M"` is the issue's
size.

Measured on R-MAT with 1M edges in headless Chromium's software WebGL2
([`BENCH.md`](BENCH.md#refinement-at-rest)): no task of the page over
50 ms while the slices are drawn, and everything drawn in 2.2 to 2.9 s in
the shell (46,318 nodes) and 3.8 to 5.4 s in the demo (131,072 nodes),
which is what drawing it at once takes there, to a tenth more. It has not
been measured on a GPU.

## Making a view

Making a view costs an upload, not a graph build (#426). At 46K nodes and
1M edges `createView` holds the page's thread for no task over 50 ms and
leaves the page's wasm memory at 16 MiB; it was one task of 190 to 350 ms
and 50 MB ([`BENCH.md`](BENCH.md#making-a-view)).

- **No adjacency.** A `Graph` is the node count and the edge list. Its
  topology, which a layout needs and a frame does not, is built when first
  asked for (`Graph::topology`), so the page's view never builds one: its
  layout runs in a worker. `View::step`, a layout on the view's own thread,
  builds it on its first call. `GraphView.hasTopology` says whether it is
  there.
- **Degrees from the edge list.** A node's degree is its distinct
  neighbours, as the adjacency counts them. `Graph::degrees` counts them in
  three passes over the edges, with no sort: each pair of nodes is put in
  the list of its lower-indexed end, and a pair seen twice in a list is
  counted once. A view made from a result keeps those lists (one `u32` a
  pair) to keep the degrees right as deltas add and remove edges.
- **Positions without a layout.** The nodes start where ForceAtlas2 seeds
  them, which depends on their count alone
  (`ursa_core::algo::fa2_seed_positions`).
- **Edges a run at a time.** The edges go to the GPU 65,536 at a time, each
  run submitted before the next is made, so a million edges are never held
  again as one list of indexes.
- **In pieces.** What is left is compiling the shader, linking five
  pipelines and the uploads: 150 to 200 ms on software WebGL2. On a page
  (`createView`, `createViewFromResult`; `View::new_paced` and
  `View::from_result_paced`) the view gives the event loop a turn between
  pieces whenever it has held the thread for 4 ms (`pace`), so a click or a
  frame waits for one piece, not all of them. The largest piece is the
  shader, about 30 ms. `View::new` and `View::from_result` make the same
  view in one go, for a caller with no event loop.

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

### The selection and the hover

`setSelection` and `setHovered` are the view's own state too: the
`interact` module (no GPU) holds the selection set and the hovered node, and
says what a click does to them, so every front end selects alike.

```js
view.selectionEdges = true;                 // draw the selected nodes' edges wide (off at first)
view.select(view.pick(x, y), e.shiftKey);   // a click: true if the selection changed
view.selection();                           // the selected nodes, in the order selected
view.clearSelection();                      // true if anything was selected
view.hover(x, y);                           // the node under the pointer becomes the hovered one; returns it
view.hovered;                               // or undefined
view.pickEdge(x, y, 4 * dpr);               // the edge within 4 px of the pixel, or undefined
view.selectEdge(edge);                      // select it alone: true if anything changed
view.selectedEdge();                        // [edge, src, dst], or undefined
```

- A click on a node selects it alone; on empty space (`undefined`) it clears
  the selection.
- With Shift (`extend`), a click on a node adds it, or takes it out if it was
  selected; on empty space it changes nothing.
- `hover` uploads nothing while the pointer stays on one node, or on none.
  The page calls it once a frame at most.
- With `selectionEdges`, the edges at a selected node are drawn wide in the
  highlight colour, as highlighted edges are, but nothing else is dimmed: a
  selection is not a highlight. Each change of the selection reads every
  edge once, and uploads 16 bytes for each edge found.
- A node that is hidden (`setHidden`) or removed by a delta leaves the
  selection and the hover.
- **An edge is picked** when no node is under the pointer (`pick` first):
  `pickEdge` gives the edge nearest the pixel, within the tolerance, of the
  edges that are drawn (not removed, and with neither end hidden); of
  parallel edges, the first. With the positions at rest it reads the edges
  near the pixel ([the grid](#the-spatial-grid-pick-hover-and-culling));
  otherwise every edge, each rejected by its bounding box as a rule.
- Nodes are selected or an edge is, never both: `selectEdge` clears the
  nodes, and a click on a node or on empty space deselects the edge. The
  selected edge is drawn wide, as a selection's edges are, and leaves the
  selection when it or one of its ends is removed or hidden.

In Rust: `interact::Selection` (`click`, `set`, `add`, `toggle`, `clear`,
`retain`, `incident_edges`), `interact::nearest_edge`, and
`interact::Interact` (`click`, `select_edge`, `edge`, `clear`).

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

## The spatial grid: pick, hover and culling

A pick used to test every node: 4.9 ms at a million, on every pointer move
that hovers. The view now keeps a grid over the node positions (#500,
`spatial::NodeGrid`), and a pick tests the nodes in the cells around the
pointer:

| at 1,048,576 nodes, native, one core | |
|---|---|
| building the grid | 9.5 to 11.7 ms |
| a pick with it | 6 to 22 µs |
| a pick that tests every node | 3.6 to 4.0 ms |

- **The grid.** Cells tile the box that holds the middle 98% of the
  positions on each axis (one stray node does not stretch them), about two
  nodes to a cell, at most 4096 cells a side; a node outside the box is in
  the nearest border cell. It is built by two counting sorts (by row, then
  each row by column), so a cell's nodes are in index order. It takes 4
  bytes a node and 4 a cell.
- **The pick is the same pick.** The grid only says which nodes to test.
  Each is tested as before (the distance in pixels to its centre, against
  its drawn radius and 2 px), a hidden node is skipped, and of nodes
  equally near the lowest index wins, as the first did in the pass over
  every node. `pickLinear` and `pickEdgeLinear` are that pass, kept for the
  tests, which compare the two at thousands of pixels: on nodes, beside
  them and anywhere, fitted and zoomed, on random and on clustered
  positions.
- **While positions move, the grid waits.** A layout moves every node every
  frame, so the grid has nothing right: a pick tests every node, as before.
  After three frames or picks with no position changed the view builds the
  grid (`gridReady`). While a layout runs that never happens, unless its
  steps are slower than the frames, when building a grid costs less than
  the step and serves every pick until the next. A delta that only adds
  nodes keeps the grid for the nodes it has, and a pick tests the new ones
  as well, until the grid is built again. An undo's saved positions move
  everything.
- **An edge pick** reads a grid of the edges (`spatial::EdgeGrid`): each
  edge is filed by its middle, in a level of cells just longer than the
  edge, so the edges that pass a point are in the cells around it on each
  level, wherever their ends are. It is built for the second edge picked
  with the positions at rest (60 ms and 4 bytes an edge at 1M edges, a few
  picks' worth), for up to 4M edges; past that, and for the first pick, an
  edge pick reads every edge (9 to 16 ms at 1M edges). With it a pick takes
  0.4 to 1.6 ms on a layout whose edges are mostly short, and no less than
  10 ms on one whose edges all cross the drawing, where most edges do pass
  near any point.
- **Culling.** When under half the nodes, and at most 65,536, are in the
  cells in view, `render` draws those alone: it gathers them in index
  order and uploads them as one buffer, 20 bytes a node, when the camera, a
  position, a style or a flag changes. The frame is the frame with every
  node drawn, byte for byte (`setCulling(false)` draws that one;
  `nodesDrawn` says how many instances the node pass draws). Selected,
  hovered and highlighted nodes are in their own pass and are never
  culled; a pinned node's ring is counted in its reach. Zoomed into 1% of
  a 131K-node R-MAT drawing, the node pass draws 0.8 to 2.4% of the
  instances.
  A graph under 1,024 nodes is never culled.

In Rust, a binding calls `View::prepare` before `View::draw` (the web one
does, in `render`): it counts the quiet frame and works out the nodes in
view. `View::pick` and `View::pick_edge` take `&mut self` for the same
count.

## Scenes that change in place

A scene is a base result and the deltas applied to it (#441, the design's
§3 and §6). A view made from a result applies a delta **in time
proportional to the delta, not to the graph**: an expansion of 100 nodes
and 300 edges on a view of a million edges uploads 5 KB and takes 0.7 ms
outside the frame in Chromium ([`BENCH.md`](BENCH.md#deltas)).

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
  colour) and 4 of flags, 8 an edge. A view made from a result starts with
  room for a sixty-fourth more, rounded up to a power of two (a million
  edges: 1,048,576), on the GPU and in the result's own arrays, so its
  first deltas move nothing.
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
  came out different, in runs (4 bytes a node, 8 if a colour changed): the
  two ends of a new edge between nodes that were not neighbours, when nodes
  are sized by degree; every node, when a new node's value widens the
  column's range, or a column that sizes or colours is set.
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
`other` counts the rest and `none` the nodes with no value. The node counts
are kept as deltas come. The shown edges are counted, on the first call
after a delta, only while a node is hidden (its edges are not shown); the
legend, only when a column colours the nodes.

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
While the camera follows the drawing, it is refitted from the last fit's
bounds and the new nodes', not from every position.

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

## Rings and layers

A result whose nodes have a level needs no force at all (#436). A `hop`
result gives each node its distance from a seed, and a set of shortest paths
gives each node its position along the path. Two **fixed** layouts draw
those, done as they are made:

```js
// distance per node: 0 for a seed, 0xFFFFFFFF for none
const rings = Layout.rings(n, src, dst, distance);
// layer per node: its position along the path
const layers = Layout.layers(n, src, dst, layer);
rings.done;        // true: no step() to run
rings.positions(); // x then y, as any layout's
rings.kind;        // "rings" ("layers", or "force" for the others)
```

- **Rings** (`LiveLayout::rings`, `ursa_core::algo::layout_rings`): the seed
  at the centre and distance `d` on the circle of radius `d`. A ring with
  more nodes than fit a quarter of a unit apart gets the radius it needs,
  and the rings outside it move out, so no two nodes are nearer than that.
  Several seeds get a ring system each, side by side in rows in seed order;
  a node is in the system of the seed it is nearest through the drawn edges
  (the lowest-numbered when several tie).
- **Layers** (`LiveLayout::layers`, `layout_layers`): layer `l` on the
  vertical line at `x = l`, the first at the left, nodes half a unit apart.
  One path alone is a straight line. It is also the drawing for a `hop`
  result with several seeds, by distance from the nearest. When a layer is
  more than twice as high as the drawing is wide the lines are spread, so a
  wide neighbourhood is not one column.
- **The order on a ring or a line** is chosen to shorten edges, by four
  barycentre sweeps: out from the first level and back, each level is
  ordered by the mean place of its nodes' neighbours on the level just
  visited (neighbours on the same level count half), ties by node index.
  Each node then goes as near that mean as it can while keeping its distance
  from the nodes beside it, so a child sits in line with its parent and a
  full ring is evenly spaced.
- **Nodes without a level** go after the last: one more ring, or one more
  line.
- **Deterministic:** both are serial, so the same input gives the same bits
  on any thread count. Layers use only `+ - * /` and are the same bits on
  every platform; rings need `sin`, `cos` and `atan2`, whose last bit is
  the platform's.
- **Cost:** 2 ms for a neighbourhood of 2,048 nodes and 8,334 edges on one
  thread, 21 ms for 16,384 nodes and 88,822 edges, 305 ms for 131,072 nodes
  and 928,785 edges (rings; layers take half that), where coarse to fine
  takes seconds ([`BENCH.md`](BENCH.md#rings-and-layers)).
- A fixed layout can **grow** like any other (`Layout.grow`): the new nodes
  settle beside their parents by force, not on a ring or a layer, and
  `kind` is `"force"` from then on.

In the worker, `{type: "start", …, layout: {kind: "rings", level}}` posts one
`positions` message and `done` (`web/public/graph-worker.js`). The demo
draws them with `&layout=rings&seeds=0&hops=2` and
`&layout=layers&from=16&to=26` (add `&graph=karate`), and
`tests/browser/view.ts` checks them: on karate, two hops from node 0 are two
rings of 16 and 9 nodes, each node on its radius and none drawn over
another; all shortest paths from 16 to 26 are six layers.

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
`&highlight=path` to start with a path highlighted;
`&layout=rings&seeds=0&hops=2` or `&layout=layers&from=16&to=26` draws a
neighbourhood or the shortest paths with a fixed layout. Drag to pan, wheel to
zoom, double-click to fit. The pointer's node is hovered; a click selects
it, shift-click adds to the selection, and two selected nodes draw the
shortest path between them. `p` pins the selection, `h` hides it, `u` shows
everything again and Escape clears. `&compact=1` drops the nodes no edge
names (R-MAT at scale 16 with 1M edges is then the shell's 46K nodes). With `&result=1` the view is made from a
graph result, and once the layout is done `e` expands the selected node (one
delta: five new nodes and their edges) and `z` undoes the last expansion.

The browser harness runs the demo on each test graph, reads frames back and
checks them:

```sh
bun ursa-viz/tests/browser/view.ts [screenshot dir]   # WEBGPU=0 for WebGL2; ONLY=<name>; MODE=flat|multilevel
ONLY=highlight bun ursa-viz/tests/browser/view.ts     # the highlight layer alone (always on WebGL2)
ONLY=delta bun ursa-viz/tests/browser/view.ts         # deltas alone; ONLY="delta karate" without the 1M-edge timing
ONLY=create bun ursa-viz/tests/browser/view.ts        # making a view of 1M edges: the longest task and the wasm memory
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

- **Refinement that survives a hover.** A change of flags while the slices
  are drawn begins them again (#496), so with no GPU, where they take
  seconds, the full picture comes only once the pointer stops crossing
  nodes. And there is one texture: a refinement begun after a pan draws
  over the picture the pan was showing, so a frame needed before it ends
  (that hover) has only what fits. A second target would keep the old
  picture until the new one is whole, at 4 bytes a pixel more.
- **A 1M-edge R-MAT graph fully converged in seconds.** Coarse to fine
  draws its shape in 7.5 s, and it looks as flat ForceAtlas2 does after
  500 iterations; but by edge length it is where flat is after about 170, as
  its finest level gets only 15 iterations. R-MAT's coarse levels stay dense
  (half the nodes, nearly all the edges), so they cost about what the graph
  does. Cheaper iterations (attraction gathers 2M random reads; repulsion
  could be approximated harder far out) would buy the rest.
- **A grid while the layout runs.** The grid waits for the positions to
  rest (#500), so a hover during a layout still tests every node, and every
  node is drawn. A grid that tolerates small moves (cells widened by the
  furthest any node has moved) would serve the layout's last, slow stretch.
- **Culling past 65,536 nodes in view.** The nodes in view are uploaded
  when the camera moves, so they are capped; between that and half the
  nodes, every node is drawn. Drawing ranges of a buffer sorted by cell
  would have no cap, but would draw overlapping nodes in another order than
  the unculled frame does, and hold every node a second time on the GPU.
- **Edges are not culled**: the line list draws every edge, in view or not.
- **Rings and layers from the shell.** The shell's worker takes
  `graph.layout = {kind, level}` from `graph.js`, but nothing sets it yet:
  the rules that turn a `hop` result or a path column into one are #433.
- **Rings that stay rings when they grow.** Growing a fixed layout hands it
  to ForceAtlas2, whose scale is not the rings' (a ring step is one unit).
- **Growing from the shell.** `Layout.grow` is in the wasm API; the
  shell's layout worker does not call it yet (`{type: "grow"}`, with #443).
- **The view's own layout.** `View::step`, for a page that lays out on its
  own thread, is still flat ForceAtlas2; the worker's `Layout` is coarse to
  fine.
- **GPU timestamps for the frame budget.** The budget follows the time
  between animation frames, which is whole refresh periods. WebGPU's
  `timestamp-query` would say what a frame cost the GPU to the microsecond,
  so the budget could go straight to what fits instead of searching; the
  view does not request the feature, and there was no GPU here to try it on.
- **The frame budget on a 30 Hz display.** A frame "fits" within 1.3
  refresh periods, with the period taken to be at most 60 Hz's: on a display
  that refreshes slower every frame is late, and the view draws its least.
- **Slices at rest.** The frame at rest draws everything in one go: seconds
  in software, on the page's thread. Drawing it in slices into a texture,
  and panning that texture, is the design's next step (P3-2).
- **An order after the graph grows.** Edges added by a delta are drawn
  after the ordered ones, least important, however central they are; and a
  delta that removes an ordered edge drops the order. Nothing asks the
  layout for a new one.
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
- **Doubling under a delta.** The delta that outgrows the buffers pays for
  the copy of what they held (on the GPU, and of the result's arrays):
  about 12 ms at a million edges here, once per doubling. Growing ahead of
  need, in an idle moment, would hide it.
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
