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

// Under `ursa serve`, the pane host's page (tests/pane-host/, fulfilled by the
// test, not the server) has no address, so Chromium's Local Network Access
// checks would keep it from the server's WebSocket; a page the server sends
// is not affected.
const launched = await chromium.launch(process.env.URSA_SERVE_TOKEN ? { args: ["--disable-features=LocalNetworkAccessChecks"] } : {});
// Against `ursa serve` (scripts/e2e-serve.ts sets URSA_SERVE_TOKEN): every
// context carries the run's token as the cookie the page gets from the link
// ursa serve prints, and the checks of browser-only features are skipped,
// each with a "skip" line: the engine's crash (a WebAssembly trap), its
// WebAssembly memory, and the wasm engine's files and worker.
const serveToken = process.env.URSA_SERVE_TOKEN ?? "";
const overSocket = serveToken !== "";
const serveCookie = overSocket ? `ursa_token_${new URL(base).port}=${serveToken}` : "";
// The checks a socket run skips, and no others: the suite's end checks that
// exactly these were skipped, so a check can't quietly drop out of the socket
// run (CI runs both transports, .github/workflows/shell-e2e.yml).
const SOCKET_SKIPS = [
  "an engine crash is reported",
  "the shell works after an engine crash",
  "graphMemory: each wasm instance's memory, and the browser's measure of the page",
  "cross-origin isolation headers on /engine/engine.json",
  "cross-origin isolation headers on the engine's .wasm",
  "the worker /engine-worker.js is cross-origin isolated",
];
const skipped: string[] = [];
const skip = (label: string) => {
  skipped.push(label);
  console.log(`skip ${label}: browser-only (the wasm engine)`);
};
const withToken = async <C extends { addCookies(c: { name: string; value: string; url: string }[]): Promise<void> }>(c: C) => {
  if (overSocket) await c.addCookies([{ name: `ursa_token_${new URL(base).port}`, value: serveToken, url: base }]);
  return c;
};
const browser = new Proxy(launched, {
  get(target, key) {
    if (key === "newContext") return async (options?: any) => withToken(await target.newContext(options));
    if (key === "newPage")
      return async (options?: any) => {
        const ctx = await withToken(await target.newContext(options));
        return ctx.newPage();
      };
    const value = (target as any)[key];
    return typeof value === "function" ? value.bind(target) : value;
  },
});
// Bun's own requests (the headers' checks) carry the cookie too.
const fetch = (url: string, init: RequestInit = {}) =>
  globalThis.fetch(url, overSocket ? { ...init, headers: { ...(init.headers as Record<string, string>), cookie: serveCookie } } : init);
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
/**
 * Wait until the view has drawn a frame, its layout has run to the end, and
 * the layout's last positions are drawn: they can come with its "done",
 * after the last frame (`pending`, URSA-10, URSA-11).
 */
async function graphSettled(p: Page) {
  try {
    await p.waitForFunction(
      () => {
        const g = (window as any).ursaShell.graph();
        return g && g.frames > 0 && g.layoutDone && !g.pending;
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
/** Wait until the camera has made room for the inspector's panel, or given it back (#493). */
const roomSettled = (p: Page) => p.waitForFunction(() => (window as any).ursaShell.graph()?.room.waits === false, null, { timeout: 10_000, polling: 50 }).catch(() => {});
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
const runs = Number(process.env.RUNS ?? 1);
for (let run = 0; run < runs; run++) {
console.log(`== run ${run + 1}`);
{
  const rctx = await watchedContext({ viewport: { width: 1280, height: 800 } });
  const r = await rctx.newPage();
  await r.goto(`${base}/?dataset=rmat`);
  await ready(r);
  await idle(r);
  await r.click("#terminal");
  // Each status as it is set, and when: the page is too busy drawing a
  // million edges (in software, headless) to be polled on time.
  await r.evaluate(() => {
    const w = window as any;
    const el = document.getElementById("graph-status")!;
    w.__timed = [];
    new MutationObserver(() => w.__timed.push([performance.now(), el.textContent])).observe(el, { childList: true, characterData: true, subtree: true });
  });
  await r.keyboard.type(".graph rmat");
  const enter: number = await r.evaluate(() => performance.now());
  await r.keyboard.press("Enter");
  await graphSettled(r);
  // #498: the aggregated view. Laid out, the million edges are drawn as a
  // cut through the layout's hierarchy: at the fitted zoom at most 20,000
  // discs and 100,000 merged edges, which the status line says first, the
  // groups together holding every node, the largest labelled with their
  // counts; a cut takes the layout worker 30 ms at most (twice that is
  // allowed here); a click on a group selects its nodes; `a` and
  // `.graph detail on|off` force full detail and let it go; and zooming in
  // reaches the graph's own nodes, drawn as the graph is.
  {
    // The cut asked for (true) or the graph itself (false), drawn: no answer
    // awaited and none still to be drawn. A cut's discs and labels come
    // with the frame after its answer (URSA-19).
    const cutDrawn = (want = true) =>
      r
        .waitForFunction((want) => { const g = (window as any).ursaShell.graph(); return g && !g.cuts.waiting && (g.cut !== null) === want; }, want, { timeout: 30_000, polling: 50 })
        .then(() => true, () => false);
    const statusNow = async () => (await r.locator("#graph-status").textContent()) ?? "";
    const drawn = await cutDrawn();
    const g = await graphInfo(r);
    const status = await statusNow();
    const said = status.match(/^aggregated: ([\d,]+) groups of 46,318 nodes · 1,000,000 edges · /);
    check(
      drawn && g.cut && g.cut.groups <= 20_000 && g.cut.groups > 20 && g.cut.edges <= 100_000 && g.cut.edges > 0 && g.cut.totalEdges >= g.cut.edges && g.coarse === null,
      "a big graph, laid out, is drawn aggregated: at most 20,000 discs and 100,000 merged edges at the fitted zoom",
      JSON.stringify(g.cut),
    );
    check(g.cut?.nodes === 46_318 && said && Number(said[1].replace(/,/g, "")) === g.cut.groups, "a big graph: the groups hold every node, and the status says aggregated: G groups of N nodes", status);
    check(g.cuts.ms.count >= 1 && g.cuts.ms.max <= 60 && g.cuts.prepared?.bytes > 0, "a big graph: a cut takes the layout worker 30 ms at most (60 allowed)", JSON.stringify(g.cuts));
    check(g.groupLabels.length >= 1 && g.groupLabels.every((l: string) => /^[\d,.]+[KM]?$/.test(l)), "a big graph: its largest groups are labelled with their node counts", JSON.stringify(g.groupLabels));
    // A click on the heaviest group's label: its nodes are selected.
    const box = (await r.locator("#graph-stage canvas").boundingBox())!;
    const label = await r.evaluate(() => {
      const el = [...document.querySelectorAll<HTMLElement>("#graph-stage .graph-group")].find((l) => !l.hidden)!;
      const b = el.getBoundingClientRect();
      return { x: b.x + b.width / 2, y: b.y + b.height / 2, text: el.textContent };
    });
    await r.mouse.click(label.x, label.y);
    await r.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
    const picked = await statusNow();
    const members = Number(label.text!.replace(/,/g, ""));
    check(
      new RegExp(`^Selected ${label.text!.replace(".", "\\.")} nodes: .* · aggregated: [\\d,]+ groups of 46,318 nodes · `).test(picked) && (await graphInfo(r)).cut !== null && members > 1,
      "a big graph: a click on a group selects its nodes",
      picked.slice(0, 160),
    );
    // A click on empty space clears. The camera then gives back the room
    // it made for the inspector's panel (#493), over a few frames: the
    // zoom below starts from where it is once it has (URSA-19: under load
    // a cut was asked for between the room's frames, and the first zoom's
    // cut was of a camera still moving).
    await r.mouse.click(box.x + 8, box.y + box.height - 8);
    await r.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
    await roomSettled(r);
    check(/^aggregated: /.test(await statusNow()), "a big graph: a click on empty space clears the group's selection", (await statusNow()).slice(0, 80));
    // `a` forces full detail, and `a` again lets it go.
    await r.locator("#graph-stage canvas").focus();
    await r.keyboard.press("a");
    const forced = await cutDrawn(false);
    const full = await graphInfo(r);
    check(forced && full.cut === null && full.cuts.detail && !full.cuts.on && !(await statusNow()).includes("aggregated") && full.groupLabels.length === 0, "a big graph: a forces full detail", `${JSON.stringify(full.cuts)}; ${(await statusNow()).slice(0, 80)}`);
    // `a` again, with the animation frames held until the cut has come: a
    // cut is drawn, and its groups labelled, by the frame after its answer,
    // and until then the view says it waits (URSA-19: read in between, the
    // labels were []).
    await r.evaluate(() => {
      const w = window as any;
      const [raf, caf] = [window.requestAnimationFrame.bind(window), window.cancelAnimationFrame.bind(window)];
      const held = new Map<number, FrameRequestCallback>();
      let id = 0;
      window.requestAnimationFrame = (cb) => (held.set(--id, cb), id);
      window.cancelAnimationFrame = (n) => (held.has(n) ? held.delete(n) : caf(n));
      w.__giveFrames = () => {
        [window.requestAnimationFrame, window.cancelAnimationFrame] = [raf, caf];
        for (const cb of held.values()) raf(cb);
        held.clear();
      };
    });
    await r.keyboard.press("a");
    const came = await r.waitForFunction(() => (window as any).ursaShell.graph().cut !== null, null, { timeout: 30_000, polling: 50 }).then(() => true, () => false);
    const held = await graphInfo(r);
    await r.evaluate(() => (window as any).__giveFrames());
    const again = (await cutDrawn()) && /^aggregated: /.test(await statusNow());
    const labelled = (await graphInfo(r)).groupLabels;
    check(again, "a big graph: a again aggregates it", (await statusNow()).slice(0, 80));
    check(
      came && held.cuts.waiting && held.groupLabels.length === 0 && labelled.length >= 1,
      "a big graph: a cut and its labels are drawn by the frame after it comes, and the view waits for that frame",
      `frames held: the cut ${came ? "came" : "did not come"}, waiting ${held.cuts.waiting}, labels ${JSON.stringify(held.groupLabels)}; then ${JSON.stringify(labelled)}`,
    );
    // Zooming in on the middle opens the groups in view, down to the
    // graph's own nodes: then the graph itself is drawn.
    const sizes: number[] = [];
    let reached = false;
    for (let step = 0; step < 16 && !reached; step++) {
      await r.evaluate(
        ([x, y]) => {
          const canvas = document.querySelector("#graph-stage canvas")!;
          for (let i = 0; i < 4; i++) canvas.dispatchEvent(new WheelEvent("wheel", { deltaY: -100, clientX: x, clientY: y, bubbles: true, cancelable: true }));
        },
        [box.x + box.width / 2, box.y + box.height / 2],
      );
      // The camera at rest, and the cut for it.
      await r.waitForTimeout(250);
      await r.waitForFunction(() => !(window as any).ursaShell.graph().cuts.waiting, null, { timeout: 30_000, polling: 50 }).catch(() => {});
      const now = await graphInfo(r);
      sizes.push(now.cut?.groups ?? 0);
      reached = now.cut === null;
    }
    const near = await graphInfo(r);
    check(
      reached && sizes.length > 1 && sizes[0] > g.cut.groups && sizes.slice(0, -1).every((n) => n <= 20_000) && near.cuts.on && !(await statusNow()).includes("aggregated"),
      "a big graph: zooming into a region opens its groups and reaches the graph's own nodes",
      `${g.cut.groups} groups fitted, then ${sizes.join(", ")} (0: the graph itself); ${(await statusNow()).slice(0, 80)}`,
    );
    check(near.cuts.ms.max <= 60, "a big graph: every cut on the way took the layout worker 30 ms at most (60 allowed)", JSON.stringify(near.cuts.ms));
    // Fitted again, it is aggregated again.
    await r.keyboard.press("0");
    check((await cutDrawn()) && (await graphInfo(r)).cut.groups === g.cut.groups, "a big graph: fitted again, the same cut as before", JSON.stringify((await graphInfo(r)).cut));
    // `.graph detail on` in the terminal: full detail from here, which is
    // what the checks below (the frame budget and the refinement of the
    // million edges themselves) are of.
    await r.click("#terminal");
    await type(".graph detail", r);
    const asked = await after(".graph detail", r);
    await type(".graph detail on", r);
    const on = await after(".graph detail on", r);
    await cutDrawn(false);
    check(
      asked.includes("drawn aggregated once it is laid out") && on.includes("drawn in full detail") && (await graphInfo(r)).cut === null && !(await statusNow()).includes("aggregated"),
      ".graph detail says which, and .graph detail on draws every node and edge",
      `${asked.trim().slice(0, 60)} | ${on.trim().slice(0, 60)} | ${(await statusNow()).slice(0, 60)}`,
    );
    await type(".graph detail sideways", r);
    check((await after(".graph detail sideways", r)).includes("usage: .graph detail on|off"), ".graph detail refuses another word");
  }
  await rctx.close();
}
}
await browser.close();
console.log(failures.length ? `\n${failures.length} failures:\n- ${failures.join("\n- ")}` : "\nall passed");
process.exit(failures.length ? 1 : 0);
