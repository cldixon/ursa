/**
 * A small graph, generated and measured at build time.
 *
 * The sky figure is a specimen of the rendering conventions, not a screenshot of
 * something that does not exist yet — so the numbers in its caption and its
 * detection overlay are computed here, from the graph actually drawn, rather
 * than invented. Preferential attachment gives the hub-and-spoke, power-law
 * shape that PageRank was designed for.
 */

import { rng } from './field';

export interface Node {
  i: number;
  x: number;
  y: number;
  degree: number;
  pagerank: number;
}

export interface Edge {
  s: number;
  d: number;
}

export interface GeneratedGraph {
  nodes: Node[];
  edges: Edge[];
  maxDegree: number;
  maxPagerank: number;
}

export interface GraphOptions {
  seed: number;
  n: number;
  /** New edges attached per arriving node. */
  m: number;
  width: number;
  height: number;
  margin?: number;
}

/**
 * Barabási–Albert preferential attachment: each arriving node attaches to `m`
 * existing nodes chosen proportionally to their degree. The result is the
 * hub-and-spoke power law of a real host graph.
 */
export function preferentialAttachment(opts: GraphOptions): GeneratedGraph {
  const { seed, n, m, width, height, margin = 40 } = opts;
  const next = rng(seed);

  const edges: Edge[] = [];
  const degree = new Array<number>(n).fill(0);
  /** Each endpoint appended once per incident edge — sampling it is degree-proportional. */
  const repeated: number[] = [];

  const seedSize = m + 1;
  for (let i = 0; i < seedSize; i += 1) {
    for (let j = i + 1; j < seedSize; j += 1) {
      edges.push({ s: i, d: j });
      degree[i]! += 1;
      degree[j]! += 1;
      repeated.push(i, j);
    }
  }

  for (let v = seedSize; v < n; v += 1) {
    const chosen = new Set<number>();
    let guard = 0;
    while (chosen.size < m && guard < 200) {
      guard += 1;
      const target = repeated[Math.floor(next() * repeated.length)]!;
      if (target !== v) chosen.add(target);
    }
    for (const target of chosen) {
      edges.push({ s: v, d: target });
      degree[v]! += 1;
      degree[target]! += 1;
      repeated.push(v, target);
    }
  }

  const pagerank = computePagerank(n, edges, 0.85, 40);
  const maxDegree = Math.max(...degree);
  const positions = forceLayout(n, edges, next, width, height, margin);

  const nodes: Node[] = positions.map((p, i) => ({
    i,
    x: round(p.x),
    y: round(p.y),
    degree: degree[i]!,
    pagerank: pagerank[i]!,
  }));

  return {
    nodes,
    edges,
    maxDegree,
    maxPagerank: Math.max(...pagerank),
  };
}

/**
 * Fruchterman–Reingold, deterministic. Attraction along edges, repulsion between
 * every pair, cooled linearly. Cosmic-web renderings look uncannily like
 * force-directed layouts precisely because both are the same physics — nodes,
 * filaments and voids — which is why this is worth doing properly rather than
 * scattering points and calling it a graph.
 */
function forceLayout(
  n: number,
  edges: Edge[],
  next: () => number,
  width: number,
  height: number,
  margin: number,
  iterations = 320,
): { x: number; y: number }[] {
  const w = width - margin * 2;
  const h = height - margin * 2;
  const area = w * h;
  const k = Math.sqrt(area / n);

  const pos = Array.from({ length: n }, () => ({
    x: (next() - 0.5) * w * 0.8,
    y: (next() - 0.5) * h * 0.8,
  }));
  const disp = Array.from({ length: n }, () => ({ x: 0, y: 0 }));

  let temperature = w * 0.12;
  const cooling = temperature / (iterations + 1);

  for (let it = 0; it < iterations; it += 1) {
    for (let v = 0; v < n; v += 1) {
      disp[v]!.x = 0;
      disp[v]!.y = 0;
    }

    // Repulsion, every pair.
    for (let v = 0; v < n; v += 1) {
      for (let u = v + 1; u < n; u += 1) {
        let dx = pos[v]!.x - pos[u]!.x;
        let dy = pos[v]!.y - pos[u]!.y;
        let d2 = dx * dx + dy * dy;
        if (d2 < 0.01) {
          dx = 0.1;
          dy = 0.1;
          d2 = 0.02;
        }
        const force = (k * k) / d2;
        disp[v]!.x += dx * force;
        disp[v]!.y += dy * force;
        disp[u]!.x -= dx * force;
        disp[u]!.y -= dy * force;
      }
    }

    // Attraction, along edges.
    for (const { s, d } of edges) {
      const dx = pos[s]!.x - pos[d]!.x;
      const dy = pos[s]!.y - pos[d]!.y;
      const dist = Math.max(Math.sqrt(dx * dx + dy * dy), 0.01);
      const force = dist / k;
      const fx = (dx / dist) * force;
      const fy = (dy / dist) * force;
      disp[s]!.x -= fx;
      disp[s]!.y -= fy;
      disp[d]!.x += fx;
      disp[d]!.y += fy;
    }

    for (let v = 0; v < n; v += 1) {
      const dx = disp[v]!.x;
      const dy = disp[v]!.y;
      const dist = Math.max(Math.sqrt(dx * dx + dy * dy), 0.01);
      const step = Math.min(dist, temperature);
      pos[v]!.x += (dx / dist) * step;
      pos[v]!.y += (dy / dist) * step;
    }
    temperature -= cooling;
  }

  // Fit the result to the frame rather than clamping it, so the aspect of the
  // layout survives and nothing piles up against an edge.
  const xs = pos.map((p) => p.x);
  const ys = pos.map((p) => p.y);
  const minX = Math.min(...xs);
  const maxX = Math.max(...xs);
  const minY = Math.min(...ys);
  const maxY = Math.max(...ys);
  // A force layout has no intrinsic aspect ratio, so fitting each axis
  // separately is legitimate — but only up to a bound, past which the distortion
  // would start misrepresenting distances.
  const MAX_ANISOTROPY = 1.6;
  const fitX = w / Math.max(maxX - minX, 1e-6);
  const fitY = h / Math.max(maxY - minY, 1e-6);
  const sx = Math.min(fitX, fitY * MAX_ANISOTROPY);
  const sy = Math.min(fitY, fitX * MAX_ANISOTROPY);
  const offX = margin + (w - (maxX - minX) * sx) / 2;
  const offY = margin + (h - (maxY - minY) * sy) / 2;

  return pos.map((p) => ({
    x: offX + (p.x - minX) * sx,
    y: offY + (p.y - minY) * sy,
  }));
}

/** Pull-based PageRank over the undirected view, with a dangling-node mass fix. */
function computePagerank(n: number, edges: Edge[], damping: number, iters: number): number[] {
  const inbound: number[][] = Array.from({ length: n }, () => []);
  const outDegree = new Array<number>(n).fill(0);
  for (const { s, d } of edges) {
    inbound[d]!.push(s);
    inbound[s]!.push(d);
    outDegree[s]! += 1;
    outDegree[d]! += 1;
  }

  let rank = new Array<number>(n).fill(1 / n);
  for (let it = 0; it < iters; it += 1) {
    const nextRank = new Array<number>(n).fill(0);
    let dangling = 0;
    for (let v = 0; v < n; v += 1) if (outDegree[v] === 0) dangling += rank[v]!;
    const base = (1 - damping) / n + (damping * dangling) / n;
    for (let v = 0; v < n; v += 1) {
      let acc = 0;
      for (const u of inbound[v]!) acc += rank[u]! / outDegree[u]!;
      nextRank[v] = base + damping * acc;
    }
    rank = nextRank;
  }
  return rank;
}

function round(v: number): number {
  return Math.round(v * 10) / 10;
}
