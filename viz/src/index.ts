/**
 * `@ursa/viz` — the public surface of the shared visual layer.
 *
 * Everything that decides how an Ursa value *looks* lives here, so the docs
 * site, the notebook widget, and the exported HTML all read it from one place
 * rather than each keeping a copy. Today that is the stretch/ramp machinery, the
 * design tokens (`@ursa/viz/tokens.css`), and the renderer.
 *
 * This package is the source, not a mirror — the site imports from it. Nothing
 * here may import from `site/`, or the dependency runs backwards and the "one
 * token set" property this package exists to hold quietly stops being true.
 */

// The instrument: mount a canvas, hand it a graph.
export { createInstrument } from './instrument';
export type { GraphSpec, Instrument, InstrumentOptions } from './instrument';

// Value → visual mapping. Exported because a host drawing its own legend must
// map values the same way the canvas does, or the legend lies.
export {
  distinctCategories,
  hexToRgbFloat,
  mapColor,
  mapSize,
  normalize,
} from './channels';
export type {
  CategoricalChannel,
  Channel,
  ColorOptions,
  ConstantChannel,
  ContinuousChannel,
} from './channels';

// The viewport, as plain arithmetic — for hosts that need to place an overlay
// over a node, or drive the camera themselves.
export {
  boundsOf,
  createCamera,
  fitBounds,
  panByScreen,
  resize,
  screenToWorld,
  visibleBounds,
  worldToScreen,
  zoomAt,
  MAX_SCALE,
  MIN_SCALE,
} from './camera';
export type { Bounds, Camera } from './camera';

export { FALLBACK_THEME, normalizeHex, resolveTheme } from './theme';
export type { Theme } from './theme';

export {
  asinhStretch,
  DEFAULT_BETA,
  logStretch,
  PAPER_RAMP,
  sampleRamp,
  SKY_RAMP,
  stretch,
  type StretchName,
} from './stretch';
