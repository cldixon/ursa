# `@ursa/viz`

The shared visual layer: everything that decides how an Ursa value *looks*, plus the renderer that
draws it.

This package is the **source**, not a mirror. The docs site imports from it; in time the notebook
widget (via anywidget) and the self-contained HTML export will too. Those last two have no Astro
anywhere in sight, which is why this is a top-level package rather than something nested inside
`site/` — putting it there would point the dependency backwards and quietly end the "one token
set" property the package exists to hold.

**The rule: nothing here may import from `site/`.**

## What's in it

| | |
|---|---|
| `src/tokens.css` | The design tokens. Two grounds — "plate" (paper/ink, for docs and static figures) and "sky" (inverted, for the live instrument) — where `[data-ground]` re-points the semantic tokens, so a component needs no knowledge of which ground it is on. Plus the temperature ramp, the flat greys for categorical channels, and the type stack. |
| `src/stretch.ts` | Stretch functions (`linear` / `log` / `asinh`) and ramp sampling. Centrality scores are power-law, so a linear radius gives one huge dot and a thousand invisible specks; `asinh` is linear near zero and logarithmic at the top, which keeps faint structure without blowing out the bright end. |
| `src/channels.ts` | Values → visual channels, and where the design system stops being a stylesheet and starts being a rule. |
| `src/camera.ts` | The viewport: pan, zoom, fit, and the world↔screen transforms. Pure arithmetic, no DOM. |
| `src/theme.ts` | Resolves the palette from the live CSS custom properties. |
| `src/renderer.ts` | The WebGL layer (regl): two draw commands, nothing else. |
| `src/instrument.ts` | The host-facing surface — mount a canvas, hand it a graph, pan and zoom it. |

## Using it

```ts
import { createInstrument } from '@ursa/viz';

const instrument = createInstrument(document.getElementById('graph')!);
instrument.setGraph({
  x, y,                                        // Float32Array world coordinates
  edges,                                       // Uint32Array of index pairs
  size:  { kind: 'continuous', values: degree, stretch: 'log' },
  color: { kind: 'continuous', values: pagerank },   // asinh by default
});
```

```css
@import '@ursa/viz/tokens.css';
```

There is no build step. The package ships TypeScript source and the consumer's bundler compiles it,
which is why `tsconfig.json` sets `moduleResolution: "bundler"` and `noEmit`.

## Rules the code enforces

Two of them, both from `tokens.css`, and both expressed as types rather than as advice:

- **The temperature ramp is for continuous measures only.** It reads as ordered because it *is*
  ordered.
- **Categorical channels get flat greys.** Colouring communities along the ramp would assert that
  community 4 sits between 3 and 5 — a claim about the data that nobody made.

`Channel` is a union of `continuous` / `categorical` / `constant`, so painting a categorical column
with the sequential scale is not expressible rather than merely discouraged. `mapSize` refuses to
vary radius by category for the same reason.

A third convention, inherited rather than invented: **the high end of a measure is the hot end of
the ramp**. `mapColor` samples at `1 - t`, matching how the site already colours its figures and
its legend. A renderer that disagreed with the legend beside it would be worse than either
convention.

## Known limits

**Categorical colour past four categories.** The token system ships four flat greys and this cycles
them, so a fifth community is drawn the same as the first. That is wrong, and wrong *visibly*, on
purpose: the fix is an owner decision about what a categorical palette on the sky ground should be,
not a palette invented here. `Instrument.categoryCount` reports how many distinct categories were
asked for, so a host can surface the collision rather than let it pass as a coincidence. Tracked in
`docs/VIZ_HANDOFF.md` §Open questions.

**The ramp still exists twice** — as `PAPER_RAMP` / `SKY_RAMP` in `stretch.ts`, and as the
`--paper-ramp-*` custom properties in `tokens.css`. At *runtime* this no longer bites: `theme.ts`
resolves the palette from the live custom properties and falls back to the constants only where no
stylesheet is reachable, so the two cannot disagree on screen. They can still disagree in source.
Change one, change the other.

## Testing

```bash
bun run --cwd viz test     # unit tests + the browser render test
bun run --cwd viz harness  # build test/harness.js, then open test/harness.html
```

The unit tests cover the camera arithmetic and the channel mapping. They would not notice a shader
that fails to compile, a buffer that never uploads, or a camera framing empty space — **every one
of those produces a clean, plausible, entirely blank canvas.** So `test/render.test.ts` bundles the
harness the way the wheel will, runs it in headless Chromium, and reads the pixels back.

That test has already earned its keep twice: it caught regl silently refusing the instancing
divisor (nothing drew at all), and a `gl.viewport` left at the canvas element's 300×150 default
while the drawing buffer was 640×720 (everything drew into the bottom-left corner at the wrong
aspect). Neither is visible to a type checker or a unit test.

It skips, loudly, when Playwright or a Chromium binary is absent, so the suite stays runnable
without a browser. `URSA_CHROMIUM` overrides the executable; `URSA_VIZ_SNAPSHOT=path.png` writes a
reference still, which is the fastest way to see what the renderer actually produced.
