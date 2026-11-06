// The engine, in a Web Worker so a long query never freezes the page.
//
// It loads the build engine.json names and keeps named sessions, each with its
// own views and graphs:
//
// - "workspace": the editor's session, with the sample graphs defined at start;
// - "tour": the Tour's, which starts empty, as the Getting Started page does;
// - "meta": for the page's own lookups (the function reference), so they never
//   replace the results the user is looking at.
//
// Messages are {id, op, ...}; replies are {id, ok: true, ...} or {id, ok: false, error}.
//   run    {session, sql, maxRows} -> {results, ms}   (see Session.run in ursa-wasm)
//   export {session, index, format} -> {bytes}         (the last run's result `index`)
//   reset  {session}                -> {}              (a fresh session)
import init, { Session, version } from "./engine/ursa_wasm.js";

// The workspace's graphs, over Parquet in the bucket (see /data/manifest.json).
const SETUP = `
CREATE VIEW karate_members AS SELECT * FROM 'karate_nodes.parquet';
CREATE VIEW karate_ties    AS SELECT * FROM 'karate_edges.parquet';
CREATE PROPERTY GRAPH karate
  VERTEX TABLES (karate_members KEY (id))
  EDGE TABLES (
    karate_ties SOURCE KEY (src) REFERENCES karate_members (id)
                DESTINATION KEY (dst) REFERENCES karate_members (id)
  );
CREATE VIEW lesmis_edges AS SELECT * FROM 'lesmis_edges.parquet';
CREATE PROPERTY GRAPH lesmis EDGE TABLES (lesmis_edges SOURCE KEY (src) DESTINATION KEY (dst));
CREATE VIEW florentine_edges AS SELECT * FROM 'florentine_edges.parquet';
CREATE PROPERTY GRAPH florentine EDGE TABLES (florentine_edges SOURCE KEY (src) DESTINATION KEY (dst));
CREATE VIEW rmat_edges AS SELECT * FROM 'rmat_1m_edges.parquet';
CREATE PROPERTY GRAPH rmat EDGE TABLES (rmat_edges SOURCE KEY (src) DESTINATION KEY (dst));
`;

const sessions = new Map();

async function open(name) {
  const session = new Session();
  if (name === "workspace") await session.run(SETUP, 0);
  sessions.set(name, session);
  return session;
}

async function get(name) {
  return sessions.get(name) ?? open(name);
}

const ready = (async () => {
  const t0 = performance.now();
  const engine = await (await fetch(new URL("./engine/engine.json", import.meta.url))).json();
  await init({ module_or_path: new URL(engine.wasm, self.location.origin) });
  // The base URL is shared by every session: relative paths are dataset files.
  new Session().setBaseUrl(new URL("/data/", self.location.origin).href);
  await open("workspace");
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
      const session = await get(args.session);
      const t = performance.now();
      const results = JSON.parse(await session.run(args.sql, args.maxRows ?? 1000));
      postMessage({ id, ok: true, results, ms: performance.now() - t });
    } else if (op === "export") {
      const session = await get(args.session);
      const bytes = session.export(args.index, args.format);
      postMessage({ id, ok: true, bytes }, [bytes.buffer]);
    } else if (op === "reset") {
      sessions.get(args.session)?.free();
      sessions.delete(args.session);
      await get(args.session);
      postMessage({ id, ok: true });
    } else {
      throw new Error(`unknown op ${op}`);
    }
  } catch (e) {
    postMessage({ id, ok: false, error: String(e?.message ?? e) });
  }
}
