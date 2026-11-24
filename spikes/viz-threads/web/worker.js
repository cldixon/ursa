// The layout worker of the measurement: load a build (pool.js), lay out one
// of the benchmark's graphs to the end, and post the times and the hash of
// the positions' bits.
import { loadLayout } from "./pool.js";

self.onmessage = async ({ data }) => {
  try {
    const t0 = performance.now();
    const { module, wasm, build, threads, initMs, poolMs, isolated, fellBack } = await loadLayout(data.want);
    const startMs = performance.now() - t0;
    const g = module.graph(data.graph);
    const n = g[0];
    const edges = (g.length - 1) / 2;
    const src = g.slice(1, 1 + edges);
    const dst = g.slice(1 + edges);
    const runs = [];
    for (let i = 0; i < (data.repeats ?? 1); i++) {
      const tb = performance.now();
      const layout = new module.Layout(n, src, dst);
      const buildMs = performance.now() - tb;
      const tl = performance.now();
      // In batches, as the shell's worker steps it (a batch is a rayon
      // entry and exit, so the pool's wake-ups are counted).
      while (!layout.done) layout.step(4);
      const layoutMs = performance.now() - tl;
      runs.push({ buildMs, layoutMs, iterations: layout.iterations, hash: layout.positionsHash() });
      layout.free();
    }
    self.postMessage({
      ok: true, graph: data.graph, build, threads, poolThreads: module.threads(), isolated, fellBack,
      initMs, poolMs, startMs, memoryBytes: wasm.memory.buffer.byteLength,
      shared: wasm.memory.buffer instanceof (self.SharedArrayBuffer ?? class {}), runs,
    });
  } catch (e) {
    self.postMessage({ ok: false, error: String(e?.stack ?? e) });
  }
};
