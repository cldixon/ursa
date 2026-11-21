// A reader for UGR, the wire format of a graph result and of a delta
// (ursa-viz/UGR.md): a JSON header, then aligned little-endian arrays.
//
// It parses the header and nothing else. Every array it returns is a typed
// array over the buffer it was given, with no copy, so a million edges cost
// what the JSON costs. Typed arrays read in the host's byte order, which is
// little-endian in every browser and JavaScript runtime in use.

const TYPES = { u8: Uint8Array, u16: Uint16Array, u32: Uint32Array, u64: BigUint64Array, f32: Float32Array };
const utf8 = new TextDecoder("utf-8", { fatal: true });
const fail = (message) => {
  throw new Error(`UGR: ${message}`);
};

// Read a buffer (an ArrayBuffer or a Uint8Array) into
// `{ header, sections, strings }`: the header as parsed JSON, each section
// as a typed array by name, and `strings(name)` for a section pair of text.
export function readUgr(input) {
  let bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
  // A typed array must start on a multiple of its element's size. A buffer
  // of its own always does; a slice of a larger one is copied if it does not.
  if (bytes.byteOffset % 8) bytes = bytes.slice();
  if (bytes.length < 8) fail(`the buffer is truncated: ${bytes.length} bytes, and the magic and the header's length take 8`);
  const magic = String.fromCharCode(...bytes.subarray(0, 4));
  if (magic !== "UGR1") {
    fail(magic.startsWith("UGR") ? `version '${magic[3]}', and this reader reads version 1` : 'not a UGR buffer: it does not start with "UGR1"');
  }
  const headerLength = new DataView(bytes.buffer, bytes.byteOffset).getUint32(4, true);
  const start = (8 + headerLength + 7) & ~7;
  if (8 + headerLength > bytes.length) {
    fail(`the buffer is truncated: its header is ${headerLength} bytes, and ${bytes.length - 8} follow`);
  }
  const header = JSON.parse(utf8.decode(bytes.subarray(8, 8 + headerLength)));
  const follow = Math.max(bytes.length - start, 0);
  if (follow < header.body) fail(`the buffer is truncated: its body is ${header.body} bytes, and ${follow} follow the header`);
  if (follow > header.body) fail(`${follow - header.body} bytes follow the body`);

  const sections = {};
  for (const { name, type, offset, count } of header.sections) {
    const Type = TYPES[type] ?? fail(`the section '${name}' has the unknown type '${type}'`);
    if (offset % 8) fail(`the section '${name}' starts at ${offset}, not a multiple of 8`);
    if (offset + count * Type.BYTES_PER_ELEMENT > header.body) {
      fail(`the section '${name}' (${count} ${type} at ${offset}) runs past the body's ${header.body} bytes`);
    }
    sections[name] = new Type(bytes.buffer, bytes.byteOffset + start + offset, count);
  }
  const need = (name) => sections[name] ?? fail(`the section '${name}' is missing`);

  // Text is two sections: `name.offsets` (one more than the strings) and
  // `name.bytes` (their UTF-8, end to end). Decoded one string at a time.
  const strings = (name) => {
    const offsets = need(`${name}.offsets`);
    const text = need(`${name}.bytes`);
    return {
      length: offsets.length - 1,
      get: (i) => utf8.decode(text.subarray(offsets[i], offsets[i + 1])),
    };
  };
  return { header, sections, need, strings };
}

// Read a result into the model's shape. Labels and flags that the writer
// left out, because every one is 0, come back as arrays of zeros.
export function readResult(input) {
  const { header, sections, need, strings } = readUgr(input);
  if (header.kind !== "result") fail(`the buffer holds a ${header.kind}, not a result`);
  const columns = (side) =>
    header[side].columns.map((column, i) => ({
      ...column,
      // f32 values, u32 values, or a dictionary column's codes.
      values: need(`${side}.columns.${i}`),
      // A dictionary column's distinct values; `values` indexes them.
      dict: column.type === "dict" ? strings(`${side}.columns.${i}.values`) : null,
    }));
  const n = header.nodes.count;
  const m = header.edges.count;
  const pathNodes = need("highlights.paths.offsets");
  const pathEdges = need("highlights.paths.edge_offsets");
  const paths = [];
  for (let p = 0; p < header.highlights.paths; p++) {
    paths.push({
      nodes: need("highlights.paths.nodes").subarray(pathNodes[p], pathNodes[p + 1]),
      edges: need("highlights.paths.edges").subarray(pathEdges[p], pathEdges[p + 1]),
    });
  }
  return {
    graph: header.graph,
    encodings: header.encodings,
    provenance: header.provenance,
    nodes: {
      count: n,
      labelNames: header.nodes.labels, // [{ name, key: "int" | "text" }]
      keys: strings("nodes.keys"),
      labels: sections["nodes.labels"] ?? new Uint16Array(n),
      flags: sections["nodes.flags"] ?? new Uint8Array(n), // bit 0: removed
      columns: columns("nodes"),
      x: header.nodes.positions ? need("nodes.x") : null,
      y: header.nodes.positions ? need("nodes.y") : null,
    },
    edges: {
      count: m,
      labelNames: header.edges.labels,
      src: need("edges.src"),
      dst: need("edges.dst"),
      labels: sections["edges.labels"] ?? new Uint16Array(m),
      rows: sections["edges.rows"] ?? null, // BigUint64Array; 2^64 - 1: unknown
      flags: sections["edges.flags"] ?? new Uint8Array(m),
      columns: columns("edges"),
    },
    highlights: { paths, seeds: need("highlights.seeds"), distance: header.highlights.distance },
  };
}
