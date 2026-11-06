// End-to-end test of the Ursa Shell in headless Chromium, against a running
// site: `cf dev` (http://localhost:8787) or a deploy.
//
//   bun tests/e2e.ts [url] [screenshot dir]
//
// It runs every Tour step in order and one out of order, every example, every
// Data action and every function's "Try it", downloads a result in each
// format, switches views, and checks history and shared links. Any error shown
// by the page fails the run.
import { chromium, type Page } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:8787").replace(/\/$/, "");
const shots = Bun.argv[3];
const failures: string[] = [];

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1360, height: 900 }, acceptDownloads: true });
const page = await context.newPage();
page.on("pageerror", (e) => failures.push(`page error: ${e.message}`));

async function settle(p: Page = page) {
  await p.waitForFunction(() => !(document.getElementById("run") as HTMLButtonElement).disabled, null, { timeout: 120_000 });
}

/** The status line and whether the result is an error, after the current run. */
async function outcome(label: string, p: Page = page) {
  await settle(p);
  const status = (await p.textContent("#status")) ?? "";
  const error = await p.$eval("#status", (e) => e.classList.contains("error"));
  if (error) failures.push(`${label}: ${(await p.textContent("#output"))?.slice(0, 300)}`);
  console.log(`${error ? "FAIL" : "ok  "} ${label}: ${status}`);
  return { status, error };
}

async function panel(name: string) {
  await page.click(`[role=tab][data-panel=${name}]`);
}

const t0 = Date.now();
await page.goto(base);
await page.waitForFunction(() => document.getElementById("engine")?.textContent?.includes("ready"), null, { timeout: 120_000 });
console.log(`engine: ${await page.textContent("#engine")} (${Date.now() - t0} ms wall)`);

// The query the editor opens with.
await page.click("#run");
await outcome("opening query");
if (shots) await page.screenshot({ path: `${shots}/shell-table.png` });

// Every Tour step, in order, then step 3 again (which starts the Tour's session over).
await panel("tour");
const steps = await page.$$eval("#tour .item", (items) => items.map((i) => i.querySelector(".title")?.textContent ?? ""));
if (steps.length < 10) failures.push(`the Tour has ${steps.length} steps`);
for (let i = 0; i < steps.length; i++) {
  await page.click(`#tour li:nth-child(${i + 1}) .item`);
  await outcome(`tour ${i + 1} ${steps[i]}`);
}
await page.click("#tour li:nth-child(3) .item");
await outcome("tour 3 again");
if (shots) await page.screenshot({ path: `${shots}/shell-tour.png` });

// Every example (in the workspace session, with its predefined graphs).
await panel("examples");
const examples = await page.$$eval("#examples .item .title", (t) => t.map((x) => x.textContent ?? ""));
for (let i = 0; i < examples.length; i++) {
  await page.click(`#examples li:nth-child(${i + 1}) .item`);
  await outcome(`example ${examples[i]}`);
}

// Text view, then back to the table.
await page.click("#view-text");
const text = (await page.textContent("#output")) ?? "";
if (!text.includes("+-")) failures.push("the text view shows no ASCII table");
await page.click("#view-table");

// Data: each graph's actions and each file.
await panel("data");
const graphButtons = await page.$$("#graphs .actions button");
for (let i = 0; i < graphButtons.length; i++) {
  const label = await graphButtons[i].textContent();
  await (await page.$$("#graphs .actions button"))[i].click();
  await outcome(`data ${label}`);
}
await page.waitForSelector("#files .item");
const files = await page.$$("#files .item");
for (let i = 0; i < files.length; i++) {
  await (await page.$$("#files .item"))[i].click();
  await outcome(`file ${i + 1}`);
}

// Downloads of the current result (the last file's first rows), in each format.
for (const format of ["csv", "parquet", "json", "arrow"]) {
  const [download] = await Promise.all([page.waitForEvent("download"), page.selectOption("#download", format)]);
  const path = await download.path();
  const size = path ? Bun.file(path).size : 0;
  console.log(`${size > 0 ? "ok  " : "FAIL"} download ${format}: ${download.suggestedFilename()} ${size} bytes`);
  if (!size) failures.push(`download ${format} is empty`);
}

// Functions: load, filter, and try each one.
await panel("functions");
await page.waitForSelector("#functions .fn");
const names = await page.$$eval("#functions .fn .name", (n) => n.map((x) => x.textContent ?? ""));
console.log(`functions: ${names.length}`);
for (let i = 0; i < names.length; i++) {
  const fn = (await page.$$("#functions .fn"))[i];
  await fn.evaluate((d) => ((d as HTMLDetailsElement).open = true));
  await (await fn.$("button"))!.click();
  await outcome(`function ${names[i]}`);
}
await page.fill("#function-filter", "path");
const visible = await page.$$eval("#functions .fn", (f) => f.filter((x) => !(x as HTMLElement).hidden).length);
if (visible < 1 || visible >= names.length) failures.push(`filtering functions by "path" shows ${visible}`);

// History holds what ran.
await panel("history");
const history = await page.$$eval("#history .item", (h) => h.length);
if (history < 5) failures.push(`history has ${history} entries`);

// A shared link runs its query on load, in a fresh page.
const linked = await context.newPage();
await linked.goto(`${base}/#${new URLSearchParams({ sql: "SELECT count(*) AS members FROM karate.nodes;" })}`);
await linked.waitForFunction(() => document.getElementById("status")?.textContent?.includes("row"), null, { timeout: 120_000 });
const cell = await linked.textContent("#output td");
console.log(`${cell === "34" ? "ok  " : "FAIL"} shared link: ${cell}`);
if (cell !== "34") failures.push(`the shared link gave ${cell}`);

// A narrow screen.
if (shots) {
  const phone = await browser.newPage({ viewport: { width: 390, height: 844 } });
  await phone.goto(base);
  await phone.waitForFunction(() => document.getElementById("engine")?.textContent?.includes("ready"), null, { timeout: 120_000 });
  await phone.click("#run");
  await settle(phone);
  await phone.screenshot({ path: `${shots}/shell-phone.png`, fullPage: true });
}

await browser.close();
console.log(failures.length ? `\n${failures.length} failures:\n- ${failures.join("\n- ")}` : "\nall passed");
process.exit(failures.length ? 1 : 0);
