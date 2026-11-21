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
//
// `layout` picks a fixed layout instead: {kind: "rings" | "layers", level},
// `level` a Uint32Array of each node's distance from a seed (rings) or its
// layer (layers), 0xFFFFFFFF for none. A fixed layout is done as it is made:
// one "positions" message, then "done".
//
// The first message after "start" carries `times` (performance.now() here, and
// `origin`, this worker's time origin): when it got the graph and when its
// hierarchy was built, for the page's timings.

let loaded = null;
/** ursa-viz, the same build as the page's: its URL carries its hash. */
function load(url) {
  loaded ??= import(url).then(async (m) => {
    await m.default();
    return m;
  });
  return loaded;
}

let times = null;
self.onmessage = async ({ data }) => {
  try {
    if (data.type === "init") {
      await load(data.module);
      self.postMessage({ type: "ready" });
      return;
    }
    if (data.type !== "start") return;
    times = { workerStart: performance.now() };
    const { Layout } = await load(data.module);
    times.workerLoaded = performance.now();
    const layout = make(Layout, data);
    times.hierarchy = performance.now();
    // Coarse to fine has a place for every node before any iteration, and a
    // fixed layout has its last: draw it.
    post(layout);
    await run(layout, data.budgetMs ?? 100);
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  }
};

/** The layout the page asked for: coarse to fine, or rings or layers by `layout.level`. */
function make(Layout, { n, src, dst, layout }) {
  if (!layout) return new Layout(n, src, dst);
  if (layout.kind === "rings") return Layout.rings(n, src, dst, layout.level);
  if (layout.kind === "layers") return Layout.layers(n, src, dst, layout.level);
  throw new Error(`no layout called ${layout.kind}`);
}

function post(layout) {
  const xy = new Float32Array(layout.positions());
  const message = { type: "positions", xy, iterations: layout.iterations, level: layout.level, levels: layout.levels, total: layout.total };
  if (times) {
    message.times = times;
    message.origin = performance.timeOrigin;
    times = null;
  }
  self.postMessage(message, [xy.buffer]);
}

/**
 * Step in batches of about `budgetMs`, posting the positions after each. A
 * step's unit is work worth one iteration of the whole graph; its cost moves
 * as the levels change, so a batch's size follows the last batch's rate.
 * (100 ms: each post prolongs every level down to the input graph, a few
 * milliseconds at a million edges.)
 */
async function run(layout, budgetMs) {
  let msPerUnit = null;
  while (!layout.done) {
    const batch = msPerUnit === null ? 1 : Math.max(1, Math.floor(budgetMs / Math.max(msPerUnit, 0.01)));
    const t = performance.now();
    layout.step(batch);
    msPerUnit = (performance.now() - t) / batch;
    post(layout);
    await new Promise((r) => setTimeout(r, 0));
  }
  self.postMessage({ type: "done", iterations: layout.iterations });
}
