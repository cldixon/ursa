// The Ursa Web Shell: the `ursa` shell itself, in a terminal on the page.
//
// The shell (dot-commands, SQL, tables) is the CLI's, compiled to WebAssembly
// and running in a Web Worker (engine-worker.js). This page is the terminal
// around it, as a real terminal is around the CLI: xterm.js draws the screen,
// and the line editor here sends the shell each typed line and writes back
// what it printed. The page adds what only a page can do: the Datasets menu,
// Share, Import, downloads, the Graph view (graph.js) and the theme.
import { Terminal } from "./vendor/xterm.mjs";
import { FitAddon } from "./vendor/addon-fit.mjs";
import { DATASETS } from "./datasets.js";
import { setup as setupSearch } from "./graph-search.js";

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
    brightBlack: "#6b6963", brightRed: "#c4491b", brightGreen: "#3f7f3f", brightYellow: "#b07728",
    brightBlue: "#4b6cb0", brightMagenta: "#7a3e8c", brightCyan: "#2f6f7f", brightWhite: "#12120f",
    scrollbarSliderBackground: "#12120f26", scrollbarSliderHoverBackground: "#12120f4d", scrollbarSliderActiveBackground: "#12120f66",
    prompt: "#c4491b",
  },
  dark: {
    background: "#0b0b0a", foreground: "#f4f1ea", cursor: "#f4f1ea", cursorAccent: "#0b0b0a",
    selectionBackground: "#aabfff40", selectionInactiveBackground: "#aabfff26",
    black: "#0b0b0a", red: "#ff6b6b", green: "#8fd18f", yellow: "#ffd2a1", blue: "#aabfff",
    magenta: "#d6a8ff", cyan: "#9fd8e0", white: "#f4f1ea",
    brightBlack: "#8a877f", brightRed: "#ff9f51", brightGreen: "#8fd18f", brightYellow: "#ffd2a1",
    brightBlue: "#c8d6ff", brightMagenta: "#d6a8ff", brightCyan: "#9fd8e0", brightWhite: "#ffffff",
    scrollbarSliderBackground: "#f4f1ea26", scrollbarSliderHoverBackground: "#f4f1ea4d", scrollbarSliderActiveBackground: "#f4f1ea66",
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
// A click on the bar's buttons hands the keys back to the terminal; pressed
// from the keyboard (a click with no pointer), they keep the focus.
const byPointer = (e) => e.detail > 0;
$("theme").onclick = (e) => {
  const next = currentTheme() === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try {
    localStorage.setItem("ursa-theme", next);
  } catch {}
  term.options.theme = xtermTheme(next);
  graphLoaded?.retheme();
  if (byPointer(e)) term.focus();
};
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  term.options.theme = xtermTheme(currentTheme());
  graphLoaded?.retheme();
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
  allowProposedApi: true, // term.unicode.register
});
const fit = new FitAddon();
term.loadAddon(fit);

// ---------------------------------------------------------------- cell widths
//
// How many cells a character takes, for xterm (which draws it) and for the
// line editor (which puts the cursor): the same function, so they agree.
// xterm's own table is Unicode 6, where emoji take one cell. Here CJK and
// emoji take two, combining marks none, and an emoji sequence (ZWJ, skin
// tone, flag, variation selector) is joined into one cluster of two cells.
const WIDE = [
  [0x1100, 0x115f], [0x231a, 0x231b], [0x2329, 0x232a], [0x2e80, 0x303e], [0x3041, 0x33ff], [0x3400, 0x4dbf],
  [0x4e00, 0x9fff], [0xa000, 0xa4cf], [0xa960, 0xa97f], [0xac00, 0xd7a3], [0xf900, 0xfaff], [0xfe10, 0xfe19],
  [0xfe30, 0xfe6f], [0xff00, 0xff60], [0xffe0, 0xffe6], [0x16fe0, 0x18d8f], [0x1aff0, 0x1b2ff], [0x1f200, 0x1f2ff],
  [0x20000, 0x2fffd], [0x30000, 0x3fffd],
];
const ZERO_WIDTH = /^[\p{Mn}\p{Me}\p{Cf}]$/u;
const EMOJI = /^\p{Emoji_Presentation}$/u;
const PICTOGRAPHIC = /^\p{Extended_Pictographic}$/u;
// A character's class: its width (the low 2 bits) and what it joins.
const JOINS = 4, PICT = 8, RI = 16, ZWJ = 32, MODIFIER = 64, VS16 = 128;
const classes = new Map();
function charClass(cp) {
  let c = classes.get(cp);
  if (c !== undefined) return c;
  const ch = String.fromCodePoint(cp);
  if (cp >= 0x1f3fb && cp <= 0x1f3ff) c = 2 | MODIFIER;
  else if (cp >= 0x1f1e6 && cp <= 0x1f1ff) c = 2 | RI;
  else if (ZERO_WIDTH.test(ch) || (cp >= 0x1160 && cp <= 0x11ff) || (cp >= 0xd7b0 && cp <= 0xd7ff)) c = JOINS | (cp === 0x200d ? ZWJ : 0) | (cp === 0xfe0f ? VS16 : 0);
  else c = (EMOJI.test(ch) || WIDE.some(([a, b]) => cp >= a && cp <= b) ? 2 : 1) | (PICTOGRAPHIC.test(ch) ? PICT : 0);
  classes.set(cp, c);
  return c;
}
// What follows a character: an emoji (a ZWJ, skin tone or VS16 joins it), a
// ZWJ after an emoji (the next emoji joins it), or one regional indicator
// (the next one makes a flag).
const AFTER_EMOJI = 1, AFTER_ZWJ = 2, AFTER_RI = 3;
const props = (state, width, join) => (state << 3) | (width << 1) | (join ? 1 : 0);
const propsWidth = (p) => (p >> 1) & 3;
/** xterm's charProperties: `cp`'s width, and whether it joins the cell before. */
function charProperties(cp, preceding) {
  if (cp < 0x300) return props(0, cp < 32 || (cp >= 0x7f && cp < 0xa0) ? 0 : 1, false);
  const c = charClass(cp);
  const state = preceding >>> 3;
  const before = propsWidth(preceding);
  if (before > 0) {
    if (c & RI && state === AFTER_RI) return props(0, 2, true);
    if (c & PICT && state === AFTER_ZWJ) return props(AFTER_EMOJI, Math.max(before, c & 3), true);
    if (c & MODIFIER && state === AFTER_EMOJI) return props(AFTER_EMOJI, 2, true);
    if (c & JOINS) {
      const next = state === AFTER_EMOJI ? (c & ZWJ ? AFTER_ZWJ : AFTER_EMOJI) : 0;
      return props(next, c & VS16 ? 2 : before, true);
    }
  }
  // A mark with nothing to join takes a cell of its own, as xterm draws it.
  return props(c & RI ? AFTER_RI : c & (PICT | MODIFIER) ? AFTER_EMOJI : 0, c & 3 || 1, false);
}
const wcwidth = (cp) => (cp < 0x300 ? propsWidth(charProperties(cp, 0)) : charClass(cp) & 3);
term.unicode.register({ version: "ursa", wcwidth, charProperties });
term.unicode.activeVersion = "ursa";

/**
 * Where `s` leaves the cursor, written from column `x` of row `y` as xterm
 * writes it: a character that doesn't fit wraps to the next row (a wide one
 * leaves the last cell empty), and `x` may be `cols`, waiting to wrap.
 */
function place(s, { x, y, p }, cols) {
  for (const ch of s) {
    const f = charProperties(ch.codePointAt(0), p);
    const width = propsWidth(f);
    const joined = f & 1 ? propsWidth(p) : 0;
    p = f;
    if (x + width - joined > cols) {
      x = joined;
      y++;
    }
    x += x && f & 1 ? width - joined : width;
  }
  return { x, y, p };
}
const cellWidth = (s) => place(s, { x: 0, y: 0, p: 0 }, Infinity).x;

// Characters as a person sees them (an emoji with its skin tone, a flag, a
// letter and its accent): the cursor moves over them, and deletes them, whole.
const segmenter = typeof Intl.Segmenter === "function" ? new Intl.Segmenter(undefined, { granularity: "grapheme" }) : null;
const graphemes = (s) => (segmenter ? Array.from(segmenter.segment(s), (g) => g.segment) : Array.from(s));
/** The start of the character before index `i` of `s`. */
function before(s, i) {
  if (i <= 0) return 0;
  if (segmenter) return segmenter.segment(s).containing(i - 1).index;
  return i - (i >= 2 && /[\udc00-\udfff]/.test(s[i - 1]) && /[\ud800-\udbff]/.test(s[i - 2]) ? 2 : 1);
}
/** The end of the character at index `i` of `s`. */
function after(s, i) {
  if (i >= s.length) return s.length;
  if (segmenter) {
    const g = segmenter.segment(s).containing(i);
    return g.index + g.segment.length;
  }
  return i + (s.codePointAt(i) > 0xffff ? 2 : 1);
}

term.open($("terminal"));
fit.fit();

/** Tables fit the terminal, a column kept free. */
function sendWidth() {
  engine("width", { columns: Math.max(20, term.cols - 1) });
}
new ResizeObserver(() => {
  const size = `${term.cols}x${term.rows}`;
  fit.fit();
  sendWidth();
  // The terminal rewraps what is on screen: draw what is typed afresh.
  if (!busy && `${term.cols}x${term.rows}` !== size) redrawInput();
}).observe($("terminal"));

term.attachCustomKeyEventHandler((e) => {
  // Tab and Shift+Tab move the focus on, as anywhere on a page (there is no
  // completion for them to do), so the keyboard can leave the terminal.
  if (e.key === "Tab" && !e.ctrlKey && !e.altKey && !e.metaKey) return false;
  // Esc clears the Graph view's selection, or else closes the view, the
  // terminal keeping the focus.
  if (e.key === "Escape" && graphOpen()) {
    if (e.type === "keydown" && !graphLoaded?.clearSelection()) closeGraph();
    return false;
  }
  // Shift+Enter starts a new line in what is typed. xterm would send Enter for
  // it (on keydown, and again on keypress), so it never reaches xterm.
  if (e.key === "Enter" && e.shiftKey && !e.isComposing) {
    e.preventDefault();
    if (e.type === "keydown") onKey("\n");
    return false;
  }
  if (e.type !== "keydown") return true;
  // Ctrl+C (Cmd+C) with text selected copies it, as in a terminal app.
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "c" && term.hasSelection()) {
    navigator.clipboard?.writeText(term.getSelection());
    return false;
  }
  return true;
});

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
  highlight().then((done) => done && !busy && draw());
  return text.split("\n");
}

// Each request to the highlighter is numbered: a reply to an older one, for
// text since changed, is dropped.
let highlightSeq = 0;
let highlighting = null;
/** Ask the highlighter for `text`, once; resolves true if `colored` is now current. */
function highlight() {
  if (colored.text === text) return Promise.resolve(true);
  if (highlighting?.text === text) return highlighting.done;
  const asked = text;
  const seq = ++highlightSeq;
  const done = engine("highlight", { text: asked }).then(
    ({ text: ansi }) => {
      if (seq !== highlightSeq || asked !== text) return false;
      colored = { text: asked, ansi };
      return true;
    },
    () => false,
  );
  highlighting = { text: asked, done };
  return done;
}

/** What was typed ran: the shell's state changed, and with it the colours. */
function forgetColors() {
  colored = { text: "", ansi: "" };
  highlighting = null;
  highlightSeq++;
}

/**
 * Where each line of what is typed goes on screen, after its prompt: the row
 * its last character is on (from the row the line starts on), whether it
 * fills that row exactly (the terminal waits to wrap), and the cursor's row
 * and column if `at` (an index in the line) is where it is.
 */
function layout(lead, line, at, cols) {
  let pos = place(lead, { x: 0, y: 0, p: 0 }, cols);
  let target = null;
  let i = 0;
  for (const g of graphemes(line)) {
    if (i === at) {
      // Where the character's first cell goes: on the next row if it doesn't fit.
      const next = place(String.fromCodePoint(g.codePointAt(0)), pos, cols);
      target = next.y > pos.y ? { row: next.y, col: 0 } : { row: pos.y, col: pos.x };
    }
    pos = place(g, pos, cols);
    i += g.length;
  }
  if (at === line.length) target = pos.x >= cols ? { row: pos.y + 1, col: 0 } : { row: pos.y, col: pos.x };
  return { last: pos.y, full: pos.x >= cols, target };
}

/**
 * Draw the prompt and what is typed, and put the cursor where it is. `final`
 * draws it for the scrollback: the cursor is left after the last character.
 */
function draw(final = false) {
  const cols = term.cols;
  const lines = text.split("\n");
  const colors = coloredLines();
  const pad = " ".repeat(cellWidth(prompt));
  let out = "\r" + (cursorRow > 0 ? `\x1b[${cursorRow}A` : "") + "\x1b[J";
  let row = 0; // the row this line starts on
  let end = 0; // the row the cursor is on once all is written
  let offset = 0;
  let target = { row: 0, col: 0 };
  lines.forEach((line, i) => {
    if (i > 0) out += "\r\n";
    out += (i === 0 ? `${promptColor[0]}${prompt}${promptColor[1]}` : pad) + (colors[i] ?? line);
    const at = cursor >= offset && cursor <= offset + line.length ? cursor - offset : -1;
    const placed = layout(i === 0 ? prompt : pad, line, at, cols);
    if (placed.target) target = { row: row + placed.target.row, col: placed.target.col };
    end = row + placed.last;
    // The last line, if it fills its last row exactly, leaves the cursor
    // waiting to wrap: wrap it now, so the cursor can go to the row below.
    // A line before it needs nothing: its newline goes to the next row.
    if (placed.full && i === lines.length - 1 && !final) {
      out += " \r";
      end++;
    }
    row = end + 1;
    offset += line.length + 1;
  });
  if (final) {
    cursorRow = 0;
    term.write(out);
    return;
  }
  const up = end - target.row;
  out += (up > 0 ? `\x1b[${up}A` : "") + "\r" + (target.col > 0 ? `\x1b[${target.col}C` : "");
  cursorRow = target.row;
  const rows = cursorRow;
  term.write(out, () => {
    // Where the input starts, kept by the terminal as it rewraps: see redrawInput.
    inputStart?.dispose();
    inputStart = term.registerMarker(-rows);
  });
}

// The row the drawn input starts on, which the terminal moves as it rewraps.
let inputStart = null;

/** Draw what is typed again, after the terminal changed size and rewrapped. */
function redrawInput() {
  term.write("", () => {
    if (busy || !inputStart || inputStart.isDisposed || inputStart.line < 0) return;
    const top = Math.max(0, inputStart.line - term.buffer.active.baseY);
    term.write(`\x1b[${top + 1};1H`);
    cursorRow = 0;
    draw();
  });
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

/** Leave the input on screen as typed, coloured, and go to the next line for output. */
async function finishLine() {
  await highlight();
  cursor = text.length;
  draw(true);
  await write("\r\n");
}

const lineStart = () => text.lastIndexOf("\n", cursor - 1) + 1;
const lineEnd = () => (text.indexOf("\n", cursor) < 0 ? text.length : text.indexOf("\n", cursor));
/** The cursor's column in its line, in cells. */
const column = () => cellWidth(text.slice(lineStart(), cursor));
/** The index in the line starting at `start` that is nearest `col` cells in, not past it. */
function atColumn(start, col) {
  const end = text.indexOf("\n", start) < 0 ? text.length : text.indexOf("\n", start);
  let i = start;
  for (const g of graphemes(text.slice(start, end))) {
    if (cellWidth(text.slice(start, i + g.length)) > col) break;
    i += g.length;
  }
  return i;
}

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
        const start = before(text, cursor);
        text = text.slice(0, start) + text.slice(cursor);
        cursor = start;
        draw();
      }
      return;
    case "\x1b[3~": // Delete
      if (cursor < text.length) {
        text = text.slice(0, cursor) + text.slice(after(text, cursor));
        draw();
      }
      return;
    case "\x1b[D": // Left
    case "\x02":
      cursor = before(text, cursor);
      draw();
      return;
    case "\x1b[C": // Right
    case "\x06":
      cursor = after(text, cursor);
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
        cursor = atColumn(text.lastIndexOf("\n", lineStart() - 2) + 1, column());
        draw();
      }
      return;
    case "\x1b[B": // Down: the line below, or the history
      if (lineEnd() === text.length) {
        if (historyAt >= history.length) return;
        historyAt++;
        setText(historyAt === history.length ? draft : history[historyAt]);
      } else {
        cursor = atColumn(lineEnd() + 1, column());
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
    forgetColors();
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
    forgetColors();
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
function runAll(commands, { session = generation, replay = false, fit = false } = {}) {
  let ok = true;
  const jobs = commands.map((command) =>
    enqueue(async () => {
      if (!ok || session !== generation) return (ok = false);
      setAside();
      // A statement left half typed would take this command as its next line.
      if (statement.length) await cancelPending();
      const lines = fit ? fitLines(command, term.cols - prompt.length - 1) : command.split("\n");
      replaying = replay;
      try {
        for (const line of lines) ok = (await feed(line, { echo: true })) && ok;
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

/**
 * The lines of `sql`, those wider than `width` broken at spaces, so that a
 * narrow terminal doesn't wrap them mid-word. A space in a quoted string or
 * after `--` is no place to break; a broken comment carries on as a comment.
 * A break goes on the next line, indented two more than its own; a line
 * indented too far to break loses its indent first.
 */
function fitLines(sql, width) {
  const out = [];
  for (const line of sql.split("\n")) {
    // A dot-command is one line: broken, its rest would run as a command of
    // its own (.graph karate SIZE BY pagerank / COLOR BY club).
    if (line.trimStart().startsWith(".")) {
      out.push(line);
      continue;
    }
    const indent = line.match(/^\s*/)[0];
    const comment = line.trimStart().startsWith("--");
    let pad = indent + (comment ? "-- " : "  ");
    let rest = line;
    let from = indent.length; // breaks come after this, so each line holds something
    while (rest.length > width) {
      let at = -1;
      let quote = "";
      for (let i = 0; i <= width && i < rest.length; i++) {
        const c = rest[i];
        if (quote) {
          if (c === quote) quote = "";
        } else if (!comment && (c === "'" || c === '"')) {
          quote = c;
        } else if (!comment && c === "-" && rest[i + 1] === "-") {
          break;
        } else if (c === " " && i > from && rest[i - 1] !== " ") {
          at = i;
        }
      }
      if (at < 0) {
        if (rest.length - rest.trimStart().length <= 2) break;
        rest = "  " + rest.trimStart();
        pad = comment ? "  -- " : "    ";
        from = 2;
        continue;
      }
      out.push(rest.slice(0, at));
      rest = pad + rest.slice(at).trimStart();
      from = pad.length;
    }
    out.push(rest);
  }
  return out;
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
    // shared link cannot rewrite the screen as it is echoed. The shell's
    // highlighter leaves comments plain: the page greys them.
    const shown = visible(line);
    const { text: ansi } = /^\s*--/.test(line) ? { text: `\x1b[90m${shown}\x1b[0m` } : await engine("highlight", { text: shown });
    await write(`${promptColor[0]}${prompt}${promptColor[1]}${ansi}\r\n`);
  }
  statement.push(line);
  const r = await engine("feed", { line });
  let ok = !r.err;
  if (r.flow === "host") {
    statement = [];
    ok = (await hostCommand(r.command)) !== false;
  } else {
    // A plain .help starts with the tour and .graph, before the list.
    if (!r.err && /^\s*\.help\s*;?\s*$/.test(line)) await write(HELP_FIRST.flatMap((l) => wrapWords(l, term.cols - 1)).join("\r\n") + "\r\n\r\n");
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
  [".graph", "?QUERY|NAME? ?AS …? ?SIZE BY x? ?COLOR BY y?", "Draw the last result as a graph, by what it is (edges by src and dst or two id columns, nodes by id with the graph's edges among them, a hop's rings, a pattern's matches, or paths), or a query's, or a property graph's, its nodes sized and coloured by a column or an algorithm (.graph karate SIZE BY pagerank COLOR BY louvain); AS edges(x, y) says what the columns are, and .help graph lists every clause; the pane says what it was drawn as; click a node to select it (Shift+click adds); / in the pane finds nodes (.find) and f fits the selection in view; Esc clears the selection, then closes it"],
  [".history", "?N?", "Show the last N commands (default 20)"],
  [".share", "", "Copy a link that replays this session's commands"],
  [".tour", "", "Take the tour: five steps on the karate club, from PageRank to a drawing"],
];

// What `.help` says first: where to start, and how to draw a graph.
const HELP_FIRST = [
  "New here? .tour runs a five-step tour, and .dataset loads a sample graph.",
  "Draw a graph: .graph karate draws a property graph, and .graph alone the last result.",
];

const SHORTCUTS = `Enter            Run the line (a statement runs once it ends with ;)
Shift+Enter      Start a new line in what is typed
Up, Down         Go to the line above or below; past the first or last, step
                 through the history (kept in this browser)
Left, Right      Move a character (also Ctrl+B, Ctrl+F)
Home, End        Go to the start or end of the line (also Ctrl+A, Ctrl+E)
Backspace        Delete the character before the cursor (also Ctrl+H)
Delete           Delete the character at the cursor
Ctrl+U, Ctrl+K   Delete to the start or end of the line
Ctrl+W           Delete the word before the cursor
Tab, Shift+Tab   Move the focus out of the terminal, to the page's buttons
Ctrl+C           Copy the selection (also Cmd+C); with none, drop what is typed
Ctrl+L           Clear the screen`;

const errorLine = (message) => write(`\x1b[31mError: ${message}\x1b[0m\r\n`).then(() => false);

/** Run a command the page added. False if it failed. */
async function hostCommand(line) {
  const [name, ...rest] = line.trim().replace(/;\s*$/, "").split(/\s+/);
  const arg = rest.join(" ");
  switch (name) {
    case ".graph":
      if (/^history\b/i.test(arg)) return explore(line);
      // The query as typed: its spaces may be in a string.
      return drawGraph(line.trim().slice(name.length).trim());
    case ".find":
      return find(line);
    case ".expand":
    case ".hide":
    case ".show":
    case ".undo":
    case ".redo":
      return explore(line);
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
    case ".tour":
      if (arg) return errorLine("usage: .tour");
      loadDataset(DATASETS.find((d) => d.id === "tour"));
      return true;
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

// ---------------------------------------------------------------- the Graph view

// The pane beside the terminal (under it on a phone), and graph.js, which
// draws in it: imported on the first `.graph`, so a session that draws no
// graph downloads none of it (it loads ursa-viz, a few MB, in turn).
const graphPane = $("graph-pane");
let graphModule = null;
// graph.js once loaded, for the test hooks.
let graphLoaded = null;
const graphOpen = () => !graphPane.hidden;
const loadGraphModule = () =>
  (graphModule ??= import("./graph.js").then(
    (m) => (graphLoaded = m),
    (e) => {
      graphModule = null;
      throw e;
    },
  ));

// When each step of the last `.graph` ended (performance.now()), for tests
// and tests/graph-timing.ts: enter, engine, viz, posted, view, the worker's
// workerStart, workerLoaded and hierarchy, firstPositions, firstDraw, done;
// and `command`, what was typed. graphTimings() (below) gives them.
let graphTimes = null;

/**
 * `.graph ?SOURCE? ?NODES (query)? ?SIZE BY x? ?COLOR BY y?`: draw the last
 * result, a query's result, or a property graph (WebShell.graph says which
 * rows are edges, and what sizes and colours the nodes), replacing what the
 * pane drew. The terminal keeps the focus: queries can still be typed.
 *
 * The pane opens with "Loading graph view…" as soon as the engine takes more
 * than a moment (at once, for a big graph; not at all, for a typo), and the
 * graph view's code starts loading then, while the engine still works.
 */
async function drawGraph(text) {
  const times = (graphTimes = { enter: performance.now(), command: `.graph ${text}`.trim() });
  const wasOpen = graphOpen();
  const status = $("graph-status");
  const before = status.textContent;
  let opened = false;
  const open = () => {
    if (opened) return;
    opened = true;
    graphPane.hidden = false;
    status.textContent = "Loading graph view…";
    loadGraphModule().then((m) => m.preload(), () => {});
  };
  const soon = setTimeout(open, 150);
  let graph;
  try {
    graph = await engine("graph", { text });
    times.engine = performance.now();
  } catch (e) {
    clearTimeout(soon);
    // The pane as it was: closed, or with its drawing.
    if (opened && !wasOpen) closeGraph();
    else if (opened) status.textContent = before;
    if (e.crash) throw e;
    return errorLine(e.message);
  }
  clearTimeout(soon);
  open();
  let drawn;
  try {
    const module = await loadGraphModule();
    // A new drawing: the last search's matches were of the old one.
    search.clear();
    drawn = await module.show($("graph-stage"), graph, { status, legend: $("graph-legend"), onError: graphFailed, times });
  } catch (e) {
    // Closed (Esc) before it was drawn: nothing went wrong.
    if (!graphOpen()) return true;
    closeGraph();
    return errorLine(`the graph view could not start: ${String(e?.message ?? e)}`);
  }
  const count = (n, one) => `${n.toLocaleString("en-US")} ${one}${n === 1 ? "" : "s"}`;
  const skipped = graph.skipped ? `, skipping ${count(graph.skipped, "row")} with a NULL endpoint` : "";
  const as = drawn.drawnAs ? ` as ${drawn.drawnAs}` : "";
  await write(`Drawing ${count(drawn.edges, "edge")}, ${count(drawn.nodes, "node")}${as}${skipped} (${wasOpen ? "replacing the last drawing; " : ""}Esc closes)\r\n`);
  return true;
}

// The last explore command's answer, for tests: its `command`, how many
// `nodes` and `edges` the engine shows after it, and `deltaBytes`, the size
// of the change it sent (0 for none).
let explored = null;

/**
 * An explore command (.expand, .hide, .show, .undo, .redo, .graph history):
 * the engine changes its graph and prints what changed, and gives the change
 * as a UGR delta. The shell itself (ursa-shell) lists these commands in .help.
 * Drawing the delta in the pane is #443: until then the pane keeps what
 * .graph drew.
 */
async function explore(line) {
  const r = await engine("explore", { command: line });
  explored = { command: line, nodes: r.nodes, edges: r.edges, deltaBytes: r.delta?.byteLength ?? 0 };
  await write(r.out + r.err);
  return !r.err;
}

// The last .find's answer, for tests: its `command`, the `ms` the engine
// took to answer (the worker's round trip), how many nodes `matched` in the
// view and how many of them were `selected`, and the matches `notShown`
// ([{key, hidden}], the first of `notShownTotal`).
let found = null;

// The pane's search (graph-search.js): `/` opens its box, and Enter there
// types .find into the shell.
const search = setupSearch({
  form: $("graph-search"),
  input: $("graph-search-input"),
  button: $("graph-find"),
  list: $("graph-found"),
  status: $("graph-status"),
  run: (command) => run(command),
  focusView: () => (graphPane.querySelector("canvas") ?? term).focus(),
});

/**
 * `.find TEXT` or `.find WHERE condition`: the engine says which nodes of the
 * graph drawn match (ursa-engine's graph::view::find), the view selects them
 * and fits them (graph.js's selectFound), and the pane lists the matches
 * that are not shown. A search that matches nothing in the view leaves the
 * selection as it was.
 */
async function find(line) {
  const start = performance.now();
  const r = await engine("find", { command: line });
  found = { command: line, ms: performance.now() - start, matched: r.matches, selected: r.selected.length, notShown: r.notShown, notShownTotal: r.notShownTotal };
  await write(r.out + r.err);
  if (r.err) return false;
  if (r.selected.length) graphLoaded?.selectFound(r.selected, r.matches);
  search.show(r.notShown, r.notShownTotal);
  return true;
}

/** Close the pane: the terminal takes the whole width again, and the focus. */
function closeGraph() {
  if (graphPane.hidden) return;
  search.close(false);
  search.clear();
  // The engine's graph closes with the pane's: .expand then says none is open.
  engine("closeGraph").catch(() => {});
  const hadFocus = graphPane.contains(document.activeElement);
  graphPane.hidden = true;
  graphModule?.then((m) => m.close(), () => {});
  $("graph-stage").replaceChildren();
  $("graph-status").textContent = "";
  $("graph-legend").replaceChildren();
  $("graph-legend").hidden = true;
  if (hadFocus) term.focus();
}

/** The view failed while drawing (a lost GPU, a layout that stopped): say so. */
function graphFailed(message) {
  closeGraph();
  enqueue(async () => {
    setAside();
    await errorLine(message);
  });
}

$("graph-close").onclick = () => {
  closeGraph();
  term.focus();
};
graphPane.addEventListener("keydown", (e) => {
  // `/` searches, from anywhere in the pane.
  if (e.key === "/" && !e.ctrlKey && !e.metaKey && !e.altKey) {
    e.preventDefault();
    search.open();
    return;
  }
  if (e.key !== "Escape") return;
  e.preventDefault();
  // A selection is cleared first; Esc again closes the pane.
  if (graphLoaded?.clearSelection()) return;
  closeGraph();
  term.focus();
});
$("graph").onclick = (e) => {
  run(".graph");
  if (byPointer(e)) term.focus();
};

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
  // Where to start: the sample graphs. Broken between words when the
  // terminal is too narrow (a phone) to hold it on one line.
  const hint = wrapWords("Pick a sample graph from the Datasets menu, or type .dataset karate", term.cols - 1);
  await write(`Ursa Web Shell running...\r\nUrsa v${r.version}\r\nEnter ".help" for usage hints.\r\n${hint.join("\r\n")}\r\n`);
}

/** `text`'s words in lines no wider than `width` (a longer word gets a line of its own). */
function wrapWords(text, width) {
  const lines = [];
  for (const word of text.split(" ")) {
    const last = lines.length - 1;
    if (last >= 0 && lines[last].length + 1 + word.length <= width) lines[last] += ` ${word}`;
    else lines.push(word);
  }
  return lines;
}

/** Start a fresh session; what was queued for the old one is skipped. */
function fresh() {
  const session = ++generation;
  // A question waiting (a shared link's) is answered no.
  answer?.("");
  // What the pane drew belongs to the old session.
  closeGraph();
  enqueue(async () => {
    if (session === generation) await restart();
  });
  return session;
}

$("new").onclick = (e) => {
  fresh();
  if (byPointer(e)) term.focus();
};

function loadDataset(d) {
  const session = fresh();
  return runAll(d.commands, { session, fit: true });
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
$("share").onclick = (e) => {
  run(".share");
  if (byPointer(e)) term.focus();
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
$("import").onclick = (e) => {
  fileInput.click();
  if (byPointer(e)) term.focus();
};
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

// The Datasets menu, a menu button: arrow keys, Home and End move between
// its items, Enter or Space picks one, Escape (or Tab) closes it.
const menu = $("dataset-menu");
const menuButton = $("datasets");
menu.replaceChildren(
  ...DATASETS.map((d) =>
    el("button", {
      type: "button",
      role: "menuitem",
      tabIndex: -1,
      textContent: d.title,
      onclick: (e) => {
        closeMenu();
        loadDataset(d);
        (byPointer(e) ? term : menuButton).focus();
      },
    }),
  ),
);
const menuItems = () => [...menu.querySelectorAll('[role="menuitem"]')];
function openMenu(at = 0) {
  menu.hidden = false;
  menuButton.setAttribute("aria-expanded", "true");
  menuItems().at(at)?.focus();
}
function closeMenu() {
  menu.hidden = true;
  menuButton.setAttribute("aria-expanded", "false");
}
menuButton.onclick = (e) => {
  e.stopPropagation();
  if (menu.hidden) openMenu();
  else closeMenu();
};
menuButton.addEventListener("keydown", (e) => {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  e.preventDefault();
  openMenu(e.key === "ArrowDown" ? 0 : -1);
});
menu.addEventListener("keydown", (e) => {
  const items = menuItems();
  const at = items.indexOf(document.activeElement);
  const to = { ArrowDown: at + 1, ArrowUp: at - 1, Home: 0, End: items.length - 1 }[e.key];
  if (to !== undefined) {
    e.preventDefault();
    items[(to + items.length) % items.length].focus();
  } else if (e.key === "Tab") {
    // Focus moves on from the menu button; the menu closes behind it.
    menuButton.focus();
    closeMenu();
  }
});
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
  // The cursor, as a line of the screen (as `screen` numbers them) and a column.
  cursor: () => ({ y: term.buffer.active.baseY + term.buffer.active.cursorY, x: term.buffer.active.cursorX }),
  // A line's cells: the character, its colour (-1 the default) and if bold.
  cells(y) {
    const line = term.buffer.active.getLine(y);
    const cells = [];
    for (let x = 0; line && x < line.length; x++) {
      const cell = line.getCell(x);
      if (cell.getWidth() === 0) continue;
      cells.push({ ch: cell.getChars(), fg: cell.isFgDefault() ? -1 : cell.getFgColor(), bold: !!cell.isBold() });
    }
    return cells;
  },
  // Keys, as if typed, all at once: before any reply from the engine.
  input: (keys) => keys.forEach((k) => term.input(k)),
  cols: () => term.cols,
  rows: () => term.rows,
  selection: () => term.getSelection(),
  datasets: DATASETS,
  // The Graph view: whether it is open, and (once graph.js is loaded) what it
  // draws, a frame's inked pixels, and the node at a canvas pixel.
  graphOpen,
  graph: () => graphLoaded?.info() ?? null,
  // The last explore command (.expand, .hide, …): {command, nodes, edges,
  // deltaBytes}, the engine's counts of what is shown after it.
  explored: () => explored,
  // The last .find: {command, ms, matched, selected, notShown, notShownTotal}.
  found: () => found,
  // The last .graph: when each step ended, in ms after Enter; and what the
  // view costs to use (#430, graph-timings.js): `frames` (how many, and the
  // p50, p95 and max of the time in render() and of the time between frames
  // while the view moves), `pick` (the last and the p95), `actions` (each
  // one's command, its ms in the engine and applying the answer, and its ms
  // from the input to the page being free of the frame that showed it) and
  // `longTasks` (the page's tasks over 50 ms: start, in ms after Enter, and
  // duration).
  graphTimings: () =>
    graphTimes && {
      ...Object.fromEntries(Object.entries(graphTimes).filter(([, v]) => typeof v === "number").map(([k, v]) => [k, Math.round(v - graphTimes.enter)])),
      ...graphLoaded?.readTimings(graphTimes.enter),
    },
  // For tests: forget the frames and picks timed so far.
  graphTimingsReset: () => graphLoaded?.resetTimings(),
  // Memory: each wasm instance's (`wasm.engine`, `wasm.view`, `wasm.layout`,
  // in bytes) and, cross-origin isolated, what the browser measures for the
  // page and its workers (`userAgent`; see graph.js's memory()).
  graphMemory: async (options) => {
    const [{ bytes }, rest] = await Promise.all([engine("memory"), graphLoaded?.memory(options)]);
    return { crossOriginIsolated, userAgent: null, ...rest, wasm: { engine: bytes, view: null, layout: null, ...rest?.wasm } };
  },
  graphInked: async () => graphLoaded?.inked() ?? null,
  graphNodeAt: (x, y) => graphLoaded?.nodeAt(x, y) ?? null,
  // For tests: where the node with a key is drawn, {index, x, y} in CSS pixels of the canvas.
  graphNodePlace: (id) => graphLoaded?.nodePlace(id) ?? null,
};
