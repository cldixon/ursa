// The aggregated view's bookkeeping (public/graph-cut.js, #498): which cut
// is asked for, and what is made of the worker's answer.
//
//   bun test tests/graph-cut.test.ts
import { expect, test } from "bun:test";
import { AGGREGATE_FROM, CUT_EDGES, SEPARATION, aggregatedText, countLabel, createCuts, sameCamera } from "../public/graph-cut.js";

const camera = [0, 0, 1, 800, 600];
const cut = (id: number, groups = 3, more = {}) => ({
  type: "cut",
  id,
  available: true,
  detail: false,
  ms: 12,
  mass: new Float32Array(groups),
  src: new Uint32Array(2),
  totalEdges: 5,
  ...more,
});

test("the status line and the labels say how many", () => {
  expect(aggregatedText(348, 131072)).toBe("aggregated: 348 groups of 131,072 nodes");
  expect(aggregatedText(1, 1)).toBe("aggregated: 1 group of 1 node");
  expect(countLabel(7)).toBe("7");
  expect(countLabel(9999)).toBe("9,999");
  expect(countLabel(12345)).toBe("12.3K");
  expect(countLabel(123456)).toBe("123K");
  expect(countLabel(2_500_000)).toBe("2.5M");
});

test("only a large graph is aggregated", () => {
  expect(createCuts({ nodes: 1000, edges: 5000 }).on).toBe(false);
  expect(createCuts({ nodes: 1000, edges: 5000 }).request(camera)).toBeNull();
  const large = createCuts({ nodes: 50_000, edges: AGGREGATE_FROM });
  expect(large.on).toBe(true);
  expect(createCuts({ nodes: 50_000, edges: AGGREGATE_FROM, detail: true }).on).toBe(false);
});

test("a cut is asked for once a camera, and the newest answer is the one drawn", () => {
  const cuts = createCuts({ nodes: 46_318, edges: 1_000_000 });
  const first = cuts.request(camera);
  expect(first).toEqual({ type: "cut", id: 1, camera, separation: SEPARATION, maxEdges: CUT_EDGES });
  // On a screen of two device pixels a CSS pixel, twice as many of the canvas's.
  expect(createCuts({ nodes: 46_318, edges: 1_000_000, separation: 2 * SEPARATION }).request(camera)?.separation).toBe(40);
  expect(cuts.waiting).toBe(true);
  // The same camera again: nothing new to ask.
  expect(cuts.request([...camera])).toBeNull();
  // The camera moved before the answer came: a second request, and the
  // first's answer is stale.
  const second = cuts.request([5, 0, 2, 800, 600]);
  expect(second?.id).toBe(2);
  expect(cuts.answer(cut(1))).toBe("stale");
  expect(cuts.shown).toBeNull();
  expect(cuts.answer(cut(2, 7))).toBe("cut");
  expect(cuts.waiting).toBe(false);
  expect(cuts.shown).toEqual({ groups: 7, nodes: 46_318, edges: 2, totalEdges: 5, ms: 12 });
  expect(cuts.took).toEqual([12]);
  // Zoomed in to the graph's own nodes: the graph is drawn.
  cuts.request([5, 0, 900, 800, 600]);
  expect(cuts.answer(cut(3, 0, { detail: true }))).toBe("detail");
  expect(cuts.shown).toBeNull();
  // And out again.
  cuts.request(camera);
  expect(cuts.answer(cut(4))).toBe("cut");
  expect(cuts.shown?.groups).toBe(3);
});

test("full detail is forced and let go", () => {
  const cuts = createCuts({ nodes: 46_318, edges: 1_000_000 });
  cuts.request(camera);
  cuts.answer(cut(1));
  // Forced: the cut drawn is to be cleared, and nothing is asked for.
  expect(cuts.setDetail(true)).toBe(true);
  expect([cuts.on, cuts.wanted, cuts.shown]).toEqual([false, false, null]);
  expect(cuts.request(camera)).toBeNull();
  expect(cuts.setDetail(true)).toBe(false);
  // Let go: the same camera is asked for again.
  expect(cuts.setDetail(false)).toBe(false);
  const again = cuts.request(camera);
  expect(again?.camera).toEqual(camera);
  // An answer to a request made before the toggle is stale.
  expect(cuts.answer(cut(1))).toBe("stale");
  expect(cuts.answer(cut(again!.id))).toBe("cut");
  // Forced while an answer is on its way: it is dropped.
  cuts.request([1, 1, 1, 800, 600]);
  cuts.setDetail(true);
  expect(cuts.answer(cut(again!.id + 1))).toBe("stale");
});

test("a layout with no hierarchy is asked once", () => {
  const cuts = createCuts({ nodes: 46_318, edges: 1_000_000 });
  const asked = cuts.request(camera);
  expect(cuts.answer({ type: "cut", id: asked!.id, available: false })).toBe("none");
  expect([cuts.on, cuts.available]).toEqual([false, false]);
  expect(cuts.request([9, 9, 9, 800, 600])).toBeNull();
});

test("a delta ends the cuts", () => {
  const cuts = createCuts({ nodes: 46_318, edges: 1_000_000 });
  expect(cuts.drop()).toBe(false);
  const fresh = createCuts({ nodes: 46_318, edges: 1_000_000 });
  fresh.request(camera);
  fresh.answer(cut(1));
  expect(fresh.drop()).toBe(true);
  expect([fresh.on, fresh.shown]).toEqual([false, null]);
  expect(fresh.answer(cut(1))).toBe("stale");
});

test("cameras compare by value", () => {
  expect(sameCamera(camera, [...camera])).toBe(true);
  expect(sameCamera(camera, [0, 0, 1, 800, 601])).toBe(false);
  expect(sameCamera(null, camera)).toBe(false);
  expect(sameCamera(new Float32Array(camera), camera)).toBe(true);
});
