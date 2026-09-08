/**
 * The render harness, bundled and loaded by `render.test.ts` in headless
 * Chromium — and openable by hand during development.
 *
 * It mounts the instrument on both grounds so one screenshot shows the plate and
 * the sky together. A token that fails to resolve shows up as a panel that does
 * not match its neighbour, which no green unit test can tell you.
 */

import { createInstrument, type Instrument } from '../src/index';

interface Harness {
  plate: Instrument;
  sky: Instrument;
  nodeCount: number;
  edgeCount: number;
  /**
   * Fraction of sampled pixels that differ from the background.
   *
   * The load-bearing assertion of the whole browser test: a shader that fails to
   * compile, a buffer that never uploads, or a camera that frames empty space all
   * produce a clean, plausible, entirely blank canvas.
   */
  inkCoverage(which: 'plate' | 'sky'): number;
  /** Last hover the plate reported, for the browser test to read. */
  lastHover: { node: number; screenX: number; screenY: number } | null;
  /** Screen position of a node, so the test can aim the pointer at one. */
  screenOf(i: number): { x: number; y: number };
}

declare global {
  interface Window {
    __ursa?: Harness;
    __ursaReady?: boolean;
    __ursaError?: string;
  }
}

/**
 * Barabási–Albert preferential attachment on a circle, with a deterministic
 * jitter.
 *
 * Not a layout — the layout kernels are step 3 — but it gives the renderer a
 * hub-and-spoke graph with a genuine power-law degree distribution, which is
 * what makes the stretch visible. A uniform random graph would look fine even if
 * the stretch were broken.
 */
function graph(n: number, m: number, seed: number) {
  let s = seed >>> 0;
  const rand = () => {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    return s / 4294967296;
  };
  const src: number[] = [];
  const dst: number[] = [];
  const repeated: number[] = [];
  const degree = new Float32Array(n);
  const link = (a: number, b: number) => {
    src.push(a);
    dst.push(b);
    repeated.push(a, b);
    degree[a]! += 1;
    degree[b]! += 1;
  };
  for (let i = 0; i < m + 1; i += 1) for (let j = i + 1; j < m + 1; j += 1) link(i, j);
  for (let v = m + 1; v < n; v += 1) {
    const picked = new Set<number>();
    while (picked.size < m) picked.add(repeated[Math.floor(rand() * repeated.length)]!);
    for (const t of picked) link(v, t);
  }

  const x = new Float32Array(n);
  const y = new Float32Array(n);
  for (let i = 0; i < n; i += 1) {
    // High-degree nodes pulled toward the centre, so the picture takes the
    // hub-and-rim shape a force layout would give, without running one.
    const a = (i / n) * Math.PI * 2;
    const r = 100 * (1 - Math.min(0.85, degree[i]! / 40)) + rand() * 12;
    x[i] = Math.cos(a) * r;
    y[i] = Math.sin(a) * r;
  }
  const edges = new Uint32Array(src.length * 2);
  for (let e = 0; e < src.length; e += 1) {
    edges[e * 2] = src[e]!;
    edges[e * 2 + 1] = dst[e]!;
  }
  return { x, y, edges, degree, edgeCount: src.length };
}

function mount(id: string, onHover?: (e: { node: number; screenX: number; screenY: number }) => void) {
  const el = document.getElementById(id);
  if (el == null) throw new Error(`no mount point #${id}`);
  // Capture needs the buffer preserved; interactive hosts leave it off.
  return createInstrument(el, { preserveDrawingBuffer: true, onHover });
}

/** Count pixels that differ from the canvas's corner colour, on a coarse grid. */
function coverage(canvas: HTMLCanvasElement): number {
  const gl = canvas.getContext('webgl') ?? canvas.getContext('experimental-webgl');
  if (gl == null) return 0;
  const g = gl as WebGLRenderingContext;
  const w = canvas.width;
  const h = canvas.height;
  const px = new Uint8Array(w * h * 4);
  g.readPixels(0, 0, w, h, g.RGBA, g.UNSIGNED_BYTE, px);
  const bg = [px[0]!, px[1]!, px[2]!];
  let differing = 0;
  let sampled = 0;
  // Every 4th pixel in each direction: enough to catch "nothing drew" without
  // walking a few million pixels in a test.
  for (let yy = 0; yy < h; yy += 4) {
    for (let xx = 0; xx < w; xx += 4) {
      const i = (yy * w + xx) * 4;
      sampled += 1;
      const d =
        Math.abs(px[i]! - bg[0]!) + Math.abs(px[i + 1]! - bg[1]!) + Math.abs(px[i + 2]! - bg[2]!);
      if (d > 12) differing += 1;
    }
  }
  return sampled === 0 ? 0 : differing / sampled;
}

try {
  const g = graph(320, 2, 20260908);

  let lastHover: Harness['lastHover'] = null;
  const plate = mount('mount-plate', (e) => {
    lastHover = { node: e.node, screenX: e.screenX, screenY: e.screenY };
  });
  plate.setGraph({
    x: g.x,
    y: g.y,
    edges: g.edges,
    size: { kind: 'continuous', values: g.degree, stretch: 'log' },
    color: { kind: 'continuous', values: g.degree, stretch: 'asinh' },
  });

  const sky = mount('mount-sky');
  const community = new Uint32Array(g.x.length);
  for (let i = 0; i < community.length; i += 1) community[i] = i % 4;
  sky.setGraph({
    x: g.x,
    y: g.y,
    edges: g.edges,
    size: { kind: 'continuous', values: g.degree, stretch: 'log' },
    color: { kind: 'categorical', codes: community },
  });

  window.__ursa = {
    plate,
    sky,
    nodeCount: g.x.length,
    edgeCount: g.edgeCount,
    get lastHover() {
      return lastHover;
    },
    screenOf(i) {
      const cam = plate.camera;
      const el = document.getElementById('mount-plate')!;
      const rect = el.getBoundingClientRect();
      // Canvas-relative -> page coordinates, which is what the test's mouse uses.
      const sx = (g.x[i]! - cam.cx) * cam.scale + cam.width / 2;
      const sy = (cam.cy - g.y[i]!) * cam.scale + cam.height / 2;
      return { x: rect.left + sx, y: rect.top + sy };
    },
    inkCoverage(which) {
      const inst = which === 'plate' ? plate : sky;
      // Draw synchronously so the pixels are in the buffer when we read them.
      inst.drawNow();
      const el = document.getElementById(which === 'plate' ? 'mount-plate' : 'mount-sky');
      const canvas = el?.querySelector('canvas');
      return canvas == null ? 0 : coverage(canvas);
    },
  };
  window.__ursaReady = true;
} catch (err) {
  // Surface the failure to the test rather than leaving it waiting for a flag
  // that will never be set.
  window.__ursaError = err instanceof Error ? `${err.message}\n${err.stack}` : String(err);
  window.__ursaReady = true;
}
