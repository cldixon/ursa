// The engine, in a Web Worker so a long query never freezes the page. It loads
// the build engine.json names, opens one session, defines the sample graphs,
// and answers {id, sql} messages with {id, out, ms} or {id, error}.
import init, { Session, version } from "./engine/ursa_wasm.js";

// Run once at start: the sample graphs, over Parquet files in R2 (see /data/manifest.json).
export const SETUP = `
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
`;

const ready = (async () => {
  const t0 = performance.now();
  const engine = await (await fetch(new URL("./engine/engine.json", import.meta.url))).json();
  await init({ module_or_path: new URL(engine.wasm, self.location.origin) });
  const session = new Session();
  session.setBaseUrl(new URL("/data/", self.location.origin).href);
  await session.sql(SETUP);
  return { session, version: version(), ms: performance.now() - t0 };
})();

ready.then(
  ({ version, ms }) => postMessage({ ready: true, version, ms }),
  (e) => postMessage({ ready: false, error: String(e?.message ?? e) }),
);

onmessage = async ({ data: { id, sql } }) => {
  try {
    const { session } = await ready;
    const t = performance.now();
    const out = await session.sql(sql);
    postMessage({ id, out, ms: performance.now() - t });
  } catch (e) {
    postMessage({ id, error: String(e?.message ?? e) });
  }
};
