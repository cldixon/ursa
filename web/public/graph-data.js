// The engine's graph as the Graph view draws it (graph.js).
//
// The engine resolves `.graph` (ursa-engine's graph::view) into a graph
// result and sends it as one UGR buffer (ursa-viz/UGR.md), which ugr.js reads
// as typed arrays over the buffer, with no copy. This turns what it read into
// the drawing: dense node indexes, each node's id, and what SIZE BY and
// COLOR BY made, with the legend.
//
// It has no DOM and no wasm in it, so tests/graph-data.test.ts runs it on the
// fixtures the engine's own tests write.

/** The most categories COLOR BY colours; the palette has as many, then "other". */
export const MAX_CATEGORIES = 8;
/** The null of a dictionary column's codes: a node with no value. */
const NULL = 0xffffffff;
/**
 * What the engine adds to the colour column's name when SIZE BY and COLOR BY
 * name the same thing (graph::view's COLOR_SUFFIX): a result's columns have
 * distinct names.
 */
const COLOR_SUFFIX = " (colour)";

/**
 * The drawing of `result` (ugr.js's readResult of the engine's buffer):
 *
 *   n, src, dst    the nodes' count and the edges' ends, as dense node indexes
 *   ids            each node's id as the shell prints it: {length, get(i)}
 *   labelOf(i)     node i's label (the vertex table it is of), or null
 *   skipped        rows left out because an endpoint is NULL
 *   name           the property graph drawn, or null
 *   provenance     {sql, rule}: the SQL it came from and the rule that drew it
 *   size, sizeBy   with SIZE BY: each node's value (NaN: none), and its name
 *   color, colorBy with COLOR BY: each node's colour index, 0..k for the k
 *                  largest categories (at most 8), 8 for the rest and for no
 *                  value; and its name
 *   colorLabels, colorCounts, colorOther, colorMissing
 *                  the legend: the coloured categories and how many nodes each
 *                  has, the nodes in other categories, and those with no value
 *
 * The arrays are views of the buffer `result` was read from, except `color`.
 */
export function drawingOf(result, skipped = 0) {
  const { nodes, edges, encodings } = result;
  const graph = {
    n: nodes.count,
    src: edges.src,
    dst: edges.dst,
    ids: nodes.keys,
    skipped,
    name: result.graph,
    provenance: result.provenance,
    labelOf: (i) => nodes.labelNames[nodes.labels[i]]?.name || null,
  };
  const column = (name) => {
    const found = nodes.columns.find((c) => c.name === name);
    if (!found) throw new Error(`the graph has no node column '${name}' to draw by`);
    return found;
  };
  if (encodings.size_by != null) {
    const c = column(encodings.size_by);
    if (c.type === "dict") throw new Error(`the node column '${c.name}' is text, and sizes are numbers`);
    graph.sizeBy = c.name;
    graph.size = c.type === "f32" ? c.values : Float32Array.from(c.values, (v) => (v === NULL ? NaN : v));
  }
  if (encodings.color_by != null) {
    const c = column(encodings.color_by);
    if (c.type !== "dict") throw new Error(`the node column '${c.name}' is not categories, and colours are`);
    // The categories are in order of rank: the largest first.
    const top = Math.min(c.dict.length, MAX_CATEGORIES);
    const color = new Uint32Array(nodes.count);
    const counts = new Array(top).fill(0);
    let other = 0;
    let missing = 0;
    for (let i = 0; i < color.length; i++) {
      const code = c.values[i];
      if (code < top) {
        color[i] = code;
        counts[code]++;
      } else {
        color[i] = MAX_CATEGORIES;
        if (code === NULL) missing++;
        else other++;
      }
    }
    const typed = c.name.endsWith(COLOR_SUFFIX) ? c.name.slice(0, -COLOR_SUFFIX.length) : null;
    graph.colorBy = typed !== null && typed === encodings.size_by ? typed : c.name;
    graph.color = color;
    graph.colorLabels = Array.from({ length: top }, (_, i) => c.dict.get(i));
    graph.colorCounts = counts;
    graph.colorOther = other;
    graph.colorMissing = missing;
  }
  return graph;
}
