/**
 * Deep-field generation.
 *
 * A long exposure of an apparently empty patch of sky resolves into thousands of
 * sources. That is the pitch: an edge list looks like nothing and is dense with
 * structure. The figures on this site are generated rather than drawn, from a
 * fixed seed, through the same score → radius mapping the docs describe — so the
 * captions can state real numbers and the same field always looks like itself.
 *
 * Every value here is deterministic: same seed in, same field out, at build time
 * and forever.
 */

import { stretch, type StretchName } from './stretch';

/** mulberry32 — small, fast, and stable across runs and platforms. */
export function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Box–Muller, for scatter around a cluster centre. */
function gaussian(next: () => number): number {
  const u = Math.max(next(), 1e-9);
  return Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * next());
}

/* -------------------------------------------------------------------------- *
 * One mapping for score
 *
 * Radius, opacity and spike length are all the same stretched score. It holds
 * across the mark, the hero, the docs figures and (eventually) the visualizer —
 * which is why the asterism can be *found* in the field rather than placed on
 * it: the seven brightest sources are simply the seven highest scores.
 * -------------------------------------------------------------------------- */

export const PLATE_BETA = 0.35;
const R_MIN = 0.5;
const R_MAX = 6.2;

/** score ∈ [0, 1] → rendered radius. The single mapping. */
export function radiusFor(score: number, beta: number = PLATE_BETA): number {
  return R_MIN + (R_MAX - R_MIN) * stretch(score, 'asinh', beta);
}

/** score ∈ [0, 1] → rendered opacity, on the same stretch. */
export function opacityFor(score: number, beta: number = PLATE_BETA): number {
  return 0.18 + 0.58 * stretch(score, 'asinh', beta);
}

export interface Source {
  x: number;
  y: number;
  /** Score in [0, 1], before any stretch. */
  score: number;
  /** Rendered radius, after the stretch. */
  r: number;
  /** Rendered opacity, after the stretch. */
  o: number;
}

export interface Cluster {
  /** Centre, in fractions of width/height. */
  cx: number;
  cy: number;
  /** Spread, in fractions of the smaller dimension. */
  spread: number;
  /** Share of the population drawn into this cluster. */
  weight: number;
}

export interface FieldOptions {
  seed: number;
  count: number;
  width: number;
  height: number;
  clusters?: Cluster[];
  /** Fraction of sources scattered uniformly rather than into a cluster. */
  background?: number;
  /**
   * Ceiling on a field source's score. The brightest objects in the frame are
   * the catalogued ones (the asterism), which sit above this.
   */
  scoreMax?: number;
  /** Skew of the score draw. Higher → steeper, more faint sources. */
  skew?: number;
  stretchKind?: StretchName;
  beta?: number;
}

export function deepField(opts: FieldOptions): Source[] {
  const {
    seed,
    count,
    width,
    height,
    clusters = [],
    background = 0.24,
    scoreMax = 0.3,
    skew = 3.2,
    beta = PLATE_BETA,
  } = opts;

  const next = rng(seed);
  const minDim = Math.min(width, height);
  const totalWeight = clusters.reduce((s, c) => s + c.weight, 0) || 1;
  const sources: Source[] = [];

  for (let i = 0; i < count; i += 1) {
    let x: number;
    let y: number;

    if (clusters.length === 0 || next() < background) {
      x = next() * width;
      y = next() * height;
    } else {
      // Pick a cluster proportionally to its weight, then scatter around it.
      let pick = next() * totalWeight;
      let cluster = clusters[clusters.length - 1]!;
      for (const c of clusters) {
        pick -= c.weight;
        if (pick <= 0) {
          cluster = c;
          break;
        }
      }
      x = cluster.cx * width + gaussian(next) * cluster.spread * minDim;
      y = cluster.cy * height + gaussian(next) * cluster.spread * minDim;
    }

    if (x < 0 || x > width || y < 0 || y > height) continue;

    // A heavy-tailed draw: most sources faint, a few near the ceiling.
    const score = scoreMax * Math.pow(next(), skew);
    sources.push({
      x: round(x),
      y: round(y),
      score,
      r: round(radiusFor(score, beta)),
      o: round(opacityFor(score, beta)),
    });
  }

  return sources;
}

function round(v: number): number {
  return Math.round(v * 10) / 10;
}

/**
 * Diffraction spikes: the cross-shaped flare on bright stars, thrown by light
 * bending around the mirror's support struts. Only bright objects get them,
 * which is exactly the property that makes them useful here — they mark
 * high-centrality nodes and stay legible when zoomed out and the node radius has
 * collapsed to a pixel.
 *
 * Spike length scales with score, on the same stretch as radius.
 */
export function spikes(cx: number, cy: number, r: number, lengthScale = 7): string[] {
  const l = r * lengthScale;
  const w = r * 0.3;
  return [
    `${cx},${cy - w} ${cx + l},${cy} ${cx},${cy + w}`,
    `${cx},${cy + w} ${cx - l},${cy} ${cx},${cy - w}`,
    `${cx - w},${cy} ${cx},${cy + l} ${cx + w},${cy}`,
    `${cx + w},${cy} ${cx},${cy - l} ${cx - w},${cy}`,
  ];
}

export interface Hub extends Source {
  name: string;
}

/**
 * The seven brightest sources in the plate — which, read as an asterism, are
 * Ursa Major. Scores are above the field ceiling and run through exactly the
 * same mapping, so the mark is continuous with the data.
 */
const URSA_MAJOR_RAW = [
  { x: 1055, y: 205, score: 0.82, name: 'Dubhe' },
  { x: 888, y: 268, score: 0.55, name: 'Merak' },
  { x: 880, y: 405, score: 0.63, name: 'Phecda' },
  { x: 1046, y: 378, score: 0.65, name: 'Megrez' },
  { x: 760, y: 300, score: 1.0, name: 'Alioth' },
  { x: 610, y: 268, score: 0.72, name: 'Mizar' },
  { x: 470, y: 335, score: 0.93, name: 'Alkaid' },
];

export const URSA_MAJOR: Hub[] = URSA_MAJOR_RAW.map((h) => ({
  ...h,
  r: Math.round(radiusFor(h.score) * 10) / 10,
  o: 1,
}));

/** The asterism lines: the bowl, then the handle. Indices into URSA_MAJOR. */
export const URSA_MAJOR_LINES: [number, number][] = [
  [0, 3],
  [3, 2],
  [2, 1],
  [1, 0],
  [1, 4],
  [4, 5],
  [5, 6],
];
