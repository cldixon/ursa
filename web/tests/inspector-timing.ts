// How long the inspector takes to show an edge's row (#489), on rmat (a
// million edges), in headless Chromium, against a running site:
// `bun scripts/preview.ts <port>`.
//
//   bun tests/inspector-timing.ts [url] [runs] [clicks]      (TABLE=wide)
//
// Each run is a fresh browser context: the rmat dataset loaded, then a hop
// from node 0 drawn (`.graph SELECT * FROM hop(rmat, …)`): its edges are
// rows of the million, and a click on one finds its row in the whole table.
// rmat's edge table, rmat_edges, is its two keys alone, so an edge's row is
// the ends the graph's index gives. With TABLE=wide the graph is of a view
// with a third column, `weight`, so each row is read from the Parquet file
// by its number.
//
// Then up to `clicks` edges are clicked, each where no node is, with the
// selection cleared before. For each click it prints, as the page measures
// them:
// - click: from the mouse's pointerdown to the row on the page (the panel's
//   "row of" heading added);
// - shown: the inspector's own rowMs, from the selection shown to the row;
// - engine: the engine's ms for the inspect op (WebShell.inspectEdge);
// - indexed: whether the rows came through the graph's index.
//
// The first click of each run is reported apart from the later ones: it is
// the first of the session to read the table by row number. Then the medians
// over all the runs, and the machine's load average before and after.
import { chromium, type Page } from "playwright";
import { loadavg } from "node:os";

const base = (Bun.argv[2] ?? "http://localhost:8788").replace(/\/$/, "");
const runs = Number(Bun.argv[3] ?? 5);
const clicks = Number(Bun.argv[4] ?? 8);
const wide = process.env.TABLE === "wide";
const graph = wide ? "rmat_w" : "rmat";
const SETUP = wide
  ? [
      "CREATE VIEW rmat_w AS SELECT src, dst, (src + dst) % 97 AS weight FROM 'rmat_1m_edges.parquet';",
      "CREATE PROPERTY GRAPH rmat_w EDGE TABLES (rmat_w SOURCE KEY (src) DESTINATION KEY (dst));",
    ]
  : [];
const DRAW = `.graph SELECT * FROM hop(${graph}, seeds => [0], steps => 1, direction => 'both')`;

const load = () => loadavg().map((l) => l.toFixed(2)).join(" ");
const browser = await chromium.launch();
console.log(`${browser.browserType().name()} ${browser.version()}; ${graph}; load ${load()}`);

async function ready(p: Page) {
  await p.waitForFunction(() => {
    const shell = (window as any).ursaShell;
    return shell && !shell.busy() && shell.screen().includes("memory U");
  }, null, { timeout: 120_000 });
}
async function type(p: Page, text: string) {
  await p.keyboard.type(text);
  await p.keyboard.press("Enter");
  await p.evaluate(() => (window as any).ursaShell.idle());
  await ready(p);
}

type Click = { click: number; shown: number; engine: number; indexed: boolean; rows: number };
const firsts: Click[] = [];
const rest: Click[] = [];

for (let run = 0; run < runs; run++) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const p = await ctx.newPage();
  p.on("pageerror", (e) => console.log(`  page error: ${e}`));
  await p.goto(`${base}/?dataset=rmat`);
  await ready(p);
  await p.click("#terminal");
  for (const line of [...SETUP, DRAW]) await type(p, line);
  await p.waitForFunction(() => {
    const g = (window as any).ursaShell.graph();
    return g && g.frames > 0 && g.layoutDone;
  }, null, { timeout: 120_000, polling: 200 });
  // And the engine done with the drawing: it counts each node's edges not
  // drawn after the drawing is shown.
  await p.waitForFunction(() => (window as any).ursaShell.graph()?.notShownKnown === true, null, { timeout: 60_000, polling: 50 });
  const box = (await p.locator("#graph-stage canvas").boundingBox())!;
  // The time of each pointerdown, and of the row the panel shows after it.
  await p.evaluate(() => {
    const w = window as any;
    w.__down = 0;
    w.__row = 0;
    addEventListener("pointerdown", () => ((w.__down = performance.now()), (w.__row = 0)), { capture: true });
    new MutationObserver(() => {
      if (w.__row || !w.__down) return;
      const h = [...document.querySelectorAll("#graph-stage .graph-inspector h3")].find((h) => h.textContent!.startsWith("row of"));
      if (h) w.__row = performance.now();
    }).observe(document.getElementById("graph-stage")!, { childList: true, subtree: true });
  });

  // Edges a click picks: no node there, and on the left, where the panel
  // (on the right) is not.
  const targets: { x: number; y: number; label: string }[] = await p.evaluate(
    ([n, w, h]) => {
      const shell = (window as any).ursaShell;
      const canvas = document.querySelector("#graph-stage canvas")!.getBoundingClientRect();
      const edges = new Map<number, any>();
      for (let y = 20; y < h - 20 && edges.size < n; y += 5)
        for (let x = 20; x < w * 0.6 && edges.size < n; x += 5) {
          const e = shell.graphEdgeAt(x, y);
          if (e && !edges.has(e.index) && document.elementFromPoint(canvas.left + x, canvas.top + y)?.tagName === "CANVAS")
            edges.set(e.index, { x, y, label: `${e.src} → ${e.dst}` });
        }
      return [...edges.values()];
    },
    [clicks, box.width, box.height],
  );

  let first = true;
  for (const t of targets) {
    // (Esc with nothing selected would close the view.)
    if (await p.evaluate(() => (window as any).ursaShell.graphInspector() !== null)) {
      await p.focus("#graph-stage canvas");
      await p.keyboard.press("Escape");
    }
    await p.evaluate(() => ((window as any).__down = 0));
    await p.mouse.click(box.x + t.x, box.y + t.y);
    const done = await p
      .waitForFunction(() => (window as any).__row > 0 && (window as any).ursaShell.graphInspector()?.rowMs !== undefined, null, { timeout: 10_000, polling: 20 })
      .then(() => true, () => false);
    const shown = await p.evaluate(() => (window as any).ursaShell.graphInspector());
    const { down, row } = await p.evaluate(() => ({ down: (window as any).__down, row: (window as any).__row }));
    if (!done || shown?.kind !== "edge") {
      console.log(`  run ${run + 1}: ${t.label}: no row (${shown?.kind ?? "nothing"} shown)`);
      continue;
    }
    const c: Click = { click: row - down, shown: shown.rowMs, engine: shown.engineMs, indexed: !!shown.indexed, rows: shown.rows.length };
    (first ? firsts : rest).push(c);
    first = false;
    console.log(
      `  run ${run + 1}: ${t.label.padEnd(16)} click ${c.click.toFixed(0).padStart(4)} ms, shown ${c.shown.toFixed(0).padStart(4)} ms, engine ${c.engine.toFixed(0).padStart(4)} ms, ${c.rows} row(s), indexed ${c.indexed}`,
    );
  }
  await ctx.close();
}

const median = (v: number[]) => {
  const s = [...v].sort((a, b) => a - b);
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};
const range = (v: number[]) => `${Math.min(...v).toFixed(0)}–${Math.max(...v).toFixed(0)}`;
for (const [name, set] of [["first clicks", firsts], ["later clicks", rest], ["all clicks", [...firsts, ...rest]]] as const) {
  if (!set.length) continue;
  const of = (f: (c: Click) => number) => set.map(f);
  console.log(
    `${name} (${set.length}): click ${median(of((c) => c.click)).toFixed(0)} ms (${range(of((c) => c.click))}), shown ${median(of((c) => c.shown)).toFixed(0)} ms, engine ${median(of((c) => c.engine)).toFixed(0)} ms (${range(of((c) => c.engine))}); indexed ${set.filter((c) => c.indexed).length} of ${set.length}`,
  );
}
console.log(`load ${load()}`);
await browser.close();
