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
// - the coarse levels (#497): when the first frame, a level of the
//   layout's hierarchy, was shown, against the 600 ms asked for; the levels
//   that were drawn; and what their frames cost, each to the next animation
//   frame. The run fails if the 95th percentile of those is over 50 ms;
// - the aggregated view (#498), on rmat: the cut through the layout's
//   hierarchy drawn once the layout is done (its discs and merged edges at
//   the fitted zoom, which fail the run over 20,000 and 100,000), what the
//   cuts take in the layout worker (the run fails if their median is over
//   30 ms), the frames of a wheel zoom over cuts (over 50 ms at the 95th
//   percentile fails), and that zooming in reaches the graph's own nodes.
//   Then `.graph detail on`: what follows is of the graph in full detail;
// - the page's long tasks since Enter;
// - the longest task while the view was made (#426), which fails the run if
//   it is over 60 ms: the view builds no adjacency, and is made in pieces,
//   the pipelines of the coarse levels and the page's own work with the
//   new view among them (#520);
// - frame times while a scripted wheel zoom runs for WHEEL_MS (5000): the
//   time from one frame to the next, and the time in render() itself (with
//   no GPU the browser rasterises after render() returns, on the page's
//   thread, so the interval is the frame's cost); and what the frame budget
//   (#495) drew in them. The run fails if the 95th percentile of the
//   intervals is over 50 ms;
// - the refinement at rest (#496) that follows the zoom: from the zoom's
//   last event to the frame with everything (which includes the 200 ms the
//   view waits to be at rest), how many slices it was drawn in, and the
//   page's tasks over 50 ms meanwhile. The run fails if there is one, or if
//   the slices take over 5 s;
// - a pick, by a click in the middle of the canvas, with the view at rest
//   (its frame is the refined texture and the selection over it);
// - the wasm heaps (the engine's, the view's and the layout worker's) and
//   the page's memory as the browser measures it; and, from
//   graphTimings().memory, what the two ursa-viz instances hold in bytes an
//   edge. On rmat (a million edges) the run fails if the view's is over
//   20 MiB or the layout worker's over 60 MB (#501): they are 17.9 to
//   18.3 MiB and 55.7 MB, the edge order of #495 included, and vary little
//   with the machine's load, since a WebAssembly memory grows by what is
//   asked of it.
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
// What the page's view and the layout worker may hold at a million edges
// (#501): 20 MiB and 60 MB of WebAssembly memory.
const VIEW_BYTES = 20 * 2 ** 20;
const LAYOUT_BYTES = 60e6;
// When a large graph's first frame should be shown, from Enter (#497,
// #525): reported, not failed. It is 0.65 to 0.75 s here on a busy
// machine, where it was 1.0 to 1.4 s (ursa-viz/BENCH.md, "The first
// frame"): making the view and building the hierarchy each take about
// 0.4 s of a fresh page's time, side by side.
const FIRST_FRAME_MS = 600;
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
  // The first drawing (#497): a coarse level of the layout's hierarchy,
  // then each level the layout comes to, until the graph itself. When the
  // first frame was shown (the action's frame: on software WebGL2
  // firstDraw is when render() returned, not when the pixels were there),
  // against the issue's 600 ms; what the frames of the coarse levels cost
  // (the time from each to the next animation frame), which fails the run
  // over 50 ms at the 95th percentile; and when the graph itself was
  // first drawn.
  const drawn = await p.evaluate(() => (window as any).ursaShell.graph());
  const lf = drawn.levelFrames;
  if (lf?.count) {
    console.log(
      `  coarse levels: the first frame shown ${Math.round(shown?.frame)} ms after Enter${shown?.frame > FIRST_FRAME_MS ? ` (over the ${FIRST_FRAME_MS} ms asked for: the engine answered at ${Math.round(rel.engine)} ms, the view was made at ${Math.round(rel.view)} ms and the hierarchy was built at ${Math.round(rel.hierarchy)} ms)` : ""}; ` +
        `levels ${JSON.stringify(drawn.hierarchy)} as [level, nodes]; ${lf.count} frames before the graph itself (at ${Math.round(rel.graphDrawn)} ms), each to the next p50 ${lf.p50} ms, p95 ${lf.p95} ms, max ${lf.max} ms`,
    );
    if (lf.p95 > 50) slow.push(`run ${r + 1}: the frames of the coarse levels cost ${lf.p95} ms at the 95th percentile; at most 50 ms are allowed`);
    rel.levelFrameP95 = lf.p95;
    rel.levelFrameMax = lf.max;
  }
  // The aggregated view (#498): once the layout is done the graph is drawn
  // as a cut through its hierarchy, made by the layout worker for the
  // camera at rest. What the fitted cut is (at most 20,000 discs and
  // 100,000 merged edges, or the run fails) and what it took; the frames of
  // a wheel zoom over cuts, in and back out for 2 s (the run fails over
  // 50 ms at the 95th percentile); then zooming in a step at a time until
  // the cut is the graph's own nodes, and what each cut took in the worker
  // (the run fails if their median is over 30 ms, or if the graph itself
  // is not reached). Then full detail is forced (`.graph detail on`):
  // everything below is of the million edges themselves, as before #498.
  if (dataset === "rmat") {
    const graph = () => p.evaluate(() => (window as any).ursaShell.graph());
    const cutAtRest = () => p.waitForFunction(() => { const g = (window as any).ursaShell.graph(); return g && !g.cuts.waiting; }, null, { timeout: 60_000, polling: 50 });
    await p.waitForFunction(() => (window as any).ursaShell.graph()?.cut, null, { timeout: 60_000, polling: 50 });
    await cutAtRest();
    const fitted = await graph();
    const statusNow = await p.evaluate(() => document.getElementById("graph-status")?.textContent ?? "");
    console.log(
      `  aggregated: ${fitted.cut.groups} discs (${fitted.cut.merged} of more than one node) of ${fitted.cut.nodes} nodes and ${fitted.cut.edges} of ${fitted.cut.totalEdges} merged edges at the fitted zoom; ` +
        `the cut took ${fitted.cuts.last?.toFixed(1)} ms in the layout worker, after ${fitted.cuts.prepared?.ms?.toFixed(1)} ms once for the hierarchy's forest (${Math.round(fitted.cuts.prepared?.bytes / 1024)} KB held); ` +
        `labels ${JSON.stringify(fitted.groupLabels.slice(0, 5))}; status ${JSON.stringify(statusNow.slice(0, 70))}`,
    );
    if (fitted.cut.groups > 20_000 || fitted.cut.edges > 100_000 || fitted.cut.nodes !== fitted.nodes) slow.push(`run ${r + 1}: the fitted cut has ${fitted.cut.groups} discs of ${fitted.cut.nodes} nodes and ${fitted.cut.edges} merged edges; at most 20,000 and 100,000 are allowed, of every node`);
    if (!statusNow.startsWith("aggregated: ")) slow.push(`run ${r + 1}: the status line of an aggregated view says ${JSON.stringify(statusNow)}`);
    const box = (await p.locator("#graph-stage canvas").boundingBox())!;
    const middle = [box.x + box.width / 2, box.y + box.height / 2];
    await p.evaluate(() => (window as any).ursaShell.graphTimingsReset());
    await p.evaluate(
      ([x, y]) =>
        new Promise<void>((done) => {
          const canvas = document.querySelector("#graph-stage canvas")!;
          const began = performance.now();
          const step = () => {
            const t = performance.now() - began;
            if (t >= 2000) return done();
            canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: t < 1000 ? -30 : 30, clientX: x, clientY: y, bubbles: true, cancelable: true }));
            requestAnimationFrame(step);
          };
          requestAnimationFrame(step);
        }),
      middle,
    );
    const moved = (await timings()).frames;
    await cutAtRest();
    const during = await graph();
    console.log(`  aggregated, a wheel zoom for 2 s: ${moved.interval.count} frame intervals, p50 ${moved.interval.p50} ms, p95 ${moved.interval.p95} ms, max ${moved.interval.max} ms; ${during.cuts.ms.count} cuts so far`);
    if (moved.interval.p95 > 50) slow.push(`run ${r + 1}: the aggregated view's frames were ${moved.interval.p95} ms apart at the 95th percentile during a wheel zoom; at most 50 ms are allowed`);
    rel.cutFrameP95 = moved.interval.p95;
    // In, a step at a time, to the graph's own nodes.
    const fit = () => p.evaluate(() => document.querySelector("#graph-stage canvas")!.dispatchEvent(new KeyboardEvent("keydown", { key: "0", bubbles: true, cancelable: true })));
    await fit();
    await p.waitForTimeout(300);
    await cutAtRest();
    const steps: string[] = [];
    let reached = false;
    for (let step = 0; step < 20 && !reached; step++) {
      await p.evaluate(
        ([x, y]) => {
          const canvas = document.querySelector("#graph-stage canvas")!;
          for (let i = 0; i < 2; i++) canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: -100, clientX: x, clientY: y, bubbles: true, cancelable: true }));
        },
        middle,
      );
      await p.waitForTimeout(300);
      await cutAtRest();
      const now = await graph();
      reached = now.cut === null;
      steps.push(reached ? `the graph itself (${now.cuts.last?.toFixed(1)} ms)` : `${now.cut.groups}/${now.cut.edges} (${now.cuts.last?.toFixed(1)} ms)`);
    }
    const cuts = (await graph()).cuts;
    console.log(`  aggregated, zooming in by 1.35x a step, discs/merged edges (the cut's ms in the worker): ${steps.join(", ")}`);
    console.log(`  cuts: ${cuts.ms.count}, in the layout worker p50 ${cuts.ms.p50} ms, p95 ${cuts.ms.p95} ms, max ${cuts.ms.max} ms`);
    if (!reached) slow.push(`run ${r + 1}: zooming in did not reach the graph's own nodes: ${steps.join(", ")}`);
    if (cuts.ms.p50 > 30) slow.push(`run ${r + 1}: a cut took ${cuts.ms.p50} ms in the layout worker (the median of ${cuts.ms.count}); at most 30 ms are allowed`);
    rel.cutMs = cuts.ms.p50;
    rel.cutMaxMs = cuts.ms.max;
    rel.cutPrepareMs = Math.round(fitted.cuts.prepared?.ms);
    rel.cutDiscs = fitted.cut.groups;
    rel.cutEdges = fitted.cut.edges;
    // Fitted again, and in full detail from here.
    await fit();
    await p.keyboard.type(".graph detail on");
    await p.keyboard.press("Enter");
    await p.waitForFunction(() => { const g = (window as any).ursaShell.graph(); return g && g.cut === null && g.cuts.detail; }, null, { timeout: 60_000, polling: 50 });
  }
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
  // (It is drawn in slices, #496: with no GPU, seconds of them.)
  const atRest = () => p.waitForFunction(() => (window as any).ursaShell.graph()?.frame.partial === false, null, { timeout: 120_000, polling: 100 });
  await atRest();
  const firstRefinement = (await p.evaluate(() => (window as any).ursaShell.graph())).frame.refinement;
  console.log(`  after the layout, everything was drawn in ${firstRefinement?.slices} slices in ${Math.round(firstRefinement?.ms)} ms`);
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
  const wheelTook = Date.now() - began;
  // The refinement at rest (#496), from the zoom's last event: the view
  // waits to be at rest, draws everything offscreen in slices, and shows
  // it. The page's long tasks meanwhile, as the browser reports them (it
  // reports a task a little after its end: 300 ms more are waited for).
  const refined: any = await p.evaluate(
    () =>
      new Promise((done) => {
        const t0 = performance.now();
        const tasks: number[][] = [];
        const seen = new PerformanceObserver((list) => {
          for (const e of list.getEntries()) tasks.push([e.startTime, e.duration]);
        });
        seen.observe({ type: "longtask" });
        let said = false;
        const look = () => {
          said ||= (document.getElementById("graph-status")?.textContent ?? "").includes("refining…");
          const frame = (window as any).ursaShell.graph().frame;
          if (frame.partial || frame.refining) return void setTimeout(look, 20);
          const ms = performance.now() - t0;
          setTimeout(() => {
            seen.disconnect();
            const status = document.getElementById("graph-status")?.textContent ?? "";
            done({ ms, said, status, refinement: frame.refinement, tasks: tasks.filter(([start]) => start >= t0 && start <= t0 + ms).map(([, d]) => Math.round(d)) });
          }, 300);
        };
        look();
      }),
  );
  console.log(
    `  refinement at rest: everything shown ${Math.round(refined.ms)} ms after the zoom's last event; drawn in ${refined.refinement?.slices} slices in ${Math.round(refined.refinement?.ms)} ms ` +
      `(the last of ${refined.refinement?.sliceEdges} edges and ${refined.refinement?.sliceNodes} nodes; the GPU's longest ${Math.round(refined.refinement?.longestSliceMs)} ms); ` +
      `tasks over 50 ms meanwhile: ${JSON.stringify(refined.tasks)}; the status said "refining…": ${refined.said}, and then ${JSON.stringify(refined.status)}`,
  );
  if (refined.tasks.length) slow.push(`run ${r + 1}: tasks of ${refined.tasks.join(", ")} ms on the page while the frame at rest was drawn in slices; none over 50 ms is allowed`);
  if (!(refined.refinement?.ms <= 5000)) slow.push(`run ${r + 1}: the frame at rest took ${Math.round(refined.refinement?.ms)} ms to draw in slices; at most 5000 are allowed`);
  if (!refined.said || refined.status.includes("refining")) slow.push(`run ${r + 1}: the status line said "refining…" while the slices were drawn: ${refined.said}; after them it says ${JSON.stringify(refined.status)}`);
  rel.refinedAfter = Math.round(refined.ms);
  rel.refineMs = Math.round(refined.refinement?.ms);
  rel.refineSlices = refined.refinement?.slices;
  rel.refineLongestTask = Math.max(0, ...refined.tasks);
  const w = (await timings()).frames;
  console.log(
    `  wheel zoom, ${events} events in ${wheelTook} ms: ${w.interval.count} frame intervals, p50 ${w.interval.p50} ms, p95 ${w.interval.p95} ms, max ${w.interval.max} ms; ` +
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

  // A click on the view at rest: its frame is the refined texture, and
  // the selection over it.
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
  const held = (await timings()).memory;
  console.log(
    `  ursa-viz, ${held.edges.toLocaleString("en-US")} edges: the view ${held.view.bytes.toLocaleString("en-US")} bytes (${held.view.perEdge} an edge), ` +
      `the layout worker ${held.layout.bytes.toLocaleString("en-US")} bytes (${held.layout.perEdge} an edge)`,
  );
  if (dataset === "rmat") {
    if (held.view.bytes > VIEW_BYTES) slow.push(`run ${r + 1}: the view holds ${mb(held.view.bytes)}; at most ${mb(VIEW_BYTES)} are allowed`);
    if (held.layout.bytes > LAYOUT_BYTES) slow.push(`run ${r + 1}: the layout worker holds ${(held.layout.bytes / 1e6).toFixed(1)} MB; at most ${LAYOUT_BYTES / 1e6} MB are allowed`);
  }
  all.push(rel);
  await ctx.close();
}
const median = (xs: number[]) => xs.sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const steps = ["viewLongestTask", "shown", "levelFrameP95", "levelFrameMax", "cutDiscs", "cutEdges", "cutPrepareMs", "cutMs", "cutMaxMs", "cutFrameP95", "wheelEdges", "wheelFrameP95", "wheelRenderP95", "refinedAfter", "refineMs", "refineSlices", "refineLongestTask", "pick", "clickToFrame", "engineHeapMiB", "viewHeapMiB", "layoutHeapMiB"];
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
