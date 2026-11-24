// graph-levels.js: the layout's levels as the page holds them, and where a
// new level's nodes are drawn on their way out from their parents (#497).
import { expect, test } from "bun:test";
// @ts-expect-error: a plain script of the page.
import { createLevels, NONE, SPLIT_MS } from "../public/graph-levels.js";

const top = () => ({ level: 2, n: 2, mass: Float32Array.of(6, 4), src: Uint32Array.of(0), dst: Uint32Array.of(1), weight: Float32Array.of(3), totalEdges: 1, xy: Float32Array.of(0, 10, 0, 20) });
// Four nodes: two from node 0, one from node 1, and one with no parent.
const next = () => ({ level: 1, n: 4, mass: Float32Array.of(3, 3, 3, 1), parents: Uint32Array.of(0, 0, 1, NONE), src: Uint32Array.of(), dst: Uint32Array.of(), weight: Float32Array.of(), totalEdges: 0, xy: Float32Array.of(-2, 2, 12, 50, -4, 4, 24, 60) });

test("the coarsest level is in place at once, and a frame is given once", () => {
  const levels = createLevels();
  expect(levels.frame(0)).toBeNull();
  levels.add(top(), 1000);
  expect(levels.current.level).toBe(2);
  expect(levels.splitting).toBe(false);
  expect(levels.frame(1000)).toEqual({ level: 2, xy: Float32Array.of(0, 10, 0, 20), moving: false });
  expect(levels.frame(1016)).toBeNull();
  // New positions: one more frame.
  expect(levels.place(2, Float32Array.of(1, 11, 1, 21))).toBe(true);
  expect(levels.frame(1032)?.xy).toEqual(Float32Array.of(1, 11, 1, 21));
  expect(levels.frame(1048)).toBeNull();
  // Not this level's, or not a level there is.
  expect(levels.place(2, Float32Array.of(1, 2))).toBe(false);
  expect(levels.place(5, Float32Array.of(1, 2))).toBe(false);
});

test("a new level's nodes move out from their parents for SPLIT_MS", () => {
  const levels = createLevels();
  levels.add(top(), 0);
  levels.frame(0);
  levels.add(next(), 1000);
  expect(levels.splitting).toBe(true);
  expect(levels.levels[2].xy).toEqual(Float32Array.of(0, 10, 0, 20));
  // At the start, each node at its parent's place; the one with none, in its own.
  let f = levels.frame(1000);
  expect(f.moving).toBe(true);
  expect(Array.from(f.xy)).toEqual([0, 0, 10, 50, 0, 0, 20, 60]);
  // Part of the way, and monotonically nearer.
  f = levels.frame(1000 + SPLIT_MS / 2);
  expect(f.moving).toBe(true);
  expect(f.xy[0]).toBeLessThan(0);
  expect(f.xy[0]).toBeGreaterThan(-2);
  expect(f.xy[3]).toBe(50);
  const half = f.xy[0];
  // Newer positions meanwhile: the nodes head for those.
  levels.place(1, Float32Array.of(-4, 2, 12, 50, -4, 4, 24, 60));
  f = levels.frame(1000 + SPLIT_MS * 0.75);
  expect(f.moving).toBe(true);
  expect(f.xy[0]).toBeLessThan(half);
  // In place: the positions themselves, once.
  f = levels.frame(1000 + SPLIT_MS);
  expect(f).toEqual({ level: 1, xy: Float32Array.of(-4, 2, 12, 50, -4, 4, 24, 60), moving: false });
  expect(levels.splitting).toBe(false);
  expect(levels.frame(1000 + SPLIT_MS + 16)).toBeNull();
});

test("under reduced motion a new level is in place at once", () => {
  const levels = createLevels({ animate: false });
  levels.add(top(), 0);
  levels.add(next(), 10);
  expect(levels.splitting).toBe(false);
  expect(levels.frame(10)).toEqual({ level: 1, xy: next().xy, moving: false });
});

test("the graph's own nodes come out from level 1, and are then the view's", () => {
  const levels = createLevels();
  levels.add({ ...next(), parents: undefined }, 0);
  levels.frame(0);
  // Level 0 arrives as its parents alone; its positions follow.
  levels.add({ level: 0, parents: Uint32Array.of(3, 3, NONE) }, 500);
  expect(levels.current.n).toBe(3);
  expect(levels.frame(500)).toBeNull();
  expect(levels.place(0, Float32Array.of(0, 1, 2, 0, 1, 2))).toBe(true);
  let f = levels.frame(500);
  expect(f.level).toBe(0);
  expect(Array.from(f.xy)).toEqual([50, 50, 2, 60, 60, 2]);
  // Settled (a test, or the layout's end): in place, and let go.
  f = levels.settle();
  expect(f).toEqual({ level: 0, xy: Float32Array.of(0, 1, 2, 0, 1, 2), moving: false });
  expect(levels.levels[0].xy).toBeNull();
  expect(levels.frame(900)).toBeNull();
  expect(levels.place(0, Float32Array.of(0, 1, 2, 0, 1, 2))).toBe(false);
  expect(levels.frame(900)).toBeNull();
  // The levels above are kept as they arrived, for whoever cuts through them.
  expect(levels.levels[1].mass).toEqual(Float32Array.of(3, 3, 3, 1));
});

test("a delta under the layout drops the graph's nodes on their way", () => {
  const levels = createLevels();
  levels.add({ ...next(), parents: undefined }, 0);
  levels.add({ level: 0, parents: Uint32Array.of(3, 3, NONE) }, 500);
  levels.place(0, Float32Array.of(0, 1, 2, 0, 1, 2));
  expect(levels.frame(520)?.moving).toBe(true);
  levels.forget();
  expect(levels.splitting).toBe(false);
  expect(levels.frame(540)).toBeNull();
});
