/**
 * The viewport: what part of the graph is on screen, and where.
 *
 * Kept deliberately free of both WebGL and the DOM. Everything a pan, a zoom or
 * a hit-test needs is arithmetic over a `Camera`, so all of it is testable
 * without a browser — which matters, because coordinate bugs are the ones that
 * look like "the renderer is broken" and are miserable to debug through a canvas.
 *
 * World space is the graph's own coordinates (layout output, y increasing
 * upward). Screen space is CSS pixels from the canvas's top-left, y increasing
 * downward — so the transform flips y, and that flip is the single place it
 * happens.
 */

/** A view of world space: which world point sits at the centre, and how zoomed. */
export interface Camera {
  /** World coordinate rendered at the viewport's centre. */
  readonly cx: number;
  readonly cy: number;
  /** Screen pixels per world unit. Larger is more zoomed in. */
  readonly scale: number;
  /** Viewport size in CSS pixels. */
  readonly width: number;
  readonly height: number;
}

export interface Bounds {
  readonly minX: number;
  readonly minY: number;
  readonly maxX: number;
  readonly maxY: number;
}

/** Zoom limits, in screen pixels per world unit. */
export const MIN_SCALE = 1e-4;
export const MAX_SCALE = 1e4;

const clampScale = (s: number) => Math.min(MAX_SCALE, Math.max(MIN_SCALE, s));

export function createCamera(width: number, height: number): Camera {
  return { cx: 0, cy: 0, scale: 1, width, height };
}

export function resize(cam: Camera, width: number, height: number): Camera {
  return { ...cam, width, height };
}

export function worldToScreen(cam: Camera, x: number, y: number): [number, number] {
  return [
    (x - cam.cx) * cam.scale + cam.width / 2,
    // Negated: world y increases upward, screen y downward.
    (cam.cy - y) * cam.scale + cam.height / 2,
  ];
}

export function screenToWorld(cam: Camera, sx: number, sy: number): [number, number] {
  return [
    (sx - cam.width / 2) / cam.scale + cam.cx,
    cam.cy - (sy - cam.height / 2) / cam.scale,
  ];
}

/** Pan by a screen-pixel delta — the drag gesture, in the units the event gives. */
export function panByScreen(cam: Camera, dxScreen: number, dyScreen: number): Camera {
  return {
    ...cam,
    cx: cam.cx - dxScreen / cam.scale,
    cy: cam.cy + dyScreen / cam.scale,
  };
}

/**
 * Zoom by `factor`, holding the world point under `(sx, sy)` fixed.
 *
 * The anchoring is what makes wheel-zoom feel like zooming rather than like the
 * graph sliding away: whatever is under the pointer stays under the pointer.
 * Implemented by reading the world point before the scale change and solving for
 * the centre that puts it back at the same screen position after.
 */
export function zoomAt(cam: Camera, factor: number, sx: number, sy: number): Camera {
  const scale = clampScale(cam.scale * factor);
  // A clamped zoom is a no-op rather than a silent pan: without this, scrolling
  // at the zoom limit would drift the view.
  if (scale === cam.scale) return cam;
  const [wx, wy] = screenToWorld(cam, sx, sy);
  return {
    ...cam,
    scale,
    cx: wx - (sx - cam.width / 2) / scale,
    cy: wy + (sy - cam.height / 2) / scale,
  };
}

/** The world-space rectangle currently visible. Useful for culling. */
export function visibleBounds(cam: Camera): Bounds {
  const halfW = cam.width / 2 / cam.scale;
  const halfH = cam.height / 2 / cam.scale;
  return {
    minX: cam.cx - halfW,
    maxX: cam.cx + halfW,
    minY: cam.cy - halfH,
    maxY: cam.cy + halfH,
  };
}

export function boundsOf(x: ArrayLike<number>, y: ArrayLike<number>, count: number): Bounds {
  if (count === 0) return { minX: -1, minY: -1, maxX: 1, maxY: 1 };
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (let i = 0; i < count; i += 1) {
    const xi = x[i]!;
    const yi = y[i]!;
    if (xi < minX) minX = xi;
    if (xi > maxX) maxX = xi;
    if (yi < minY) minY = yi;
    if (yi > maxY) maxY = yi;
  }
  return { minX, minY, maxX, maxY };
}

/**
 * Frame `bounds` in the viewport, with `padding` as a fraction of the smaller
 * axis (0.08 leaves a comfortable margin rather than pinning nodes to the edge).
 *
 * A degenerate extent — every node at one point, or a single node — would divide
 * by zero, so a zero-width or zero-height box is given a unit extent and framed
 * at scale 1 rather than infinity.
 */
export function fitBounds(cam: Camera, bounds: Bounds, padding = 0.08): Camera {
  const w = bounds.maxX - bounds.minX;
  const h = bounds.maxY - bounds.minY;
  const cx = (bounds.minX + bounds.maxX) / 2;
  const cy = (bounds.minY + bounds.maxY) / 2;
  if (w <= 0 && h <= 0) return { ...cam, cx, cy, scale: 1 };
  const usableW = cam.width * (1 - 2 * padding);
  const usableH = cam.height * (1 - 2 * padding);
  // An axis with no extent must not constrain the fit, or a horizontal line of
  // nodes would zoom to MAX_SCALE trying to "fill" its zero height.
  const sx = w > 0 ? usableW / w : Infinity;
  const sy = h > 0 ? usableH / h : Infinity;
  return { ...cam, cx, cy, scale: clampScale(Math.min(sx, sy)) };
}
