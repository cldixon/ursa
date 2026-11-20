// Runs the layout off the page's thread and posts positions as it goes.
import init, { Layout } from "../pkg/ursa_viz.js";

let layout = null;
let wasm = null;
let running = false;

self.onmessage = async ({ data }) => {
  if (data.type === "start") {
    const ti = performance.now();
    wasm = await init();
    const t0 = performance.now();
    const workerInitMs = t0 - ti;
    layout = new Layout(data.n, data.src, data.dst, data.multilevel ?? true);
    const buildMs = performance.now() - t0;
    running = true;
    // Coarse to fine has a place for every node before any iteration: draw it.
    post({ buildMs, workerInitMs });
    loop(data.budgetMs ?? 100, layout.total ?? data.maxIterations ?? 1000, t0);
  } else if (data.type === "stop") {
    running = false;
  }
};

function post(extra) {
  const xy = new Float32Array(layout.positions());
  self.postMessage({ type: "positions", xy, iterations: layout.iterations, level: layout.level, memoryBytes: wasm.memory.buffer.byteLength, ...extra }, [xy.buffer]);
}

/**
 * Step in batches of about `budgetMs`, posting the positions after each
 * (100 ms: coarse to fine, posting means prolonging every level down to the
 * input graph, a few milliseconds a time at 1M edges).
 * Coarse to fine, a step's unit is work worth one iteration of the whole
 * graph; its cost per unit moves as the levels change, so the batch size
 * follows the last batch's rate rather than the run's average.
 */
async function loop(budgetMs, maxIterations, t0) {
  let msPerUnit = null;
  let totalMs = 0;
  while (running && !layout.done && layout.iterations < maxIterations) {
    let batch = msPerUnit === null ? 1 : Math.max(1, Math.floor(budgetMs / Math.max(msPerUnit, 0.01)));
    batch = Math.min(batch, maxIterations - layout.iterations);
    const t = performance.now();
    layout.step(batch);
    const ms = performance.now() - t;
    totalMs += ms;
    msPerUnit = ms / batch;
    post({ msPerIteration: totalMs / Math.max(layout.iterations, 1) });
    await new Promise((r) => setTimeout(r, 0)); // let "stop" arrive
  }
  self.postMessage({ type: "done", iterations: layout.iterations, layoutMs: performance.now() - t0 });
}
