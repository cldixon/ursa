// The graph view, bare: a generated graph, its layout running in a worker,
// drawn every frame. Drag to pan, wheel to zoom, double-click to fit, click a
// node to see its index. ?graph=rmat&scale=17&edges=1000000 | grid | clusters
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
await init();
const g = fromParams(params);
stats.generateMs = performance.now() - t0;
const t1 = performance.now();
const view = await createView(canvas, g.n, g.src, g.dst);
stats.createMs = performance.now() - t1;
if (g.community) view.colorBy(g.community);
window.view = view;
// An image of the view; frames pause while it is taken.
let paused = false;
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
    stats.iterations = data.iterations;
    stats.msPerIteration = data.msPerIteration;
  } else if (data.type === "done") {
    stats.layoutDone = true;
  }
};
const maxIterations = Number(params.get("iterations") ?? 300);
worker.postMessage({ type: "start", n: g.n, src: g.src, dst: g.dst, maxIterations }, []);

// Frames: draw every animation frame, recording how long each draw takes.
const frameMs = [];
let frames = 0, since = performance.now();
function frame() {
  if (paused) { requestAnimationFrame(frame); return; }
  const t = performance.now();
  if (pending) { view.setPositions(pending); pending = null; }
  view.render();
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
      `layout ${stats.iterations}/${maxIterations} it · ${stats.msPerIteration?.toFixed(1) ?? "–"} ms/it (worker)\n` +
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
