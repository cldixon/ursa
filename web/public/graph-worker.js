// The Graph view's layout (graph.js), off the page's thread: ursa-viz's
// coarse-to-fine ForceAtlas2, stepped in batches of about 100 ms, posting the
// positions (x then y, transferred) after each, until its schedule is done.
// Adapted from ursa-viz/demo/layout-worker.js. The page terminates the worker
// to stop it.
//
//   {type: "start", module, n, src, dst}
//     -> {type: "positions", xy, iterations, level} ..., then {type: "done", iterations}
//     or {type: "error", message}

self.onmessage = async ({ data }) => {
  if (data.type !== "start") return;
  try {
    // The same ursa-viz build as the page's: its URL carries its hash.
    const { default: init, Layout } = await import(data.module);
    await init();
    const layout = new Layout(data.n, data.src, data.dst);
    // Coarse to fine has a place for every node before any iteration: draw it.
    post(layout);
    await run(layout, data.budgetMs ?? 100);
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  }
}

function post(layout) {
  const xy = new Float32Array(layout.positions());
  self.postMessage({ type: "positions", xy, iterations: layout.iterations, level: layout.level }, [xy.buffer]);
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
