// Test graphs, generated the same way every time.

/** xorshift64, as ursa-core's examples use: deterministic for a seed. */
function rng(seed) {
  let s = BigInt.asUintN(64, BigInt(seed));
  return () => {
    s ^= BigInt.asUintN(64, s << 13n);
    s ^= s >> 7n;
    s ^= BigInt.asUintN(64, s << 17n);
    return Number(s >> 11n) / 2 ** 53;
  };
}

/** An R-MAT power-law graph (Graph500 parameters): 2^scale nodes, `edges` edges. */
export function rmat(scale, edges, seed = 42) {
  const next = rng(seed);
  const src = new Uint32Array(edges);
  const dst = new Uint32Array(edges);
  for (let e = 0; e < edges; e++) {
    let u = 0, v = 0;
    for (let bit = scale - 1; bit >= 0; bit--) {
      const r = next();
      if (r < 0.57) {
      } else if (r < 0.76) v |= 1 << bit;
      else if (r < 0.95) u |= 1 << bit;
      else { u |= 1 << bit; v |= 1 << bit; }
    }
    src[e] = u >>> 0;
    dst[e] = v >>> 0;
  }
  return { n: 2 ** scale, src, dst };
}

/** A grid of `w` x `h` nodes: its layout should come out as a flat sheet. */
export function grid(w, h) {
  const src = [], dst = [];
  for (let y = 0; y < h; y++)
    for (let x = 0; x < w; x++) {
      const i = y * w + x;
      if (x + 1 < w) { src.push(i); dst.push(i + 1); }
      if (y + 1 < h) { src.push(i); dst.push(i + w); }
    }
  return { n: w * h, src: Uint32Array.from(src), dst: Uint32Array.from(dst) };
}

/** Clusters of `size` nodes, dense inside, a few edges between: the layout should separate them. */
export function clusters(k, size, inside = 6, between = 1, seed = 7) {
  const next = rng(seed);
  const src = [], dst = [];
  const n = k * size;
  for (let c = 0; c < k; c++)
    for (let i = 0; i < size; i++) {
      const a = c * size + i;
      for (let j = 0; j < inside; j++) { src.push(a); dst.push(c * size + Math.floor(next() * size)); }
      for (let j = 0; j < between; j++) if (next() < 0.1) { src.push(a); dst.push(Math.floor(next() * n)); }
    }
  return { n, src: Uint32Array.from(src), dst: Uint32Array.from(dst), community: Uint32Array.from({ length: n }, (_, i) => Math.floor(i / size)) };
}

/** Zachary's karate club: 34 nodes, 78 edges. */
export function karate() {
  const pairs = "0-1 0-2 0-3 0-4 0-5 0-6 0-7 0-8 0-10 0-11 0-12 0-13 0-17 0-19 0-21 0-31 1-2 1-3 1-7 1-13 1-17 1-19 1-21 1-30 2-3 2-7 2-8 2-9 2-13 2-27 2-28 2-32 3-7 3-12 3-13 4-6 4-10 5-6 5-10 5-16 6-16 8-30 8-32 8-33 9-33 13-33 14-32 14-33 15-32 15-33 18-32 18-33 19-33 20-32 20-33 22-32 22-33 23-25 23-27 23-29 23-32 23-33 24-25 24-27 24-31 25-31 26-29 26-33 27-33 28-31 28-33 29-32 29-33 30-32 30-33 31-32 31-33 32-33".split(" ").map((e) => e.split("-").map(Number));
  return { n: 34, src: Uint32Array.from(pairs, (p) => p[0]), dst: Uint32Array.from(pairs, (p) => p[1]) };
}

const NONE = 0xffffffff;

/** Each node's neighbours, both ways. */
function adjacency(g) {
  const adj = Array.from({ length: g.n }, () => []);
  for (let i = 0; i < g.src.length; i++) {
    adj[g.src[i]].push(g.dst[i]);
    adj[g.dst[i]].push(g.src[i]);
  }
  return adj;
}

/** Each node's distance from the nearest of `seeds`, NONE past `max`. */
function distances(g, adj, seeds, max = Infinity) {
  const d = new Uint32Array(g.n).fill(NONE);
  let frontier = [...seeds];
  for (const s of seeds) d[s] = 0;
  for (let step = 1; step <= max && frontier.length; step++) {
    const next = [];
    for (const v of frontier) for (const u of adj[v]) if (d[u] === NONE) { d[u] = step; next.push(u); }
    frontier = next;
  }
  return d;
}

/** The nodes `keep[v]`, renumbered, with the edges `edge(a, b)` keeps and `level` for each. */
function subgraph(g, keep, level, edge) {
  const ids = [], dense = new Uint32Array(g.n).fill(NONE);
  for (let v = 0; v < g.n; v++) if (keep(v)) { dense[v] = ids.length; ids.push(v); }
  const src = [], dst = [];
  for (let i = 0; i < g.src.length; i++) {
    const a = g.src[i], b = g.dst[i];
    if (dense[a] !== NONE && dense[b] !== NONE && edge(a, b)) { src.push(dense[a]); dst.push(dense[b]); }
  }
  return { n: ids.length, src: Uint32Array.from(src), dst: Uint32Array.from(dst), ids: Uint32Array.from(ids), level: Uint32Array.from(ids, (v) => level[v]) };
}

/**
 * What `hop(g, seeds, n => hops)` draws: the nodes within `hops` of a seed,
 * the graph's edges among them, and each one's distance as its `level`.
 */
export function neighbourhood(g, seeds, hops) {
  const d = distances(g, adjacency(g), seeds, hops);
  return subgraph(g, (v) => d[v] !== NONE, d, () => true);
}

/**
 * What `ALL SHORTEST` from `from` to `to` draws: the nodes and edges on a
 * shortest path, and each node's position along it as its `level`.
 */
export function shortestPaths(g, from, to) {
  const adj = adjacency(g);
  const a = distances(g, adj, [from]), b = distances(g, adj, [to]);
  const on = (v) => a[v] !== NONE && b[v] !== NONE && a[v] + b[v] === a[to];
  return subgraph(g, on, a, (u, v) => Math.abs(a[u] - a[v]) === 1);
}

/**
 * The graph the page's parameters name: `graph=rmat|grid|clusters|karate`.
 * With `layout=rings&seeds=0,5&hops=2` it is that graph's neighbourhood, and
 * with `layout=layers&from=16&to=26` its shortest paths, each with a `level`
 * per node for the fixed layout and `ids`, each node's index in the graph.
 */
export function fromParams(params) {
  const g = whole(params);
  const layout = params.get("layout");
  const numbers = (name, or) => (params.get(name) ?? or).split(",").map(Number);
  if (layout === "rings") return { ...neighbourhood(g, numbers("seeds", "0"), Number(params.get("hops") ?? 2)), layout };
  if (layout === "layers" && params.has("seeds")) return { ...neighbourhood(g, numbers("seeds", "0"), Number(params.get("hops") ?? 2)), layout };
  if (layout === "layers") return { ...shortestPaths(g, Number(params.get("from") ?? 0), Number(params.get("to") ?? g.n - 1)), layout };
  return g;
}

function whole(params) {
  const kind = params.get("graph") ?? "rmat";
  if (kind === "karate") return karate();
  if (kind === "grid") return grid(Number(params.get("w") ?? 100), Number(params.get("h") ?? 100));
  if (kind === "clusters") return clusters(Number(params.get("k") ?? 8), Number(params.get("size") ?? 500));
  return rmat(Number(params.get("scale") ?? 14), Number(params.get("edges") ?? 100000));
}
