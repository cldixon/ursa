# Session protocol

The browser shell reaches its engine through the session protocol, version 1. The demo's engine runs in a Web Worker (`ursa-wasm`, over `postMessage`); `ursa serve` runs the same engine natively and carries the same messages over a WebSocket. One dispatch, `ursa_shell::session::Session::handle`, answers both, so a page cannot tell them apart. The design is `design/console.md` §4.

## Messages

Every message is one JSON object, plus zero or more binary **parts**.

A **request**, from a client:

```json
{"v": 1, "id": 17, "op": "query", "args": {"sql": "SELECT * FROM pagerank(karate) LIMIT 5", "rows": 1000},
 "parts": []}
```

A **reply**, exactly one per request, with the request's `id`:

```json
{"id": 17, "ok": true, "result": {"result": "r4", "columns": [...], "metadata": {}, "rows": 5, "more": false, "ms": 12, "notices": []},
 "parts": [{"name": "arrow", "kind": "arrow", "bytes": 1832}]}
```

An **error** reply:

```json
{"id": 17, "ok": false, "error": {"kind": "sql", "message": "column \"pagernk\" not found"}}
```

The error kinds:

| kind | meaning |
|---|---|
| `sql` | the statement is wrong (most engine errors are planning errors, "no graph is open" among them) |
| `engine` | it failed while running |
| `cancelled` | `cancel` stopped it |
| `not_found` | a result or graph that has gone |
| `protocol` | a bad request: an unknown op, a missing argument, a request before `hello`, a version the session does not speak |
| `crash` | the engine is in an unknown state (a WebAssembly trap, a closed socket): start a new session |

An **event**, from the session, unprompted: `{"event": "catalog"}`.

## Parts

A part is a binary payload of a request or a reply. The JSON lists them in order, each with a `name`, a `kind` and its length in `bytes`:

| kind | holds |
|---|---|
| `ugr` | a `GraphResult` or a `GraphDelta` (`ursa-viz/UGR.md`) |
| `arrow` | an Arrow IPC stream: the schema, with its `ursa.*` metadata, then the record batches |
| `f32`, `u32` | a bare little-endian array |
| `bytes` | a file |

The name says which part is which: a reply can carry two parts of one kind, and some parts are there only sometimes (`graph.open`'s `positions`, `graph.explore`'s `delta`). A client that reads the reply as one object puts each part under its name; `engine-client.js` gives `ugr` and `bytes` parts as a `Uint8Array`, `f32` as a `Float32Array` and `u32` as a `Uint32Array`.

Requests carry parts the same way: `addFile` sends its file as one.

Over `postMessage`, a part's `bytes` is the `ArrayBuffer` itself, transferred. Over the WebSocket, the JSON goes in a text frame, with each part's length in `bytes`, and each part follows in its own binary frame, in order, after an 8-byte header:

| offset | size | what |
|---|---|---|
| 0 | 4 | the request's `id`, `u32` little-endian (for a reply too) |
| 4 | 2 | the part's index in `parts`, `u16` |
| 6 | 2 | reserved, zero |
| 8 | … | the part's bytes |

A message is complete when its text frame and every part it lists have arrived.

## Results

`query` keeps the result it gives, by an id such as `"r4"`, so that a client can keep several open: `fetch` pages through one, `export` writes it, `graph.open` draws it, and `close` drops it. Each session keeps its last 16 results, within 256 MB, and drops the oldest first; ids are not reused. A result over 256 MB on its own keeps only the rows `query` sent; `fetch` past them, and `export` or `graph.open` of it, fail with `not_found` and say to run the query again, as they do for a result that was closed or dropped. A result's size is what its Arrow buffers hold, each counted once: 1M rows of two `Int64`s is 16 MB.

The shell's last result is apart from these: `feed`'s statements and `query` both set it, and `export` and `graph.open` without a `result` use it, as `.export` and `.graph` do.

## Order

One request runs at a time, in arrival order. A reply comes after every event its request caused. `cancel` and `hello` go ahead of the queue on a host that can cancel (capability `cancel`); the browser demo cannot.

## Versions

The first request is `hello`, with `protocol: 1`; the session refuses any other request before it, and refuses a `protocol` it does not speak, with a `protocol` error. Additions (new ops, new optional fields, new events) keep the version and add a capability to `hello`'s `capabilities`; a change that breaks a client increments it.

`hello`'s `session` asks for a session: `new` for one of the client's own (the shell, whose `start` replaces its session), `shared` for the one the console and agents share. The browser demo has one session per page either way.

## Operations

The table is generated from `ursa_shell::session::OPS`, and a test keeps the two equal; another runs each listed operation through `Session::handle`.

<!-- ops: generated from ursa_shell::session::OPS -->
| op | args | result; parts (kind) | what it does |
|---|---|---|---|
| `hello` | `protocol`, `client?: {name, kind}`, `session?: "new" \| "shared"` | `{protocol, version, transport, capabilities, session: {kind}}` | Must be the first request: any other before it fails with `protocol`. Refuses a `protocol` other than 1. `session` asks the host for a session of its own (`new`, the shell's) or the one it shares (`shared`, the default: the console's and agents'). |
| `cancel` | `target` | `{}` | Stops the running request `target`, which then fails with `cancelled`. A host with capability `cancel` answers it ahead of the queue; a request that reaches the session has nothing running, so it answers `{}`. |
| `start` | `commands?`, `shortcuts?`, `graphMaxEdges?` | `{version, prompt, promptColor}` | A fresh shell, with the host's dot-commands (`[[name, args, help]]`), its `.help shortcuts` text and its default `ursa.graph_max_edges`. |
| `feed` | `line` | `{out, err, flow, command, prompt, continuation, pending}` | Runs one typed line, as the CLI does. `flow` is `continue`, `exit` (`.quit`) or `host` (a host's dot-command, in `command`). |
| `cancelInput` | — | `{prompt}` | Drops a half-typed statement (Ctrl-C at the prompt); `cancel` stops a running request. |
| `width` | `columns` | `{}` | The terminal's width: tables are drawn no wider (0: no limit). |
| `highlightAnsi` | `text`, `alone?` | `{text}` | `text` coloured as the shell colours what is typed: after what is pending, or with `alone`, as a statement of its own. |
| `memory` | — | `{bytes, kind: "wasm" \| "rss"}` | The engine's memory: its WebAssembly memory in the browser, the process's resident set natively (`bytes` is `null` where it cannot be read). |
| `query` | `sql`, `rows?` (1,000) | `{result, columns, metadata, rows, more, ms, notices}`; `arrow` (`arrow`) | Runs `sql` for the console. The last statement's result is kept as `result` (`"r4"`; `null` for a statement with no columns) for `fetch`, `export` and `graph.open`, and becomes the last result too. `columns` are `[{name, type, nullable, metadata}]`, `metadata` is the schema's (`ursa.pattern`), `rows` the whole result's count; the part holds the first `rows` rows as an Arrow IPC stream. The session keeps its last 16 results, within 256 MB, and drops the oldest first; a result over 256 MB on its own keeps only the rows the reply sent. |
| `fetch` | `result`, `offset?` (0), `rows?` (1,000) | `{rows, more}`; `arrow` (`arrow`) | A page of a kept result: `rows` rows from `offset`, as an Arrow IPC stream, and whether more follow. `not_found` for a result closed or dropped, or for rows past those a result over the cap kept: run the query again. |
| `close` | `result` | `{}` | Drops a kept result; `not_found` if it has gone already. |
| `addFile` | `name`; the file as the request's one part | `{url}` | Puts a file in the session's files, at `memory://uploads/`. |
| `files` | — | `{files: [{name, url, bytes}]}` | The session's files: added ones, and those `COPY … TO 'name'` wrote. |
| `export` | `format`, `result?` | `{}`; `bytes` (`bytes`) | A kept result, or without `result` the last result, every row, as `csv`, `json`, `arrow` or `parquet`. `not_found` for a result that has gone, or that kept only its first rows. |
| `graph.open` | `text?`, `result?` | `{skipped}`; `ugr` (`ugr`), `positions?` (`f32`) | Opens a graph to draw (`.graph TEXT`). A `text` that names no source draws the kept `result`, or without it the last result. `positions`, every `x` then every `y`, only with `POSITIONS`. |
| `graph.explore` | `command` | `{out, err, nodes, edges, delta: null}`; `delta?` (`ugr`) | Runs `.expand`, `.hide`, `.show`, `.undo`, `.redo` or `.graph history` on the open graph; `delta` only when the graph changed. |
| `graph.find` | `command` | `{out, err, matches, notShown, notShownTotal}`; `selected` (`u32`) | Runs `.find`: the matches shown, as node slots, the best first. |
| `graph.notShown` | — | `{}`; `counts` (`u32`) | For each node slot, its edges to nodes not shown: the view's `+N`. |
| `graph.inspect` | `node, key, neighbours?` or `edge, src, dst` | inspect's JSON, with `ms` | What the graph's tables say of a node or an edge. |
| `graph.close` | — | `{}` | Closes the open graph. |
<!-- /ops -->
