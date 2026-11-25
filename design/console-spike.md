# The console spike: what `ursa serve` and the session protocol showed

> **Status:** the report of the spike, #536, November 2026. It tests
> [`console.md`](console.md) §3 (architecture), §4 (the protocol) and §5
> (`ursa serve` and security) before phase C1 commits to them.
> **Branch:** `claude/console-spike-536`, in two commits: layer (a), meant to
> merge, and layer (b), the spike itself.

## Verdict

**Go for C1**, with the protocol changes in "What didn't fit" below. One
session protocol serves both the browser demo (wasm, in a Web Worker) and a
native `ursa serve` (over a WebSocket): the same Rust dispatch
(`ursa_shell::session::Session::handle`) runs behind both, the shell's page
runs unchanged on either, and the existing e2e suite passes on both, apart
from the browser-only checks listed below. The socket costs about 1 ms on a
small reply and about 6 ms a megabyte on a large one; the native engine wins
that back and more on every query that does real work.

## What was built

**Layer (a), production quality** (commit `eba54e5`, meant to merge):

- `ursa_shell::session::Session`: the per-operation glue that was
  `ursa-wasm`'s `WebShell` (output capture, UGR encoding, inspect's JSON,
  uploads, export) as typed methods, with every operation behind
  `Session::handle(op, args, parts) -> Result<Reply, Error>`. A `Reply` is a
  JSON result and named binary parts; an `Error` has a protocol kind.
  `envelope()` makes the protocol's JSON of either.
- `ursa-wasm`'s `WebShell` is wasm-bindgen glue over it with one method,
  `handle`. The worker no longer dispatches: it hands every request to
  `handle`, except `memory`, which is the worker's own.
- `web/public/engine-client.js`: `connect(transport)` returns `{ready,
  call(op, args, parts), on(event, fn), close()}`; `WorkerTransport` wraps
  the worker. `app.js`'s `engine()` is a call to the client, and the ops take
  their protocol names.
- The suite: 475 checks pass against `bun scripts/preview.ts`.

**Layer (b), spike quality** (the second commit):

- `ursa-server`: axum with `ws`; `/session`; the framing of §4.2, both ways;
  a session actor per connection (a thread, `Handle::block_on` of the
  multi-threaded runtime inside a `LocalSet`, requests over an `mpsc`
  channel); loopback only; the per-run token; `Origin` and `Host` checks;
  the deploy's security headers, kept equal to `web/src/headers.ts` by a
  test that reads the TypeScript; `/data/NAME` from the served directory.
- `ursa serve [DIR] [--port N] [--assets DIR]` behind ursa-cli's
  non-default `serve` feature.
- `SocketTransport` in `engine-client.js`, chosen by a `<meta
  name="ursa-transport" content="socket">` the server puts in the index.
- The session's `query` op (the first rows as an Arrow IPC part, typed
  columns with their metadata, the result kept under an id) and `graph.open`
  by result id.
- `web/console/`: the bare console page (a textarea, `query`, the table with
  each column's type and `ursa.*` metadata, a Graph button that draws the
  result with the shell's `graph.js`). `bun run console` builds it, with
  `apache-arrow` 21.2.0 bundled, into `web/dist-console/`, which only `ursa
  serve` serves: the deploy's `dist/` is unchanged.
- Tests: `cargo test -p ursa-server` (security and the shell over a socket);
  `bun scripts/e2e-serve.ts` (the e2e suite against `ursa serve`);
  `web/tests/console.ts` (the console page's acceptance);
  `web/tests/protocol-timing.ts` (the measurements below).

## The e2e suite on both transports

- **Worker (preview):** all 475 checks pass.
- **Socket (`ursa serve`):** all checks pass except the ones the suite skips
  with `URSA_SERVE_TOKEN` set. They are exactly these 6:
  - an engine crash is reported; the shell works after an engine crash (a
    WebAssembly trap; natively the query runs, or the process would die);
  - graphMemory's check of each wasm instance's memory (the engine has no
    WebAssembly memory; `memory` reports the server's resident set instead);
  - the cross-origin isolation headers on `/engine/engine.json` and on the
    engine's `.wasm` (`ursa serve` serves no wasm engine);
  - the engine's worker (`/engine-worker.js`) is cross-origin isolated (there
    is none).

  Uploads were not skipped: requests carry binary parts too (see below), so
  Import and `addFile` run over the socket, and the native session keeps its
  files in memory as the demo does.
- The suite runs with two changes beyond the skips: every browser context
  gets the run's token as the cookie the printed link gives, and Bun's own
  `fetch` (the header checks) sends it too.
- Two timing checks of the big graph's wheel zoom (p95 of frame intervals,
  long tasks while slices draw) can fail on a loaded machine on either
  transport; the suite allows them ("100 allowed").

## Measurements

On a 4-core Intel Xeon at 2.1 GHz with 15 GB, headless Chromium, the server
built `--release`, the wasm engine `wasm-release`. The machine's load average
was 2.3 to 3.6 throughout (the browser and the other side of each test).
Medians: `protocol-timing.ts` runs each request 5 times, three times over (15
in all, the median of the three medians given); `graph-timing.ts` three
times.

| | worker (wasm) | socket (native) |
|---|---|---|
| `.graph rmat`: Enter to the engine's answer | 171 ms | 111 ms |
| `.graph rmat`: Enter to the first frame | 789 ms | 859 ms |
| `.graph rmat`: Enter to the whole graph drawn | 5,129 ms | 4,103 ms |
| `graph.open rmat` round trip (8.4 MB of UGR) | 96 ms | 112 ms |
| `query`, 1 row of `rmat_edges` (1M rows) | 71 ms (engine 71) | 26 ms (engine 24) |
| `query`, 1,000 rows | 85 ms (engine 83) | 29 ms (engine 28) |
| `query`, 100,000 rows (1.6 MB of Arrow) | 69 ms (engine 68) | 32 ms (engine 22) |
| `export arrow`, 1M rows (16 MB) | 32 ms | 129 ms |
| `pagerank(rmat)` in the engine | 378 ms | 65 ms |
| memory after loading rmat and opening it | 311 MB (wasm memory) | 440 MB (process RSS; 17 MB idle) |

**Bytes on the wire** are the same for both: the transports carry the same
messages. A `query` reply is about 700 bytes of JSON plus the Arrow stream:
736 bytes for 1 row, 16.6 KB for 1,000 rows, 1.6 MB for 100,000. `.graph
rmat` is 8.4 MB of UGR. The socket adds 2 to 14 bytes of WebSocket framing
and 8 bytes of part header per frame.

What they say:

- **The socket's cost is in transfer, not in round trips.** A small reply
  costs about 1 ms over the engine's time (26 against 24 ms). Large parts
  cost about 6 to 8 ms a megabyte (16 MB in 129 ms against 32 ms through
  `postMessage`, which transfers the buffer without a copy).
- **The first frame is the view's, not the transport's.** On both, the
  engine answers in 110 to 170 ms and the view and its hierarchy take the
  next 500 to 600 ms. The socket's 70 ms more is the page's thread receiving
  and assembling 8.4 MB (the viz module loads later, 305 against 237 ms):
  C1 should hand the part to the layout worker without touching it on the
  page, or stream it.
- **The native engine is 2 to 6 times faster** on anything with work in it
  (PageRank 65 against 378 ms; a scan of 1M rows 22 against 68 ms), and it
  has every core.
- **Nagle's algorithm cost 40 ms on every reply** until the server set
  `TCP_NODELAY`: a reply is a text frame and then its part frames, and the
  second write waited for the client's delayed ACK. C1 must keep it.
- **Memory:** the server's RSS (440 MB) is the whole process, with the
  allocator keeping what it freed; the wasm figure is the engine's linear
  memory, which also never shrinks. The native session holds the same data.

## What didn't fit the protocol of §4, and the changes proposed

Each is a change to `console.md` §4, made there too where it is settled.

1. **`cancel` was already taken.** Today's `cancel` drops a half-typed
   statement; §4.3's `cancel` stops a running request. *Change:* the shell's
   op is `cancelInput`; `cancel` keeps §4.3's meaning.
2. **The shell needs `start`.** The terminal's New, a dataset and crash
   recovery all start a fresh session with the page's dot-commands,
   shortcuts and phone settings. §4.3 lists no such op. *Change:* add
   `start {commands, shortcuts, graphMaxEdges}` to the compatibility ops,
   and `cancelInput`; the shell's `highlight` is `highlightAnsi {text,
   alone}`.
3. **Parts need names.** A reply can carry two parts of one kind, and some
   are optional (`graph.open`'s `positions`, `graph.explore`'s `delta`): a
   kind alone does not say which is which. *Change:* `parts: [{name, kind,
   bytes}]`; a client reading the reply as one object puts each part under
   its name (`engine-client.js` does).
4. **Requests carry parts too.** `addFile` sends a file. *Change:* §4.2's
   framing applies to requests as to replies (the header's id is the
   request's).
5. **One session per server did not fit the shell.** The shell's `start`
   replaces its session, and two tabs (or the e2e suite's many pages) would
   wipe each other's. The spike gives each WebSocket a session of its own.
   *Change:* `hello` takes `session: "new" | "shared"`; the shell asks for a
   new one, the console and agents for the shared one (§9's pairing).
6. **The token lives in a cookie, not `sessionStorage`.** A token in the
   page's address ends up in its Share links (`.share` copies the address),
   and a page script holding it can leak it. The server takes `/?token=…`
   once, answers with an `HttpOnly`, `SameSite=Strict` cookie named for its
   port (`ursa_token_4280`; cookies are shared across a host's ports) and
   a redirect without the token; the WebSocket and `/data/` take the cookie
   (or `?token=` for non-browser clients). *Change:* §5 says so.
7. **`/data/` needs the token too.** The shell's page lists the datasets from
   `/data/manifest.json`, so the server serves the directory's files, which
   are data: they need the token, unlike the page's files.
8. **Schema metadata.** `ursa.pattern` is on the schema of a `GRAPH_TABLE`
   result, not on a column, so `columns` alone loses it. *Change:* `query`'s
   result has `metadata`, the schema's.
9. **`memory` differs by transport.** The demo reports its WebAssembly
   memory; natively there is none. *Change:* `memory` answers `{bytes,
   kind: "wasm" | "rss"}`.
10. **Errors from the transport.** A WebAssembly trap is not the engine's
    error but the worker's; the worker reports it as `crash`, as a closed
    socket is. Most engine errors are DataFusion `Plan` errors, so `sql`
    covers "no graph is open" too: the kinds are coarse but enough.
11. **Relative paths.** The wasm engine resolves them against a URL
    (`set_base_url`, global); natively they resolve against the process's
    working directory, which `ursa serve` sets to `DIR`. That is one root
    per process: a per-session root needs the engine to take one
    (`ursa_engine::paths`). C1 issue.
12. **`COPY … TO 'name'` natively.** The spike's sessions keep files in
    memory, as the demo does, so the shell behaves alike on both. In a real
    `ursa serve`, `COPY` should write to `DIR`. C1 decides; until then the
    server's sessions have `uploads: true`.
13. **`hello` first** is the client's habit, not enforced by the server.
    Harmless; C1 can enforce it.

What fit as designed: the JSON envelope, the binary framing, UGR unchanged,
Arrow IPC with the `ursa.*` metadata inside the schema, one request at a
time, `graph.*` under one prefix with today's payloads, and the session
actor (no `Send` needed; DataFusion's partitions ran on the runtime's
workers).

## §12's questions

**1. Shipping the UI.** What `ursa serve` needs is `web/dist` without the
wasm engine: 3.7 MB on disk, 1.3 MB gzipped (the graph view's wasm is 2.9 MB
of it, 1.0 MB gzipped), plus the console's 59 KB gzipped. Today's published
`ursa-cli` crate is 104 KB compressed, so prebuilt assets would make it about
13 times larger, and the crate would carry generated files. Also, the
`serve` feature cannot be published as the spike has it: `cargo publish`
of ursa-cli fails ("no matching package named `ursa-server`") while
`ursa-server` is unpublished, even with the feature off. *Answer:* ship
`serve` in the release binaries only, with the assets embedded at build time
(`rust-embed`), and publish `ursa-server` without assets, its `serve` reading
them from a directory (`--assets`) for people who build it themselves. The
crates.io dry run must then list `ursa-server`.

**3. Results kept per client.** On rmat, a whole result kept is 16 MB
(`SELECT * FROM rmat_edges`, 1M rows of two `Int64`s, as Arrow), PageRank's
0.7 MB. 16 results of the largest kind is 256 MB. The `query` op holds every
row (it collects the result before it sends the first rows). *Answer:* keep
16 results, and cap them by bytes, as the design says, at 256 MB; a result
over the cap is kept for its first rows only, and `fetch` past them says to
run the query again. Measure again in C2 with real sources.

**4. The graph view's coupling to the terminal.** Less than feared.
`graph.js` imports nothing of the terminal: it takes `status`, `legend`,
`onCommand(command)`, `run(command)` and `inspect(args)` from its host, and
the console page drew a `query` result with it in about 30 lines, sending
the view's commands (a double-click's `.expand 33`) to its History list and
their deltas back to the view. The coupling is in `app.js`: about 300 lines
that tie the view to the shell (the search box runs `.find` by typing it,
`countNotShown` after each change, Esc, the pane's markup and ids, the
`.graph` echo, the theme). C1 moves them into a `graph-pane.js` that both
hosts use, with a `commands` sink, as §3 said.

## C1's issues (drafts)

1. **Merge the session layer (a).**
   *Scope:* `ursa_shell::session`, `WebShell.handle`, `engine-client.js`
   with `WorkerTransport`, `app.js` on it (commit `eba54e5`).
   *Acceptance:* the e2e suite passes on preview; the demo is unchanged.
2. **Protocol v1 as the spike amended it.**
   *Scope:* §4's changes 1–4, 8–10 above: `cancelInput` and `start`, named
   parts, request parts, schema metadata, `memory`'s kind, `hello`'s
   `session`; a protocol document (`docs/` or `ursa-shell`) generated from or
   tested against `Session::handle`.
   *Acceptance:* a Rust test per op through `handle`; the JS client and both
   transports follow it; `hello` refuses another version.
3. **`ursa-server`, publishable.**
   *Scope:* the spike's crate made production quality: `TCP_NODELAY`,
   shared and new sessions (`hello`), a session per tab or one shared,
   graceful shutdown, errors as the protocol's, `cancel` (§4.3, native
   only) with DataFusion's cancellation, `rust-embed` assets behind a feature.
   *Acceptance:* `cargo publish --dry-run` lists and packages it;
   `cargo deny check` passes; the security tests (no token, a wrong token or
   cookie, a foreign `Origin`, a wrong `Host`, loopback only) run in CI.
4. **`ursa serve` in the release binaries.**
   *Scope:* the `serve` feature on in `release-cli.yml` and the Homebrew
   build, assets built by `bun run build` first and embedded; `--port`,
   `--no-open` (and opening the browser), `--read-only`; a database file or
   a directory as its argument.
   *Acceptance:* `ursa serve club.ursa` opens the shell with the database
   attached; the release binary's size growth is stated in the PR.
5. **The e2e suite on both transports in CI.**
   *Scope:* `shell-e2e.yml` runs `bun scripts/e2e-serve.ts` beside the
   preview run, with the 6 skips.
   *Acceptance:* both jobs green on a PR that touches the shell.
6. **A root per session, and `COPY` to disk natively.**
   *Scope:* `ursa_engine::paths` takes a session's base (a URL in wasm, a
   directory natively) in place of the process's directory; native
   `ursa serve` sessions write `COPY … TO 'name'` under it, and keep
   `memory://uploads` for imports only.
   *Acceptance:* two sessions with different roots read their own files;
   the demo's behaviour is unchanged.
7. **Large parts without the page's thread.**
   *Scope:* the socket's binary parts go to the graph's layout worker (or
   are read as a stream) without a copy on the page; measure `.graph rmat`
   Enter to the first frame again.
   *Acceptance:* over the socket, the first frame comes no later than over
   the worker.
8. **`graph-pane.js`: the graph pane for any host.**
   *Scope:* move the view's host glue out of `app.js` (search, `.find`,
   not-shown counts, Esc, the pane's markup) behind a `commands` sink (§3).
   *Acceptance:* the shell and the console page both use it; the graph view
   behaves as before (the e2e suite).
9. **Results kept and paged.**
   *Scope:* `query`, `fetch`, `close` and `export {result}` as §4.3, with
   16 results and a 256 MB cap per session, the oldest dropped.
   *Acceptance:* tests of the cap and of `not_found` for a dropped result.

## How to run it

```sh
cd web && bun run engine && bun run build && bun run console
cargo build --release -p ursa-cli --features serve
$CARGO_TARGET_DIR/release/ursa serve web/.data           # prints the link
bun scripts/e2e-serve.ts 8901                            # e2e over the socket
URSA_SERVE_TOKEN=… bun tests/console.ts http://localhost:PORT
URSA_SERVE_TOKEN=… bun tests/protocol-timing.ts http://localhost:PORT 5
bun tests/protocol-timing.ts http://localhost:8788 5     # the worker, under preview
```
