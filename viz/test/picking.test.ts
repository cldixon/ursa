/**
 * Hit-testing.
 *
 * "The wrong node highlights" is otherwise a bug you can only find by waving a
 * mouse around, so the grid and the screen-space comparison are pinned here.
 */

import { describe, expect, test } from 'bun:test';

import { boundsOf, createCamera, fitBounds, worldToScreen, type Camera } from '../src/camera';
import { buildPickIndex, pick } from '../src/picking';

/** Four nodes at the corners of a square, plus one at the origin. */
const X = Float32Array.from([-10, 10, -10, 10, 0]);
const Y = Float32Array.from([-10, -10, 10, 10, 0]);
const SIZES = Float32Array.from([8, 8, 8, 8, 8]);
const N = 5;

function setup(cam?: Camera) {
  const bounds = boundsOf(X, Y, N);
  const camera = cam ?? fitBounds(createCamera(400, 400), bounds);
  return { index: buildPickIndex(X, Y, N, bounds), camera };
}

/** Pick at the screen position of node `i`. */
function pickAtNode(i: number, cam: Camera, index: ReturnType<typeof buildPickIndex>) {
  const [sx, sy] = worldToScreen(cam, X[i]!, Y[i]!);
  return pick(index, X, Y, SIZES, 8, cam, sx, sy);
}

describe('buildPickIndex', () => {
  test('places every node in exactly one cell', () => {
    const { index } = setup();
    expect(index.items.length).toBe(N);
    expect(Array.from(index.items).sort((a, b) => a - b)).toEqual([0, 1, 2, 3, 4]);
    // The offsets are a prefix sum, so the last one is the item count.
    expect(index.starts[index.starts.length - 1]).toBe(N);
  });

  test('handles an empty graph', () => {
    const index = buildPickIndex(new Float32Array(0), new Float32Array(0), 0, {
      minX: -1,
      minY: -1,
      maxX: 1,
      maxY: 1,
    });
    expect(index.items.length).toBe(0);
    expect(pick(index, new Float32Array(0), new Float32Array(0), new Float32Array(0), 8, createCamera(100, 100), 50, 50)).toBe(-1);
  });

  test('survives a degenerate extent', () => {
    // Every node at one point: the bounds have no width, and a naive divide would
    // produce NaN cell indices and silently index nothing.
    const x = Float32Array.from([3, 3, 3]);
    const y = Float32Array.from([7, 7, 7]);
    const index = buildPickIndex(x, y, 3, { minX: 3, maxX: 3, minY: 7, maxY: 7 });
    expect(index.items.length).toBe(3);
    const cam = { ...createCamera(200, 200), cx: 3, cy: 7, scale: 1 };
    const hit = pick(index, x, y, Float32Array.from([6, 6, 6]), 6, cam, 100, 100);
    expect(hit).toBeGreaterThanOrEqual(0);
  });
});

describe('pick', () => {
  test('finds each node under its own centre', () => {
    const { index, camera } = setup();
    for (let i = 0; i < N; i += 1) expect(pickAtNode(i, camera, index)).toBe(i);
  });

  test('misses empty space', () => {
    const { index, camera } = setup();
    // Midway between the origin node and a corner, well outside both radii.
    const [sx, sy] = worldToScreen(camera, -5.5, 0);
    expect(pick(index, X, Y, SIZES, 8, camera, sx, sy)).toBe(-1);
  });

  test('the hit area is the disc you can see, at any zoom', () => {
    // Radii are screen pixels held constant under zoom, so a point 6px from a
    // centre must hit — and one 9px away must miss — whether the camera is zoomed
    // in or out. A world-space comparison would make nodes progressively harder to
    // hit the further out you zoom.
    //
    // One isolated node, deliberately: with neighbours in range, zooming out drags
    // them within a few screen pixels and the nearest-centre rule correctly picks
    // one of them, which would test the fixture rather than the zoom behaviour.
    const x = Float32Array.from([0]);
    const y = Float32Array.from([0]);
    const sizes = Float32Array.from([8]);
    const index = buildPickIndex(x, y, 1, { minX: -10, maxX: 10, minY: -10, maxY: 10 });
    for (const scale of [0.5, 2, 20]) {
      const cam = { ...createCamera(400, 400), cx: 0, cy: 0, scale };
      const [sx, sy] = worldToScreen(cam, 0, 0);
      expect(pick(index, x, y, sizes, 8, cam, sx + 6, sy)).toBe(0);
      expect(pick(index, x, y, sizes, 8, cam, sx + 9, sy)).toBe(-1);
    }
  });

  test('overlapping nodes resolve to the nearest centre', () => {
    const x = Float32Array.from([0, 4]);
    const y = Float32Array.from([0, 0]);
    const sizes = Float32Array.from([20, 20]);
    const bounds = boundsOf(x, y, 2);
    const index = buildPickIndex(x, y, 2, bounds);
    const cam = { ...createCamera(400, 400), cx: 2, cy: 0, scale: 1 };
    const at = (wx: number) => {
      const [sx, sy] = worldToScreen(cam, wx, 0);
      return pick(index, x, y, sizes, 20, cam, sx, sy);
    };
    expect(at(-1)).toBe(0);
    expect(at(5)).toBe(1);
  });

  test('a node outside the grid bounds is still found', () => {
    // The index is built from bounds that may go stale — a layout tick moves a
    // node, the grid does not. Clamping keeps such a node reachable rather than
    // dropping it into a cell that does not exist.
    const x = Float32Array.from([0, 500]);
    const y = Float32Array.from([0, 0]);
    const sizes = Float32Array.from([10, 10]);
    const index = buildPickIndex(x, y, 2, { minX: -10, maxX: 10, minY: -10, maxY: 10 });
    const cam = { ...createCamera(400, 400), cx: 500, cy: 0, scale: 1 };
    const [sx, sy] = worldToScreen(cam, 500, 0);
    expect(pick(index, x, y, sizes, 10, cam, sx, sy)).toBe(1);
  });
});
