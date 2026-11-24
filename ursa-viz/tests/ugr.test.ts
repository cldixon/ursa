// The JavaScript reader of UGR (ui/ugr.js) against fixtures the Rust encoder
// wrote (tests/ugr_fixtures.rs). Each NAME.ugr comes with a NAME.json that
// Rust wrote from its own values, so this is Rust → JS, bit for bit:
// floats are compared as their bits, and 64-bit rows as decimal text.
//
//   bun test ursa-viz/tests/ugr.test.ts

import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { readResult, readUgr } from "../ui/ugr.js";

const fixtures = join(import.meta.dir, "fixtures");
// A buffer of its own, as a transfer or a fetch gives.
const ugr = (name: string): ArrayBuffer => {
  const file = readFileSync(join(fixtures, `${name}.ugr`));
  return file.buffer.slice(file.byteOffset, file.byteOffset + file.byteLength) as ArrayBuffer;
};
const json = (name: string) => JSON.parse(readFileSync(join(fixtures, `${name}.json`), "utf8"));

// A typed array as JSON can hold it: floats as their bits, 64-bit as text.
const plain = (array: ArrayLike<number | bigint> | null) => {
  if (array === null) return null;
  if (array instanceof Float32Array) return Array.from(new Uint32Array(array.buffer, array.byteOffset, array.length));
  if (array instanceof BigUint64Array) return Array.from(array, String);
  return Array.from(array as ArrayLike<number>);
};
const texts = (strings: { length: number; get(i: number): string } | null) =>
  strings === null ? null : Array.from({ length: strings.length }, (_, i) => strings.get(i));

// A result as read, in the fixture's JSON shape.
function shape(buffer: ArrayBuffer | Uint8Array) {
  const r = readResult(buffer);
  const columns = (list: any[]) =>
    list.map((c) => ({ name: c.name, type: c.type, role: c.role, values: plain(c.values), dict: texts(c.dict) }));
  return {
    graph: r.graph,
    encodings: r.encodings,
    provenance: r.provenance,
    nodes: {
      count: r.nodes.count,
      labelNames: r.nodes.labelNames,
      keys: texts(r.nodes.keys),
      labels: plain(r.nodes.labels),
      flags: plain(r.nodes.flags),
      columns: columns(r.nodes.columns),
      x: plain(r.nodes.x),
      y: plain(r.nodes.y),
    },
    edges: {
      count: r.edges.count,
      labelNames: r.edges.labelNames,
      src: plain(r.edges.src),
      dst: plain(r.edges.dst),
      labels: plain(r.edges.labels),
      rows: plain(r.edges.rows),
      flags: plain(r.edges.flags),
      columns: columns(r.edges.columns),
      adjacency: r.edges.adjacency && { offsets: plain(r.edges.adjacency.offsets), targets: plain(r.edges.adjacency.targets) },
    },
    highlights: {
      paths: r.highlights.paths.map((p: any) => ({ nodes: plain(p.nodes), edges: plain(p.edges) })),
      seeds: plain(r.highlights.seeds),
      distance: r.highlights.distance,
    },
  };
}

for (const name of ["empty", "plain", "town", "adjacent"]) {
  test(`reads the result '${name}' as Rust wrote it`, () => {
    expect(shape(ugr(name))).toEqual(json(name));
  });
}

test("returns views of the buffer, not copies", () => {
  const buffer = ugr("town");
  const r = readResult(buffer);
  for (const view of [r.edges.src, r.edges.dst, r.edges.rows, r.nodes.x, r.nodes.columns[0].values]) {
    expect(view.buffer).toBe(buffer);
  }
  // The typed reads: an integer key, text keys, a null code and a NaN.
  expect(r.nodes.labelNames[r.nodes.labels[2]]).toEqual({ name: "people", key: "int" });
  expect(BigInt(r.nodes.keys.get(2))).toBe(-3n);
  expect([r.nodes.keys.get(4), r.nodes.keys.get(5)]).toEqual(["Zürich", "東京"]);
  const club = r.nodes.columns[2];
  expect(club.dict.get(club.values[4])).toBe("blå");
  expect(club.values[2]).toBe(0xffffffff);
  expect(Number.isNaN(r.nodes.columns[0].values[2])).toBe(true);
  expect(r.edges.rows[2]).toBe(7n);
  expect(r.edges.rows[3]).toBe(2n ** 64n - 1n);
});

test("reads the adjacency a result carries, as views for a layout", () => {
  const buffer = ugr("adjacent");
  const { adjacency } = readResult(buffer).edges;
  expect(adjacency.offsets).toBeInstanceOf(BigUint64Array);
  expect(adjacency.targets).toBeInstanceOf(Uint32Array);
  expect(adjacency.offsets.buffer).toBe(buffer);
  // Node 1's neighbours.
  expect(Array.from(adjacency.targets.subarray(Number(adjacency.offsets[1]), Number(adjacency.offsets[2])))).toEqual([0, 2]);
  expect(readResult(ugr("plain")).edges.adjacency).toBeNull();
});

test("reads a buffer at any offset of a larger one", () => {
  const bytes = new Uint8Array(ugr("town"));
  for (const shift of [1, 4, 8, 13]) {
    const larger = new Uint8Array(bytes.length + shift + 3);
    larger.set(bytes, shift);
    expect(shape(larger.subarray(shift, shift + bytes.length))).toEqual(json("town"));
  }
});

test("reads the delta 'expand' as Rust wrote it", () => {
  const { header, sections } = readUgr(ugr("expand"));
  const expected = json("expand");
  expect(header.kind).toBe("delta");
  expect(header.provenance).toEqual(expected.provenance);
  expect(header.ops.map((op: any) => op.op)).toEqual(expected.ops);
  expect(Object.fromEntries(Object.entries(sections).map(([name, view]) => [name, plain(view as any)]))).toEqual(
    expected.sections,
  );
  expect(header.ops[0]).toEqual({ op: "add_node_label", name: "pets", key: "text" });
  expect(header.ops[4].new.color_by).toBe("hops");
  expect(() => readResult(ugr("expand"))).toThrow("UGR: the buffer holds a delta, not a result");
});

test("refuses a truncated or foreign buffer with a message", () => {
  const bytes = new Uint8Array(ugr("town"));
  expect(() => readUgr(bytes.slice(0, 5))).toThrow("UGR: the buffer is truncated: 5 bytes");
  expect(() => readUgr(bytes.slice(0, 100))).toThrow(/UGR: the buffer is truncated: its header is \d+ bytes, and 92 follow/);
  expect(() => readUgr(bytes.slice(0, bytes.length - 8))).toThrow(/UGR: the buffer is truncated: its body is \d+ bytes/);
  for (let cut = 0; cut < bytes.length; cut += 7) expect(() => readUgr(bytes.slice(0, cut))).toThrow();
  const longer = new Uint8Array(bytes.length + 8);
  longer.set(bytes);
  expect(() => readUgr(longer)).toThrow("UGR: 8 bytes follow the body");
  const png = bytes.slice();
  png.set([0x89, 0x50, 0x4e, 0x47]);
  expect(() => readUgr(png)).toThrow('UGR: not a UGR buffer: it does not start with "UGR1"');
  const next = bytes.slice();
  next[3] = "2".charCodeAt(0);
  expect(() => readUgr(next)).toThrow("UGR: version '2', and this reader reads version 1");
});
