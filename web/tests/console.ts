// End-to-end test of the Ursa Console (web/console/, design/console.md §6)
// over `ursa serve`, in headless Chromium.
//
//   bun tests/console.ts [port] [ursa binary] [screenshot dir]
//   bun run test:console -- 8981
//
// It starts `ursa serve .data --port PORT` with this checkout's pages
// (dist-console/ and dist/: `bun run build` first) from a build with
// `--features serve` (by default $CARGO_TARGET_DIR/release/ursa; URSA_BIN=
// also names it), opens the link it prints, and checks:
//
// - the page connects to the shared session and says so, with the engine's
//   version and the session's root;
// - the six panels are in place, labelled, each with its mount point and the
//   issue that fills it, in both themes, with no page error and no Content
//   Security Policy violation;
// - the keyboard: the skip links, F6 between panels, the splitters' keys,
//   folding a panel and hiding a column, visible focus;
// - a phone's width: the side columns become drawers, Esc closes one, and
//   nothing scrolls sideways;
// - Cancel stops a long query (`cancel {target}`) and the session goes on;
// - when the server stops, the page says so and tries again; when it comes
//   back with a new token, the page says to open its link, and connects once
//   it has been opened;
// - the shell is still at /.
//
// With a screenshot dir, it saves the console in light and dark at 1440×900
// and 390×844.
import { chromium, type Page } from "playwright";
import { realpathSync } from "node:fs";
import { join } from "node:path";
import { cargoTargetDir, DATA_DIR, WEB } from "../scripts/common";

const port = Number(Bun.argv[2] ?? 8981);
const bin = Bun.argv[3] || process.env.URSA_BIN || join(cargoTargetDir(), "release", "ursa");
const shots = Bun.argv[4];
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(`${label}${detail ? `: ${detail}` : ""}`);
};
/** A long statement: a sum over 10^11 rows, which runs until it is cancelled. */
const FOREVER = "SELECT sum(value) AS s FROM range(0, 100000000000)";

/** Start `ursa serve`; resolves to it and the link it printed. */
async function serve() {
  const server = Bun.spawn([bin, "serve", DATA_DIR, "--port", String(port), "--assets", join(WEB, "dist-console"), "--assets", join(WEB, "dist")], {
    stdout: "pipe",
    stderr: "inherit",
  });
  const reader = server.stdout.getReader();
  let printed = "";
  let link: string | undefined;
  // Up to its last line, which says where the shell is.
  while (!link || !/once the link has opened\.\n/.test(printed)) {
    const { value, done } = await reader.read();
    if (done) throw new Error(`ursa serve stopped:\n${printed}`);
    printed += new TextDecoder().decode(value);
    link = printed.match(/http:\/\/127\.0\.0\.1:\d+\/\S*\?token=[A-Za-z0-9_-]+/)?.[0];
  }
  reader.releaseLock();
  return { server, link, printed };
}

/** Stop the server as Ctrl+C does; whether it stopped gracefully. */
async function stop(server: ReturnType<typeof Bun.spawn>) {
  server.kill("SIGINT");
  const code = await Promise.race([server.exited, Bun.sleep(10_000).then(() => "timeout" as const)]);
  if (code !== 0) server.kill();
  return code === 0;
}

let { server, link, printed } = await serve();
console.log(printed.trim());
const served = Number(new URL(link).port);
const base = `http://127.0.0.1:${served}`;
check(new URL(link).pathname === "/console/", "ursa serve's link opens the console", link);
check(printed.includes(`The Web Shell is at ${base}/`), "ursa serve says where the shell is", printed);

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
const violations: string[] = [];
await context.addInitScript(() => {
  document.addEventListener("securitypolicyviolation", (e) => console.error(`CSP violation: ${e.violatedDirective} blocked ${e.blockedURI || "inline"}`));
});
const errors: string[] = [];
function watch(p: Page) {
  p.on("pageerror", (e) => errors.push(`page error: ${e.message}`));
  p.on("console", (m) => {
    if (m.type() !== "error") return;
    if (/CSP violation|Content Security Policy/.test(m.text())) violations.push(m.text());
    // A WebSocket that cannot connect logs an error: expected while the
    // server is away, checked below.
    else if (!/WebSocket|Failed to load resource/.test(m.text())) errors.push(`console error: ${m.text()}`);
  });
}
const page = await context.newPage();
watch(page);
const text = (sel: string) => page.locator(sel).innerText();
const connected = () => page.waitForFunction(() => (window as any).ursaConsole?.session.state === "connected", null, { timeout: 30_000 });

// ---------------------------------------------------------------- load

await page.goto(link);
check(new URL(page.url()).pathname === "/console/" && !page.url().includes("token"), "the link's token leaves the address", page.url());
await connected();
check((await text("#connection")).startsWith("Connected"), "the status bar says it is connected", await text("#connection"));
const hello = await page.evaluate(() => (window as any).ursaConsole.session.hello);
check(hello?.session?.kind === "shared", "the console's session is the shared one", JSON.stringify(hello?.session));
check(hello?.transport === "native" && (await text("#engine")).includes(`ursa ${hello.version}`) && (await text("#engine")).includes("native"), "the status bar names the engine", await text("#engine"));
const root = realpathSync(DATA_DIR);
check((await page.locator("#root-path").textContent()) === hello?.session?.cwd && hello?.session?.cwd === root, "the status bar shows the session's root", `${await page.locator("#root-path").textContent()} (served ${root})`);
check(await page.locator("#running").isHidden(), "no request is running");

// ---------------------------------------------------------------- panels

const PANELS: [string, string, string][] = [
  ["sources", "Sources", "URSA-16"],
  ["catalog", "Catalog", "URSA-15"],
  ["editor", "Editor", ""],
  ["results", "Results", "URSA-14"],
  ["inspector", "Inspector", ""],
  ["history", "History", "URSA-17"],
];
for (const [id, title, issue] of PANELS) {
  const panel = page.locator(`#${id}`);
  const name = await page.evaluate((id) => {
    const el = document.getElementById(id)!;
    return document.getElementById(el.getAttribute("aria-labelledby")!)?.textContent?.trim();
  }, id);
  const visible = await panel.isVisible();
  const mount = await page.locator(`#${id} [data-mount="${id}"]`).count();
  // A panel filled in has no placeholder.
  const placeholder = issue ? await page.locator(`#${id} .placeholder`).innerText() : "";
  check(
    visible && name === title && mount === 1 && (!issue || placeholder.includes(issue)),
    `the ${title} panel: labelled, with its mount point${issue ? ` and ${issue}` : ""}`,
    `${visible} ${name} ${mount} ${JSON.stringify(placeholder)}`,
  );
}
const mounts = await page.evaluate(() => Object.entries((window as any).ursaConsole.panels).map(([k, v]: [string, any]) => `${k}=${v.dataset.mount}`));
check(mounts.join(" ") === "sources=sources catalog=catalog editor=editor results=results inspector=inspector history=history", "ursaConsole.panels names each mount point", mounts.join(" "));
const order = await page.evaluate(() => {
  const boxes = ["left", "main", "right"].map((id) => document.getElementById(id)!.getBoundingClientRect());
  return boxes.every((b, i) => i === 0 || b.left >= boxes[i - 1].right - 1) && boxes.every((b) => b.width > 0);
});
check(order, "three columns: sources and catalog, the editor and results, the inspector and history");

async function screenshot(name: string) {
  if (!shots) return;
  await page.waitForTimeout(150);
  await page.screenshot({ path: join(shots, name) });
}

// Both themes: the ground follows the theme, and the button flips it.
const ground = () => page.evaluate(() => getComputedStyle(document.body).backgroundColor);
await page.evaluate(() => {
  document.documentElement.dataset.theme = "light";
});
const light = await ground();
await screenshot("console-light-1440x900.png");
await page.click("#theme");
const dark = await ground();
check((await page.evaluate(() => document.documentElement.dataset.theme)) === "dark" && light !== dark, "the theme button switches to dark", `${light} → ${dark}`);
await screenshot("console-dark-1440x900.png");
await page.reload();
await connected();
check((await page.evaluate(() => document.documentElement.dataset.theme)) === "dark", "the theme is kept across a reload");
await page.click("#theme");
check((await page.evaluate(() => document.documentElement.dataset.theme)) === "light", "and back to light");

// ---------------------------------------------------------------- keyboard

// A fresh load: the keyboard starts at the top of the page.
await page.reload();
await connected();
await page.keyboard.press("Tab");
const first = await page.evaluate(() => {
  const el = document.activeElement as HTMLElement;
  const box = el.getBoundingClientRect();
  return { text: el.textContent, visible: box.top >= 0 && box.bottom <= innerHeight && box.width > 0, outline: getComputedStyle(el).outlineStyle };
});
check(first.text === "Skip to the editor" && first.visible && first.outline !== "none", "Tab first reaches a visible skip link, with a focus ring", JSON.stringify(first));
await page.keyboard.press("Enter");
check((await page.evaluate(() => document.activeElement?.id)) === "editor", "the skip link moves the keyboard to the editor");
const focusShown = await page.evaluate(() => getComputedStyle(document.activeElement!).outlineStyle !== "none");
check(focusShown, "the focused panel shows a focus ring");
await page.keyboard.press("F6");
check((await page.evaluate(() => document.activeElement?.id)) === "results", "F6 moves to the next panel");
await page.keyboard.press("Shift+F6");
await page.keyboard.press("Shift+F6");
check((await page.evaluate(() => document.activeElement?.id)) === "catalog", "Shift+F6 moves back");

// Tab order: the header, then the left column, the editor and results, the
// right column and the status bar.
const tabOrder = await page.evaluate(() => {
  const all = [...document.querySelectorAll<HTMLElement>("button, a[href], [tabindex='0']")].filter((el) => el.offsetParent !== null && !el.closest(".skip-links"));
  return all.map((el) => el.id || el.dataset.split || el.className);
});
const expected = ["toggle-left", "brand", "toggle-right", "theme", "sources-title", "left", "catalog-title", "left-width", "editor-title", "editor-run", "editor-run all", "main", "results-title", "right-width", "inspector-title", "right", "history-title"];
check(JSON.stringify(tabOrder) === JSON.stringify(expected), "the focus order follows the layout", tabOrder.join(" "));

// The splitters' keys.
const splitter = page.locator('[data-split="left-width"]');
const width = () => page.evaluate(() => document.getElementById("left")!.getBoundingClientRect().width);
const before = await width();
await splitter.focus();
await page.keyboard.press("ArrowRight");
await page.keyboard.press("Shift+ArrowRight");
const after = await width();
check(Math.round(after - before) === 80 && (await splitter.getAttribute("aria-valuenow")) === String(Math.round(after)), "the column splitter's arrows widen the column", `${before} → ${after}, aria-valuenow ${await splitter.getAttribute("aria-valuenow")}`);
await page.keyboard.press("Home");
check(Math.round(await width()) === 200, "Home narrows it to its least", `${await width()}`);
const rows = page.locator('[data-split="main"]');
const editorHeight = () => page.evaluate(() => document.getElementById("editor")!.getBoundingClientRect().height);
const h0 = await editorHeight();
await rows.focus();
await page.keyboard.press("ArrowDown");
check((await editorHeight()) > h0 + 10, "the row splitter's arrows give the editor more room", `${h0} → ${await editorHeight()}`);
check(/Editor \d+%, Results \d+%/.test((await rows.getAttribute("aria-valuetext")) ?? ""), "the row splitter says how the room is shared", (await rows.getAttribute("aria-valuetext")) ?? "");
// Dragging.
const box = (await rows.boundingBox())!;
await page.mouse.move(box.x + box.width / 2, box.y);
await page.mouse.down();
await page.mouse.move(box.x + box.width / 2, box.y - 120, { steps: 4 });
await page.mouse.up();
const dragged = await page.evaluate(() => document.getElementById("editor")!.getBoundingClientRect().bottom);
check(Math.abs(dragged - (box.y - 120)) < 6, "dragging the row splitter moves it", `${dragged} vs ${box.y - 120}`);
// Enter folds the column away, and the header's button brings it back.
await splitter.focus();
await page.keyboard.press("Enter");
check((await page.locator("#left").isHidden()) && (await page.locator("#toggle-left").getAttribute("aria-expanded")) === "false", "Enter on the column splitter hides the column");
check((await page.evaluate(() => document.activeElement?.id)) === "toggle-left", "and the keyboard goes to its button");
await page.keyboard.press("Enter");
check((await page.locator("#left").isVisible()) && (await page.locator("#toggle-left").getAttribute("aria-expanded")) === "true", "the button shows it again");
// A skip link opens a hidden column.
await page.click("#toggle-right");
check(await page.locator("#right").isHidden(), "the right column's button hides it");
await page.evaluate(() => (document.querySelector('.skip-links a[href="#inspector"]') as HTMLElement).focus());
await page.keyboard.press("Enter");
check((await page.locator("#right").isVisible()) && (await page.evaluate(() => document.activeElement?.id)) === "inspector", "the skip link to the inspector opens its column");
// Folding a panel to its header.
await page.click("#sources-title");
const folded = await page.evaluate(() => ({
  expanded: document.getElementById("sources-title")!.getAttribute("aria-expanded"),
  body: document.getElementById("sources-body")!.hidden,
  catalog: document.getElementById("catalog")!.getBoundingClientRect().height,
}));
check(folded.expanded === "false" && folded.body && folded.catalog > 500, "a panel's title folds it, and the other takes the room", JSON.stringify(folded));
// Kept across a reload.
const leftWidth = await width();
await page.reload();
await connected();
check(Math.round(await width()) === Math.round(leftWidth) && (await page.locator("#sources-body").isHidden()), "the layout is kept across a reload");
await page.click("#sources-title");
await page.evaluate(() => localStorage.removeItem("ursa-console-layout"));
await page.reload();
await connected();

// ---------------------------------------------------------------- a phone

await page.setViewportSize({ width: 390, height: 844 });
await page.waitForFunction(() => document.getElementById("console")!.dataset.narrow === "true" && document.getElementById("left")!.hidden, null, { timeout: 10_000 }).catch(() => {});
const phone = await page.evaluate(() => ({
  left: document.getElementById("left")!.hidden,
  right: document.getElementById("right")!.hidden,
  main: document.getElementById("main")!.getBoundingClientRect().width,
  sideways: document.documentElement.scrollWidth - innerWidth,
  status: document.getElementById("connection")!.getBoundingClientRect().width > 0,
}));
check(phone.left && phone.right && phone.main >= 380 && phone.sideways <= 0 && phone.status, "a phone: the editor and results fill the width, the side columns fold away, nothing scrolls sideways", JSON.stringify(phone));
await screenshot("console-light-390x844.png");
await page.click("#toggle-left");
const drawer = await page.evaluate(() => ({
  shown: !document.getElementById("left")!.hidden,
  over: getComputedStyle(document.getElementById("left")!).position,
  focus: document.activeElement?.id,
  expanded: document.getElementById("toggle-left")!.getAttribute("aria-expanded"),
}));
check(drawer.shown && drawer.over === "absolute" && drawer.focus === "sources" && drawer.expanded === "true", "its button opens the sources as a drawer, with the keyboard in it", JSON.stringify(drawer));
await screenshot("console-light-390x844-drawer.png");
await page.keyboard.press("Escape");
check((await page.locator("#left").isHidden()) && (await page.evaluate(() => document.activeElement?.id)) === "toggle-left", "Esc closes the drawer, back to its button");
await page.click("#toggle-right");
await page.click(".scrim", { position: { x: 20, y: 300 } });
check(await page.locator("#right").isHidden(), "a click beside a drawer closes it");
await page.click("#theme");
await screenshot("console-dark-390x844.png");
await page.click("#theme");
await page.setViewportSize({ width: 1440, height: 900 });

// ---------------------------------------------------------------- cancel

await page.evaluate((sql) => {
  const w = window as any;
  w.longQuery = w.ursaConsole.session.run("query", { sql }, { label: "query" }).then(
    () => "finished",
    (e: any) => e.kind,
  );
}, FOREVER);
await page.waitForSelector("#running:not([hidden])");
await page.waitForTimeout(800);
const runningText = await text("#running");
check(/Running query/.test(runningText) && /\d\.\d s/.test(runningText), "the status bar shows the running query and its time", runningText);
if (shots) {
  await page.evaluate(() => (document.activeElement as HTMLElement)?.blur());
  await screenshot("console-light-1440x900-running.png");
}
const t0 = Date.now();
await page.click("#cancel");
const outcome = await page.evaluate(() => (window as any).longQuery);
check(outcome === "cancelled", "Cancel stops the long query: it fails as cancelled", `${outcome} after ${Date.now() - t0} ms`);
await page.waitForSelector("#running", { state: "hidden" });
check((await page.locator("#announce").textContent()) === "Cancelled query", "and a screen reader hears it was cancelled", (await page.locator("#announce").textContent()) ?? "");
const after1 = await page.evaluate(() => (window as any).ursaConsole.session.run("query", { sql: "SELECT 41 + 1 AS answer" }).then((r: any) => r.rows));
check(after1 === 1, "the session goes on after a cancel", String(after1));

// ---------------------------------------------------------------- the server goes

check(await stop(server), "ursa serve stops gracefully with the console connected");
await page.waitForFunction(() => (window as any).ursaConsole.session.state === "reconnecting" && /Disconnected: ursa serve (stopped|is not running)/.test(document.getElementById("connection")!.textContent!), null, { timeout: 15_000 }).catch(() => {});
check(/^Disconnected: ursa serve (stopped|is not running)\.$/.test(await text("#connection")), "the status bar says the server went", await text("#connection"));
check(await page.locator("#reconnect").isVisible(), "with a Reconnect now button");
await page.waitForFunction(() => /ursa serve is not running/.test(document.getElementById("connection")!.textContent!), null, { timeout: 15_000 }).catch(() => {});
check((await text("#connection")).includes("ursa serve is not running") && /Trying again/.test(await text("#retry-in")), "and keeps trying, saying the server is not running", `${await text("#connection")} ${await text("#retry-in")}`);
if (shots) await screenshot("console-light-1440x900-disconnected.png");
({ server, link } = await serve());
check(Number(new URL(link).port) === served, "ursa serve starts again on its port", link);
await page.click("#reconnect");
await page.waitForFunction(() => /restarted/.test(document.getElementById("connection")!.textContent!), null, { timeout: 15_000 }).catch(() => {});
check((await text("#connection")).includes("ursa serve was restarted: open the link it printed"), "a restarted server's new token: the page says to open its link", await text("#connection"));
// Opening the new link (in another tab) sets the token; the page connects.
const other = await context.newPage();
watch(other);
await other.goto(link);
await page.click("#reconnect");
await connected().catch(() => {});
check((await page.evaluate(() => (window as any).ursaConsole.session.state)) === "connected", "once it is opened, the console connects again", await text("#connection"));
const again = await page.evaluate(() => (window as any).ursaConsole.session.run("query", { sql: "SELECT 1" }).then((r: any) => r.rows));
check(again === 1, "and runs requests", String(again));
await other.close();

// ---------------------------------------------------------------- the shell

const shell = await context.newPage();
watch(shell);
await shell.goto(`${base}/`);
check((await shell.title()) === "Ursa Web Shell", "the shell is still at /", await shell.title());
const started = await shell
  .waitForFunction(() => (window as any).ursaShell?.screen().includes("memory U"), null, { timeout: 60_000 })
  .then(() => true, () => false);
check(started, "and starts its session over the socket");
await shell.close();

check(violations.length === 0, "no Content Security Policy violations", violations.join(" | "));
check(errors.length === 0, "no page errors", errors.join(" | "));
await browser.close();
check(await stop(server), "ursa serve stops gracefully");
console.log(failures.length ? `\n${failures.length} failed` : "\nall passed");
process.exit(failures.length ? 1 : 0);
