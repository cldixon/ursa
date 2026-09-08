/**
 * Value → visual mapping, including the design-system rules it enforces.
 */

import { describe, expect, test } from 'bun:test';

import {
  distinctCategories,
  hexToRgbFloat,
  mapColor,
  mapSize,
  normalize,
  type CategoricalChannel,
  type ColorOptions,
  type ContinuousChannel,
} from '../src/channels';
import { FALLBACK_THEME } from '../src/theme';

const opts: ColorOptions = FALLBACK_THEME;

const continuous = (values: number[], extra: Partial<ContinuousChannel> = {}): ContinuousChannel => ({
  kind: 'continuous',
  values: Float32Array.from(values),
  ...extra,
});

/**
 * Compare a node's colour against a hex value.
 *
 * `mapColor` writes into a `Float32Array` — that is the point, it feeds a vertex
 * buffer — so the stored value is the f32 rounding of the f64 the conversion
 * produced. Exact equality would be testing IEEE rounding rather than the
 * mapping.
 */
function expectColorAt(buf: Float32Array, node: number, hex: string): void {
  const want = hexToRgbFloat(hex);
  for (let k = 0; k < 3; k += 1) expect(buf[node * 3 + k]!).toBeCloseTo(want[k]!, 6);
}

describe('normalize', () => {
  test('spans the data range', () => {
    const t = normalize(continuous([0, 5, 10], { stretch: 'linear' }), 3);
    expect(Array.from(t)).toEqual([0, 0.5, 1]);
  });

  test('honours an explicit domain', () => {
    const t = normalize(continuous([5], { stretch: 'linear', domain: [0, 10] }), 1);
    expect(t[0]).toBeCloseTo(0.5, 6);
  });

  test('a constant column maps to mid-scale, not to nothing', () => {
    // Mapping a no-range column to 0 would make every node vanish at the small
    // end of whatever it drives.
    const t = normalize(continuous([7, 7, 7], { stretch: 'linear' }), 3);
    expect(Array.from(t)).toEqual([0.5, 0.5, 0.5]);
  });

  test('asinh is the default and lifts the faint end', () => {
    const values = [0, 0.01, 0.02, 1];
    const linear = normalize(continuous(values, { stretch: 'linear' }), 4);
    const asinh = normalize(continuous(values), 4);
    // The whole point of the stretch: small values become visible.
    expect(asinh[1]!).toBeGreaterThan(linear[1]!);
    expect(asinh[0]).toBeCloseTo(0, 6);
    expect(asinh[3]).toBeCloseTo(1, 6);
  });
});

describe('mapSize', () => {
  test('spans the requested pixel range, monotonically', () => {
    const s = mapSize(continuous([0, 1, 2, 3], { stretch: 'linear' }), 4, 4, 20);
    expect(s[0]).toBeCloseTo(4, 6);
    expect(s[3]).toBeCloseTo(20, 6);
    for (let i = 1; i < s.length; i += 1) expect(s[i]!).toBeGreaterThan(s[i - 1]!);
  });

  test('a constant channel is one size', () => {
    const s = mapSize({ kind: 'constant', value: 9 }, 3, 4, 20);
    expect(Array.from(s)).toEqual([9, 9, 9]);
  });

  test('a categorical channel does not vary size', () => {
    // Sizing by category would encode order into a radius — the same error the
    // ramp rule forbids for colour.
    const ch: CategoricalChannel = { kind: 'categorical', codes: Uint32Array.from([0, 1, 2]) };
    const s = mapSize(ch, 3, 4, 20);
    expect(Array.from(s)).toEqual([12, 12, 12]);
  });
});

describe('mapColor', () => {
  test('the high end of a measure is the hot end of the ramp', () => {
    // Ramp position 0 is hot, so a high value samples at 0 — matching how the
    // site already colours its figures and its legend.
    const c = mapColor(continuous([0, 1], { stretch: 'linear' }), 2, opts);
    expectColorAt(c, 0, opts.ramp[opts.ramp.length - 1]!);
    expectColorAt(c, 1, opts.ramp[0]!);
  });

  test('categorical codes take flat greys, never the ramp', () => {
    const ch: CategoricalChannel = { kind: 'categorical', codes: Uint32Array.from([0, 1]) };
    const c = mapColor(ch, 2, opts);
    expectColorAt(c, 0, opts.categorical[0]!);
    expectColorAt(c, 1, opts.categorical[1]!);
    // And emphatically not the ramp.
    expect(c[0]).not.toBeCloseTo(hexToRgbFloat(opts.ramp[0]!)[0], 4);
  });

  test('a negative category code stays inside the palette', () => {
    // Component labels are arbitrary ids; a naive modulo would index out of the
    // palette and produce undefined rather than a colour.
    const ch: CategoricalChannel = { kind: 'categorical', codes: Int32Array.from([-1]) };
    const c = mapColor(ch, 1, opts);
    expect(Number.isFinite(c[0])).toBe(true);
    expectColorAt(c, 0, opts.categorical[opts.categorical.length - 1]!);
  });

  test('categories past the palette collide, visibly', () => {
    // Documented limit, not an accident: the token system has four flat greys and
    // what to do beyond them is an owner decision, so the fifth repeats the first
    // rather than being quietly assigned an invented colour.
    const codes = Uint32Array.from([0, 1, 2, 3, 4]);
    const c = mapColor({ kind: 'categorical', codes }, 5, opts);
    expect(Array.from(c.slice(0, 3))).toEqual(Array.from(c.slice(12, 15)));
    expectColorAt(c, 4, opts.categorical[0]!);
    expect(distinctCategories({ kind: 'categorical', codes }, 5)).toBe(5);
  });
});

describe('distinctCategories', () => {
  test('is zero for a non-categorical channel', () => {
    expect(distinctCategories(continuous([1, 2, 3]), 3)).toBe(0);
    expect(distinctCategories({ kind: 'constant', value: 1 }, 3)).toBe(0);
  });

  test('stops counting at the limit', () => {
    const codes = Uint32Array.from({ length: 1000 }, (_, i) => i);
    expect(distinctCategories({ kind: 'categorical', codes }, 1000, 16)).toBe(16);
  });
});
