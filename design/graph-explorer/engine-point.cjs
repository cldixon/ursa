// Point-query latency of the wasm engine in headless Chromium: hop, 1-hop
// GRAPH_TABLE, induced edges, property lookup. Against a running preview.
//   node engine-point.cjs [url]
const { chromium } = require("playwright");
const base = (process.argv[2] ?? "http://localhost:8800").replace(/\/$/, "");
(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.error("page error:", e.message));
  await page.goto(base + "/404.html");
  const out = await page.evaluate(async () => {
    const engine = await (await fetch("/engine/engine.json")).json();
    const m = await import("/engine/ursa_wasm.js");
    await m.default({ module_or_path: new URL(engine.wasm, location.origin) });
    const s = new m.Session();
    s.setBaseUrl(location.origin + "/data/");
    const res = {};
    const time = async (name, sql, reps = 1) => {
      const ts = [];
      let last;
      for (let i = 0; i < reps; i++) {
        const q = typeof sql === "function" ? sql(i) : sql;
        const t = performance.now();
        try { last = await s.sqlArrow(q); } catch (e) { try { last = await s.sql(q); } catch (e2) { res[name] = "ERROR " + String(e2?.message ?? e2).slice(0, 300); return; } }
        ts.push(performance.now() - t);
      }
      ts.sort((a, b) => a - b);
      res[name] = { n: reps, first: +ts[0].toFixed(2), median: +ts[Math.floor(ts.length / 2)].toFixed(2), max: +ts[ts.length - 1].toFixed(2), bytes: last?.length };
      return last;
    };
    const ddl = async (q) => { try { await s.sql(q); } catch (e) { res["ddl:" + q.slice(0, 40)] = String(e?.message ?? e).slice(0, 300); } };
    // rmat: 46K nodes? 1M edges.
    await ddl("CREATE VIEW rmat_edges AS SELECT * FROM 'rmat_1m_edges.parquet'");
    await ddl("CREATE PROPERTY GRAPH rmat EDGE TABLES (rmat_edges SOURCE KEY (src) DESTINATION KEY (dst))");
    res.describe = await s.sql("SELECT * FROM describe(rmat)");
    const seed = (i) => (i * 7919 + 13) % 40000;
    await time("rmat hop first (builds/reuses index)", "SELECT * FROM hop(rmat, seeds => [1], n => 1, direction => 'both')");
    await time("rmat hop n=1 both", (i) => `SELECT * FROM hop(rmat, seeds => [${seed(i)}], n => 1, direction => 'both')`, 40);
    await time("rmat hop n=1 out", (i) => `SELECT * FROM hop(rmat, seeds => [${seed(i)}], n => 1)`, 40);
    await time("rmat hop n=2 both", (i) => `SELECT * FROM hop(rmat, seeds => [${seed(i)}], n => 2, direction => 'both')`, 10);
    await time("rmat count hop n=1", (i) => `SELECT count(*) FROM hop(rmat, seeds => [${seed(i)}], n => 1, direction => 'both')`, 40);
    await time("rmat GRAPH_TABLE 1-hop bound", (i) => `SELECT * FROM GRAPH_TABLE (rmat MATCH (a WHERE a.id = ${seed(i)})-[e]-(b) COLUMNS (a.id AS src, b.id AS dst))`, 20);
    await time("rmat edge-table filter src= or dst=", (i) => `SELECT src, dst FROM rmat_edges WHERE src = ${seed(i)} OR dst = ${seed(i)}`, 10);
    await time("rmat induced edges among 200 ids (edge-table scan + IN)", (i) => `WITH v AS (SELECT id FROM hop(rmat, seeds => [${seed(i)}], n => 1, direction => 'both') LIMIT 200) SELECT src, dst FROM rmat_edges WHERE src IN (SELECT id FROM v) AND dst IN (SELECT id FROM v)`, 5);
    await time("rmat induced edges via GRAPH_TABLE IN-list", (i) => `SELECT * FROM GRAPH_TABLE (rmat MATCH (a WHERE a.id IN (${[0,1,2,3,4,5,6,7,8,9].map((k) => seed(i + k)).join(",")}))-[e]->(b) COLUMNS (a.id AS src, b.id AS dst))`, 10);
    await time("rmat shortest path GRAPH_TABLE", (i) => `SELECT * FROM GRAPH_TABLE (rmat MATCH p = ANY SHORTEST (a WHERE a.id = ${seed(i)})-[e]-*(b WHERE b.id = ${seed(i + 1)}) COLUMNS (vertices(p) AS v, edges(p) AS e, element_id(p) AS el))`, 10);
    await time("rmat full edge list SELECT src,dst (arrow ipc)", "SELECT src, dst FROM rmat_edges", 2);
    await time("rmat pagerank", "SELECT * FROM pagerank(rmat)", 2);
    await time("rmat louvain", "SELECT * FROM louvain(rmat, seed => 1)", 1);
    await time("rmat community aggregate (louvain join edges, group)", "WITH c AS (SELECT id, louvain AS c FROM louvain(rmat, seed => 1)) SELECT a.c AS src, b.c AS dst, count(*) AS w FROM rmat_edges e JOIN c a ON a.id = e.src JOIN c b ON b.id = e.dst GROUP BY 1, 2", 1);
    // karate: properties.
    await ddl("CREATE VIEW members AS SELECT * FROM 'karate_nodes.parquet'");
    await ddl("CREATE VIEW ties AS SELECT * FROM 'karate_edges.parquet'");
    await ddl("CREATE PROPERTY GRAPH karate VERTEX TABLES (members KEY (id)) EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))");
    await time("karate hop n=1", (i) => `SELECT * FROM hop(karate, seeds => [${i % 34}], n => 1, direction => 'both')`, 40);
    await time("karate vertex properties WHERE id =", (i) => `SELECT * FROM members WHERE id = ${i % 34}`, 40);
    await time("karate GRAPH_TABLE edges", "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]->(b) COLUMNS (a.id AS src, b.id AS dst))", 5);
    res.pathShape = await s.sql("SELECT * FROM GRAPH_TABLE (karate MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) COLUMNS (path_length(p) AS steps, vertices(p) AS members, edges(p) AS ties, element_id(p) AS el))");
    res.hopShape = await s.sql("SELECT * FROM hop(karate, seeds => [16], n => 2, direction => 'both') LIMIT 5");
    res.spShape = await s.sql("SELECT * FROM shortest_path(karate, source => 16, target => 26, direction => 'both')");
    res.mem = performance.memory ? Math.round(performance.memory.usedJSHeapSize / 1e6) + " MB js heap" : null;
    return res;
  });
  for (const [k, v] of Object.entries(out)) console.log(k, "=>", typeof v === "string" ? "\n" + v : JSON.stringify(v));
  await browser.close();
})();
