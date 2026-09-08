# `@ursa/viz`

The shared visual layer: everything that decides how an Ursa value *looks*.

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
| `src/stretch.ts` | Stretch functions (`linear` / `log` / `asinh`) and ramp sampling — the size-and-brightness mapping. Centrality scores are power-law, so a linear radius gives one huge dot and a thousand invisible specks; `asinh` is linear near zero and logarithmic at the top, which keeps faint structure without blowing out the bright end. |
| `src/index.ts` | The public surface. `@ursa/viz/tokens.css` is a separate export so CSS can `@import` it. |

The renderer lands here next.

## Consuming it

Within the workspace, as an ordinary dependency (`"@ursa/viz": "workspace:*"`):

```ts
import { sampleRamp, stretch, SKY_RAMP } from '@ursa/viz';
```

```css
@import '@ursa/viz/tokens.css';
```

There is no build step. The package ships TypeScript source and the consumer's bundler compiles
it, which is why `tsconfig.json` sets `moduleResolution: "bundler"` and `noEmit`. `bun run check`
at the repository root type-checks it before the site does.

## Known duplication

The temperature ramp exists twice: as `PAPER_RAMP` / `SKY_RAMP` in `stretch.ts`, and as the
`--paper-ramp-*` / `--sky-ramp-*` custom properties in `tokens.css`. The same five hex values,
maintained in two places — a renderer needs them as JavaScript, CSS needs them as custom
properties, and neither can read the other's form directly.

Both are now in this package, so at least the copies sit side by side rather than in different
trees. Collapsing them to one source (generating the CSS from the TypeScript, or reading the
custom properties at runtime) is worth doing when the renderer arrives and there is a third
consumer to justify the machinery. Until then: **change one, change the other.**
