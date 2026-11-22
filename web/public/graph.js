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
// Drag (or the arrow keys) pans, the wheel (or + and -) zooms and a
// double-click (or 0) fits the graph in view. A click on a node selects it,
// Shift+click adds a node to the selection (or takes it out), and a click on
// empty space (or Esc) clears it: the selected nodes carry a ring, their
// edges are drawn wide, and the pane's header, a live region, says what is
// selected ("Selected node 33: …"). A click on an edge (within 4 px of it,
// with no node under the pointer) selects the edge. Whatever is selected, the
// inspector (graph-inspector.js) shows what is known of it, in a panel over
// the right of the view: `i` moves the focus into it, and Esc back. A mouse or a pen over a node rings it and
// shows a tooltip: its key, label, SIZE BY and COLOR BY values and degree.
// On a touch screen one finger pans, two pinch to zoom (and pan), a tap
// selects a node and a double tap fits. The selection itself is ursa-viz's
// (its interact module): this module tells it what was clicked. Colours are
// the page's --graph-* tokens (style.css), read again when the theme changes
// (retheme).
//
// Time to the first drawing: preload() starts fetching ursa-viz and a layout
// worker (which loads it too) while the engine is still running the query;
// show() hands the worker the graph before it builds the view, so the
// layout's hierarchy is built while the page sets up its canvas; and nothing
// is drawn until the layout's first positions, not a random cloud first.
//
// The graph comes from the engine as one UGR buffer (ursa-viz/UGR.md), a
// graph result: ugr.js (ursa-viz/ui/ugr.js, copied in by the build) reads it
// as typed arrays over the buffer, and graph-data.js makes it the drawing.

import { drawingOf } from "./graph-data.js";
import { createInspector } from "./graph-inspector.js";
import { readResult } from "./ugr.js";

import * as timings from "./graph-timings.js";

let viz = null;
/** ursa-viz, loaded and initialised once: its module, its wasm exports and its URL. */
function loadViz() {
  viz ??= (async () => {
    const manifest = await (await fetch("/viz/viz.json")).json();
    const module = await import(manifest.module);
    const wasm = await module.default();
    return { module, wasm, url: new URL(manifest.module, location.origin).href };
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
    // The highlight layer's: paths and halos, selection rings, pins.
    highlight: token("highlight"),
    selection: token("selection"),
    pin: token("pin"),
  };
}

/** The most canvas pixels drawn for each CSS pixel. */
const MAX_PIXEL_RATIO = 2;
/** Two taps this close, in ms and in CSS pixels, are a double tap. */
const DOUBLE_TAP_MS = 300;
const DOUBLE_TAP_PX = 30;
/** A finger that moves no further than this, in CSS pixels, taps; a mouse has less. */
const TAP_SLOP_PX = 8;
/** An edge is picked by a click this near it, in CSS pixels, with no node under the pointer. */
const EDGE_REACH_PX = 4;
/** The header names this many selected nodes, and counts the rest. */
const MAX_KEYS_NAMED = 5;

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
 * Draw the engine's graph (`sent`: {ugr, skipped}, a graph result as a UGR
 * buffer, and the rows left out of it; graph-data.js's drawingOf says what is
 * read from it: dense node indexes, each node's id, and what SIZE BY and
 * COLOR BY made, with the legend) in `stage`, replacing what was drawn. `status` is the pane's header line and `legend`
 * the legend under it; `onError(message)` is called if the view fails once
 * drawing (a lost GPU device or WebGL context, a layout that stopped).
 * `run(command)` types a command in the terminal and runs it, and
 * `inspect(args)` asks the engine for a node's or an edge's row (the engine
 * worker's `inspect`): the inspector's buttons and rows.
 * `times` gets the moments each step ended (performance.now()), for
 * graphTimings. Resolves once the view is made (it draws when the layout's
 * first positions come), to {nodes, edges, backend}.
 */
export async function show(stage, sent, { status, legend, onError, times = {}, run, inspect }) {
  close();
  const graph = drawingOf(readResult(sent.ugr), sent.skipped);
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
  // The view is made in pieces, with turns of the event loop between them
  // (#426), and a small graph's layout is done before the last: what the
  // worker says meanwhile is kept for the handlers below.
  const early = [];
  worker.onmessage = (e) => early.push(["message", e]);
  worker.onerror = (e) => {
    e.preventDefault();
    early.push(["error", e]);
  };

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
      "Drag or arrow keys to pan, wheel, pinch or plus and minus to zoom, double-click, double tap or 0 to fit. " +
      "Click a node to select it, Shift+click to add another, click empty space or press Escape to clear the selection.",
  );
  // The hovered node's tooltip: for the pointer only (the header says what
  // is selected, for everyone), so assistive technology is not told of it.
  const tip = document.createElement("div");
  tip.className = "graph-tip";
  tip.setAttribute("aria-hidden", "true");
  tip.hidden = true;
  stage.replaceChildren(canvas, tip);
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
  // A selection shows its nodes' edges too.
  view.selectionEdges = true;
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
    // The selected nodes' indexes, in the order selected (the view's), and
    // the node under the pointer.
    selection: [],
    // The selected edge, {edge, src, dst} (its index and its ends'), or null.
    edge: null,
    hovered: null,
    hoverPicks: 0,
    tip,
    frames: 0,
    // The layout worker's wasm memory, in bytes, as its last message gave it.
    layoutMemory: null,
    paused: false,
    pending: null,
    frame: 0,
    times,
    cleanup: [],
  };
  current = state;
  // The `.graph` action, for graphTimings: shown by the first frame.
  {
    const shown = timings.action(times.command ?? ".graph", times.enter ?? times.viz);
    if (times.engine !== undefined && times.enter !== undefined) shown.engine = times.engine - times.enter;
    shown.delta = times.view - (times.engine ?? times.viz);
    state.cleanup.push(() => timings.drop(shown));
  }
  renderLegend(legend, graph);
  state.drawnAs = graph.drawnAs;

  // What is known of node `i`, for the tooltip and the header: its key, its
  // label (the vertex table it is of), its degree, and its SIZE BY and COLOR
  // BY values, as [name, value] pairs.
  const describe = (i) => {
    const values = [];
    if (graph.size) values.push([graph.sizeBy, Number.isNaN(graph.size[i]) ? "none" : short(graph.size[i])]);
    if (graph.color) values.push([graph.colorBy, categoryName(graph, graph.color[i], i)]);
    return { id: state.ids.get(i), label: graph.labelOf(i), degree: state.degree[i], values };
  };
  // The selection, as the header says it: "Selected node 33: …", or the
  // nodes' keys when there are several.
  const selectionText = () => {
    const nodes = state.selection;
    if (state.edge) return `Selected edge ${state.ids.get(state.edge.src)} → ${state.ids.get(state.edge.dst)}`;
    if (nodes.length === 1) {
      const d = describe(nodes[0]);
      // SIZE BY degree is the degree already said.
      const values = d.values.filter(([name, value]) => !(name === "degree" && value === short(d.degree)));
      const parts = [d.label && `label ${d.label}`, `degree ${number(d.degree)}`, ...values.map(([name, value]) => `${name} ${value}`)];
      return `Selected node ${d.id}: ${parts.filter(Boolean).join(", ")}`;
    }
    const keys = nodes.slice(0, MAX_KEYS_NAMED).map((i) => state.ids.get(i));
    const more = nodes.length - keys.length;
    return `Selected ${number(nodes.length)} nodes: ${keys.join(", ")}${more ? ` and ${number(more)} more` : ""}`;
  };

  // The header: what is selected, then the graph's size and how far the
  // layout is.
  const header = () => {
    const counts = `${plural(state.nodes, "node")} · ${plural(state.edges, "edge")}`;
    // The rule that drew it (graph-data.js's drawnAs): the pane never guesses silently.
    const drawn = graph.drawnAs ? ` · drawn as ${graph.drawnAs}` : "";
    let text;
    if (state.selection.length || state.edge) {
      text = `${selectionText()} · ${counts}`;
    } else if (state.layoutDone) {
      text = `${counts}${drawn} · Done`;
    } else if (state.levels !== null) {
      text = `${counts}${drawn} · Laying out · level ${state.levels - state.level} of ${state.levels}`;
    } else {
      text = `Loading graph view… ${counts}`;
    }
    // Only when it changes: the line is a live region.
    if (status.textContent !== text) status.textContent = text;
    status.title = `${counts}${drawn} · drawn with ${state.backend}`;
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
      const start = performance.now();
      view.render();
      const end = performance.now();
      timings.frameDrawn(start, end);
      state.frames++;
      times.firstDraw ??= end;
    } catch (e) {
      fail(`the graph view stopped drawing: ${String(e?.message ?? e)}`);
    }
  };
  // `input` unless the layout or a test asked: graphTimings' frame intervals
  // are of frames the user's movement asked for.
  const redraw = (input = true) => {
    if (input) timings.frameAsked();
    if (!state.frame) state.frame = requestAnimationFrame(draw);
  };
  state.redraw = redraw;
  // An action with nothing to draw (a click that changes no selection) is
  // shown by the next frame the browser paints.
  const shownSoon = () => {
    if (!state.frame) requestAnimationFrame(() => current === state && timings.painted());
  };

  // The layout, in a worker of its own: a slow iteration never holds up a frame.
  const fromWorker = ({ data }) => {
    if (current !== state) return;
    if (data.times) for (const [k, v] of Object.entries(data.times)) times[k] ??= v - (performance.timeOrigin - data.origin);
    if (data.memory) state.layoutMemory = data.memory;
    if (data.type === "positions") {
      times.firstPositions ??= performance.now();
      state.pending = data.xy;
      state.iterations = data.iterations;
      state.level = data.level;
      state.levels = data.levels;
      state.positioned = true;
      header();
      redraw(false);
      // The nodes moved under a pointer at rest.
      if (pointerAt) hoverSoon();
    } else if (data.type === "done") {
      times.done ??= performance.now();
      state.layoutDone = true;
      header();
    } else if (data.type === "error") {
      fail(`the layout stopped: ${data.message}`);
    }
  };
  const workerFailed = (e) => {
    e.preventDefault();
    fail(`the layout stopped: ${e.message || "its worker failed"}`);
  };
  // What the worker said while the view was made, in order and a task
  // each, as it would have arrived; then the worker's messages directly.
  (async () => {
    while (early.length) {
      const [kind, e] = early.shift();
      (kind === "message" ? fromWorker : workerFailed)(e);
      await new Promise((resolve) => setTimeout(resolve));
    }
    worker.onmessage = fromWorker;
    worker.onerror = workerFailed;
  })();

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

  // Hover, for a mouse or a pen (a finger has no "over"): the node under the
  // pointer carries a ring and a tooltip. The pointer's place is kept as it
  // moves, and looked up once a frame at most, however many moves came.
  let pointerAt = null;
  let hoverFrame = 0;
  const showTip = () => {
    const node = state.hovered;
    if (node === null) {
      tip.hidden = true;
      return;
    }
    const d = describe(node);
    const row = (name, value, cls) => {
      const line = document.createElement("div");
      if (cls) line.className = cls;
      const key = document.createElement("span");
      key.className = "key";
      key.textContent = name;
      line.append(key, " ", String(value));
      return line;
    };
    tip.replaceChildren(row("node", d.id, "id"), ...(d.label ? [row("label", d.label)] : []), ...d.values.map(([name, value]) => row(name, value)), row("degree", number(d.degree)));
    tip.hidden = false;
    // Beside the node, on the side with room.
    const [x, y] = Array.from(view.nodePixel(node), (v) => v / dpr);
    const [w, h] = [canvas.clientWidth, canvas.clientHeight];
    const gap = 12;
    const left = x + gap + tip.offsetWidth <= w ? x + gap : x - gap - tip.offsetWidth;
    const top = y + gap + tip.offsetHeight <= h ? y + gap : y - gap - tip.offsetHeight;
    tip.style.left = `${Math.max(0, Math.min(left, w - tip.offsetWidth))}px`;
    tip.style.top = `${Math.max(0, Math.min(top, h - tip.offsetHeight))}px`;
  };
  const hover = () => {
    hoverFrame = 0;
    if (current !== state || !state.positioned) return;
    let node = null;
    // Not while a button or a finger is down: that is a drag.
    if (pointerAt && pointers.size === 0) {
      const start = performance.now();
      node = view.hover(pointerAt.x, pointerAt.y) ?? null;
      timings.picked(performance.now() - start);
      state.hoverPicks++;
    } else {
      view.setHovered(undefined);
    }
    if (node !== state.hovered) {
      state.hovered = node;
      canvas.classList.toggle("over-node", node !== null);
      redraw(false);
    }
    // Again even for the same node: it may have moved (a zoom, the layout).
    showTip();
  };
  const hoverSoon = () => {
    hoverFrame ||= requestAnimationFrame(hover);
  };
  state.cleanup.push(() => hoverFrame && cancelAnimationFrame(hoverFrame));
  const hoverAt = (e) => {
    pointerAt = e && e.pointerType !== "touch" ? at(e) : null;
    hoverSoon();
  };
  on(canvas, "pointerleave", () => hoverAt(null));

  // The selection is the view's: it is told what was clicked, and says what
  // is selected now.
  const selectionChanged = () => {
    state.selection = Array.from(view.selection());
    const edge = view.selectedEdge();
    state.edge = edge ? { edge: edge[0], src: edge[1], dst: edge[2] } : null;
    header();
    redraw();
    state.inspector.selectionChanged();
  };
  state.inspector = mountInspector(state, stage, { run, inspect, selectionChanged, redraw });
  state.clearSelection = () => {
    if (!view.clearSelection()) return false;
    selectionChanged();
    return true;
  };

  on(canvas, "pointerdown", (e) => {
    if (e.button !== 0) return;
    pointers.set(e.pointerId, at(e));
    // No hover while dragging.
    hoverSoon();
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
    if (!was) {
      if (pointers.size === 0) hoverAt(e);
      return;
    }
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
    if (pointers.size === 0) {
      drag = null;
      // The pointer is over what the drag left under it.
      hoverAt(cancelled ? null : e);
    }
    if (!tap) return;
    const { x, y } = at(e);
    if (e.pointerType !== "mouse") {
      // A mouse's double-click is the browser's (dblclick, below); a
      // finger's or a pen's second tap beside the first is found here.
      const near = lastTap && e.timeStamp - lastTap.t < DOUBLE_TAP_MS && Math.hypot(x - lastTap.x, y - lastTap.y) < DOUBLE_TAP_PX * dpr;
      lastTap = near ? null : { t: e.timeStamp, x, y };
      if (near) {
        timings.action("fit", e.timeStamp);
        view.fit();
        redraw();
        return;
      }
    }
    timings.action("click", e.timeStamp);
    if (!pick(x, y, e.shiftKey)) shownSoon();
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
      // What is under the pointer changed, though it did not move.
      if (pointerAt) hoverSoon();
    },
    { passive: false },
  );
  on(canvas, "dblclick", (e) => {
    timings.action("fit", e.timeStamp);
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
    else if (e.key === "i") {
      // Into the inspector, if it is open; Esc there comes back.
      if (state.inspector.focus()) e.preventDefault();
      return;
    } else return;
    e.preventDefault();
    timings.action(`key ${e.key}`, e.timeStamp);
    redraw();
  });

  // A click at (x, y): the node there is selected (added to the selection,
  // or taken out of it, with Shift), and a click on empty space clears the
  // selection. Returns whether the selection changed, and so the frame.
  function pick(x, y, extend) {
    const start = performance.now();
    const node = view.pick(x, y);
    // With no node there, the edge within 4 px (a plain click only: Shift
    // builds a selection of nodes).
    const edge = node === undefined && !extend ? view.pickEdge(x, y, EDGE_REACH_PX * dpr) : undefined;
    timings.picked(performance.now() - start);
    const changed = edge === undefined ? view.select(node, extend) : view.selectEdge(edge);
    if (changed) selectionChanged();
    return changed;
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

  return { nodes: state.nodes, edges: state.edges, backend: state.backend, drawnAs: graph.drawnAs };
}

/**
 * The inspector of the drawing `state` (graph-inspector.js), its panel in
 * `stage`: it shows what is selected, each time the selection changes. This
 * says what is selected as the inspector's plain data, and does what its
 * buttons ask: `run` a command in the terminal, `inspect` a row in the
 * engine, pin a node, select an edge's end. Returns {selectionChanged(),
 * focus(), state(), remove()}.
 */
function mountInspector(state, stage, { run, inspect, selectionChanged, redraw }) {
  const { view, graph, canvas } = state;
  // The pinned nodes: marked with a small ring (the highlight layer's).
  state.pinned = new Set();
  const key = (i) => String(state.ids.get(i));
  const panel = createInspector({
    run: (command) => run?.(command),
    fetchRow: (subject) => (subject.kind === "node" ? inspect({ node: subject.index, key: subject.key }) : inspect({ edge: subject.index, src: subject.src.key, dst: subject.dst.key })),
    fetchDegree: (subject) => inspect({ node: subject.index, key: subject.key, degree: true }).then((r) => r.degree ?? null),
    pin(index) {
      const on = !state.pinned.has(index);
      if (on) state.pinned.add(index);
      else state.pinned.delete(index);
      view.setPinned(Uint32Array.from(state.pinned));
      redraw();
      return on;
    },
    select(index) {
      if (view.select(index, false)) selectionChanged();
    },
    onLeave: () => canvas.focus(),
  });
  stage.append(panel.element);
  const name = graph.name ?? null;
  // A column of a node's row can size or colour the nodes where the drawing
  // is the graph itself: there SIZE BY and COLOR BY read the vertex tables.
  const styled = name !== null && graph.provenance?.rule === "graph";
  const node = (i) => ({ index: i, key: key(i), keyType: graph.keyTypeOf(i) });
  // The panel is over the right of the view, unless what is selected is drawn
  // there: then over the left, so a click never hides what it selected.
  const sideOf = (...nodes) => {
    const ratio = canvas.width / Math.max(1, canvas.clientWidth);
    const width = canvas.clientWidth;
    const edge = width - Math.min(280, width * 0.45) - 24;
    return nodes.some((i) => view.nodePixel(i)[0] / ratio > edge) ? "left" : "right";
  };
  return {
    selectionChanged() {
      const nodes = state.selection;
      if (state.edge) {
        const { edge, src, dst } = state.edge;
        const known = edge < graph.src.length ? graph.edgeOf(edge) : { label: null, row: null, values: [] };
        panel.show({ kind: "edge", side: sideOf(src, dst), index: edge, src: node(src), dst: node(dst), ...known });
      } else if (nodes.length === 1) {
        const [i] = nodes;
        panel.show({
          kind: "node",
          side: sideOf(i),
          ...node(i),
          label: graph.labelOf(i),
          graph: name,
          command: state.times.command ?? ".graph",
          degree: state.degree[i],
          values: i < graph.n ? graph.valuesOf(i) : [],
          pinned: state.pinned.has(i),
          styled,
        });
      } else if (nodes.length > 1) {
        const among = new Set(nodes);
        let edges = 0;
        for (let e = 0; e < graph.src.length; e++) if (among.has(graph.src[e]) && among.has(graph.dst[e])) edges++;
        panel.show({ kind: "nodes", side: sideOf(nodes.at(-1)), nodes: nodes.map(node), edges, graph: name });
      } else {
        panel.close();
      }
    },
    focus: () => panel.focus(),
    state: () => panel.state(),
    remove: () => panel.element.remove(),
  };
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

/**
 * Select nothing. Returns whether anything was selected: Esc clears a
 * selection before it closes the pane (app.js).
 */
export function clearSelection() {
  return current?.clearSelection?.() ?? false;
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
  state.tip.remove();
  state.inspector?.remove();
}

/** For tests: what is drawn. */
export function info() {
  if (!current) return null;
  const { nodes, edges, backend, iterations, level, levels, layoutDone, frames, hoverPicks } = current;
  const node = (index) => ({ index, id: current.ids.get(index), degree: current.degree[index] });
  const selection = current.selection.map(node);
  return {
    nodes,
    edges,
    backend,
    iterations,
    level,
    levels,
    layoutDone,
    // The selected nodes in the order selected, the last of them, the node
    // under the pointer, and how many times the pointer was looked up.
    selection,
    // The selected edge: its index and its ends.
    edge: current.edge && { index: current.edge.edge, src: node(current.edge.src), dst: node(current.edge.dst) },
    pinned: Array.from(current.pinned ?? [], (i) => current.ids.get(i)),
    picked: selection.at(-1) ?? null,
    hovered: current.hovered === null ? null : node(current.hovered),
    hoverPicks,
    frames,
    drawnAs: current.drawnAs,
    width: current.canvas.width,
    height: current.canvas.height,
  };
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
    state.redraw?.(false);
  }
}

/**
 * For graphTimings (#430): what drawing and using the view cost, times on
 * the page's clock given from `origin` (the last `.graph`'s Enter). See
 * graph-timings.js's read().
 */
export const readTimings = (origin) => timings.read(origin);
/** For tests: forget the frames and picks timed so far. */
export const resetTimings = () => timings.reset();

/**
 * For graphMemory (#430): `wasm`, the bytes of the view's WebAssembly memory
 * and of the layout worker's (as its last message gave it; null with nothing
 * drawn), and `userAgent`, performance.measureUserAgentSpecificMemory()'s
 * answer ({bytes, breakdown}) for the page and its workers. That needs
 * cross-origin isolation (#429), and the browser may wait for a garbage
 * collection before it answers: after `timeoutMs` (10 s), or without
 * isolation, `userAgent` is null and `userAgentError` says why.
 */
export async function memory({ timeoutMs = 10_000 } = {}) {
  const loaded = viz ? await viz.catch(() => null) : null;
  const out = {
    crossOriginIsolated: globalThis.crossOriginIsolated === true,
    wasm: { view: loaded?.wasm?.memory?.buffer.byteLength ?? null, layout: current?.layoutMemory ?? null },
    userAgent: null,
  };
  if (!out.crossOriginIsolated || !performance.measureUserAgentSpecificMemory) {
    out.userAgentError = out.crossOriginIsolated ? "not supported by this browser" : "the page is not cross-origin isolated";
    return out;
  }
  try {
    let timer;
    const late = new Promise((_, reject) => (timer = setTimeout(() => reject(new Error(`no answer in ${timeoutMs} ms`)), timeoutMs)));
    const m = await Promise.race([performance.measureUserAgentSpecificMemory(), late]).finally(() => clearTimeout(timer));
    out.userAgent = { bytes: m.bytes, breakdown: m.breakdown.filter((b) => b.bytes > 0).map((b) => ({ bytes: b.bytes, types: b.types, urls: b.attribution.map((a) => a.url) })) };
  } catch (e) {
    out.userAgentError = String(e?.message ?? e);
  }
  return out;
}

/**
 * For tests: where the node with the key `id` is drawn, {index, x, y} in CSS
 * pixels from the canvas's top left, or null with no such node.
 */
export function nodePlace(id) {
  const state = current;
  if (!state) return null;
  for (let index = 0; index < state.ids.length; index++) {
    if (String(state.ids.get(index)) !== String(id)) continue;
    const [x, y] = state.view.nodePixel(index);
    const ratio = state.canvas.width / state.canvas.clientWidth;
    return { index, x: x / ratio, y: y / ratio };
  }
  return null;
}

/**
 * For tests: the edge a click at (x, y) picks (CSS pixels from the canvas's
 * top left), {index, src, dst} with its ends' keys, or null: none within
 * reach, or a node is there.
 */
export function edgeAt(x, y) {
  const state = current;
  if (!state) return null;
  const ratio = state.canvas.width / state.canvas.clientWidth;
  if (state.view.pick(x * ratio, y * ratio) !== undefined) return null;
  const index = state.view.pickEdge(x * ratio, y * ratio, EDGE_REACH_PX * ratio);
  if (index === undefined) return null;
  return { index, src: state.ids.get(state.graph.src[index]), dst: state.ids.get(state.graph.dst[index]) };
}

/** For tests: what the inspector shows (graph-inspector.js's state()), or null. */
export function inspected() {
  return current?.inspector?.state() ?? null;
}

/** For tests: the node drawn at the canvas pixel (x, y), as a click picks it, or null. */
export function nodeAt(x, y) {
  const node = current?.view.pick(x, y);
  return node === undefined ? null : node;
}
