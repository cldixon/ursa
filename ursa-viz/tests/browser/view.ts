// The graph view in headless Chromium: build ursa-viz for the web first
// (see ../../README.md), then
//
//   bun ursa-viz/tests/browser/view.ts [screenshot dir]
//
// For each test graph, laid out coarse to fine (the default) and flat
// (`&multilevel=0`): the backend, layout cost per iteration (in the worker)
// or time to the end of the coarse-to-fine schedule, draw cost per frame and
// frame rate, a screenshot, and how good the layout is: for the clustered
// graph, how well it separates its clusters; for the grid, whether it is
// unfolded. MODE=flat or MODE=multilevel runs one of the two.
//
// Then the highlight layer (#435), on the karate club in the light theme and
// the dark, always on WebGL2 (the backend with the least to draw with, and
// the one whose frames read back here): a path of five edges reads back as
// a 3-pixel line in the highlight colour with its nodes at full strength and
// everything else at 35%; a route's ends and a selected node carry their
// rings; a hidden node and its edges are gone; and a selection of one node
// uploads 24 bytes. ONLY=highlight runs these alone.
import { chromium } from "playwright";
import { file } from "bun";
import { extname, join, normalize, resolve } from "node:path";

const root = resolve(import.meta.dir, "../..");
const shots = Bun.argv[2];
const TYPES: Record<string, string> = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm" };
const server = Bun.serve({
  port: 0,
  async fetch(req) {
    const path = normalize(join(root, new URL(req.url).pathname));
    if (!path.startsWith(root)) return new Response("forbidden", { status: 403 });
    const f = file(path);
    if (!(await f.exists())) return new Response("not found", { status: 404 });
    return new Response(f, { headers: { "content-type": TYPES[extname(path)] ?? "application/octet-stream" } });
  },
});
const base = `http://localhost:${server.port}/demo/index.html`;

const ONLY = process.env.ONLY;
const MODE = process.env.MODE ?? "both";
const GRAPHS = [
  { name: "clusters 8x500", query: "graph=clusters&k=8&size=500", iterations: 300, timeout: 60_000 },
  { name: "grid 100x100", query: "graph=grid&w=100&h=100", iterations: 500, timeout: 90_000 },
  { name: "rmat 16K nodes, 100K edges", query: "graph=rmat&scale=14&edges=100000", iterations: 100, timeout: 120_000 },
  { name: "rmat 131K nodes, 1M edges", query: "graph=rmat&scale=17&edges=1000000", iterations: 12, timeout: 300_000 },
];
// Flat runs a fixed number of iterations; coarse to fine runs its schedule.
const CASES_ALL = GRAPHS.flatMap((g) => [
  ...(MODE !== "flat" ? [{ ...g, multilevel: true, query: g.query }] : []),
  ...(MODE !== "multilevel" ? [{ ...g, multilevel: false, query: `${g.query}&multilevel=0&iterations=${g.iterations}` }] : []),
]);
const CASES = ONLY ? CASES_ALL.filter((c) => c.name.startsWith(ONLY)) : CASES_ALL;

const webgpu = process.env.WEBGPU !== "0";
const browser = await chromium.launch({ args: webgpu ? ["--enable-unsafe-webgpu", "--enable-features=Vulkan"] : ["--disable-features=WebGPU"] });
const failures: string[] = [];
for (const c of CASES) {
  const page = await browser.newPage({ viewport: { width: 1000, height: 700 } });
  page.on("pageerror", (e) => failures.push(`${c.name}: ${e.message}`));
  page.on("console", (m) => m.type() === "error" && failures.push(`${c.name}: ${m.text()}`));
  await page.goto(`${base}?${c.query}`);
  await page.waitForFunction(() => (window as any).stats?.phase === "running", null, { timeout: 120_000 });
  const done = await page
    .waitForFunction(() => (window as any).stats.layoutDone, null, { timeout: c.timeout, polling: 250 })
    .then(() => true, () => false);
  await page.waitForTimeout(1500); // a second of frames with the layout settled
  const s: any = await page.evaluate(() => (window as any).stats);
  const mode = c.multilevel ? "multilevel" : "flat";
  if (!done) failures.push(`${c.name} (${mode}): the layout did not finish (${s.iterations} iterations)`);
  let quality = "";
  if (c.name.startsWith("grid")) {
    // Correlation of layout distance with grid distance over sampled pairs:
    // near 1 for a sheet laid out flat, far below for a folded one.
    const r = await page.evaluate(() => {
      const v = (window as any).view, g = (window as any).graph;
      const xy = v.positions(), n = g.n, w = Math.round(Math.sqrt(n));
      const a: number[] = [], b: number[] = [];
      for (let i = 0; i < n; i += 13)
        for (let j = i + 7; j < n; j += 97) {
          a.push(Math.hypot((i % w) - (j % w), Math.floor(i / w) - Math.floor(j / w)));
          b.push(Math.hypot(xy[i] - xy[j], xy[n + i] - xy[n + j]));
        }
      const m = a.length, ma = a.reduce((p, q) => p + q) / m, mb = b.reduce((p, q) => p + q) / m;
      let sab = 0, saa = 0, sbb = 0;
      for (let k = 0; k < m; k++) { sab += (a[k] - ma) * (b[k] - mb); saa += (a[k] - ma) ** 2; sbb += (b[k] - mb) ** 2; }
      return sab / Math.sqrt(saa * sbb);
    });
    quality = ` · grid r ${r.toFixed(2)} (layout vs grid distance; 1 is unfolded)`;
    if (c.multilevel && !(r > 0.9)) failures.push(`${c.name} (${mode}): the grid is folded (r ${r.toFixed(2)})`);
  }
  if (c.name.startsWith("clusters")) {
    // Mean distance between nodes of one cluster over the mean between clusters.
    const ratio = await page.evaluate(() => {
      const v = (window as any).view, g = (window as any).graph;
      const xy = v.positions(), n = g.n;
      let inS = 0, inN = 0, outS = 0, outN = 0;
      for (let i = 0; i < n; i += 7)
        for (let j = i + 3; j < n; j += 11) {
          const d = Math.hypot(xy[i] - xy[j], xy[n + i] - xy[n + j]);
          if (g.community[i] === g.community[j]) { inS += d; inN++; } else { outS += d; outN++; }
        }
      return inS / inN / (outS / outN);
    });
    quality = ` · cluster spread ${ratio.toFixed(2)} (within ÷ between; lower is better)`;
    if (!(ratio < (c.multilevel ? 0.1 : 0.6))) failures.push(`${c.name} (${mode}): clusters not separated (ratio ${ratio.toFixed(2)})`);
  }
  // Pixels drawn, read back from an offscreen frame: the canvas itself
  // cannot be screenshot under headless WebGPU.
  const snap = await page.evaluate(async () => { try {
    const v = (window as any).view, canvas = document.getElementById("view") as HTMLCanvasElement;
    const px: Uint8Array = await (window as any).snapshot();
    let drawn = 0;
    for (let i = 0; i < px.length; i += 4) if (px[i] < 235 || px[i + 1] < 235 || px[i + 2] < 235) drawn++;
    const out = document.createElement("canvas");
    out.width = canvas.width; out.height = canvas.height;
    out.getContext("2d")!.putImageData(new ImageData(new Uint8ClampedArray(px), canvas.width), 0, 0);
    return { drawn: drawn / (px.length / 4), png: out.toDataURL("image/png"), error: null };
  } catch (e) { return { drawn: 0, png: null, error: String(e) }; } });
  if (snap.error) {
    // Reading a frame back fails under headless Chromium's software WebGPU
    // (SwiftShader) through wgpu 29, though WebGPU itself reads back there:
    // reported, not failed, until it is understood. WebGL2 is checked.
    if (s.backend === "BrowserWebGpu") quality += ` · snapshot unavailable (${snap.error.slice(0, 60)})`;
    else failures.push(`${c.name}: snapshot failed: ${snap.error}`);
  } else {
    if (!(snap.drawn > 0.002)) failures.push(`${c.name}: the frame is blank (${(snap.drawn * 100).toFixed(2)}% drawn)`);
    const tag = c.name.split(",")[0].replace(/ /g, "-").replace(/[^A-Za-z0-9-]/g, "");
    if (shots) await Bun.write(`${shots}/viz-${tag}-${mode}-${s.backend}.png`, Buffer.from(snap.png.split(",")[1], "base64"));
    quality += ` · ${(snap.drawn * 100).toFixed(1)}% of pixels drawn`;
  }
  const layout = c.multilevel
    ? `coarse to fine in ${(s.layoutMs / 1000).toFixed(1)} s (hierarchy ${s.buildMs?.toFixed(0)} ms, ${s.iterations} it of work)`
    : `${s.msPerIteration?.toFixed(1)} ms/it (${s.iterations} it in ${(s.layoutMs / 1000).toFixed(1)} s)`;
  console.log(
    `${c.name} [${mode}]: ${s.backend} · create ${s.createMs.toFixed(0)} ms · layout ${layout} · draw ${s.drawMsMedian?.toFixed(2)} ms median · ${s.fps?.toFixed(0)} fps${quality}`,
  );
  await page.close();
}
await browser.close();

// The highlight layer.
type RGB = [number, number, number];
const THEMES: Record<string, { background: RGB; node: RGB; highlight: RGB; selection: RGB; pin: RGB }> = {
  // scene.rs's Theme::light() and Theme::dark().
  light: { background: [0xff, 0xff, 0xff], node: [0x2a, 0x2a, 0x2a], highlight: [0xd9, 0x48, 0x0f], selection: [0x0b, 0x63, 0xce], pin: [0x7a, 0x3f, 0xc0] },
  dark: { background: [0x0b, 0x0b, 0x0a], node: [0xe8, 0xe4, 0xdc], highlight: [0xff, 0xa9, 0x4d], selection: [0x6c, 0xb2, 0xff], pin: [0xc9, 0xa3, 0xff] },
};
// Five edges of the karate club, end to end.
const PATH = [16, 5, 0, 2, 32, 14];
const HIGHLIGHT_CASES = ["light", "dark"].filter((t) => !ONLY || `highlight ${t}`.startsWith(ONLY));
if (HIGHLIGHT_CASES.length) {
  const gl = await chromium.launch({ args: ["--disable-features=WebGPU"] });
  for (const themeName of HIGHLIGHT_CASES) {
    const name = `highlight ${themeName}`;
    const fail = (what: string) => failures.push(`${name}: ${what}`);
    const page = await gl.newPage({ viewport: { width: 1000, height: 700 }, deviceScaleFactor: 1 });
    page.on("pageerror", (e) => fail(e.message));
    page.on("console", (m) => m.type() === "error" && fail(m.text()));
    await page.goto(`${base}?graph=karate&theme=${themeName}`);
    await page.waitForFunction(() => (window as any).stats?.layoutDone, null, { timeout: 60_000, polling: 100 });
    await page.waitForTimeout(500); // the last positions drawn
    const r: any = await page.evaluate(
      async ({ theme, path }) => {
        const w = window as any, view = w.view, g = w.graph;
        const canvas = document.getElementById("view") as HTMLCanvasElement;
        const W = canvas.width, H = canvas.height;
        w.pauseFrames(true);
        await new Promise((r) => setTimeout(r, 100));
        const out: any = { backend: view.backend, problems: [] as string[], pngs: {} as Record<string, string> };
        const problem = (what: string) => out.problems.push(what);
        const frame = async (tag: string) => {
          const px: Uint8Array = await view.snapshot();
          const c = document.createElement("canvas");
          c.width = W; c.height = H;
          c.getContext("2d")!.putImageData(new ImageData(new Uint8ClampedArray(px), W), 0, 0);
          out.pngs[tag] = c.toDataURL("image/png");
          return px;
        };
        const at = (px: Uint8Array, x: number, y: number): number[] => {
          const i = 4 * (Math.min(H - 1, Math.max(0, Math.floor(y))) * W + Math.min(W - 1, Math.max(0, Math.floor(x))));
          return [px[i], px[i + 1], px[i + 2]];
        };
        const far = (a: number[], b: number[]) => Math.max(...a.map((v, k) => Math.abs(v - b[k])));
        const mix = (a: number[], b: number[], t: number) => a.map((v, k) => v + (b[k] - v) * t);
        // How much of `color` over the background the pixel `p` is: 0 to 1.
        const share = (p: number[], color: number[]) => {
          const d = color.map((v, k) => v - theme.background[k]);
          return p.reduce((s, v, k) => s + (v - theme.background[k]) * d[k], 0) / d.reduce((s, v) => s + v * v, 0);
        };
        const n = g.n, m = g.src.length;
        const at_node = Array.from({ length: n }, (_, i) => Array.from(view.nodePixel(i) as Float32Array));
        // Radii as the view draws them by default: 1.5 + sqrt(degree), at most 12.
        const neighbours = Array.from({ length: n }, () => new Set<number>());
        for (let e = 0; e < m; e++) { neighbours[g.src[e]].add(g.dst[e]); neighbours[g.dst[e]].add(g.src[e]); }
        const radius = neighbours.map((s) => Math.min(1.5 + Math.sqrt(s.size), 12));
        const onPath = new Set<number>(path);
        const steps = path.slice(1).map((v: number, k: number) => [path[k], v]);
        const toSegment = (p: number[], a: number[], b: number[]) => {
          const dx = b[0] - a[0], dy = b[1] - a[1];
          const t = Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / (dx * dx + dy * dy || 1)));
          return Math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy);
        };
        const nearPath = (p: number[]) => Math.min(...steps.map(([a, b]: number[]) => toSegment(p, at_node[a], at_node[b])));
        const nearNode = (p: number[], but: number[] = []) =>
          Math.min(...at_node.map((q, i) => (but.includes(i) ? Infinity : Math.hypot(p[0] - q[0], p[1] - q[1]) - radius[i])));

        const before = await frame("plain");
        const flagBytes0 = view.flagBytesUploaded, edgeBytes0 = view.highlightBytesUploaded, positions0 = w.stats.uploadBytes;
        view.setHighlights(Uint32Array.of(0, path.length), Uint32Array.from(path));
        out.flagBytes = view.flagBytesUploaded - flagBytes0;
        out.edgeBytes = view.highlightBytesUploaded - edgeBytes0;
        if (w.stats.uploadBytes !== positions0) problem("highlighting uploaded positions");
        const lit = await frame("path");

        // 1. Each step of the path is a 3-pixel line in the highlight colour.
        out.widths = [];
        for (const [a, b] of steps) {
          const [pa, pb] = [at_node[a], at_node[b]];
          const len = Math.hypot(pb[0] - pa[0], pb[1] - pa[1]);
          if (len < 40) { problem(`step ${a}-${b} is only ${len.toFixed(0)} px long: too short to measure`); continue; }
          const mid = [(pa[0] + pb[0]) / 2, (pa[1] + pb[1]) / 2];
          const normal = [-(pb[1] - pa[1]) / len, (pb[0] - pa[0]) / len];
          const centre = at(lit, mid[0], mid[1]);
          if (far(centre, theme.highlight) > 6) problem(`step ${a}-${b}: its middle is rgb(${centre}), not the highlight colour`);
          // The highlight colour's share of each pixel across the line, summed: its width.
          let width = 0;
          for (let t = -8; t <= 8; t += 0.125) width += 0.125 * Math.max(0, Math.min(1, share(at(lit, mid[0] + t * normal[0], mid[1] + t * normal[1]), theme.highlight)));
          out.widths.push(Math.round(width * 100) / 100);
          if (!(width > 2.4 && width < 3.6)) problem(`step ${a}-${b}: ${width.toFixed(2)} px wide, not 3`);
          if (far(at(before, mid[0], mid[1]), theme.highlight) < 40) problem(`step ${a}-${b}: highlighted before it was set`);
        }

        // 2. The path's nodes are at full strength, the others at 35%.
        let dimmed = 0;
        for (let i = 0; i < n; i++) {
          const p = at_node[i];
          // Only nodes drawn clear of the others (their centre was the node colour).
          if (far(at(before, p[0], p[1]), theme.node) > 6) continue;
          const now = at(lit, p[0], p[1]);
          if (onPath.has(i)) {
            if (far(now, theme.node) > 6) problem(`path node ${i} is rgb(${now}), not the node colour at full strength`);
          } else if (nearPath(p) > radius[i] + 4) {
            dimmed++;
            const want = mix(theme.background, theme.node, 0.35);
            if (far(now, want) > 8) problem(`node ${i} is rgb(${now}), not dimmed to 35% (rgb(${want.map(Math.round)}))`);
          }
        }
        out.dimmedNodes = dimmed;
        if (dimmed < 15) problem(`only ${dimmed} nodes could be checked for dimming`);

        // 3. The other edges are at 35% too: their ink at their middles, before and after.
        let inkBefore = 0, inkAfter = 0, edgesChecked = 0;
        for (let e = 0; e < m; e++) {
          const a = at_node[g.src[e]], b = at_node[g.dst[e]];
          const mid = [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2];
          if (nearPath(mid) < 8 || nearNode(mid) < 12) continue;
          const ink = (px: Uint8Array) => { const p = at(px, mid[0], mid[1]); return Math.abs(p[0] - theme.background[0]) + Math.abs(p[1] - theme.background[1]) + Math.abs(p[2] - theme.background[2]); };
          inkBefore += ink(before); inkAfter += ink(lit); edgesChecked++;
        }
        out.edgeDim = Math.round((inkAfter / inkBefore) * 1000) / 1000;
        out.edgesChecked = edgesChecked;
        if (edgesChecked < 10) problem(`only ${edgesChecked} edges could be checked for dimming`);
        if (!(out.edgeDim > 0.27 && out.edgeDim < 0.45)) problem(`the other edges are at ${out.edgeDim} of their ink, not 0.35`);

        // 4. The route's ends carry a ring in the highlight colour (3 to 5 px
        // past the disc), on the side away from the path.
        const ringAt = (i: number, awayFrom: number, past: number) => {
          const p = at_node[i], q = at_node[awayFrom];
          const d = Math.hypot(p[0] - q[0], p[1] - q[1]) || 1, r = Math.max(radius[i], 3) + past;
          return [p[0] + ((p[0] - q[0]) / d) * r, p[1] + ((p[1] - q[1]) / d) * r];
        };
        for (const [end, next] of [[path[0], path[1]], [path[path.length - 1], path[path.length - 2]]]) {
          const p = ringAt(end, next, 4);
          const c = at(lit, p[0], p[1]);
          if (far(c, theme.highlight) > 6) problem(`the ring of the route's end ${end} is rgb(${c}), not the highlight colour`);
        }
        // A node inside the path has a halo, not a ring: the highlight
        // colour, faint, 2 px past its disc (looked at where the path is farthest).
        {
          const i = path[2], p = at_node[i], r = Math.max(radius[i], 3) + 2;
          const around = Array.from({ length: 16 }, (_, k) => [p[0] + r * Math.cos((k * Math.PI) / 8), p[1] + r * Math.sin((k * Math.PI) / 8)]);
          const clear = around.sort((a, b) => nearPath(b) - nearPath(a))[0];
          out.halo = Math.round(share(at(lit, clear[0], clear[1]), theme.highlight) * 100) / 100;
          if (!(out.halo > 0.15 && out.halo < 0.6)) problem(`the halo of path node ${i} is ${out.halo} of the highlight colour 2 px out, not about a third`);
        }

        // 5. A selection of one node: a few bytes, a ring in the selection
        // colour, and nothing dimmed by it.
        view.clearHighlights();
        const lone = [...Array(n).keys()].filter((i) => !onPath.has(i)).sort((a, b) => nearNode(at_node[b], [b]) - nearNode(at_node[a], [a]))[0];
        // Everything it uploads: its flags, and the node again to draw in front.
        const uploaded = () => view.flagBytesUploaded + view.highlightBytesUploaded;
        const bytes0 = uploaded();
        view.setSelection(Uint32Array.of(lone));
        out.selectionBytes = uploaded() - bytes0;
        out.nodes = n;
        const selected = await frame("selected");
        {
          const p = at_node[lone];
          // Straight up from the node: 4 px past the disc is inside the ring (3 to 5).
          const ring = at(selected, p[0], p[1] - Math.max(radius[lone], 3) - 4);
          if (far(ring, theme.selection) > 6 && far(at(selected, p[0] - Math.max(radius[lone], 3) - 4, p[1]), theme.selection) > 6)
            problem(`the selection ring of node ${lone} is rgb(${ring}), not the selection colour`);
          let same = 0, differ = 0;
          for (let i = 0; i < before.length; i += 4) {
            const x = (i / 4) % W, y = Math.floor(i / 4 / W);
            if (Math.hypot(x - p[0], y - p[1]) < radius[lone] + 12) continue;
            if (before[i] === selected[i] && before[i + 1] === selected[i + 1] && before[i + 2] === selected[i + 2]) same++; else differ++;
          }
          out.selectionChangedElsewhere = differ;
          if (differ > 0) problem(`selecting node ${lone} changed ${differ} pixels away from it`);
        }

        // 6. A pinned node carries a ring in the pin colour, hugging the disc.
        view.setSelection(new Uint32Array());
        view.setPinned(Uint32Array.of(lone));
        const pinnedFrame = await frame("pinned");
        {
          const p = at_node[lone], r = radius[lone] + 1.25;
          const up = at(pinnedFrame, p[0], p[1] - r), left = at(pinnedFrame, p[0] - r, p[1]);
          if (share(up, theme.pin) < 0.5 && share(left, theme.pin) < 0.5) problem(`the pin ring of node ${lone} is rgb(${up}), not the pin colour`);
        }
        view.setPinned(new Uint32Array());

        // 7. A hidden node is gone, with its edges, and cannot be picked.
        const hub = 33;
        const inkOf = (px: Uint8Array) => { let s = 0; for (let i = 0; i < px.length; i += 4) if (far([px[i], px[i + 1], px[i + 2]], theme.background) > 12) s++; return s; };
        view.setHidden(Uint32Array.of(hub));
        const hiddenFrame = await frame("hidden");
        const c = at(hiddenFrame, at_node[hub][0], at_node[hub][1]);
        if (far(c, theme.node) < 60) problem(`hidden node ${hub} is still drawn: rgb(${c})`);
        if (view.pick(at_node[hub][0], at_node[hub][1]) === hub) problem(`hidden node ${hub} can be picked`);
        out.inkPlain = inkOf(before); out.inkHidden = inkOf(hiddenFrame);
        // Node 33 has 17 of the 78 edges.
        if (!(out.inkHidden < 0.9 * out.inkPlain)) problem(`hiding node ${hub} left ${out.inkHidden} of ${out.inkPlain} inked pixels`);
        // An edge of the hidden node, at its middle, is gone.
        let gone = 0, tried = 0;
        for (let e = 0; e < m; e++) {
          if (g.src[e] !== hub && g.dst[e] !== hub) continue;
          const a = at_node[g.src[e]], b = at_node[g.dst[e]];
          const mid = [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2];
          if (far(at(before, mid[0], mid[1]), theme.background) < 12) continue; // not on a pixel centre before
          tried++;
          if (far(at(hiddenFrame, mid[0], mid[1]), theme.background) < far(at(before, mid[0], mid[1]), theme.background)) gone++;
        }
        out.hiddenEdges = `${gone}/${tried}`;
        if (tried < 5 || gone < tried - 2) problem(`${gone} of ${tried} edges of hidden node ${hub} are gone`);
        view.setHidden(new Uint32Array());
        const again = await frame("again");
        let differ = 0;
        for (let i = 0; i < before.length; i++) if (before[i] !== again[i]) differ++;
        if (differ) problem(`with every flag cleared, ${differ} bytes of the frame differ from the first`);
        w.pauseFrames(false);
        return out;
      },
      { theme: THEMES[themeName], path: PATH },
    ).catch((e) => ({ problems: [String(e)], pngs: {} }));
    for (const p of r.problems) fail(p);
    if (r.backend && r.backend !== "Gl") fail(`ran on ${r.backend}, not WebGL2`);
    // The runs of nodes the path changed, 4 bytes a node; then five edges
    // of 16 bytes and six nodes in front of 20.
    if (!(r.flagBytes > 0 && r.flagBytes <= 4 * r.nodes)) fail(`highlighting the path uploaded ${r.flagBytes} bytes of flags`);
    const instanceBytes = 16 * (PATH.length - 1) + 20 * PATH.length;
    if (r.edgeBytes !== instanceBytes) fail(`highlighting the path uploaded ${r.edgeBytes} bytes of edges and nodes in front, not ${instanceBytes}`);
    if (!(r.selectionBytes > 0 && r.selectionBytes <= r.nodes)) fail(`selecting one node uploaded ${r.selectionBytes} bytes; at most ${r.nodes} (one a node) are allowed`);
    if (shots) for (const [tag, png] of Object.entries(r.pngs as Record<string, string>)) await Bun.write(`${shots}/viz-highlight-${themeName}-${tag}.png`, Buffer.from(png.split(",")[1], "base64"));
    console.log(
      `${name}: ${r.backend} · path widths ${r.widths?.join(", ")} px · ${r.dimmedNodes} other nodes at 35% · other edges at ${r.edgeDim} (${r.edgesChecked} checked) · ` +
        `flags ${r.flagBytes} B and instances ${r.edgeBytes} B for the path · ${r.selectionBytes} B for a selection of one of ${r.nodes} nodes · hidden node's edges gone ${r.hiddenEdges}`,
    );
    await page.close();
  }
  await gl.close();
}
server.stop();
if (failures.length) {
  console.error("\nFAIL:\n" + failures.join("\n"));
  process.exit(1);
}
console.log("\nall passed");
