// How long `.graph rmat` (1M edges) takes to draw and what the view costs to
// use, in headless Chromium, against a running site: `bun run preview`
// (http://localhost:8788).
//
//   bun tests/graph-timing.ts [url] [runs] [dataset]      (WHEEL_MS=, CHANNEL=shell)
//
// Each run is a fresh browser context (nothing cached), the dataset loaded
// (rmat unless named), then `.graph <dataset>` typed. Everything it prints
// is the page's own account, window.ursaShell.graphTimings() and
// graphMemory() (#430):
//
// - when each step ended, in ms from Enter, as the page and the layout
//   worker mark them, and the `.graph` action: ms in the engine, ms making
//   the view of its answer, and ms from Enter to when the page's thread was
//   free of the frame that showed it (firstDraw is when render() returned);
// - the page's long tasks since Enter;
// - the longest task while the view was made (#426), which fails the run if
//   it is over 60 ms: the view builds no adjacency, and is made in pieces;
// - frame times while a scripted wheel zoom runs for WHEEL_MS (5000): the
//   time from one frame to the next, and the time in render() itself (with
//   no GPU the browser rasterises after render() returns, on the page's
//   thread, so the interval is the frame's cost); and what the frame budget
//   (#495) drew in them. The run fails if the 95th percentile of the
//   intervals is over 50 ms;
// - a pick, by a click in the middle of the canvas, with the view at rest
//   (its frame draws every edge);
// - the wasm heaps (the engine's, the view's and the layout worker's) and
//   the page's memory as the browser measures it.
//
// Then the medians over the runs.
import { chromium } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:8788").replace(/\/$/, "");
const runs = Number(Bun.argv[3] ?? 3);
const dataset = Bun.argv[4] ?? "rmat";
const wheelMs = Number(process.env.WHEEL_MS ?? 5000);
// performance.measureUserAgentSpecificMemory() is not available in
// Playwright's default headless shell: the full Chromium (the new headless
// mode) has it, so that is what runs unless CHANNEL=shell, or it is not
// installed. Without the flag the browser answers after a garbage collection
// of its own choosing, up to a minute later.
const args = ["--enable-blink-features=ForceEagerMeasureMemory"];
const browser =
  process.env.CHANNEL === "shell"
    ? await chromium.launch({ args })
    : await chromium.launch({ args, channel: process.env.CHANNEL ?? "chromium" }).catch(() => chromium.launch({ args }));
console.log(`${browser.browserType().name()} ${browser.version()}${process.env.CHANNEL === "shell" ? " (headless shell)" : ""}`);
const all: Record<string, number>[] = [];
// What took too long: the run fails if anything did.
const slow: string[] = [];
const mb = (bytes: number | null | undefined) => (bytes == null ? "–" : `${(bytes / 2 ** 20).toFixed(1)} MiB`);
for (let r = 0; r < runs; r++) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
  const p = await ctx.newPage();
  p.on("pageerror", (e) => console.error(`page error: ${e.message}`));
  await p.goto(`${base}/?dataset=${dataset}`);
  await p.waitForFunction(() => {
    const s = (window as any).ursaShell;
    return s && !s.busy() && s.screen().includes("memory U");
  }, null, { timeout: 180_000 });
  await p.evaluate(() => (window as any).ursaShell.idle());
  await p.click("#terminal");
  await p.keyboard.type(`.graph ${dataset}`);
  await p.keyboard.press("Enter");
  // The status, as soon as it shows.
  const status = p
    .waitForFunction(() => document.getElementById("graph-status")?.textContent?.trim() || null, null, { timeout: 120_000, polling: 20 })
    .then((h) => h.jsonValue());
  // The layout done, and the first frame shown (a small graph's layout ends first).
  await p.waitForFunction(
    () => {
      const t = (window as any).ursaShell.graphTimings();
      return t?.done && t.actions.some((a: any) => a.command.startsWith(".graph"));
    },
    null,
    { timeout: 180_000, polling: 100 },
  );
  const timings = () => p.evaluate(() => (window as any).ursaShell.graphTimings());
  const t = await timings();
  // The steps: every number graphTimings() gives is ms after Enter.
  const rel: Record<string, number> = {};
  for (const [k, v] of Object.entries(t)) if (typeof v === "number" && k !== "enter") rel[k] = v;
  console.log(`run ${r + 1}:`, JSON.stringify(rel), "first status:", JSON.stringify(await status));
  const shown = t.actions.find((a: any) => a.command.startsWith(".graph"));
  console.log(`  action ${JSON.stringify(shown?.command)}: engine ${shown?.engine} ms, view ${shown?.delta} ms, Enter to its frame shown ${shown?.frame} ms`);
  console.log("  main-thread tasks over 50 ms since Enter [start, ms]:", JSON.stringify(t.longTasks.filter((l: any) => l.start + l.duration > 0).map((l: any) => [l.start, l.duration])));

  // Making the view (#426): from when the layout worker was posted its
  // graph to when createView returned. No task that overlaps it may hold
  // the page's thread for more than 60 ms.
  const making = t.longTasks.filter((l: any) => l.start + l.duration > rel.posted && l.start < rel.view);
  rel.viewLongestTask = Math.max(0, ...making.map((l: any) => l.duration));
  console.log(`  making the view, ${Math.round(rel.view - rel.posted)} ms: ${making.length} tasks over 50 ms, the longest ${rel.viewLongestTask} ms`);
  if (rel.viewLongestTask > 60) slow.push(`run ${r + 1}: a task of ${rel.viewLongestTask} ms while the view was made; at most 60 ms are allowed`);

  // A wheel zoom, in and back out, for wheelMs: a wheel event every
  // animation frame, dispatched on the page (an event through the browser's
  // input pipeline takes this script about 30 ms a time, two frames), so
  // the view is asked to move as fast as it can draw.
  const box = (await p.locator("#graph-stage canvas").boundingBox())!;
  const [cx, cy] = [box.x + box.width / 2, box.y + box.height / 2];
  await p.mouse.move(cx, cy);
  // From rest: the frame with every edge that follows the layout (#495) is
  // drawn, so that the zoom's frames are the view moving and nothing else.
  // (With no GPU that frame holds the page's thread for seconds, as every
  // frame did before the budget: a zoom begun under it waits for it.)
  const atRest = () => p.waitForFunction(() => (window as any).ursaShell.graph()?.frame.partial === false, null, { timeout: 120_000, polling: 100 });
  await atRest();
  await p.evaluate(() => new Promise((done) => requestAnimationFrame(() => setTimeout(done, 150))));
  await p.evaluate(() => (window as any).ursaShell.graphTimingsReset());
  const began = Date.now();
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
  // The last frame asked for, drawn.
  await p.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  const w = (await timings()).frames;
  console.log(
    `  wheel zoom, ${events} events in ${Date.now() - began} ms: ${w.interval.count} frame intervals, p50 ${w.interval.p50} ms, p95 ${w.interval.p95} ms, max ${w.interval.max} ms; ` +
      `render() p50 ${w.render.p50} ms, p95 ${w.render.p95} ms, max ${w.render.max} ms`,
  );
  // The frame budget (#495): what the last of those frames drew, and what
  // the budget's own measure of them was. Over 50 ms at the 95th percentile
  // fails the run.
  const budget = (await p.evaluate(() => (window as any).ursaShell.graph())).frame;
  console.log(
    `  frame budget: the last moving frame drew ${budget.k} of ${budget.edges} edges and ${budget.nodes} nodes; ${budget.ordered} edges in importance order (ordered in ${Math.round(budget.orderMs)} ms in the layout worker); ` +
      `the budget's p95 ${budget.p95} ms of ${budget.frames} frames, held to ${Math.round(budget.limit * 10) / 10} ms`,
  );
  if (w.interval.p95 > 50) slow.push(`run ${r + 1}: the wheel zoom's frames were ${w.interval.p95} ms apart at the 95th percentile; at most 50 ms are allowed`);
  rel.wheelEdges = budget.k;
  rel.wheelFrameP95 = w.interval.p95;
  rel.wheelRenderP95 = w.render.p95;
  rel.shown = shown?.frame;

  // A click on the view at rest: its frame draws every edge.
  await atRest();
  await p.mouse.click(cx, cy);
  // The frame that shows the click.
  await p.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
  const after = await timings();
  const click = after.actions.findLast((a: any) => a.command === "click");
  console.log(`  click: pick ${after.pick.last} ms (p95 ${after.pick.p95} ms of ${after.pick.count}), input to frame ${click?.frame} ms`);
  rel.pick = after.pick.last;
  rel.clickToFrame = click?.frame;

  const m = await p.evaluate(() => (window as any).ursaShell.graphMemory({ timeoutMs: 30_000 }));
  console.log(
    `  wasm heaps: engine ${mb(m.wasm.engine)}, view ${mb(m.wasm.view)}, layout worker ${mb(m.wasm.layout)}; ` +
      `page and workers ${mb(m.userAgent?.bytes)}${m.userAgent ? "" : ` (${m.userAgentError})`}; cross-origin isolated: ${m.crossOriginIsolated}`,
  );
  rel.engineHeapMiB = Math.round(m.wasm.engine / 2 ** 20);
  rel.viewHeapMiB = Math.round(m.wasm.view / 2 ** 20);
  rel.layoutHeapMiB = Math.round(m.wasm.layout / 2 ** 20);
  all.push(rel);
  await ctx.close();
}
const median = (xs: number[]) => xs.sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const steps = ["viewLongestTask", "shown", "wheelEdges", "wheelFrameP95", "wheelRenderP95", "pick", "clickToFrame", "engineHeapMiB", "viewHeapMiB", "layoutHeapMiB"];
const keys = Object.keys(all[0]).filter((k) => !steps.includes(k)).sort((a, b) => all[0][a] - all[0][b]);
console.log("median ms from Enter:");
for (const k of keys) console.log(`  ${k.padEnd(16)} ${median(all.map((x) => x[k]))}`);
console.log("median, in use:");
for (const k of steps) console.log(`  ${k.padEnd(16)} ${median(all.map((x) => x[k]))}`);
await browser.close();
if (slow.length) {
  console.error("\nFAIL:\n" + slow.join("\n"));
  process.exit(1);
}
