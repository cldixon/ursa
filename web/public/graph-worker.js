// The Graph view's layout (graph.js), off the page's thread: ursa-viz's
// coarse-to-fine ForceAtlas2, stepped in batches of about 100 ms, posting the
// positions (x then y, transferred) after each, until its schedule is done.
// Adapted from ursa-viz/demo/layout-worker.js. The page terminates the worker
// to stop it.
//
//   {type: "init", module}                 -> {type: "ready"}  (ursa-viz loaded,
//                                             ahead of the graph: graph.js preload)
//   {type: "start", module, n, src, dst, layout?}
//     -> {type: "positions", xy, iterations, level, levels, total} ..., then {type: "done", iterations}
//     or {type: "error", message}
//   {type: "grow", epoch, n, src, dst}     the graph grew (an expansion, #443):
//     the same nodes, new ones after them, and the edges shown now. The new
//     nodes start beside their neighbours and the layout runs warm, the old
//     nodes held or slow (Layout.grow); what is left of an earlier schedule
//     is dropped. -> "positions" ..., then "done", as after "start"
//   {type: "place", epoch, n, src, dst, xy} the graph is at these positions
//     (an undo restored them): the layout is there, with nothing to run,
//     for the next "grow". -> "done"
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
      layout = make(Layout, data);
      times.hierarchy = performance.now();
      budgetMs = data.budgetMs ?? 100;
      // Coarse to fine has a place for every node before any iteration, and a
      // fixed layout has its last: draw it.
      post();
      run();
    } else if (data.type === "grow") {
      if (!layout) throw new Error("no layout to grow");
      layout.grow(data.n, data.src, data.dst);
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
      epoch = data.epoch;
      if (!running) done();
    }
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  }
}

/** The layout the page asked for: coarse to fine, or rings or layers by `layout.level`. */
function make(Layout, { n, src, dst, layout }) {
  if (!layout) return new Layout(n, src, dst);
  if (layout.kind === "rings") return Layout.rings(n, src, dst, layout.level);
  if (layout.kind === "layers") return Layout.layers(n, src, dst, layout.level);
  throw new Error(`no layout called ${layout.kind}`);
}

function post() {
  const xy = new Float32Array(layout.positions());
  const message = { type: "positions", epoch, xy, iterations: layout.iterations, level: layout.level, levels: layout.levels, total: layout.total, memory: wasm.memory.buffer.byteLength };
  if (times) {
    message.times = times;
    message.origin = performance.timeOrigin;
    times = null;
  }
  self.postMessage(message, [xy.buffer]);
}

function done() {
  self.postMessage({ type: "done", epoch, iterations: layout.iterations, memory: wasm.memory.buffer.byteLength });
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
      layout.step(batch);
      msPerUnit = (performance.now() - t) / batch;
      post();
      await new Promise((r) => setTimeout(r, 0));
    }
    done();
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  } finally {
    running = false;
  }
}
