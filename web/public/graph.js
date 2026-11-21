// The Graph view (`.graph`): a result's graph drawn by ursa-viz (wgpu, on
// WebGPU or else WebGL2) on a canvas in the pane beside the terminal, its
// coarse-to-fine ForceAtlas2 layout converging live in a worker
// (graph-worker.js).
//
// app.js imports this module on the first `.graph` only, and this module
// loads ursa-viz (a few MB, named in /viz/viz.json by its build's hash) on
// the first drawing, so a session that never draws a graph downloads none of
// it. app.js owns the pane (opening, closing, Esc); this module owns what is
// in it: the status line, the legend, the canvas, the view, the layout and
// the pointer and keys.
//
// Drag (or the arrow keys) pans, the wheel (or + and -) zooms, a double-click
// (or 0) fits the graph in view, and a click on a node names it in the
// pane's header. On a touch screen one finger pans, two pinch to zoom (and
// pan), a tap names a node and a double tap fits. Colours are the page's --graph-* tokens (style.css), read
// again when the theme changes (retheme).
//
// Time to the first drawing: preload() starts fetching ursa-viz and a layout
// worker (which loads it too) while the engine is still running the query;
// show() hands the worker the graph before it builds the view, so the
// layout's hierarchy is built while the page sets up its canvas; and nothing
// is drawn until the layout's first positions, not a random cloud first.

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

// A layout worker started ahead of the graph (preload), its ursa-viz loaded:
// {worker, ready}, or null.
let spare = null;

/**
 * Start what a drawing needs before the graph is known: ursa-viz, here and
 * in a layout worker. Safe to call more than once; resolves when both are
 * ready, and never rejects (show() reports a failure).
 */
export function preload() {
  const page = loadViz();
  if (!spare) {
    const worker = new Worker(new URL("./graph-worker.js", import.meta.url), { type: "module" });
    const ready = page.then(
      ({ url }) =>
        new Promise((resolve) => {
          worker.addEventListener("message", function once({ data }) {
            if (data.type !== "ready" && data.type !== "error") return;
            worker.removeEventListener("message", once);
            resolve(data);
          });
          worker.postMessage({ type: "init", module: url });
        }),
    );
    spare = { worker, ready };
  }
  return Promise.all([page, spare.ready]).catch(() => {});
}

/** The spare worker, or a new one (the page's ursa-viz `url` given to it). */
function takeWorker(url) {
  if (spare) {
    const { worker } = spare;
    spare = null;
    return worker;
  }
  const worker = new Worker(new URL("./graph-worker.js", import.meta.url), { type: "module" });
  worker.postMessage({ type: "init", module: url });
  return worker;
}

// What is drawn now, or null.
let current = null;
// Bumped by every drawing and close: a drawing started before is dropped.
let seq = 0;

const number = (n) => n.toLocaleString("en-US");
const plural = (n, one) => `${number(n)} ${one}${n === 1 ? "" : "s"}`;
/** A value for the header and legend: short, and as SQL would print a whole number. */
const short = (v) => (Number.isInteger(v) ? number(v) : Math.abs(v) >= 1e-3 && Math.abs(v) < 1e5 ? String(Number(v.toPrecision(3))) : v.toExponential(2));

/** The page's colours for the view, from its --graph-* tokens (style.css). */
export function theme() {
  const css = getComputedStyle(document.documentElement);
  const token = (name) => css.getPropertyValue(`--graph-${name}`).trim();
  return {
    background: token("bg"),
    edge: token("edge"),
    node: token("node"),
    // COLOR BY's eight categories, then "other" (index 8, as the engine numbers it).
    palette: [0, 1, 2, 3, 4, 5, 6, 7].map((i) => token(`c${i}`)).concat(token("other")),
  };
}

/** The most canvas pixels drawn for each CSS pixel. */
const MAX_PIXEL_RATIO = 2;
/** Two taps this close, in ms and in CSS pixels, are a double tap. */
const DOUBLE_TAP_MS = 300;
const DOUBLE_TAP_PX = 30;
/** A finger that moves no further than this, in CSS pixels, taps; a mouse has less. */
const TAP_SLOP_PX = 8;

/**
 * What two pointers ask of the view when they move from `a0`, `b0` to `a1`,
 * `b1` ({x, y} each, in canvas pixels): a pan by their midpoint's movement
 * `(dx, dy)`, then a zoom by `factor`, the change in their distance, around
 * the new midpoint `(x, y)`. So what was between the fingers stays between
 * them. Fingers that start on one point zoom by nothing.
 */
export function pinch(a0, b0, a1, b1) {
  const mid = (a, b) => [(a.x + b.x) / 2, (a.y + b.y) / 2];
  const [x0, y0] = mid(a0, b0);
  const [x, y] = mid(a1, b1);
  const before = Math.hypot(a0.x - b0.x, a0.y - b0.y);
  const after = Math.hypot(a1.x - b1.x, a1.y - b1.y);
  return { dx: x - x0, dy: y - y0, factor: before > 0 && after > 0 ? after / before : 1, x, y };
}

/**
 * Draw `graph` (from the engine: {n, src, dst, ids}, dense node indexes and
 * each node's id, and with SIZE BY or COLOR BY, {size, sizeBy} and {color,
 * colorBy, colorLabels, colorCounts, colorOther, colorMissing}) in `stage`,
 * replacing what was drawn. `status` is the pane's header line and `legend`
 * the legend under it; `onError(message)` is called if the view fails once
 * drawing (a lost GPU device or WebGL context, a layout that stopped).
 * `times` gets the moments each step ended (performance.now()), for
 * graphTimings. Resolves once the view is made (it draws when the layout's
 * first positions come), to {nodes, edges, backend}.
 */
export async function show(stage, graph, { status, legend, onError, times = {} }) {
  close();
  const mine = ++seq;
  status.textContent = `Loading graph view… ${plural(graph.n, "node")} · ${plural(graph.src.length, "edge")}`;
  const { module, url } = await loadViz();
  times.viz ??= performance.now();
  if (mine !== seq) throw new Error("replaced by a newer drawing");

  // The layout first: its hierarchy is built while the view is made here.
  const worker = takeWorker(url);
  {
    const src = graph.src.slice();
    const dst = graph.dst.slice();
    // graph.layout, {kind: "rings" | "layers", level}, asks for a fixed layout
    // (graph-worker.js): each node's distance or layer in `level`.
    worker.postMessage({ type: "start", n: graph.n, src, dst, module: url, layout: graph.layout }, [src.buffer, dst.buffer]);
    times.posted = performance.now();
  }

  // At most 2 canvas pixels a CSS pixel: a phone at 3 would fill 2.25 times
  // the pixels each frame for a picture no sharper to the eye.
  const dpr = Math.min(window.devicePixelRatio || 1, MAX_PIXEL_RATIO);
  const canvas = document.createElement("canvas");
  canvas.tabIndex = 0;
  canvas.setAttribute("role", "application");
  canvas.setAttribute("aria-roledescription", "graph");
  const styled = [graph.sizeBy && `sized by ${graph.sizeBy}`, graph.colorBy && `coloured by ${graph.colorBy}`].filter(Boolean).join(" and ");
  canvas.setAttribute(
    "aria-label",
    `Graph of ${plural(graph.n, "node")} and ${plural(graph.src.length, "edge")}${styled ? `, ${styled} (see the legend)` : ""}. ` +
      "Drag or arrow keys to pan, wheel, pinch or plus and minus to zoom, double-click, double tap or 0 to fit, click a node to name it.",
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
    worker.terminate();
    canvas.remove();
    throw new Error(String(e?.message ?? e));
  }
  times.view = performance.now();
  if (mine !== seq) {
    worker.terminate();
    view.free();
    canvas.remove();
    throw new Error("replaced by a newer drawing");
  }
  view.setTheme(theme());
  // SIZE BY's values, or else degree, so hubs read as hubs: radius in CSS pixels.
  if (graph.size) view.sizeBy(graph.size, 3 * dpr, 14 * dpr);
  else view.sizeBy(degree, 1.5 * dpr, 8 * dpr);
  if (graph.color) view.colorBy(graph.color);
  view.fit();

  const state = {
    canvas,
    view,
    worker,
    ids: graph.ids,
    degree,
    graph,
    nodes: graph.n,
    edges: graph.src.length,
    backend: view.backend === "Gl" ? "WebGL2" : "WebGPU",
    iterations: 0,
    level: null,
    levels: null,
    // Nothing is drawn before the layout's first positions.
    positioned: false,
    layoutDone: false,
    picked: null,
    frames: 0,
    paused: false,
    pending: null,
    frame: 0,
    times,
    cleanup: [],
  };
  current = state;
  renderLegend(legend, graph);

  // The header: the graph's size and how far the layout is, or the node last clicked.
  const header = () => {
    const counts = `${plural(state.nodes, "node")} · ${plural(state.edges, "edge")}`;
    const picked = state.picked;
    let text;
    if (picked) {
      const extra = [];
      if (graph.size) extra.push(`${graph.sizeBy} ${Number.isNaN(graph.size[picked.index]) ? "none" : short(graph.size[picked.index])}`);
      if (graph.color) extra.push(`${graph.colorBy} ${categoryName(graph, graph.color[picked.index], picked.index)}`);
      text = `${counts} · node ${picked.id} (degree ${number(picked.degree)})${extra.map((e) => ` · ${e}`).join("")}`;
    } else if (state.layoutDone) {
      text = `${counts} · Done`;
    } else if (state.levels !== null) {
      text = `${counts} · Laying out · level ${state.levels - state.level} of ${state.levels}`;
    } else {
      text = `Loading graph view… ${counts}`;
    }
    // Only when it changes: the line is a live region.
    if (status.textContent !== text) status.textContent = text;
    status.title = `${counts} · drawn with ${state.backend}`;
  };
  state.header = header;
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
    if (current !== state || state.paused || !state.positioned) return;
    if (state.pending) {
      view.setPositions(state.pending);
      state.pending = null;
    }
    try {
      view.render();
      state.frames++;
      times.firstDraw ??= performance.now();
    } catch (e) {
      fail(`the graph view stopped drawing: ${String(e?.message ?? e)}`);
    }
  };
  const redraw = () => {
    if (!state.frame) state.frame = requestAnimationFrame(draw);
  };
  state.redraw = redraw;

  // The layout, in a worker of its own: a slow iteration never holds up a frame.
  worker.onmessage = ({ data }) => {
    if (current !== state) return;
    if (data.times) for (const [k, v] of Object.entries(data.times)) times[k] ??= v - (performance.timeOrigin - data.origin);
    if (data.type === "positions") {
      times.firstPositions ??= performance.now();
      state.pending = data.xy;
      state.iterations = data.iterations;
      state.level = data.level;
      state.levels = data.levels;
      state.positioned = true;
      header();
      redraw();
    } else if (data.type === "done") {
      times.done ??= performance.now();
      state.layoutDone = true;
      header();
    } else if (data.type === "error") {
      fail(`the layout stopped: ${data.message}`);
    }
  };
  worker.onerror = (e) => {
    e.preventDefault();
    fail(`the layout stopped: ${e.message || "its worker failed"}`);
  };

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

  // Pointers: one drags to pan, and a press without a drag picks the node
  // under it. Two pinch: they zoom by the change in their distance, around
  // the point between them, and pan as that point moves. Each pointer's
  // place is kept in canvas pixels (`at`), by its id.
  const pointers = new Map();
  // The one-pointer gesture: {id, moved}, moved once it is a drag or was part
  // of a pinch, and so no tap.
  let drag = null;
  // The last tap of a finger or pen, {t, x, y}: a second one beside it fits.
  let lastTap = null;
  const at = (e) => {
    const box = canvas.getBoundingClientRect();
    return { x: (e.clientX - box.left) * dpr, y: (e.clientY - box.top) * dpr };
  };
  on(canvas, "pointerdown", (e) => {
    if (e.button !== 0) return;
    pointers.set(e.pointerId, at(e));
    // A second finger: what the first began is no tap. A third is ignored.
    if (pointers.size === 1) drag = { id: e.pointerId, moved: false, from: pointers.get(e.pointerId), slop: (e.pointerType === "mouse" ? 1.5 : TAP_SLOP_PX) * dpr };
    else if (drag) drag.moved = true;
    try {
      canvas.setPointerCapture(e.pointerId);
    } catch {
      // The pointer is gone already: its moves come while it is over the canvas.
    }
  });
  on(canvas, "pointermove", (e) => {
    const was = pointers.get(e.pointerId);
    if (!was) return;
    const now = at(e);
    const [first, second] = pointers.keys();
    if (second !== undefined) {
      // The first two pointers pinch; any other only rests on the glass.
      if (e.pointerId === first || e.pointerId === second) {
        const other = pointers.get(e.pointerId === first ? second : first);
        const { dx, dy, factor, x, y } = pinch(was, other, now, other);
        view.pan(dx, dy);
        view.zoom(factor, x, y);
        redraw();
      }
    } else {
      const [dx, dy] = [now.x - was.x, now.y - was.y];
      // A press that slips a little is still a click or a tap.
      if (drag && !drag.moved && Math.hypot(now.x - drag.from.x, now.y - drag.from.y) > drag.slop) drag.moved = true;
      view.pan(dx, dy);
      redraw();
    }
    pointers.set(e.pointerId, now);
  });
  const release = (e, cancelled) => {
    if (!pointers.delete(e.pointerId)) return;
    const tap = !cancelled && drag && drag.id === e.pointerId && !drag.moved;
    // A finger lifted from a pinch leaves the other panning, and no tap.
    if (pointers.size === 0) drag = null;
    if (!tap) return;
    const { x, y } = at(e);
    if (e.pointerType !== "mouse") {
      // A mouse's double-click is the browser's (dblclick, below); a
      // finger's or a pen's second tap beside the first is found here.
      const near = lastTap && e.timeStamp - lastTap.t < DOUBLE_TAP_MS && Math.hypot(x - lastTap.x, y - lastTap.y) < DOUBLE_TAP_PX * dpr;
      lastTap = near ? null : { t: e.timeStamp, x, y };
      if (near) {
        view.fit();
        redraw();
        return;
      }
    }
    pick(x, y);
  };
  on(canvas, "pointerup", (e) => release(e, false));
  on(canvas, "pointercancel", (e) => release(e, true));
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
  state.cleanup.push(() => legend.replaceChildren());
  state.cleanup.push(() => (legend.hidden = true));

  return { nodes: state.nodes, edges: state.edges, backend: state.backend };
}

/** A node's COLOR BY category, as the legend names it. */
function categoryName(graph, index, node) {
  if (index < graph.colorLabels.length) return graph.colorLabels[index];
  return graph.colorMissing && graph.colorOther === 0 ? "none" : "other";
}

/**
 * The legend: what sizes the nodes (and its range), and each colour with its
 * category's name and count, so no colour is the only way to tell.
 */
function renderLegend(legend, graph) {
  const parts = [];
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  };
  if (graph.size) {
    let lo = Infinity;
    let hi = -Infinity;
    for (const v of graph.size) {
      if (v < lo) lo = v;
      if (v > hi) hi = v;
    }
    const key = el("span", "size-key");
    key.append(el("span", "dot small"), el("span", "dot large"));
    key.firstChild.setAttribute("aria-hidden", "true");
    key.children[1].setAttribute("aria-hidden", "true");
    const line = el("span", "size");
    line.append(key, " size: ", el("span", "key", graph.sizeBy), Number.isFinite(lo) ? ` ${short(lo)}–${short(hi)}` : "");
    parts.push(line);
  }
  if (graph.color) {
    const head = el("span", "color-head");
    head.append("colour: ", el("span", "key", graph.colorBy));
    parts.push(head);
    const list = el("ul", "colors");
    list.setAttribute("aria-label", `Colours by ${graph.colorBy}`);
    const item = (cls, label, count) => {
      const li = el("li");
      const sw = el("span", `swatch ${cls}`);
      sw.setAttribute("aria-hidden", "true");
      li.append(sw, el("span", "label", label), " ", el("span", "count", `(${number(count)})`));
      list.append(li);
    };
    graph.colorLabels.forEach((label, i) => item(`c${i}`, label, graph.colorCounts[i]));
    if (graph.colorOther) item("other", "other", graph.colorOther);
    if (graph.colorMissing) item("other", "no value", graph.colorMissing);
    parts.push(list);
  }
  legend.replaceChildren(...parts);
  legend.hidden = parts.length === 0;
}

/** Draw in the page's colours again: the theme changed. */
export function retheme() {
  const state = current;
  if (!state) return;
  state.view.setTheme(theme());
  state.redraw();
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
  const { nodes, edges, backend, iterations, level, levels, layoutDone, picked, frames } = current;
  return { nodes, edges, backend, iterations, level, levels, layoutDone, picked, frames, width: current.canvas.width, height: current.canvas.height };
}

/**
 * For tests: a frame drawn offscreen, summarised: its size, its corner pixel
 * (the background), how many pixels are not the background, and how many
 * are each palette colour (`palette[i]`, within a small distance: node
 * fills, not their blend with edges).
 */
export async function inked() {
  const state = current;
  if (!state) return null;
  state.paused = true;
  try {
    const px = await state.view.snapshot();
    const hex = (h) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
    const { background, palette } = theme();
    const bg = hex(background);
    const colors = palette.map(hex);
    const counts = new Array(colors.length).fill(0);
    let count = 0;
    for (let i = 0; i < px.length; i += 4) {
      const [r, g, b] = [px[i], px[i + 1], px[i + 2]];
      if (Math.abs(r - bg[0]) + Math.abs(g - bg[1]) + Math.abs(b - bg[2]) < 15) continue;
      count++;
      for (let c = 0; c < colors.length; c++) {
        const [cr, cg, cb] = colors[c];
        if (Math.abs(r - cr) + Math.abs(g - cg) + Math.abs(b - cb) < 24) {
          counts[c]++;
          break;
        }
      }
    }
    return { width: state.canvas.width, height: state.canvas.height, inked: count, corner: [px[0], px[1], px[2]], palette: counts };
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
