// The graph view, bare: a generated graph, its layout running in a worker,
// drawn every frame. Drag to pan, wheel to zoom, double-click to fit.
// ?graph=rmat&scale=17&edges=1000000 | grid | clusters | karate
// (&compact=1 drops the nodes no edge names);
// &multilevel=0 for flat ForceAtlas2 (coarse to fine is the default);
// &theme=dark.
//
// The highlight layer: the pointer's node is hovered, a click selects it
// (shift-click adds to the selection, a click on nothing clears it), and a
// second selected node draws the shortest path from the first. With a
// selection, `p` pins or unpins it, `h` hides it and `u` shows everything
// again; Escape clears the path and the selection. &highlight=path starts
// with a path drawn (on karate, one of five edges).
//
// Scenes that change in place: &result=1 makes the view from a graph result
// (UGR), so deltas apply to it. Once the layout is done, `e` expands the
// selected node (five new nodes and their edges, one delta) and `z` undoes
// the last expansion (the inverse delta, and the positions saved before
// it). &result=xy gives the result positions (a circle), so a delta's new
// nodes must bring their own: for the tests.
import init, * as viz from "../pkg/ursa_viz.js";
import { fromParams, shortestPath } from "./graphs.js";

const params = new URLSearchParams(location.search);
const canvas = document.getElementById("view");
const statsEl = document.getElementById("stats");
const dpr = window.devicePixelRatio || 1;
canvas.width = Math.round(canvas.clientWidth * dpr);
canvas.height = Math.round(canvas.clientHeight * dpr);

const stats = (window.stats = { phase: "loading" });
// The page's tasks over 50 ms: `[start, duration]`, on the page's clock.
stats.longTasks = [];
new PerformanceObserver((list) => {
  for (const e of list.getEntries()) stats.longTasks.push([e.startTime, e.duration]);
}).observe({ type: "longtask", buffered: true });

const t0 = performance.now();
// The wasm's exports: `memory` is what the bench reads.
window.wasm = await init();
stats.initMs = performance.now() - t0;
const tg = performance.now();
const g = fromParams(params);
stats.generateMs = performance.now() - tg;
// A task of its own for making the view, so that what it costs the page's
// thread is not counted with making the graph.
await new Promise((resolve) => setTimeout(resolve));
const t1 = performance.now();
// Page-clock marks (ms since navigation), for the bench: when the graph was
// ready, the first frame with layout positions, the layout's end.
stats.graphReadyAt = t1;
const fromResult = params.get("result");
const circle = () => Float32Array.from({ length: 2 * g.n }, (_, i) => (i < g.n ? Math.cos : Math.sin)((2 * Math.PI * (i % g.n)) / g.n));
const view = fromResult
  ? await viz.createViewFromResult(canvas, viz.resultFromEdges(g.n, g.src, g.dst, fromResult === "xy" ? circle() : undefined))
  : await viz.createView(canvas, g.n, g.src, g.dst);
window.viz = viz;
stats.createMs = performance.now() - t1;
stats.createWasmBytes = window.wasm.memory.buffer.byteLength;
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
  // Positions for the graph as it was before a delta are dropped.
  if (pending && pending.length !== 2 * view.nodeCount) pending = null;
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
  } else if (e.key === "e" && fromResult === "1" && stats.layoutDone && selection.size) {
    // One delta: five new nodes, each joined to the selected node, and in a ring.
    const [at] = selection, n = view.nodeCount, fresh = [0, 1, 2, 3, 4].map((k) => n + k);
    const delta = new viz.DeltaBuilder()
      .addNodes(n, 5)
      .addEdges(Uint32Array.from([...fresh, ...fresh]), Uint32Array.from([...fresh.map(() => at), ...fresh.map((_, k) => n + ((k + 1) % 5))]))
      .toBytes();
    const positions = view.positions();
    const applied = view.applyDelta(delta);
    undo.push({ delta, positions });
    Object.assign(stats, { nodes: applied.nodes, edges: applied.edges, deltaBytes: applied.bytesUploaded });
  } else if (e.key === "z" && undo.length) {
    // The inverse delta, and every node back where it was: no query.
    const { delta, positions } = undo.pop();
    const applied = view.applyDelta(viz.invertDelta(delta), positions);
    Object.assign(stats, { nodes: applied.nodes, edges: applied.edges, deltaBytes: applied.bytesUploaded });
  }
});
const undo = [];
canvas.addEventListener("wheel", (e) => { e.preventDefault(); view.zoom(Math.exp(-e.deltaY * 0.0015), e.offsetX * dpr, e.offsetY * dpr); }, { passive: false });
canvas.addEventListener("dblclick", () => view.fit());
new ResizeObserver(() => {
  canvas.width = Math.round(canvas.clientWidth * dpr);
  canvas.height = Math.round(canvas.clientHeight * dpr);
  view.resize(canvas.width, canvas.height);
}).observe(canvas);
