# ursa-server

`ursa serve`: Ursa's shell page (and the console spike's page) in a browser
tab, over a WebSocket to a native engine session in the `ursa` process. The
spike of #536; the design is `design/console.md`, and what the spike found is
`design/console-spike.md`.

```sh
cd web && bun run engine && bun run build && bun run console   # the pages
cargo run --release -p ursa-cli --features serve -- serve web/.data
# Ursa is serving /…/web/.data at
#   http://127.0.0.1:4280/?token=…
```

- **The protocol** is `ursa_shell::session::Session`'s, the same code the
  browser demo runs in its Web Worker: a JSON text frame, then a binary frame
  per part, each after an 8-byte header (request id `u32`, part index `u16`,
  two reserved bytes, little-endian). `web/public/engine-client.js`'s
  `SocketTransport` is the other end.
- **The session actor:** each WebSocket gets a session of its own, on a
  thread of its own (`Handle::block_on` of the server's multi-threaded
  runtime, inside a `LocalSet`); requests run one at a time.
- **Security:** it listens on `127.0.0.1` and `::1` only. The link's token
  becomes an HTTP-only, `SameSite=Strict` cookie (`ursa_token_PORT`) and
  leaves the address; the WebSocket and `/data/` need it (the cookie, or
  `?token=`). Every request's `Host` must be the server's, and every
  upgrade's `Origin`. Every response has `web/src/headers.ts`'s headers
  (`src/headers.rs`, checked against the TypeScript by a test).
- **Files:** the page's files from `--assets` (by default this checkout's
  `web/dist-console` and `web/dist`), and `/data/NAME` from the served
  directory. Relative paths in queries resolve against that directory (the
  process's working directory).

`cargo test -p ursa-server` runs the security tests (no token, a wrong token
or cookie, a foreign `Origin`, a wrong `Host`, loopback only) and the shell
over a socket. `bun scripts/e2e-serve.ts` (in `web/`) runs the browser e2e
suite against it.
