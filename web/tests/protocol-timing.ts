// The session protocol's costs on the R-MAT graph (1M edges), over whichever
// transport the site serves (#536): the wasm engine in a Web Worker under
// `bun run preview`, or `ursa serve`'s native engine over a WebSocket (set
// URSA_SERVE_TOKEN, as scripts/e2e-serve.ts does).
//
//   bun tests/protocol-timing.ts [url] [runs]
//
// In a page of the site it connects a client of its own (engine-client.js,
// the page's default transport), loads rmat, and times, each `runs` times:
// `query` round trips for 1, 1,000 and 100,000 rows of rmat_edges, with the
// bytes on the wire (the reply's JSON and its Arrow part); `graph.open rmat`
// (the engine's graph, as UGR); and the engine's memory after it (`memory`:
// the WebAssembly memory, or the server's resident set). It prints the
// medians as JSON.
import { chromium } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:8788").replace(/\/$/, "");
const runs = Number(Bun.argv[3] ?? 5);
const browser = await chromium.launch();
const ctx = await browser.newContext();
if (process.env.URSA_SERVE_TOKEN) await ctx.addCookies([{ name: `ursa_token_${new URL(base).port}`, value: process.env.URSA_SERVE_TOKEN, url: base }]);
const page = await ctx.newPage();
await page.goto(`${base}/`);
const result = await page.evaluate(async (runs) => {
  const { connect, defaultTransport } = await import("/engine-client.js");
  const client = connect(defaultTransport(), { name: "timing", kind: "agent" });
  const hello = await client.call("hello", {});
  await client.call("start", {});
  for (const line of ["CREATE VIEW rmat_edges AS SELECT * FROM 'rmat_1m_edges.parquet';", "CREATE PROPERTY GRAPH rmat EDGE TABLES (rmat_edges SOURCE KEY (src) DESTINATION KEY (dst));"]) {
    const r = await client.call("feed", { line });
    if (r.err) throw new Error(r.err);
  }
  const median = (xs: number[]) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];
  const out: Record<string, unknown> = { transport: hello.transport };
  for (const rows of [1, 1000, 100000]) {
    const times: number[] = [];
    const engine: number[] = [];
    let bytes = 0;
    for (let i = 0; i < runs; i++) {
      const t0 = performance.now();
      const r = await client.call("query", { sql: "SELECT * FROM rmat_edges", rows });
      times.push(performance.now() - t0);
      engine.push(r.ms);
      const { arrow, ...json } = r;
      bytes = arrow.byteLength + JSON.stringify(json).length;
    }
    out[`query ${rows}`] = { roundTripMs: median(times), engineMs: median(engine), bytes };
  }
  {
    // The whole of the last result (rmat_edges, 1M rows), as the session
    // keeps it: what one kept result costs (design §12, question 3).
    await client.call("query", { sql: "SELECT * FROM rmat_edges", rows: 0 });
    const t0 = performance.now();
    const { bytes } = await client.call("export", { format: "arrow" });
    out["export arrow (1M rows)"] = { roundTripMs: performance.now() - t0, bytes: bytes.byteLength };
    const p = await client.call("query", { sql: "SELECT * FROM pagerank(rmat)", rows: 0 });
    const { bytes: pr } = await client.call("export", { format: "arrow" });
    out["pagerank(rmat)"] = { engineMs: p.ms, rows: p.rows, bytes: pr.byteLength };
  }
  {
    const times: number[] = [];
    let bytes = 0;
    for (let i = 0; i < runs; i++) {
      const t0 = performance.now();
      const g = await client.call("graph.open", { text: "rmat" });
      times.push(performance.now() - t0);
      bytes = g.ugr.byteLength;
    }
    out["graph.open rmat"] = { roundTripMs: median(times), bytes };
  }
  out.memory = await client.call("memory", {});
  client.close();
  return out;
}, runs);
console.log(JSON.stringify(result, null, 2));
await browser.close();
