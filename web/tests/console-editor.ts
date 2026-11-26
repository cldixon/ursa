// End-to-end test of the console's SQL editor (web/console/editor.js,
// URSA-13) over `ursa serve`, in headless Chromium.
//
//   bun tests/console-editor.ts [port] [ursa binary] [screenshot dir]
//   bun run test:console-editor -- 8982
//
// As tests/console.ts, it starts `ursa serve .data --port PORT` with this
// checkout's pages (`bun run build` first) from a build with `--features
// serve` (by default $CARGO_TARGET_DIR/release/ursa; URSA_BIN= also names
// it), and checks:
//
// - the editor is in the Editor panel, labelled, with its keys described;
// - its colours are `highlight`'s spans, exactly, and each kind is drawn as
//   the shell's terminal draws it, in light and dark;
// - typing part of a table's name offers it (with its kind and detail),
//   Tab or Enter accepts it, and Ctrl+Enter runs the statement at the cursor;
// - Shift+Ctrl+Enter runs every statement and stops at the first error;
// - an error shows at its statement: a squiggle under what it names, and the
//   message with the engine's hint; editing the statement clears it;
// - a dot-command runs through `feed`;
// - the statement running is marked, and the status bar's Cancel stops it;
// - Esc then Tab leaves the editor;
// - the text survives a reload, kept per server port;
// - ursaConsole.editor.insert() for the other panels;
// - no page errors and no CSP violations.
//
// With a screenshot dir, it saves the editor in light and dark at 1440×900:
// highlighted SQL, the completion list open, and an inline error
// (editor-*.png).
import { chromium, type Page } from "playwright";
import { join } from "node:path";
import { cargoTargetDir, DATA_DIR, WEB } from "../scripts/common";

const port = Number(Bun.argv[2] ?? 8982);
const bin = Bun.argv[3] || process.env.URSA_BIN || join(cargoTargetDir(), "release", "ursa");
const shots = Bun.argv[4];
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(`${label}${detail ? `: ${detail}` : ""}`);
};
const FOREVER = "SELECT sum(value) AS s FROM range(0, 100000000000)";
const MOD = process.platform === "darwin" ? "Meta" : "Control";

const server = Bun.spawn([bin, "serve", DATA_DIR, "--port", String(port), "--assets", join(WEB, "dist-console"), "--assets", join(WEB, "dist")], {
  stdout: "pipe",
  stderr: "inherit",
});
const reader = server.stdout.getReader();
let printed = "";
let link: string | undefined;
while (!link || !/once the link has opened\.\n/.test(printed)) {
  const { value, done } = await reader.read();
  if (done) throw new Error(`ursa serve stopped:\n${printed}`);
  printed += new TextDecoder().decode(value);
  link = printed.match(/http:\/\/127\.0\.0\.1:\d+\/\S*\?token=[A-Za-z0-9_-]+/)?.[0];
}
reader.releaseLock();
const base = new URL(link).origin;

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
await context.addInitScript(() => {
  document.addEventListener("securitypolicyviolation", (e) => console.error(`CSP violation: ${e.violatedDirective} blocked ${e.blockedURI || "inline"}`));
});
const errors: string[] = [];
const violations: string[] = [];
function watch(p: Page) {
  p.on("pageerror", (e) => errors.push(`page error: ${e.message}`));
  p.on("console", (m) => {
    if (m.type() !== "error") return;
    if (/CSP violation|Content Security Policy/.test(m.text())) violations.push(m.text());
    else if (!/WebSocket|Failed to load resource/.test(m.text())) errors.push(`console error: ${m.text()}`);
  });
}
const page = await context.newPage();
watch(page);

/** Load the console and wait for the editor and the session. */
async function open(url = `${base}/console/`) {
  await page.goto(url);
  await page.waitForFunction(() => (window as any).ursaConsole?.session.state === "connected" && (window as any).ursaConsole?.editor, null, { timeout: 30_000 });
}
await open(link);

const ed = <T>(fn: string) => page.evaluate(`(() => { const e = window.ursaConsole.editor; return ${fn}; })()`) as Promise<T>;
/** Set the text and wait until the session's colours for it are painted. */
async function setText(text: string) {
  const before = await ed<number>("e.stats.highlights");
  await page.evaluate((t) => (window as any).ursaConsole.editor.setText(t), text);
  await page.waitForFunction((n) => (window as any).ursaConsole.editor.stats.highlights > n, before, { timeout: 10_000 });
}
/** Collect the editor's ursa:output events from now on. */
async function outputs() {
  await page.evaluate(() => {
    const w = window as any;
    w.editorOutputs = [];
    if (!w.editorListening) {
      w.editorListening = true;
      document.addEventListener("ursa:output", (e: any) => w.editorOutputs.push({ kind: e.detail.kind, text: e.detail.text, rows: e.detail.reply?.rows, tab: Boolean(e.detail.reply?.tab), out: e.detail.out }));
    }
  });
}
const idle = () => page.waitForFunction(() => !(window as any).ursaConsole.editor.running, null, { timeout: 30_000 });
async function screenshot(name: string) {
  if (!shots) return;
  await page.waitForTimeout(200);
  await page.screenshot({ path: join(shots, name) });
}
const theme = (name: string) => page.evaluate((n) => (document.documentElement.dataset.theme = n), name);

// A table for the completion to find.
await page.evaluate(() =>
  (window as any).ursaConsole.session.run("query", { sql: "CREATE OR REPLACE TABLE karate_club AS SELECT * FROM 'karate_edges.parquet'" }),
);

// ---------------------------------------------------------------- in place

const placed = await page.evaluate(() => {
  const content = document.querySelector("#editor-body .cm-content");
  return {
    inPanel: Boolean(content),
    label: content?.getAttribute("aria-label"),
    described: document.getElementById(content?.getAttribute("aria-describedby") ?? "")?.textContent ?? "",
    placeholder: document.querySelector("#editor .placeholder") === null,
  };
});
check(placed.inPanel && placed.placeholder, "the editor fills the Editor panel");
check(placed.label === "SQL editor" && /Enter/.test(placed.described) && /Esc/.test(placed.described), "it is labelled for a screen reader, with its keys described", JSON.stringify(placed));

// ---------------------------------------------------------------- colour

const SAMPLE = `-- the karate club's edges
SELECT src, dst, 'it''s' AS s, 42 AS n
FROM karate_club /* a table */ WHERE src < 5;
.tables karate
`;
await setText(SAMPLE);
const spans = await ed<any[]>("e.spans");
const theirs = await page.evaluate((sql) => (window as any).ursaConsole.session.client.call("highlight", { sql }).then((r: any) => r.spans), SAMPLE);
check(JSON.stringify(spans) === JSON.stringify(theirs), "the editor's colour spans are highlight's, exactly", `${JSON.stringify(spans)} vs ${JSON.stringify(theirs)}`);
const kinds = new Set(spans.map((s) => s.kind));
check(["keyword", "string", "comment", "command", "arguments"].every((k) => kinds.has(k)), "every kind is coloured", [...kinds].join(" "));
// The painted text, by kind: each span's DOM carries its kind.
const painted = await page.evaluate(() =>
  [...document.querySelectorAll(".cm-content [data-kind]")].map((el) => [(el as HTMLElement).dataset.kind, el.textContent]),
);
check(painted.some(([k, t]) => k === "string" && t === "'it''s'") && painted.some(([k, t]) => k === "command" && t === ".tables"), "the colours are on the right text", JSON.stringify(painted));

/** The colour and weight of each kind as the editor draws it. */
const editorLook = () =>
  page.evaluate(() => {
    const out: Record<string, string> = {};
    for (const el of document.querySelectorAll<HTMLElement>(".cm-content [data-kind]")) {
      const s = getComputedStyle(el);
      out[el.dataset.kind!] = `${s.color} ${Number(s.fontWeight) >= 600 ? "bold" : "normal"} ${s.fontStyle}`;
    }
    return out;
  });

// The shell's terminal, typing the same kinds: each one's colour as drawn.
const shell = await context.newPage();
watch(shell);
await shell.goto(`${base}/`);
await shell.waitForFunction(() => (window as any).ursaShell?.screen().includes("memory U"), null, { timeout: 60_000 });
/** The look of each kind in the shell's input line, read from xterm's DOM rows. */
async function shellLook(themeName: string) {
  await shell.evaluate((n) => {
    if (document.documentElement.dataset.theme !== n) (document.getElementById("theme") as HTMLElement).click();
  }, themeName);
  await shell.waitForTimeout(200);
  return shell.evaluate(() => {
    const want: Record<string, string> = { keyword: "SELECT", string: "'s'", comment: "--c", command: ".tables", arguments: "karate" };
    const out: Record<string, string> = {};
    const spans = [...document.querySelectorAll<HTMLElement>(".xterm-rows span")];
    for (const [kind, text] of Object.entries(want)) {
      const el = spans.find((s) => s.textContent === text) ?? spans.find((s) => s.textContent?.trim() === text);
      if (!el) continue;
      const s = getComputedStyle(el);
      out[kind] = `${s.color} ${Number(s.fontWeight) >= 600 ? "bold" : "normal"} ${s.fontStyle}`;
    }
    return out;
  });
}
await shell.locator(".xterm").click();
// One line per kind, so xterm draws each piece in its own span. A dot-command
// and a statement: typed, not run (Ctrl+C drops each).
await shell.keyboard.type(".tables karate");
const shellCommandLight = await shellLook("light");
const shellCommandDark = await shellLook("dark");
await shell.keyboard.press("Control+c");
await shell.keyboard.type("SELECT 's' --c");
const shellSqlDark = await shellLook("dark");
const shellSqlLight = await shellLook("light");
await shell.keyboard.press("Control+c");
const shellTerm: Record<string, Record<string, string>> = {
  light: { ...shellSqlLight, ...shellCommandLight },
  dark: { ...shellSqlDark, ...shellCommandDark },
};
await shell.close();
for (const name of ["light", "dark"]) {
  await theme(name);
  const mine = await editorLook();
  const want = shellTerm[name];
  const differ = Object.keys(want).filter((k) => mine[k] !== want[k]);
  check(Object.keys(want).length === 5 && differ.length === 0, `${name}: each kind looks as the shell's terminal draws it`, differ.length || Object.keys(want).length !== 5 ? `editor ${JSON.stringify(mine)} shell ${JSON.stringify(want)}` : JSON.stringify(mine));
}
await theme("light");
await page.locator(".cm-content").click();
await screenshot("editor-light-1440x900-highlighted.png");
await theme("dark");
await screenshot("editor-dark-1440x900-highlighted.png");
await theme("light");

// Typing: the colours follow, without waiting for a reload.
await page.evaluate(() => (window as any).ursaConsole.editor.setText(""));
await page.locator(".cm-content").click();
const t0 = Date.now();
await page.keyboard.type("SELECT 'x'");
await page.waitForFunction(() => [...document.querySelectorAll(".cm-content [data-kind=string]")].some((e) => e.textContent === "'x'"), null, { timeout: 5_000 }).catch(() => {});
const typedColoured = await page.evaluate(() => [...document.querySelectorAll(".cm-content [data-kind]")].map((e) => `${(e as HTMLElement).dataset.kind}:${e.textContent}`).join(" "));
check(typedColoured === "keyword:SELECT string:'x'", "what is typed is coloured as it is typed", `${typedColoured} within ${Date.now() - t0} ms`);

// ---------------------------------------------------------------- completion and running

await outputs();
await page.evaluate(() => (window as any).ursaConsole.editor.setText(""));
await page.locator(".cm-content").click();
await page.keyboard.type("SELECT * FROM karate_c");
await page.waitForSelector(".cm-tooltip-autocomplete li", { timeout: 10_000 });
const offered = await page.evaluate(() =>
  [...document.querySelectorAll(".cm-tooltip-autocomplete li")].map((li) => ({
    label: li.querySelector(".cm-completionLabel")?.textContent,
    kind: li.querySelector(".cm-sql-kind")?.textContent,
    icon: [...(li.querySelector(".cm-completionIcon")?.classList ?? [])].join(" "),
    detail: li.querySelector(".cm-completionDetail")?.textContent ?? "",
    selected: li.getAttribute("aria-selected") === "true",
  })),
);
const club = offered.find((o) => o.label === "karate_club");
check(Boolean(club && club.kind === "table" && /cm-completionIcon-table/.test(club.icon)), "typing part of a table's name offers it, as a table", JSON.stringify(offered));
check(Boolean(club?.detail), "with its detail", club?.detail ?? "");
await screenshot("editor-light-1440x900-completion.png");
await theme("dark");
await screenshot("editor-dark-1440x900-completion.png");
await theme("light");
check(offered[0]?.label === "karate_club" && offered[0]?.selected, "the table comes first, selected", JSON.stringify(offered[0]));
await page.keyboard.press("Tab");
check((await ed<string>("e.text")) === "SELECT * FROM karate_club", "Tab accepts it", await ed<string>("e.text"));
await page.keyboard.press(`${MOD}+Enter`);
await page.waitForFunction(() => (window as any).editorOutputs.length > 0, null, { timeout: 30_000 });
await idle();
let out = await page.evaluate(() => (window as any).editorOutputs);
check(out.length === 1 && out[0].text === "SELECT * FROM karate_club" && (out[0].rows === 78 || out[0].tab), "Ctrl+Enter runs it, and the result comes back", JSON.stringify(out));
const tabShown = await page.evaluate(() => Boolean((window as any).ursaConsole.results));
if (tabShown) check(/karate_club|78/.test(await page.locator("#results-body").innerText()), "the result is in the Results panel");

// Completion after `t.`: the table's columns.
await page.evaluate(() => (window as any).ursaConsole.editor.setText(""));
await page.keyboard.type("SELECT k.");
await page.waitForTimeout(50);
await page.evaluate(() => (window as any).ursaConsole.editor.setText("SELECT k. FROM karate_club k"));
await page.evaluate(() => {
  const v = (window as any).ursaConsole.editor.view;
  v.dispatch({ selection: { anchor: "SELECT k.".length } });
});
await page.keyboard.press("Control+Space");
await page.waitForSelector(".cm-tooltip-autocomplete li", { timeout: 10_000 }).catch(() => {});
const columns = await page.evaluate(() => [...document.querySelectorAll(".cm-tooltip-autocomplete li")].map((li) => `${li.querySelector(".cm-completionLabel")?.textContent}:${li.querySelector(".cm-sql-kind")?.textContent}`));
check(columns.includes("src:column") && columns.includes("dst:column"), "after an alias and a dot, Ctrl+Space offers its columns", columns.join(" "));
await page.keyboard.press("Escape");

// The statement at the cursor, among several.
await outputs();
await setText("SELECT 1 AS one;\nSELECT 2 AS two;\nSELECT 3 AS three;");
await page.evaluate(() => (window as any).ursaConsole.editor.view.dispatch({ selection: { anchor: 20 } }));
await page.locator(".cm-content").focus();
await page.keyboard.press(`${MOD}+Enter`);
await page.waitForFunction(() => (window as any).editorOutputs.length > 0, null, { timeout: 30_000 });
await idle();
out = await page.evaluate(() => (window as any).editorOutputs);
check(out.length === 1 && out[0].text === "SELECT 2 AS two", "Ctrl+Enter runs the statement at the cursor only", JSON.stringify(out.map((o: any) => o.text)));
// The selection.
await outputs();
await page.evaluate(() => (window as any).ursaConsole.editor.view.dispatch({ selection: { anchor: 0, head: 31 } }));
await page.keyboard.press(`${MOD}+Enter`);
await page.waitForFunction(() => (window as any).editorOutputs.length >= 2, null, { timeout: 30_000 });
await idle();
out = await page.evaluate(() => (window as any).editorOutputs);
check(out.map((o: any) => o.text).join("|") === "SELECT 1 AS one|SELECT 2 AS two", "with a selection, it runs the selection's statements", JSON.stringify(out.map((o: any) => o.text)));

// ---------------------------------------------------------------- errors

await outputs();
await setText("SELECT 1 AS one;\nSELECT srcc FROM karate_club;\nSELECT 3 AS three;");
await page.locator(".cm-content").focus();
await page.keyboard.press(`Shift+${MOD}+Enter`);
await page.waitForSelector(".cm-sql-error", { timeout: 30_000 });
await idle();
out = await page.evaluate(() => (window as any).editorOutputs);
check(out.map((o: any) => o.text).join("|") === "SELECT 1 AS one", "Shift+Ctrl+Enter runs every statement and stops at the first error", JSON.stringify(out.map((o: any) => o.text)));
const shown = await page.evaluate(() => {
  const box = document.querySelector(".cm-sql-error") as HTMLElement;
  const squiggle = document.querySelector(".cm-lintRange-error");
  const line = box?.closest(".cm-content") ? box.previousElementSibling?.textContent : null;
  return {
    message: box?.innerText ?? "",
    role: box?.getAttribute("role"),
    under: squiggle?.textContent ?? "",
    after: line,
    errors: (window as any).ursaConsole.editor.errors,
  };
});
check(/srcc/.test(shown.message) && shown.role === "alert", "the error shows at its statement, with its message", JSON.stringify(shown));
check(shown.under === "srcc", "a squiggle under what it names", shown.under);
check(shown.after === "SELECT srcc FROM karate_club;", "below the statement that failed", shown.after ?? "");
check(/Hint: /.test(shown.message) || /did you mean|Valid fields/i.test(shown.message), "with the engine's hint", shown.message);
await screenshot("editor-light-1440x900-error.png");
await theme("dark");
await screenshot("editor-dark-1440x900-error.png");
await theme("light");
// A parse error: placed at the line and column the parser gives.
await setText("SELECT 1;\nSELECT * FROM karate_club WHERE src >;");
await page.evaluate(() => (window as any).ursaConsole.editor.view.dispatch({ selection: { anchor: 15 } }));
await page.evaluate(() => (window as any).ursaConsole.editor.runCurrent());
const parse = await page.evaluate(() => (window as any).ursaConsole.editor.errors[0]);
check(Boolean(parse) && parse.from === 10, "a parse error shows at its statement", JSON.stringify(parse));
// Editing the statement takes its error away.
await page.evaluate(() => (window as any).ursaConsole.editor.view.dispatch({ changes: { from: 37, insert: " 1" } }));
check((await page.locator(".cm-sql-error").count()) === 0 && (await page.locator(".cm-lintRange-error").count()) === 0, "editing the statement clears its error");

// ---------------------------------------------------------------- dot-commands

await outputs();
await setText(".tables\nSELECT 5 AS five;");
await page.evaluate(() => (window as any).ursaConsole.editor.view.dispatch({ selection: { anchor: 2 } }));
await page.evaluate(() => (window as any).ursaConsole.editor.runCurrent());
out = await page.evaluate(() => (window as any).editorOutputs);
check(out.length === 1 && out[0].kind === "command" && /karate_club/.test(out[0].out), "a dot-command runs through feed, as the shell's", JSON.stringify(out));
await setText(".nosuchcommand");
await page.evaluate(() => (window as any).ursaConsole.editor.runCurrent());
const dotErr = await page.evaluate(() => (window as any).ursaConsole.editor.errors[0]?.message ?? "");
check(/nosuchcommand/.test(dotErr), "and its error shows at it", dotErr);

// ---------------------------------------------------------------- cancel

await setText(`SELECT 1;\n${FOREVER};`);
await page.evaluate(() => (window as any).ursaConsole.editor.view.dispatch({ selection: { anchor: 15 } }));
await page.locator(".cm-content").focus();
await page.keyboard.press(`${MOD}+Enter`);
await page.waitForSelector(".cm-sql-running", { timeout: 10_000 }).catch(() => {});
await page.waitForSelector("#running:not([hidden])");
const mark = await page.evaluate(() => [...document.querySelectorAll(".cm-sql-running")].map((e) => e.textContent).join(""));
check(mark.startsWith("SELECT sum(value)"), "the statement running is marked", mark);
await page.waitForTimeout(500);
await page.click("#cancel");
await idle();
check((await page.locator(".cm-sql-running").count()) === 0, "Cancel in the status bar stops it, and the mark goes");
const cancelled = await page.evaluate(() => (window as any).ursaConsole.editor.errors[0]);
check(cancelled?.kind === "cancelled" && /Cancelled/.test(await page.locator(".cm-sql-error").innerText()), "the statement says it was cancelled", JSON.stringify(cancelled));

// ---------------------------------------------------------------- the keyboard

await page.locator(".cm-content").focus();
await page.keyboard.press("Tab");
check((await ed<string>("e.text")).includes("  ") && (await page.evaluate(() => document.activeElement?.classList.contains("cm-content"))), "Tab indents in the editor");
await page.keyboard.press("Escape");
await page.keyboard.press("Tab");
const left = await page.evaluate(() => ({ inEditor: Boolean(document.activeElement?.closest(".cm-editor")), id: document.activeElement?.id || document.activeElement?.className }));
check(!left.inEditor, "Esc then Tab leaves the editor", JSON.stringify(left));

// ---------------------------------------------------------------- the API

await setText("SELECT 1;");
const inserted = await page.evaluate(() => (window as any).ursaConsole.editor.insert("SELECT count(*) AS n FROM karate_club", { statement: true, run: true }));
check(inserted === true && (await ed<string>("e.text")) === "SELECT 1;\n\nSELECT count(*) AS n FROM karate_club;\n", "insert() adds a statement of its own and runs it", JSON.stringify(await ed<string>("e.text")));

// ---------------------------------------------------------------- kept

const KEPT = "-- kept\nSELECT 'across a reload' AS kept;";
await setText(KEPT);
await page.reload();
await page.waitForFunction(() => (window as any).ursaConsole?.editor, null, { timeout: 30_000 });
check((await ed<string>("e.text")) === KEPT, "the text survives a reload", await ed<string>("e.text"));
const firstPaint = await page.evaluate(() => [...document.querySelectorAll(".cm-content [data-kind]")].length);
check(firstPaint >= 3, "and is coloured at once, from the colours kept with it", String(firstPaint));
const stored = await page.evaluate(() => Object.keys(localStorage).filter((k) => k.startsWith("ursa-console-editor:")));
check(stored.length === 1 && stored[0] === `ursa-console-editor:${port}`, "kept per server port", stored.join(" "));

check(violations.length === 0, "no Content Security Policy violations", violations.join(" | "));
check(errors.length === 0, "no page errors", errors.join(" | "));
await browser.close();
server.kill("SIGINT");
await Promise.race([server.exited, Bun.sleep(10_000)]);
console.log(failures.length ? `\n${failures.length} failed` : "\nall passed");
process.exit(failures.length ? 1 : 0);
