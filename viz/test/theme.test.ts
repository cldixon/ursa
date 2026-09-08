/**
 * Theme resolution, and the colour-format coercion it depends on.
 */

import { describe, expect, test } from 'bun:test';

import { FALLBACK_THEME, normalizeHex, resolveTheme } from '../src/theme';

describe('normalizeHex', () => {
  test('passes six-digit hex through', () => {
    expect(normalizeHex('#f4f1ea')).toBe('#f4f1ea');
    expect(normalizeHex('  #12120F  ')).toBe('#12120F');
  });

  test('expands shorthand hex', () => {
    expect(normalizeHex('#abc')).toBe('#aabbcc');
  });

  test('converts the rgb() form a browser hands back', () => {
    // `getComputedStyle` returns whatever the stylesheet wrote, but a value that
    // has passed through colour parsing comes back as rgb(). Both must reach the
    // shader as hex.
    expect(normalizeHex('rgb(244, 241, 234)')).toBe('#f4f1ea');
    expect(normalizeHex('rgba(18, 18, 15, 1)')).toBe('#12120f');
    expect(normalizeHex('rgb(0 0 0)')).toBe('#000000');
  });

  test('clamps out-of-range components rather than emitting bad hex', () => {
    expect(normalizeHex('rgb(300, -20, 12)')).toBe('#ff000c');
  });

  test('falls back on anything unrecognized', () => {
    expect(normalizeHex('rebeccapurple')).toBe(FALLBACK_THEME.background);
  });
});

describe('resolveTheme', () => {
  test('falls back wholesale with no element', () => {
    expect(resolveTheme(null)).toBe(FALLBACK_THEME);
  });

  test('the fallback obeys the design rules it stands in for', () => {
    // The ramp is sequential and the categorical greys are not part of it — if a
    // fallback quietly reused ramp colours for categories it would break the rule
    // exactly where nobody would look.
    for (const grey of FALLBACK_THEME.categorical) {
      expect(FALLBACK_THEME.ramp).not.toContain(grey);
    }
    expect(FALLBACK_THEME.ramp.length).toBe(5);
  });
});
