# ursa-wasm

Ursa's SQL engine in the browser: `ursa-sql` compiled to WebAssembly, with a
small JavaScript API. Part of #277; the Ursa Shell in `web/` is built on it.

```js
import init, { Session, version } from "./ursa_wasm.js";
await init();
const session = new Session();
// Relative paths in SQL resolve against this URL (the browser has no current directory).
session.setBaseUrl(new URL("data/", location.href).href);
console.log(await session.sql("SELECT * FROM 'karate_edges.csv' LIMIT 3"));
```

- `session.sql(text)` runs one or more statements and resolves to every result
  printed as the `ursa` shell prints it (ASCII tables). It rejects with the
  shell's error message.
- `session.sqlArrow(text)` resolves to the last result as an Arrow IPC stream
  (`Uint8Array`).
- `version()` is the engine's version.

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
