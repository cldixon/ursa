// The graph view, bare: a generated graph, its layout running in a worker,
// drawn every frame. Drag to pan, wheel to zoom, double-click to fit.
// ?graph=rmat&scale=17&edges=1000000 | grid | clusters | karate;
// &multilevel=0 for flat ForceAtlas2 (coarse to fine is the default);
// &theme=dark. &layout=rings&seeds=0&hops=2 draws a neighbourhood as
// rings, and &layout=layers&from=16&to=26 the shortest paths as layers
// (graphs.js).
//
// The highlight layer: the pointer's node is hovered, a click selects it
// (shift-click adds to the selection, a click on nothing clears it), and a
// second selected node draws the shortest path from the first. With a
// selection, `p` pins or unpins it, `h` hides it and `u` shows everything
// again; Escape clears the path and the selection. &highlight=path starts
// with a path drawn (on karate, one of five edges).
import init, { createView } from "../pkg/ursa_viz.js";
import { fromParams, shortestPath } from "./graphs.js";

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
const dark = params.get("theme") === "dark";
if (dark) {
  view.setThemeNamed("dark");
  document.documentElement.classList.add("dark");
}
if (g.community) view.colorBy(g.community);
window.view = view;
// Path `nodes`, in order, highlighted: UGR's sections, for one path.
const highlightPath = (nodes) => view.setHighlights(Uint32Array.of(0, nodes.length), Uint32Array.from(nodes));
// On karate, a path of five edges; else from node 0 to the last node reached.
if (params.get("highlight") === "path") {
  const path = params.get("graph") === "karate" ? [16, 5, 0, 2, 32, 14] : shortestPath(g, g.src[0], g.dst[g.dst.length - 1]);
  if (path) highlightPath(path);
}
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
    stats.positionMessages = (stats.positionMessages ?? 0) + 1;
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
stats.layout = g.layout ?? "force";
worker.postMessage({ type: "start", n: g.n, src: g.src, dst: g.dst, maxIterations, multilevel, layout: g.layout, level: g.level }, []);

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
      (g.layout
        ? `layout ${g.layout}` + (stats.layoutDone ? ` · done in ${stats.layoutMs.toFixed(1)} ms` : "") + "\n"
        : multilevel
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
const selection = new Set(), pinned = new Set(), hidden = new Set();
let hovered;
canvas.addEventListener("pointermove", (e) => {
  if (!drag) {
    // Hover: one node's flags change, so a few bytes are uploaded.
    const at = view.pick(e.offsetX * dpr, e.offsetY * dpr);
    if (at !== hovered) view.setHovered((hovered = at));
    return;
  }
  const dx = (e.clientX - drag.x) * dpr, dy = (e.clientY - drag.y) * dpr;
  if (Math.abs(dx) + Math.abs(dy) > 2) drag.moved = true;
  view.pan(dx, dy);
  drag.x = e.clientX; drag.y = e.clientY;
});
canvas.addEventListener("pointerup", (e) => {
  if (drag && !drag.moved) {
    const at = (stats.picked = view.pick(e.offsetX * dpr, e.offsetY * dpr));
    if (!e.shiftKey || at === undefined) selection.clear();
    if (at !== undefined) selection.add(at);
    view.setSelection(Uint32Array.from(selection));
    // Two nodes selected: the shortest path between them.
    const [a, b] = [...selection];
    const path = selection.size === 2 ? shortestPath(g, a, b) : null;
    if (path) highlightPath(path);
    else view.clearHighlights();
  }
  drag = null;
});
window.addEventListener("keydown", (e) => {
  if (e.key === "p") {
    for (const v of selection) pinned.has(v) ? pinned.delete(v) : pinned.add(v);
    view.setPinned(Uint32Array.from(pinned));
  } else if (e.key === "h") {
    for (const v of selection) hidden.add(v);
    selection.clear();
    view.setSelection(new Uint32Array());
    view.setHidden(Uint32Array.from(hidden));
  } else if (e.key === "u") {
    hidden.clear();
    view.setHidden(new Uint32Array());
  } else if (e.key === "Escape") {
    selection.clear();
    view.setSelection(new Uint32Array());
    view.clearHighlights();
  }
});
canvas.addEventListener("wheel", (e) => { e.preventDefault(); view.zoom(Math.exp(-e.deltaY * 0.0015), e.offsetX * dpr, e.offsetY * dpr); }, { passive: false });
canvas.addEventListener("dblclick", () => view.fit());
new ResizeObserver(() => {
  canvas.width = Math.round(canvas.clientWidth * dpr);
  canvas.height = Math.round(canvas.clientHeight * dpr);
  view.resize(canvas.width, canvas.height);
}).observe(canvas);
