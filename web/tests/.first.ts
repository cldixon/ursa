import { chromium } from "playwright";
const base = Bun.argv[2] ?? "http://localhost:8880";
const runs = Number(Bun.argv[3] ?? 3);
const browser = await chromium.launch({ channel: "chromium" }).catch(() => chromium.launch());
for (let r = 0; r < runs; r++) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1 });
  const p = await ctx.newPage();
  await p.goto(`${base}/?dataset=rmat`);
  await p.waitForFunction(() => { const s = (window as any).ursaShell; return s && !s.busy() && s.screen().includes("memory U"); }, null, { timeout: 180000 });
  await p.evaluate(() => (window as any).ursaShell.idle());
  await p.click("#terminal");
  await p.keyboard.type(".graph rmat");
  await p.keyboard.press("Enter");
  await p.waitForFunction(() => { const t = (window as any).ursaShell.graphTimings(); return t?.graphDrawn && t.actions.some((a: any) => a.command.startsWith(".graph") && a.frame); }, null, { timeout: 60000, polling: 100 });
  const t = await p.evaluate(() => (window as any).ursaShell.graphTimings());
  const a = t.actions.find((a: any) => a.command.startsWith(".graph"));
  const f = (k: string) => Math.round(t[k]);
  const load = (await Bun.file("/proc/loadavg").text()).split(" ")[0];
  console.log(`FIRST frame ${Math.round(a.frame)} engine ${f("engine")} posted ${f("posted")} view ${f("view")} hierarchy ${f("hierarchy")} firstPositions ${f("firstPositions")} firstDraw ${f("firstDraw")} load ${load}`);
  await ctx.close();
}
await browser.close();
