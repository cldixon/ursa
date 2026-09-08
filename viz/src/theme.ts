/**
 * The palette the renderer draws with, read from the live tokens.
 *
 * Deliberately resolved from CSS custom properties rather than from the
 * TypeScript constants: `tokens.css` re-points every semantic token under
 * `[data-ground="sky"]`, so reading the computed style means the instrument
 * inherits its ground from wherever it is mounted. Put the canvas on the sky and
 * it draws on the sky, with no prop to pass and no second switch to keep in sync.
 *
 * It also settles, at runtime, the duplication called out in the package README:
 * the ramp exists both as custom properties and as `PAPER_RAMP` / `SKY_RAMP` in
 * `stretch.ts`. **The CSS wins where it is available**; the constants are the
 * fallback for contexts with no stylesheet — a bare canvas, a unit test, a
 * server-side render. So the two copies cannot silently disagree on screen.
 */

import { hexToRgbFloat, type ColorOptions } from './channels';
import { PAPER_RAMP } from './stretch';

export interface Theme extends ColorOptions {
  /** Page background, and what the canvas clears to. */
  readonly background: string;
  /** Edge colour. Drawn at low alpha; density does the rest. */
  readonly edge: string;
  /** Foreground ink, for labels and the hover affordance. */
  readonly ink: string;
}

/**
 * The plate: ink on paper. Used when no stylesheet is reachable, and as the
 * per-token fallback for anything `tokens.css` does not define.
 */
export const FALLBACK_THEME: Theme = {
  background: '#f4f1ea',
  ink: '#12120f',
  edge: '#5a5852',
  ramp: PAPER_RAMP,
  categorical: ['#3d3c38', '#6b6963', '#8f8c84', '#b3afa5'],
  flat: '#5a5852',
};

/**
 * Read `--name` off `el`, falling back when the stylesheet has not loaded or the
 * token does not exist. `getComputedStyle` returns '' for an undefined custom
 * property, so an empty result is a miss rather than a colour.
 */
function token(style: CSSStyleDeclaration, name: string, fallback: string): string {
  const v = style.getPropertyValue(name).trim();
  return v === '' ? fallback : v;
}

/**
 * Resolve the theme in effect at `el` — the element the canvas lives in, so that
 * an enclosing `[data-ground="sky"]` is picked up.
 *
 * Falls back wholesale outside a browser (no `getComputedStyle`), and per-token
 * inside one, so a partially-loaded stylesheet degrades to the plate rather than
 * to transparent black.
 */
export function resolveTheme(el: Element | null): Theme {
  if (el == null || typeof getComputedStyle !== 'function') return FALLBACK_THEME;
  const s = getComputedStyle(el);
  const ramp = [0, 1, 2, 3, 4].map((i) =>
    token(s, `--paper-ramp-${i}`, FALLBACK_THEME.ramp[i] ?? '#6e6e72'),
  );
  const categorical = [0, 1, 2, 3].map((i) =>
    token(s, `--cat-${i}`, FALLBACK_THEME.categorical[i] ?? '#6b6963'),
  );
  const ink = token(s, '--ink', FALLBACK_THEME.ink);
  return {
    background: token(s, '--paper', FALLBACK_THEME.background),
    ink,
    // `--ink-2` is the muted ink; edges are structure, not subject, so they take
    // the muted weight rather than full ink.
    edge: token(s, '--ink-2', FALLBACK_THEME.edge),
    ramp,
    categorical,
    flat: token(s, '--ink-2', FALLBACK_THEME.flat),
  };
}

/** The clear colour as premultiplied RGBA floats, ready for `regl.clear`. */
export function backgroundRgba(theme: Theme): [number, number, number, number] {
  const [r, g, b] = hexToRgbFloat(normalizeHex(theme.background));
  return [r, g, b, 1];
}

/**
 * Coerce a computed colour to `#rrggbb`.
 *
 * `getComputedStyle` hands back whatever the stylesheet wrote for a custom
 * property — usually the literal `#f4f1ea`, but `rgb(244, 241, 234)` once a
 * value has passed through a browser's colour parsing. Both have to survive the
 * trip to a shader.
 */
export function normalizeHex(color: string): string {
  const c = color.trim();
  if (c.startsWith('#')) {
    if (c.length === 4) {
      // #abc -> #aabbcc
      return `#${c[1]}${c[1]}${c[2]}${c[2]}${c[3]}${c[3]}`;
    }
    return c.slice(0, 7);
  }
  // Signed components are accepted so the clamp below is actually reachable —
  // matching only unsigned digits would drop a negative straight to the fallback
  // and quietly make the clamp dead code.
  const m = c.match(/rgba?\(\s*(-?[\d.]+)[\s,]+(-?[\d.]+)[\s,]+(-?[\d.]+)/i);
  if (m) {
    const hex = (v: string) =>
      Math.round(Math.min(255, Math.max(0, parseFloat(v))))
        .toString(16)
        .padStart(2, '0');
    return `#${hex(m[1]!)}${hex(m[2]!)}${hex(m[3]!)}`;
  }
  return FALLBACK_THEME.background;
}
