// The cost of today's hand-off into ursa-viz, in headless Chromium: copies
// in and out of wasm, createView on the page's thread, the wasm heap, and
// pick / setPositions at scale.   node handoff.cjs [url]
const { chromium } = require("playwright");
const base = (process.argv[2] ?? "http://localhost:8800").replace(/\/$/, "");
(async () => {
  const browser = await chromium.launch();
  const p = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  p.on("pageerror", (e) => console.error("page error:", e.message));
  await p.goto(base + "/404.html");
  for (const [n, m] of [[46318, 1e6], [131072, 1e6], [1e6, 2e6]]) {
    const out = await p.evaluate(async ([n, m]) => {
      const manifest = await (await fetch("/viz/viz.json")).json();
      const mod = await import(manifest.module);
      const wasm = await mod.default();
      const mb = () => +(wasm.memory.buffer.byteLength / 1048576).toFixed(1);
      // A skewed random graph (not R-MAT; costs are about sizes, not shape).
      let s = 12345;
      const rnd = () => ((s = (Math.imul(s, 1664525) + 1013904223) >>> 0) / 4294967296);
      const src = new Uint32Array(m), dst = new Uint32Array(m);
      for (let i = 0; i < m; i++) { src[i] = Math.floor(n * rnd() ** 3); dst[i] = Math.floor(n * rnd()); }
      const r = { n, m, wasmStartMB: mb() };
      let t = performance.now();
      const copy = src.slice(); const copy2 = dst.slice();
      r.sliceBothMs = +(performance.now() - t).toFixed(1);
      t = performance.now();
      structuredClone({ src: copy, dst: copy2 }, { transfer: [copy.buffer, copy2.buffer] });
      r.transferMs = +(performance.now() - t).toFixed(2);
      const canvas = document.createElement("canvas");
      canvas.width = 640; canvas.height = 700; document.body.append(canvas);
      const before = mb();
      t = performance.now();
      const view = await mod.createView(canvas, n, src, dst);
      r.createViewMs = +(performance.now() - t).toFixed(1);
      r.viewWasmMB = +(mb() - before).toFixed(1);
      const b2 = mb();
      t = performance.now();
      const layout = new mod.Layout(n, src, dst);
      r.layoutCtorMs = +(performance.now() - t).toFixed(1);
      r.layoutWasmMB = +(mb() - b2).toFixed(1);
      t = performance.now();
      const xy = new Float32Array(layout.positions());
      r.positionsOutMs = +(performance.now() - t).toFixed(2);
      t = performance.now();
      view.setPositions(xy);
      r.setPositionsMs = +(performance.now() - t).toFixed(2);
      view.fit();
      t = performance.now();
      for (let i = 0; i < 20; i++) view.pick(50 + i * 25, 60 + i * 30);
      r.pickMs = +((performance.now() - t) / 20).toFixed(3);
      t = performance.now();
      view.render();
      r.renderCallMs = +(performance.now() - t).toFixed(1);
      r.backend = view.backend;
      layout.free(); view.free(); canvas.remove();
      r.wasmEndMB = mb();
      return r;
    }, [n, m]);
    console.log(JSON.stringify(out));
    await p.reload();
  }
  await browser.close();
})();
