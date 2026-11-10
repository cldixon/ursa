// The engine, in a Web Worker so a long query never freezes the page.
//
// It loads the build engine.json names and keeps named sessions:
//
// - "shell": the terminal's. It starts empty; the Datasets menu defines tables
//   and graphs in it by running their SQL at the prompt.
// - "meta": the page's own lookups (`.functions NAME`), so they never replace
//   the results `.export` writes.
//
// Messages are {id, op, ...}; replies are {id, ok: true, ...} or {id, ok: false, error}.
//   run     {session, sql, maxRows} -> {results, ms}  (see Session.run in ursa-wasm)
//   export  {session, index, format} -> {bytes}       (the last run's result `index`)
//   addFile {session, name, bytes}  -> {url}          (memory://uploads/<name>)
//   reset   {session}               -> {}             (a fresh session)
import init, { Session, version } from "./engine/ursa_wasm.js";

const sessions = new Map();

function get(name) {
  let session = sessions.get(name);
  if (!session) {
    session = new Session();
    sessions.set(name, session);
  }
  return session;
}

const ready = (async () => {
  const t0 = performance.now();
  const engine = await (await fetch(new URL("./engine/engine.json", import.meta.url))).json();
  await init({ module_or_path: new URL(engine.wasm, self.location.origin) });
  // The base URL is shared by every session: relative paths are dataset files.
  new Session().setBaseUrl(new URL("/data/", self.location.origin).href);
  get("shell");
  return { version: version(), ms: performance.now() - t0 };
})();

ready.then(
  ({ version, ms }) => postMessage({ ready: true, version, ms }),
  (e) => postMessage({ ready: false, error: String(e?.message ?? e) }),
);

// One request at a time, in order: a session runs one statement list at once.
let queue = Promise.resolve();

onmessage = ({ data }) => {
  queue = queue.then(() => handle(data));
};

async function handle({ id, op, ...args }) {
  try {
    await ready;
    if (op === "run") {
      const t = performance.now();
      const results = JSON.parse(await get(args.session).run(args.sql, args.maxRows ?? 40));
      postMessage({ id, ok: true, results, ms: performance.now() - t });
    } else if (op === "export") {
      const bytes = get(args.session).export(args.index, args.format);
      postMessage({ id, ok: true, bytes }, [bytes.buffer]);
    } else if (op === "addFile") {
      const url = await get(args.session).addFile(args.name, new Uint8Array(args.bytes));
      postMessage({ id, ok: true, url });
    } else if (op === "reset") {
      sessions.get(args.session)?.free();
      sessions.delete(args.session);
      get(args.session);
      postMessage({ id, ok: true });
    } else {
      throw new Error(`unknown op ${op}`);
    }
  } catch (e) {
    postMessage({ id, ok: false, error: String(e?.message ?? e) });
  }
}
