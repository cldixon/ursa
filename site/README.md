# The Ursa site

The landing page and documentation, built with [Astro](https://astro.build) and managed with
[bun](https://bun.com).

Nothing here needs the native extension — the site is static, built from markdown and Astro
components — so the docs build never waits on a Rust compile.

```bash
cd site
bun install
bun run dev              # http://localhost:4321
bun run build            # -> site/dist
bun run check            # astro check (type-checks .astro and the content schema)
bun run preview:worker   # the production build, served by a local workerd
```

## Deploying

The site deploys to a Cloudflare Worker — `ursa-docs`, live at
<https://ursa.cldixon.dev> — as static assets. Workers rather than Pages:
Cloudflare now points new projects at Workers, and Static Assets is the supported way to host a
build output there.

`wrangler.jsonc` configures an **assets-only** Worker: no `main`, no server-side code, `dist/`
served straight from the edge. `html_handling: "drop-trailing-slash"` matches the unslashed form
every internal link uses, so navigation costs no redirects; `not_found_handling: "404-page"`
serves the built 404 rather than falling through to `index.html` and returning 200 for a URL that
does not exist.

### CI/CD — Workers Builds

Deployment is Cloudflare's git integration (**Workers Builds**), configured on the Worker in the
dashboard rather than in this repository:

- Pushes to `main` that touch `site/` build and **deploy production**.
- Every other branch runs `wrangler versions upload` instead, which produces **preview URLs** —
  one per commit, plus a stable per-branch alias
  (`<branch>-ursa-docs.cl-dixon.workers.dev`) — posted to the pull request as a comment. The
  branch URL follows the branch as commits land, like a Pages preview deployment.

`.github/workflows/docs.yml` is a build check only (install, `astro check`, build); it proves a
docs PR builds from a clean checkout independent of the Cloudflare account.

### Pinning production to the release

Deploying production from `main` publishes documentation for unreleased code: a reader follows an
example for a feature that is not in the wheel yet. That is what happened between 0.2.0 and
0.3.0, and it is the root of most of the onboarding friction found in that period.

`.github/workflows/docs-pin.yml` moves a **`docs-release`** branch to the commit each release was
cut from. The branch is machine-owned — nobody commits to it, and it only ever moves to a released
tag.

**One manual step, once:** set the `ursa-docs` Worker's Workers Builds *production* branch to
`docs-release` in the Cloudflare dashboard. Until that is done the workflow maintains the branch
and changes nothing about what is served.

After the switch:

- `ursa.cldixon.dev` serves the documentation for the current release.
- `main` and every docs PR still get preview URLs, so unreleased documentation stays reviewable —
  it just is not what the custom domain serves.
- Re-pin to another release by running the workflow manually with a tag.

The version the site states is derived, not typed: `astro.config.mjs` reads
`[workspace.package] version` from the workspace `Cargo.toml` and injects it as a compile-time
constant, which `src/lib/version.ts` re-exports. The landing page and the footer read it from
there, so a release bump updates the site with no prose to edit. Never hardcode a version in a
page — it will be wrong one release later.

### Manual deploy

```bash
bun run deploy   # build, then wrangler deploy (needs `wrangler login`)
```

The custom domain is one line in `wrangler.jsonc` (`routes` with `custom_domain: true`) —
Cloudflare provisions the DNS record and certificate on deploy. Canonical tags and the sitemap
default to that domain; `SITE_URL` overrides them if the host ever changes:

```bash
SITE_URL=https://elsewhere.example.com bun run deploy
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

Internal links are written root-relative and unslashed (`/docs/concepts`) so they read correctly
in the repository. The base prefix for the target being built is added at build time — by
`src/lib/url.ts` in components, and by a rehype plugin in markdown.

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
