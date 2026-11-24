// The Graph view's layout (graph.js), off the page's thread: ursa-viz's
// coarse-to-fine ForceAtlas2, stepped in batches of about 100 ms, posting the
// positions (x then y, transferred) after each, until its schedule is done.
// Adapted from ursa-viz/demo/layout-worker.js. The page terminates the worker
// to stop it.
//
//   {type: "init", module}                 -> {type: "ready"}  (ursa-viz loaded,
//                                             ahead of the graph: graph.js preload)
//   {type: "start", module, n, src, dst, layout?}
//   {type: "start", module, n, adjacency: {offsets, targets}, src, dst, layout?}
//   {type: "start", module, n, src, dst, positions}
//     -> {type: "positions", xy, iterations, level, levels, total} ..., then {type: "done", iterations}
//     (a large graph: "level" and "levelPositions" first, see below)
//     or {type: "error", message}
//     The graph is its edges (src, dst), or, where the page was given it,
//     its undirected adjacency (UGR's edges.adjacency.*), which the layout
//     uses as it is, sorting nothing. The layout is the same either way.
//     With an adjacency the edges still come, for the "order" below, which
//     is of the page's edges as it numbers them; the layout does not read
//     them.
//   {type: "grow", epoch, n, src, dst}     the graph grew (an expansion, #443):
//     the same nodes, new ones after them, and the edges shown now. The new
//     nodes start beside their neighbours and the layout runs warm, the old
//     nodes held or slow (Layout.grow); what is left of an earlier schedule
//     is dropped. -> "positions" ..., then "done", as after "start"
//   {type: "place", epoch, n, src, dst, xy} the graph is at these positions
//     (an undo restored them): the layout is there, with nothing to run,
//     for the next "grow". -> "done"
//
// After the first "positions" of a "start" comes, once,
//   {type: "order", order, ms}: the order to draw the edges in (#495), most
//     important first, a Uint32Array of edge indexes (transferred) for
//     GraphView.setEdgeOrder, and the ms it took to compute. The layout has
//     the hierarchy the order comes from (Layout.edgeOrder).
//
// A large graph is drawn a level at a time (#497): while the layout is at
// a coarse level of its hierarchy, the page gets that level as a graph of
// its own, not every node's position prolonged from it. `coarse` in
// "start" says whether (by default, a graph of COARSE_FROM nodes and edges
// or more), and `levelEdges` how many of a level's merged edges to send
// (LEVEL_EDGES). In place of "positions", until the layout reaches the
// graph itself:
//   {type: "level", level, levels, n, mass, parents?, src, dst, weight, totalEdges, xy, ...}
//     the layout came to `level` (first, the coarsest, with its iterations
//     done): its `n` nodes' masses (the summed degree + 1 of the nodes
//     each stands for), each node's parent in the level above (0xFFFFFFFF
//     for none; absent for the coarsest), its heaviest merged edges
//     (heaviest first: ends and weights) of the `totalEdges` it has, and
//     its positions (x then y). Sent when the layout reaches the level,
//     before the hierarchy lets anything of it go; all typed arrays,
//     transferred. For level 0 it has only `parents` (the nodes are the
//     graph's), and "positions" follow as ever.
//   {type: "levelPositions", level, xy, ...}  the level's positions, after
//     each batch.
// Both carry `epoch`, `iterations`, `total` and `memory` as "positions"
// does. The layout ends on the same positions, bit for bit.
//
// `positions` (x then y, one per node each) says where the nodes are
// already (saved positions, .graph … POSITIONS (query), #502): no layout
// runs. The page has drawn them; the layout here is at them, for a later
// "grow" to start from, and the answer is the "order" (by degree: there is
// no hierarchy) and then "done", with no "positions".
//
// The aggregated view (#498): once the layout has reached the graph
// itself, the page asks for a cut through its hierarchy when its camera
// comes to rest, and draws that in place of the graph (GraphView.setCut).
//   {type: "cut", id, camera, separation?, maxNodes?, maxEdges?, detailNodes?}
//     `camera` is GraphView.camera()'s [x, y, pixelsPerUnit, width, height].
//     -> {type: "cut", id, epoch, ms, memory, available: false}  no hierarchy
//        to cut: the layout is flat or fixed, has not reached the graph, or
//        the graph grew; or
//     -> {type: "cut", id, epoch, ms, memory, available: true, detail, xy,
//         mass, members, of, src, dst, weight, totalEdges, inView,
//         membersInView}: Layout.cut's answer, its typed arrays
//        transferred, and the ms it took. `detail` says the graph itself is
//        to be drawn: what is in view is its own nodes.
// The hierarchy is the layout's own: it keeps each level's members when
// it comes down from it (under 1 MB at a million edges). When the layout
// is done, after "done" is posted, a forest is made of them, with the
// graph's edges in its order (Layout.prepareCut: 5 MB there, in memory
// the layout has let go of, held until the graph grows): every answer
// carries `prepareMs`, what that took, and `bytes`, what it holds.
//
// `layout` picks a fixed layout instead: {kind: "rings" | "layers", level},
// `level` a Uint32Array of each node's distance from a seed (rings) or its
// layer (layers), 0xFFFFFFFF for none. A fixed layout is done as it is made:
// one "positions" message, then "done".
//
// Every "positions" and "done" carries `epoch`: the last "grow" or "place"
// it follows (0 before any), so the page drops positions of a graph it no
// longer draws.
//
// The first message after "start" carries `times` (performance.now() here, and
// `origin`, this worker's time origin): when it got the graph and when its
// hierarchy was built, for the page's timings. Every "positions" and "done"
// carries `memory`, the bytes of this worker's WebAssembly memory.
//
// Positions are copied twice between the layout and the GPU (#501): once
// here, from the layout's state into the array posted (Layout.positions),
// which is transferred, and once on the page, into the view.

let loaded = null;
// ursa-viz's wasm exports, once loaded: `memory` is what graphMemory reports.
let wasm = null;
/** ursa-viz, the same build as the page's: its URL carries its hash. */
function load(url) {
  loaded ??= import(url).then(async (m) => {
    wasm = await m.default();
    return m;
  });
  return loaded;
}

let times = null;
// The layout, the epoch of the graph it lays out, and whether run() is
// stepping it.
let layout = null;
let epoch = 0;
let running = false;
let budgetMs = 100;
// The cost of one unit of the layout's work, as the last batch measured it.
let msPerUnit = null;
// Whether the page is sent the level being laid out, while it is a coarse
// one, and how many of its merged edges.
let coarse = false;
let levelEdges = 0;
/** A graph of this many nodes and edges is drawn a level at a time: its layout takes a second or more. */
const COARSE_FROM = 200_000;
/** The most merged edges of a level sent to the page: the heaviest. */
const LEVEL_EDGES = 1 << 14;

// One message at a time, in order: a "grow" waits for the "start" before it
// (which loads ursa-viz), but not for the layout to finish.
let queue = Promise.resolve();
self.onmessage = ({ data }) => {
  queue = queue.then(() => handle(data));
};

async function handle(data) {
  try {
    if (data.type === "init") {
      await load(data.module);
      self.postMessage({ type: "ready" });
    } else if (data.type === "start") {
      times = { workerStart: performance.now() };
      const { Layout } = await load(data.module);
      times.workerLoaded = performance.now();
      if (data.positions) {
        // Saved positions: the nodes are where the page drew them, and
        // nothing runs.
        layout?.free();
        layout = Layout.fromPositions(data.n, data.src, data.dst, data.positions);
        times.hierarchy = performance.now();
        coarse = false;
        postOrder(data);
        send({ type: "done" });
        return;
      }
      layout = make(Layout, data);
      prepareMs = null;
      times.hierarchy = performance.now();
      budgetMs = data.budgetMs ?? 100;
      levelEdges = data.levelEdges ?? LEVEL_EDGES;
      coarse = layout.kind === "force" && layout.levels > 1 && (data.coarse ?? data.n + (data.src?.length ?? 0) >= COARSE_FROM);
      if (coarse) {
        // The coarsest level, laid out: a few dozen nodes, in a few
        // milliseconds. The first picture is the drawing's shape, not
        // where the seed put them.
        const t = performance.now();
        while (!layout.levelFinished && performance.now() - t < budgetMs) layout.stepLevel(1);
        postLevel();
      } else {
        // Coarse to fine has a place for every node before any iteration, and a
        // fixed layout has its last: draw it.
        post();
      }
      // Before the layout comes down a level: the hierarchy the order is
      // read from is let go level by level.
      postOrder(data);
      run();
    } else if (data.type === "cut") {
      postCut(data);
    } else if (data.type === "grow") {
      if (!layout) throw new Error("no layout to grow");
      layout.grow(data.n, data.src, data.dst);
      // A grown layout has no levels: every node has its place.
      coarse = false;
      epoch = data.epoch;
      msPerUnit = null;
      // The new nodes beside their neighbours, before any iteration.
      post();
      run();
    } else if (data.type === "place") {
      const { Layout } = await loaded;
      const placed = Layout.fromPositions(data.n, data.src, data.dst, data.xy);
      layout?.free();
      layout = placed;
      coarse = false;
      epoch = data.epoch;
      if (!running) done();
    }
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  }
}

/** The layout the page asked for: coarse to fine, or rings or layers by `layout.level`. */
function make(Layout, { n, src, dst, adjacency, layout }) {
  if (adjacency) return Layout.fromAdjacency(n, adjacency.offsets, adjacency.targets, layout?.kind, layout?.level);
  if (!layout) return new Layout(n, src, dst);
  if (layout.kind === "rings") return Layout.rings(n, src, dst, layout.level);
  if (layout.kind === "layers") return Layout.layers(n, src, dst, layout.level);
  throw new Error(`no layout called ${layout.kind}`);
}

function post() {
  // A new array, filled from the layout's memory: this message's to give.
  const xy = layout.positions();
  send({ type: "positions", xy }, [xy.buffer]);
}

/** Post `message`, with how far the layout is and what this worker holds; the first carries `times`. */
function send(message, transfer) {
  Object.assign(message, { epoch, iterations: layout.iterations, level: layout.level, levels: layout.levels, total: layout.total, memory: wasm.memory.buffer.byteLength });
  if (times) {
    message.times = times;
    message.origin = performance.timeOrigin;
    times = null;
  }
  self.postMessage(message, transfer);
}

/**
 * The level the layout has come to, as a graph of its own (#497): read
 * from the hierarchy now, before the layout steps on and lets it go. Level
 * 0 is the graph itself: only its nodes' parents go, for the page to bring
 * them out from, and the positions as ever.
 */
function postLevel() {
  const g = layout.levelGraph(levelEdges);
  if (g.level === 0) {
    send({ type: "level", parents: g.parents }, g.parents ? [g.parents.buffer] : []);
    return post();
  }
  const xy = layout.levelPositions();
  const { mass, parents, src, dst, weight, totalEdges } = g;
  const message = { type: "level", n: mass.length, mass, parents, src, dst, weight, totalEdges, xy };
  send(message, [mass, parents, src, dst, weight, xy].filter(Boolean).map((a) => a.buffer));
}

/** The positions of the coarse level being laid out, or, at level 0, of the graph. */
function postLevelPositions() {
  if (layout.level === 0) return post();
  const xy = layout.levelPositions();
  send({ type: "levelPositions", xy }, [xy.buffer]);
}

/**
 * The order to draw the edges `(src[i], dst[i])` in, most important first
 * (#495): what the page draws a prefix of while the view moves. Once, after
 * the first positions: the hierarchy it comes from never changes.
 */
function postOrder({ src, dst }) {
  if (!src || !dst) return;
  const t = performance.now();
  // The edges stay here: the layout reads them a run at a time, and writes
  // the order into an array of this worker's, to give away (#501).
  const order = layout.edgeOrder(src, dst);
  self.postMessage({ type: "order", order, ms: performance.now() - t }, [order.buffer]);
}

/**
 * A cut through the layout's hierarchy for the page's camera (#498): the
 * groups to draw in place of the graph, where its nodes are now, or that
 * there is none to give.
 */
function postCut({ id, camera, separation, maxNodes, maxEdges, detailNodes }) {
  // (Made already, unless the page asked before the layout was done.)
  prepareCut();
  const t = performance.now();
  const cut = layout?.hasCut ? layout.cut(camera, separation, maxNodes, maxEdges, detailNodes) : undefined;
  const message = { type: "cut", id, epoch, ms: performance.now() - t, prepareMs, bytes: layout?.cutBytes, memory: wasm?.memory.buffer.byteLength, available: Boolean(cut), ...cut };
  self.postMessage(message, cut ? [cut.xy, cut.mass, cut.members, cut.of, cut.src, cut.dst, cut.weight].map((a) => a.buffer) : []);
}

// What making the hierarchy's forest took, in ms, once it is made.
let prepareMs = null;
/** The forest a cut is made from, if the layout has a hierarchy and has reached the graph: once. */
function prepareCut() {
  if (prepareMs !== null || !layout?.hasCut) return;
  const t = performance.now();
  layout.prepareCut();
  prepareMs = performance.now() - t;
}

function done() {
  self.postMessage({ type: "done", epoch, iterations: layout.iterations, memory: wasm.memory.buffer.byteLength });
  // For the cuts the page will ask for (#498): after the page is told.
  prepareCut();
}

/**
 * Step the layout in batches of about `budgetMs`, posting the positions
 * after each, until it is done; the layout is whichever is there now (a
 * "grow" or a "place" changes it between batches). A step's unit is work
 * worth one iteration of the whole graph; its cost moves as the levels
 * change, so a batch's size follows the last batch's rate. (100 ms: each
 * post prolongs every level down to the input graph, a few milliseconds at
 * a million edges.)
 */
async function run() {
  if (running) return;
  running = true;
  try {
    while (!layout.done) {
      const batch = msPerUnit === null ? 1 : Math.max(1, Math.floor(budgetMs / Math.max(msPerUnit, 0.01)));
      const t = performance.now();
      if (coarse) {
        // A level at a time: a batch ends where its level does, so what a
        // unit costs is measured by the work done, not the work asked for.
        const [level, work] = [layout.level, layout.work];
        layout.stepLevel(batch);
        const worked = layout.work - work;
        if (worked > 0) msPerUnit = (performance.now() - t) / worked;
        if (layout.level !== level) postLevel();
        else postLevelPositions();
        if (layout.level === 0) coarse = false;
      } else {
        layout.step(batch);
        msPerUnit = (performance.now() - t) / batch;
        post();
      }
      await new Promise((r) => setTimeout(r, 0));
    }
    done();
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  } finally {
    running = false;
  }
}
