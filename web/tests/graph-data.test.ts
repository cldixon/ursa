// `.graph`'s data path, engine to page, without a browser: the UGR bytes
// ursa-engine's graph::view writes (the fixtures of
// ursa-engine/tests/graph_view.rs), read by ursa-viz's ui/ugr.js and shaped
// by public/graph-data.js as the Graph view draws them. Each NAME.ugr comes
// with a NAME.json that Rust wrote from its own values, never from the
// bytes, so this is Rust → UGR → JavaScript. Floats are compared as bits.
//
//   bun test web/tests/graph-data.test.ts

import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { readResult } from "../../ursa-viz/ui/ugr.js";
import { drawingOf, MAX_CATEGORIES } from "../public/graph-data.js";

const fixtures = join(import.meta.dir, "../../ursa-engine/tests/fixtures");
// A buffer of its own, as the engine worker's transfer gives.
const ugr = (name: string): ArrayBuffer => {
  const file = readFileSync(join(fixtures, `${name}.ugr`));
  return file.buffer.slice(file.byteOffset, file.byteOffset + file.byteLength) as ArrayBuffer;
};
const json = (name: string) => JSON.parse(readFileSync(join(fixtures, `${name}.json`), "utf8"));

// A drawing in the fixture's JSON shape.
function shape(buffer: ArrayBuffer, skipped: number) {
  const g: any = drawingOf(readResult(buffer), skipped);
  const out: any = {
    skipped: g.skipped,
    n: g.n,
    src: Array.from(g.src),
    dst: Array.from(g.dst),
    ids: Array.from({ length: g.ids.length }, (_, i) => g.ids.get(i)),
    graph: g.name,
    provenance: g.provenance,
    drawnAs: g.drawnAs,
  };
  if (g.layout) out.layout = { kind: g.layout.kind, level: Array.from(g.layout.level) };
  if (g.size) {
    out.sizeBy = g.sizeBy;
    out.size = Array.from(new Uint32Array(g.size.buffer, g.size.byteOffset, g.size.length));
  }
  if (g.color) {
    out.colorBy = g.colorBy;
    out.color = Array.from(g.color);
    out.colorLabels = g.colorLabels;
    out.colorCounts = g.colorCounts;
    out.colorOther = g.colorOther;
    out.colorMissing = g.colorMissing;
  }
  return out;
}

for (const name of ["graph_view_karate", "graph_view_rows", "graph_view_hop", "graph_view_paths"]) {
  test(`${name}: the page reads what the engine drew`, () => {
    const expected = json(name);
    expect(shape(ugr(name), expected.skipped)).toEqual(expected);
  });
}

test("karate: sized by degree, coloured by club, with no copy of the arrays", () => {
  const buffer = ugr("graph_view_karate");
  const g: any = drawingOf(readResult(buffer));
  expect([g.n, g.src.length, g.name, g.provenance.rule]).toEqual([34, 78, "karate", "graph"]);
  expect([g.sizeBy, g.colorBy]).toEqual(["degree", "club"]);
  expect(g.colorLabels.slice().sort()).toEqual(["Mr. Hi", "Officer"]);
  expect(g.colorCounts.reduce((a: number, b: number) => a + b, 0)).toBe(34);
  expect([g.colorOther, g.colorMissing]).toEqual([0, 0]);
  // The ends and the sizes are views of the engine's buffer.
  expect(g.src.buffer).toBe(buffer);
  expect(g.dst.buffer).toBe(buffer);
  expect(g.size.buffer).toBe(buffer);
  // Node 0 has 16 ties; an id is read when asked.
  expect(g.size[0]).toBe(16);
  expect(g.ids.get(33)).toBe("33");
});

test("rows: one name sizes and colours, the ninth category is other, no value is other too", () => {
  const g: any = drawingOf(readResult(ugr("graph_view_rows")), 1);
  // The colour column is "Rank" (colour) on the wire; the legend says "Rank".
  expect([g.sizeBy, g.colorBy]).toEqual(['"Rank"', '"Rank"']);
  expect(g.colorLabels.length).toBe(MAX_CATEGORIES);
  expect(g.colorLabels[0]).toBe("1");
  expect([g.colorOther, g.colorMissing]).toEqual([4, 1]);
  const last = g.n - 1;
  expect(g.ids.get(last)).toBe("é");
  expect(g.color[last]).toBe(MAX_CATEGORIES);
  expect(Number.isNaN(g.size[last])).toBe(true);
  expect(g.provenance.sql.startsWith("SELECT 'n' ||")).toBe(true);
});

test("a column the encodings name must be there, and of a kind that draws", () => {
  const r: any = readResult(ugr("graph_view_karate"));
  expect(() => drawingOf({ ...r, encodings: { ...r.encodings, size_by: "nope" } })).toThrow("no node column 'nope'");
  expect(() => drawingOf({ ...r, encodings: { ...r.encodings, size_by: "club" } })).toThrow("sizes are numbers");
  expect(() => drawingOf({ ...r, encodings: { ...r.encodings, color_by: "degree" } })).toThrow("not categories");
});

test("a hop result: rings by distance, coloured by distance, and said so", () => {
  const buffer = ugr("graph_view_hop");
  const result = readResult(buffer);
  const g: any = drawingOf(result);
  expect(g.drawnAs).toBe("rings on karate (seed, id, distance)");
  expect(g.provenance.roles.map((r: any) => r.role)).toEqual(["seed", "node", "distance"]);
  // One ring per distance: the seed, its 16 neighbours, and 9 at two steps.
  expect(g.layout.kind).toBe("rings");
  expect(g.layout.level.buffer).toBe(buffer);
  const ring = (d: number) => g.layout.level.filter((v: number) => v === d).length;
  expect([ring(0), ring(1), ring(2)]).toEqual([1, 16, 9]);
  expect(Array.from(result.highlights.seeds)).toEqual([0]);
  // The colour column is "distance (colour)" on the wire; the legend says "distance".
  expect(g.colorBy).toBe("distance");
  expect(g.colorLabels).toEqual(["0", "1", "2"]);
  expect(g.colorCounts).toEqual([1, 16, 9]);
});

test("paths: the union of the rows, in layers by step", () => {
  const result = readResult(ugr("graph_view_paths"));
  const g: any = drawingOf(result);
  expect(g.drawnAs).toBe("2 paths (members)");
  expect([g.n, g.src.length]).toEqual([4, 4]);
  expect(g.layout.kind).toBe("layers");
  expect(Array.from(g.layout.level)).toEqual([0, 1, 2, 1]);
  expect(result.highlights.paths.map((p: any) => [Array.from(p.nodes), Array.from(p.edges)])).toEqual([
    [[0, 1, 2], [0, 1]],
    [[0, 3, 2], [2, 3]],
  ]);
});
