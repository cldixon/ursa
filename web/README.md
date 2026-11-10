# Ursa Web Shell

A web page where anyone can run Ursa SQL in their browser, over sample graphs
stored in R2 (#279, part of #277). It is the `ursa` shell itself, in a
terminal, as DuckDB's web shell is DuckDB's CLI: the shell (dot-commands, SQL a
line at a time, DuckDB-style tables) is the `ursa-shell` crate, compiled into
`ursa-wasm` as `WebShell` and run in a Web Worker; the page draws it with
xterm.js. Around it is a bar laid out after DuckDB's web shell: New, Share,
Import, a Datasets menu and a light/dark toggle. The engine reads Parquet and
CSV over HTTP with range requests. The Worker here serves files and does no
query work.

The setup follows the Plain Text site's (`cldixon/plane-text`, `site/cloudflare/`):
bun for the scripts, the `cf` CLI to build and deploy, a Worker in front of
static assets and an R2 bucket.

## Layout

- `public/`: the page (`index.html`, `app.js`, `style.css`), the Datasets
  menu (`datasets.js`) and the engine's Web Worker (`engine-worker.js`).
  - **The terminal** is xterm.js (`@xterm/xterm`, copied into `dist/vendor/`
    by the build), with a line editor in `app.js`: Enter sends the line to the
    shell, which runs a statement once it ends with `;` and shows the
    continuation prompt meanwhile; Shift+Enter adds a line to what is typed;
    ↑/↓ recall history (kept in this browser); Ctrl+C copies a selection, or
    drops what was typed; Ctrl+L clears. Keys pressed while a line runs wait
    for it. What the shell prints (tables, errors, colour) is written to the
    terminal as is, so a selection copies as the text on screen.
  - **Dot-commands** are the CLI's (`.help`, `.tables`, `.describe`,
    `.functions`, `.maxrows`, `.maxwidth`, `.timer`, `.mode`, …; `.help graph`
    is a page on the graph SQL, whose forms the e2e test runs), and the page's
    own, which the shell lists in `.help` and hands back to the page:
    `.clear`, `.dataset ?NAME?`, `.export FORMAT` (downloads the last result as
    CSV, Parquet, JSON or Arrow), `.files ?add?`, `.history ?N?` and `.share`.
  - **Datasets.** Picking one starts a fresh session and runs its commands at
    the prompt, as if typed: its views and `CREATE PROPERTY GRAPH`, then two or
    three queries that show the graph functions, `GRAPH_TABLE` patterns and
    shortest paths. `?dataset=karate` in the URL loads one on open.
  - **Share** copies a link whose fragment holds the session's commands; opening
    it replays them. **Import** adds files from this computer
    (`WebShell.addFile`, at `memory://uploads/<name>`) and defines a view over
    each. **New** starts a fresh session. The theme follows the system until the
    toggle picks one, which is remembered; the terminal's colours follow it.
- `src/index.ts`: the Worker. `/data/<file>` serves `datasets/<file>` from the
  bucket with HEAD, byte ranges and validators; `/engine/<hash>.wasm` serves the
  engine; everything else is a static asset.
- `scripts/`: building the engine, assembling `dist/`, and uploading to R2.
  The build also copies the terminal's font, IBM Plex Mono (SIL OFL 1.1, from
  `@fontsource/ibm-plex-mono`), into `dist/fonts/`, so the page loads nothing
  from other hosts.
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

Or without Cloudflare: `bun run preview` serves `dist/`, `.data/` (with byte
ranges) and the engine from `.engine/` as the Worker does, at
http://localhost:8788.

## Test

`tests/e2e.ts` drives the page in headless Chromium against a running site
(`bun run preview`, `cf dev` or a deploy): every dataset in the menu, SQL typed
at the prompt (one line, several, an error), every dot-command, a long table,
downloads in each format, Import, a shared link replayed in a fresh page, New,
the theme toggle and history.

```sh
bun run preview                               # in one terminal
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
