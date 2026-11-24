// The benchmark of saved positions at the raised edge cap (#502): an R-MAT
// graph of 5M edges, generated here for the run and never shipped, drawn in
// headless Chromium against a running site (`bun run preview`).
//
//   bun tests/graph-positions-bench.ts [url] [edges] [scale]     (LIVE=0, WHEEL_MS=, CHANNEL=shell)
//
// It writes the graph's edges to a CSV in the system's temporary directory
// (R-MAT with Graph500's parameters, from a fixed seed: `edges` edges, 5M
// unless given, among 2^`scale` ids, 20 unless given), adds the file to the
// page as `.files add` does, and makes a property graph of it. Then, each
// timed and with the memory the page reports (graphMemory, graphTimings):
//
// 1. `.graph big`: the live layout, to its end (skipped with LIVE=0: with no
//    GPU it is minutes), and where it left each node;
// 2. `CREATE TABLE pos AS SELECT * FROM layout_fa2(big)`: the layout, once;
// 3. `.graph big POSITIONS (SELECT * FROM pos)`: the engine's time, the first
//    frame after the engine's answer (the issue asks for 2 s at most, which
//    fails the run here), the frame with everything, a wheel zoom's frames
//    (the moving-frame target is 50 ms at the 95th percentile, reported),
//    and the memory of the page's view and the layout worker;
// 4. with the live layout: whether the saved positions are its end state,
//    node for node, to the bit (centred, as layout_fa2 centres its rows).
//
// The CSV is deleted at the end.
import { rmSync, openSync, writeSync, closeSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium, type Page } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:8788").replace(/\/$/, "");
const edges = Number(Bun.argv[3] ?? 5_000_000);
const scale = Number(Bun.argv[4] ?? 20);
const live = process.env.LIVE !== "0";
const wheelMs = Number(process.env.WHEEL_MS ?? 5000);
const mb = (bytes: number | null | undefined) => (bytes == null ? "–" : `${(bytes / 2 ** 20).toFixed(1)} MiB`);

// R-MAT: each edge picks a quadrant of the adjacency matrix `scale` times
// (a 0.57, b 0.19, c 0.19, d 0.05), from xorshift32 with a fixed seed.
const file = join(tmpdir(), `ursa-rmat-${edges}-${scale}-${process.pid}.csv`);
{
  const t = performance.now();
  let s = 0x9e3779b9;
  const next = () => {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    return (s >>> 0) / 4294967296;
  };
  const fd = openSync(file, "w");
  writeSync(fd, "src,dst\n");
  let chunk: string[] = [];
  for (let e = 0; e < edges; e++) {
    let [u, v] = [0, 0];
    for (let l = 0; l < scale; l++) {
      const r = next();
      u = u * 2 + (r >= 0.76 ? 1 : 0);
      v = v * 2 + ((r >= 0.57 && r < 0.76) || r >= 0.95 ? 1 : 0);
    }
    chunk.push(`${u},${v}\n`);
    if (chunk.length === 1 << 16) {
      writeSync(fd, chunk.join(""));
      chunk = [];
    }
  }
  writeSync(fd, chunk.join(""));
  closeSync(fd);
  console.log(`R-MAT: ${edges.toLocaleString("en-US")} edges among 2^${scale} ids, ${mb(statSync(file).size)} of CSV, generated in ${Math.round(performance.now() - t)} ms`);
}

const failures: string[] = [];
const args = ["--enable-unsafe-swiftshader"];
const browser = await chromium.launch(process.env.CHANNEL === "shell" ? { args } : { args, channel: process.env.CHANNEL ?? "chromium" }).catch(() => chromium.launch({ args }));
console.log(`${browser.browserType().name()} ${browser.version()}`);
try {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
  const p = await ctx.newPage();
  p.on("pageerror", (e) => failures.push(`page error: ${e.message}`));
  const LONG = 3_600_000;
  const ready = (page: Page) =>
    page.waitForFunction(() => {
      const s = (window as any).ursaShell;
      return s && !s.busy() && s.screen().includes("memory U");
    }, null, { timeout: LONG });
  const idle = async () => {
    await p.evaluate(() => (window as any).ursaShell.idle());
    await ready(p);
  };
  /** Type a line and wait for it; the ms it took. */
  const type = async (line: string) => {
    const t = performance.now();
    await p.keyboard.type(line);
    await p.keyboard.press("Enter");
    await idle();
    return Math.round(performance.now() - t);
  };
  const screenTail = async (n = 6) => (await p.evaluate(() => (window as any).ursaShell.screen())).split("\n").filter((l: string) => l.trim()).slice(-n).join("\n");
  const graph = () => p.evaluate(() => (window as any).ursaShell.graph());
  const timings = () => p.evaluate(() => (window as any).ursaShell.graphTimings());
  const status = () => p.evaluate(() => document.getElementById("graph-status")?.textContent ?? "");
  const settled = () =>
    p.waitForFunction(() => {
      const g = (window as any).ursaShell.graph();
      return g && g.frames > 0 && g.layoutDone;
    }, null, { timeout: LONG, polling: 250 });
  const atRest = () => p.waitForFunction(() => (window as any).ursaShell.graph()?.frame.partial === false, null, { timeout: LONG, polling: 250 });
  const memory = async (label: string) => {
    const m = await p.evaluate(() => (window as any).ursaShell.graphMemory({ timeoutMs: 30_000 }));
    const held = (await timings()).memory;
    console.log(
      `  ${label}: wasm heaps: engine ${mb(m.wasm.engine)}, view (page) ${mb(m.wasm.view)}, layout worker ${mb(m.wasm.layout)}; page and workers ${mb(m.userAgent?.bytes)}${m.userAgent ? "" : ` (${m.userAgentError})`}; ` +
        `ursa-viz holds, of ${held.edges.toLocaleString("en-US")} edges: the view ${mb(held.view.bytes)} (${held.view.perEdge} bytes an edge), the layout worker ${mb(held.layout.bytes)} (${held.layout.perEdge} an edge)`,
    );
    const js = await p.evaluate(() => (performance as any).memory?.usedJSHeapSize ?? null);
    console.log(`  ${label}: the page's JS heap ${mb(js)}`);
    return { m, held };
  };

  await p.goto(base);
  await ready(p);
  await idle();
  let t = performance.now();
  await p.setInputFiles("#file-input", file);
  await p.waitForFunction(() => (window as any).ursaShell.screen().includes("LIMIT 10;"), null, { timeout: LONG });
  await idle();
  // The view the page made of the file: its last line says the name.
  const view: string = (await p.evaluate(() => (window as any).ursaShell.screen())).match(/FROM "([^"]+)" LIMIT 10;/)?.[1] ?? "";
  console.log(`added the CSV in ${Math.round(performance.now() - t)} ms`);
  await p.click("#terminal");
  console.log(`table: ${await type(`CREATE TABLE big_edges AS SELECT src, dst FROM "${view}";`)} ms`);
  await type(`DROP VIEW "${view}";`);
  await type("CREATE PROPERTY GRAPH big EDGE TABLES (big_edges SOURCE KEY (src) DESTINATION KEY (dst));");
  if (/Error: /.test(await screenTail(6))) throw new Error(`the graph could not be made:\n${await screenTail(6)}`);

  if (live) {
    t = performance.now();
    await type(".graph big");
    await settled();
    const tm = await timings();
    console.log(`live layout: done ${Math.round(tm.done)} ms after Enter (engine ${Math.round(tm.engine)} ms, first frame ${Math.round(tm.firstDraw)} ms, the graph itself ${Math.round(tm.graphDrawn)} ms); ${await status()}`);
    await atRest();
    await memory("live");
    await p.evaluate(() => {
      const w = window as any;
      const g = w.ursaShell.graphPlacement();
      const n = g.ids.length;
      const f = Math.fround;
      let [sx, sy] = [0, 0];
      for (let i = 0; i < n; i++) {
        sx = f(sx + g.xy[i]);
        sy = f(sy + g.xy[n + i]);
      }
      const [mx, my] = [f(sx / f(n)), f(sy / f(n))];
      w.__live = new Map();
      for (let i = 0; i < n; i++) w.__live.set(g.ids[i], [f(g.xy[i] - mx), f(g.xy[n + i] - my)]);
    });
    console.log(await screenTail(8));
    await p.keyboard.press("Escape");
    await p.click("#terminal");
  }

  console.log(`layout_fa2, once, in the engine: ${await type("CREATE TABLE pos AS SELECT * FROM layout_fa2(big);")} ms`);
  await type(".graph big POSITIONS (SELECT * FROM pos)");
  await settled();
  // The layout worker, at the positions and with nothing to run, says so.
  await p.waitForFunction(() => (window as any).ursaShell.graphTimings()?.done > 0, null, { timeout: LONG, polling: 250 });
  const tm = await timings();
  const g = await graph();
  const first = tm.firstDraw - tm.engine;
  const shown = tm.actions.findLast((a: any) => a.command.startsWith(".graph"));
  console.log(
    `saved positions: ${g.nodes.toLocaleString("en-US")} nodes, ${g.edges.toLocaleString("en-US")} edges; engine ${Math.round(tm.engine)} ms; ` +
      `first frame ${Math.round(first)} ms after the engine's answer (render() returned; the page's thread was free of it ${Math.round(shown.frame - tm.engine)} ms after), view made in ${Math.round(tm.view - tm.posted)} ms; ` +
      `layout worker ready ${Math.round(tm.done)} ms after Enter; ${await status()}`,
  );
  console.log("  main-thread tasks over 50 ms since Enter [start, ms]:", JSON.stringify(tm.longTasks.filter((l: any) => l.start + l.duration > 0).map((l: any) => [l.start, l.duration])));
  if (!(first <= 2_000)) failures.push(`the first frame came ${Math.round(first)} ms after the engine's answer; at most 2000 are allowed`);
  if (!g.saved || g.iterations !== 0) failures.push(`a layout ran: ${JSON.stringify({ saved: g.saved, iterations: g.iterations })}`);
  await atRest();
  const refinement = (await graph()).frame.refinement;
  console.log(`  everything was drawn in ${refinement?.slices} slices in ${Math.round(refinement?.ms)} ms (the longest ${Math.round(refinement?.longestSliceMs)} ms)`);

  // A wheel zoom, in and back out: a wheel event an animation frame.
  const box = (await p.locator("#graph-stage canvas").boundingBox())!;
  const [cx, cy] = [box.x + box.width / 2, box.y + box.height / 2];
  await p.mouse.move(cx, cy);
  await p.evaluate(() => new Promise((done) => requestAnimationFrame(() => setTimeout(done, 150))));
  await p.evaluate(() => (window as any).ursaShell.graphTimingsReset());
  const events: number = await p.evaluate(
    ([x, y, ms]) =>
      new Promise<number>((done) => {
        const canvas = document.querySelector("#graph-stage canvas")!;
        const began = performance.now();
        let n = 0;
        const step = () => {
          const t = performance.now() - began;
          if (t >= ms) return done(n);
          canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: t < ms / 2 ? -30 : 30, clientX: x, clientY: y, bubbles: true, cancelable: true }));
          n++;
          requestAnimationFrame(step);
        };
        requestAnimationFrame(step);
      }),
    [cx, cy, wheelMs],
  );
  const w = (await timings()).frames;
  const budget = (await graph()).frame;
  console.log(
    `  wheel zoom, ${events} events in ${wheelMs} ms: ${w.interval.count} frame intervals, p50 ${w.interval.p50} ms, p95 ${w.interval.p95} ms, max ${w.interval.max} ms; render() p95 ${w.render.p95} ms; ` +
      `the last moving frame drew ${budget.k} of ${budget.edges} edges and ${budget.nodes} nodes (${budget.ordered} edges ordered, in ${Math.round(budget.orderMs)} ms in the worker)${w.interval.p95 > 50 ? " — over the 50 ms target" : ""}`,
  );
  await atRest();
  await memory("saved positions");

  if (live) {
    const same = await p.evaluate(() => {
      const w = window as any;
      const g = w.ursaShell.graphPlacement();
      const n = g.ids.length;
      let [same, differ, missing] = [0, 0, 0];
      for (let i = 0; i < n; i++) {
        const a = w.__live.get(g.ids[i]);
        if (!a) missing++;
        else if (Object.is(a[0], g.xy[i]) && Object.is(a[1], g.xy[n + i])) same++;
        else differ++;
      }
      return { nodes: n, live: w.__live.size, same, differ, missing };
    });
    console.log(`  read back against the live layout's end state: ${JSON.stringify(same)}`);
    if (same.same !== same.nodes || same.live !== same.nodes) failures.push(`the saved positions are not the live layout's end state: ${JSON.stringify(same)}`);
  }
  await ctx.close();
} catch (e) {
  failures.push(String((e as Error)?.message ?? e));
} finally {
  await browser.close();
  rmSync(file, { force: true });
}
if (failures.length) {
  console.error("\nFAIL:\n" + failures.join("\n"));
  process.exit(1);
}
console.log("\nall passed");
