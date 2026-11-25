# ursa-wasm

Ursa's SQL engine in the browser: `ursa-sql` compiled to WebAssembly, with a
small JavaScript API. Part of #277; the Ursa Shell in `web/` is built on it.

```js
import init, { Session, version } from "./ursa_wasm.js";
await init();
const session = new Session();
// Relative paths in SQL resolve against this URL (the browser has no current
// directory): this session's root, and the page's, which later sessions take.
session.setBaseUrl(new URL("data/", location.href).href);
console.log(await session.sql("SELECT * FROM 'karate_edges.csv' LIMIT 3"));
```

- `session.sql(text)` runs one or more statements and resolves to every result
  printed as the `ursa` shell prints it (box tables). It rejects with the
  shell's error message.
- `session.sqlArrow(text)` resolves to the last result as an Arrow IPC stream
  (`Uint8Array`).
- `version()` is the engine's version.

`WebShell` is the `ursa` shell itself (crate `ursa-shell`), for a terminal on
the page such as xterm.js, as DuckDB's web shell is DuckDB's CLI. It speaks
the session protocol (`design/console.md` §4): every operation goes through
`handle(op, args, parts)`, which runs `ursa_shell::session::Session`, the
same code `ursa serve` runs natively over a WebSocket:

```js
import init, { WebShell } from "./ursa_wasm.js";
await init();
// The page's root: a shell made after this resolves relative paths against it.
WebShell.setBaseUrl(new URL("data/", location.href).href);
const shell = new WebShell();
await shell.handle("hello", JSON.stringify({ protocol: 1, session: "new" }), []); // first, always
const { message } = await shell.handle("feed", JSON.stringify({ line: "SELECT 42 AS answer;" }), []);
const { result } = JSON.parse(message); // {ok, result} or {ok: false, error: {kind, message}}
term.write((result.out + result.err).replaceAll("\n", "\r\n")); // ANSI colour, as in a terminal
```

- `args` is a JSON object as text, and `parts` an array of `Uint8Array`s (a
  file for `addFile`). It resolves to `{message, parts}`: the reply as JSON
  text, and its binary parts (UGR graphs and deltas, `f32` and `u32` arrays,
  exported files), each with a buffer of its own to transfer.
- The first operation is `hello` with `protocol: 1`; any other before it,
  and a `hello` of another version, fail with kind `protocol`.
- The operations are `hello`, `cancel`, `start` (a fresh session, with the
  page's own dot-commands), `feed` (one typed line: `flow` is `continue`,
  `exit` or `host`), `cancelInput`, `width`, `highlightAnsi`, `memory`
  (`{bytes, kind: "wasm"}`, the WebAssembly memory), `query` (rows as Arrow
  IPC), `addFile`, `files`, `export`, and the graph view's `graph.open`,
  `graph.explore`, `graph.find`, `graph.notShown`, `graph.inspect` and
  `graph.close`: [the protocol's reference](../docs/reference/protocol.md),
  generated from `ursa_shell::session::OPS`.

Files are read over HTTP with range requests. A server must answer `HEAD` and
single `Range: bytes=a-b` requests (206 with `Content-Range`).

## How it runs

Everything runs on one thread, with no Tokio runtime: the browser's event loop
drives the engine's futures. In the wasm32 build:

- the kernels take `ursa-core`'s serial path, whose results are bit-identical to
  the parallel one;
- blocking work runs inline (`ursa_engine::run_blocking`);
- plans have one partition (`ursa_engine::serial::SingleTask`), so DataFusion
  never spawns a task;
- `COPY … TO` is written by the engine, not by DataFusion's file sinks, which
  spawn tasks (`ursa_engine::session_files`). A `WebShell` keeps files in
  memory at `memory://uploads/`: `COPY … TO 'name.parquet'` (or `.csv`,
  `.json`, `.arrow`) writes one, `FROM 'name.parquet'` reads it, and
  the `files` operation lists them with the ones `addFile` added. Any other
  target is refused, as is an `INSERT INTO` a table of files; a `Session`
  keeps no files, so its `COPY` is refused: export a result instead;
- `s3://`, `gs://`, `az://` and local paths are not available, nor is the
  `.ursa` database file.

## Build

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked   # must match the wasm-bindgen crate

cargo build -p ursa-wasm --target wasm32-unknown-unknown --profile wasm-release
wasm-bindgen --target web --remove-name-section --remove-producers-section \
  --out-dir pkg target/wasm32-unknown-unknown/wasm-release/ursa_wasm.wasm
```

`wasm-release` is the smallest build (`opt-level = "z"`, fat LTO): about 31 MB
raw, 7.2 MB gzip and 4.7 MB brotli at 0.6.0. `wasm-test` builds in a fraction of
the time, for tests.

## Parity test

`tests/getting_started.sql` is the Getting Started page's SQL as one session.
`tests/browser/parity.ts` runs it in headless Chromium and in the native `ursa`
binary, and requires the outputs to be byte-identical:

```sh
cargo build -p ursa-cli
bun ursa-wasm/tests/browser/parity.ts pkg target/debug/ursa
```

CI runs it on every pull request (the `wasm` job).
