// Graph view costs in headless Chromium (software GL): for several edge
// counts, .graph timings, the cost of one redraw (a wheel zoom), of a pick,
// and JS heap. Against a running preview.   node view-cost.cjs [url]
const { chromium } = require("playwright");
const base = (process.argv[2] ?? "http://localhost:8800").replace(/\/$/, "");
const sizes = (process.argv[3] ?? "1000,10000,100000,300000,1000000").split(",").map(Number);
(async () => {
  const browser = await chromium.launch({ args: process.env.GPU_ARGS ? process.env.GPU_ARGS.split(" ") : [] });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
  const p = await ctx.newPage();
  p.on("pageerror", (e) => console.error("page error:", e.message));
  await p.goto(`${base}/?dataset=rmat`);
  await p.waitForFunction(() => { const s = window.ursaShell; return s && !s.busy() && s.screen().includes("memory U"); }, null, { timeout: 180000 });
  await p.evaluate(() => window.ursaShell.idle());
  for (const n of sizes) {
    await p.click("#terminal");
    const cmd = n >= 1000000 ? ".graph rmat" : `.graph SELECT src, dst FROM rmat_edges LIMIT ${n}`;
    await p.keyboard.type(cmd);
    await p.keyboard.press("Enter");
    await p.waitForFunction((n) => { const g = window.ursaShell.graph(); return g && g.layoutDone && (n >= 1000000 ? g.edges >= 1000000 : g.edges === n); }, n, { timeout: 300000, polling: 100 });
    await p.waitForTimeout(1500);
    const t = await p.evaluate(() => window.ursaShell.graphTimings());
    const info = await p.evaluate(() => window.ursaShell.graph());
    // Redraws: a wheel event on the canvas, timed in the page until the frame
    // after the one that drew it (so a blocked main thread or GPU shows).
    const frames = await p.evaluate(async () => {
      const canvas = document.querySelector("#graph-stage canvas");
      const out = [];
      for (let i = 0; i < 6; i++) {
        const before = window.ursaShell.graph().frames;
        const t0 = performance.now();
        canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: i % 2 ? 40 : -40, clientX: 900, clientY: 400, bubbles: true, cancelable: true }));
        while (window.ursaShell.graph().frames === before) await new Promise((r) => requestAnimationFrame(r));
        const t1 = performance.now();
        await new Promise((r) => requestAnimationFrame(r));
        await new Promise((r) => setTimeout(r, 0));
        out.push([+(t1 - t0).toFixed(1), +(performance.now() - t0).toFixed(1)]);
      }
      return out;
    });
    const pick = await p.evaluate(() => {
      const t0 = performance.now();
      for (let i = 0; i < 20; i++) window.ursaShell.graphNodeAt(100 + i * 13, 200 + i * 7);
      return (performance.now() - t0) / 20;
    });
    const heap = await p.evaluate(() => performance.memory && Math.round(performance.memory.usedJSHeapSize / 1e6));
    console.log(JSON.stringify({ edges: info.edges, nodes: info.nodes, backend: info.backend, timings: t, redrawMs_toRenderReturn_toNextFrame: frames, pickMs: +pick.toFixed(3), jsHeapMB: heap }));
  }
  await browser.close();
})();
