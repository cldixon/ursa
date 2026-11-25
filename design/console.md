# The Ursa Console: `ursa serve`, a graph explorer on a live session

> **Status:** design, November 2026, for `ursa` 0.13 and after. Nothing here is built yet.
> **Follows:** [`graph-explorer.md`](graph-explorer.md) (Phases 1–3, shipped in the web shell).
> **Spike:** #536. It tests §4 and §5 before anything else is built.

`ursa serve` starts a local server, opens a browser tab, and gives you a console: an SQL editor, results as tables, a sources panel (local files, object storage, attached databases), and the graph view rendering results from live queries. It is DuckDB's `duckdb -ui` for a graph engine, and Neo4j's Bloom for Ursa.

The engine runs natively in the `ursa` process. The browser runs the interface, the graph's layout and its rendering. They speak one **session protocol** (§4), which the browser demo also speaks to its engine in a Web Worker. The same protocol is what an agent connects to later (§9).

## Summary

1. **One session, several clients.** `ursa serve` holds one engine session: the same `ursa_shell::Shell` the interactive CLI runs, with its tables, attached databases, settings, cached graph indexes and open graph. The browser is a client of it, not a caller of the CLI. Later, an agent is a second client of the same session.
2. **One protocol, two transports.** The session protocol is JSON control messages plus binary payloads (UGR for graphs, Arrow IPC for tables). Over `postMessage` it reaches the wasm engine in the browser demo; over a WebSocket it reaches `ursa serve`. The page doesn't know which.
3. **It grows from what exists.** The web shell already talks to its engine through 15 message operations, with graphs as UGR. The protocol is those operations made explicit and versioned, plus what a console needs that a terminal does not: structured results, a catalog, sources and cancellation.
4. **Local and safe by default.** The server binds to `127.0.0.1`, requires a per-run token, and checks `Origin` and `Host` on every request, because the native engine can read any file the user can.
5. **The browser draws; the engine thinks.** Layout and rendering stay in the browser (`ursa-viz`), as today. The engine answers queries and owns the open graph. Credentials never reach the browser.
6. **The web shell stays the demo.** Its graph view is frozen, not removed. New exploration work goes into the console. Once the console runs in the browser demo too (the wasm transport, phase C4), we decide whether the shell keeps its graph view.

| phase | ships |
|---|---|
| Spike | `ursa serve` runs today's web shell over a WebSocket against a native session, and a bare console page proves structured results (§11) |
| C1 | The protocol in `ursa-shell`, both transports, `ursa serve` with the shell UI, e2e on both |
| C2 | The console: editor, result grid, catalog, the graph pane; local files and object storage |
| C3 | Agent-ready: the skill doc and agent-friendly CLI help; MCP on the session |
| C4 | Paired mode, and the console in the browser demo (wasm transport) |
| later | Database connectors, desktop packaging, hosted options |

## 1. What exists

Read from `main` at `d9496af`.

### The browser shell's engine boundary

The page (`web/public/app.js`) never calls the engine directly. It starts `engine-worker.js` and calls `engine(op, args, transfer)`, which posts `{id, op, ...args}` and resolves with the worker's `{id, ok, ...}` reply. Requests run one at a time, in order. The operations:

| op | what it does | reply |
|---|---|---|
| `start` | a fresh session; registers the page's host commands and shortcuts | version, prompt |
| `feed` | one typed line: SQL or a dot-command | `{out, err, flow, prompt, continuation, pending}`; `out` and `err` are ANSI text for the terminal |
| `cancel` | abandons a statement being typed (not a running query) | prompt |
| `width` | the terminal's width, for table drawing | — |
| `highlight` | ANSI-highlights a line of SQL | text |
| `memory` | the wasm memory's size | bytes |
| `addFile`, `files` | puts a file in `memory://uploads`, lists them | URL, list |
| `export` | the last result as CSV, Parquet, JSON or Arrow | bytes |
| `graph` | `.graph …`: opens a graph | UGR bytes, positions |
| `explore` | `.expand`, `.hide`, `.show`, `.undo`, `.redo`, `.collapse` | text, a UGR delta |
| `find` | `.find …` | text, selected node slots |
| `notShown` | each node's count of edges not drawn | `Uint32Array` |
| `inspect` | a node's or an edge's row of its table | JSON |
| `closeGraph` | closes the open graph | — |

Behind the worker, `ursa-wasm/src/shell.rs` (`WebShell`) wraps `ursa_shell::shell::Shell`, the type the native CLI's REPL also runs. `WebShell` owns the glue: capturing output into buffers, encoding graphs and deltas as UGR, and inspect's JSON. That glue is the part §3 moves into `ursa-shell`.

The graph view (`graph.js`, `graph-worker.js` and the `ursa-viz` wasm) never touches the engine. It receives UGR and deltas through `app.js`, lays the graph out in its own worker, and renders with wgpu.

### What the terminal shape costs a console

- **Results are text.** `feed` returns what the shell printed, as ANSI. A console needs columns, types, Arrow metadata (`ursa.role` and friends, `docs/sql/reference/metadata.md`) and rows it can page and sort.
- **One result at a time.** `export` and `.graph` work on "the last result". A console keeps several results open, in tabs.
- **No catalog.** The page learns what tables exist only by running `.tables`.
- **No real cancel.** `cancel` drops typed input. A running query in the browser cannot be stopped: the worker is single-threaded.
- **The graph view echoes into the terminal.** A double-click runs `.expand 33` and prints it. In the console, those commands go to a history panel instead.

## 2. Goals and non-goals

**Goals.**

- `ursa serve` starts in under a second and opens a working console.
- Everything the CLI can read, the console can read: local files, `s3://`, `gs://`, `az://` and R2 through the existing `object_store` backends and credential chain, and Ursa database files.
- Graph results render with the full graph view: Phase 1–3's rules, exploration, inspector, aggregation, at native engine speed and without wasm's 4 GB ceiling on the engine.
- The browser demo keeps working, tested by the same e2e suite.
- The protocol is stable enough that an MCP server can be built on it without changing it.

**Not now.**

- Accounts, sign-in, several users or remote access. `ursa serve` is for the person at the machine.
- Hosted or SaaS deployment, or a self-hosted Cloudflare bundle.
- A desktop app. Once `ursa serve` exists, a Tauri wrapper is packaging.
- Database connectors (Postgres and so on). The sources panel is designed for them; they come after C2.
- Replacing the CLI's REPL.

## 3. Architecture

```
          ursa serve (native, one process)                                browser tab
 ┌───────────────────────────────────────────────┐            ┌───────────────────────────────────────┐
 │ axum: static UI, /session (WebSocket)         │◀──────────▶│ console UI                            │
 │   │ token, Origin and Host checks             │  protocol  │   engine client ── SocketTransport    │
 │   ▼                                            │            │   graph pane: graph.js                │
 │ session actor (one thread, LocalSet)          │            │      └── graph-worker.js + ursa-viz   │
 │   ursa_shell::session::Session                │            │          (layout, rendering)          │
 │     └── Shell ── ursa-engine ── object_store  │            └───────────────────────────────────────┘
 │                    (files, S3, GCS, Azure)    │
 └───────────────────────────────────────────────┘

 browser demo (unchanged shape):
 page ── WorkerTransport ── engine-worker.js ── ursa-wasm ── ursa_shell::session::Session ── Shell (wasm)
```

### Crates

- **`ursa-shell`: a new `session` module.** `Session` owns a `Shell`, its output buffers, its results, and the protocol's operations as Rust methods: `Session::handle(Request) -> Reply`. It is target-independent, and both `ursa-wasm` and the server call it. The UGR encoding, inspect JSON and output capture move here from `ursa-wasm/src/shell.rs`. `ursa-wasm` shrinks to wasm-bindgen glue over `Session`.
- **`ursa-server`: a new native-only crate.** It holds axum, the WebSocket, the security checks, the static assets and the session actor. Keeping it out of `ursa-cli` keeps the CLI's dependencies unchanged for people who don't build the UI, and gives MCP (§9) a home beside it.
- **`ursa-cli`: the `serve` subcommand**, behind a `serve` cargo feature (on in release builds) that pulls in `ursa-server`.

### The session actor

`Shell` is built around single-threaded state (`Rc`, `RefCell`, boxed closures), and the engine runs one statement at a time anyway. So the server gives the session its own OS thread running a Tokio `LocalSet`. Clients talk to it through an `mpsc` channel of `(Request, reply sender)`. The engine still uses the multi-threaded runtime for DataFusion's work, so queries are parallel; only the session's bookkeeping is single-threaded. This avoids making `Shell` `Send`, and serializes requests, which the protocol requires anyway (§4.5).

### The web side

- **`engine-client.js`:** `connect(transport)` returns `{call(op, args), on(event, fn), close()}`. `WorkerTransport` wraps today's worker; `SocketTransport` opens `ws://127.0.0.1:PORT/session`. `app.js`'s `engine()` becomes a call to this client, with no other change.
- **The console:** a second entry point beside the shell (`web/console/`, served at `/console/`; URSA-12), sharing `engine-client.js` and the graph modules. The graph modules stop writing to the terminal directly: they call a `commands` sink the host provides (the terminal in the shell, the history panel in the console). Done in C1 (URSA-8): `graph-pane.js`'s `createPane({container, engine, commands, status})` is the pane for any host. It owns its markup, search, Esc, the not-shown counts and the theme, and runs the graph commands itself (`pane.exec(line)`); the sink, `commands(command, {via, current})`, receives what the view, the inspector and the search ask to run, and the host hands each back to `pane.exec` once it has shown it.

## 4. The session protocol

Version 1, as the spike amended it (`console-spike.md`, "What didn't fit") and URSA-2 made it. The reference, generated from `ursa_shell::session::OPS` and tested against `Session::handle`, is `docs/reference/protocol.md`; what is built so far is there, and the tables below are the whole design.

### 4.1 Messages

Every message is one JSON object, plus zero or more binary **parts**.

A **request**, from a client:

```json
{"v": 1, "id": 17, "op": "query", "args": {"sql": "SELECT * FROM pagerank(karate) LIMIT 5", "rows": 1000}}
```

A **reply**, from the session, exactly one per request, with the same `id`:

```json
{"id": 17, "ok": true, "result": {"result": "r4", "columns": [...], "metadata": {}, "rows": 5, "more": false, "ms": 12},
 "parts": [{"name": "arrow", "kind": "arrow", "bytes": 1832}]}
```

An **error** reply:

```json
{"id": 17, "ok": false,
 "error": {"kind": "sql", "message": "column \"pagernk\" not found", "hint": "did you mean pagerank?"}}
```

`kind` is one of:
- `sql`: the statement is wrong;
- `engine`: it failed while running;
- `cancelled`;
- `not_found`: a result or graph id that has gone;
- `protocol`: a bad request;
- `crash`: the engine is in an unknown state, and the client should start a new session.

An **event**, from the session, unprompted:

```json
{"event": "progress", "for": 17, "phase": "scan", "rows": 4200000}
```

### 4.2 Binary parts

Each part is listed in the JSON as `{name, kind, bytes}`. The name says which is which: a reply can carry two parts of one kind, and some are optional (`graph.open`'s `positions`, `graph.explore`'s `delta`). A client reading a reply as one object puts each part under its name. Requests carry parts the same way (`addFile`'s file), with the same framing; the header's id is the request's.

Over `postMessage`, `parts` hold the `ArrayBuffer`s themselves, transferred, not copied, as `engine-worker.js` does today.

Over the WebSocket, the JSON goes in a text frame, and each part follows in its own binary frame, in the order `parts` lists them. Each binary frame starts with 8 bytes:

| offset | size | what |
|---|---|---|
| 0 | 4 | the request's `id`, `u32` little-endian |
| 4 | 2 | the part's index in `parts`, `u16` |
| 6 | 2 | reserved, zero |
| 8 | … | the part's bytes |

A reply is complete when its text frame and every part it lists have arrived. Frames of different replies don't interleave: the session sends one reply at a time.

Part kinds:
- **`ugr`:** a `GraphResult` or a `GraphDelta`, as `ursa-viz/UGR.md` defines them. No change to UGR.
- **`arrow`:** an Arrow IPC stream (schema, then record batches), which the console decodes with `apache-arrow`'s JS reader. The `ursa.*` field metadata travels inside the schema.
- **`f32`, `u32`:** a bare little-endian array, for positions, node slots or counts.
- **`bytes`:** a file, for exports.

### 4.3 Operations

**Session.**

| op | args | result | notes |
|---|---|---|---|
| `hello` | `protocol: 1`, `client: {name, kind}`, `session: "new" \| "shared"` | `{protocol: 1, version, transport: "native" \| "wasm", capabilities: [...], session: {kind, id?, database?, cwd?}}` | Must be first: the session refuses any other request before it, and a `protocol` it doesn't speak. `kind` is `"human"` or `"agent"` (§9). The shell asks for a `new` session (its `start` replaces it); the console and agents for the `shared` one. |
| `cancel` | `target: id` | `{}` | Stops the running request `target`: it replies `cancelled`. Capability `cancel`, native only. The shell's dropping of typed input is `cancelInput`. |
| `settings` | `set?: {name: value}` | all settings | The same as `SET` and `RESET` statements. |

**The console's own operations.**

| op | args | result, parts |
|---|---|---|
| `query` | `sql`, `rows` (default 1,000), `params?` | `{result, columns, metadata, rows, more, ms, notices}`; `arrow`: the first `rows` rows |
| `fetch` | `result`, `offset`, `rows` | `{rows, more}`; `arrow` |
| `export` | `result`, `format` (`csv`, `parquet`, `json`, `arrow`) | `bytes` |
| `close` | `result` | `{}` |
| `explain` | `sql` | `{plan}`: the plan as text |
| `complete` | `sql`, `cursor` | `{items: [{label, kind, detail}]}`: tables, columns, functions |
| `highlight` | `sql` | `{spans: [{from, to, kind}]}`, from `ursa_shell::highlight::pieces` |

- `columns` gives each column's `{name, type, nullable, metadata}`; `metadata` is the schema's own, where `ursa.pattern` sits for a `GRAPH_TABLE` result.
- `result` is an id such as `"r4"`. The session keeps a client's last 16 results, and drops the oldest beyond 256 MB.
- `.graph` of a result, export and paging all name it, instead of "the last result".
- A statement with no rows (`CREATE`, `SET`) gives `columns: []` and a `notices` line.

**Catalog and sources.**

| op | args | result |
|---|---|---|
| `catalog` | — | `{databases: [{name, schemas: [{name, tables: [{name, kind, columns}]}]}], graphs: [{name, vertices, edges}]}` |
| `sources.list` | `url`: a directory or a bucket prefix (`file:///data/`, `s3://bucket/prefix/`) | `{entries: [{name, url, kind: "dir" \| "file", format, bytes}]}` |
| `sources.attach` | `url`, `as` | `{}`: the same as `ATTACH` |
| `files`, `addFile` | as today | as today, for uploads into `memory://uploads` |

`catalog` changes are announced: after any statement that changes the catalog, the session sends `{"event": "catalog"}`, and clients refetch.

**Graphs.**

| op | args | result, parts |
|---|---|---|
| `graph.open` | `text`: `.graph`'s arguments, or `result` | `{graph, nodes, edges, skipped, rule}`; `ugr`, and `f32` positions if placed |
| `graph.explore` | `command`: `.expand 33`, `.undo`… | `{out, err, nodes, edges}`; `ugr` delta |
| `graph.find` | `command` | `{matches, notShown, notShownTotal}`; `u32` selected slots |
| `graph.notShown` | — | `u32` counts |
| `graph.inspect` | `node, key` or `edge, src, dst`, `neighbours?` | inspect's JSON, unchanged |
| `graph.close` | — | `{}` |

These are today's `graph`, `explore`, `find`, `notShown`, `inspect` and `closeGraph` under one prefix, with the same payloads. The session has one open graph, as today. Several graphs, in tabs, are a later change: `graph` ids are in the protocol from the start so that it is additive.

**The shell's compatibility operations.**

| op | args | result |
|---|---|---|
| `start` | `commands`, `shortcuts`, `graphMaxEdges` | a fresh session: `{version, prompt, promptColor}` |
| `feed` | `line` | as today |
| `cancelInput` | — | drops a half-typed statement: `{prompt}` |
| `width` | `columns` | `{}` |
| `highlightAnsi` | `text`, `alone` | `{text}` |
| `memory` | — | `{bytes, kind: "wasm" \| "rss"}`: what it measured |

The terminal UI runs on these over either transport.

### 4.4 Events

| event | when | fields |
|---|---|---|
| `progress` | while a request runs, at most every 250 ms | `for`, `phase`, `rows?`, `bytes?` |
| `notice` | the engine reports something (an index built, a setting adjusted) | `text`, `for?` |
| `catalog` | the catalog changed | — |
| `graph` | the open graph changed | `by: client`, `nodes`, `edges`; with a `ugr` part, a delta |
| `activity` | another client ran something (§9) | `client`, `op`, `summary` |

The `graph` and `activity` events matter once a second client is attached. With one client they are sent and can be ignored.

### 4.5 Ordering and concurrency

- **One request runs at a time per session.** Others queue in arrival order, across all clients. The engine is not re-entrant, and a single order is what makes undo, history and paired work make sense.
- **`cancel` and `hello` bypass the queue.** Natively, `memory` does too. `cancel {target}` names one of the same connection's requests, queued or running; it replies `cancelled`, and a request that has already replied makes `cancel` a no-op, not an error. A closed connection cancels what it had queued or running. Cancelling drops the request's future, which is DataFusion's cancellation; a graph kernel that does not yield finishes first. *(URSA-3.)*
- **The reply to a request comes after every event it caused.**
- **The browser demo's worker keeps the same order** as today.

### 4.6 Versioning

`hello`'s reply carries `protocol: 1` and a `capabilities` list, for example `cancel`, `sources`, `arrow`, `multi-client`. Additions are new ops, new optional fields or new events, all marked by a new capability. A change that breaks a client increments `protocol`, and the server refuses a client that asks for a version it doesn't speak. The browser demo's worker reports `transport: "wasm"` and has no `cancel` and no `sources`.

### 4.7 Why not something else

- **Plain HTTP requests:** progress, cancel, and pushing another client's activity all need a channel the server can write to. HTTP would need a second, streaming channel for those anyway.
- **Arrow Flight SQL:** browsers can't speak gRPC without a proxy, and it has no notion of graphs, the open graph, or events. A Flight SQL endpoint for BI tools could be added to `ursa-server` later, beside this protocol.
- **Server-rendered graphs:** the graph view's interactivity (picking, camera, aggregation as you zoom) needs the scene in the browser, and the layout already runs well there.

## 5. `ursa serve`

```
ursa serve [DATABASE | DIRECTORY] [--port N] [--no-open] [--read-only]
```

- **The argument** is a database file, opened as the CLI opens one, or a directory. Relative paths in queries resolve against it, and the sources panel starts there. With no argument, it is an in-memory session rooted at the current directory.
- **The port** is a fixed default (proposed 4280), so bookmarks work. If it's taken, the server uses the next free port and says so.
- **Startup** prints the URL with its token and opens it in the default browser unless `--no-open`. Ctrl+C stops the server and closes the session.
- **The pages:** the console at `/console/`, which the printed link opens, and the web shell at `/`, where the demo's addresses and share links have it. *(Decided in URSA-12: the console as the index would have moved the shell, its share links and its e2e suite; a path of its own moves nothing.)*
- **`--read-only`** refuses statements that write: `COPY TO`, `CREATE` in an attached database, `INSERT`, and the rest. It is meant for pairing with an agent (§9).
- **Sessions:** `hello {session: "new"}` (the shell's choice) gives a tab a session of its own; `hello {session: "shared"}` (the default) attaches it to the server's one shared session, which every tab and agent that asks for it sees (§9). A shared session's requests from every connection run in one queue. *(Changed by the spike, #536, and URSA-3: one session per process did not fit the shell, whose `start` replaces its session.)*
- **Ctrl+C stops it gracefully:** no new connections, every running request cancelled, every WebSocket closed (code 1001, "the server stopped"), and the sessions ended.

### Security

The native engine reads any file and any bucket the user's credentials reach, so the server must be reachable only by the user's own browser tab.

- **Bind to `127.0.0.1` and `::1` only.** There is no flag to bind elsewhere in v1.
- **A token:** 32 random bytes, base64url, made at startup, in the printed link (`/console/?token=…`; either page takes it). The server takes it from the URL once: it answers with an `HttpOnly`, `SameSite=Strict` cookie named for its port (`ursa_token_4280`; cookies are shared across a host's ports) and a redirect to the same address without it, so the token never reaches the page's scripts or its share links. The WebSocket (`/session`) and the served directory's files (`/data/`, with byte ranges) need it: the cookie, or `?token=` for clients that are not browsers. The page's own files need no token: they hold no data. *(Changed by the spike, #536: it was kept in `sessionStorage`.)*
- **`Origin` must be the server's own,** `http://127.0.0.1:PORT` or `http://localhost:PORT`, on every WebSocket upgrade. This stops other sites' pages from connecting, even ones that learned the token.
- **`Host` must be `127.0.0.1:PORT` or `localhost:PORT`.** This stops DNS rebinding.
- **The same security headers as the deploy:** `web/src/headers.ts` (a CSP with `connect-src 'self'`, COOP, COEP and CORP). The console works cross-origin isolated, as the shell does.
- **Credentials stay in the process.** `sources.list` shows names and sizes, never credentials or signed URLs.

### Shipping the UI

The UI's files (about 3–4 MB, most of it the graph view's wasm) are embedded in the binary with `rust-embed`, behind `ursa-server`'s `embed` feature (ursa-cli's `serve-embed`), built by `bun run build` before `cargo build`; the wasm engine's files are left out. Release binaries and the Homebrew formula have it on (URSA-4). *Decided by the spike (§12, question 1):* the published crates ship no assets. `ursa-server` is published without them, and `ursa-cli`'s `serve` feature, from crates.io, serves the page from a directory (`--assets DIR`, a `web/dist` its user built). `ursa-server` is published with the other crates, before `ursa-cli`: `cargo publish` of `ursa-cli` fails while an optional dependency is not on crates.io, even with the feature off.

## 6. The console

The console is what the shell can't be: several results, a real editor, panels.

```
┌───────────┬───────────────────────────────────────────────┬────────────────┐
│ Sources   │ editor (SQL, with completion and highlight)   │ Inspector      │
│  files    ├───────────────────────────────────────────────┤  the selected  │
│  buckets  │ result tabs:  Table │ Graph │ Plan            │  node's or     │
│  attached │                                               │  edge's row    │
│ Catalog   │  table: paged, sortable, typed                │                │
│  tables   │  graph: the graph view, with its legend,      ├────────────────┤
│  graphs   │         search, aggregation, expand           │ History        │
│           │                                               │  queries and   │
│           │                                               │  graph steps   │
└───────────┴───────────────────────────────────────────────┴────────────────┘
```

- **A result opens as a table,** and as a graph when its columns say it can be one, by the Phase 1 rules.
- **The graph pane is the shell's graph view,** with the same keys, inspector and exploration. Its commands (`.expand 33`) appear in History, not a terminal, and replay like the shell's share links.
- **History lists queries and graph steps,** each with who ran it (§9).
- **The sources panel lists files and buckets.** Clicking a file inserts `SELECT * FROM 'path' LIMIT 100`, and clicking a graph inserts `.graph name`.
- **The editor is CodeMirror 6.** It's small and modular, and the `complete` and `highlight` operations feed it.

The console is plain JavaScript modules served as they are, like the shell, with no framework. The spike's console page is far smaller than this sketch (§11).

**The frame (URSA-12).** `web/console/`: `console.html` (the page), `layout.js`, `session.js` and `console.js`, with `console.css` over the shell's `style.css` (its tokens, fonts, bar and theme button) and `graph-pane.css`. `bun run build` copies them, with apache-arrow bundled as one module, into `web/dist-console/console/`: a directory of its own, so the deployed `web/dist/` gains nothing, which `ursa serve` serves ahead of `web/dist/` (and embeds beside it).

- **Panels:** each panel's body is a mount point (`window.ursaConsole.panels.editor` and the rest) labelled with its issue: the editor (URSA-13), the result tabs (URSA-14), the catalog (URSA-15), the sources (URSA-16) and History (URSA-17); the inspector's is not yet assigned. Splitters (`role="separator"`) resize the side columns and share each column between its two panels, by pointer or arrow keys; a panel's title folds it; the header's buttons show and hide the side columns, which are drawers under 1024px. Skip links and F6 move between panels. Sizes are kept in the browser.
- **The session:** `session.js` holds one engine client on the shared session, and connects again when it goes (after 0.5 s, then longer, up to every 10 s), saying why when it can tell: the server is not running (no answer), or it was restarted (its `/data/` answers 401 to this page's token, so its new link must be opened). Later panels listen with `session.on("connect", fn)`, for every (re)connection, and `session.onEvent(event, fn)`.
- **The status bar:** the connection, the engine (`hello`'s `version` and `transport`), the session's root (`session.cwd`), and the console's running request (the oldest it waits for) with its time and Cancel, which sends `cancel {target}` (`engine-client.js`'s `cancel(reply)`) where `hello`'s capabilities list `cancel`.

## 7. The web shell

The shell stays the demo of the CLI: a real engine in a tab, with the tour, share links and sample datasets.

- **Its graph view is frozen.** It keeps working and keeps its e2e tests, but new exploration features go into the console.
- **It moves onto `engine-client.js` in C1,** so it runs over either transport. That is also how the spike tests the protocol: today's e2e suite, unchanged, run against `ursa serve`.
- **Once the console runs in the browser demo (C4),** the demo site serves both. Then we decide whether the shell's graph view stays, as the quick look, or goes, leaving the shell a pure CLI demo.

Removing it sooner would lose the only place the graph view is deployed and tested, before its replacement exists.

## 8. Data sources

| source | browser demo | `ursa serve` |
|---|---|---|
| Sample datasets | R2 over HTTP | the same URLs, or local copies |
| Local files | uploads into `memory://uploads` | the disk, by path |
| `s3://` (and R2), `gs://`, `az://` | no (no wasm backend, #278) | yes: `object_store` with the CLI's credential chain |
| Ursa database files | uploads | open or `ATTACH` by path |
| Postgres and other databases | no | later: a connector, credentials in a local config file |

`sources.list` on a bucket prefix lists objects with `object_store`'s `list_with_delimiter`, the same call the engine uses to read a prefix.

## 9. Agents

The protocol makes an agent a second client of the same session. That gives the human and the agent equal seats.

- **C3, without new protocol:**
  - **The skill doc,** which starts from `ursa-cli/src/ursa-sql-skill.md`.
  - **Agent-friendly CLI help:** `ursa --help-agent` prints a short orientation; dot-command help can be read as JSON; and errors name the fix.
  - **An MCP server in `ursa-server`,** speaking MCP to the agent and the session protocol to the session. Its tools:
    - `query`, which returns a summary, not the rows: columns, row count, a 20-row sample, and for graphs the node and edge counts, degree spread and component sizes;
    - `catalog`, `sources`, `graph_open` and `graph_explore`;
    - `graph_snapshot`, a PNG of the graph view as the human sees it, through the view's offscreen read-back (`ursa-viz`'s snapshot), which the page sends on request;
    - `selection`, the nodes the human has selected.
- **C4, paired mode:**
  - The agent's requests run on the human's session. They appear in the console's History and the graph pane as they happen (the `activity` and `graph` events), marked as the agent's.
  - The human can pause the agent's queue, and the session can be `--read-only`.
  - `explorer.selection` (graph-explorer.md §4, #447) lets the agent query what the human picked: "why are these five connected?"

## 10. Phases

| phase | contents | done when |
|---|---|---|
| **Spike** | §11 | its report says go or no-go, with numbers |
| **C1** | `ursa_shell::session` with the protocol's ops; `ursa-wasm` on it; `engine-client.js` with both transports; the shell on it; `ursa-server` with the security model; `ursa serve` with the shell's UI | The e2e suite passes on both transports in CI; the browser demo is unchanged |
| **C2** | The console UI: editor, results, catalog, sources, the graph pane, history; `query`, `fetch`, `catalog` and `sources` ops; object storage | A user can open a bucket of Parquet files, query them, and explore the result as a graph, all in the console |
| **C3** | The skill doc; agent CLI help; MCP in `ursa-server` | An agent with only the skill doc and MCP can load a dataset, run PageRank and send the human a graph snapshot |
| **C4** | Paired mode; the console in the browser demo over the wasm transport | A human and an agent work in one session, each seeing the other's steps |

## 11. The spike

Time-boxed to a week. It answers whether this architecture holds, before C1 commits to it.

1. **Extract `Session` into `ursa-shell`,** with just enough of the ops for the shell: `feed`, `graph.*`, `export`, `files`. `ursa-wasm` calls it, and the browser demo's e2e still passes.
2. **Add `engine-client.js` with both transports,** and port `app.js` to it.
3. **Build a minimal `ursa-server`:** axum, the WebSocket framing of §4.2, the session actor, the token, `Origin` and `Host` checks, and the static `web/dist`. `ursa serve DIR` runs the shell's UI against a native session.
4. **Run the existing e2e suite against `ursa serve`.** It must pass, apart from tests of browser-only features: uploads into the wasm session and `memory`.
5. **Build a bare console page** (no editor beyond a `<textarea>`): `query` with an Arrow IPC part, rendered as a table with its column types; and a "Graph" button that opens the result in the existing graph pane.
6. **Measure, on the 1M-edge R-MAT graph, worker against socket:**
   - `.graph rmat`: the time from Enter to the first frame;
   - `query` round trips for 1, 1,000 and 100,000 rows;
   - the bytes on the wire;
   - the server's memory.
7. **Write a report** (`design/console-spike.md`): the numbers, what didn't fit the protocol, the answers to §12, and C1's issue list.

## 12. Open questions

1. **Shipping the UI:** prebuilt assets inside the published crate, or `serve` only in release binaries? The spike measures the crate's size each way.
2. **The name:** `ursa serve` or `ursa ui`. DuckDB uses `-ui`, and `serve` says what it does. This design uses `serve`.
3. **Results kept per client:** 16 results and 256 MB are guesses. The spike measures real result sizes in the console.
4. **The graph view's coupling to the terminal:** how much of `graph.js` (2,336 lines) assumes a terminal. The spike finds out when it adds the console's graph pane.
5. **Windows:** the WebSocket, the browser opening and the paths need a CI job. When?

## 13. Risks

- **Two transports drift apart.** The mitigation is one e2e suite run on both in CI, from C1.
- **The graph view's coupling to the shell's page.** It may cost more to separate than the spike shows. The mitigation is the spike's console page, which reuses the graph pane in a second host.
- **Security mistakes in a local server** expose the user's files and credentials. The mitigation is the checks in §5, tested in CI with a request from a foreign `Origin`, a wrong `Host` and a missing token.
- **Scope.** A console invites features. The phases end at C4, and database connectors, desktop and hosting are out until then.
