// The Graph view (`.graph`): a result's graph drawn by ursa-viz (wgpu, on
// WebGPU or else WebGL2) on a canvas in the pane beside the terminal, its
// ForceAtlas2 layout converging live in a worker (graph-worker.js).
//
// app.js imports this module on the first `.graph` only, and this module
// loads ursa-viz (a few MB, named in /viz/viz.json by its build's hash) on
// the first drawing, so a session that never draws a graph downloads none of
// it. app.js owns the pane (opening, closing, Esc); this module owns what is
// in it: the canvas, the view, the layout and the pointer and keys.
//
// Drag (or the arrow keys) pans, the wheel (or + and -) zooms, a double-click
// (or 0) fits the graph in view, and a click on a node names it in the
// pane's header. Colours are the view's own, ink on white; the dark theme
// inverts the canvas in CSS (style.css), so nothing here knows the theme.

let viz = null;
/** ursa-viz, loaded and initialised once. */
function loadViz() {
  viz ??= (async () => {
    const manifest = await (await fetch("/viz/viz.json")).json();
    const module = await import(manifest.module);
    await module.default();
    return { module, url: new URL(manifest.module, location.origin).href };
  })().catch((e) => {
    viz = null; // try again next time
    throw e;
  });
  return viz;
}

// What is drawn now, or null.
let current = null;
// Bumped by every drawing and close: a drawing started before is dropped.
let seq = 0;

const number = (n) => n.toLocaleString("en-US");
const plural = (n, one) => `${number(n)} ${one}${n === 1 ? "" : "s"}`;

/**
 * Draw `graph` ({n, src, dst, ids}: dense node indexes and each node's id) in
 * `stage`, replacing what was drawn. `status` is the pane's header line, and
 * `onError(message)` is called if the view fails once drawing (a lost GPU
 * device or WebGL context, a layout that stopped). Resolves once the first
 * frame is drawn, to {nodes, edges, backend}.
 */
export async function show(stage, graph, { status, onError }) {
  close();
  const mine = ++seq;
  const { module, url } = await loadViz();
  if (mine !== seq) throw new Error("replaced by a newer drawing");

  const dpr = window.devicePixelRatio || 1;
  const canvas = document.createElement("canvas");
  canvas.tabIndex = 0;
  canvas.setAttribute("role", "application");
  canvas.setAttribute("aria-roledescription", "graph");
  canvas.setAttribute(
    "aria-label",
    `Graph of ${plural(graph.n, "node")} and ${plural(graph.src.length, "edge")}. ` +
      "Drag or arrow keys to pan, wheel or plus and minus to zoom, double-click or 0 to fit, click a node to name it.",
  );
  stage.replaceChildren(canvas);
  const size = () => [Math.max(1, Math.round(canvas.clientWidth * dpr)), Math.max(1, Math.round(canvas.clientHeight * dpr))];
  [canvas.width, canvas.height] = size();

  const degree = new Float32Array(graph.n);
  for (let i = 0; i < graph.src.length; i++) {
    degree[graph.src[i]]++;
    degree[graph.dst[i]]++;
  }
  let view;
  try {
    view = await module.createView(canvas, graph.n, graph.src, graph.dst);
  } catch (e) {
    canvas.remove();
    throw new Error(String(e?.message ?? e));
  }
  if (mine !== seq) {
    view.free();
    canvas.remove();
    throw new Error("replaced by a newer drawing");
  }
  // Hubs read as hubs: radius by degree, in CSS pixels.
  view.sizeBy(degree, 1.5 * dpr, 8 * dpr);
  view.fit();

  const state = {
    canvas,
    view,
    worker: null,
    ids: graph.ids,
    degree,
    nodes: graph.n,
    edges: graph.src.length,
    backend: view.backend === "Gl" ? "WebGL2" : "WebGPU",
    iterations: 0,
    layoutDone: false,
    picked: null,
    frames: 0,
    paused: false,
    pending: null,
    frame: 0,
    cleanup: [],
  };
  current = state;
  // The header: the graph's size, and the node last clicked.
  const header = () => {
    const size = `${plural(state.nodes, "node")} · ${plural(state.edges, "edge")}`;
    const picked = state.picked;
    status.textContent = picked ? `${size} · node ${picked.id} (degree ${number(picked.degree)})` : `${size} · ${state.backend}`;
  };
  header();

  let failed = false;
  const fail = (message) => {
    if (failed || current !== state) return;
    failed = true;
    close();
    onError(message);
  };

  // Frames: drawn when something changed (new positions, a pan or zoom), not
  // every animation frame, so an idle view costs nothing.
  const draw = () => {
    state.frame = 0;
    if (current !== state || state.paused) return;
    if (state.pending) {
      view.setPositions(state.pending);
      state.pending = null;
    }
    try {
      view.render();
      state.frames++;
    } catch (e) {
      fail(`the graph view stopped drawing: ${String(e?.message ?? e)}`);
    }
  };
  const redraw = () => {
    if (!state.frame) state.frame = requestAnimationFrame(draw);
  };
  state.redraw = redraw;
  redraw();

  // The layout, in a worker of its own: a slow iteration never holds up a frame.
  const worker = new Worker(new URL("./graph-worker.js", import.meta.url), { type: "module" });
  state.worker = worker;
  worker.onmessage = ({ data }) => {
    if (current !== state) return;
    if (data.type === "positions") {
      state.pending = data.xy;
      state.iterations = data.iterations;
      redraw();
    } else if (data.type === "done") {
      state.layoutDone = true;
    } else if (data.type === "error") {
      fail(`the layout stopped: ${data.message}`);
    }
  };
  worker.onerror = (e) => {
    e.preventDefault();
    fail(`the layout stopped: ${e.message || "its worker failed"}`);
  };
  // Small graphs settle in a few hundred iterations; large ones get fewer,
  // as each costs more (half a second at a million edges).
  const maxIterations = state.edges > 200_000 ? 300 : state.edges > 20_000 ? 600 : 1000;
  const src = graph.src.slice();
  const dst = graph.dst.slice();
  worker.postMessage({ type: "start", module: url, n: graph.n, src, dst, maxIterations }, [src.buffer, dst.buffer]);

  // A lost GPU: WebGL2 says so on the canvas; WebGPU's loss shows as a frame
  // that fails to draw (above).
  const on = (target, type, fn, opts) => {
    target.addEventListener(type, fn, opts);
    state.cleanup.push(() => target.removeEventListener(type, fn, opts));
  };
  on(canvas, "webglcontextlost", (e) => {
    e.preventDefault();
    fail("the graph view lost its WebGL context (the GPU was reset or is busy elsewhere); run .graph to draw it again");
  });

  // Pointer: drag to pan, wheel to zoom, double-click to fit, click to pick.
  let drag = null;
  on(canvas, "pointerdown", (e) => {
    if (e.button !== 0) return;
    drag = { x: e.clientX, y: e.clientY, moved: false };
    canvas.setPointerCapture(e.pointerId);
  });
  on(canvas, "pointermove", (e) => {
    if (!drag) return;
    const dx = (e.clientX - drag.x) * dpr;
    const dy = (e.clientY - drag.y) * dpr;
    if (Math.abs(dx) + Math.abs(dy) > 2) drag.moved = true;
    view.pan(dx, dy);
    drag.x = e.clientX;
    drag.y = e.clientY;
    redraw();
  });
  on(canvas, "pointerup", (e) => {
    if (drag && !drag.moved) pick(e.offsetX * dpr, e.offsetY * dpr);
    drag = null;
  });
  on(canvas, "pointercancel", () => (drag = null));
  on(
    canvas,
    "wheel",
    (e) => {
      e.preventDefault();
      view.zoom(Math.exp(-e.deltaY * 0.0015), e.offsetX * dpr, e.offsetY * dpr);
      redraw();
    },
    { passive: false },
  );
  on(canvas, "dblclick", () => {
    view.fit();
    redraw();
  });
  on(canvas, "keydown", (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    const step = 40 * dpr;
    const pan = { ArrowLeft: [step, 0], ArrowRight: [-step, 0], ArrowUp: [0, step], ArrowDown: [0, -step] }[e.key];
    const middle = [canvas.width / 2, canvas.height / 2];
    if (pan) view.pan(...pan);
    else if (e.key === "+" || e.key === "=") view.zoom(1.25, ...middle);
    else if (e.key === "-" || e.key === "_") view.zoom(0.8, ...middle);
    else if (e.key === "0") view.fit();
    else return;
    e.preventDefault();
    redraw();
  });

  function pick(x, y) {
    const node = view.pick(x, y);
    state.picked = node === undefined ? null : { index: node, id: state.ids[node], degree: state.degree[node] };
    header();
  }

  // The pane changes size with the window, and the terminal beside it.
  const ro = new ResizeObserver(() => {
    const [w, h] = size();
    if (w === canvas.width && h === canvas.height) return;
    canvas.width = w;
    canvas.height = h;
    view.resize(w, h);
    redraw();
  });
  ro.observe(canvas);
  state.cleanup.push(() => ro.disconnect());

  return { nodes: state.nodes, edges: state.edges, backend: state.backend };
}

/** Stop the layout and drop the view and its canvas. */
export function close() {
  seq++;
  const state = current;
  if (!state) return;
  current = null;
  if (state.frame) cancelAnimationFrame(state.frame);
  state.worker?.terminate();
  for (const undo of state.cleanup) undo();
  state.view.free();
  state.canvas.remove();
}

/** For tests: what is drawn. */
export function info() {
  if (!current) return null;
  const { nodes, edges, backend, iterations, layoutDone, picked, frames } = current;
  return { nodes, edges, backend, iterations, layoutDone, picked, frames, width: current.canvas.width, height: current.canvas.height };
}

/**
 * For tests: a frame drawn offscreen, summarised: its size and how many of
 * its pixels are not the white background.
 */
export async function inked() {
  const state = current;
  if (!state) return null;
  state.paused = true;
  try {
    const px = await state.view.snapshot();
    let count = 0;
    for (let i = 0; i < px.length; i += 4) if (px[i] < 250 || px[i + 1] < 250 || px[i + 2] < 250) count++;
    return { width: state.canvas.width, height: state.canvas.height, inked: count };
  } finally {
    state.paused = false;
    state.redraw?.();
  }
}

/** For tests: the node drawn at the canvas pixel (x, y), as a click picks it, or null. */
export function nodeAt(x, y) {
  const node = current?.view.pick(x, y);
  return node === undefined ? null : node;
}
