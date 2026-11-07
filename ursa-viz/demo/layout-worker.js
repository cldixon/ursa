// Runs the layout off the page's thread and posts positions as it goes.
import init, { Layout } from "../pkg/ursa_viz.js";

let layout = null;
let running = false;

self.onmessage = async ({ data }) => {
  if (data.type === "start") {
    await init();
    layout = new Layout(data.n, data.src, data.dst);
    running = true;
    loop(data.budgetMs ?? 30, data.maxIterations ?? 1000);
  } else if (data.type === "stop") {
    running = false;
  }
};

/** Step in batches of about `budgetMs`, posting the positions after each. */
async function loop(budgetMs, maxIterations) {
  let perIteration = null; // ms, measured over the whole run
  let totalMs = 0;
  while (running && layout.iterations < maxIterations) {
    const batch = perIteration === null ? 1 : Math.max(1, Math.floor(budgetMs / Math.max(perIteration, 0.01)));
    const n = Math.min(batch, maxIterations - layout.iterations);
    const t = performance.now();
    layout.step(n);
    totalMs += performance.now() - t;
    perIteration = totalMs / layout.iterations;
    const xy = new Float32Array(layout.positions());
    self.postMessage({ type: "positions", xy, iterations: layout.iterations, msPerIteration: perIteration }, [xy.buffer]);
    await new Promise((r) => setTimeout(r, 0)); // let "stop" arrive
  }
  self.postMessage({ type: "done", iterations: layout.iterations });
}
