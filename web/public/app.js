// The Ursa Web Shell: the `ursa` shell itself, in a terminal on the page.
//
// The shell (dot-commands, SQL, tables) is the CLI's, compiled to WebAssembly
// and running in a Web Worker (engine-worker.js). This page is the terminal
// around it, as a real terminal is around the CLI: xterm.js draws the screen,
// and the line editor here sends the shell each typed line and writes back
// what it printed. The page adds what only a page can do: the Datasets menu,
// Share, Import, downloads, and the theme.
import { Terminal } from "./vendor/xterm.mjs";
import { FitAddon } from "./vendor/addon-fit.mjs";
import { DATASETS } from "./datasets.js";

const $ = (id) => document.getElementById(id);
const el = (tag, props = {}, ...children) => {
  const node = document.createElement(tag);
  Object.assign(node, props);
  node.append(...children);
  return node;
};

// ---------------------------------------------------------------- engine

// The worker that runs the engine. If the engine crashes (a WebAssembly trap,
// such as running out of memory), the page replaces the worker and starts a
// fresh session: see `crashed`.
let worker;
let engineStarted;
let nextId = 0;
const calls = new Map();

function startWorker() {
  worker?.terminate();
  for (const call of calls.values()) call.reject(Object.assign(new Error("the engine stopped"), { crash: true }));
  calls.clear();
  let started;
  engineStarted = new Promise((resolve, reject) => (started = { resolve, reject }));
  engineStarted.catch(() => {});
  worker = new Worker(new URL("./engine-worker.js", import.meta.url), { type: "module" });
  worker.onmessage = ({ data }) => {
    if ("ready" in data) {
      if (data.ready) started.resolve(data);
      else started.reject(new Error(data.error));
      return;
    }
    const call = calls.get(data.id);
    calls.delete(data.id);
    if (!call) return;
    if (data.ok) call.resolve(data);
    else call.reject(Object.assign(new Error(data.error), { crash: call.crashes }));
  };
  // An error the worker didn't catch: it may be in any state.
  worker.onerror = (e) => {
    e.preventDefault();
    for (const call of calls.values()) call.reject(Object.assign(new Error(e.message || "the engine stopped"), { crash: true }));
    calls.clear();
  };
}
startWorker();

// The calls whose failure means the engine itself broke: the shell reports
// SQL and command errors in what it prints, so `feed` only rejects when the
// engine trapped. `export` and `addFile` reject for ordinary mistakes.
const CRASHES = new Set(["start", "feed", "cancel", "width", "highlight"]);

function engine(op, args = {}, transfer = []) {
  return engineStarted.then(
    () =>
      new Promise((resolve, reject) => {
        const id = nextId++;
        calls.set(id, { resolve, reject, crashes: CRASHES.has(op) });
        worker.postMessage({ id, op, ...args }, transfer);
      }),
  );
}

// ---------------------------------------------------------------- theme

// The terminal's colours: the page's, with the ANSI colours the shell uses
// (yellow strings, blue dot-commands, grey types and nulls, red errors, and
// 256-colour 208 for the prompt) taken from the star-temperature ramp.
const THEMES = {
  light: {
    background: "#ffffff", foreground: "#12120f", cursor: "#12120f", cursorAccent: "#ffffff",
    selectionBackground: "#1f3e8c33", selectionInactiveBackground: "#1f3e8c22",
    black: "#12120f", red: "#c0262d", green: "#3f7f3f", yellow: "#a8650f", blue: "#1f3e8c",
    magenta: "#7a3e8c", cyan: "#2f6f7f", white: "#5a5852",
    brightBlack: "#8a877f", brightRed: "#c4491b", brightGreen: "#3f7f3f", brightYellow: "#b07728",
    brightBlue: "#4b6cb0", brightMagenta: "#7a3e8c", brightCyan: "#2f6f7f", brightWhite: "#12120f",
    prompt: "#c4491b",
  },
  dark: {
    background: "#0b0b0a", foreground: "#f4f1ea", cursor: "#f4f1ea", cursorAccent: "#0b0b0a",
    selectionBackground: "#aabfff40", selectionInactiveBackground: "#aabfff26",
    black: "#0b0b0a", red: "#ff6b6b", green: "#8fd18f", yellow: "#ffd2a1", blue: "#aabfff",
    magenta: "#d6a8ff", cyan: "#9fd8e0", white: "#f4f1ea",
    brightBlack: "#8a877f", brightRed: "#ff9f51", brightGreen: "#8fd18f", brightYellow: "#ffd2a1",
    brightBlue: "#c8d6ff", brightMagenta: "#d6a8ff", brightCyan: "#9fd8e0", brightWhite: "#ffffff",
    prompt: "#ff9f51",
  },
};

function xtermTheme(name) {
  const { prompt, ...theme } = THEMES[name];
  // 256-colour 208 (the prompt's) is extendedAnsi[208 - 16].
  const extendedAnsi = [];
  extendedAnsi[208 - 16] = prompt;
  return { ...theme, extendedAnsi };
}

function currentTheme() {
  const set = document.documentElement.dataset.theme;
  if (set) return set;
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}
$("theme").onclick = () => {
  const next = currentTheme() === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try {
    localStorage.setItem("ursa-theme", next);
  } catch {}
  term.options.theme = xtermTheme(next);
};
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  term.options.theme = xtermTheme(currentTheme());
});

// ---------------------------------------------------------------- terminal

// xterm measures the font when it opens: have it loaded first. Its DOM
// renderer draws the text with the browser's own text rendering (and
// selection). Box drawing comes from "Ursa Box" (style.css), whose lines run
// past each row, so a table's borders join.
await Promise.all([
  document.fonts.load('13px "IBM Plex Mono"'),
  document.fonts.load('13px "Ursa Box"', "\u2502"),
]).catch(() => {});
const term = new Terminal({
  fontFamily: '"Ursa Box", "IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
  fontSize: 13,
  lineHeight: 1,
  theme: xtermTheme(currentTheme()),
  convertEol: true,
  cursorBlink: true,
  scrollback: 10_000,
});
const fit = new FitAddon();
term.loadAddon(fit);
term.open($("terminal"));
fit.fit();

/** Tables fit the terminal, a column kept free. */
function sendWidth() {
  engine("width", { columns: Math.max(20, term.cols - 1) });
}
new ResizeObserver(() => {
  fit.fit();
  sendWidth();
}).observe($("terminal"));

term.attachCustomKeyEventHandler((e) => {
  if (e.type !== "keydown") return true;
  // Ctrl+C (Cmd+C) with text selected copies it, as in a terminal app.
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "c" && term.hasSelection()) {
    navigator.clipboard?.writeText(term.getSelection());
    return false;
  }
  return true;
});
// Shift+Enter starts a new line in what is typed. xterm would also turn its
// keypress into Enter, so the key never reaches xterm.
term.textarea?.addEventListener(
  "keydown",
  (e) => {
    if (e.key !== "Enter" || !e.shiftKey || e.isComposing) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    onKey("\n");
  },
  true,
);

const write = (s) => new Promise((resolve) => term.write(s, resolve));

// ---------------------------------------------------------------- the line editor

// What is typed: text that may hold newlines (Shift+Enter, a paste, a
// statement from the history), and the cursor in it.
let text = "";
let cursor = 0;
// The prompt for the next line, from the shell: `memory U ` or spaces as wide.
let prompt = "memory U ";
let promptColor = ["", ""];
// How many rows the cursor is below where the drawn input starts.
let cursorRow = 0;
let busy = true;
// `text` coloured by the shell's highlighter, once it has answered.
let colored = { text: "", ansi: "" };

const HISTORY_KEY = "ursa-shell-history";
let history = [];
try {
  history = JSON.parse(localStorage.getItem(HISTORY_KEY) ?? "[]").filter((h) => typeof h === "string");
} catch {}
let historyAt = history.length;
let draft = "";

function remember(entry) {
  if (!entry.trim()) return;
  history = history.filter((h) => h !== entry);
  history.push(entry);
  history = history.slice(-500);
  historyAt = history.length;
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(history));
  } catch {}
}

/** Lines of `text`, coloured once the shell's highlighter has answered. */
function coloredLines() {
  if (colored.text === text) return colored.ansi.split("\n");
  const asked = text;
  engine("highlight", { text: asked }).then(({ text: ansi }) => {
    if (asked === text && !busy) {
      colored = { text: asked, ansi };
      draw();
    }
  });
  return text.split("\n");
}

const length = (s) => [...s].length;

/** Draw the prompt and what is typed, and put the cursor where it is. */
function draw() {
  const cols = term.cols;
  const lines = text.split("\n");
  const colors = coloredLines();
  let out = "\r" + (cursorRow > 0 ? `\x1b[${cursorRow}A` : "") + "\x1b[J";
  let row = 0; // the row this line starts on
  let end = 0; // the row the last line ends on
  let offset = 0;
  let target = { row: 0, col: 0 };
  lines.forEach((line, i) => {
    if (i > 0) out += "\r\n";
    out += (i === 0 ? `${promptColor[0]}${prompt}${promptColor[1]}` : " ".repeat(prompt.length)) + (colors[i] ?? line);
    const width = prompt.length + length(line);
    // A line that fills its last row exactly leaves the cursor waiting to
    // wrap: wrap it now, so the rows below are where they are counted.
    if (width > 0 && width % cols === 0) out += " \r";
    if (cursor >= offset && cursor <= offset + line.length) {
      const at = prompt.length + length(line.slice(0, cursor - offset));
      target = { row: row + Math.floor(at / cols), col: at % cols };
    }
    end = row + Math.floor(width / cols);
    row = end + 1;
    offset += line.length + 1;
  });
  const up = end - target.row;
  out += (up > 0 ? `\x1b[${up}A` : "") + "\r" + (target.col > 0 ? `\x1b[${target.col}C` : "");
  cursorRow = target.row;
  term.write(out);
}

/** Take what is drawn off the screen (before the page runs a command). */
function erase() {
  term.write("\r" + (cursorRow > 0 ? `\x1b[${cursorRow}A` : "") + "\x1b[J");
  cursorRow = 0;
}

function insert(s) {
  text = text.slice(0, cursor) + s + text.slice(cursor);
  cursor += s.length;
  draw();
}

function setText(s) {
  text = s;
  cursor = s.length;
  draw();
}

/** Leave the input on screen as typed, and go to the next line for output. */
async function finishLine() {
  cursor = text.length;
  draw();
  cursorRow = 0;
  await write("\r\n");
}

const lineStart = () => text.lastIndexOf("\n", cursor - 1) + 1;
const lineEnd = () => (text.indexOf("\n", cursor) < 0 ? text.length : text.indexOf("\n", cursor));

// Keys pressed while a line runs wait for it, as a terminal keeps them.
let typeahead = [];

term.onData((data) => onKey(data));

function onKey(data) {
  if (answer) answer(data);
  else if (busy) typeahead.push(data);
  else key(data);
}

// A question the page asks (a shared link's "Run N shared commands? [y/N]"):
// while one waits, the next key answers it.
let answer = null;

/** Write `question` and resolve to the next key pressed ("" if a fresh session starts first). */
function ask(question) {
  return write(question).then(
    () =>
      new Promise((resolve) => {
        term.options.cursorBlink = true;
        answer = (data) => {
          answer = null;
          term.options.cursorBlink = false;
          resolve(data);
        };
      }),
  );
}

// Control characters, which a terminal would act on (ESC starts a sequence
// that can move the cursor, clear the screen or hide text), and invisible
// format characters, as the shell's `.print` writes them: \u{1b}, \r. Lines
// and tabs stay.
const CONTROL = /[\x00-\x08\x0b-\x1f\x7f-\x9f\u061c\u180e\u200b\u200e\u200f\u2028-\u202e\u2060-\u2064\u2066-\u206f\ufeff\ufff9-\ufffb]/g;
const visible = (s) => s.replace(CONTROL, (c) => (c === "\r" ? "\\r" : `\\u{${c.codePointAt(0).toString(16)}}`));

function key(data) {
  // A paste arrives in one piece: its lines stay in the input, to run with Enter.
  if (data.length > 1 && !data.startsWith("\x1b")) {
    insert(data.replace(/\r\n?/g, "\n"));
    return;
  }
  switch (data) {
    case "\r":
      submitTyped();
      return;
    case "\n": // Shift+Enter
      insert("\n");
      return;
    case "\x7f": // Backspace
    case "\b":
      if (cursor > 0) {
        text = text.slice(0, cursor - 1) + text.slice(cursor);
        cursor--;
        draw();
      }
      return;
    case "\x1b[3~": // Delete
      if (cursor < text.length) {
        text = text.slice(0, cursor) + text.slice(cursor + 1);
        draw();
      }
      return;
    case "\x1b[D": // Left
    case "\x02":
      if (cursor > 0) cursor--;
      draw();
      return;
    case "\x1b[C": // Right
    case "\x06":
      if (cursor < text.length) cursor++;
      draw();
      return;
    case "\x1b[H": // Home
    case "\x01": // Ctrl+A
      cursor = lineStart();
      draw();
      return;
    case "\x1b[F": // End
    case "\x05": // Ctrl+E
      cursor = lineEnd();
      draw();
      return;
    case "\x0b": // Ctrl+K: delete to the end of the line
      text = text.slice(0, cursor) + text.slice(lineEnd());
      draw();
      return;
    case "\x15": {
      // Ctrl+U: delete to the start of the line
      const start = lineStart();
      text = text.slice(0, start) + text.slice(cursor);
      cursor = start;
      draw();
      return;
    }
    case "\x17": {
      // Ctrl+W: delete the word before the cursor
      const before = text.slice(0, cursor).replace(/\S+\s*$/, "");
      text = before + text.slice(cursor);
      cursor = before.length;
      draw();
      return;
    }
    case "\x1b[A": // Up: the line above, or the history
      if (lineStart() === 0) {
        if (historyAt === 0) return;
        if (historyAt === history.length) draft = text;
        historyAt--;
        setText(history[historyAt]);
      } else {
        const column = cursor - lineStart();
        const start = text.lastIndexOf("\n", lineStart() - 2) + 1;
        cursor = Math.min(start + column, lineStart() - 1);
        draw();
      }
      return;
    case "\x1b[B": // Down: the line below, or the history
      if (lineEnd() === text.length) {
        if (historyAt >= history.length) return;
        historyAt++;
        setText(historyAt === history.length ? draft : history[historyAt]);
      } else {
        const column = cursor - lineStart();
        const start = lineEnd() + 1;
        const end = text.indexOf("\n", start) < 0 ? text.length : text.indexOf("\n", start);
        cursor = Math.min(start + column, end);
        draw();
      }
      return;
    case "\x03": // Ctrl+C: drop what is typed
      cancelTyped();
      return;
    case "\x0c": // Ctrl+L
      clearScreen();
      return;
    case "\t":
      insert("  ");
      return;
    default:
      if (data >= " " && !data.startsWith("\x1b")) insert(data);
  }
}

/** Ctrl+C: drop what is typed, and the statement the shell holds. */
function cancelTyped() {
  enqueue(async () => {
    await finishLine();
    await write("^C\r\n");
    await cancelPending();
    text = "";
    cursor = 0;
    historyAt = history.length;
  });
}

function clearScreen() {
  term.clear();
  term.write("\x1b[H\x1b[2J");
  cursorRow = 0;
  draw();
}

// ---------------------------------------------------------------- running

// Everything that runs does so as a job, one after another: typed lines, the
// page's commands, a dataset, New. While jobs are waiting the terminal is busy
// and keys wait for them (`typeahead`). A job that throws never stops the
// ones after it: its error is printed, and a crashed engine is replaced.
let queue = Promise.resolve();
let waiting = 0;
// The statements and commands this session ran, for Share.
let ran = [];
// The lines of the statement being fed, until it runs.
let statement = [];
let uploads = [];
// Bumped by every fresh session (New, a dataset, a crash): jobs queued for an
// older session are skipped.
let generation = 0;
// What was typed when the page started running its own commands, to put back.
let stash = null;

function setBusy(on) {
  busy = on;
  term.options.cursorBlink = !on;
}

/** Queue `job`; resolves to its result, or to `undefined` if it failed. */
function enqueue(job) {
  waiting++;
  setBusy(true);
  const done = queue.then(job).catch(async (e) => {
    await failed(e);
    return undefined;
  });
  queue = done.finally(() => {
    if (--waiting === 0) ready();
  });
  return done;
}

/** A job threw: say so, and if the engine broke, start a new one. */
async function failed(e) {
  if (!e?.crash) {
    await write(`\x1b[31mError: ${String(e?.message ?? e)}\x1b[0m\r\n`);
    return;
  }
  await write(`\x1b[31mError: the engine stopped (${e.message}). Starting a fresh session: what was defined is gone.\x1b[0m\r\n`);
  startWorker();
  generation++;
  await restart({ clear: false });
}

/** Back at the prompt: draw it, then the keys pressed meanwhile. */
function ready() {
  setBusy(false);
  if (stash && !text) ({ text, cursor } = stash);
  stash = null;
  draw();
  const keys = typeahead;
  typeahead = [];
  // A key may start another job (Enter): the rest wait for it.
  for (let i = 0; i < keys.length; i++) {
    if (busy) {
      typeahead.push(...keys.slice(i));
      return;
    }
    key(keys[i]);
  }
}

/** Enter: what is typed goes to the shell, a line at a time. */
function submitTyped() {
  const entry = text;
  enqueue(async () => {
    await finishLine();
    remember(entry);
    text = "";
    cursor = 0;
    for (const line of entry.split("\n")) await feed(line, { echo: false });
  });
}

/**
 * Run the page's `commands` as if typed, each drawn after the prompt and fed
 * to the shell a line at a time, in order, stopping at the first that fails.
 * They are queued at once, so nothing queued later runs between them, and
 * they are skipped if a fresh session starts before they run. What is being
 * typed is put back afterwards. Resolves to whether all of them ran.
 */
function runAll(commands, { session = generation, replay = false } = {}) {
  let ok = true;
  const jobs = commands.map((command) =>
    enqueue(async () => {
      if (!ok || session !== generation) return (ok = false);
      setAside();
      // A statement left half typed would take this command as its next line.
      if (statement.length) await cancelPending();
      replaying = replay;
      try {
        for (const line of command.split("\n")) ok = (await feed(line, { echo: true })) && ok;
      } finally {
        replaying = false;
      }
      return ok;
    }),
  );
  return Promise.all(jobs).then((results) => results.every((r) => r === true));
}

const run = (command) => runAll([command]);

// Whether the commands running are a shared link's, which may not download.
let replaying = false;

/** Take what is typed off the screen, to put back once the page's jobs are done. */
function setAside() {
  if (text && !stash) stash = { text, cursor };
  erase();
  text = "";
  cursor = 0;
}

/** Drop the statement the shell holds, unrun. */
async function cancelPending() {
  const r = await engine("cancel");
  prompt = r.prompt;
  statement = [];
}

// Commands that only look: Share leaves them out.
const LOOKS = /^\s*\.(help|tables|graphs|describe|functions|version|print|history|files|share|quit|exit|dataset)\b/;

/** Feed one line to the shell and write what it printed. True if no error. */
async function feed(line, { echo }) {
  if (echo) {
    // Shown as typed, but with control characters escaped: a command from a
    // shared link cannot rewrite the screen as it is echoed.
    const { text: ansi } = await engine("highlight", { text: visible(line) });
    await write(`${promptColor[0]}${prompt}${promptColor[1]}${ansi}\r\n`);
  }
  statement.push(line);
  const r = await engine("feed", { line });
  let ok = !r.err;
  if (r.flow === "host") {
    statement = [];
    ok = (await hostCommand(r.command)) !== false;
  } else {
    await write(r.out + r.err);
    if (r.flow === "exit") await write("This shell lives in the page: New starts a fresh session.\r\n");
    if (!r.pending) {
      const done = statement.join("\n");
      statement = [];
      if (done.trim() && ok && !LOOKS.test(done)) ran.push(done);
    }
  }
  prompt = r.pending ? r.continuation : r.prompt;
  return ok;
}

// ---------------------------------------------------------------- the page's own commands

const HOST_COMMANDS = [
  [".clear", "", "Clear the screen (also Ctrl+L)"],
  [".dataset", "?NAME?", "List the sample datasets, or load one in a fresh session"],
  [".export", "FORMAT", "Download the last result: csv, parquet, json or arrow"],
  [".files", "?add?", "List the files you can query by name; add picks some from your computer"],
  [".history", "?N?", "Show the last N commands (default 20)"],
  [".share", "", "Copy a link that replays this session's commands"],
];

const SHORTCUTS = `Enter            Run the line (a statement runs once it ends with ;)
Shift+Enter      Start a new line in what is typed
Up, Down         Step through the history (kept in this browser)
Ctrl+C           Copy the selection; with none, drop what is typed
Ctrl+L           Clear the screen
Ctrl+A, Ctrl+E   Go to the start or end of the line
Ctrl+U, Ctrl+K   Delete to the start or end of the line
Ctrl+W           Delete the word before the cursor`;

const errorLine = (message) => write(`\x1b[31mError: ${message}\x1b[0m\r\n`).then(() => false);

/** Run a command the page added. False if it failed. */
async function hostCommand(line) {
  const [name, ...rest] = line.trim().replace(/;\s*$/, "").split(/\s+/);
  const arg = rest.join(" ");
  switch (name) {
    case ".clear":
      term.clear();
      await write("\x1b[H\x1b[2J");
      return true;
    case ".share":
      return share();
    case ".export":
      if (replaying) {
        await write("Skipped .export: a shared link does not download files. Run it yourself to download.\r\n");
        return true;
      }
      return exportLast(arg.toLowerCase());
    case ".files":
      if (arg === "add") {
        $("file-input").click();
        return true;
      }
      if (arg) return errorLine("usage: .files ?add?");
      return listFiles();
    case ".history": {
      const n = arg ? Number(arg) : 20;
      if (!Number.isInteger(n) || n < 1) return errorLine("usage: .history ?N?");
      const first = Math.max(0, history.length - n);
      const width = String(history.length).length;
      for (let i = first; i < history.length; i++) {
        const { text: ansi } = await engine("highlight", { text: history[i], alone: true });
        await write(`\x1b[90m${String(i + 1).padStart(width)}\x1b[0m  ${ansi.replace(/\n/g, "\n" + " ".repeat(width + 2))}\r\n`);
      }
      return true;
    }
    case ".dataset": {
      if (!arg) {
        const rows = DATASETS.map((d) => `('${d.id}', '${d.title.replace(/'/g, "''")}')`).join(", ");
        return quiet(`SELECT * FROM (VALUES ${rows}) AS datasets(name, title);`);
      }
      const d = DATASETS.find((x) => x.id === arg.toLowerCase());
      if (!d) return errorLine(`no dataset named '${arg}'; .dataset lists them`);
      // Queued now, it runs once this line's job has finished.
      loadDataset(d);
      return true;
    }
  }
  return errorLine(`unknown command ${name}`);
}

/** Run `sql` in the shell for a listing the page makes; not recorded. */
async function quiet(sql) {
  const r = await engine("feed", { line: sql });
  await write(r.out + r.err);
  return !r.err;
}

let manifest = null;
async function listFiles() {
  manifest ??= await (await fetch("/data/manifest.json")).json();
  const rows = manifest.datasets.flatMap((d) => d.files.map((f) => `('${f.name}', ${f.bytes}, '${d.title.replace(/'/g, "''")}')`));
  rows.push(...uploads.map((u) => `('${u.url.replace(/'/g, "''")}', ${u.bytes}, 'imported')`));
  return quiet(`SELECT name AS file, bytes, dataset FROM (VALUES ${rows.join(", ")}) AS t(name, bytes, dataset) ORDER BY dataset = 'imported', name;`);
}

const EXTENSIONS = { csv: "csv", parquet: "parquet", json: "json", arrow: "arrows" };
const MIME = { csv: "text/csv", parquet: "application/vnd.apache.parquet", json: "application/json", arrow: "application/vnd.apache.arrow.stream" };

async function exportLast(format) {
  if (!EXTENSIONS[format]) return errorLine("usage: .export csv|parquet|json|arrow");
  let bytes;
  try {
    ({ bytes } = await engine("export", { format }));
  } catch (e) {
    return errorLine(e.message);
  }
  const name = `ursa-result.${EXTENSIONS[format]}`;
  const url = URL.createObjectURL(new Blob([bytes], { type: MIME[format] }));
  el("a", { href: url, download: name }).click();
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
  await write(`Downloaded ${name} (${formatBytes(bytes.byteLength)}).\r\n`);
  return true;
}

function formatBytes(n) {
  return n < 1024 ? `${n} B` : n < 1024 ** 2 ? `${(n / 1024).toFixed(1)} KB` : `${(n / 1024 ** 2).toFixed(1)} MB`;
}

// ---------------------------------------------------------------- the bar

/** A fresh shell (New, a dataset, after a crash), and a clear screen. */
async function restart({ clear = true } = {}) {
  const r = await engine("start", { commands: HOST_COMMANDS, shortcuts: SHORTCUTS });
  prompt = r.prompt;
  promptColor = r.promptColor;
  sendWidth();
  ran = [];
  uploads = [];
  statement = [];
  history_replace("");
  if (clear) term.reset();
  cursorRow = 0;
  await write(`Ursa Web Shell running...\r\nUrsa v${r.version}\r\nEnter ".help" for usage hints.\r\n`);
}

/** Start a fresh session; what was queued for the old one is skipped. */
function fresh() {
  const session = ++generation;
  // A question waiting (a shared link's) is answered no.
  answer?.("");
  enqueue(async () => {
    if (session === generation) await restart();
  });
  return session;
}

$("new").onclick = () => {
  fresh();
  term.focus();
};

function loadDataset(d) {
  const session = fresh();
  return runAll(d.commands, { session });
}

// Share: a link whose fragment holds the session's commands, replayed on open.
function shareUrl() {
  const encoded = btoa(unescape(encodeURIComponent(JSON.stringify(ran))))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
  return `${location.origin}${location.pathname}#run=${encoded}`;
}
async function share() {
  if (!ran.length) {
    await write("No commands to share.\r\n");
    return true;
  }
  const url = shareUrl();
  history_replace(url);
  const n = `${ran.length} command${ran.length === 1 ? "" : "s"}`;
  try {
    await navigator.clipboard.writeText(url);
    await write(`Copied a link that replays ${n}.\r\n`);
  } catch {
    await write(`A link that replays ${n} is in the address bar.\r\n`);
  }
  if (uploads.length) await write("Imported files stay in this browser: whoever opens the link imports them again.\r\n");
  return true;
}
$("share").onclick = () => {
  run(".share");
  term.focus();
};

function history_replace(url) {
  window.history.replaceState(null, "", url || location.pathname);
}

function sharedCommands() {
  const m = location.hash.match(/^#run=([A-Za-z0-9_-]+)$/);
  if (!m) return [];
  try {
    const b64 = m[1].replace(/-/g, "+").replace(/_/g, "/");
    const list = JSON.parse(decodeURIComponent(escape(atob(b64))));
    return Array.isArray(list) ? list.filter((c) => typeof c === "string") : [];
  } catch {
    return [];
  }
}

/**
 * A shared link's commands: show them (control characters escaped, so none
 * can hide what it runs) and run them only if the answer is y. A link cannot
 * download files: `.export` is skipped as they run.
 */
function confirmShared(shared, session) {
  const n = `${shared.length} shared command${shared.length === 1 ? "" : "s"}`;
  enqueue(async () => {
    await write(`This link runs ${n}:\r\n`);
    for (const command of shared) {
      const { text: ansi } = await engine("highlight", { text: visible(command), alone: true });
      await write(`  ${ansi.replace(/\n/g, "\r\n  ")}\r\n`);
    }
    const reply = await ask(`Run ${n}? [y/N] `);
    const yes = reply === "y" || reply === "Y";
    await write(`${yes ? "y" : "N"}\r\n`);
    if (session !== generation) return;
    if (!yes) {
      await write("Not run.\r\n");
      return;
    }
    await write(`Replaying ${n} from a shared link.\r\n`);
    runAll(shared, { session, replay: true });
  });
}

// Import: a file from this computer becomes a view named after it.
const fileInput = $("file-input");
$("import").onclick = () => fileInput.click();
fileInput.onchange = () => {
  const files = [...fileInput.files];
  fileInput.value = "";
  for (const file of files) {
    // Letters (any script) and digits; the rest become _.
    const view =
      file.name
        .replace(/\.(gz|zst|bz2|xz)$/i, "")
        .replace(/\.[^.]+$/, "")
        .toLowerCase()
        .replace(/[^\p{L}\p{N}_]+/gu, "_")
        .replace(/^(\p{N})/u, "_$1") || "imported";
    // Quoted: a file may be named after a keyword (order.csv).
    const quoted = `"${view.replace(/"/g, '""')}"`;
    enqueue(async () => {
      // Off the prompt line: an error prints on lines of its own, and the
      // prompt and what was typed are drawn again after it.
      setAside();
      const buffer = await file.arrayBuffer();
      const { url } = await engine("addFile", { name: file.name, bytes: buffer }, [buffer]);
      uploads.push({ name: file.name, url, bytes: file.size });
      runAll([`CREATE OR REPLACE VIEW ${quoted} AS SELECT * FROM '${url.replace(/'/g, "''")}';`, `FROM ${quoted} LIMIT 10;`]);
    });
  }
  term.focus();
};

// The Datasets menu.
const menu = $("dataset-menu");
const menuButton = $("datasets");
menu.replaceChildren(
  ...DATASETS.map((d) =>
    el("button", {
      type: "button",
      role: "menuitem",
      textContent: d.title,
      onclick: () => {
        closeMenu();
        loadDataset(d);
        term.focus();
      },
    }),
  ),
);
function closeMenu() {
  menu.hidden = true;
  menuButton.setAttribute("aria-expanded", "false");
}
menuButton.onclick = (e) => {
  e.stopPropagation();
  const open = menu.hidden;
  menu.hidden = !open;
  menuButton.setAttribute("aria-expanded", String(open));
  if (open) menu.querySelector("button")?.focus();
};
document.addEventListener("click", (e) => {
  if (!menu.hidden && !menu.contains(e.target)) closeMenu();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !menu.hidden) {
    closeMenu();
    menuButton.focus();
  }
});

// ---------------------------------------------------------------- start

term.write("Loading the engine (about 5 MB)...\r\n");
setBusy(true);
engineStarted.then(
  () => {
    const shared = sharedCommands();
    const wanted = DATASETS.find((d) => d.id === new URLSearchParams(location.search).get("dataset"));
    if (wanted && !shared.length) {
      loadDataset(wanted);
    } else {
      const session = fresh();
      if (shared.length) confirmShared(shared, session);
    }
    term.focus();
  },
  (e) => term.write(`\x1b[31mThe engine failed to start: ${e.message}\x1b[0m\r\n`),
);

// For tests: the screen as text, and a promise that resolves once everything
// queued so far has run.
window.ursaShell = {
  async idle() {
    let seen;
    do {
      seen = queue;
      await seen;
    } while (seen !== queue || waiting > 0);
  },
  screen() {
    const buffer = term.buffer.active;
    const lines = [];
    for (let i = 0; i < buffer.length; i++) lines.push(buffer.getLine(i)?.translateToString(true) ?? "");
    return lines.join("\n").replace(/\n+$/, "");
  },
  busy: () => busy,
  cols: () => term.cols,
  rows: () => term.rows,
  selection: () => term.getSelection(),
  datasets: DATASETS,
};
