# Ursa website

The website for Ursa SQL, to be served at **https://ursa.cldixon.dev**: the
landing page at `/` and the docs at `/docs/`, one site, as duckdb.org and
duckdb.org/docs are. It is built with [Astro](https://astro.build) and
[Starlight](https://starlight.astro.build), and served as Cloudflare Workers
static assets, as the web shell (`web/`) is.

The web shell, [Ursa Shell](https://ursa-shell.cl-dixon.workers.dev), is a
separate app; the site links to it. The Python library's docs stay on the
Zensical site, https://cldixon.github.io/ursa/.

## Layout

- `src/pages/index.astro`: the landing page, an ordinary Astro page in the
  docs' fonts and colours (`src/styles/landing.css`).
- `src/content/docs/docs/`: the docs. Starlight serves only this tree: its
  pages are under `docs/` in the collection, so each one's URL starts with
  `/docs/`. `index.mdx` is the overview, `/docs/`.
- `astro.config.mjs`: Starlight's settings and the sidebar ("Get started",
  then "Topics" and its groups).
- `src/components/`: the docs header (with the Ursa Shell link), the site
  title, the logo (the web shell's Big Dipper) and the overview's cards.
- `src/styles/`: `fonts.css` (Inter, IBM Plex Mono and Ursa Box, the web
  shell's box-drawing font, so result tables join up), `docs.css` (the docs'
  colours and layout) and `landing.css`.
- `cloudflare.config.ts`: the Worker, `ursa-site`, static assets only.
  `wrangler.config.ts` is the setting for the asset bundler `cf build` uses
  internally (the assets are `dist/`).

## The docs and the tests

The docs pages are copies of `docs/sql/`, regrouped. Until the cutover removes
`docs/sql/`, both exist, and `ursa-cli`'s docs test checks both:

- `site_docs_run` runs every ```` ```sql ```` block on every page under
  `src/content/docs/` and compares its output with the ```` ```text ```` block
  after it, as `sql_docs_run` does for `docs/sql/`. The convention is at the
  top of `ursa-cli/tests/docs.rs`; in an `.mdx` page, skip a block with
  `{/* skip */}` on the line before it.
- `src/content/docs/docs/reference/functions.md` is generated from
  `SELECT * FROM ursa_functions()`, with `docs/sql/reference/functions.md`.
  Do not edit it by hand. `function_reference_is_current` checks both; to
  regenerate them, run from the repository root:

  ```sh
  URSA_BLESS=1 cargo test -p ursa-cli --test docs function_reference
  ```

```sh
cargo test -p ursa-cli --test docs     # all the docs checks
```

A link between docs pages that does not resolve, to a page or a heading,
fails the build (`starlight-links-validator`). Write links as absolute paths
with a trailing slash: `/docs/graphs/algorithms/#weights`.

## Build and preview

Needs bun (and Node 22.23 or later, which Astro runs on).

```sh
bun install
bun run dev        # http://localhost:4321, reloading as you edit
bun run build      # the static site, in dist/
bun run preview    # serve dist/ at http://localhost:4321
```

The Docs workflow (`.github/workflows/docs.yml`, job `site`) runs
`bun run build` on every pull request that touches `site/`.

## Deploy

Deployed at https://ursa-site.cl-dixon.workers.dev. To deploy, with
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` set for the account (or
after `bunx cf auth login`):

```sh
bun install
bun run deploy     # astro build, cf-wrangler build, cf deploy --prebuilt
```

`cf build` (which a plain `cf deploy` runs) sees Astro and expects the Astro
Cloudflare adapter's output, which a static site does not have. So the deploy
script packages `dist/` with `cf-wrangler build` (the bundler `cf` delegates
to for static projects; it reads `wrangler.config.ts`) and deploys that with
`cf deploy --prebuilt`, which uploads `dist/` as the static assets of the
Worker `ursa-site` and deploys it, at `https://ursa-site.<account subdomain>.workers.dev`. There is
no Worker code: `htmlHandling: "force-trailing-slash"` matches Astro's
`trailingSlash: "always"`, and Starlight's `404.html` answers any other path.

### The custom domain

The site's home is `ursa.cldixon.dev`. The `cldixon.dev` zone must be on the
same Cloudflare account. After the first deploy:

1. In the Cloudflare dashboard, go to **Workers & Pages** and open
   **ursa-site**.
2. Open **Settings**, then **Domains & Routes**, and choose **Add** and
   **Custom domain**.
3. Enter `ursa.cldixon.dev` and choose **Add domain**. Cloudflare creates
   the DNS record and the certificate; it can take a few minutes to go live.
   If a DNS record for `ursa` already exists, delete it first.
4. Open https://ursa.cldixon.dev/ and https://ursa.cldixon.dev/docs/.

Or let deploys apply it: add `domains: ["ursa.cldixon.dev"]` to the worker in
`cloudflare.config.ts` (it is commented there) and deploy again. Once the
domain serves the site, `workersDev` can be set to `false`.

After the cutover, the Zensical site's SQL pages (`docs/sql/`) can be removed
and their URLs pointed here.
