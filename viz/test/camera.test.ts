/**
 * Camera arithmetic.
 *
 * Coordinate bugs present as "the renderer is broken" and are miserable to
 * diagnose through a canvas, so the transforms are pinned here where a failure
 * names the actual problem.
 */

import { describe, expect, test } from 'bun:test';

import {
  boundsOf,
  createCamera,
  fitBounds,
  MAX_SCALE,
  MIN_SCALE,
  panByScreen,
  screenToWorld,
  visibleBounds,
  worldToScreen,
  zoomAt,
} from '../src/camera';

const cam = () => ({ ...createCamera(800, 600), cx: 10, cy: -4, scale: 2 });

describe('world <-> screen', () => {
  test('the camera centre lands at the viewport centre', () => {
    const c = cam();
    expect(worldToScreen(c, c.cx, c.cy)).toEqual([400, 300]);
  });

  test('world y increases upward, screen y downward', () => {
    const c = cam();
    const [, above] = worldToScreen(c, c.cx, c.cy + 1);
    expect(above).toBeLessThan(300);
  });

  test('screenToWorld inverts worldToScreen', () => {
    const c = cam();
    for (const [x, y] of [
      [0, 0],
      [123.5, -87.25],
      [-1000, 1000],
    ] as const) {
      const [sx, sy] = worldToScreen(c, x, y);
      const [rx, ry] = screenToWorld(c, sx, sy);
      expect(rx).toBeCloseTo(x, 6);
      expect(ry).toBeCloseTo(y, 6);
    }
  });
});

describe('pan', () => {
  test('dragging right moves the view left over the graph', () => {
    // Content follows the cursor: drag right, and the world point under the
    // cursor stays under it, which means the camera centre moves left.
    const c = panByScreen(cam(), 100, 0);
    expect(c.cx).toBe(10 - 100 / 2);
  });

  test('a drag holds the world point under the cursor', () => {
    const c = cam();
    const [wx, wy] = screenToWorld(c, 200, 150);
    const moved = panByScreen(c, 40, -25);
    const [nx, ny] = screenToWorld(moved, 240, 125);
    expect(nx).toBeCloseTo(wx, 6);
    expect(ny).toBeCloseTo(wy, 6);
  });
});

describe('zoom', () => {
  test('holds the world point under the anchor fixed', () => {
    const c = cam();
    const [ax, ay] = [612, 91];
    const [wx, wy] = screenToWorld(c, ax, ay);
    const zoomed = zoomAt(c, 2.5, ax, ay);
    const [nx, ny] = screenToWorld(zoomed, ax, ay);
    expect(nx).toBeCloseTo(wx, 5);
    expect(ny).toBeCloseTo(wy, 5);
  });

  test('composes multiplicatively', () => {
    const c = cam();
    expect(zoomAt(c, 2, 400, 300).scale).toBeCloseTo(4, 10);
    expect(zoomAt(zoomAt(c, 2, 400, 300), 2, 400, 300).scale).toBeCloseTo(8, 10);
  });

  test('clamps, and a clamped zoom does not pan', () => {
    // Without the early return, a wheel event at the limit would change the
    // centre while leaving the scale alone — the view would drift while
    // apparently doing nothing.
    const atMax = { ...cam(), scale: MAX_SCALE };
    expect(zoomAt(atMax, 4, 100, 100)).toBe(atMax);
    const atMin = { ...cam(), scale: MIN_SCALE };
    expect(zoomAt(atMin, 0.25, 100, 100)).toBe(atMin);
  });
});

describe('bounds', () => {
  test('boundsOf spans the points', () => {
    const x = new Float32Array([0, 5, -3]);
    const y = new Float32Array([1, -2, 7]);
    expect(boundsOf(x, y, 3)).toEqual({ minX: -3, minY: -2, maxX: 5, maxY: 7 });
  });

  test('an empty graph gets a unit box rather than infinities', () => {
    expect(boundsOf(new Float32Array(0), new Float32Array(0), 0)).toEqual({
      minX: -1,
      minY: -1,
      maxX: 1,
      maxY: 1,
    });
  });

  test('fitBounds frames the content with margin to spare', () => {
    const c = createCamera(800, 600);
    const fitted = fitBounds(c, { minX: -100, maxX: 100, minY: -50, maxY: 50 });
    expect(fitted.cx).toBe(0);
    expect(fitted.cy).toBe(0);
    const vis = visibleBounds(fitted);
    expect(vis.minX).toBeLessThan(-100);
    expect(vis.maxX).toBeGreaterThan(100);
    expect(vis.minY).toBeLessThan(-50);
    expect(vis.maxY).toBeGreaterThan(50);
  });

  test('a single point does not zoom to infinity', () => {
    const fitted = fitBounds(createCamera(800, 600), { minX: 3, maxX: 3, minY: 3, maxY: 3 });
    expect(fitted.scale).toBe(1);
    expect(fitted.cx).toBe(3);
    expect(fitted.cy).toBe(3);
  });

  test('a horizontal line is framed by its width alone', () => {
    // Zero height must not constrain the fit — otherwise the empty axis "wins"
    // the min() and the view zooms to the clamp trying to fill it.
    const fitted = fitBounds(createCamera(800, 600), { minX: -100, maxX: 100, minY: 0, maxY: 0 });
    expect(fitted.scale).toBeLessThan(MAX_SCALE);
    expect(fitted.scale).toBeCloseTo((800 * 0.84) / 200, 6);
  });
});
