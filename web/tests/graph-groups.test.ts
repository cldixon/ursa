// A grouped drawing's bookkeeping (public/graph-groups.js, #499): what a
// result's groups are, the scene that draws them, and where each node goes.
//
//   bun test tests/graph-groups.test.ts
import { expect, test } from "bun:test";
import { drawingOf } from "../public/graph-data.js";
import { PACK_GAP, PACK_GAP_SHARE, discRadii, groupedText, groupsOf, heaviest, idOf, pack, place, sceneOf } from "../public/graph-groups.js";

const NULL = 0xffffffff;
const dict = (values: string[]) => ({ length: values.length, get: (i: number) => values[i] });
// karate GROUP BY club with group 0 open, as ugr.js's readResult gives it:
// two groups, then three of group 0's members (keys 4, 7 and 9).
const result = (hidden: number[] = []) => ({
  graph: "karate",
  provenance: { rule: "groups" },
  highlights: null,
  nodes: {
    count: 5,
    keys: dict(["0", "1", "4", "7", "9"]),
    labels: Uint16Array.of(0, 0, 1, 1, 1),
    labelNames: [
      { name: "group", key: "int" },
      { name: "", key: "int" },
    ],
    flags: Uint8Array.from({ length: 5 }, (_, i) => (hidden.includes(i) ? 1 : 0)),
    columns: [
      { name: "club", type: "dict", values: Uint32Array.of(0, 1, 0, 0, 0), dict: dict(["Mr. Hi", "Officer"]) },
      { name: "members", type: "u32", values: Uint32Array.of(17, 17, NULL, NULL, NULL) },
      { name: "edges inside", type: "u32", values: Uint32Array.of(36, 31, NULL, NULL, NULL) },
      { name: "group x", type: "f32", values: Float32Array.of(NaN, NaN, 0.5, -0.5, 0) },
      { name: "group y", type: "f32", values: Float32Array.of(NaN, NaN, 0, 0.25, -0.75) },
    ],
  },
  edges: {
    count: 3,
    src: Uint32Array.of(0, 2, 3),
    dst: Uint32Array.of(1, 3, 4),
    flags: new Uint8Array(3),
    labels: new Uint16Array(3),
    labelNames: [""],
    columns: [{ name: "count", type: "u32", values: Uint32Array.of(11, NULL, NULL) }],
    adjacency: null,
  },
  encodings: { size_by: "members", color_by: null, group_by: "club", layout: "force" },
});

test("a result that groups by nothing has no groups", () => {
  const plain = result();
  plain.encodings.group_by = null as any;
  expect(groupsOf(plain)).toBeNull();
  expect(groupedText("louvain", 12)).toBe("grouped by louvain: 12 groups");
  expect(groupedText("club", 1)).toBe("grouped by club: 1 group");
});

test("the groups, their members and the open ones are read from the result", () => {
  const g = groupsOf(result())!;
  expect([g.by, g.groups, g.total]).toEqual(["club", 2, 34]);
  expect([g.names.get(0), g.names.get(1)]).toEqual(["Mr. Hi", "Officer"]);
  expect(Array.from(g.of)).toEqual([0, 1, 0, 0, 0]);
  expect(Array.from(g.color)).toEqual([0, 1, 0, 0, 0]);
  expect(g.open).toEqual([0]);
  expect([g.isGroup(1), g.isGroup(2)]).toEqual([true, false]);
  // A group is named group:N in a command, and a member by its key.
  const ids = result().nodes.keys;
  expect([idOf(g, ids, 1), idOf(g, ids, 3)]).toEqual(["group:1", "7"]);
  // With every member hidden (.collapse), the group is closed.
  expect(groupsOf(result([2, 3, 4]))!.open).toEqual([]);
  // The drawing has them, and shows no place in a disc as a value.
  const drawing = drawingOf(result());
  expect(drawing.grouped.groups).toBe(2);
  expect(drawing.valuesOf(2)).toEqual([["club", "Mr. Hi"]]);
  expect(drawing.valuesOf(0)).toEqual([
    ["club", "Mr. Hi"],
    ["members", "17"],
    ["edges inside", "36"],
  ]);
  expect(drawing.drawnAs).toBe("groups of karate");
});

test("the scene has a disc a node: closed and open groups, members, and hidden nodes", () => {
  const r = result();
  const g = groupsOf(r)!;
  const scene = sceneOf(r, g, new Map([[0, 2.5]]), 3);
  expect(Array.from(scene.mass)).toEqual([17, 17, 0, 0, 0]);
  expect(Array.from(scene.members)).toEqual([17, 17, 1, 1, 1]);
  expect(Array.from(scene.of)).toEqual([0, 1, 2, 3, 4]);
  // Group 0 is open, 2.5 units wide; group 1 is by its mass; members are points.
  expect(Array.from(scene.kinds)).toEqual([2.5, 0, -3, -3, -3]);
  // The edge between the groups first, by its count; then the members'.
  expect([Array.from(scene.src), Array.from(scene.dst), Array.from(scene.weight)]).toEqual([
    [0, 2, 3],
    [1, 3, 4],
    [11, 1, 1],
  ]);
  expect(scene.totalEdges).toBe(3);
  expect(sceneOf(r, g, new Map([[0, 2.5]]), 3, 2).src.length).toBe(2);
  // An open group with no radius yet is drawn closed.
  expect(Array.from(sceneOf(r, g, new Map(), 3).kinds)).toEqual([0, 0, -3, -3, -3]);
  // A hidden node is not drawn, nor its edges.
  const h = result([1, 3]);
  const hidden = sceneOf(h, groupsOf(h)!, new Map([[0, 2.5]]), 3);
  expect(Array.from(hidden.kinds)).toEqual([2.5, NaN, -3, NaN, -3]);
  expect(hidden.src.length).toBe(0);
  expect(heaviest(r, g, 12)).toEqual([{ src: 0, dst: 1, count: 11 }]);
  expect(heaviest(h, groupsOf(h)!, 12)).toEqual([]);
});

test("a member is in its group's disc, wherever the layout has the group", () => {
  const g = groupsOf(result())!;
  // Group 0 at (10, 20), group 1 at (-5, 0).
  const groupXY = Float32Array.of(10, -5, 20, 0);
  const xy = place(g, groupXY, new Map([[0, 4]]), 5);
  expect(Array.from(xy.subarray(0, 5))).toEqual([10, -5, 12, 8, 10]);
  expect(Array.from(xy.subarray(5))).toEqual([20, 0, 20, 21, 17]);
  // Every member is within the open radius of its group's middle.
  for (let i = 2; i < 5; i++) expect(Math.hypot(xy[i] - 10, xy[5 + i] - 20)).toBeLessThanOrEqual(4);
  // With no radius, a member is at the middle.
  expect(Array.from(place(g, groupXY, new Map(), 5).subarray(2, 5))).toEqual([10, 10, 10]);
});

test("discs that a layout put on one spot are moved apart, the same way every time", () => {
  const [w, h] = [800, 600];
  // Twenty groups of 1,000 nodes on (nearly) one spot, and four of one node far away.
  const members = Uint32Array.from({ length: 24 }, (_, i) => (i < 20 ? 1000 : 1));
  const radii = discRadii({ groups: 24, members } as any, w, h);
  // A group's disc is its share of 6% of the canvas; a node's is the least.
  expect(radii[0]).toBeCloseTo(Math.sqrt((0.06 * w * h * (1000 / 20004)) / Math.PI), 3);
  expect(radii[23]).toBe(1.5);
  const xy = new Float32Array(48);
  for (let i = 0; i < 20; i++) [xy[i], xy[24 + i]] = [Math.cos(i) * 1e-3, Math.sin(i) * 1e-3];
  [xy[20], xy[21], xy[22], xy[23]] = [50, -50, 0, 0];
  [xy[44], xy[45], xy[46], xy[47]] = [0, 0, 50, -50];
  const packed = pack(xy, radii, w, h);
  let [x0, y0, x1, y1] = [Infinity, Infinity, -Infinity, -Infinity];
  for (let a = 0; a < 24; a++) {
    [x0, y0, x1, y1] = [Math.min(x0, packed[a]), Math.min(y0, packed[24 + a]), Math.max(x1, packed[a]), Math.max(y1, packed[24 + a])];
    for (let b = a + 1; b < 24; b++) {
      const d = Math.hypot(packed[a] - packed[b], packed[24 + a] - packed[24 + b]);
      const room = radii[a] + radii[b] + Math.max(PACK_GAP, PACK_GAP_SHARE * (radii[a] + radii[b]));
      expect(d).toBeGreaterThanOrEqual(room - 0.01);
    }
  }
  // Together they take less room than the canvas has.
  expect(x1 - x0).toBeLessThan(w);
  expect(y1 - y0).toBeLessThan(h);
  expect(Array.from(pack(xy, radii, w, h))).toEqual(Array.from(packed));
  // Two groups keep the direction the layout gave them.
  const two = pack(Float32Array.of(0, 1, 0, 0), Float32Array.of(64, 64), w, h);
  expect(two[1] - two[0]).toBeGreaterThanOrEqual(128 + PACK_GAP_SHARE * 128 - 0.01);
  expect(two[2]).toBe(two[3]);
  expect(Array.from(pack(Float32Array.of(3, 4), Float32Array.of(10), w, h))).toEqual([0, 0]);
  expect(pack(new Float32Array(0), new Float32Array(0), w, h).length).toBe(0);
});
