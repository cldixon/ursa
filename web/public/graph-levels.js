// The levels of the layout's hierarchy, as the page holds them (#497).
//
// A large graph is laid out coarse to fine: a pyramid of it, a few dozen
// nodes at the top, each standing for thousands, then twice as many, down
// to the graph itself. Until the layout reaches the graph, the layout
// worker (graph-worker.js) sends the level it is laying out as a graph of
// its own, and the view draws that (GraphView.setLevel): a disc for each
// of the level's nodes, its area by the node's mass, and the level's
// heaviest merged edges, each as wide as its weight says.
//
// This module keeps what the worker sent of each level, and says where
// the nodes of the level drawn now are: for SPLIT_MS after the layout
// comes down a level, each node on its way out from its parent's place to
// its own (and at once, under prefers-reduced-motion). It has no DOM and
// no wasm in it, so tests/graph-levels.test.ts runs it on the server.
//
// `levels[l]` is level `l` as it arrived, and its newest positions:
//
//   { level, n,          which level (0 is the graph itself), and its nodes
//     mass,              Float32Array(n): the summed degree + 1 of the
//                        graph's nodes each stands for
//     parents,           Uint32Array(n): each node's parent, a node of
//                        level l + 1 (NONE for none); absent at the top
//     src, dst, weight,  its heaviest merged edges, heaviest first
//     totalEdges,        how many merged edges it has
//     xy }               Float32Array(2n): x then y, the newest
//
// in the level's own node numbering: a cut through the hierarchy (#498)
// is some nodes of each level, found by going down `parents`. Level 0 has
// only `parents` (its nodes, masses and edges are the graph's own, and its
// positions the view's), and those only until its nodes are in place.

/** A node with no parent: it joined the drawing at this level. */
export const NONE = 0xffffffff;
/** How long the nodes of a new level take to move out from their parents, in ms. */
export const SPLIT_MS = 150;

/**
 * The levels of one layout. `animate` is false under
 * prefers-reduced-motion: a new level's nodes are in place at once.
 */
export function createLevels({ animate = true, duration = SPLIT_MS } = {}) {
  const levels = [];
  // The level drawn now, where its nodes started from (x then y, NaN for
  // a node that starts in place) and when; and whether its newest
  // positions have been given out.
  let current = null;
  let from = null;
  let began = 0;
  let fresh = false;
  let blend = null;

  /** Where each node of `level` starts from: its parent's newest place. */
  const startsOf = (level) => {
    const above = levels[level.level + 1];
    if (!animate || duration <= 0 || !level.parents || !above?.xy) return null;
    const { n, parents } = level;
    const starts = new Float32Array(2 * n).fill(NaN);
    for (let i = 0; i < n; i++) {
      const p = parents[i];
      if (p >= above.n) continue;
      starts[i] = above.xy[p];
      starts[n + i] = above.xy[above.n + p];
    }
    return starts;
  };

  return {
    levels,
    /** The level drawn now: the last that arrived, or null. */
    get current() {
      return current;
    },
    /** Whether the nodes of the level drawn are still moving out from their parents. */
    get splitting() {
      return from !== null;
    },

    /** The layout came to a level: the worker's "level" message, at `now` (ms). */
    add(message, now) {
      const { level, mass, parents, src, dst, weight, totalEdges, xy } = message;
      const n = message.n ?? parents?.length ?? 0;
      current = { level, n, mass, parents, src, dst, weight, totalEdges, xy: xy ?? null };
      levels[level] = current;
      from = startsOf(current);
      began = now;
      blend = null;
      fresh = Boolean(xy);
      return current;
    },

    /** The newest positions of `level` (x then y): the worker's "levelPositions", or for level 0 its "positions". */
    place(level, xy) {
      const held = levels[level];
      if (!held || xy.length !== 2 * held.n) return false;
      // The graph's own positions are kept only while its nodes are on
      // their way.
      if (level === 0 && held !== current) return false;
      held.xy = xy;
      if (held === current) fresh = true;
      return true;
    },

    /**
     * Where to draw the nodes of the current level at `now`: {level, xy,
     * moving}, `moving` while they are on their way out from their
     * parents (draw again next frame); or null if nothing changed since
     * the last call. `xy` is the caller's to read until the next call.
     */
    frame(now) {
      if (!current?.xy) return null;
      const { level, xy } = current;
      if (from && from.length === xy.length) {
        const t = (now - began) / duration;
        if (t < 1) {
          // Fast out, slow in: 1 - (1 - t)^3.
          const e = 1 - (1 - Math.max(t, 0)) ** 3;
          blend ??= new Float32Array(xy.length);
          for (let i = 0; i < xy.length; i++) {
            const f = from[i];
            blend[i] = f === f ? f + (xy[i] - f) * e : xy[i];
          }
          fresh = false;
          return { level, xy: blend, moving: true };
        }
        // In place: this last frame is the positions themselves.
        fresh = true;
      }
      from = null;
      blend = null;
      if (!fresh) return null;
      fresh = false;
      const out = { level, xy, moving: false };
      // The graph's own positions are the view's from here on.
      if (level === 0) this.forget();
      return out;
    },

    /** The nodes in place at once, as frame() would have them after SPLIT_MS: its answer, or null. */
    settle() {
      began = -Infinity;
      return this.frame(0);
    },

    /** The graph changed under the layout (a delta): level 0's nodes are no longer the ones that were on their way. */
    forget() {
      if (levels[0]) {
        levels[0].xy = null;
        levels[0].parents = null;
      }
      if (current?.level === 0) {
        current = null;
        from = null;
        blend = null;
        fresh = false;
      }
    },
  };
}
