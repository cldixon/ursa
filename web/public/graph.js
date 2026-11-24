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
// double-click on empty space (or 0) fits the graph in view. A double-click
// on a node (a double tap, or Enter with nodes selected) expands it: the page
// runs `.expand ID` in the terminal, as if typed, and the node carries a busy
// ring until the command has run (Shift: `LIMIT 5000`). A click on a node selects it,
// Shift+click adds a node to the selection (or takes it out), and a click on
// empty space (or Esc) clears it: the selected nodes carry a ring, their
// edges are drawn wide, and the pane's header, a live region, says what is
// selected ("Selected node 33: …"). A click on an edge (within 4 px of it,
// with no node under the pointer) selects the edge. Whatever is selected, the
// inspector (graph-inspector.js) shows what is known of it, in a panel over
// the right of the view: `i` moves the focus into it, and Esc back. A mouse or a pen over a node rings it and
// shows a tooltip: its key, label, SIZE BY and COLOR BY values and degree.
// On a touch screen one finger pans, two pinch to zoom (and pan), a tap
// selects a node and a double tap expands it, or fits. `f` fits the selection,
// and a search (app.js runs .find, for the terminal and for the pane's `/`)
// selects what it found and fits it (selectFound). The selection itself is ursa-viz's
// (its interact module): this module tells it what was clicked. Colours are
// the page's --graph-* tokens (style.css), read again when the theme changes
// (retheme).
//
// Time to the first drawing: preload() starts fetching ursa-viz and a layout
// worker (which loads it too) while the engine is still running the query;
// show() hands the worker the graph before it builds the view, so the
// layout's hierarchy is built while the page sets up its canvas; and nothing
// is drawn until the layout's first positions, not a random cloud first.
// A large graph, once laid out, is drawn aggregated (#498, graph-cut.js): a
// cut through its layout's hierarchy, a disc for each group of nodes, made
// by the layout worker for the camera whenever it comes to rest. Zooming
// in opens the groups in view, down to the graph itself. `a` (and
// `.graph detail on|off`: detail(), below) forces full detail.
//
// A large graph's first drawing is the coarsest level of its layout's
// hierarchy, a few dozen discs, and then each level the layout comes to
// (#497, graph-levels.js): the graph's own nodes and edges are first drawn
// when the layout reaches them.
//
// The graph comes from the engine as one UGR buffer (ursa-viz/UGR.md), a
// graph result: ugr.js (ursa-viz/ui/ugr.js, copied in by the build) reads it
// as typed arrays over the buffer, and graph-data.js makes it the drawing.
// The view is made from the result itself (createViewFromResult), so that it
// can change in place.
//
// Exploring (#443): an explore command (.expand, .hide, .show, .undo, .redo)
// gives a UGR delta, and applyDelta() below applies it to the view. The
// layout worker then grows: new nodes start beside their neighbours and the
// nodes there were are held or slow. The positions before each step are kept
// here, and an undo (or a redo) puts every node back where it was, with no
// layout. What this module knows of the graph (ids, degrees, the legend) is
// read again from the view's result after each delta.

import { drawingOf } from "./graph-data.js";
import { createInspector } from "./graph-inspector.js";
import { createLevels } from "./graph-levels.js";
import { DISC_RADIUS, LABEL_RADIUS, MAX_LABELS, REST_MS, SEPARATION, aggregatedText, countLabel, createCuts } from "./graph-cut.js";
import { readResult } from "./ugr.js";

import * as timings from "./graph-timings.js";

let viz = null;
// Its WebAssembly memory, once loaded: what the view holds (memoryNow).
let vizMemory = null;
/** ursa-viz, loaded and initialised once: its module, its wasm exports and its URL. */
function loadViz() {
  viz ??= (async () => {
    const manifest = await (await fetch("/viz/viz.json")).json();
    const module = await import(manifest.module);
    const wasm = await module.default();
    vizMemory = wasm.memory;
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
/** Fitting the selection (`f`, .find) moves the camera over this long, in ms. */
const FIT_MS = 200;
/** It zooms in by no more than this at once, and leaves this share of the canvas around the selection. */
const MAX_FIT_ZOOM = 8;
const FIT_MARGIN = 0.8;

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

/** Whether any node or edge of `graph` (graph-data.js's drawing) is not shown. */
function anyRemoved({ removed, edgeRemoved }) {
  for (let i = 0; i < removed.length; i++) if (removed[i] & 1) return true;
  for (let e = 0; e < edgeRemoved.length; e++) if (edgeRemoved[e] & 1) return true;
  return false;
}

/** The edges of `graph` that are shown, as copies: {src, dst}. */
function shownEdges(graph) {
  const { src, dst, removed, edgeRemoved } = graph;
  if (!anyRemoved(graph)) return { src: src.slice(), dst: dst.slice() };
  const keep = [];
  for (let e = 0; e < src.length; e++) {
    if (!(edgeRemoved[e] & 1) && !(removed[src[e]] & 1) && !(removed[dst[e]] & 1)) keep.push(e);
  }
  return { src: Uint32Array.from(keep, (e) => src[e]), dst: Uint32Array.from(keep, (e) => dst[e]) };
}

/** Each node's degree among the edges of `graph` that are shown. */
function degreesOf(graph) {
  const { src, dst, removed, edgeRemoved } = graph;
  const degree = new Float32Array(graph.n);
  if (!anyRemoved(graph)) {
    for (let e = 0; e < src.length; e++) {
      degree[src[e]]++;
      degree[dst[e]]++;
    }
    return degree;
  }
  for (let e = 0; e < src.length; e++) {
    if (edgeRemoved[e] & 1 || removed[src[e]] & 1 || removed[dst[e]] & 1) continue;
    degree[src[e]]++;
    degree[dst[e]]++;
  }
  return degree;
}

/** A node's key as an explore command takes it: quoted if it has more than a word's characters. */
const commandId = (id) => (/^[\w.:@-]+$/.test(id) && id !== "--sql" ? id : `'${String(id).replace(/'/g, "''")}'`);

/** The most positions kept for undo and redo, in numbers (4 bytes each): older steps undo where the nodes are. */
const MAX_SAVED_POSITIONS = 16_000_000;
/**
 * From this many edges the layout's fine level takes about a minute, and
 * the status line offers saved positions instead (#502).
 */
export const SLOW_LAYOUT_EDGES = 2_000_000;

/**
 * For a drawing of `edges` edges that is being laid out, large enough that
 * it takes a minute or so: how to lay it out once and draw from that
 * (`graph` is the property graph drawn, if it is one; else "g" stands for
 * one over the edges). null for a smaller one.
 *
 *   status  for the pane's status line: the statement that saves the
 *           positions to a file, which the ursa CLI runs (on every core)
 *   copy    that statement
 *   here    the statements that do it in this session, where COPY has no
 *           disk to write to: the positions as a table, and the drawing
 *   draw    the drawing from the file, once it is added (.files add)
 */
export function positionsHint(edges, graph) {
  if (edges <= SLOW_LAYOUT_EDGES) return null;
  const g = graph ?? "g";
  const copy = `COPY (SELECT * FROM layout_fa2(${g})) TO 'pos.parquet'`;
  return {
    status: `the fine level takes about a minute; saved positions draw at once: ${copy}`,
    copy: `${copy};`,
    here: [`CREATE TABLE pos AS SELECT * FROM layout_fa2(${g});`, `.graph ${g} POSITIONS (SELECT * FROM pos)`],
    draw: `.graph ${g} POSITIONS (SELECT * FROM 'pos.parquet')`,
  };
}

/**
 * Draw the engine's graph (`sent`: {ugr, skipped, positions?}, a graph result as a UGR
 * buffer, the rows left out of it, and, with POSITIONS (query), where the
 * nodes are (x then y: they are drawn there at once, and no layout runs, #502); graph-data.js's drawingOf says what is
 * read from it: dense node indexes, each node's id, and what SIZE BY and
 * COLOR BY made, with the legend) in `stage`, replacing what was drawn. `status` is the pane's header line and `legend`
 * the legend under it; `onError(message)` is called if the view fails once
 * drawing (a lost GPU device or WebGL context, a layout that stopped).
 * `onCommand(command)` runs a command the view asks for (`.expand 33`, for a
 * double-click on node 33) as the terminal runs a typed one, and resolves
 * once it has run.
 *
 * `run(command)` types a command in the terminal and runs it, and
 * `inspect(args)` asks the engine for a node's or an edge's row (the engine
 * worker's `inspect`): the inspector's buttons and rows.
 * `times` gets the moments each step ended (performance.now()), for
 * graphTimings. Resolves once the view is made (it draws when the layout's
 * first positions come), to {nodes, edges, backend, drawnAs, saved, graph}:
 * `saved` whether it is drawn at saved positions, `graph` the property
 * graph drawn, if it is one.
 */
export async function show(stage, sent, { status, legend, onError, onCommand, times = {}, run, inspect }) {
  close();
  // What is drawn: read again from the view's result after each delta.
  let graph = drawingOf(readResult(sent.ugr), sent.skipped);
  const mine = ++seq;
  status.textContent = `Loading graph view… ${plural(graph.n, "node")} · ${plural(graph.src.length, "edge")}`;
  const { module, url } = await loadViz();
  times.viz ??= performance.now();
  if (mine !== seq) throw new Error("replaced by a newer drawing");

  // The layout first: its hierarchy is built while the view is made here.
  const worker = takeWorker(url);
  {
    // graph.layout, {kind: "rings" | "layers", level}, asks for a fixed layout
    // (graph-worker.js): each node's distance or layer in `level`.
    const start = { type: "start", n: graph.n, module: url, layout: graph.layout };
    if (sent.positions) {
      // Saved positions (#502): the worker's layout is at them, with
      // nothing to run, for an expansion to grow from. A copy: these are
      // the view's to draw.
      const { src, dst } = shownEdges(graph);
      const positions = sent.positions.slice();
      worker.postMessage({ ...start, layout: undefined, src, dst, positions }, [src.buffer, dst.buffer, positions.buffer]);
    } else if (graph.adjacency && !anyRemoved(graph)) {
      // The engine had the adjacency to send: the layout uses it as it is
      // (a copy of it: the result's buffer stays this page's).
      // The edges go too, in this page's numbering: the order to draw them
      // in (#495) is an order of these, which an adjacency does not have.
      const adjacency = { offsets: graph.adjacency.offsets.slice(), targets: graph.adjacency.targets.slice() };
      const { src, dst } = shownEdges(graph);
      worker.postMessage({ ...start, adjacency, src, dst }, [adjacency.offsets.buffer, adjacency.targets.buffer, src.buffer, dst.buffer]);
    } else {
      const { src, dst } = shownEdges(graph);
      worker.postMessage({ ...start, src, dst }, [src.buffer, dst.buffer]);
    }
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
  const describeCanvas = (nodes, edges) => {
    const styled = [graph.sizeBy && `sized by ${graph.sizeBy}`, graph.colorBy && `coloured by ${graph.colorBy}`].filter(Boolean).join(" and ");
    canvas.setAttribute(
      "aria-label",
      `Graph of ${plural(nodes, "node")} and ${plural(edges, "edge")}${styled ? `, ${styled} (see the legend)` : ""}. ` +
        "Drag or arrow keys to pan, wheel, pinch or plus and minus to zoom, double-click empty space or press 0 to fit. " +
        "Click a node to select it, Shift+click to add another, click empty space or press Escape to clear the selection. " +
        "Double-click or double tap a node, or press Enter with nodes selected, to add its neighbours. Slash finds nodes, F fits the selection. " +
        "A large graph is drawn as groups of nodes once it is laid out, which open as the view zooms in: click a group to select its nodes, and press A for every node and edge.",
    );
  };
  describeCanvas(graph.n, graph.src.length);
  // The hovered node's tooltip: for the pointer only (the header says what
  // is selected, for everyone), so assistive technology is not told of it.
  const tip = document.createElement("div");
  tip.className = "graph-tip";
  tip.setAttribute("aria-hidden", "true");
  tip.hidden = true;
  stage.replaceChildren(canvas, tip);
  const size = () => [Math.max(1, Math.round(canvas.clientWidth * dpr)), Math.max(1, Math.round(canvas.clientHeight * dpr))];
  [canvas.width, canvas.height] = size();

  const degree = degreesOf(graph);
  // A turn of the event loop: reading the result here, and the view reading
  // it again, are two tasks of the page, not one long one.
  await new Promise((resolve) => setTimeout(resolve));
  let view;
  try {
    // From the result, not its edge list alone: a view made so takes deltas.
    view = await module.createViewFromResult(canvas, sent.ugr);
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
  // The nodes' sizes and colours are this page's, not the view's own reading
  // of the result: SIZE BY's values, or else degree, so hubs read as hubs
  // (radius in CSS pixels), and COLOR BY's eight categories, then "other".
  // Set again after each delta, for every node.
  const style = (degree) => {
    if (graph.size) view.sizeBy(graph.size, 3 * dpr, 14 * dpr);
    else view.sizeBy(degree, 1.5 * dpr, 8 * dpr);
    if (graph.color) view.colorBy(graph.color);
  };
  style(degree);
  // A selection shows its nodes' edges too.
  view.selectionEdges = true;
  // Saved positions: the nodes are where POSITIONS put them, from the
  // first frame.
  const placed = sent.positions?.length === 2 * view.nodeCount;
  if (placed) view.setPositions(sent.positions);
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
    // The levels of the layout's hierarchy as they arrive (#497), and how
    // many nodes the one drawn in place of the graph has (null: the graph
    // is drawn).
    hierarchy: createLevels({ animate: !matchMedia("(prefers-reduced-motion: reduce)").matches }),
    groups: null,
    // The aggregated view (#498): the cuts through the layout's hierarchy
    // drawn in place of the graph once it is laid out; the timer of the
    // next one, asked for when the camera has rested; the camera of the
    // last frame; and the labels of the groups on the canvas.
    cuts: createCuts({ nodes: graph.n, edges: graph.src.length, detail: fullDetail, separation: SEPARATION * dpr }),
    cutRest: 0,
    camera: null,
    labels: [],
    // What each frame of a coarse level cost, in ms: the time to the next
    // animation frame, as the frame budget measures a moving frame.
    levelFrames: [],
    // Nothing is drawn before the layout's first positions. Saved positions
    // (#502) are there from the start, and there is no layout to wait for.
    positioned: placed,
    layoutDone: placed,
    saved: placed,
    // The first frame of saved positions is drawn as a moving one: within
    // the frame budget, at once, and then everything at rest.
    kick: placed,
    // The layout is placing an expansion's nodes, not laying the graph out.
    growing: false,
    // Bumped by each delta that moves the layout on: the worker's positions
    // of a graph before it are dropped.
    epoch: 0,
    // Deltas applied so far.
    deltas: 0,
    // For each node, how many of its edges in the property graph lead to
    // something not shown (the engine's count: setNotShown), or null.
    notShown: null,
    // The nodes with a command running or queued: index -> {count, ring}.
    busy: new Map(),
    // The selected nodes' indexes, in the order selected (the view's), and
    // the node under the pointer.
    selection: [],
    // How many nodes the search that made the selection matched (0: none did).
    matched: 0,
    // The camera is moving to fit the selection.
    fitting: false,
    // The selected edge, {edge, src, dst} (its index and its ends'), or null.
    edge: null,
    hovered: null,
    hoverPicks: 0,
    tip,
    frames: 0,
    // The frame at rest is being drawn in slices (#496), and the timer of the next.
    refining: false,
    refine: 0,
    // The layout worker's wasm memory, in bytes, as its last message gave it.
    layoutMemory: null,
    paused: false,
    pending: null,
    frame: 0,
    // The timer for the frame that draws every edge once the view rests
    // (#495), and whether the layout worker numbers the edges as the view
    // does (it is sent only the edges shown), so that its order is the view's.
    rest: 0,
    orderable: !anyRemoved(graph),
    // How long the worker took to order the edges, in ms, once it has.
    orderMs: null,
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
    return { id: state.ids.get(i), label: graph.labelOf(i), degree: state.degree[i], values, notShown: state.notShown?.[i] ?? 0 };
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
      // The +N a badge would say: what a double-click starts to add.
      const parts = [d.label && `label ${d.label}`, `degree ${number(d.degree)}`, ...values.map(([name, value]) => `${name} ${value}`), d.notShown > 0 && `+${number(d.notShown)} not shown`];
      return `Selected node ${d.id}: ${parts.filter(Boolean).join(", ")}`;
    }
    const keys = nodes.slice(0, MAX_KEYS_NAMED).map((i) => state.ids.get(i));
    const more = nodes.length - keys.length;
    // A search that matched more than it selected says how many (.find's cap).
    const of = state.matched > nodes.length ? ` of ${number(state.matched)} matching` : "";
    return `Selected ${number(nodes.length)}${of} nodes: ${keys.join(", ")}${more ? ` and ${number(more)} more` : ""}`;
  };

  // The header: what is selected, then the graph's size and how far the
  // layout is.
  const header = () => {
    // A cut is drawn in place of the graph (#498): always said, and first,
    // where a narrow pane does not cut it off.
    const shown = state.cuts.shown;
    const counts = `${shown ? aggregatedText(shown.groups, state.nodes) : plural(state.nodes, "node")} · ${plural(state.edges, "edge")}`;
    // The rule that drew it (graph-data.js's drawnAs): the pane never guesses silently.
    const drawn = graph.drawnAs ? ` · drawn as ${graph.drawnAs}` : "";
    let text;
    if (state.selection.length || state.edge) {
      text = `${selectionText()} · ${counts}`;
    } else if (state.layoutDone) {
      text = `${counts}${drawn}${state.saved ? " · saved positions" : ""} · Done`;
    } else if (state.growing) {
      text = `${counts}${drawn} · Placing the new nodes`;
    } else if (state.levels !== null) {
      // A coarse level drawn in place of the graph says how many nodes it has.
      const groups = state.groups === null ? "" : ` · ${plural(state.groups, "group")}`;
      // A layout that will take a minute says how to save one (#502).
      const hint = positionsHint(state.edges, graph.name);
      text = `${counts}${drawn} · Laying out · level ${state.levels - state.level} of ${state.levels}${groups}${hint ? ` · ${hint.status}` : ""}`;
    } else {
      text = `Loading graph view… ${counts}`;
    }
    // The frame at rest is being drawn in slices (#496): what is on the
    // canvas is not yet everything.
    if (state.refining && state.layoutDone) text += " · refining…";
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
  const draw = (frameTime) => {
    state.frame = 0;
    if (current !== state || state.paused || !state.positioned) return;
    // The level drawn, its nodes on their way out from their parents or in
    // place (#497); or the graph's own, on their way out from level 1.
    const split = state.hierarchy.frame(performance.now());
    if (split && split.level > 0) {
      if (state.groups !== null) view.setLevelPositions(split.xy);
    } else if (split && split.xy.length === 2 * view.nodeCount) {
      view.setPositions(split.xy);
      state.pending = null;
    } else if (state.pending) {
      view.setPositions(state.pending);
      state.pending = null;
    }
    try {
      const start = performance.now();
      // Within the frame budget (#495): while the view moves, as many edges
      // as fit in a frame; at rest, all of them.
      // A layout that is running is movement, between its positions too.
      if (!state.layoutDone || state.kick) view.keepMoving();
      state.kick = false;
      const moving = view.renderFrame(frameTime);
      const end = performance.now();
      timings.frameDrawn(start, end);
      state.frames++;
      times.firstDraw ??= end;
      if (moving) budgeted(frameTime);
      else if (view.refining) refineSoon();
      refiningIs(view.refining);
      if (split?.moving) redraw(false);
    } catch (e) {
      fail(`the graph view stopped drawing: ${String(e?.message ?? e)}`);
      return;
    }
    placeBusy();
    cameraLooked();
    placeLabels();
  };
  // Refinement at rest (#496). A frame at rest that does not fit in a frame
  // is not drawn at once: renderFrame leaves the canvas as it is, and the
  // view draws everything offscreen a slice a task, each sent when the GPU
  // has finished the last, so a pan or a zoom begun meanwhile waits for one
  // slice at most. Then a frame, which has everything, and so has every
  // frame at rest after it; the frames of the next pan or zoom draw it
  // under their own, moved as the camera is. If the view moved or changed
  // meanwhile, the slices stop and that frame is the one that shows it.
  const refine = () => {
    state.refine = 0;
    if (current !== state) return;
    // Not while a snapshot reads the view (inked): the view is borrowed.
    if (state.paused) return void (state.refine = setTimeout(refine, 50));
    try {
      if (view.refine(performance.now())) state.refine = setTimeout(refine);
      else redraw(false);
    } catch (e) {
      fail(`the graph view stopped drawing: ${String(e?.message ?? e)}`);
    }
  };
  const refineSoon = () => {
    state.refine ||= setTimeout(refine);
  };
  // The status line says so while slices remain.
  const refiningIs = (on) => {
    if (state.refining === on) return;
    state.refining = on;
    header();
  };
  state.cleanup.push(() => clearTimeout(state.refine));
  // After a moving frame: what it cost, which the budget follows, is the
  // time to the next animation frame (with no GPU the browser rasterises on
  // this thread first; with one, a late frame misses a refresh), from one
  // frame's time to the next's: whole refreshes, whenever in its frame each
  // callback ran. And if it left anything out, a frame with everything once
  // nothing has moved for a while.
  const budgeted = (frameTime) => {
    // A frame of a coarse level (#497): its cost is kept too, for graphTimings.
    const level = state.groups !== null;
    requestAnimationFrame((next) => {
      if (current !== state || state.paused) return;
      view.frameCost(next - frameTime);
      if (level) state.levelFrames.push(next - frameTime);
    });
    clearTimeout(state.rest);
    state.rest = view.framePartial ? setTimeout(rested, view.frameRestMs + 1) : 0;
  };
  // Nothing has moved for a while: everything, unless the layout is still
  // running (its next positions are on their way: look again later).
  const rested = () => {
    if (current !== state) return;
    if (state.layoutDone) redraw(false);
    else state.rest = setTimeout(rested, view.frameRestMs);
  };
  state.cleanup.push(() => clearTimeout(state.rest));
  // The order the layout worker gives the edges, most important first: the
  // view writes it to the GPU a piece a task, and draws the order it had
  // until the new one is whole. A frame with every edge is the same either way.
  const orderEdges = ({ order, ms }) => {
    if (!state.orderable || current !== state) return;
    // Not while a snapshot reads the view (inked): the view is borrowed.
    if (state.paused) return void setTimeout(() => orderEdges({ order, ms }), 50);
    state.orderMs = ms;
    try {
      view.setEdgeOrder(order);
    } catch {
      // Not an order of this view's edges: they stay as they are.
      return;
    }
    const more = () => {
      if (current !== state) return;
      if (state.paused) return void setTimeout(more, 50);
      // A run a task, each once the GPU has taken the last (#501).
      if (view.orderEdges(undefined, true)) setTimeout(more);
    };
    more();
  };
  // The aggregated view (#498, graph-cut.js). Once the layout is done, and
  // whenever the camera has then been still for REST_MS, the layout worker
  // is asked for a cut through its hierarchy at that camera; its answer is
  // drawn in place of the graph, or says that the graph itself is to be
  // (zoomed in to its own nodes). Meanwhile the last cut is drawn, moved
  // as the camera is.
  const askCut = () => {
    state.cutRest = 0;
    if (current !== state || !state.layoutDone || !state.cuts.on) return;
    // Not while a snapshot reads the view (inked): the view is borrowed.
    if (state.paused) return void cutSoon(50);
    const message = state.cuts.request(view.camera());
    if (message) worker.postMessage(message);
  };
  const cutSoon = (ms = REST_MS) => {
    clearTimeout(state.cutRest);
    state.cutRest = state.layoutDone && state.cuts.on ? setTimeout(askCut, ms) : 0;
  };
  state.cleanup.push(() => clearTimeout(state.cutRest));
  // After a frame: if the camera is not where the last frame had it, a
  // cut once it rests.
  const cameraLooked = () => {
    if (!state.cuts.on || !state.layoutDone) return;
    const camera = view.camera();
    const was = state.camera;
    state.camera = camera;
    if (was && camera.every((v, i) => v === was[i])) return;
    cutSoon();
  };
  // The graph itself again, in place of a cut.
  const leaveCut = () => {
    view.clearLevel();
    placeLabels();
    header();
    redraw(false);
  };
  const applyCut = (data) => {
    if (current !== state) return;
    if (state.paused) return void setTimeout(() => applyCut(data), 50);
    const was = state.cuts.shown !== null;
    const what = state.cuts.answer(data);
    if (what === "stale") return;
    if (what !== "cut") return void (was && leaveCut());
    try {
      view.setCut(data.xy, data.mass, data.members, data.of, data.src, data.dst, data.weight, data.totalEdges, DISC_RADIUS * dpr);
    } catch {
      // Not a cut of what is drawn (the graph changed meanwhile): the graph itself.
      if (state.cuts.drop() || was) leaveCut();
      return;
    }
    header();
    redraw(false);
  };
  // Full detail forced, or let go (`a`, `.graph detail`).
  state.setDetail = (detail) => {
    if (state.cuts.setDetail(detail)) leaveCut();
    else header();
    if (!detail) cutSoon(0);
  };
  // The groups' labels: how many nodes each disc that is large enough
  // stands for, over the canvas.
  const placeLabels = () => {
    const found = state.cuts.shown ? view.groupLabels(LABEL_RADIUS * dpr, MAX_LABELS) : [];
    const count = found.length / 5;
    while (state.labels.length < count) {
      const label = document.createElement("div");
      label.className = "graph-group";
      label.setAttribute("aria-hidden", "true");
      stage.append(label);
      state.labels.push(label);
    }
    // Heaviest first: a label that would lie over one already placed is
    // left out.
    const placed = [];
    state.labels.forEach((label, i) => {
      label.hidden = true;
      if (i >= count) return;
      const [, x, y, , members] = found.slice(5 * i, 5 * i + 5);
      const text = countLabel(members);
      const [cx, cy, w, h] = [x / dpr, y / dpr, 7 * text.length + 12, 18];
      if (placed.some((b) => Math.abs(b[0] - cx) < (b[2] + w) / 2 && Math.abs(b[1] - cy) < (b[3] + h) / 2)) return;
      placed.push([cx, cy, w, h]);
      if (label.textContent !== text) label.textContent = text;
      label.style.left = `${cx}px`;
      label.style.top = `${cy}px`;
      label.hidden = false;
    });
  };
  state.cleanup.push(() => {
    for (const label of state.labels) label.remove();
    state.labels = [];
  });
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
    // The edges' drawing order (#495) is of the edges the layout started
    // with, whatever was added since.
    if (data.type === "order") return orderEdges(data);
    // Of a graph before the last delta: not what is drawn now.
    if (data.type !== "error" && (data.epoch ?? 0) !== state.epoch) return;
    if (data.type === "cut") return applyCut(data);
    if (data.type === "level" || data.type === "levelPositions") {
      // The layout at a level of its hierarchy (#497): drawn in place of
      // the graph while it is a coarse one. Level 0 is the graph itself:
      // its positions follow, and its nodes come out from level 1's.
      times.firstPositions ??= performance.now();
      if (data.type === "levelPositions") state.hierarchy.place(data.level, data.xy);
      else {
        state.hierarchy.add(data, performance.now());
        if (data.level > 0) {
          view.setLevel(data.level, data.levels, data.mass, data.src, data.dst, data.weight, data.totalEdges);
          state.groups = data.n;
        }
      }
      if (data.level === 0) return;
      state.iterations = data.iterations;
      state.level = data.level;
      state.levels = data.levels;
      state.positioned = true;
      header();
      redraw(false);
    } else if (data.type === "positions") {
      if (data.xy.length !== 2 * view.nodeCount) return;
      times.firstPositions ??= performance.now();
      // The graph itself, from here on.
      if (state.groups !== null) {
        view.clearLevel();
        state.groups = null;
        times.graphDrawn ??= performance.now();
      }
      state.hierarchy.place(0, data.xy);
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
      state.growing = false;
      header();
      // Laid out: aggregated from here, if it is large (#498).
      cutSoon(0);
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
    // Once the rest of this function has run: the handlers read what it
    // declares further down (the pointer's place).
    await null;
    while (early.length) {
      const [kind, e] = early.shift();
      (kind === "message" ? fromWorker : workerFailed)(e);
      await new Promise((resolve) => setTimeout(resolve));
    }
    worker.onmessage = fromWorker;
    worker.onerror = workerFailed;
  })();
  if (placed) {
    // The first frame, of the whole graph where it was saved: it waits for
    // no worker.
    times.firstPositions ??= performance.now();
    times.graphDrawn ??= times.firstPositions;
    redraw(false);
  }

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
    tip.replaceChildren(
      row("node", d.id, "id"),
      ...(d.label ? [row("label", d.label)] : []),
      ...d.values.map(([name, value]) => row(name, value)),
      row("degree", number(d.degree)),
      // The +N a badge would say: its edges in the graph that lead to something not shown.
      ...(d.notShown > 0 ? [row("not shown", `+${number(d.notShown)}`, "more")] : []),
    );
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
  // The inspector follows. After a tap (`byTouch`) it waits until a second
  // tap would no longer be a double tap: its panel, under the view on a
  // phone, would otherwise open over the node between the two taps and
  // take the second, so a double tap never expanded (#443).
  let inspectLater = 0;
  const inspectNow = () => {
    clearTimeout(inspectLater);
    inspectLater = 0;
    if (current === state) state.inspector.selectionChanged();
  };
  state.inspectNow = () => inspectLater && inspectNow();
  state.cleanup.push(() => clearTimeout(inspectLater));
  // `matched`: how many nodes the search that selected them matched.
  const selectionChanged = (byTouch = false, matched = 0) => {
    state.matched = matched;
    state.selection = Array.from(view.selection());
    const edge = view.selectedEdge();
    state.edge = edge ? { edge: edge[0], src: edge[1], dst: edge[2] } : null;
    header();
    redraw();
    if (!byTouch) return inspectNow();
    clearTimeout(inspectLater);
    inspectLater = setTimeout(inspectNow, DOUBLE_TAP_MS + 50);
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
        expandOrFit(x, y, false, e.timeStamp);
        return;
      }
    }
    timings.action("click", e.timeStamp);
    if (!pick(x, y, e.shiftKey, e.pointerType !== "mouse")) shownSoon();
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
    const { x, y } = at(e);
    expandOrFit(x, y, e.shiftKey, e.timeStamp);
  });
  on(canvas, "keydown", (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    // Enter expands the selection, a node at a time, in the order selected.
    if (e.key === "Enter") {
      if (!state.selection.length) return;
      e.preventDefault();
      for (const node of state.selection) expand(node, e.shiftKey, e.timeStamp);
      return;
    }
    const step = 40 * dpr;
    const pan = { ArrowLeft: [step, 0], ArrowRight: [-step, 0], ArrowUp: [0, step], ArrowDown: [0, -step] }[e.key];
    const middle = [canvas.width / 2, canvas.height / 2];
    if (pan) view.pan(...pan);
    else if (e.key === "+" || e.key === "=") view.zoom(1.25, ...middle);
    else if (e.key === "-" || e.key === "_") view.zoom(0.8, ...middle);
    else if (e.key === "0") view.fit();
    else if (e.key === "a") {
      // Full detail, or aggregated again (#498): `.graph detail on|off`.
      e.preventDefault();
      detail(state.cuts.wanted);
      return;
    } else if (e.key === "i") {
      // Into the inspector, if it is open; Esc there comes back.
      state.inspectNow();
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
  function pick(x, y, extend, byTouch = false) {
    const start = performance.now();
    // A cut is drawn (#498): the group there, for its nodes.
    if (state.cuts.shown) {
      const group = view.pickGroup(x, y);
      timings.picked(performance.now() - start);
      const changed = view.selectGroup(group, extend);
      if (changed) selectionChanged(byTouch);
      return changed;
    }
    const node = view.pick(x, y);
    // With no node there, the edge within 4 px (a plain click only: Shift
    // builds a selection of nodes).
    const edge = node === undefined && !extend ? view.pickEdge(x, y, EDGE_REACH_PX * dpr) : undefined;
    timings.picked(performance.now() - start);
    const changed = edge === undefined ? view.select(node, extend) : view.selectEdge(edge);
    if (changed) selectionChanged(byTouch);
    return changed;
  }

  // Fit the selection (`f`, and a search's matches): the camera moves over
  // FIT_MS so that the selected nodes fill FIT_MARGIN of the canvas, or at
  // once under prefers-reduced-motion. One node (or nodes on one point) is
  // centred at the zoom the view has. Returns whether there was a selection
  // to fit.
  let fitFrame = 0;
  const cancelFit = () => {
    if (fitFrame) cancelAnimationFrame(fitFrame);
    fitFrame = 0;
    state.fitting = false;
  };
  state.cleanup.push(cancelFit);
  const fitSelection = (timeStamp = performance.now()) => {
    cancelFit();
    const nodes = state.selection;
    if (!nodes.length || !state.positioned) return false;
    if (state.pending) {
      view.setPositions(state.pending);
      state.pending = null;
    }
    // The selection's box, in canvas pixels, and the nodes at its sides: a
    // pan or a zoom keeps them at its sides, so they say where the box is
    // as the camera moves (and as the layout moves the nodes).
    let [x0, y0, x1, y1] = [Infinity, Infinity, -Infinity, -Infinity];
    const sides = [nodes[0], nodes[0], nodes[0], nodes[0]];
    for (const i of nodes) {
      const [x, y] = view.nodePixel(i);
      if (x < x0) [x0, sides[0]] = [x, i];
      if (x > x1) [x1, sides[1]] = [x, i];
      if (y < y0) [y0, sides[2]] = [y, i];
      if (y > y1) [y1, sides[3]] = [y, i];
    }
    const centre = () => {
      const [l, r, t, b] = sides.map((i) => view.nodePixel(i));
      return [(l[0] + r[0]) / 2, (t[1] + b[1]) / 2];
    };
    const [w, h] = [canvas.width, canvas.height];
    const [bw, bh] = [x1 - x0, y1 - y0];
    const factor = bw < 1 && bh < 1 ? 1 : Math.min(MAX_FIT_ZOOM, FIT_MARGIN * Math.min(w / Math.max(bw, 1), h / Math.max(bh, 1)));
    const from = centre();
    let zoomed = 1;
    // `p` of the way, 0 to 1: zoomed by factor^p about the canvas's middle,
    // the box's centre that far towards it.
    const moveTo = (p) => {
      const z = factor ** p;
      view.zoom(z / zoomed, w / 2, h / 2);
      zoomed = z;
      const [cx, cy] = centre();
      view.pan(from[0] + (w / 2 - from[0]) * p - cx, from[1] + (h / 2 - from[1]) * p - cy);
      redraw(false);
      if (pointerAt) hoverSoon();
    };
    timings.action("fit selection", timeStamp);
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) {
      moveTo(1);
      return true;
    }
    const start = performance.now();
    state.fitting = true;
    const tick = (now) => {
      fitFrame = 0;
      if (current !== state) return;
      const t = Math.min(1, Math.max(0, (now - start) / FIT_MS));
      // Ease out: quick away, slow to rest.
      moveTo(1 - (1 - t) ** 3);
      if (t < 1) fitFrame = requestAnimationFrame(tick);
      else state.fitting = false;
    };
    fitFrame = requestAnimationFrame(tick);
    return true;
  };
  // A hand on the view takes the camera back.
  on(canvas, "pointerdown", cancelFit);
  on(canvas, "wheel", cancelFit, { passive: true });
  on(canvas, "keydown", (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    if (e.key !== "f" && e.key !== "F") {
      cancelFit();
      return;
    }
    e.preventDefault();
    fitSelection(e.timeStamp);
  });
  state.fitSelection = fitSelection;
  // A search's answer (.find): select `nodes`, the first of `matched`
  // matches, and fit them.
  state.selectFound = (nodes, matched) => {
    // The engine's graph can be ahead of the view's (a change not drawn
    // yet): a node the view has not got is left out.
    const count = view.nodeCount;
    view.setSelection(nodes.every((i) => i < count) ? nodes : nodes.filter((i) => i < count));
    selectionChanged(false, matched);
    fitSelection();
  };
  // Expanding (#443). A double-click, a double tap or Enter asks the page
  // to run `.expand ID` as the terminal runs a typed line: it is echoed,
  // kept in the history and replayed by a shared link, and its answer comes
  // back here as a delta (state.apply). Commands run one at a time, so a
  // second double-click waits for the first; until its command has run, a
  // node carries a busy ring.
  const actions = [];
  function expand(node, all, when) {
    if (!onCommand) return;
    const command = `.expand ${commandId(String(state.ids.get(node)))}${all ? " LIMIT 5000" : ""}`;
    // For graphTimings: the action begins now, and the frame that draws its
    // delta shows it (state.apply records it then: a frame drawn meanwhile
    // shows nothing of it).
    const waiting = { command, at: when };
    actions.push(waiting);
    setBusy(node, +1);
    Promise.resolve(onCommand(command))
      .catch(() => {})
      .finally(() => {
        if (current !== state) return;
        setBusy(node, -1);
        // It changed nothing (or failed): the next frame painted shows that.
        const i = actions.indexOf(waiting);
        if (i >= 0) {
          actions.splice(i, 1);
          timings.action(command, when);
          shownSoon();
        }
      });
  }
  // A double-click or a double tap at (x, y): on a node it expands the node
  // (with `all`, up to the engine's cap), and on empty space it fits.
  function expandOrFit(x, y, all, when) {
    const start = performance.now();
    const node = state.positioned ? view.pick(x, y) : undefined;
    timings.picked(performance.now() - start);
    if (node !== undefined) {
      expand(node, all, when);
      return;
    }
    timings.action("fit", when);
    view.fit();
    redraw();
  }

  // The busy rings: an element over each node with a command running or
  // queued, moved with the node as each frame is drawn.
  function setBusy(node, by) {
    const entry = state.busy.get(node) ?? { count: 0, ring: null };
    entry.count += by;
    if (entry.count <= 0) {
      entry.ring?.remove();
      state.busy.delete(node);
      return;
    }
    if (!entry.ring) {
      entry.ring = document.createElement("div");
      entry.ring.className = "graph-busy";
      entry.ring.setAttribute("aria-hidden", "true");
      stage.append(entry.ring);
    }
    state.busy.set(node, entry);
    placeBusy();
  }
  function placeBusy() {
    for (const [node, { ring }] of state.busy) {
      const there = state.positioned && node < view.nodeCount && !(graph.removed[node] & 1);
      ring.hidden = !there;
      if (!there) continue;
      const [x, y] = view.nodePixel(node);
      ring.style.left = `${x / dpr}px`;
      ring.style.top = `${y / dpr}px`;
    }
  }
  state.cleanup.push(() => {
    for (const { ring } of state.busy.values()) ring?.remove();
    state.busy.clear();
  });

  // The positions before each step applied, newest last, and those before
  // each step undone: what an undo and a redo put back.
  const saved = { undo: [], redo: [] };
  const trimSaved = () => {
    let held = 0;
    for (const list of [saved.undo, saved.redo]) for (const xy of list) held += xy.length;
    while (held > MAX_SAVED_POSITIONS && saved.undo.length + saved.redo.length > 1) {
      const dropped = (saved.undo.length ? saved.undo : saved.redo).shift();
      held -= dropped.length;
    }
  };

  // Apply an explore command's delta (UGR bytes): `kind` is "undo", "redo"
  // or "step" (anything else), `command` the line that made it, `at` when it
  // began and `engine` its ms in the engine, for graphTimings. Throws, with
  // nothing changed, if the delta does not fit what is drawn.
  state.apply = (bytes, { kind = "step", command = kind, at: began = performance.now(), engine = null } = {}) => {
    const start = performance.now();
    // A step while a coarse level is drawn (#497): the graph is drawn from
    // here on, and the layout, grown, has a place for every node.
    if (state.groups !== null) {
      view.clearLevel();
      state.groups = null;
    }
    state.hierarchy.forget();
    // Nor is a cut drawn from here on (#498): the layout's hierarchy is of
    // the graph before the step.
    clearTimeout(state.cutRest);
    if (state.cuts.drop()) leaveCut();
    // The layout's newest positions are the ones the step starts from.
    if (state.pending) {
      view.setPositions(state.pending);
      state.pending = null;
    }
    const before = view.positions();
    const n0 = view.nodeCount;
    // An undo, or a redo, puts every node back where it was.
    let restored = kind === "undo" ? saved.undo.pop() : kind === "redo" ? saved.redo.pop() : undefined;
    let stats;
    try {
      stats = view.applyDelta(bytes, restored);
    } catch (e) {
      if (!restored) throw e;
      // The saved positions are not of this step: nodes stay where they are.
      restored = undefined;
      stats = view.applyDelta(bytes);
    }
    const uploaded = stats.bytesUploaded;
    stats.free();
    if (kind === "undo") saved.redo.push(before);
    else saved.undo.push(before);
    if (kind !== "undo" && kind !== "redo") saved.redo.length = 0;
    trimSaved();

    // What is drawn now, as this module reads it: from the view's result.
    graph = drawingOf(readResult(view.resultBytes()), graph.skipped);
    state.graph = graph;
    state.ids = graph.ids;
    state.degree = degreesOf(graph);
    style(state.degree);
    const shown = view.info();
    state.nodes = shown.nodes;
    state.edges = shown.edges;
    state.deltas++;
    // The engine counts again, and the page hands it over (setNotShown).
    state.notShown = null;
    state.drawnAs = graph.drawnAs;
    renderLegend(legend, graph);
    describeCanvas(state.nodes, state.edges);

    // The layout. Positions restored, or nodes taken away: it is where the
    // view is, with nothing to run. Nodes added: it grows, the new nodes
    // from beside their neighbours. Else (hidden, shown) it goes on.
    const n1 = view.nodeCount;
    if (restored || n1 < n0) {
      const { src, dst } = shownEdges(graph);
      const xy = view.positions();
      state.epoch++;
      state.layoutDone = true;
      state.growing = false;
      worker.postMessage({ type: "place", epoch: state.epoch, n: n1, src, dst, xy }, [src.buffer, dst.buffer, xy.buffer]);
    } else if (n1 > n0) {
      const { src, dst } = shownEdges(graph);
      state.epoch++;
      state.layoutDone = false;
      state.growing = true;
      worker.postMessage({ type: "grow", epoch: state.epoch, n: n1, src, dst }, [src.buffer, dst.buffer]);
    }

    // The view dropped what is gone from the selection and the hover.
    state.selection = Array.from(view.selection());
    state.hovered = view.hovered ?? null;
    canvas.classList.toggle("over-node", state.hovered !== null);
    if (pointerAt) hoverSoon();
    else showTip();
    header();

    // Its action is shown by the frame asked for here.
    const i = actions.findIndex((a) => a.command === command);
    const record = timings.action(command, i >= 0 ? actions.splice(i, 1)[0].at : began);
    record.engine = engine;
    record.delta = performance.now() - start;
    // (Nothing is drawn before the layout's first positions: then, by those.)
    redraw(false);
    return { nodes: state.nodes, edges: state.edges, slots: n1, bytesUploaded: uploaded, restored: !!restored };
  };
  state.setNotShown = (counts) => {
    if (counts.length !== view.nodeCount) return false;
    state.notShown = counts;
    header();
    showTip();
    return true;
  };

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

  return { nodes: state.nodes, edges: state.edges, backend: state.backend, drawnAs: graph.drawnAs, saved: placed, graph: graph.name };
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
    fetchNeighbours: (subject) => inspect({ node: subject.index, key: subject.key, neighbours: true }).then((r) => r.neighbours ?? null),
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

/**
 * Select what a search found (.find, the pane's `/`): `nodes`, the view's
 * indexes of the first of `matched` matches, and fit them in view. Returns
 * whether there is a view to select in.
 */
export function selectFound(nodes, matched = nodes.length) {
  if (!current?.selectFound) return false;
  current.selectFound(nodes, matched);
  return true;
}

// Whether full detail is forced (`a`, `.graph detail on`): kept from one
// drawing to the next.
let fullDetail = false;

/**
 * Force full detail (`true`), or let a large graph be drawn aggregated
 * again (`false`); with no argument, change nothing (#498). Returns
 * {detail, large, aggregated}: whether full detail is forced now, whether
 * what is drawn is large enough to be aggregated (null with nothing
 * drawn), and the cut drawn, {groups, nodes}, or null.
 */
export function detail(on) {
  if (on !== undefined) {
    fullDetail = Boolean(on);
    current?.setDetail?.(fullDetail);
  }
  const shown = current?.cuts.shown ?? null;
  return { detail: fullDetail, large: current ? current.cuts.large : null, aggregated: shown && { groups: shown.groups, nodes: shown.nodes } };
}

/** Fit the selection in view, as `f` does. Returns whether there was one. */
export function fitSelection() {
  return current?.fitSelection?.() ?? false;
}

/**
 * Apply an explore command's delta (UGR bytes, as the engine gave them) to
 * what is drawn: `kind` is "undo", "redo" or "step", `command` the line that
 * made it, `at` when it began (performance.now()) and `engine` its ms in the
 * engine. Returns {nodes, edges, slots, bytesUploaded, restored} (what is
 * shown now, and whether saved positions were put back), or null with
 * nothing drawn. Throws if the delta does not fit what is drawn.
 */
export function applyDelta(bytes, options) {
  return current?.apply?.(bytes, options) ?? null;
}

/**
 * For each node, how many of its edges in the property graph lead to
 * something not shown (a Uint32Array, the engine's count after the last
 * delta): the tooltip and the header say it as +N. False if it is not of
 * what is drawn now.
 */
export function setNotShown(counts) {
  return current?.setNotShown?.(counts) ?? false;
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

/** How many `values` there are, and their median, 95th percentile and greatest (null for none). */
function spread(values) {
  const sorted = Float64Array.from(values).sort();
  const at = (q) => (sorted.length ? Math.round(sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))] * 100) / 100 : null);
  return { count: sorted.length, p50: at(0.5), p95: at(0.95), max: at(1) };
}

/** For tests: what is drawn. */
export function info() {
  if (!current) return null;
  const { nodes, edges, backend, iterations, level, levels, layoutDone, frames, hoverPicks } = current;
  const node = (index) => ({ index, id: current.ids.get(index), degree: current.degree[index], notShown: current.notShown?.[index] ?? null });
  const selection = current.selection.map(node);
  return {
    nodes,
    edges,
    backend,
    iterations,
    level,
    levels,
    layoutDone,
    // Whether the nodes are where POSITIONS (query) put them (#502): no
    // layout ran.
    saved: current.saved,
    // The selected nodes in the order selected, the last of them, the node
    // under the pointer, and how many times the pointer was looked up.
    selection,
    // The selected edge: its index and its ends.
    edge: current.edge && { index: current.edge.edge, src: node(current.edge.src), dst: node(current.edge.dst) },
    pinned: Array.from(current.pinned ?? [], (i) => current.ids.get(i)),
    picked: selection.at(-1) ?? null,
    hovered: current.hovered === null ? null : node(current.hovered),
    hoverPicks,
    // How many nodes the search that made the selection matched (0: no
    // search made it), and whether the camera is moving to fit it.
    matched: current.matched,
    fitting: current.fitting,
    // Exploring: the node slots (shown or hidden), the deltas applied, the
    // nodes with a command running or queued, and whether the engine's
    // counts of what is not shown are in.
    slots: current.view.nodeCount,
    deltas: current.deltas,
    // Whether the layout's first positions are in, and the layout's epoch.
    positioned: current.positioned,
    epoch: current.epoch,
    busy: [...current.busy.keys()].map(node),
    notShownKnown: current.notShown !== null,
    frames,
    // The frame budget (#495) and the refinement at rest (#496): {k, nodes,
    // edges, ordered, p95, frames, limit, refining, refined, refinement}
    // (GraphView.frameInfo), whether the canvas lacks anything after the
    // last frame, and the ms the layout worker took to order the edges
    // (null until it has).
    frame: { ...current.view.frameInfo(), partial: current.view.framePartial, orderMs: current.orderMs },
    // The level of the layout's hierarchy drawn in place of the graph
    // (#497): {level, levels, nodes, nodesDrawn, edges, edgesDrawn,
    // totalEdges} (GraphView.levelInfo), or null when the graph is drawn;
    // and the levels that have arrived, coarsest first: [level, nodes].
    coarse: current.view.levelInfo() ?? null,
    // What the frames of the coarse levels cost: how many, and the median,
    // the 95th percentile and the most, in ms (null before any).
    levelFrames: spread(current.levelFrames),
    hierarchy: current.hierarchy.levels.filter(Boolean).map((l) => [l.level, l.n]).reverse(),
    // The aggregated view (#498): the cut drawn in place of the graph,
    // {groups, merged, nodes, groupsDrawn, edges, edgesDrawn, totalEdges}
    // (GraphView.cutInfo), or null when the graph itself is; whether cuts
    // are asked for (a large graph, laid out coarse to fine, full detail
    // not forced), whether an answer is awaited, what the cuts took in
    // the layout worker, in ms, and the labels on the canvas.
    cut: current.view.cutInfo() ?? null,
    cuts: { on: current.cuts.on, large: current.cuts.large, detail: !current.cuts.wanted, waiting: current.cuts.waiting || current.cutRest !== 0, ms: spread(current.cuts.took), last: current.cuts.took.at(-1) ?? null, prepared: current.cuts.prepared },
    groupLabels: current.labels.filter((l) => !l.hidden).map((l) => l.textContent),
    camera: Array.from(current.view.camera()),
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
export const readTimings = (origin) => ({ ...timings.read(origin), memory: memoryNow() });

/**
 * For graphTimings (#501): what the two ursa-viz instances hold now, each
 * as {bytes, perEdge}: the bytes of its WebAssembly memory (the view's, on
 * this page; the layout worker's, as its last message gave it) and those
 * bytes per edge slot of the graph drawn (`edges`). Null with nothing
 * drawn. A WebAssembly memory only grows, so each is its instance's peak
 * since it loaded: a spare worker starts fresh with each drawing, and the
 * view's module is this page's for as long as it is open.
 */
function memoryNow() {
  const state = current;
  if (!state) return null;
  const edges = state.view.edgeCount;
  const part = (bytes) => (bytes == null ? null : { bytes, perEdge: edges ? Math.round(bytes / edges) : null });
  return { edges, view: part(vizMemory?.buffer.byteLength), layout: part(state.layoutMemory) };
}
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
 * For tests: the view at the layout's newest positions, which the next frame
 * would draw: a test that reads a node's place as the layout ends clicks
 * where the node will be.
 */
function atNewest(state) {
  // Nodes on their way out from their parents (#497): in place.
  const split = state.hierarchy.settle();
  if (split?.level === 0 && split.xy.length === 2 * state.view.nodeCount) state.pending = split.xy;
  if (!state.pending) return;
  state.view.setPositions(state.pending);
  state.pending = null;
  state.redraw?.(false);
}

/**
 * For tests: where every shown node is, by its key: {ids, xy} with `xy` the
 * view's positions (x then y, one per node slot), read back from the view
 * once the layout's newest are in it.
 */
export function placement() {
  const state = current;
  if (!state) return null;
  atNewest(state);
  const ids = Array.from({ length: state.ids.length }, (_, i) => String(state.ids.get(i)));
  return { ids, xy: state.view.positions() };
}

/**
 * For tests: where the node with the key `id` is drawn, {index, x, y} in CSS
 * pixels from the canvas's top left, or null with no such node.
 */
export function nodePlace(id) {
  const state = current;
  if (!state) return null;
  atNewest(state);
  for (let index = 0; index < state.ids.length; index++) {
    if (String(state.ids.get(index)) !== String(id)) continue;
    const [x, y] = state.view.nodePixel(index);
    const ratio = state.canvas.width / state.canvas.clientWidth;
    return { index, x: x / ratio, y: y / ratio, hidden: (state.graph.removed[index] & 1) === 1 };
  }
  return null;
}

/**
 * For tests: up to `limit` shown nodes with the most edges not shown (the
 * engine's count), each {index, id, notShown, x, y} with its place in whole
 * CSS pixels of the canvas; only those on the canvas that a click there picks.
 */
export function expandable(limit = 10) {
  const state = current;
  if (!state?.notShown) return [];
  atNewest(state);
  const ratio = state.canvas.width / state.canvas.clientWidth;
  const box = state.canvas.getBoundingClientRect();
  const order = Array.from(state.notShown.keys())
    .filter((i) => state.notShown[i] > 0 && !(state.graph.removed[i] & 1))
    .sort((a, b) => state.notShown[b] - state.notShown[a] || a - b);
  const out = [];
  for (const index of order) {
    if (out.length >= limit) break;
    // At a whole CSS pixel, as a mouse event gives its place.
    const [x, y] = Array.from(state.view.nodePixel(index), (v) => Math.round(v / ratio));
    if (x < 0 || y < 0 || x >= state.canvas.clientWidth || y >= state.canvas.clientHeight || state.view.pick(x * ratio, y * ratio) !== index) continue;
    // Not one under the inspector's panel: a click there is the panel's.
    if (document.elementFromPoint(box.left + x, box.top + y) !== state.canvas) continue;
    out.push({ index, id: state.ids.get(index), notShown: state.notShown[index], x, y });
  }
  return out;
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
  current?.inspectNow?.();
  return current?.inspector?.state() ?? null;
}

/** For tests: the node drawn at the canvas pixel (x, y), as a click picks it, or null. */
export function nodeAt(x, y) {
  if (current) atNewest(current);
  const node = current?.view.pick(x, y);
  return node === undefined ? null : node;
}
