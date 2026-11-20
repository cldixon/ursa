// Ways to get the edges among a node set (wasm engine, rmat 1M edges).
const { chromium } = require("playwright");
const base = (process.argv[2] ?? "http://localhost:8800").replace(/\/$/, "");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(base + "/404.html");
  const out = await page.evaluate(async () => {
    const engine = await (await fetch("/engine/engine.json")).json();
    const m = await import("/engine/ursa_wasm.js");
    await m.default({ module_or_path: new URL(engine.wasm, location.origin) });
    const s = new m.Session();
    s.setBaseUrl(location.origin + "/data/");
    await s.sql("CREATE VIEW rmat_edges AS SELECT * FROM 'rmat_1m_edges.parquet'");
    await s.sql("CREATE PROPERTY GRAPH rmat EDGE TABLES (rmat_edges SOURCE KEY (src) DESTINATION KEY (dst))");
    await s.sql("SELECT count(*) FROM hop(rmat, seeds => [1], n => 1)");
    const res = {};
    const time = async (name, sql, reps = 5) => {
      const ts = []; let rows = null;
      for (let i = 0; i < reps; i++) {
        const t = performance.now();
        try { const r = JSON.parse(await s.run(sql, 1)); rows = r[r.length - 1].totalRows; } catch (e) { res[name] = "ERROR " + String(e?.message ?? e).slice(0, 400); return; }
        ts.push(performance.now() - t);
      }
      ts.sort((a, b) => a - b);
      res[name] = { median: +ts[Math.floor(ts.length / 2)].toFixed(1), max: +ts[ts.length - 1].toFixed(1), rows };
    };
    const ids = (k) => Array.from({ length: k }, (_, i) => (i * 7919 + 13) % 40000).join(",");
    for (const k of [50, 500, 5000]) {
      await time(`GRAPH_TABLE a IN (${k} ids), b filtered outside by IN list`, `SELECT src, dst FROM GRAPH_TABLE (rmat MATCH (a WHERE a.id IN (${ids(k)}))-[e]->(b) COLUMNS (a.id AS src, b.id AS dst)) WHERE dst IN (${ids(k)})`);
      await time(`GRAPH_TABLE a IN and b IN (${k} ids) inside`, `SELECT src, dst FROM GRAPH_TABLE (rmat MATCH (a WHERE a.id IN (${ids(k)}))-[e]->(b WHERE b.id IN (${ids(k)})) COLUMNS (a.id AS src, b.id AS dst))`);
      await time(`edge table scan, src IN and dst IN (${k} ids)`, `SELECT src, dst FROM rmat_edges WHERE src IN (${ids(k)}) AND dst IN (${ids(k)})`);
      await time(`rmat.edges scan, src IN and dst IN (${k} ids)`, `SELECT src, dst FROM rmat.edges WHERE src IN (${ids(k)}) AND dst IN (${ids(k)})`, 2);
    }
    await time("hop n=1 both from 500 seeds", `SELECT * FROM hop(rmat, seeds => [${ids(500)}], n => 1, direction => 'both')`);
    await time("hop 2 rings + induced edges (subquery form)", `WITH v AS (SELECT id FROM hop(rmat, seeds => [20013], n => 2, direction => 'both') UNION SELECT 20013) SELECT src, dst FROM rmat_edges WHERE src IN (SELECT id FROM v) AND dst IN (SELECT id FROM v)`, 3);
    await time("top-100 pagerank node table", `SELECT * FROM pagerank(rmat) ORDER BY pagerank DESC LIMIT 100`, 3);
    return res;
  });
  for (const [k, v] of Object.entries(out)) console.log(k, "=>", JSON.stringify(v));
  await browser.close();
})();
