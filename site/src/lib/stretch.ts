/**
 * Stretch functions.
 *
 * The problem astronomical imaging solved a century ago and graph visualization
 * still has: an enormous dynamic range mapped onto a small displayable one.
 * Centrality scores are power-law, so a linear radius gives you one huge dot and
 * a million invisible specks — exactly what a linear stretch does to a bright
 * galaxy core.
 *
 *   linear   r ∝ s
 *   log      r ∝ log(1 + s/s₀)
 *   asinh    r ∝ asinh(s/β) / asinh(1/β)      β ≈ 0.01     ← default
 *
 * `asinh` is linear near zero and logarithmic at the top, which preserves faint
 * structure without blowing out the bright end. Defaults by algorithm:
 * pagerank / betweenness → asinh · degree / component size → log · positions →
 * linear.
 */

export type StretchName = 'linear' | 'log' | 'asinh';

export const DEFAULT_BETA = 0.01;

/** Normalized asinh stretch. `s` in [0, 1] → [0, 1]. */
export function asinhStretch(s: number, beta: number = DEFAULT_BETA): number {
  return Math.asinh(s / beta) / Math.asinh(1 / beta);
}

/** Normalized log stretch. `s` in [0, 1] → [0, 1]. */
export function logStretch(s: number, s0: number = DEFAULT_BETA): number {
  return Math.log1p(s / s0) / Math.log1p(1 / s0);
}

export function stretch(s: number, kind: StretchName = 'asinh', beta = DEFAULT_BETA): number {
  const clamped = Math.min(1, Math.max(0, s));
  if (kind === 'linear') return clamped;
  if (kind === 'log') return logStretch(clamped, beta);
  return asinhStretch(clamped, beta);
}

/**
 * The temperature scale, hot → cool. Star colour follows temperature, so the
 * ramp is physics-derived and never needs justifying. Sample position 0 is the
 * hot end.
 *
 * This is a *sequential* scale — for continuous measures only. Categorical
 * channels (components, communities) must use flat greys, because the ramp
 * implies an order that categories do not have.
 */
export const PAPER_RAMP = ['#C4491B', '#B07728', '#6E6E72', '#4B6CB0', '#1F3E8C'] as const;
export const SKY_RAMP = ['#FF9F51', '#FFD2A1', '#F4F3FF', '#AABFFF', '#9BB0FF'] as const;

function hexToRgb(hex: string): [number, number, number] {
  const n = parseInt(hex.slice(1), 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function rgbToHex([r, g, b]: [number, number, number]): string {
  const h = (v: number) => Math.round(Math.min(255, Math.max(0, v))).toString(16).padStart(2, '0');
  return `#${h(r)}${h(g)}${h(b)}`;
}

/** Sample a ramp at `t` in [0, 1], 0 = hot. Linear interpolation between stops. */
export function sampleRamp(t: number, ramp: readonly string[] = PAPER_RAMP): string {
  const clamped = Math.min(1, Math.max(0, t));
  const scaled = clamped * (ramp.length - 1);
  const i = Math.min(ramp.length - 2, Math.floor(scaled));
  const f = scaled - i;
  const a = hexToRgb(ramp[i]!);
  const b = hexToRgb(ramp[i + 1]!);
  return rgbToHex([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, a[2] + (b[2] - a[2]) * f]);
}
