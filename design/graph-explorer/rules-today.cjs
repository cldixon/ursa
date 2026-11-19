// What `.graph` draws today for each kind of result (karate).  node rules-today.cjs [url]
const { chromium } = require("playwright");
const base = (process.argv[2] ?? "http://localhost:8800").replace(/\/$/, "");
const cases = [
  ["pagerank result (id, pagerank)", "SELECT * FROM pagerank(karate) LIMIT 5;"],
  ["hop n=2 (seed, id, distance)", "SELECT * FROM hop(karate, seeds => [0], n => 2, direction => 'both');"],
  ["path (steps, members, ties)", "SELECT * FROM GRAPH_TABLE (karate MATCH p = ANY SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) COLUMNS (path_length(p) AS steps, vertices(p) AS members, edges(p) AS ties));"],
  ["path vertices only", "SELECT * FROM GRAPH_TABLE (karate MATCH p = ALL SHORTEST (a WHERE a.id = 16)-[e]-*(b WHERE b.id = 26) COLUMNS (vertices(p) AS members));"],
  ["GRAPH_TABLE 3 vars (a,b,c ids)", "SELECT * FROM GRAPH_TABLE (karate MATCH (a WHERE a.id = 0)-[]-(b)-[]-(c) COLUMNS (a.id AS a, b.id AS b, c.id AS c)) LIMIT 20;"],
  ["GRAPH_TABLE names not ids", "SELECT * FROM GRAPH_TABLE (karate MATCH (a)-[e]-(b) WHERE a.club <> b.club COLUMNS (a.club AS ca, b.club AS cb, a.id AS x, b.id AS y));"],
  ["shortest_path()", "SELECT * FROM shortest_path(karate, source => 16, target => 26, direction => 'both');"],
  ["one column", "SELECT id FROM members;"],
  ["algorithms()", "SELECT * FROM algorithms(karate, pagerank(direction => 'both'), community => louvain(seed => 1));"],
  ["weighted edge list (src,dst,w)", "SELECT src, dst, 1.5 AS weight FROM ties;"],
];
(async () => {
  const browser = await chromium.launch();
  const p = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  await p.goto(`${base}/?dataset=karate`);
  await p.waitForFunction(() => { const s = window.ursaShell; return s && !s.busy() && s.screen().includes("memory U"); }, null, { timeout: 120000 });
  await p.evaluate(() => window.ursaShell.idle());
  await p.click("#terminal");
  for (const [name, sql] of cases) {
    await p.keyboard.type(sql); await p.keyboard.press("Enter");
    await p.waitForTimeout(400); await p.evaluate(() => window.ursaShell.idle());
    await p.keyboard.type(".graph"); await p.keyboard.press("Enter");
    await p.waitForTimeout(1500); await p.evaluate(() => window.ursaShell.idle());
    const line = (await p.evaluate(() => window.ursaShell.screen())).split("\n").filter((l) => l.trim()).slice(-2, -1)[0];
    const g = await p.evaluate(() => window.ursaShell.graphOpen() ? window.ursaShell.graph() : null);
    console.log(`${name}\n    -> ${line.trim()}${g ? `  [open: ${g.nodes} nodes, ${g.edges} edges]` : ""}`);
    await p.keyboard.press("Escape");
  }
  await browser.close();
})();
