/**
 * `@ursa/viz` — the public surface of the shared visual layer.
 *
 * Everything that decides how an Ursa value *looks* lives here, so the docs
 * site, the notebook widget, and the exported HTML all read it from one place
 * rather than each keeping a copy. Today that is the stretch/ramp machinery and
 * the design tokens (`@ursa/viz/tokens.css`); the renderer lands here next.
 *
 * This package is the source, not a mirror — the site imports from it. Nothing
 * here may import from `site/`, or the dependency runs backwards and the "one
 * token set" property this package exists to hold quietly stops being true.
 */

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
