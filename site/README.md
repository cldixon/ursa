# The Ursa site

The landing page and documentation, built with [Astro](https://astro.build). Deployed to GitHub
Pages by `.github/workflows/docs.yml` on every push to `main` that touches this directory.

Nothing here needs the native extension — the site is static, built from markdown and Astro
components — so the docs build never waits on a Rust compile.

```bash
cd site
npm install
npm run dev      # http://localhost:4321/ursa
npm run build    # -> site/dist
npm run check    # astro check (type-checks .astro and the content schema)
```

## Layout

```
site/
├── src/
│   ├── content/docs/       # the documentation, as markdown/MDX
│   ├── components/         # figures, legends, nav, footer
│   ├── layouts/            # Base (chrome) and Docs (sidebar + TOC + prose)
│   ├── lib/                # figure generation, stretch functions, nav config
│   ├── pages/              # index.astro and the docs route
│   └── styles/             # tokens.css (the design system) + global.css
└── public/
```

## Adding a documentation page

1. Add a markdown or MDX file under `src/content/docs/`. Frontmatter requires `title` and
   `description`; `subtitle` is the optional mono line under the page title.
2. Add it to `DOCS_NAV` in `src/lib/site.ts` — that one list drives the sidebar, the footer, the
   docs index and the previous/next links.

Internal links are written root-relative (`/docs/concepts`) so they read correctly in the
repository; a rehype plugin adds the Pages base path at build time.

## The design system

The tokens live in `src/styles/tokens.css` and the rules are stated on the landing page. In short:

- **The ground is a negative.** Site and docs are paper; the visualizer is sky. `[data-ground="sky"]`
  re-points the semantic tokens at the inverted set.
- **Colour only where there is data.** No brand accent, no coloured buttons. The temperature ramp
  is the only colour, and it always means a measured value. Categorical channels use flat greys,
  because the ramp implies an order categories do not have.
- **Magnitude is centrality.** Radius, opacity and diffraction-spike length are one stretched
  score — `radiusFor()` in `src/lib/field.ts` is the single mapping.
- **One caption, no chrome.** Every figure carries one mono line stating what it is and how it was
  processed. Figures are generated from fixed seeds and measured at build time
  (`src/lib/figures.ts`), so the captions state real numbers rather than illustrative ones.
- **Two faces.** Source Serif 4 and IBM Plex Mono, self-hosted. No sans anywhere.
