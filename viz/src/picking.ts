/**
 * Which node is under the pointer.
 *
 * A linear scan is fine at a thousand nodes and hopeless at a million, and hover
 * runs on every pointer move — so positions go into a uniform grid once, and a
 * query touches only the cells a node could plausibly be in.
 *
 * Pure arithmetic, like `camera.ts`, so the hit-testing can be tested without a
 * browser. "The wrong node highlights" is otherwise a bug you can only find by
 * waving a mouse around.
 */

import { screenToWorld, type Bounds, type Camera } from './camera';

export interface PickIndex {
  readonly cols: number;
  readonly rows: number;
  readonly cellSize: number;
  readonly minX: number;
  readonly minY: number;
  /** Start offset into `items` for each cell; length `cols * rows + 1`. */
  readonly starts: Uint32Array;
  /** Node indices, grouped by cell. */
  readonly items: Uint32Array;
}

/** Cap on grid dimensions, so a degenerate extent cannot allocate unboundedly. */
const MAX_SIDE = 1024;

/**
 * Bucket node positions into a uniform grid.
 *
 * Built with a counting sort — two passes and two flat arrays, no per-cell array
 * allocation — which is the same shape as the CSR build in the core, and for the
 * same reason.
 */
export function buildPickIndex(
  x: ArrayLike<number>,
  y: ArrayLike<number>,
  count: number,
  bounds: Bounds,
): PickIndex {
  const w = Math.max(bounds.maxX - bounds.minX, 1e-6);
  const h = Math.max(bounds.maxY - bounds.minY, 1e-6);
  // Aim at roughly one node per cell: a grid much finer than that wastes memory
  // on empty cells, and much coarser degrades toward the linear scan.
  const target = Math.max(1, Math.ceil(Math.sqrt(Math.max(count, 1))));
  const cellSize = Math.max(w, h) / Math.min(target, MAX_SIDE);
  const cols = Math.min(MAX_SIDE, Math.max(1, Math.ceil(w / cellSize)));
  const rows = Math.min(MAX_SIDE, Math.max(1, Math.ceil(h / cellSize)));

  const cellOf = (i: number): number => {
    const cx = Math.min(cols - 1, Math.max(0, Math.floor((x[i]! - bounds.minX) / cellSize)));
    const cy = Math.min(rows - 1, Math.max(0, Math.floor((y[i]! - bounds.minY) / cellSize)));
    return cy * cols + cx;
  };

  const starts = new Uint32Array(cols * rows + 1);
  for (let i = 0; i < count; i += 1) starts[cellOf(i) + 1]! += 1;
  for (let c = 0; c < cols * rows; c += 1) starts[c + 1]! += starts[c]!;

  const cursor = Uint32Array.from(starts.subarray(0, cols * rows));
  const items = new Uint32Array(count);
  for (let i = 0; i < count; i += 1) {
    const c = cellOf(i);
    items[cursor[c]!] = i;
    cursor[c]! += 1;
  }
  return { cols, rows, cellSize, minX: bounds.minX, minY: bounds.minY, starts, items };
}

/**
 * The node under `(sx, sy)` in screen pixels, or -1.
 *
 * `sizes` are screen-pixel radii, which is what makes this agree with what is on
 * screen at any zoom: a node's clickable area is exactly the disc you can see.
 *
 * Among overlapping candidates it returns the one whose centre is nearest the
 * pointer, breaking ties toward the higher index — the later a node is drawn, the
 * more it is on top, so that is the one a click should mean.
 */
export function pick(
  index: PickIndex,
  x: ArrayLike<number>,
  y: ArrayLike<number>,
  sizes: ArrayLike<number>,
  maxRadiusPx: number,
  cam: Camera,
  sx: number,
  sy: number,
): number {
  const [wx, wy] = screenToWorld(cam, sx, sy);
  // The search radius is a screen distance, so it shrinks in world terms as the
  // camera zooms in — which is what keeps the query cheap when zoomed out.
  const reachWorld = maxRadiusPx / cam.scale;
  const reachCells = Math.ceil(reachWorld / index.cellSize);

  // Clamped the same way the build clamps. The bounds the grid was built from can
  // go stale — a layout tick moves a node, the grid does not — and such a node was
  // bucketed into the edge cell. Without clamping here, a query near it computes a
  // cell index past the grid, every candidate cell is rejected, and the node
  // becomes silently unpickable.
  const cx = Math.min(index.cols - 1, Math.max(0, Math.floor((wx - index.minX) / index.cellSize)));
  const cy = Math.min(index.rows - 1, Math.max(0, Math.floor((wy - index.minY) / index.cellSize)));

  let best = -1;
  let bestDist = Infinity;
  for (let gy = cy - reachCells; gy <= cy + reachCells; gy += 1) {
    if (gy < 0 || gy >= index.rows) continue;
    for (let gx = cx - reachCells; gx <= cx + reachCells; gx += 1) {
      if (gx < 0 || gx >= index.cols) continue;
      const cell = gy * index.cols + gx;
      for (let k = index.starts[cell]!; k < index.starts[cell + 1]!; k += 1) {
        const i = index.items[k]!;
        // Compare in screen space: radii are screen pixels, so a world-space
        // comparison would make nodes harder to hit the further out you zoom.
        const dx = (x[i]! - wx) * cam.scale;
        const dy = (y[i]! - wy) * cam.scale;
        const d2 = dx * dx + dy * dy;
        const r = sizes[i]!;
        if (d2 > r * r) continue;
        if (d2 < bestDist || (d2 === bestDist && i > best)) {
          bestDist = d2;
          best = i;
        }
      }
    }
  }
  return best;
}
