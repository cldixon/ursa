// A grouped drawing, as the page keeps it (#499).
//
// `.graph karate GROUP BY club` draws a disc for each club in place of its
// members, and the ties between the clubs, counted. The engine's result
// (ursa-engine's graph::view::groups) has a node for each group, of the
// label "group", with the column the result is grouped by (the group's
// value), "members" (how many nodes it stands for) and "edges inside"; and
// an edge for each two groups with edges between them, with "count". The
// members are not in it. A double-click on a disc runs `.expand group:N`,
// which adds that group's members and the edges among them to the same
// result, each member with its group's value and its place in the group's
// disc ("group x", "group y": a point of the unit disc); `.collapse
// group:N` hides them again.
//
// The Graph view draws all of it as one scene of discs and links
// (GraphView.setCut, as the aggregated view's cuts are drawn, #498): a
// closed group by its mass, an open one as a rim of the radius it had when
// it opened, in the layout's units, with its members as points inside it.
// Only the groups are laid out; a member is where its group is, moved by
// its place in the disc.
//
// This module is what a result's groups are, the scene that draws them and
// where each node goes: no DOM, no wasm and no worker in it, so
// tests/graph-groups.test.ts runs it on the server.

/** The label of the nodes that are groups (graph::view::groups's GROUP). */
export const GROUP = "group";
/** The node columns of a grouped result, and its edge column. */
export const MEMBERS = "members";
export const INSIDE = "edges inside";
export const X = "group x";
export const Y = "group y";
export const COUNT = "count";
/** The most links of a grouped scene: the heaviest between groups, then those among members. */
export const SCENE_EDGES = 1 << 16;
/** A member's radius, in CSS pixels. */
export const MEMBER_RADIUS = 2.5;
/** The most colours the groups take; the rest are "other" (graph-data.js's MAX_CATEGORIES). */
const MAX_CATEGORIES = 8;
const NULL = 0xffffffff;

/** The legend's line: "grouped by louvain: 12 groups". */
export function groupedText(by, groups) {
  return `grouped by ${by}: ${groups.toLocaleString("en-US")} ${groups === 1 ? "group" : "groups"}`;
}

/**
 * The groups of `result` (ugr.js's readResult), or null if it is not
 * grouped (its encodings group by nothing):
 *
 *   by         what it is grouped by: the group column's name
 *   groups     how many groups there are: nodes 0..groups-1 are the groups
 *   total      how many nodes the groups stand for
 *   names      {length, get(g)}: each group's value
 *   members    for each group, how many nodes it stands for
 *   isGroup(i) whether node i is a group (else it is a member of an open one)
 *   of         for each node, its group's node: itself, for a group
 *   ux, uy     for each node, its place in its group's disc (NaN for a group)
 *   color      for each node, its colour index: its group's, 0..7 for the
 *              eight largest groups and 8 for the rest
 *   open       the groups with a member shown, ascending
 */
export function groupsOf(result) {
  const { nodes, encodings } = result;
  const by = encodings?.group_by;
  if (by == null) return null;
  const column = (name) => nodes.columns.find((c) => c.name === name);
  const group = column(by);
  const members = column(MEMBERS);
  const [ux, uy] = [column(X), column(Y)];
  if (!group || group.type !== "dict" || !members || !ux || !uy) throw new Error(`the graph is grouped by '${by}', and has not the columns of a grouped one`);
  const label = nodes.labelNames.findIndex((l) => l.name === GROUP);
  let groups = 0;
  while (groups < nodes.count && nodes.labels[groups] === label) groups++;
  const of = new Uint32Array(nodes.count);
  const color = new Uint32Array(nodes.count);
  const open = new Set();
  let total = 0;
  for (let i = 0; i < nodes.count; i++) {
    const code = group.values[i];
    of[i] = i < groups || code === NULL || code >= groups ? i : code;
    color[i] = Math.min(of[i] < groups ? of[i] : MAX_CATEGORIES, MAX_CATEGORIES);
    if (i < groups) total += members.values[i] === NULL ? 0 : members.values[i];
    else if (of[i] !== i && (nodes.flags[i] & 1) === 0) open.add(of[i]);
  }
  return {
    by,
    groups,
    total,
    names: group.dict,
    members: members.values,
    isGroup: (i) => i < groups,
    of,
    ux: ux.values,
    uy: uy.values,
    color,
    open: [...open].sort((a, b) => a - b),
  };
}

/**
 * The id a command names node `i` by: "group:3" for a group, and a
 * member's own key.
 */
export function idOf(grouped, ids, i) {
  return grouped.isGroup(i) ? `${GROUP}:${ids.get(i)}` : String(ids.get(i));
}

/**
 * The scene that draws a grouped `result` (GraphView.setCut's arguments):
 * a disc for each node of the result, its mass the nodes it stands for
 * (none, for a member); the edges between groups, heaviest first, and
 * then the edges among members, of weight 1, at most `maxEdges` in all;
 * and `kinds`, how each node is drawn: by its mass (0), as an open disc of
 * the radius `radii` has for it (a group in `grouped.open`), as a point
 * of `point` pixels (a member), or not at all (NaN: a hidden node).
 *
 * `radii` is a Map from a group to its open radius in the layout's units.
 */
export function sceneOf(result, grouped, radii, point, maxEdges = SCENE_EDGES) {
  const { nodes, edges } = result;
  const n = nodes.count;
  const mass = new Float32Array(n);
  const members = new Uint32Array(n).fill(1);
  const kinds = new Float32Array(n);
  const of = Uint32Array.from({ length: n }, (_, i) => i);
  for (let i = 0; i < n; i++) {
    const hidden = (nodes.flags[i] & 1) !== 0;
    if (grouped.isGroup(i)) {
      const count = grouped.members[i] === NULL ? 0 : grouped.members[i];
      mass[i] = count;
      members[i] = count;
      const radius = radii.get(i);
      kinds[i] = hidden ? NaN : grouped.open.includes(i) && radius > 0 ? radius : 0;
    } else {
      kinds[i] = hidden ? NaN : -point;
    }
  }
  // The edges shown: those between groups by their count, heaviest
  // first, and then the members', in the result's order.
  const count = edges.columns.find((c) => c.name === COUNT)?.values;
  const between = [];
  const among = [];
  for (let e = 0; e < edges.count; e++) {
    if (edges.flags[e] & 1) continue;
    const [s, d] = [edges.src[e], edges.dst[e]];
    if ((nodes.flags[s] & 1) || (nodes.flags[d] & 1)) continue;
    if (grouped.isGroup(s) && grouped.isGroup(d)) between.push(e);
    else among.push(e);
  }
  const weightOf = (e) => (count && count[e] !== NULL ? count[e] : 1);
  between.sort((a, b) => weightOf(b) - weightOf(a) || a - b);
  const kept = between.slice(0, maxEdges).concat(among.slice(0, Math.max(0, maxEdges - between.length)));
  const src = new Uint32Array(kept.length);
  const dst = new Uint32Array(kept.length);
  const weight = new Float32Array(kept.length);
  kept.forEach((e, i) => {
    src[i] = edges.src[e];
    dst[i] = edges.dst[e];
    weight[i] = grouped.isGroup(edges.src[e]) ? weightOf(e) : 1;
  });
  return { mass, members, of, src, dst, weight, totalEdges: between.length + among.length, kinds };
}

/**
 * Where every node of a grouped result goes: `xy` (x for each node, then
 * y), from the groups' places `groupXY` (x for each group, then y: the
 * layout's). A group is where the layout has it; a member is at its
 * group's middle, moved by its place in the unit disc times the group's
 * open radius (`radii`, a Map from a group to it; with none, the member
 * is at the middle).
 */
export function place(grouped, groupXY, radii, n) {
  const g = grouped.groups;
  const xy = new Float32Array(2 * n);
  for (let i = 0; i < n; i++) {
    const group = grouped.of[i];
    const at = group < g ? group : 0;
    let [x, y] = [groupXY[at] ?? 0, groupXY[g + at] ?? 0];
    if (i >= g) {
      const r = radii.get(group) ?? 0;
      const [ux, uy] = [grouped.ux[i], grouped.uy[i]];
      if (r > 0 && !Number.isNaN(ux) && !Number.isNaN(uy)) {
        x += r * ux;
        y += r * uy;
      }
    }
    xy[i] = x;
    xy[n + i] = y;
  }
  return xy;
}

/**
 * The heaviest edges between the groups of a grouped `result` that are
 * shown, at most `most`: [{src, dst, count}], heaviest first. What the
 * view labels with their counts.
 */
export function heaviest(result, grouped, most) {
  const { nodes, edges } = result;
  const count = edges.columns.find((c) => c.name === COUNT)?.values;
  if (!count) return [];
  const between = [];
  for (let e = 0; e < edges.count; e++) {
    if (edges.flags[e] & 1) continue;
    const [s, d] = [edges.src[e], edges.dst[e]];
    if (!grouped.isGroup(s) || !grouped.isGroup(d) || (nodes.flags[s] & 1) || (nodes.flags[d] & 1) || count[e] === NULL) continue;
    between.push(e);
  }
  between.sort((a, b) => count[b] - count[a] || a - b);
  return between.slice(0, most).map((e) => ({ src: edges.src[e], dst: edges.dst[e], count: count[e] }));
}

// A disc's size on a fitted canvas, as the view draws it (ursa-viz's
// levels.rs: COVERAGE, MIN_RADIUS and MAX_RADIUS_SHARE).
const COVERAGE = 0.06;
const MIN_RADIUS = 1.5;
const MAX_RADIUS_SHARE = 0.12;
/** The least room between two discs that are packed, in pixels, and the room as a share of their radii. */
export const PACK_GAP = 8;
export const PACK_GAP_SHARE = 0.35;

/**
 * Each group's radius on a fitted `width` x `height` canvas, in pixels: its
 * area is its share of the nodes, of 6% of the canvas (LevelScene::radius
 * at zoom 1).
 */
export function discRadii(grouped, width, height) {
  const radii = new Float32Array(grouped.groups);
  let total = 0;
  const mass = (g) => (grouped.members[g] === NULL ? 0 : grouped.members[g]);
  for (let g = 0; g < grouped.groups; g++) total += mass(g);
  const most = Math.max(MIN_RADIUS, MAX_RADIUS_SHARE * Math.min(width, height));
  for (let g = 0; g < grouped.groups; g++) {
    const share = total > 0 ? mass(g) / total : 0;
    radii[g] = Math.min(most, Math.max(MIN_RADIUS, Math.sqrt((COVERAGE * width * height * share) / Math.PI)));
  }
  return radii;
}

/**
 * The groups where a layout put them (`xy`: x for each, then y), moved so
 * that no two discs overlap: `radii[g]` is group g's radius in pixels, and
 * the answer is in pixels too, about the origin, each two discs at least
 * PACK_GAP apart (and PACK_GAP_SHARE of their radii).
 *
 * A layout by the edges alone knows nothing of the discs' sizes: twenty
 * communities that all have edges to one another are laid out on one
 * spot. So the layout is scaled to half the canvas, and then, `iterations`
 * times, every disc is pulled toward the middle (less each time) and each
 * two that are too close are pushed apart, the smaller the further. The
 * layout says which group is beside which; the packing says how far.
 * The same input gives the same answer.
 */
export function pack(xy, radii, width, height, iterations = 200) {
  const n = radii.length;
  const x = new Float64Array(n);
  const y = new Float64Array(n);
  let [x0, y0, x1, y1] = [Infinity, Infinity, -Infinity, -Infinity];
  for (let i = 0; i < n; i++) {
    const [px, py] = [xy[i], xy[n + i]];
    if (!Number.isFinite(px) || !Number.isFinite(py)) continue;
    [x0, y0, x1, y1] = [Math.min(x0, px), Math.min(y0, py), Math.max(x1, px), Math.max(y1, py)];
  }
  const span = Math.max(x1 - x0, y1 - y0);
  const scale = span > 0 ? (0.5 * Math.min(width, height)) / span : 1;
  for (let i = 0; i < n; i++) {
    const ok = Number.isFinite(xy[i]) && Number.isFinite(xy[n + i]);
    x[i] = ok ? (xy[i] - (x0 + x1) / 2) * scale : 0;
    y[i] = ok ? (xy[n + i] - (y0 + y1) / 2) * scale : 0;
  }
  let reach = 0;
  for (let i = 0; i < n; i++) reach = Math.max(reach, radii[i]);
  const room = (a, b) => radii[a] + radii[b] + Math.max(PACK_GAP, PACK_GAP_SHARE * (radii[a] + radii[b]));
  const order = Array.from({ length: n }, (_, i) => i);
  // Push apart each two discs that are too close: a sweep along x.
  const collide = () => {
    order.sort((a, b) => x[a] - radii[a] - (x[b] - radii[b]) || a - b);
    let moved = false;
    for (let i = 0; i < n; i++) {
      const a = order[i];
      // The furthest right a disc that touches a's room can start.
      const right = x[a] + radii[a] + Math.max(PACK_GAP, PACK_GAP_SHARE * (radii[a] + reach));
      for (let j = i + 1; j < n; j++) {
        const b = order[j];
        if (x[b] - radii[b] > right) break;
        let [dx, dy] = [x[b] - x[a], y[b] - y[a]];
        let d = Math.hypot(dx, dy);
        const need = room(a, b);
        if (d >= need) continue;
        if (d < 1e-9) {
          // On one spot: apart along a direction of their own.
          const turn = (a * 0.618033988749895 + b * 0.381966011250105) % 1;
          [dx, dy, d] = [Math.cos(2 * Math.PI * turn), Math.sin(2 * Math.PI * turn), 1];
        }
        const push = (need - d) / d;
        // The smaller disc moves the further.
        const [wa, wb] = [radii[a] ** 2, radii[b] ** 2];
        const share = wa + wb > 0 ? wb / (wa + wb) : 0.5;
        x[a] -= dx * push * share;
        y[a] -= dy * push * share;
        x[b] += dx * push * (1 - share);
        y[b] += dy * push * (1 - share);
        moved = true;
      }
    }
    return moved;
  };
  for (let it = 0; it < iterations; it++) {
    const pull = 0.06 * (1 - it / iterations);
    for (let i = 0; i < n; i++) {
      x[i] -= x[i] * pull;
      y[i] -= y[i] * pull;
    }
    collide();
  }
  // Only apart, until none are too close.
  for (let it = 0; it < 50 && collide(); it++);
  const out = new Float32Array(2 * n);
  for (let i = 0; i < n; i++) {
    out[i] = x[i];
    out[n + i] = y[i];
  }
  return out;
}
