# Ursa Shell

A web page where anyone can run Ursa SQL in their browser, over sample graphs
stored in R2 (#279, part of #277). The engine is `ursa-wasm`: Ursa's SQL engine
compiled to WebAssembly. It runs in a Web Worker in the page and reads Parquet
and CSV over HTTP with range requests. The Worker here serves files and does no
query work.

The setup follows the Plain Text site's (`cldixon/plane-text`, `site/cloudflare/`):
bun for the scripts, the `cf` CLI to build and deploy, a Worker in front of
static assets and an R2 bucket.

## Layout

- `public/`: the page (`index.html`, `app.js`, `examples.js`, `style.css`) and
  the engine's Web Worker (`engine-worker.js`). The worker keeps three engine
  sessions: the **workspace** (the editor's, with the sample graphs defined at
  start), the **Tour**'s (empty at start, as the Getting Started page is) and a
  **meta** one for the page's own lookups.
- **The Tour** is built from `ursa-wasm/tests/getting_started.sql`, the fixture
  the browser parity test runs: each `-- @step` there is a Tour step
  (`scripts/tour.ts`, written to `dist/tour.json`). So every step is tested in
  CI against the native engine. Running a step runs the steps before it first.
- `src/index.ts`: the Worker. `/data/<file>` serves `datasets/<file>` from the
  bucket with HEAD, byte ranges and validators; `/engine/<hash>.wasm` serves the
  engine; everything else is a static asset.
- `scripts/`: building the engine, assembling `dist/`, and uploading to R2.
- `cloudflare.config.ts`: the Worker (name `ursa-shell`, the `ursa-graph` bucket
  as `DATA`, on `workers.dev` for now). `wrangler.config.ts` is the setting for
  the asset bundler `cf build` uses internally; deploys use `cf` only.

**Why the engine is in R2:** Workers static assets are limited to 25 MiB a file,
and the engine is about 31 MB. It is stored brotli-compressed (about 5 MB) under
its content hash, `engine/<hash>.wasm.br`, and the Worker sends it as is with
`Content-Encoding: br`. `dist/engine/engine.json` names the build the page loads,
so a deploy switches engines atomically and old builds stay valid for open tabs.

## Build and deploy

Needs bun, the `wasm32-unknown-unknown` target and `wasm-bindgen-cli` 0.2.126
(see `ursa-wasm/README.md`), and `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID`
for the account.

```sh
bun install
bun run engine          # build ursa-wasm into .engine/ (a few minutes: fat LTO)
bun run upload:engine   # put engine/<hash>.wasm.br in the bucket
bun run upload:data     # stage the sample datasets (CSV + Parquet) and upload them
bun run deploy          # assemble dist/ and deploy the Worker with cf
```

Upload the engine before deploying the page that names it. The datasets only
need uploading when they change.

## Local

`cf dev` simulates the bucket under `.wrangler/state`; seed it with `--local`:

```sh
bun run engine && bun run upload:engine -- --local && bun run upload:data -- --local
bun run dev
```

## Test

`tests/e2e.ts` drives the page in headless Chromium against a running site
(`cf dev`, or a deploy): every Tour step, example, Data action and function,
downloads in each format, both views, history and shared links.

```sh
bun run dev                                   # in one terminal
bun run test:e2e                              # in another; or: bun tests/e2e.ts <url> [screenshot dir]
```

It needs Playwright and Chromium (`bun add --no-save playwright && bunx playwright install chromium`).

## Datasets

- The library's bundled sample graphs (`python/ursa/datasets/data/`), each as
  CSV and as Parquet. They are redistributed from NetworkX under its 3-clause BSD
  licence (see `python/ursa/datasets/LICENSES.md`).
- `rmat_1m_edges.parquet`: a seeded R-MAT graph with 1,000,000 edges over
  46,318 nodes, made by `benchmarks/sql_scale/gen.py`. Generated here, so it is
  ours to publish (CC0).

`manifest.json` lists every file with its size, source and licence.
