/**
 * The canonical figures, built once at module load.
 *
 * A figure cannot be captioned without real numbers, so the caption and the
 * drawing must read from the same object. Everything here is derived from a
 * fixed seed: the same graph always looks like itself.
 */

import { deepField, type Cluster } from './field';
import { preferentialAttachment } from './graph';

/* --- Fig. 1: the plate ---------------------------------------------------- */

export const PLATE = (() => {
  const seed = 1847;
  const width = 1200;
  const height = 720;
  const clusters: Cluster[] = [
    { cx: 0.16, cy: 0.24, spread: 0.075, weight: 1.0 },
    { cx: 0.28, cy: 0.35, spread: 0.055, weight: 0.7 },
    { cx: 0.46, cy: 0.33, spread: 0.06, weight: 0.8 },
    { cx: 0.55, cy: 0.28, spread: 0.05, weight: 0.6 },
    { cx: 0.7, cy: 0.29, spread: 0.06, weight: 0.9 },
    { cx: 0.86, cy: 0.7, spread: 0.075, weight: 1.0 },
    { cx: 0.69, cy: 0.78, spread: 0.07, weight: 0.9 },
    { cx: 0.5, cy: 0.8, spread: 0.06, weight: 0.7 },
    { cx: 0.25, cy: 0.68, spread: 0.07, weight: 0.9 },
    { cx: 0.24, cy: 0.55, spread: 0.06, weight: 0.8 },
    { cx: 0.45, cy: 0.58, spread: 0.06, weight: 0.6 },
  ];
  const sources = deepField({ seed, count: 660, width, height, clusters });
  return { seed, width, height, sources, beta: 0.35 };
})();

/* --- Fig. 2: the sky ------------------------------------------------------ */

export const SKY = (() => {
  const seed = 2311;
  const width = 760;
  const height = 420;
  const limit = 0.42;
  const graph = preferentialAttachment({ seed, n: 148, m: 2, width, height });
  return { seed, width, height, limit, graph };
})();

/* --- Tab. 1: the worked example ------------------------------------------
 *
 * The eight-node graph from `examples/quickstart.py`: two triangles (0-1-2 and
 * 3-4-5) joined by the bridge 2 → 3, plus 6 and 7 both pointing at hub 0. Small
 * enough to check by hand, which is the point — the table below is the actual
 * output of the code beside it, not a mock-up of one.
 */

export const EXAMPLE_EDGES: [number, number][] = [
  [0, 1],
  [1, 2],
  [2, 0],
  [3, 4],
  [4, 5],
  [5, 3],
  [2, 3],
  [6, 0],
  [7, 0],
];

/**
 * Directed PageRank, iterated to a 1e-9 residual. Every node in this graph has
 * at least one out-edge, so there is no dangling mass to redistribute and any
 * correct implementation converges to the same vector.
 */
function directedPagerank(edges: [number, number][], damping = 0.85): number[] {
  const n = Math.max(...edges.flat()) + 1;
  const inbound: number[][] = Array.from({ length: n }, () => []);
  const outDegree = new Array<number>(n).fill(0);
  for (const [s, d] of edges) {
    inbound[d]!.push(s);
    outDegree[s]! += 1;
  }

  let rank = new Array<number>(n).fill(1 / n);
  for (let it = 0; it < 200; it += 1) {
    const nextRank = new Array<number>(n).fill((1 - damping) / n);
    for (let v = 0; v < n; v += 1) {
      let acc = 0;
      for (const u of inbound[v]!) acc += rank[u]! / outDegree[u]!;
      nextRank[v]! += damping * acc;
    }
    let residual = 0;
    for (let v = 0; v < n; v += 1) residual += Math.abs(nextRank[v]! - rank[v]!);
    rank = nextRank;
    if (residual < 1e-12) break;
  }
  return rank;
}

function inDegrees(edges: [number, number][], n: number): number[] {
  const deg = new Array<number>(n).fill(0);
  for (const [, d] of edges) deg[d]! += 1;
  return deg;
}

export const EXAMPLE = (() => {
  const pagerank = directedPagerank(EXAMPLE_EDGES);
  const indeg = inDegrees(EXAMPLE_EDGES, pagerank.length);
  const rows = pagerank
    .map((score, id) => ({ id, pagerank: score, in_degree: indeg[id]! }))
    .sort((a, b) => b.pagerank - a.pagerank);
  const max = rows[0]!.pagerank;
  const min = rows[rows.length - 1]!.pagerank;
  return {
    rows,
    top: rows.slice(0, 5),
    max,
    min,
    n_nodes: pagerank.length,
    n_edges: EXAMPLE_EDGES.length,
  };
})();
