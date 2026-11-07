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

export function fromParams(params) {
  const kind = params.get("graph") ?? "rmat";
  if (kind === "grid") return grid(Number(params.get("w") ?? 100), Number(params.get("h") ?? 100));
  if (kind === "clusters") return clusters(Number(params.get("k") ?? 8), Number(params.get("size") ?? 500));
  return rmat(Number(params.get("scale") ?? 14), Number(params.get("edges") ?? 100000));
}
