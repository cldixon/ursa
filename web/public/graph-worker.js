// The Graph view's layout (graph.js), off the page's thread: ursa-viz's
// ForceAtlas2, stepped in batches of about 30 ms, posting the positions
// (x then y, transferred) after each, until `maxIterations`. Adapted from
// ursa-viz/demo/layout-worker.js. The page terminates the worker to stop it.
//
//   {type: "start", module, n, src, dst, maxIterations}
//     -> {type: "positions", xy, iterations} ..., then {type: "done", iterations}
//     or {type: "error", message}

self.onmessage = async ({ data }) => {
  if (data.type !== "start") return;
  try {
    // The same ursa-viz build as the page's: its URL carries its hash.
    const { default: init, Layout } = await import(data.module);
    await init();
    const layout = new Layout(data.n, data.src, data.dst);
    await run(layout, data.budgetMs ?? 30, data.maxIterations ?? 1000);
  } catch (e) {
    self.postMessage({ type: "error", message: String(e?.message ?? e) });
  }
};

/** Step in batches of about `budgetMs`, posting the positions after each. */
async function run(layout, budgetMs, maxIterations) {
  let totalMs = 0;
  while (layout.iterations < maxIterations) {
    const perIteration = layout.iterations ? totalMs / layout.iterations : null;
    const batch = perIteration === null ? 1 : Math.max(1, Math.floor(budgetMs / Math.max(perIteration, 0.01)));
    const n = Math.min(batch, maxIterations - layout.iterations);
    const t = performance.now();
    layout.step(n);
    totalMs += performance.now() - t;
    const xy = new Float32Array(layout.positions());
    self.postMessage({ type: "positions", xy, iterations: layout.iterations }, [xy.buffer]);
    await new Promise((r) => setTimeout(r, 0));
  }
  self.postMessage({ type: "done", iterations: layout.iterations });
}
