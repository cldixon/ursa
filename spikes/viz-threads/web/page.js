// The page of the measurement: `run({ graph, want, repeats })` starts a fresh
// layout worker, as the shell does for each view, and resolves with what it
// measured. (A file, not inline: the shell's CSP allows no inline script.)
import { pickBuild } from "./pool.js";

window.pick = () => pickBuild();
window.run = (job) =>
  new Promise((resolve) => {
    const t0 = performance.now();
    const worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
    worker.onmessage = ({ data }) => {
      worker.terminate();
      resolve({ ...data, workerMs: performance.now() - t0 });
    };
    worker.onerror = (e) => resolve({ ok: false, error: `worker: ${e.message}` });
    worker.postMessage(job);
  });
window.ready = true;
