# ursa-server

`ursa serve`: the Ursa Console (and the Web Shell) in a browser tab, over a
WebSocket to a native engine session in the `ursa` process. It is the native end of the session
protocol the browser demo speaks to its wasm engine; the design is
[`design/console.md`](https://github.com/cldixon/ursa/blob/main/design/console.md)
§3–§5.

```sh
cd web && bun run engine && bun run build          # web/dist and web/dist-console
cargo run --release -p ursa-cli --features serve -- serve web/.data
# Ursa is serving /…/web/.data at
#   http://127.0.0.1:4280/console/?token=…
```

As a library:

```rust
let config = ursa_server::Config {
    root: ".".into(),
    port: ursa_server::DEFAULT_PORT,
    assets: ursa_server::Assets::Dirs(vec!["web/dist-console".into(), "web/dist".into()]),
    version: "0.13.0".into(),
};
let bound = ursa_server::bind(config).await?;
println!("{}", bound.url());
// Until Ctrl+C, then gracefully.
bound.serve_with_shutdown(async { tokio::signal::ctrl_c().await.ok(); }).await?;
```

## The protocol

- **Framing** (§4.2): a message is a JSON text frame, then one binary frame
  per part it lists, each after an 8-byte header (request id `u32`, part
  index `u16`, two reserved zero bytes, little-endian). Requests and replies
  are framed alike. `web/public/socket-transport.js` is the other end.
- **Sessions:** the first request is `hello {protocol: 1}`. `hello {session:
  "new"}` gives the connection a session of its own, as the shell wants a
  session per tab; `hello {session: "shared"}`, the default, attaches it to
  the server's one shared session, which every such connection sees, as the
  console and agents want. `hello`'s result names it: `session: {kind, id,
  cwd}`. The shared session's later `hello`s are answered at once, not
  behind its running request.
- **The session actor:** each session is a thread of its own that owns an
  `ursa_shell::session::Session` (`Handle::block_on` of the server's
  multi-threaded runtime, inside a `LocalSet`, so DataFusion's partitions
  still run on every core). Requests reach it over a channel and run one at
  a time, in the order they came, from every connection attached to it.
- **`cancel {target}`** (native only) and `memory` bypass the queue: the
  server answers them itself.
  `cancel` stops one of the connection's own requests, queued or running,
  which then replies `cancelled`: its future is dropped, which is
  DataFusion's cancellation. A graph kernel that does not yield runs to its
  end first. A closed connection cancels what it had queued or running.
- **Errors** are the protocol's kinds: `sql` and `engine` from the session,
  `protocol` for a bad request (another version, no `hello` first, a part
  that does not match its listing), `cancelled`, and `crash` when the
  session's engine stopped (the page then starts a new one). A request whose
  framing is lost is answered, and the connection closed (code 1002).
- **Stopping:** `Bound::serve_with_shutdown` stops listening, cancels every
  running request, closes each WebSocket (code 1001, "the server stopped")
  and ends the sessions, waiting up to five seconds for them.

## Security

The native engine reads any file and any bucket the user's credentials
reach, so only the user's own browser tab may reach it (§5):

- it listens on `127.0.0.1` and `::1` only;
- a token, 32 random bytes made at startup, is in the printed link. A
  page (`/console/` or `/`) takes it once: it answers with an `HttpOnly`, `SameSite=Strict` cookie
  named for the port (`ursa_token_4280`; cookies are shared across a host's
  ports) and a redirect to the same address without it, so it never ends up
  in a shared link. The WebSocket and `/data/` need it: the cookie, or
  `?token=` for clients that are not browsers;
- every request's `Host` must be one of the server's own names
  (`127.0.0.1:PORT`, `localhost:PORT`, `[::1]:PORT`), against DNS
  rebinding, and every WebSocket upgrade's `Origin` the server's own page;
- every response has the deploy's security headers (`src/headers.rs`, which
  a test checks against `web/src/headers.ts`).

## Files

- **The pages** need no token: they hold no data. The console is at
  `/console/` (`web/dist-console/console/`), and `Bound::url()`, the link
  `ursa serve` prints, opens it when the assets have it; the Web Shell is at
  `/` (`web/dist`), where the demo's addresses and share links have it. Both
  are built by `bun run build`, the console into a directory of its own so
  the deployed `web/dist` has none of it. With the `embed` feature both are
  built into the binary (`rust-embed`, without the wasm engine's files; a
  build without `web/dist-console` serves the shell alone), which builds
  only from a checkout after `bun run build`: the published crate has no
  assets. Without it, `Assets::Dirs` (`ursa serve --assets DIR`, repeated:
  `web/dist-console`, then `web/dist`) serves them from directories. Each
  page is served with `<meta name="ursa-transport" content="socket">`, which
  makes it talk to the server, and takes the token from its address as `/`
  does; `/console` redirects to `/console/`.
- **`/data/PATH`** serves a file under the served directory (no `..`, no
  hidden names) to a page with the token, with `HEAD` and a byte range, read
  from the file as it is sent. Relative paths in queries resolve against
  that directory too (the process's working directory, which `ursa serve`
  sets to it).

## Tests

`cargo test -p ursa-server` runs the security tests (`tests/security.rs`: no
token, a wrong token or cookie, a foreign `Origin`, a wrong `Host`, loopback
only, `/data/`) and the protocol's (`tests/protocol.rs`: the shell over a
socket, sessions, `cancel`, stopping). In `web/`, `bun scripts/e2e-serve.ts
PORT` runs the browser e2e suite against `ursa serve`.
