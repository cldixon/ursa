// End-to-end test of the Ursa Web Shell in headless Chromium, against a
// running site: `bun run preview` (http://localhost:8788), `cf dev` or a deploy.
//
//   bun tests/e2e.ts [url] [screenshot dir]
//
// The page is the `ursa` shell in a terminal (xterm.js), so the test reads the
// screen as text, as a person would. It loads every dataset from the Datasets
// menu, types SQL at the prompt (one line and several), runs dot-commands (the
// shell's and the page's), downloads the last result in each format, imports a
// file, replays a shared link in a fresh page, and checks New, the theme toggle
// and history, Shift+Enter, Ctrl+C, overlapping datasets and an engine crash. It
// also checks the keyboard (Tab out of the terminal, the Datasets menu's keys,
// the focus after a click), a phone's bar and SQL, and the grey text's
// contrast. The line editor's checks cover wide characters (CJK and emoji),
// lines that wrap or fill the width exactly, resizing, colour, and its keys.
// And it checks what keeps the page safe: a shared link asks before it runs,
// shows control characters as escapes and downloads nothing; awkward file
// names import; and every response carries the security headers, which the
// page runs under with no Content Security Policy violation. An error on
// screen fails the run, except the ones a test asks for. It also runs the
// tour on a phone, and opens every "Try it live" link in the README, the SQL
// docs and the website (a Share link is answered y).
import { chromium, type Page } from "playwright";

const base = (Bun.argv[2] ?? "http://localhost:8788").replace(/\/$/, "");
const shots = Bun.argv[3];
const failures: string[] = [];
const check = (ok: boolean, label: string, detail = "") => {
  console.log(`${ok ? "ok  " : "FAIL"} ${label}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(`${label}${detail ? `: ${detail}` : ""}`);
};

const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, acceptDownloads: true });
// Content Security Policy violations, from every page: the console's, and
// the documents' securitypolicyviolation events.
const violations: string[] = [];
await context.addInitScript(() => {
  document.addEventListener("securitypolicyviolation", (e) =>
    console.error(`CSP violation: ${e.violatedDirective} blocked ${e.blockedURI || "inline"} @${e.sourceFile}:${e.lineNumber}:${e.columnNumber}`),
  );
});
context.on("console", (m) => {
  if (m.type() === "error" && /Content Security Policy|CSP violation/i.test(m.text())) violations.push(m.text());
});
const page = await context.newPage();
page.on("pageerror", (e) => failures.push(`page error: ${e.message}`));

async function ready(p: Page) {
  // Not busy, and started: the banner and a prompt are on screen. A fresh page
  // is not busy for a moment before its session starts (and clears the screen).
  await p.waitForFunction(() => {
    const shell = (window as any).ursaShell;
    return shell && !shell.busy() && shell.screen().includes("memory U");
  }, null, { timeout: 120_000 });
}
async function idle(p: Page = page) {
  await p.evaluate(() => (window as any).ursaShell.idle());
  await ready(p);
}
const screen = (p: Page = page): Promise<string> => p.evaluate(() => (window as any).ursaShell.screen());
const count = (text: string, needle: string) => text.split(needle).length - 1;
/** Tables on screen: each starts with a ┌ at the start of a line. */
const tables = async (p: Page = page) => (await screen(p)).split("\n").filter((l) => l.startsWith("┌")).length;
const errors = async (p: Page = page) => count(await screen(p), "Error: ");
/** The queries in a dataset's commands: not its CREATEs, dot-commands or comments. */
const queries = (commands: string[]) =>
  commands.map((c) => c.split("\n").filter((l) => !/^\s*--/.test(l)).join("\n")).filter((c) => !/^(CREATE|\.)/i.test(c)).length;
/** What has the focus: an id, or the terminal. */
const focused = (p: Page = page): Promise<string> =>
  p.evaluate(() => (document.activeElement?.classList.contains("xterm-helper-textarea") ? "terminal" : document.activeElement?.id || document.activeElement?.tagName || ""));
/** WCAG contrast of two computed CSS colours, rgb(...). */
function contrast(a: string, b: string) {
  const lum = (c: string) => {
    const [r, g, bl] = c.match(/\d+(\.\d+)?/g)!.slice(0, 3).map((v) => {
      const x = Number(v) / 255;
      return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * bl;
  };
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}
/** The colour of the types under a table's header (the shell's grey), and the terminal's background. */
// xterm draws its rows on the next animation frame: wait for the span.
const greyText = (p: Page = page) =>
  p
    .waitForFunction(
      () => {
        const span = [...document.querySelectorAll(".xterm-rows span")].findLast((s) => s.textContent!.trim() === "Int64");
        return span ? [getComputedStyle(span).color, getComputedStyle(document.querySelector(".terminal")!).backgroundColor] : null;
      },
      null,
      { timeout: 5_000 },
    )
    .then((h) => h.jsonValue() as Promise<string[]>, () => null);

/** Type `text` at the prompt, a line at a time, each with Enter; wait for it. */
async function type(text: string, p: Page = page) {
  for (const line of text.split("\n")) {
    await p.keyboard.type(line);
    await p.keyboard.press("Enter");
  }
  await idle(p);
}
/** What the last command printed: the screen after the last line that starts with `prompt + first`. */
async function after(first: string, p: Page = page): Promise<string> {
  const lines = (await screen(p)).split("\n");
  for (let i = lines.length - 1; i >= 0; i--) if (lines[i].startsWith(`memory U ${first}`)) return lines.slice(i + 1).join("\n");
  return "";
}
/** The cells of the last table in `text`, row by row (header, types, then rows). */
function lastTable(text: string) {
  const lines = text.split("\n");
  const end = lines.findLastIndex((l) => l.startsWith("└"));
  const start = lines.slice(0, end).findLastIndex((l) => l.startsWith("┌"));
  const rows = lines.slice(start + 1, end).filter((l) => l.startsWith("│"));
  const cells = rows.map((l) => l.slice(1, -1).split("│").map((c) => c.trim()));
  return { head: cells[0] ?? [], types: cells[1] ?? [], rows: cells.slice(2), footer: lines[end + 1] ?? "" };
}

const t0 = Date.now();
await page.goto(base);
await ready(page);
async function watchedContext(options: Parameters<typeof browser.newContext>[0]) {
  const ctx = await browser.newContext(options);
  await ctx.addInitScript(() => {
    document.addEventListener("securitypolicyviolation", (e) =>
      console.error(`CSP violation: ${e.violatedDirective} blocked ${e.blockedURI || "inline"} @${e.sourceFile}:${e.lineNumber}:${e.columnNumber}`),
    );
  });
  ctx.on("console", (m) => {
    if (m.type() === "error" && /Content Security Policy|CSP violation/i.test(m.text())) violations.push(m.text());
  });
  ctx.on("page", (p) =>
    p.on("pageerror", (e) => {
      // Said at once too: a later step may stop the run before the end.
      console.log(`FAIL page error: ${e.message}`);
      failures.push(`page error: ${e.message}`);
    }),
  );
  ctx.on("console", (m) => {
    if (m.type() === "error" && /panicked|unreachable|recursive use|borrowed/i.test(m.text())) console.log(`CONSOLE ${m.text().slice(0, 600)}`);
  });
  return ctx;
}
const graphInfo = (p: Page): Promise<any> => p.evaluate(() => (window as any).ursaShell.graph());
const graphOpen = (p: Page): Promise<boolean> => p.evaluate(() => (window as any).ursaShell.graphOpen());
/** Wait until the view has drawn a frame and its layout has run to the end. */
async function graphSettled(p: Page) {
  try {
    await p.waitForFunction(
      () => {
        const g = (window as any).ursaShell.graph();
        return g && g.frames > 0 && g.layoutDone;
      },
      null,
      { timeout: 60_000, polling: 200 },
    );
  } catch (e) {
    // What the page shows, so that a view that closed itself (a lost WebGL
    // context, a layout that stopped) says so here.
    const seen = await p
      .evaluate(() => {
        const shell = (window as any).ursaShell;
        return { open: shell.graphOpen(), graph: shell.graph(), status: document.getElementById("graph-status")?.textContent, screen: shell.screen().split("\n").slice(-6) };
      })
      .catch(() => null);
    throw new Error(`the graph view did not settle: ${JSON.stringify(seen)}`, { cause: e });
  }
}
/** The terminal's columns, once a resize has settled. */
async function settledCols(p: Page) {
  let last = -1;
  for (let i = 0; i < 20; i++) {
    await p.waitForTimeout(100);
    const now: number = await p.evaluate(() => (window as any).ursaShell.cols());
    if (now === last) return now;
    last = now;
  }
  return last;
}
for (const phone of [false, true]) {
  const name = phone ? "a phone" : "a desktop";
  const rctx = await watchedContext(phone ? { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true } : { viewport: { width: 1280, height: 800 } });
  const p = await rctx.newPage();
  await p.goto(`${base}/?dataset=karate`);
  await ready(p);
  await idle(p);
  await p.click("#terminal");
  await type(".graph SELECT * FROM hop(karate, seeds => [0], steps => 1, direction => 'both')", p);
  await graphSettled(p);
  await p.waitForFunction(() => (window as any).ursaShell.graph()?.notShownKnown === true, null, { timeout: 30_000, polling: 50 }).catch(() => {});
  const box = (await p.locator("#graph-stage canvas").boundingBox())!;
  type At = { id: number; x: number; y: number };
  /** Where every node drawn is, in CSS pixels of the canvas. */
  const places = (): Promise<At[]> =>
    p.evaluate(() => {
      const shell = (window as any).ursaShell;
      const out: { id: number; x: number; y: number }[] = [];
      for (let id = 0; id < 34; id++) {
        const at = shell.graphNodePlace(id);
        if (at && !at.hidden) out.push({ id, x: at.x, y: at.y });
      }
      return out;
    });
  const inspector = (): Promise<any> => p.evaluate(() => (window as any).ursaShell.graphInspector());
  const panel = () => p.locator("#graph-stage .graph-inspector").boundingBox();
  const onCanvas = (at: { x: number; y: number }) => p.evaluate(([x, y]) => document.elementFromPoint(x, y)?.tagName === "CANVAS", [box.x + at.x, box.y + at.y]);
  /** The camera has made room for the panel (`open`), or has given it back. */
  const room = (open: boolean) =>
    p
      .waitForFunction(
        (open) => {
          const r = (window as any).ursaShell.graph()?.room;
          return r && !r.waits && r.inset.some((v: number) => v > 0) === open;
        },
        open,
        { timeout: 10_000, polling: 50 },
      )
      .then(() => true, () => false);
  const cdp = phone ? await rctx.newCDPSession(p) : null;
  const touch = (type: "touchStart" | "touchMove" | "touchEnd", at: { x: number; y: number }[]) => cdp!.send("Input.dispatchTouchEvent", { type, touchPoints: at.map((f, id) => ({ x: box.x + f.x, y: box.y + f.y, id })) });
  const click = async (at: { x: number; y: number }) => {
    if (!phone) return p.mouse.click(box.x + at.x, box.y + at.y);
    await touch("touchStart", [at]);
    await touch("touchEnd", []);
  };
  const dblclick = async (at: { x: number; y: number }) => {
    if (!phone) return p.mouse.dblclick(box.x + at.x, box.y + at.y);
    await click(at);
    await p.waitForTimeout(60);
    await click(at);
  };
  const drag = async (from: { x: number; y: number }, to: { x: number; y: number }) => {
    const steps = 8;
    const at = (i: number) => ({ x: from.x + ((to.x - from.x) * i) / steps, y: from.y + ((to.y - from.y) * i) / steps });
    if (phone) {
      await touch("touchStart", [from]);
      for (let i = 1; i <= steps; i++) await touch("touchMove", [at(i)]);
      await touch("touchEnd", []);
    } else {
      await p.mouse.move(box.x + from.x, box.y + from.y);
      await p.mouse.down();
      for (let i = 1; i <= steps; i++) await p.mouse.move(box.x + at(i).x, box.y + at(i).y);
      await p.mouse.up();
    }
  };
  const esc = async () => {
    await p.focus("#graph-stage canvas");
    await p.keyboard.press("Escape");
  };
  // Along the side the panel takes: across a desktop's view, down a phone's.
  const along = (at: { x: number; y: number }) => (phone ? at.y : at.x);
  const extent = phone ? box.height : box.width;
  const within = (at: { x: number; y: number }) => at.x >= 0 && at.y >= 0 && at.x <= box.width && at.y <= box.height;
  const find = (all: At[], id: number) => all.find((n) => n.id === id)!;

  // A node to expand, drawn where the panel opens (a drag puts it there:
  // the camera no longer follows the layout), and a node to select, far
  // from that side.
  const expandable: At[] = (await p.evaluate(() => (window as any).ursaShell.graphExpandable(100))).map((n: any) => ({ id: Number(n.id), x: n.x, y: n.y }));
  const b0 = expandable.filter((n) => n.id !== 0).reduce((a, b) => (along(b) > along(a) ? b : a), expandable[0]);
  const spot = phone ? { x: b0.x, y: box.height - 40 } : { x: box.width - 40, y: 60 };
  await drag({ x: 5, y: 5 }, { x: 5 + spot.x - b0.x, y: 5 + spot.y - b0.y });
  const before = await places();
  const b = find(before, b0.id);
  const a = before.filter((n) => n.id !== b.id && n.x > 20 && n.y > 20 && n.x < box.width - 20 && n.y < box.height - 20).reduce((a, b) => (along(b) < along(a) ? b : a));
  check(Math.hypot(b.x - spot.x, b.y - spot.y) < 3 && along(a) < extent / 2 && !(await graphInfo(p)).room.following, `#493, ${name}: a node to expand is drawn where the panel opens, and one to select away from it`, `node ${b?.id} at ${Math.round(b.x)},${Math.round(b.y)}, node ${a?.id} at ${Math.round(a.x)},${Math.round(a.y)}, of ${Math.round(box.width)} x ${Math.round(box.height)}`);

  // Selecting opens the panel at once, over the first node; then the
  // camera makes room, and no node in sight is under the panel.
  await click(a);
  const opened = await inspector();
  check(await room(true), `#493, ${name}: the camera makes room for the panel`, JSON.stringify((await graphInfo(p)).room));
  const over = (await panel())!;
  const wasUnder = box.x + b.x > over.x && box.x + b.x < over.x + over.width && box.y + b.y > over.y && box.y + b.y < over.y + over.height;
  const made = await places();
  const clearNow = await Promise.all(made.filter(within).map(onCanvas));
  check(
    opened?.kind === "node" && opened.side === "right" && wasUnder && before.filter(within).every((n) => within(find(made, n.id))) && clearNow.length >= before.filter(within).length && clearNow.every(Boolean),
    `#493, ${name}: every node that was in sight is in sight still, and none is under the panel`,
    `side ${opened?.side}; node ${b.id} was at ${Math.round(b.x)},${Math.round(b.y)}, the panel at ${JSON.stringify(over)}; ${clearNow.filter(Boolean).length} of the ${clearNow.length} in sight clear`,
  );
  if (shots) await p.screenshot({ path: `${shots}/graph-inspector-room-${phone ? "phone" : "desktop"}.png` });

  // Closing the panel gives the view back as it was.
  await esc();
  const back = await room(false);
  const restored = await places();
  const moved = Math.max(...before.map((n) => Math.hypot(find(restored, n.id).x - n.x, find(restored, n.id).y - n.y)));
  check(back && (await inspector()) === null && moved < 1, `#493, ${name}: closing the panel puts the view back where it was`, `${moved.toFixed(2)} px`);

  // Under prefers-reduced-motion the camera moves in one step; otherwise over several frames.
  const steps = async (reduced: boolean) => {
    await p.emulateMedia({ reducedMotion: reduced ? "reduce" : "no-preference" });
    await p.evaluate(() => {
      const w = window as any;
      w.__room = [];
      const look = () => {
        const now = JSON.stringify(w.ursaShell.graph()?.room.inset);
        if (w.__room.at(-1) !== now) w.__room.push(now);
      };
      look();
      w.__look = setInterval(look, 5);
    });
    await click(find(await places(), a.id));
    await room(true);
    const seen: string[] = await p.evaluate(() => {
      clearInterval((window as any).__look);
      return (window as any).__room;
    });
    return seen.length - 1;
  };
  const eased = await steps(false);
  await esc();
  await room(false);
  const atOnce = await steps(true);
  await p.emulateMedia({ reducedMotion: "no-preference" });
  check(eased > 2 && atOnce === 1, `#493, ${name}: the camera eases to make room, and moves in one step under prefers-reduced-motion`, `${eased} steps, then ${atOnce}`);

  // The acceptance: with the panel open, a double-click on the node that
  // was under it expands it.
  const nodes0 = (await graphInfo(p)).nodes;
  const target = find(await places(), b.id);
  const reached = await onCanvas(target);
  await dblclick(target);
  await idle(p);
  await graphSettled(p);
  const grown = await graphInfo(p);
  const said = (await screen(p)).replace(/\s+/g, "");
  check(
    reached && grown.nodes > nodes0 && said.includes(`fromthegraphmemoryU.expand${b.id}`) && (await inspector())?.title === `node ${b.id}`,
    `#493, ${name}: with the panel open, a double-click on a node that was under it expands the node`,
    `${nodes0} -> ${grown.nodes} nodes; inspecting ${(await inspector())?.title}`,
  );

  // A fit is of the part the panel leaves clear, and follows the layout there.
  await room(true);
  await p.focus("#graph-stage canvas");
  await p.keyboard.press("0");
  await p.waitForTimeout(100);
  const fit = await places();
  const fitClear = await Promise.all(fit.map(onCanvas));
  const info = await graphInfo(p);
  check(info.room.following && fit.every(within) && fitClear.every(Boolean), `#493, ${name}: 0 fits the graph in the part of the view the panel leaves clear`, `${fitClear.filter(Boolean).length} of ${fit.length} clear; ${JSON.stringify(info.room)}`);
  // The panel is still reached from the keys: i goes in, Esc comes back.
  await p.keyboard.press("i");
  const inPanel = (await inspector())?.focused;
  await p.keyboard.press("Escape");
  check(inPanel === true && (await p.evaluate(() => document.activeElement?.tagName)) === "CANVAS" && (await inspector()) !== null, `#493, ${name}: i still moves into the panel, and Esc back to the view`);

  // The panel takes the other side when what is selected is drawn under
  // its own: a second click or tap reaches the node at once.
  await esc();
  await room(false);
  const last = (await places()).filter(within).reduce((a, b) => (along(b) > along(a) ? b : a));
  await drag({ x: 5, y: 5 }, phone ? { x: 5, y: 5 + extent - 40 - along(last) } : { x: 5 + extent - 40 - along(last), y: 5 });
  const low = find(await places(), last.id);
  await click(low);
  const flipped = await inspector();
  check(flipped?.side === "left" && (await onCanvas(low)), `#493, ${name}: the panel opens on the other side when the node selected is drawn under its own`, `side ${flipped?.side}, node ${low.id} at ${Math.round(along(low))} of ${Math.round(extent)}`);
  check((await errors(p)) === 0, `#493, ${name}: no error`, `${await errors(p)}`);
  await rctx.close();
}
await browser.close();
console.log(failures.length ? `\n${failures.length} failures:\n- ${failures.join("\n- ")}` : "\nall passed");
process.exit(failures.length ? 1 : 0);
