// The graph view, bare: a generated graph, its layout running in a worker,
// drawn every frame. Drag to pan, wheel to zoom, double-click to fit, click a
// node to see its index. ?graph=rmat&scale=17&edges=1000000 | grid | clusters;
// &multilevel=0 for flat ForceAtlas2 (coarse to fine is the default).
import init, { createView } from "../pkg/ursa_viz.js";
import { fromParams } from "./graphs.js";

const params = new URLSearchParams(location.search);
const canvas = document.getElementById("view");
const statsEl = document.getElementById("stats");
const dpr = window.devicePixelRatio || 1;
canvas.width = Math.round(canvas.clientWidth * dpr);
canvas.height = Math.round(canvas.clientHeight * dpr);

const stats = (window.stats = { phase: "loading" });

const t0 = performance.now();
// The wasm's exports: `memory` is what the bench reads.
window.wasm = await init();
stats.initMs = performance.now() - t0;
const tg = performance.now();
const g = fromParams(params);
stats.generateMs = performance.now() - tg;
const t1 = performance.now();
// Page-clock marks (ms since navigation), for the bench: when the graph was
// ready, the first frame with layout positions, the layout's end.
stats.graphReadyAt = t1;
const view = await createView(canvas, g.n, g.src, g.dst);
stats.createMs = performance.now() - t1;
if (g.community) view.colorBy(g.community);
window.view = view;
// An image of the view; frames pause while it is taken.
let paused = false;
window.pauseFrames = (on) => { paused = on; };
window.snapshot = async () => {
  paused = true;
  try { return await view.snapshot(); } finally { paused = false; }
};
window.graph = g;
Object.assign(stats, { phase: "running", backend: view.backend, nodes: view.nodeCount, edges: view.edgeCount, iterations: 0 });

// The layout, in a worker.
const worker = new Worker(new URL("./layout-worker.js", import.meta.url), { type: "module" });
let pending = null;
worker.onmessage = ({ data }) => {
  if (data.type === "positions") {
    pending = data.xy;
    stats.firstPositionsAt ??= performance.now();
    stats.workerMemoryBytes = data.memoryBytes;
    stats.iterations = data.iterations;
    stats.level = data.level;
    if (data.msPerIteration !== undefined) stats.msPerIteration = data.msPerIteration;
    if (data.buildMs !== undefined) stats.buildMs = data.buildMs;
    if (data.workerInitMs !== undefined) stats.workerInitMs = data.workerInitMs;
  } else if (data.type === "done") {
    stats.layoutDone = true;
    stats.layoutMs = data.layoutMs;
    stats.layoutDoneAt = performance.now();
  }
};
const multilevel = params.get("multilevel") !== "0";
const maxIterations = Number(params.get("iterations") ?? 300);
stats.multilevel = multilevel;
worker.postMessage({ type: "start", n: g.n, src: g.src, dst: g.dst, maxIterations, multilevel }, []);

// Frames: draw every animation frame, recording how long each draw takes,
// and the positions uploaded (8 bytes a node each time).
stats.uploads = 0;
stats.uploadBytes = 0;
const frameMs = [];
let frames = 0, since = performance.now();
function frame() {
  if (paused) { requestAnimationFrame(frame); return; }
  const t = performance.now();
  let uploaded = false;
  if (pending) {
    view.setPositions(pending);
    stats.uploads++;
    stats.uploadBytes += pending.byteLength;
    pending = null;
    uploaded = true;
  }
  view.render();
  if (uploaded && stats.firstFrameAt === undefined) stats.firstFrameAt = performance.now();
  frameMs.push(performance.now() - t);
  if (frameMs.length > 120) frameMs.shift();
  frames++;
  const now = performance.now();
  if (now - since >= 1000) {
    stats.fps = (frames * 1000) / (now - since);
    frames = 0; since = now;
    const sorted = [...frameMs].sort((a, b) => a - b);
    stats.drawMsMedian = sorted[Math.floor(sorted.length / 2)];
    statsEl.textContent =
      `${stats.backend} · ${stats.nodes.toLocaleString()} nodes · ${stats.edges.toLocaleString()} edges\n` +
      (multilevel
        ? `layout coarse to fine, level ${stats.level ?? "–"} · ${stats.iterations} it of work` +
          (stats.layoutDone ? ` · done in ${(stats.layoutMs / 1000).toFixed(1)} s` : "") + "\n"
        : `layout ${stats.iterations}/${maxIterations} it · ${stats.msPerIteration?.toFixed(1) ?? "–"} ms/it (worker)` +
          (stats.layoutDone ? ` · done in ${(stats.layoutMs / 1000).toFixed(1)} s` : "") + "\n") +
      `draw ${stats.drawMsMedian.toFixed(2)} ms · ${stats.fps.toFixed(0)} fps` + (stats.picked !== undefined ? `\nnode ${stats.picked}` : "");
  }
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);

// Input.
let drag = null;
canvas.addEventListener("pointerdown", (e) => { drag = { x: e.clientX, y: e.clientY, moved: false }; canvas.setPointerCapture(e.pointerId); });
canvas.addEventListener("pointermove", (e) => {
  if (!drag) return;
  const dx = (e.clientX - drag.x) * dpr, dy = (e.clientY - drag.y) * dpr;
  if (Math.abs(dx) + Math.abs(dy) > 2) drag.moved = true;
  view.pan(dx, dy);
  drag.x = e.clientX; drag.y = e.clientY;
});
canvas.addEventListener("pointerup", (e) => {
  if (drag && !drag.moved) stats.picked = view.pick(e.offsetX * dpr, e.offsetY * dpr);
  drag = null;
});
canvas.addEventListener("wheel", (e) => { e.preventDefault(); view.zoom(Math.exp(-e.deltaY * 0.0015), e.offsetX * dpr, e.offsetY * dpr); }, { passive: false });
canvas.addEventListener("dblclick", () => view.fit());
new ResizeObserver(() => {
  canvas.width = Math.round(canvas.clientWidth * dpr);
  canvas.height = Math.round(canvas.clientHeight * dpr);
  view.resize(canvas.width, canvas.height);
}).observe(canvas);
