// The tables of BENCH.md, from a results file the bench wrote:
//
//   bun bench/report.ts [bench/results.json]
//
// Prints Markdown: the machine, then a table per backend, then the native
// layout timings. Paste it into BENCH.md's "Results" section after a rerun.
import { resolve } from "node:path";

const path = Bun.argv[2] ?? resolve(import.meta.dir, "results.json");
const r: any = await Bun.file(path).json();

const ms = (x: number | null | undefined, d = 0) =>
  x == null ? "–" : x >= 10_000 ? `${(x / 1000).toFixed(1)} s` : x.toFixed(d);
const t = (x: number | null | undefined) => (x == null ? "–" : x >= 10_000 ? `${(x / 1000).toFixed(1)} s` : `${x.toFixed(0)} ms`);
const mb = (b: number | null | undefined) => (b == null ? "–" : (b / 2 ** 20).toFixed(1));
const pair = (s: any, d = 1) => (s?.median == null ? "–" : `${ms(s.median, d)} / ${ms(s.p95, d)}`);
const label = (c: any) => `${c.graph} ${c.size}`;

const m = r.machine;
const out: string[] = [];
out.push(`Run ${r.date.slice(0, 10)} on ${m.cpu}, ${m.cores} cores, ${(m.memoryBytes / 2 ** 30).toFixed(0)} GiB, ${m.platform};`);
out.push(`Chromium ${m.chromium}, ${m.headless ? "headless" : "headed"}; backends ${Object.entries(m.backends).map(([k, v]) => `${k} → ${v}`).join(", ")}.`);
out.push("");

const backends = [...new Set(r.browser.map((c: any) => c.requested))];
for (const b of backends) {
  const cases = r.browser.filter((c: any) => c.requested === b);
  const ml = cases.filter((c: any) => c.mode === "multilevel");
  const ran = ml.find((c: any) => c.backend)?.backend ?? "?";
  const sync = ml.find((c: any) => c.syncKind)?.syncKind ?? "?";
  out.push(`### ${b} (ran as ${ran})`);
  out.push("");
  out.push(`Frame sync: ${sync}.`);
  out.push("");
  out.push("| graph | nodes | edges | first frame | layout done, coarse to fine | flat, ms/it | pan frame, ms (median / p95) | zoom frame, ms | burst, ms/frame | rAF interval, ms | pick, ms (median / p95) | grid, ms | nodes drawn at 1% | wasm MiB (page / worker) | JS heap MiB | uploads × bytes |");
  out.push("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
  for (const c of ml) {
    if (c.error) {
      out.push(`| ${label(c)} | – | – | error: ${c.error.slice(0, 60)} |||||||||||`);
      continue;
    }
    const flat = cases.find((f: any) => f.mode === "flat" && f.graph === c.graph && f.size === c.size);
    out.push(
      `| ${label(c)} | ${c.nodes.toLocaleString("en")} | ${c.edges.toLocaleString("en")} | ${t(c.firstFrameMs)} | ${t(c.layoutDoneMs)} | ${flat?.msPerIteration ?? "–"} | ${pair(c.pan?.frameMs)} | ${pair(c.zoom?.frameMs)} | ${ms(c.burstMsPerFrame?.median, 1)} | ${ms(c.pan?.rafIntervalMs?.median, 1)} | ${pair(c.pickMs, 3)} | ${ms(c.gridBuildMs?.median, 2)} | ${c.culledNodes?.toLocaleString("en") ?? "–"} | ${mb(c.memory?.pageWasmBytes)} / ${mb(c.memory?.workerWasmBytes)} | ${mb(c.memory?.jsHeapUsedBytes)} | ${c.uploads} × ${c.bytesPerUpload.toLocaleString("en")} |`,
    );
  }
  out.push("");
}

for (const [label, n] of Object.entries(r.native ?? {}) as [string, any][]) {
  if (!n?.graphs) continue;
  out.push(`### Native layout, ${n.rayon ? `rayon, ${n.threads} threads` : "one thread (as in the browser)"}`);
  out.push("");
  out.push("| graph | nodes | undirected edges | levels | hierarchy | coarse to fine | flat, ms/it |");
  out.push("|---|---|---|---|---|---|---|");
  for (const g of n.graphs)
    out.push(`| ${g.graph} ${g.size} | ${g.nodes.toLocaleString("en")} | ${g.edges.toLocaleString("en")} | ${g.levels} | ${t(g.hierarchyMs)} | ${t(g.multilevelMs)} | ${g.flatMsPerIteration} |`);
  out.push("");
}
console.log(out.join("\n"));
