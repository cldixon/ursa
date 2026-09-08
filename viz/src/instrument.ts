/**
 * The instrument: a canvas you can pan and zoom, showing a graph.
 *
 * This is the host-facing surface. The docs site, the notebook widget and the
 * exported HTML all mount *this* — they differ in chrome and in where the data
 * comes from, not in what draws it.
 *
 * It stays dumb on purpose, per the design in `docs/VIZ_HANDOFF.md`: it holds
 * render buffers and a viewport, and forwards intent. It does not decide what to
 * fetch, what a value means, or which nodes matter. Those belong to the engine.
 */

import {
  boundsOf,
  createCamera,
  fitBounds,
  panByScreen,
  resize,
  zoomAt,
  type Camera,
} from './camera';
import { distinctCategories, mapColor, mapSize, type Channel } from './channels';
import {
  createRenderer,
  type RenderBuffers,
  type Renderer,
  type RendererOptions,
} from './renderer';
import { resolveTheme, type Theme } from './theme';

export interface GraphSpec {
  /** World x, one per node. */
  readonly x: Float32Array;
  /** World y, one per node. */
  readonly y: Float32Array;
  /** Node index pairs, 2 per edge — dense indices into `x`/`y`. */
  readonly edges?: Uint32Array;
  /** What drives node radius. Defaults to a constant. */
  readonly size?: Channel;
  /** What drives node colour. Defaults to a constant. */
  readonly color?: Channel;
}

export interface InstrumentOptions extends RendererOptions {
  /** Node radius range in screen pixels, smallest to largest. */
  readonly sizeRange?: readonly [number, number];
  /** Override the theme instead of resolving it from the mounted element. */
  readonly theme?: Theme;
}

export interface Instrument {
  /** Replace the graph. Re-frames the view only on the first non-empty one. */
  setGraph(spec: GraphSpec): void;
  /** Re-read the theme from the DOM — call after changing `[data-ground]`. */
  refreshTheme(): void;
  /** Frame the whole graph. */
  fit(): void;
  /** Re-read the container's size. Call on layout changes. */
  resize(): void;
  /**
   * Draw synchronously, outside the frame loop.
   *
   * Normal drawing is coalesced to an animation frame, which is right for
   * interaction and wrong for capture: a caller that wants to read the canvas
   * needs the pixels to be there *now*. Pairs with `preserveDrawingBuffer`.
   */
  drawNow(): void;
  readonly camera: Camera;
  /**
   * How many distinct categories the colour channel carries. Above the palette
   * size, colours repeat — see `mapColor`. Exposed so a host can warn rather
   * than let the collision read as a coincidence.
   */
  readonly categoryCount: number;
  destroy(): void;
}

const DEFAULT_SIZE_RANGE: readonly [number, number] = [3, 18];
const EMPTY: GraphSpec = { x: new Float32Array(0), y: new Float32Array(0) };

/**
 * Mount an instrument into `container`. The canvas fills it, and the theme is
 * resolved from it — so an enclosing `[data-ground="sky"]` is inherited.
 */
export function createInstrument(
  container: HTMLElement,
  options: InstrumentOptions = {},
): Instrument {
  const canvas = document.createElement('canvas');
  canvas.style.width = '100%';
  canvas.style.height = '100%';
  canvas.style.display = 'block';
  // The canvas is the input surface; without this a drag selects surrounding
  // text instead of panning, and touch scrolls the page instead of the view.
  canvas.style.touchAction = 'none';
  container.appendChild(canvas);

  const renderer: Renderer = createRenderer(canvas, {
    preserveDrawingBuffer: options.preserveDrawingBuffer,
  });
  const sizeRange = options.sizeRange ?? DEFAULT_SIZE_RANGE;

  let theme = options.theme ?? resolveTheme(container);
  let camera = createCamera(1, 1);
  let spec: GraphSpec = EMPTY;
  let buffers: RenderBuffers = emptyBuffers();
  let categoryCount = 0;
  let framed = false;
  let frame = 0;

  function emptyBuffers(): RenderBuffers {
    return {
      nodeCount: 0,
      positions: new Float32Array(0),
      sizes: new Float32Array(0),
      colors: new Float32Array(0),
      edges: new Uint32Array(0),
    };
  }

  function build(next: GraphSpec): RenderBuffers {
    const count = next.x.length;
    const positions = new Float32Array(count * 2);
    for (let i = 0; i < count; i += 1) {
      positions[i * 2] = next.x[i]!;
      positions[i * 2 + 1] = next.y[i]!;
    }
    const size: Channel = next.size ?? { kind: 'constant', value: sizeRange[0] * 2 };
    const color: Channel = next.color ?? { kind: 'constant', value: 0 };
    return {
      nodeCount: count,
      positions,
      sizes: mapSize(size, count, sizeRange[0], sizeRange[1]),
      colors: mapColor(color, count, theme),
      edges: next.edges ?? new Uint32Array(0),
    };
  }

  /**
   * Draw at most once per animation frame.
   *
   * A wheel gesture or a drag emits events far faster than the display refreshes,
   * and each one would otherwise force a full buffer upload and redraw. Coalescing
   * to the frame is the difference between smooth and stuttering on any graph big
   * enough to be worth looking at.
   */
  function requestDraw(): void {
    if (frame !== 0) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      renderer.draw(buffers, camera, theme);
    });
  }

  function syncSize(): void {
    const rect = container.getBoundingClientRect();
    // A hidden or not-yet-laid-out container measures 0, which would make the
    // projection divide by zero; 1px is a harmless stand-in until it has a size.
    const width = Math.max(1, Math.round(rect.width));
    const height = Math.max(1, Math.round(rect.height));
    const dpr = window.devicePixelRatio || 1;
    // The drawing buffer is in device pixels so the render is sharp on a HiDPI
    // display, while the camera stays in CSS pixels so pointer coordinates — which
    // arrive in CSS pixels — need no conversion.
    canvas.width = Math.round(width * dpr);
    canvas.height = Math.round(height * dpr);
    camera = resize(camera, width, height);
  }

  const observer = new ResizeObserver(() => {
    syncSize();
    requestDraw();
  });
  observer.observe(container);

  // --- input ---------------------------------------------------------------
  let dragging = false;
  let lastX = 0;
  let lastY = 0;

  function onPointerDown(e: PointerEvent): void {
    dragging = true;
    lastX = e.clientX;
    lastY = e.clientY;
    canvas.setPointerCapture(e.pointerId);
  }

  function onPointerMove(e: PointerEvent): void {
    if (!dragging) return;
    camera = panByScreen(camera, e.clientX - lastX, e.clientY - lastY);
    lastX = e.clientX;
    lastY = e.clientY;
    requestDraw();
  }

  function onPointerUp(e: PointerEvent): void {
    dragging = false;
    if (canvas.hasPointerCapture(e.pointerId)) canvas.releasePointerCapture(e.pointerId);
  }

  function onWheel(e: WheelEvent): void {
    // Without this the page scrolls behind the instrument as you zoom.
    e.preventDefault();
    const rect = canvas.getBoundingClientRect();
    // A trackpad reports small deltas continuously and a mouse wheel reports
    // large ones in steps; exponentiating the delta makes both land on the same
    // "one notch is one step" feel, and keeps zoom multiplicative so successive
    // zooms compose evenly.
    const factor = Math.exp(-e.deltaY * 0.002);
    camera = zoomAt(camera, factor, e.clientX - rect.left, e.clientY - rect.top);
    requestDraw();
  }

  canvas.addEventListener('pointerdown', onPointerDown);
  canvas.addEventListener('pointermove', onPointerMove);
  canvas.addEventListener('pointerup', onPointerUp);
  canvas.addEventListener('pointercancel', onPointerUp);
  canvas.addEventListener('wheel', onWheel, { passive: false });

  syncSize();

  const api: Instrument = {
    setGraph(next) {
      spec = next;
      buffers = build(next);
      categoryCount = distinctCategories(next.color ?? { kind: 'constant', value: 0 }, next.x.length);
      // Frame only the first graph that has anything in it. Re-framing on every
      // update would yank the view out from under someone who had navigated
      // somewhere deliberately.
      if (!framed && next.x.length > 0) {
        api.fit();
        framed = true;
      }
      requestDraw();
    },
    refreshTheme() {
      theme = options.theme ?? resolveTheme(container);
      // Colours are baked into the buffers, so a ground change has to rebuild
      // them, not just repaint.
      buffers = build(spec);
      requestDraw();
    },
    fit() {
      camera = fitBounds(camera, boundsOf(spec.x, spec.y, spec.x.length));
      requestDraw();
    },
    resize() {
      syncSize();
      requestDraw();
    },
    drawNow() {
      renderer.draw(buffers, camera, theme);
    },
    get camera() {
      return camera;
    },
    get categoryCount() {
      return categoryCount;
    },
    destroy() {
      if (frame !== 0) cancelAnimationFrame(frame);
      observer.disconnect();
      canvas.removeEventListener('pointerdown', onPointerDown);
      canvas.removeEventListener('pointermove', onPointerMove);
      canvas.removeEventListener('pointerup', onPointerUp);
      canvas.removeEventListener('pointercancel', onPointerUp);
      canvas.removeEventListener('wheel', onWheel);
      renderer.destroy();
      canvas.remove();
    },
  };

  return api;
}
