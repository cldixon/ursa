# Ursa Web Shell

A web page where anyone can run Ursa SQL in their browser, over sample graphs
stored in R2 (#279, part of #277). It is the `ursa` shell itself, in a
terminal, as DuckDB's web shell is DuckDB's CLI: the shell (dot-commands, SQL a
line at a time, DuckDB-style tables) is the `ursa-shell` crate, compiled into
`ursa-wasm` as `WebShell` and run in a Web Worker; the page draws it with
xterm.js. Around it is a bar laid out after DuckDB's web shell: New, Share,
Import, Graph, a Datasets menu and a light/dark toggle. The engine reads Parquet and
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
    drops what was typed; Ctrl+L clears; Tab and Shift+Tab move the focus out
    of the terminal, to the bar (there is no completion); `.help shortcuts`
    lists every key (the `SHORTCUTS` text in `app.js`, which the e2e test
    checks against the keys). The cursor moves and deletes by the character
    as seen (an emoji sequence, a flag, a letter and its accent), and CJK and
    emoji take two cells, by the width table `app.js` gives xterm. Keys pressed while a
    line runs wait for it. What the shell prints (tables, errors, colour) is written to the
    terminal as is, so a selection copies as the text on screen.
  - **Dot-commands** are the CLI's (`.help`, `.tables`, `.describe`,
    `.functions`, `.maxrows`, `.maxwidth`, `.timer`, `.mode`, …; `.help graph`
    is a page on the graph SQL, whose forms the e2e test runs), and the page's
    own, which the shell lists in `.help` and hands back to the page:
    `.clear`, `.dataset ?NAME?`, `.export FORMAT` (downloads the last result as
    CSV, Parquet, JSON or Arrow), `.files ?add?`, `.graph ?QUERY|NAME? ?SIZE BY x? ?COLOR BY y?`,
    `.history ?N?` and `.share`.
  - **Datasets.** Picking one starts a fresh session and runs its commands at
    the prompt, as if typed: its views and `CREATE PROPERTY GRAPH`, then two or
    three queries that show the graph functions, `GRAPH_TABLE` patterns and
    shortest paths, each after `--` comments that say what it shows (keep the
    explanations in the commands, not in JS comments). Long lines are broken
    at spaces to fit the terminal. `?dataset=karate` in the URL loads one on
    open. The menu works from the keyboard as a menu button (arrows, Home,
    End, Enter, Escape).
  - **Share** copies a link whose fragment holds the session's commands;
    opening it lists them, with control characters shown as escapes, and asks
    `Run N shared commands? [y/N]`: only `y` replays them, and a link's
    `.export` is skipped, so a link never downloads files. **Import** adds
    files from this computer (`WebShell.addFile`, at `memory://uploads/<name>`,
    where characters other than ASCII letters, digits, `.`, `_` and `-` become
    `_` and a name so changed gets a hash of the original) and defines a view
    over each, named after the file and quoted (`"données"`, `"order"`).
    **New** starts a fresh session. The theme follows the system until the
    toggle picks one, which is remembered; the terminal's colours follow it.
    A click on a bar button hands the focus back to the terminal; pressed from
    the keyboard, the button keeps it. Under 640px the bar is one row: the
    logo without its name, and under 420px icons for New, Share, Import and
    Graph.
  - **The Graph view** (`public/graph.js`, `public/graph-worker.js`):
    `.graph` (or the Graph button) draws the last result as a graph in a
    pane beside the terminal, or under it under 640px; `.graph QUERY` draws a
    query's and `.graph NAME` a property graph's (or a table's). The engine
    makes the graph (`ursa-engine`'s `graph::view`, through `WebShell.graph`
    and the `graph` op of the worker; see "The engine's graph" below): each
    row an edge from its `src` to its `dst` column (in any case), else from
    its first column to its second, the ids numbered `0..n` in the order
    they are first seen, at most 2,000,000 edges. The two columns must hold
    ids: integers, strings or dates, and the first two both of one kind. A
    float, a boolean, a list or a struct is refused with what to type
    instead (#427), so `.graph` after `SELECT * FROM pagerank(karate)` no
    longer draws each id tied to its score, nor a path query one edge to a
    list: `.graph karate SIZE BY pagerank` draws an algorithm's result, and
    `SELECT … AS src, … AS dst` names the ends of other rows. `ursa-viz` draws
    it (WebGPU, else WebGL2) with its ForceAtlas2 layout running in
    `graph-worker.js`, nodes sized by degree unless asked otherwise.
  - **Sizes, colours and the legend** (#389): `.graph [SOURCE] [NODES
    (query)] [SIZE BY x] [COLOR BY y]`, the clauses in any order after the
    source. `.graph karate SIZE BY pagerank COLOR BY louvain` sizes Karate's
    members by PageRank and colours them by Louvain community. A name is, in
    turn: a column of `NODES (SELECT id, … FROM …)` (its `id` column, or else
    its first, joined to the drawing's node ids); a column of the property
    graph's vertex tables (`COLOR BY club`); `degree` (of the drawing); or a
    per-node algorithm run on the property graph with the view's defaults
    through `algorithms(G, …)` (`direction => 'both'` where it has one,
    `seed => 1` where it is random): `pagerank`, `louvain`, `betweenness`,
    `closeness`, `label_propagation`, `connected_components`,
    `triangle_count`, `clustering_coefficient`. A source that is rows (a
    query, a table, the last result) takes `NODES` and `degree` only.
    `SIZE BY` needs numbers; `COLOR BY` colours the eight largest
    categories, the rest "other", and refuses a fractional column with more
    than eight values or any with more than eight that are over half the
    nodes (it suggests `SIZE BY`). The engine parses and resolves it all
    (`ursa-engine/src/graph/view/`) into two node columns of the result,
    from which the page reads each node's size and colour index and the
    legend's labels and counts. The legend, under the pane's
    header, names what sizes the nodes (with its range) and each colour's
    category and count, in text.
  - **The engine's graph** (#425): `.graph` is resolved in `ursa-engine`
    (`graph::view`: the clauses, the source, the ids as dense node indexes,
    `SIZE BY`, `COLOR BY` and `NODES`), not in `ursa-wasm`, so the CLI and
    the notebook can draw what the shell draws. It makes a graph result
    (`ursa-viz`'s `model`), which `WebShell.graph` hands the worker as one
    UGR buffer (`ursa-viz/UGR.md`), and the worker transfers it to the page:
    `{ugr, skipped}`. `graph.js` reads it with `ursa-viz/ui/ugr.js`
    (`build.ts` copies it to `dist/ugr.js`), as typed arrays over the buffer
    with no copy, and `public/graph-data.js` makes it the drawing: `n`,
    `src`, `dst`, each node's id (decoded when asked for), the size column
    as it is, and the colour column, a dictionary whose values are the
    categories by rank, as colour indexes and the legend. The result's
    provenance says the SQL and the rule that drew it (`edges`, or `graph`
    for a property graph); nothing shows it yet (#433 will).
    `bun test tests/graph-data.test.ts` runs that path without a browser,
    on the fixtures `ursa-engine`'s `tests/graph_view.rs` writes.
  - **Theme:** `ursa-viz` draws in the page's `--graph-*` tokens
    (`style.css`: background, edge ink, plain nodes, eight categorical hues
    and "other", stepped for each theme; and `--graph-highlight`,
    `--graph-selection` and `--graph-pin` for the view's highlight layer,
    #435, which nothing in the shell drives yet), set with `view.setTheme(…)`; the
    theme toggle (or the system's) recolours an open view. No CSS filter.
  - **Progress** (#390): the pane opens with "Loading graph view…" as soon
    as the engine takes more than 150 ms (not at all, for a quick error),
    and starts loading `ursa-viz`, here and in the layout worker, then,
    while the engine still runs the query. The graph goes to the worker
    before the page builds its view, and nothing is drawn before the
    layout's first positions. The header then reads `… · Laying out · level
    k of n`, then `N nodes · M edges · Done`. `window.ursaShell.graphTimings()`
    gives when each step ended, and `bun tests/graph-timing.ts [url] [runs]`
    times `.graph rmat` (1M edges): in headless Chromium, the first drawing
    went from 4.6 s after Enter to 0.95 s, and the layout's end from 12.1 s
    to 6.0 s.
  - **Timings** (#430): `graphTimings()` also says what the view costs to
    use, so the targets of `design/graph-explorer.md` §9 can be read from
    the page:

    | field | what |
    |---|---|
    | `frames` | `count`, the frames drawn since the page loaded; `render`, the time in `view.render()` of the last 120; `interval`, the time from one frame's start to the next's, of the last 120 frames that input asked for (a drag, a wheel, a key) and that began within 100 ms of the page's thread being free of the frame before: the view moving. It is the frame's whole cost; `render` is only the call. Each is `{count, p50, p95, max}` in ms. |
    | `pick` | `{count, last, p95}`: the time in `view.pick()` of a click, over the last 120. |
    | `actions` | the last 32, oldest first: `{command, at, engine, delta, frame}`. `command` is what was typed (`.graph karate`) or the input (`click`, `fit`, `key +`); `at` is when the input arrived, in ms after the last `.graph`'s Enter; `engine` is the ms the engine took and `delta` the ms applying its answer to the view (both `null` for an input that needs neither); `frame` is the ms from the input to when the page's thread was free of the frame that showed it: with no GPU the browser rasterises a frame on that thread after `render()` returns (4 s at a million edges), so this is when the pixels are there, where `firstDraw` is when `render()` returned. A click, which draws nothing, is shown by the next frame the browser paints. |
    | `longTasks` | the page's last 64 tasks over 50 ms: `{start, duration}`, `start` in ms after Enter. |

    `await window.ursaShell.graphMemory()` gives `wasm.engine`, `wasm.view`
    and `wasm.layout`, the bytes of each WebAssembly instance's memory (the
    engine's worker, the view on the page, the layout's worker), and
    `userAgent`, `performance.measureUserAgentSpecificMemory()`'s answer for
    the page and its workers (`{bytes, breakdown}`), which the cross-origin
    isolation of #429 allows. The browser answers after a garbage collection
    of its choosing, so after `timeoutMs` (10 s) `userAgent` is `null` and
    `userAgentError` says so.

    Keeping these costs a frame three clock reads, a timer and two writes
    into arrays made once (`graph-timings.js`): under a microsecond, and no
    allocation.
    Percentiles are worked out when `graphTimings()` is read.
    `tests/graph-timing.ts` prints all of it: the steps, the `.graph`
    action, the long tasks, the frame times of a scripted wheel zoom, a pick
    and the heaps. It runs the full Chromium (`channel: "chromium"`), since
    Playwright's headless shell has no `measureUserAgentSpecificMemory`;
    `CHANNEL=shell` runs the shell. `bun test tests/graph-timings.test.ts`
    tests the arithmetic.

    On `.graph rmat` (1M edges, headless Chromium, software WebGL2, median
    of three, 2026-11-21): `firstDraw` 1.28 s after Enter, but the `.graph`
    action's `frame` 5.85 s, since the browser spends 4.5 to 6.3 s
    rasterising that first frame on the page's thread after `render()`
    returns (0.7 ms); frames while a wheel zoom runs are 3.4 s apart at the
    p95; a pick takes 2.3 ms and a click is shown 16 ms after it; the wasm
    heaps are 49 MiB (engine), 52 MiB (view) and 97 MiB (layout worker),
    of 219 MiB for the page and its workers. On karate: frames 16.7 ms
    apart (p95 20 ms), `render()` 1.4 ms, a pick 0.2 ms.

    The layout worker (`graph-worker.js`) also lays out **rings and layers**
    (#436): a graph that carries `layout: {kind: "rings" | "layers", level}`,
    each node's distance from a seed or its layer, is laid out in one
    message, with no levels to wait for. Nothing sets it yet: the rules
    that draw a `hop` result as rings and paths as layers are #433.
    `graph.js` is imported on the first `.graph`, and it loads `ursa-viz`
    (about 1 MB gzipped), so a session that draws nothing downloads neither.
    The terminal keeps the focus and refits to its half (xterm's fit addon,
    then the engine's width); Esc (in the terminal or the pane) or the
    close button closes the pane, and New or a dataset does too. In the
    pane, drag or the arrow keys pan, the wheel or `+`/`-` zoom, a
    double-click or `0` fits, and a click on a node names it in the pane's
    header. On a touch screen (#428) one finger pans and two pinch: the
    view zooms by the change in the fingers' distance around the point
    between them, and pans as that point moves, so what is between the
    fingers stays there. A tap names a node (a second finger cancels it),
    and a double tap fits. The canvas draws at most 2 pixels for each CSS
    pixel, whatever the screen's ratio. A lost WebGL context or GPU device, or a layout that stops,
    closes the pane with an error in the terminal.
- `src/index.ts`: the Worker. `/data/<file>` serves `datasets/<file>` from the
  bucket with HEAD, byte ranges and validators; `/engine/<hash>.wasm` serves the
  engine; everything else is a static asset. A path that does not decode is
  404. `src/headers.ts` holds the security headers every response carries, from
  the Worker and `bun run preview` alike: a Content Security Policy (the page's
  own scripts, worker, fetches, fonts and images; WebAssembly compiles under
  `'wasm-unsafe-eval'`; `style-src` allows `'unsafe-inline'` for the `<style>`
  elements xterm.js adds; no framing), `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, and the three that make the page
  cross-origin isolated (#429): `Cross-Origin-Opener-Policy: same-origin`,
  `Cross-Origin-Embedder-Policy: require-corp` and
  `Cross-Origin-Resource-Policy: same-origin`. So `crossOriginIsolated` is
  true on the page and in the engine's and the layout's workers: timers are
  precise, `performance.measureUserAgentSpecificMemory()` works, and a
  `SharedArrayBuffer` can be made, which a threaded build would need. Every
  resource is same-origin, as the policy requires, so nothing else changes;
  a cross-origin resource could only be added if it sends CORP or CORS
  headers. The page has no inline script: the theme is
  set before the first paint by `public/theme.js`.
- `scripts/`: building the engine, assembling `dist/`, and uploading to R2.
  `build.ts` also builds the graph view (`scripts/viz.ts`: `ursa-viz` with
  its `web` feature and the `wasm-release` profile, then wasm-bindgen) into
  `dist/viz/<hash>/`, a static asset of about 4 MB (1 MB gzipped) named by
  a hash of its glue and `.wasm`; `dist/viz/viz.json` names the build.
  The build also copies the terminal's font, IBM Plex Mono (SIL OFL 1.1, from
  `@fontsource/ibm-plex-mono`), into `dist/fonts/`, so the page loads nothing
  from other hosts.
  Plex Mono's web fonts have no box-drawing characters, so table borders come
  from `public/fonts/ursa-box.woff2`, a font of just those, made by
  `scripts/box-font.py` with lines that run past each row so borders join.
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
downloads in each format, Import (with awkward file names), a shared link
replayed in a fresh page (answered y and N, and a crafted one with escape
sequences and `.export`), New, the theme toggle and history; the Graph view
(`.graph` on a result, a query and a property graph, its errors, pixels drawn
in both themes; `SIZE BY` and `COLOR BY` from an algorithm, a vertex column and
`NODES (…)`, each of their errors, at least two palette colours read back in
each theme with no inversion, a theme switch with the pane open, the legend's
text, and the status from Enter to Done, within a second for rmat; a node picked, typing while it shows, Esc and the close
button, the terminal refitting beside it and under it on a phone, a pinch, a
one-finger drag, a tap and a double tap from Chromium's touch input with each
node's place read back, the Graph button, and `ursa-viz` not downloaded until `.graph`); the keyboard and
phone checks: Tab out of the terminal, the menu's keys, the focus after a
click, no mid-word wraps and a one-row bar at 320px, a 16px input, and the grey
text's contrast; and the security headers, cross-origin isolation (its headers
on every kind of response, a dataset's ranges included, and
`crossOriginIsolated` on the page and in both workers), 404s for malformed and traversing
paths, and no Content Security Policy violation in the console.

```sh
bun run preview                               # in one terminal
bun run test:e2e                              # in another; or: bun tests/e2e.ts <url> [screenshot dir]
```

It needs Playwright and Chromium (`bun add --no-save playwright && bunx playwright install chromium`).

CI runs it: the "Ursa Shell (browser e2e)" job (`.github/workflows/shell-e2e.yml`)
runs on pull requests and pushes to `main` that touch `web/`, the crates built
into the page (`ursa-wasm`, `ursa-shell`, `ursa-viz` and the engine crates
under them), the Cargo files, or the datasets' inputs; it can also be started
by hand (Run workflow). It does what is above, with no Cloudflare credentials:
`bun run engine`, `bun run build`, `bun run upload:data -- --dry` (which only
stages `.data/`), then `bun scripts/preview.ts` and the suite against it, with
Playwright pinned in the workflow (`PLAYWRIGHT_VERSION`) and its own Chromium.
Cargo's dependencies are cached; so are `.engine/`, keyed by the engine's
sources, and `.data/`, keyed by the bundled CSVs, `gen.py` and
`upload-data.ts` (a miss builds the native `ursa` in release, several
minutes). A failed run uploads the screenshots and the server's and suite's
logs as the `shell-e2e` artifact.

## Datasets

- The library's bundled sample graphs (`python/ursa/datasets/data/`), each as
  CSV and as Parquet. They are redistributed from NetworkX under its 3-clause BSD
  licence (see `python/ursa/datasets/LICENSES.md`).
- `rmat_1m_edges.parquet`: a seeded R-MAT graph with 1,000,000 edges over
  46,318 nodes, made by `benchmarks/sql_scale/gen.py`. Generated here, so it is
  ours to publish (CC0).

`manifest.json` lists every file with its size, source and licence.
