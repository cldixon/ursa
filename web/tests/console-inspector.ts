// End-to-end test of the console's Inspector panel (URSA-23,
// web/console/inspector.js) over `ursa serve`, in headless Chromium.
//
//   bun tests/console-inspector.ts [port] [ursa binary] [screenshot dir]
//   bun run test:console-inspector -- 9131
//
// It starts `ursa serve` on a root of its own (data/ with the karate club's
// Parquet files from web/.data) with this checkout's pages (`bun run build`
// first; the binary as tests/console.ts finds it: URSA_BIN, or
// $CARGO_TARGET_DIR/release/ursa), and checks:
//
// - with nothing selected, the panel says what to select;
// - selecting a node in a result's Graph view shows it in the Inspector
//   panel, docked (no inspector floats over the view): its label, degree and
//   row, and the buttons expand, hide, pin and path from here;
// - selecting an edge shows its ends and its row;
// - expand in the Inspector goes through the commands sink: History lists
//   the step, from the inspector, and the graph grows;
// - selecting a table row (pointer, then keyboard) shows the whole row, one
//   field a line, long values in full and NULL as NULL; the row is marked;
// - what it shows is announced politely; when the selection goes, it falls
//   back to the other selection, then to its empty state;
// - no page errors.
//
// It waits for conditions, never a fixed time, so it holds on a slow runner.
//
// With a screenshot dir, it saves the console at 1440×900 in light and dark
// with a node selected (inspector-node-light-1440x900.png, …-dark-…) and with
// a table row selected (inspector-row-light-1440x900.png, …-dark-…).
import { chromium, type Page } from "playwright";
import { copyFileSync, mkdirSync, mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { cargoTargetDir, DATA_DIR, WEB } from "../scripts/common";

const port = Number(Bun.argv[2] ?? 9131);
const bin = Bun.argv[3] || process.env.URSA_BIN || join(cargoTargetDir(), "release", "ursa");
const shots = Bun.argv[4];
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  detail = detail.replace(/\s+/g, " ");
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(`${label}${detail ? `: ${detail}` : ""}`);
};

// ---------------------------------------------------------------- the server

const root = realpathSync(mkdtempSync(join(tmpdir(), "ursa-inspector-")));
mkdirSync(join(root, "data"), { recursive: true });
for (const f of ["karate_nodes.parquet", "karate_edges.parquet"]) copyFileSync(join(DATA_DIR, f), join(root, "data", f));
const server = Bun.spawn([bin, "serve", root, "--port", String(port), "--no-open", "--assets", join(WEB, "dist-console"), "--assets", join(WEB, "dist")], { stdout: "pipe", stderr: "inherit" });
process.on("exit", () => {
  server.kill();
  rmSync(root, { recursive: true, force: true });
});
let printed = "";
let link: string | undefined;
{
  const reader = server.stdout.getReader();
  while (!link || !/once the link has opened\.\n/.test(printed)) {
    const { value, done } = await reader.read();
    if (done) throw new Error(`ursa serve stopped:\n${printed}`);
    printed += new TextDecoder().decode(value);
    link = printed.match(/http:\/\/127\.0\.0\.1:\d+\/\S*\?token=[A-Za-z0-9_-]+/)?.[0];
  }
  reader.releaseLock();
}
console.log(printed.trim());

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
const errors: string[] = [];
function watch(p: Page, name: string) {
  p.on("pageerror", (e) => errors.push(`${name}: page error: ${e.message}`));
  p.on("console", (m) => {
    if (m.type() === "error" && !/WebSocket|Failed to load resource/.test(m.text())) errors.push(`${name}: console error: ${m.text()}`);
  });
}
const page = await context.newPage();
watch(page, "page");
// Light, and room for the Inspector over History.
await page.addInitScript(() => {
  try {
    if (!sessionStorage.getItem("inspector-test")) {
      sessionStorage.setItem("inspector-test", "1");
      localStorage.setItem("ursa-console-layout", JSON.stringify({ right: 340, rows: { main: 0.24, right: 0.64 } }));
      localStorage.setItem("ursa-theme", "light");
    }
  } catch {}
});
await page.goto(link!);
await page.waitForFunction(() => (window as any).ursaConsole?.session.state === "connected", null, { timeout: 30_000 });

/** Wait for `fn(arg)` to hold in the page; whether it did. */
const until = <A>(fn: (arg: A) => unknown, arg?: A, timeout = 20_000) =>
  page.waitForFunction(fn as any, arg, { timeout, polling: 50 }).then(
    () => true,
    () => false,
  );
/** The Inspector's state (inspector.js's state()). */
const inspector = (): Promise<any> => page.evaluate(() => (window as any).ursaConsole.inspector.state());
/** The docked graph inspector's state (graph-inspector.js's state()), or null. */
const graphInspected = (): Promise<any> => page.evaluate(() => (window as any).ursaConsole.results.pane()?.view()?.inspected() ?? null);
const announced = () => page.evaluate(() => document.querySelector('#inspector [aria-live="polite"]')?.textContent ?? "");
const row = (shown: any, i = 0): Record<string, string | null> => Object.fromEntries(shown?.rows?.[i] ?? []);

// ---------------------------------------------------------------- empty

{
  const empty = page.locator("#inspector .ix-empty");
  const s = await inspector();
  check(
    (await page.locator("#inspector .placeholder").count()) === 0 && (await empty.isVisible()) && s.showing === null && /Nothing selected/.test(s.text) && /Select a row .* node or an edge/.test(s.text),
    "with nothing selected, the Inspector says what to select",
    s.text,
  );
}

// ---------------------------------------------------------------- a node

const run = (sql: string) => page.evaluate((sql) => (window as any).ursaConsole.run(sql).then((r: any) => ({ id: r.tab?.id ?? null, error: r.error?.message ?? null })), sql);
for (const sql of [
  "CREATE VIEW members AS SELECT * FROM 'data/karate_nodes.parquet'",
  "CREATE VIEW ties AS SELECT * FROM 'data/karate_edges.parquet'",
  "CREATE PROPERTY GRAPH karate VERTEX TABLES (members KEY (id)) EDGE TABLES (ties SOURCE KEY (src) REFERENCES members (id) DESTINATION KEY (dst) REFERENCES members (id))",
])
  await run(sql);
const HOP = "SELECT * FROM hop(karate, seeds => [0], steps => 1, direction => 'both')";
const hop = await run(HOP);
check(Boolean(hop.id), "the hop query opens its tab", JSON.stringify(hop));
await page.evaluate((id) => (window as any).ursaConsole.results.tabs().find((t: any) => t.id === id).show("graph"), hop.id);
const drawn = await until(() => (window as any).ursaConsole.results.pane()?.view()?.info()?.nodes > 0, undefined, 60_000);
check(drawn, "its Graph view draws");
/** Wait until the layout is done and drawn, and the camera still: nodes stay where they are clicked. */
async function settled() {
  const ok = await until(() => {
    const i = (window as any).ursaConsole.results.pane()?.view()?.info();
    return i && i.frames > 0 && i.layoutDone && !i.pending && !i.fitting;
  }, undefined, 60_000);
  if (!ok) console.log("     (the layout did not settle in 60 s)");
}
// The layout settled and the not-shown counts in: nodes stay where they are clicked.
await until(() => (window as any).ursaConsole.results.pane()?.view()?.info()?.notShownKnown === true, undefined, 30_000);
await settled();

const canvas = page.locator("#results-graph-stage canvas");
const box = (await canvas.boundingBox())!;
const place = (id: number | string) => page.evaluate((id) => (window as any).ursaConsole.results.pane().view().nodePlace(id), id);

// A node with neighbours not shown, so that expand adds some.
const target = (await page.evaluate(() => (window as any).ursaConsole.results.pane().view().expandable(5)))[0];
check(Boolean(target), "a node to expand is in reach", JSON.stringify(target));
await page.mouse.click(box.x + target.x, box.y + target.y);
const nodeShown = await until((key) => {
  const s = (window as any).ursaConsole.inspector.state();
  const g = (window as any).ursaConsole.results.pane()?.view()?.inspected();
  return s.showing === "graph" && g?.kind === "node" && g.title === `node ${key}` && Array.isArray(g.rows) && g.rows.length > 0;
}, String(target?.id));
let s = await inspector();
let g = await graphInspected();
check(
  nodeShown && g.table === "members" && row(g).id === String(target.id) && /club/.test(s.text) && s.text.includes(String(row(g).club)),
  "selecting a node shows its row in the Inspector",
  `${JSON.stringify(g?.rows)} | ${s.text.slice(0, 200)}`,
);
check(/degree\s*\d+ drawn/.test(g?.text ?? "") && g.title === `node ${target.id}`, "and its key and degree", g?.text?.slice(0, 120));
check(["expand", "hide", "pin", "path from here"].every((b) => g?.buttons.includes(b)), "with the graph pane's buttons: expand, hide, pin, path from here", JSON.stringify(g?.buttons));
const docked = await page.evaluate(() => ({
  inPanel: document.querySelectorAll("#inspector .graph-inspector.docked:not([hidden])").length,
  inStage: document.querySelectorAll("#results-graph-stage .graph-inspector").length,
  close: document.querySelectorAll("#inspector .graph-inspector .inspector-close").length,
}));
check(docked.inPanel === 1 && docked.inStage === 0 && docked.close === 0, "one inspector: docked in the panel, none floating over the view", JSON.stringify(docked));
check(await until((key) => (document.querySelector('#inspector [aria-live="polite"]')?.textContent ?? "").includes(`node ${key}`), String(target.id), 5_000), "the selection is announced politely", await announced());

// The keyboard: i in the view moves into the Inspector's panel, Esc back.
await canvas.focus();
await page.keyboard.press("i");
const inPanel = await page.evaluate(() => Boolean(document.activeElement?.closest("#inspector")));
await page.keyboard.press("Tab");
const onButton = await page.evaluate(() => document.activeElement?.tagName === "BUTTON" && Boolean(document.activeElement.closest("#inspector")));
await page.keyboard.press("Escape");
const back = await page.evaluate(() => document.activeElement?.tagName === "CANVAS");
check(inPanel && onButton && back && (await inspector()).showing === "graph", "i moves into the Inspector, Tab reaches its buttons, Esc goes back to the graph", `${inPanel} ${onButton} ${back}`);

// Screenshots: a node selected.
async function screenshot(name: string) {
  if (!shots) return;
  mkdirSync(shots, { recursive: true });
  // The pointer off the graph and the table: no tooltip, no hover.
  await page.mouse.move(1300, 880);
  for (const theme of ["light", "dark"] as const) {
    await page.evaluate((t) => (document.documentElement.dataset.theme = t), theme);
    await page.evaluate(() => (document.activeElement as HTMLElement)?.blur?.());
    // The graph redraws in the theme's colours: two frames.
    await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    await page.screenshot({ path: join(shots, `inspector-${name}-${theme}-1440x900.png`) });
  }
  await page.evaluate(() => (document.documentElement.dataset.theme = "light"));
}
await screenshot("node");

// ---------------------------------------------------------------- expand from the Inspector

const before = await page.evaluate(() => (window as any).ursaConsole.results.pane().view().info().nodes);
await page.locator("#inspector .graph-inspector .inspector-actions button", { hasText: /^expand$/ }).click();
const stepped = await until(
  (key) => (window as any).ursaConsole.history.entries().some((e: any) => e.op === "graph.explore" && e.summary === `.expand ${key}` && !e.error),
  String(target.id),
  30_000,
);
const step = (await page.evaluate(() => (window as any).ursaConsole.history.entries())).find((e: any) => e.op === "graph.explore");
const grew = await until((n) => (window as any).ursaConsole.results.pane().view().info().nodes > n, before, 30_000);
const after = await page.evaluate(() => (window as any).ursaConsole.results.pane().view().info().nodes);
check(stepped && step?.via === "inspector" && grew, "expand in the Inspector adds the step to History, from the inspector, and grows the graph", `${JSON.stringify(step)} nodes ${before} → ${after}`);
const listed = page.locator("#history .hx-row", { hasText: `.expand ${target.id}` });
check(await listed.first().waitFor({ timeout: 10_000 }).then(() => true, () => false), "History shows the step", `.expand ${target.id}`);

// ---------------------------------------------------------------- an edge

// Clear the selection (Esc in the view), then click an edge between 0 and a
// neighbour, at the middle of it where no node is.
await canvas.focus();
await page.keyboard.press("Escape");
await until(() => (window as any).ursaConsole.inspector.state().showing === null, undefined, 5_000);
check((await inspector()).showing === null && (await page.locator("#inspector .ix-empty").isVisible()), "clearing the selection brings the empty state back");
await settled();
// Every node shown but 0: edgeAt says which have an edge from 0 at the middle.
const neighbours: string[] = (await page.evaluate(() => (window as any).ursaConsole.results.pane().view().placement().ids)).filter((id: string) => id !== "0");
let edge: any = null;
for (const other of neighbours) {
  const [a, b] = [await place(0), await place(other)];
  if (!a || !b || a.hidden || b.hidden) continue;
  const mid = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
  const under = await page.evaluate(([x, y]) => (window as any).ursaConsole.results.pane().view().edgeAt(x, y), [mid.x, mid.y]);
  if (under && String(under.src) === "0" && String(under.dst) === other) {
    edge = { other, mid };
    break;
  }
}
check(Boolean(edge), "an edge from 0 is in reach", JSON.stringify(edge));
if (edge) {
  await page.mouse.click(box.x + edge.mid.x, box.y + edge.mid.y);
  const edgeShown = await until((title) => {
    const g = (window as any).ursaConsole.results.pane()?.view()?.inspected();
    return (window as any).ursaConsole.inspector.state().showing === "graph" && g?.kind === "edge" && g.title === title && Array.isArray(g.rows) && g.rows.length > 0;
  }, `edge 0 → ${edge.other}`);
  g = await graphInspected();
  s = await inspector();
  check(
    edgeShown && g.table === "ties" && row(g).src === "0" && row(g).dst === edge.other && s.title === `edge 0 → ${edge.other}` && /from\s*0/.test(s.text) && new RegExp(`to\\s*${edge.other}`).test(s.text),
    "selecting an edge shows its ends and its row in the Inspector",
    `${JSON.stringify(g?.rows)} | ${s.text.slice(0, 160)}`,
  );
}

// ---------------------------------------------------------------- a table row

const LONG = "a long value, ".repeat(30).trim();
const ROWS = `SELECT id, club, '${LONG}' || CAST(id AS VARCHAR) AS note, CAST(NULL AS VARCHAR) AS nothing FROM members ORDER BY id`;
const rows = await run(ROWS);
check(Boolean(rows.id), "a query with a long column opens its tab", JSON.stringify(rows));
// Its table shown; the graph's selection stays, and the Inspector still shows it until a row is picked.
await until((id) => (window as any).ursaConsole.results.selected()?.id === id, rows.id);
check((await inspector()).showing === "graph", "a new tab leaves the Inspector on the graph's selection");
const grid = page.locator(".rs-panel:not([hidden]) .rt-scroll");
await grid.waitFor();
// The pointer: the third row's club.
const cell = page.locator(".rs-panel:not([hidden]) .rt-body .rt-row[data-v='2'] .rt-cell").nth(2);
await cell.click();
const rowShown = await until(() => {
  const s = (window as any).ursaConsole.inspector.state();
  return s.showing === "row" && s.title === "row 3" && s.fields?.every((f: any) => f[1] !== undefined);
});
s = await inspector();
const fields = Object.fromEntries(s.fields ?? []);
check(rowShown && fields.id === "2" && fields.note === `${LONG}2` && fields.nothing === null && typeof fields.club === "string", "selecting a table row shows the full row", JSON.stringify(s.fields)?.slice(0, 300));
const lines = await page.evaluate(() =>
  [...document.querySelectorAll("#inspector .ix-field")].map((f) => ({ name: f.querySelector(".ix-name")?.textContent, value: f.querySelector("dd")?.textContent, height: (f as HTMLElement).offsetHeight })),
);
check(
  lines.length === 4 && lines.map((l) => l.name).join() === "id,club,note,nothing" && lines[2].value === `${LONG}2` && lines[2].height > lines[0].height * 2 && lines[3].value === "NULL",
  "one field a line, the long value whole (wrapped), NULL as NULL",
  JSON.stringify(lines.map((l) => [l.name, l.value?.length, l.height])),
);
const marked = await page.evaluate(() => [...document.querySelectorAll(".rs-panel:not([hidden]) .rt-body .rt-row.current")].map((r) => (r as HTMLElement).dataset.v));
check(marked.join() === "2", "the table marks the selected row", JSON.stringify(marked));
check(await until(() => (document.querySelector('#inspector [aria-live="polite"]')?.textContent ?? "").includes("row 3"), undefined, 5_000), "the row is announced politely", await announced());

// The keyboard: Down moves the selection to the next row.
await page.keyboard.press("ArrowDown");
const down = await until(() => (window as any).ursaConsole.inspector.state().title === "row 4", undefined, 5_000);
check(down && Object.fromEntries((await inspector()).fields).id === "3", "the keyboard moves the selected row (Down: row 4)", (await inspector()).title);
// The fields are a Tab stop, labelled.
const label = await page.locator("#inspector .ix-fields").getAttribute("aria-label");
check((await page.locator("#inspector .ix-fields").getAttribute("tabindex")) === "0" && /row 4 of /.test(label ?? ""), "the row's fields are one Tab stop, labelled", String(label));
// Back to the first row for the screenshots.
await page.keyboard.press("ArrowUp");
await until(() => (window as any).ursaConsole.inspector.state().title === "row 3", undefined, 5_000);
await screenshot("row");

// ---------------------------------------------------------------- the other selection, then nothing

// Closing the row's tab: back to the graph's edge, still selected.
await page.evaluate((id) => (window as any).ursaConsole.results.tabs().find((t: any) => t.id === id).close(), rows.id);
const fellBack = await until(() => (window as any).ursaConsole.inspector.state().showing === "graph", undefined, 5_000);
check(fellBack, "closing the row's tab goes back to the graph's selection", JSON.stringify((await inspector()).showing));
// And clearing that: nothing selected.
await page.evaluate((id) => (window as any).ursaConsole.results.tabs().find((t: any) => t.id === id).show("graph"), hop.id);
await until(() => (window as any).ursaConsole.results.pane()?.isOpen(), undefined, 10_000);
await canvas.focus();
await page.keyboard.press("Escape");
const nothing = await until(() => (window as any).ursaConsole.inspector.state().showing === null, undefined, 5_000);
check(nothing && (await page.locator("#inspector .ix-empty").isVisible()), "with neither selected, the empty state shows again");

check(errors.length === 0, "no page errors", errors.join(" | "));
await browser.close();
if (failures.length) {
  console.log(`\n${failures.length} failed:\n  ${failures.join("\n  ")}`);
  process.exit(1);
}
console.log("\nall passed");
process.exit(0);
