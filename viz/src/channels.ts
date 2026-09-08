/**
 * Data values → visual channels.
 *
 * This is where the design system stops being a stylesheet and starts being a
 * rule the renderer enforces. Two rules, both from `tokens.css`:
 *
 * 1. **The temperature ramp is for continuous measures only.** It reads as
 *    ordered because it is ordered.
 * 2. **Categorical channels get flat greys.** Colouring communities along the
 *    ramp would assert that community 4 sits between 3 and 5, which is a claim
 *    about the data that nobody made.
 *
 * Encoding those as separate types means a caller cannot quietly paint a
 * categorical column with the sequential scale — the wrong thing is not
 * expressible rather than merely discouraged.
 */

import { sampleRamp, stretch, type StretchName } from './stretch';

/** A continuous measure — pagerank, betweenness, a weight. */
export interface ContinuousChannel {
  readonly kind: 'continuous';
  readonly values: ArrayLike<number>;
  /**
   * Dynamic-range compression before mapping. Defaults follow the algorithm that
   * produced the values: `asinh` for power-law scores (pagerank, betweenness),
   * `log` for counts (degree, component size), `linear` for anything already
   * uniform.
   */
  readonly stretch?: StretchName;
  /** Value range to normalize against. Defaults to the data's own min/max. */
  readonly domain?: readonly [number, number];
}

/** A categorical label — community, component, type. Dense codes, any integers. */
export interface CategoricalChannel {
  readonly kind: 'categorical';
  readonly codes: ArrayLike<number>;
}

/** One value for every node. */
export interface ConstantChannel {
  readonly kind: 'constant';
  readonly value: number;
}

export type Channel = ContinuousChannel | CategoricalChannel | ConstantChannel;

/** Normalize to [0, 1] against `domain`, then apply the stretch. */
export function normalize(ch: ContinuousChannel, count: number): Float32Array {
  const out = new Float32Array(count);
  let lo: number;
  let hi: number;
  if (ch.domain) {
    [lo, hi] = ch.domain;
  } else {
    lo = Infinity;
    hi = -Infinity;
    for (let i = 0; i < count; i += 1) {
      const v = ch.values[i]!;
      if (v < lo) lo = v;
      if (v > hi) hi = v;
    }
  }
  const span = hi - lo;
  const kind = ch.stretch ?? 'asinh';
  for (let i = 0; i < count; i += 1) {
    // A constant column has no range to spread over. Mapping it to 0 would make
    // every node vanish at the small end of whatever it drives; mid-scale is the
    // honest reading of "these are all the same".
    const t = span > 0 ? (ch.values[i]! - lo) / span : 0.5;
    out[i] = stretch(t, kind);
  }
  return out;
}

/**
 * Per-node radius in world units, from `min` at the bottom of the range to `max`
 * at the top.
 *
 * The stretch is doing the perceptual work here, deliberately: this maps the
 * *stretched* value linearly to radius rather than to area. Area-proportional
 * sizing is the usual advice, but `asinh` already compresses a power-law range
 * hard, and compounding the two flattens the difference between a hub and a leaf
 * to almost nothing. If a plot reads wrong, reach for the stretch parameter
 * before reaching for an area correction.
 */
export function mapSize(ch: Channel, count: number, min: number, max: number): Float32Array {
  const out = new Float32Array(count);
  if (ch.kind === 'constant') {
    out.fill(ch.value);
    return out;
  }
  if (ch.kind === 'categorical') {
    // Sizing by category would encode order into a radius, the same error the
    // ramp rule forbids for colour. Mid-size for everything, and say so.
    out.fill((min + max) / 2);
    return out;
  }
  const t = normalize(ch, count);
  for (let i = 0; i < count; i += 1) out[i] = min + (max - min) * t[i]!;
  return out;
}

/** `#rrggbb` → three floats in 0..1, the form a shader wants. */
export function hexToRgbFloat(hex: string): [number, number, number] {
  const n = parseInt(hex.slice(1), 16);
  return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255];
}

export interface ColorOptions {
  /** The sequential ramp for continuous channels, hot end first. */
  readonly ramp: readonly string[];
  /** Flat greys for categorical channels, in order of use. */
  readonly categorical: readonly string[];
  /** Colour for a constant channel, and the fallback. */
  readonly flat: string;
}

/**
 * Per-node colour as packed RGB floats (3 per node, 0..1).
 *
 * **Known limit — categorical colour past the palette.** The token system ships
 * four flat greys, and this cycles them, so a fifth community is drawn the same
 * as the first. That is wrong, and it is wrong *visibly*, which is the intent:
 * the fix is an owner decision about what a categorical palette on the sky ground
 * should be, not a palette invented here. `distinctCategories` reports how many
 * distinct categories were asked for so a caller can surface it rather than let
 * the collision pass as a coincidence.
 */
export function mapColor(ch: Channel, count: number, opts: ColorOptions): Float32Array {
  const out = new Float32Array(count * 3);
  const put = (i: number, rgb: [number, number, number]) => {
    out[i * 3] = rgb[0];
    out[i * 3 + 1] = rgb[1];
    out[i * 3 + 2] = rgb[2];
  };

  if (ch.kind === 'constant') {
    const rgb = hexToRgbFloat(opts.flat);
    for (let i = 0; i < count; i += 1) put(i, rgb);
    return out;
  }

  if (ch.kind === 'categorical') {
    const palette = opts.categorical.map(hexToRgbFloat);
    const n = palette.length;
    for (let i = 0; i < count; i += 1) {
      // Non-negative modulo: component labels are arbitrary ids and may be any
      // integer, and a negative index would read past the palette as undefined.
      const code = Math.trunc(ch.codes[i]!);
      put(i, palette[((code % n) + n) % n]!);
    }
    return out;
  }

  const t = normalize(ch, count);
  for (let i = 0; i < count; i += 1) {
    // `1 - t`: sample position 0 is the ramp's hot end, and the high end of a
    // measure should read as hot. Both existing consumers in the site invert the
    // same way (`sampleRamp(1 - t, ...)`), and a renderer that disagreed with the
    // legend beside it would be worse than either convention.
    put(i, hexToRgbFloat(sampleRamp(1 - t[i]!, opts.ramp)));
  }
  return out;
}

/**
 * How many distinct categories a categorical channel carries, capped at `limit`
 * so a pathological column cannot make counting them expensive. Returns 0 for a
 * channel that is not categorical.
 */
export function distinctCategories(ch: Channel, count: number, limit = 256): number {
  if (ch.kind !== 'categorical') return 0;
  const seen = new Set<number>();
  for (let i = 0; i < count && seen.size < limit; i += 1) seen.add(Math.trunc(ch.codes[i]!));
  return seen.size;
}
