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
 * What the engine adds to the colour column's name when another node column
 * has it, as when SIZE BY and COLOR BY name the same thing (graph::view's
 * COLOR_SUFFIX): a result's columns have distinct names.
 */
const COLOR_SUFFIX = " (colour)";

/** An edge with no known row in its table (UGR's `edges.rows`). */
const NO_ROW = 0xffffffffffffffffn;

/**
 * Row `i`'s value in each of `columns` (a result's node or edge columns), as
 * [name, text] pairs; a row with no value in a column is left out of it. The
 * colour column's " (colour)" twin of another column is left out too.
 */
function valuesOf(columns, i) {
  const out = [];
  for (const c of columns) {
    if (c.name.endsWith(COLOR_SUFFIX) && columns.some((o) => o.name === c.name.slice(0, -COLOR_SUFFIX.length))) continue;
    const v = c.values[i];
    if (c.type === "dict") {
      if (v !== NULL) out.push([c.name, c.dict.get(v)]);
    } else if (c.type === "f32") {
      if (!Number.isNaN(v)) out.push([c.name, String(Number(v.toPrecision(6)))]);
    } else if (v !== NULL) {
      out.push([c.name, String(v)]);
    }
  }
  return out;
}

/**
 * The drawing of `result` (ugr.js's readResult of the engine's buffer):
 *
 *   n, src, dst    the nodes' count and the edges' ends, as dense node indexes
 *   adjacency      the edges' undirected adjacency, where the engine had it
 *                  to send ({offsets, targets}, UGR's edges.adjacency.*), for
 *                  the layout to use as it is; else null
 *   ids            each node's id as the shell prints it: {length, get(i)}
 *   removed, edgeRemoved
 *                  a byte for each node and each edge: 1 for one that is
 *                  not shown (hidden by the explore session; a hidden node
 *                  keeps its index), 0 otherwise. An edge at a removed node
 *                  is not shown either.
 *   labelOf(i)     node i's label (the vertex table it is of), or null
 *   keyTypeOf(i)   "int" or "text": how node i's key is written in SQL
 *   valuesOf(i)    node i's value in each node column, as [name, text] pairs
 *                  (the inspector's "drawn" values): what SIZE BY and COLOR BY
 *                  made and what the rule read (a hop's distance)
 *   edgeOf(e)      edge e, for the inspector: {label, row, values}, its edge
 *                  table (or null), its row in that table (a string, or null
 *                  where the engine did not know it) and its edge columns
 *   skipped        rows left out because an endpoint is NULL
 *   name           the property graph drawn, or null
 *   provenance     {sql, rule, roles?, notes?}: the SQL it came from, the
 *                  result → graph rule that drew it, the columns the rule read
 *                  and what it adds (what was left out, other readings)
 *   drawnAs        what it was drawn as, for the status line (drawnAs below)
 *   layout         when the rule asks for rings or layers: {kind, level}, each
 *                  node's ring or layer (a hop's distance, a path's step;
 *                  0xffffffff for none), a view of the buffer
 *   size, sizeBy   with SIZE BY: each node's value (NaN: none), and its name
 *   color, colorBy with COLOR BY: each node's colour index, 0..k for the k
 *                  largest categories (at most 8), 8 for the rest and for no
 *                  value; and its name
 *   colorLabels, colorCounts, colorOther, colorMissing
 *                  the legend: the coloured categories and how many nodes each
 *                  has, the nodes in other categories, and those with no value
 *                  (of the nodes shown)
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
    removed: nodes.flags,
    edgeRemoved: edges.flags,
    adjacency: edges.adjacency,
    skipped,
    name: result.graph,
    provenance: result.provenance,
    labelOf: (i) => nodes.labelNames[nodes.labels[i]]?.name || null,
    keyTypeOf: (i) => nodes.labelNames[nodes.labels[i]]?.key ?? "text",
    valuesOf: (i) => valuesOf(nodes.columns, i),
    edgeOf: (e) => {
      const row = edges.rows?.[e];
      return {
        label: edges.labelNames[edges.labels[e]] || null,
        row: row === undefined || row === NO_ROW ? null : String(row),
        values: valuesOf(edges.columns, e),
      };
    },
    drawnAs: drawnAs(result),
  };
  const column = (name) => {
    const found = nodes.columns.find((c) => c.name === name);
    if (!found) throw new Error(`the graph has no node column '${name}' to draw by`);
    return found;
  };
  // Rings and layers are laid out by level: the column the highlights name.
  const level = result.highlights?.distance;
  if (encodings.layout && encodings.layout !== "force" && level != null) {
    const c = column(level);
    if (c.type === "u32") graph.layout = { kind: encodings.layout, level: c.values };
  }
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
      // The legend counts the nodes shown.
      const shown = (nodes.flags[i] & 1) === 0;
      if (code < top) {
        color[i] = code;
        if (shown) counts[code]++;
      } else {
        color[i] = MAX_CATEGORIES;
        if (!shown) continue;
        if (code === NULL) missing++;
        else other++;
      }
    }
    const typed = c.name.endsWith(COLOR_SUFFIX) ? c.name.slice(0, -COLOR_SUFFIX.length) : null;
    // The suffix is there when another column has the name: SIZE BY's, or
    // the rule's (a hop's distance).
    graph.colorBy = typed !== null && nodes.columns.some((o) => o.name === typed) ? typed : c.name;
    graph.color = color;
    graph.colorLabels = Array.from({ length: top }, (_, i) => c.dict.get(i));
    graph.colorCounts = counts;
    graph.colorOther = other;
    graph.colorMissing = missing;
  }
  return graph;
}

/**
 * What `result` was drawn as, by its provenance: the rule, the graph, the
 * columns read and the rule's notes. "a path on karate (members, ties)",
 * "nodes of karate (id, pagerank, community)", "edges (src, dst)", "the graph
 * karate". The status line says "drawn as" and then this; "" when the result
 * names no rule. (graph::view's drawn_as says the same.)
 */
export function drawnAs(result) {
  const { rule, roles = [], notes = [] } = result.provenance ?? {};
  if (!rule) return "";
  const paths = result.highlights?.paths?.length ?? 0;
  const [what, joins] =
    {
      graph: ["the graph", " "],
      edges: ["edges", " of "],
      path: [paths > 1 ? `${paths.toLocaleString("en-US")} paths` : "a path", " on "],
      rings: ["rings", " on "],
      pattern: ["pattern matches", " on "],
      nodes: ["nodes", " of "],
    }[rule] ?? [rule, " on "];
  let out = what;
  if (result.graph != null) out += joins + result.graph;
  if (roles.length) out += ` (${roles.map((r) => r.column).join(", ")})`;
  for (const note of notes) out += ` · ${note}`;
  return out;
}
