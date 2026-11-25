// The engine, in a Web Worker so a long query never freezes the page.
//
// It runs the `ursa` shell itself (WebShell, from ursa-wasm, over
// ursa_shell::session::Session): the page's terminal sends it each typed line
// and writes back what it printed, as a terminal shows the CLI's output.
//
// It speaks the session protocol (design/console.md §4), as `ursa serve` does
// over a WebSocket; engine-client.js's WorkerTransport is its other end.
//
//   request  {v: 1, id, op, args, parts: [{kind, name, bytes: ArrayBuffer}]}
//   reply    {id, ok: true, result, parts: [{kind, name, bytes: ArrayBuffer}]}
//            {id, ok: false, error: {kind, message}}
//
// The parts' buffers are transferred, both ways. The operations are the
// session's (Session::handle in ursa-shell/src/session.rs lists them), and
// one of the worker's own: `memory`, the engine's WebAssembly memory in
// bytes, for graphMemory. A WebAssembly trap (out of memory) fails its
// request with kind `crash`: the page then starts a fresh worker.
//
// Before any request, the worker says it loaded: {ready: true, version, ms},
// or {ready: false, error}.
import init, { WebShell, version } from "./engine/ursa_wasm.js";

let shell = null;
// The engine's wasm exports, once loaded.
let wasm = null;

const ready = (async () => {
  const t0 = performance.now();
  const engine = await (await fetch(new URL("./engine/engine.json", import.meta.url))).json();
  wasm = await init({ module_or_path: new URL(engine.wasm, self.location.origin) });
  // Relative paths are the datasets' files.
  WebShell.setBaseUrl(new URL("/data/", self.location.origin).href);
  return { version: version(), ms: performance.now() - t0 };
})();

ready.then(
  ({ version, ms }) => postMessage({ ready: true, version, ms }),
  (e) => postMessage({ ready: false, error: String(e?.message ?? e) }),
);

// One request at a time, in order: the shell runs one line at once.
let queue = Promise.resolve();
onmessage = ({ data }) => {
  queue = queue.then(() => handle(data));
};

async function handle({ id, op, args = {}, parts = [] }) {
  try {
    await ready;
    if (op === "memory") {
      postMessage({ id, ok: true, result: { bytes: wasm.memory.buffer.byteLength }, parts: [] });
      return;
    }
    shell ??= new WebShell();
    const answer = await shell.handle(op, JSON.stringify(args), parts.map((p) => new Uint8Array(p.bytes)));
    const reply = JSON.parse(answer.message);
    // Each part's bytes, in a buffer of its own: transferred, not copied.
    reply.parts = (reply.parts ?? []).map((p, i) => ({ ...p, bytes: answer.parts[i].buffer }));
    postMessage({ id, ...reply }, reply.parts.map((p) => p.bytes));
  } catch (e) {
    // The engine threw rather than answered: a trap, which leaves it in an
    // unknown state.
    postMessage({ id, ok: false, error: { kind: "crash", message: String(e?.message ?? e) } });
  }
}
