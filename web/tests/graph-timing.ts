// How long `.graph rmat` (1M edges) takes to draw, in headless Chromium,
// against a running site: `bun run preview` (http://localhost:8788).
//
//   bun tests/graph-timing.ts [url] [runs]
//
// Each run is a fresh browser context (nothing cached), the rmat dataset
// loaded, then `.graph rmat` typed. It prints, in ms from Enter, when each
// step ended, as the page and the layout worker mark them
// (window.ursaShell.graphTimings()), the page's long tasks, and the medians.
import { chromium } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:8788").replace(/\/$/, "");
const runs = Number(Bun.argv[3] ?? 3);
const browser = await chromium.launch();
const all: Record<string, number>[] = [];
for (let r = 0; r < runs; r++) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
  // Long tasks on the page's thread, for where its time goes.
  await ctx.addInitScript(() => {
    (window as any).__long = [];
    new PerformanceObserver((l) => {
      for (const e of l.getEntries()) (window as any).__long.push([e.startTime, e.duration]);
    }).observe({ type: "longtask", buffered: true });
  });
  const p = await ctx.newPage();
  p.on("pageerror", (e) => console.error(`page error: ${e.message}`));
  await p.goto(`${base}/?dataset=rmat`);
  await p.waitForFunction(() => {
    const s = (window as any).ursaShell;
    return s && !s.busy() && s.screen().includes("memory U");
  }, null, { timeout: 180_000 });
  await p.evaluate(() => (window as any).ursaShell.idle());
  await p.click("#terminal");
  await p.keyboard.type(".graph rmat");
  const enterAt: number = await p.evaluate(() => performance.now());
  await p.keyboard.press("Enter");
  // The status, as soon as it shows.
  const status = p
    .waitForFunction(() => document.getElementById("graph-status")?.textContent?.trim() || null, null, { timeout: 120_000, polling: 20 })
    .then((h) => h.jsonValue());
  await p.waitForFunction(() => {
    const t = (window as any).ursaShell.graphTimings();
    return t?.done;
  }, null, { timeout: 180_000, polling: 100 });
  const t = await p.evaluate(() => (window as any).ursaShell.graphTimings());
  t.enterAt = enterAt;
  const rel: Record<string, number> = {};
  for (const [k, v] of Object.entries(t as Record<string, number>)) if (k !== "enter" && k !== "enterAt") rel[k] = Math.round(v - t.enter);
  all.push(rel);
  // The page's long tasks after Enter: [start, duration] in ms.
  const long: number[][] = await p.evaluate((at) => (window as any).__long.filter((e: number[]) => e[0] + e[1] > at), enterAt);
  console.log("  main-thread tasks over 50 ms:", JSON.stringify(long.map(([s, d]: number[]) => [Math.round(s - t.enterAt), Math.round(d)])));
  console.log(`run ${r + 1}:`, JSON.stringify(rel), "first status:", JSON.stringify(await status));
  await ctx.close();
}
const median = (xs: number[]) => xs.sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const keys = Object.keys(all[0]).sort((a, b) => all[0][a] - all[0][b]);
console.log("median ms from Enter:");
for (const k of keys) console.log(`  ${k.padEnd(16)} ${median(all.map((x) => x[k]))}`);
await browser.close();
