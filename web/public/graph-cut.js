// The aggregated view, as the page keeps it (#498).
//
// Zoomed out, a large graph is a blot at any frame rate. Its layout has a
// hierarchy of it (graph-levels.js), and once the layout is done the Graph
// view draws a cut through that hierarchy in place of the graph: a disc
// for each group of nodes, as far apart on screen as discs can be told
// apart, and the merged edges between them. The layout worker makes the
// cut (graph-worker.js, "cut"), for the camera the view has when it comes
// to rest; while the camera moves, the last cut is drawn, moved as the
// camera is. Zooming in opens the groups in view, down to the graph's own
// nodes: then the worker's answer says `detail`, and the graph itself is
// drawn, exactly as with no cut.
//
// This module is which cut to ask for and what to make of an answer: no
// DOM, no wasm and no worker in it, so tests/graph-cut.test.ts runs it on
// the server. graph.js gives it the camera and does what it says.

/** A graph of this many nodes and edges is drawn aggregated: as graph-worker.js's COARSE_FROM. */
export const AGGREGATE_FROM = 200_000;
/** How long the camera is still before a cut is asked for, in ms. */
export const REST_MS = 150;
/**
 * The most merged edges of a cut the page asks for: the heaviest. As many
 * as a level's (graph-worker.js's LEVEL_EDGES): a frame at rest draws them
 * all, and with no GPU a wide, long edge is costly.
 */
export const CUT_EDGES = 1 << 14;
/**
 * How far apart the discs of a cut stand, about, and the greatest radius
 * one is drawn with, in CSS pixels: discs that far apart do not cover one
 * another.
 */
export const SEPARATION = 20;
export const DISC_RADIUS = 12;
/** A group's label is shown when its disc's radius is at least this, in CSS pixels; and at most this many labels. */
export const LABEL_RADIUS = 9;
export const MAX_LABELS = 24;

/** The status line's account of a cut: "aggregated: 348 groups of 131,072 nodes". */
export function aggregatedText(groups, nodes) {
  const n = (v) => v.toLocaleString("en-US");
  return `aggregated: ${n(groups)} ${groups === 1 ? "group" : "groups"} of ${n(nodes)} ${nodes === 1 ? "node" : "nodes"}`;
}

/** A group's label: how many nodes it stands for, short ("1,234", "12.3K", "1.2M"). */
export function countLabel(members) {
  if (members < 10_000) return members.toLocaleString("en-US");
  if (members < 1_000_000) return `${(members / 1000).toFixed(members < 100_000 ? 1 : 0)}K`;
  return `${(members / 1_000_000).toFixed(1)}M`;
}

/** Whether two cameras ([x, y, pixelsPerUnit, width, height]) are the same. */
export function sameCamera(a, b) {
  if (!a || !b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

/**
 * The cuts of one drawing of `nodes` nodes and `edges` edges. `detail`
 * forces full detail from the start (the `a` key, `.graph detail on`);
 * `separation` is in the canvas's pixels (SEPARATION times the device's
 * pixel ratio).
 *
 *   wanted     the user has not forced full detail
 *   available  the layout has a hierarchy to cut, as far as is known
 *   on         both, for a graph large enough: cuts are asked for
 *   shown      the cut drawn now, {groups, nodes, edges, totalEdges, ms},
 *              or null when the graph itself is
 */
export function createCuts({ nodes, edges, detail = false, from = AGGREGATE_FROM, maxEdges = CUT_EDGES, separation = SEPARATION }) {
  const large = nodes + edges >= from;
  let wanted = !detail;
  let available = true;
  // The newest request's id, the camera it was for, and whether it is
  // still to be answered.
  let id = 0;
  let asked = null;
  let waiting = false;
  let shown = null;
  // How long each cut took in the worker, in ms; and what making the
  // hierarchy's forest took there, once, and what it holds.
  const took = [];
  let prepared = null;

  return {
    large,
    took,
    get wanted() {
      return wanted;
    },
    get available() {
      return available;
    },
    get on() {
      return large && wanted && available;
    },
    get shown() {
      return shown;
    },
    get waiting() {
      return waiting;
    },
    get prepared() {
      return prepared;
    },

    /**
     * The message to post the worker for a cut at `camera`, or null if
     * none is to be asked for: cuts are off, or the newest request (or
     * the cut drawn) is already for this camera.
     */
    request(camera) {
      if (!this.on || sameCamera(camera, asked)) return null;
      asked = Array.from(camera);
      waiting = true;
      return { type: "cut", id: ++id, camera: asked, separation, maxEdges };
    },

    /**
     * What to do with the worker's answer `data`:
     *   "stale"   nothing: a newer request is out, or cuts were turned off
     *   "none"    the layout has no hierarchy (any more): draw the graph
     *   "detail"  what is in view is the graph's own nodes: draw the graph
     *   "cut"     draw `data` (GraphView.setCut)
     * After "none" no more are asked for until reset().
     */
    answer(data) {
      if (data.id !== id || !wanted) return "stale";
      waiting = false;
      if (!data.available) {
        available = false;
        shown = null;
        return "none";
      }
      took.push(data.ms);
      prepared = { ms: data.prepareMs ?? null, bytes: data.bytes ?? null };
      if (data.detail) {
        shown = null;
        return "detail";
      }
      shown = { groups: data.mass.length, nodes, edges: data.src.length, totalEdges: data.totalEdges, ms: data.ms };
      return "cut";
    },

    /**
     * Force full detail (`true`) or let the view aggregate again
     * (`false`). Returns whether a cut that was drawn is to be cleared.
     */
    setDetail(detail) {
      wanted = !detail;
      // An answer on its way is for nothing now; the next request is new.
      id++;
      asked = null;
      waiting = false;
      const clear = detail && shown !== null;
      if (detail) shown = null;
      return clear;
    },

    /**
     * The graph changed under the layout (a delta): the hierarchy is of
     * the graph before. Returns whether a cut that was drawn is to be
     * cleared. No more are asked for.
     */
    drop() {
      id++;
      asked = null;
      waiting = false;
      available = false;
      const clear = shown !== null;
      shown = null;
      return clear;
    },
  };
}
