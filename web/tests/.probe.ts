import { chromium } from "playwright";
const base = Bun.argv[2] ?? "http://localhost:8880";
const cmds = Bun.argv.slice(3);
const browser = await chromium.launch({ channel: "chromium" }).catch(() => chromium.launch());
const p = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
p.on("console", (m) => { if (m.text().startsWith("PROBE")) console.log(m.text()); });
console.log("go"); await p.goto(`${base}/?dataset=rmat`);
await p.waitForFunction(() => { const s = (window as any).ursaShell; return s && !s.busy() && s.screen().includes("memory U"); }, null, { timeout: 180000 });
console.log("loaded"); await p.evaluate(() => (window as any).ursaShell.idle()); console.log("idle");
for (const c of cmds) {
  await p.click("#terminal");
  await p.keyboard.type(c);
  await p.keyboard.press("Enter");
  await p.waitForFunction((c) => { const t = (window as any).ursaShell.graphTimings(); return t?.done && t.command === c.trim(); }, c, { timeout: 30000, polling: 100 }).catch((e) => console.log("timeout", c));
  const t = await p.evaluate(() => (window as any).ursaShell.graphTimings());
  const rel: any = {}; for (const [k, v] of Object.entries(t)) if (typeof v === "number" && k !== "enter") rel[k] = Math.round(v as number);
  const a = t.actions.find((a: any) => a.command.startsWith(".graph"));
  console.log(c, JSON.stringify(rel), "frame", Math.round(a?.frame), JSON.stringify(t.memory ?? {}).slice(0, 200));
}
await browser.close();
