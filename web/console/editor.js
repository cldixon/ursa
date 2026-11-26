// The console's SQL editor (URSA-13, design/console.md §6): CodeMirror 6 in
// the Editor panel, coloured and completed by the session itself, so it
// colours SQL exactly as the shell's terminal does and completes the tables,
// columns, graphs and functions the session has.
//
//   ursaConsole.editor.insert("SELECT * FROM 'karate_edges.parquet'", { statement: true, run: true });
//   ursaConsole.editor.text;                  // the editor's text
//   ursaConsole.editor.setText("SELECT 1");   // replaces it (undoably)
//   ursaConsole.editor.runCurrent();          // as Ctrl+Enter
//   ursaConsole.editor.runAll();              // as Shift+Ctrl+Enter
//   ursaConsole.editor.focus();
//
// - Colour: `highlight {sql}` (debounced) gives the shell's spans, applied as
//   decorations by kind (.cm-sql-keyword, …), in the terminal's palette
//   (editor.css). Until a reply comes the last spans are kept, mapped through
//   the edits; after a reload the spans saved with the text paint at once.
// - Completion: `complete {sql, cursor}`, with each item's kind and detail.
// - Running: Ctrl/Cmd+Enter runs the statement at the cursor, or the
//   selection; Shift+Ctrl/Cmd+Enter runs every statement, in order, stopping
//   at the first error. SQL goes to `ursaConsole.run(sql)` when the Results
//   panel (URSA-14) provides it, else to the session's `query`; dot-commands
//   (`.tables`, `.graph …`) go to `feed`, as the shell's do. The statement
//   running is marked; the status bar's Cancel stops it. An error is shown at
//   its statement: a squiggle under what it names, and the message below.
// - The text (and the cursor) are kept in localStorage, per server port.
// - The keyboard: Tab indents (and accepts a completion); Esc then Tab, or
//   Ctrl+M, leaves the editor, as CodeMirror's guidance has it.
import {
  EditorState,
  EditorSelection,
  StateEffect,
  StateField,
  Prec,
  EditorView,
  Decoration,
  ViewPlugin,
  WidgetType,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  drawSelection,
  dropCursor,
  rectangularSelection,
  crosshairCursor,
  placeholder,
  defaultKeymap,
  history,
  historyKeymap,
  indentWithTab,
  autocompletion,
  completionKeymap,
  closeBrackets,
  closeBracketsKeymap,
  acceptCompletion,
  completionStatus,
  setDiagnostics,
  search,
  searchKeymap,
  highlightSelectionMatches,
} from "/console/vendor/codemirror.mjs";
import { session } from "./session.js";
import { statements, statementAt } from "./statements.js";

const mount = document.querySelector('[data-mount="editor"]');

// ---------------------------------------------------------------- storage

const PORT = location.port || (location.protocol === "https:" ? "443" : "80");
/** Where the text is kept: per server port, so two `ursa serve`s keep their own. */
const STORE = `ursa-console-editor:${PORT}`;
const WELCOME = `-- Ctrl+Enter runs the statement at the cursor; Shift+Ctrl+Enter runs them all.
SELECT * FROM 'karate_edges.parquet' LIMIT 10;
`;

function load() {
  try {
    const saved = JSON.parse(localStorage.getItem(STORE) ?? "null");
    if (saved && typeof saved.text === "string") return saved;
  } catch {}
  return null;
}
let saveTimer = null;
function save(view) {
  clearTimeout(saveTimer);
  saveTimer = null;
  const { state } = view;
  const main = state.selection.main;
  const spans = highlighted.text === state.doc.toString() ? highlighted.spans : null;
  try {
    localStorage.setItem(STORE, JSON.stringify({ text: state.doc.toString(), anchor: main.anchor, head: main.head, spans }));
  } catch {}
}
function saveSoon(view) {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => save(view), 250);
}

// ---------------------------------------------------------------- colour

/** The kinds `highlight` answers, and their classes (editor.css colours them). */
const KINDS = new Set(["keyword", "string", "number", "comment", "command", "arguments"]);
const marks = Object.fromEntries([...KINDS].map((k) => [k, Decoration.mark({ class: `cm-sql-${k}`, attributes: { "data-kind": k } })]));

/** The spans the session last answered, for the text it was asked about. */
let highlighted = { text: null, spans: [] };

function decorate(spans, length) {
  const ranges = [];
  for (const { from, to, kind } of spans) {
    if (!KINDS.has(kind) || !(from < to) || to > length) continue;
    ranges.push(marks[kind].range(from, to));
  }
  return Decoration.set(ranges, true);
}

const setColours = StateEffect.define();
/** The colour decorations: kept through edits (mapped) until the session answers again. */
const colours = StateField.define({
  create: () => Decoration.none,
  update(value, tr) {
    value = value.map(tr.changes);
    for (const e of tr.effects) if (e.is(setColours)) value = e.value;
    return value;
  },
  provide: (f) => EditorView.decorations.from(f),
});

/** How long the editor waits after a keystroke before asking for colour, in ms. */
const HIGHLIGHT_DELAY = 40;

/** Asks the session for colour after each pause in typing. */
const colourer = ViewPlugin.fromClass(
  class {
    constructor(view) {
      this.view = view;
      this.timer = null;
      /** The request in flight: the text asked about, and the edits since. */
      this.asking = null;
      this.again = false;
      this.offConnect = session.on("connect", () => this.schedule(0));
      this.schedule(0);
    }
    update(update) {
      if (!update.docChanged) return;
      if (this.asking) this.asking.changes = this.asking.changes.compose(update.changes);
      this.schedule(HIGHLIGHT_DELAY);
    }
    schedule(delay) {
      clearTimeout(this.timer);
      this.timer = setTimeout(() => this.ask(), delay);
    }
    async ask() {
      this.timer = null;
      if (this.asking) {
        // One at a time: ask again when this one is answered.
        this.again = true;
        return;
      }
      const client = session.client;
      if (!client) return;
      const sql = this.view.state.doc.toString();
      const asking = (this.asking = { sql, changes: this.view.state.changes() });
      let spans = null;
      try {
        spans = (await client.call("highlight", { sql })).spans;
      } catch {}
      this.asking = null;
      if (spans && !this.destroyed) {
        highlighted = { text: sql, spans };
        // Laid on the text it was asked about, then moved through what has
        // been typed since.
        const set = decorate(spans, sql.length).map(asking.changes);
        this.view.dispatch({ effects: setColours.of(set) });
        if (asking.changes.empty) {
          ursa.highlights++;
          this.view.dom.dataset.highlighted = String(ursa.highlights);
          saveSoon(this.view);
        }
      }
      if (this.again || (spans && !asking.changes.empty)) {
        this.again = false;
        this.schedule(spans ? 0 : HIGHLIGHT_DELAY);
      }
    }
    destroy() {
      this.destroyed = true;
      clearTimeout(this.timer);
      this.offConnect();
    }
  },
);

// ---------------------------------------------------------------- completion

/** What each kind of completion is called, shown beside it. */
const KIND_NAMES = {
  table: "table",
  view: "view",
  external: "table of files",
  schema: "schema",
  column: "column",
  graph: "graph",
  function: "function",
  keyword: "keyword",
  command: "dot-command",
};

/** The session's completions at the cursor. */
async function completeFromSession(context) {
  const { state, pos, explicit } = context;
  const before = state.sliceDoc(Math.max(0, pos - 1), pos);
  const word = context.matchBefore(/[\w$.]+$/);
  // Typing: complete once a word (or `t.`, or a dot-command) is begun.
  if (!explicit && !word) return null;
  const client = session.client;
  if (!client) return null;
  let reply;
  try {
    reply = await client.call("complete", { sql: state.doc.toString(), cursor: pos });
  } catch {
    return null;
  }
  if (context.aborted || !reply?.items?.length) return null;
  return {
    from: reply.from,
    to: reply.to,
    options: reply.items.map((item, i) => ({
      label: item.label,
      type: item.kind,
      detail: item.detail || undefined,
      // The session's order is its ranking.
      boost: Math.max(-99, 99 - i),
      ...(item.kind === "function" ? { apply: item.label } : {}),
    })),
    // Narrowed here as the word goes on, without asking again; after a dot,
    // a new word asks again.
    validFor: before === "." ? /^\w*$/ : /^[\w$]*$/,
  };
}

/** How long a Tab waits for CodeMirror to let it take a list just opened, in ms. */
const TAB_ACCEPT_WAIT = 250;

/**
 * Tab with the completion list open: take the selected completion.
 *
 * CodeMirror's acceptCompletion declines for its `interactionDelay` (75 ms)
 * after the list opens or changes, so that Enter pressed as a list appears
 * makes a new line rather than taking it. Tab never makes a new line: with
 * the list shown, it is for the list, as Tab is in the shell. So a Tab in
 * that window is taken here (not passed on to indent the line) and the
 * completion is accepted as soon as CodeMirror allows, unless the text, the
 * cursor or the list has changed by then.
 */
function acceptWithTab(view) {
  if (completionStatus(view.state) !== "active") return false;
  if (acceptCompletion(view)) return true;
  const { doc, selection } = view.state;
  const until = Date.now() + TAB_ACCEPT_WAIT;
  const retry = () => {
    const now = view.state;
    if (now.doc !== doc || !now.selection.eq(selection) || completionStatus(now) !== "active") return;
    if (!acceptCompletion(view) && Date.now() < until) setTimeout(retry, 10);
  };
  setTimeout(retry, 10);
  return true;
}

// ---------------------------------------------------------------- running

const markRunning = StateEffect.define();
const runningMark = Decoration.mark({ class: "cm-sql-running", attributes: { "aria-busy": "true" } });
const runningLine = Decoration.line({ class: "cm-sql-running-line" });
/** The statement running: marked until it ends. */
const running = StateField.define({
  create: () => Decoration.none,
  update(value, tr) {
    value = value.map(tr.changes);
    for (const e of tr.effects) {
      if (!e.is(markRunning)) continue;
      if (!e.value) value = Decoration.none;
      else {
        const { from, to } = e.value;
        const lines = [];
        for (let line = tr.state.doc.lineAt(from); ; line = tr.state.doc.line(line.number + 1)) {
          lines.push(runningLine.range(line.from));
          if (line.to >= to || line.number === tr.state.doc.lines) break;
        }
        value = Decoration.set([runningMark.range(from, to), ...lines], true);
      }
    }
    return value;
  },
  provide: (f) => EditorView.decorations.from(f),
});

// An error, shown where its statement is: a squiggle (the lint package's)
// under what it names, and the message and hint in a block below the
// statement, until the statement is edited or run again.
const setErrors = StateEffect.define();

class ErrorWidget extends WidgetType {
  constructor(error) {
    super();
    this.error = error;
  }
  eq(other) {
    return other.error.message === this.error.message && other.error.hint === this.error.hint;
  }
  toDOM() {
    // No margins on a block widget (CodeMirror measures it without them):
    // the space around the box is the wrapper's padding.
    const wrap = document.createElement("div");
    wrap.className = "cm-sql-error-wrap";
    const box = wrap.appendChild(document.createElement("div"));
    box.className = "cm-sql-error";
    box.setAttribute("role", "alert");
    const head = box.appendChild(document.createElement("div"));
    head.className = "cm-sql-error-message";
    const label = head.appendChild(document.createElement("span"));
    label.className = "cm-sql-error-label";
    label.textContent = this.error.kind === "cancelled" ? "Cancelled" : "Error";
    head.appendChild(document.createTextNode(` ${this.error.message}`));
    if (this.error.hint) {
      const hint = box.appendChild(document.createElement("div"));
      hint.className = "cm-sql-error-hint";
      hint.textContent = this.error.hint;
    }
    return wrap;
  }
  ignoreEvent() {
    return false;
  }
}

/** The errors shown: [{from, to, at, until, message, hint, kind}]. */
const errors = StateField.define({
  create: () => [],
  update(value, tr) {
    for (const e of tr.effects) if (e.is(setErrors)) return e.value;
    if (!tr.docChanged || !value.length) return value;
    // An edit to a statement takes its error away; the others move with it.
    const kept = [];
    for (const error of value) {
      let touched = false;
      tr.changes.iterChangedRanges((fromA, toA) => {
        if (fromA <= error.to && toA >= error.from) touched = true;
      });
      if (touched) continue;
      const map = (p, assoc) => tr.changes.mapPos(p, assoc);
      kept.push({ ...error, from: map(error.from, 1), to: map(error.to, -1), at: map(error.at, 1), until: map(error.until, -1) });
    }
    return kept;
  },
  // Each below the last line of its statement.
  provide: (f) =>
    EditorView.decorations.compute([f], (state) =>
      Decoration.set(
        state.field(f).map((error) => Decoration.widget({ widget: new ErrorWidget(error), block: true, side: 1 }).range(state.doc.lineAt(Math.min(error.to, state.doc.length)).to)),
        true,
      ),
    ),
});

/** Show `list` (replacing what is shown), as widgets and as the lint package's squiggles. */
function showErrors(view, list) {
  view.dispatch(
    { effects: setErrors.of(list) },
    setDiagnostics(
      view.state,
      list
        .filter((e) => e.kind !== "cancelled")
        .map((e) => ({ from: e.at, to: e.until, severity: "error", source: "ursa", message: e.hint ? `${e.message}\n${e.hint}` : e.message })),
    ),
  );
}

/** Keeps the lint package's squiggles in step with the errors when an edit takes one away. */
const errorSync = EditorView.updateListener.of((update) => {
  if (!update.docChanged) return;
  const before = update.startState.field(errors);
  const after = update.state.field(errors);
  if (before.length !== after.length) {
    queueMicrotask(() => showErrors(update.view, update.view.state.field(errors)));
  }
});

/**
 * The error `e` (the protocol's {kind, message}) of the statement `s`, placed:
 * at the line and column a parse error gives, else at the first word the
 * message quotes that the statement has, else across the statement.
 */
function placeError(s, e) {
  let message = String(e.message ?? e);
  // The engine's hint: what follows "; did you mean …", a "HINT:"/"Hint:"
  // line, or a second line.
  let hint = "";
  const m = message.match(/^([\s\S]*?)(?:;\s*|\.\s+|\n\s*)((?:did you mean|hint:|try |use |enter )[\s\S]*)$/i);
  if (m) {
    message = m[1].trim();
    hint = m[2].trim().replace(/^hint:\s*/i, "");
    hint = hint.charAt(0).toUpperCase() + hint.slice(1);
  } else if (message.includes("\n")) {
    const [first, ...rest] = message.split("\n");
    message = first.trim();
    hint = rest.join("\n").trim();
  }
  let at = s.from;
  let until = s.to;
  const pos = message.match(/at Line: (\d+), Column: (\d+)/);
  // The names it gives: quoted, or after "named", "field", "column", "table"…
  const quoted = [
    ...[...message.matchAll(/['"`]([^'"`\s]{1,128})['"`]/g)].map((q) => q[1]),
    ...[...message.matchAll(/(?<=\b(?:named|field|column|table|function|graph)\s+)([A-Za-z_][\w$.]{0,127})/gi)].map((q) => q[1]),
  ];
  if (pos) {
    const lines = s.text.split("\n");
    const line = Number(pos[1]) - 1;
    const column = Number(pos[2]) - 1;
    if (line < lines.length) {
      let offset = 0;
      for (let i = 0; i < line; i++) offset += lines[i].length + 1;
      offset += Math.min(column, lines[line].length);
      const word = s.text.slice(offset).match(/^[\w$]+|^\S/);
      at = s.from + offset;
      until = at + (word ? word[0].length : 1);
    }
  } else {
    for (const name of quoted) {
      const bare = name.split(".").pop();
      const found = new RegExp(`(?<![\\w$])${bare.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?![\\w$])`, "i").exec(s.text);
      if (found) {
        at = s.from + found.index;
        until = at + found[0].length;
        break;
      }
    }
  }
  return { from: s.from, to: s.to, at, until: Math.max(until, at + 1), message, hint, kind: e.kind ?? "sql" };
}

/** The editor's state for the e2e test and other panels: counts of what happened. */
const ursa = { highlights: 0, runs: 0 };

let busy = null;

/**
 * Run the statements `list` ([{from, to, text, command}]) one by one,
 * stopping at the first error; each is marked while it runs. Resolves to
 * whether all ran.
 */
async function runStatements(view, list) {
  if (!list.length) return true;
  if (busy) {
    say("A statement is already running: Cancel it in the status bar first.");
    return false;
  }
  // The statements about to run lose their errors; the others keep theirs.
  showErrors(
    view,
    view.state.field(errors).filter((e) => !list.some((s) => e.from <= s.to && e.to >= s.from)),
  );
  const mapped = list.map((s) => ({ ...s }));
  busy = { list: mapped };
  view.dom.dataset.running = "true";
  let ok = true;
  try {
    for (const s of mapped) {
      // Where the statement is now (the text may have been edited).
      const { from, to } = busy.positions?.get(s) ?? s;
      view.dispatch({ effects: markRunning.of({ from, to }) });
      try {
        await runOne(s);
        ursa.runs++;
      } catch (e) {
        ok = false;
        const now = busy.positions?.get(s) ?? s;
        const error = placeError({ ...s, from: now.from, to: now.to }, e);
        showErrors(view, [...view.state.field(errors), error]);
        say(`${error.kind === "cancelled" ? "Cancelled" : "Error"}: ${error.message}${error.hint ? `. ${error.hint}` : ""}`);
        break;
      }
    }
  } finally {
    busy = null;
    delete view.dom.dataset.running;
    view.dispatch({ effects: markRunning.of(null) });
  }
  return ok;
}

/** Follow the statements running through edits made while they run. */
const runningTracker = EditorView.updateListener.of((update) => {
  if (!busy || !update.docChanged) return;
  busy.positions ??= new Map();
  for (const s of busy.list) {
    const was = busy.positions.get(s) ?? s;
    busy.positions.set(s, { from: update.changes.mapPos(was.from, 1), to: update.changes.mapPos(was.to, -1) });
  }
});

/** A short label for the status bar: the statement's start, on one line. */
function label(text) {
  const line = text.replace(/\s+/g, " ").trim();
  return line.length > 48 ? `${line.slice(0, 47)}…` : line;
}

/**
 * Run one statement: a dot-command through `feed`, SQL through the Results
 * panel's `ursaConsole.run` when it has one, else the session's `query`.
 * Rejects with the protocol's error.
 */
async function runOne(s) {
  const results = window.ursaConsole?.run;
  if (s.command) {
    const reply = await session.run("feed", { line: s.text }, { label: label(s.text) });
    const err = stripAnsi(reply.err ?? "").trim();
    output({ kind: "command", text: s.text, out: reply.out ?? "", err: reply.err ?? "", reply });
    if (err) throw { kind: "sql", message: err.replace(/^Error:\s*/, "") };
    return reply;
  }
  if (typeof results === "function") {
    // results.js's run resolves to {tab}, {notices} or {error}: it shows the
    // error in the Results panel too; the editor shows it at the statement.
    const reply = await results(s.text);
    if (reply?.error) throw reply.error;
    output({ kind: "query", text: s.text, reply, shown: true });
    return reply;
  }
  const reply = await session.run("query", { sql: s.text }, { label: label(s.text) });
  output({ kind: "query", text: s.text, reply });
  return reply;
}

/**
 * What ran, for whoever shows it: an `ursa:output` event on the editor's
 * mount ({kind: "query" | "command", text, reply, out?, err?, shown?}).
 * `shown` says the Results panel (URSA-14's ursaConsole.run) has shown it;
 * else it is logged.
 */
function output(detail) {
  mount.dispatchEvent(new CustomEvent("ursa:output", { detail, bubbles: true }));
  if (detail.shown) return;
  if (detail.kind === "query") console.info(`[editor] ${detail.text}: ${detail.reply.rows} rows in ${Math.round(detail.reply.ms)} ms`, detail.reply);
  else console.info(`[editor] ${detail.text}\n${stripAnsi(detail.out)}${stripAnsi(detail.err)}`);
}

function stripAnsi(text) {
  return String(text).replace(/\x1b\[[0-9;]*[A-Za-z]/g, "");
}

/** The statements at the cursor, or the selection's. */
function currentStatements(state) {
  const text = state.doc.toString();
  const all = statements(text);
  const sel = state.selection.main;
  if (!sel.empty) {
    const picked = statements(text.slice(sel.from, sel.to)).map((s) => ({ ...s, from: s.from + sel.from, to: s.to + sel.from }));
    return picked;
  }
  const one = statementAt(all, sel.head, text);
  return one ? [one] : [];
}

function runCurrent(view) {
  void runStatements(view, currentStatements(view.state));
  return true;
}
function runAll(view) {
  void runStatements(view, statements(view.state.doc.toString()));
  return true;
}

// ---------------------------------------------------------------- the view

const live = document.createElement("div");
live.className = "visually-hidden";
live.setAttribute("role", "status");
/** Say `text` to a screen reader. */
function say(text) {
  live.textContent = text;
}

/** The editor's look, from the console's tokens: it follows the theme with them. */
const look = EditorView.theme({
  "&": { height: "100%", color: "var(--fg)", backgroundColor: "var(--bg)", fontSize: "13px" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: '"IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace', lineHeight: "1.55" },
  ".cm-content": { caretColor: "var(--fg)", padding: "8px 0" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--fg)", borderLeftWidth: "1.5px" },
  "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection": {
    backgroundColor: "var(--editor-selection) !important",
  },
  ".cm-gutters": { backgroundColor: "var(--bg)", color: "var(--faint)", border: "none", paddingLeft: "4px" },
  ".cm-lineNumbers .cm-gutterElement": { padding: "0 10px 0 6px", minWidth: "28px" },
  ".cm-activeLine": { backgroundColor: "var(--editor-active-line)" },
  ".cm-activeLineGutter": { backgroundColor: "transparent", color: "var(--muted)" },
  ".cm-selectionMatch": { backgroundColor: "var(--editor-match)" },
  ".cm-tooltip": { backgroundColor: "var(--menu-bg)", color: "var(--fg)", border: "1px solid var(--border)", borderRadius: "8px", boxShadow: "var(--editor-shadow)" },
  ".cm-panels": { backgroundColor: "var(--side-bg)", color: "var(--fg)" },
  ".cm-panels.cm-panels-bottom": { borderTop: "1px solid var(--border)" },
  ".cm-placeholder": { color: "var(--faint)" },
});

const startText = load();
const state = EditorState.create({
  doc: startText?.text ?? WELCOME,
  selection: startText ? EditorSelection.single(Math.min(startText.anchor ?? 0, startText.text.length), Math.min(startText.head ?? 0, startText.text.length)) : EditorSelection.cursor(WELCOME.length),
  extensions: [
    lineNumbers(),
    highlightActiveLineGutter(),
    highlightSpecialChars(),
    history(),
    drawSelection(),
    dropCursor(),
    EditorState.allowMultipleSelections.of(true),
    rectangularSelection(),
    crosshairCursor(),
    highlightActiveLine(),
    highlightSelectionMatches(),
    closeBrackets(),
    search({ top: false }),
    autocompletion({
      override: [completeFromSession],
      activateOnTyping: true,
      activateOnTypingDelay: 60,
      icons: true,
      optionClass: (c) => `cm-sql-option-${c.type}`,
      addToOptions: [
        {
          render(completion) {
            const kind = document.createElement("span");
            kind.className = "cm-sql-kind";
            kind.textContent = KIND_NAMES[completion.type] ?? completion.type ?? "";
            return kind;
          },
          position: 90,
        },
      ],
    }),
    colours,
    colourer,
    running,
    runningTracker,
    errors,
    errorSync,
    placeholder("SQL, SQL/PGQ or a dot-command (.tables, .graph …). Ctrl+Enter runs it."),
    Prec.highest(
      keymap.of([
        { key: "Mod-Enter", run: runCurrent, preventDefault: true },
        { key: "Shift-Mod-Enter", run: runAll, preventDefault: true },
        // Tab takes a completion when the list is open; otherwise it indents.
        { key: "Tab", run: acceptWithTab },
      ]),
    ),
    keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, ...completionKeymap, indentWithTab]),
    look,
    EditorView.lineWrapping,
    EditorView.contentAttributes.of({
      "aria-label": "SQL editor",
      "aria-describedby": "editor-keys",
      "aria-multiline": "true",
    }),
    EditorView.updateListener.of((update) => {
      if (update.docChanged || update.selectionSet) saveSoon(update.view);
    }),
  ],
});

// Paint the saved colours at once, before the session answers.
if (startText?.spans && startText.text === state.doc.toString()) highlighted = { text: startText.text, spans: startText.spans };

mount.replaceChildren();
mount.classList.add("editor-mount");
const keys = document.createElement("div");
keys.className = "editor-bar";
keys.innerHTML = `
  <button type="button" class="editor-run" data-run="current" title="Run the statement at the cursor, or the selection (Ctrl+Enter)">
    <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 3.2v9.6L12.6 8z" fill="currentColor"/></svg>Run
  </button>
  <button type="button" class="editor-run all" data-run="all" title="Run every statement, stopping at the first error (Shift+Ctrl+Enter)">Run all</button>
  <span id="editor-keys" class="editor-keys"><kbd>${isMac() ? "⌘" : "Ctrl"}</kbd>+<kbd>Enter</kbd> runs the statement at the cursor or the selection; with <kbd>Shift</kbd>, all of them. <kbd>Esc</kbd> then <kbd>Tab</kbd> leaves the editor.</span>`;
const host = document.createElement("div");
host.className = "editor-host";
mount.append(keys, host, live);

const view = new EditorView({ state, parent: host });
if (highlighted.text === state.doc.toString()) view.dispatch({ effects: setColours.of(decorate(highlighted.spans, highlighted.text.length)) });
keys.querySelector('[data-run="current"]').addEventListener("click", () => (runCurrent(view), view.focus()));
keys.querySelector('[data-run="all"]').addEventListener("click", () => (runAll(view), view.focus()));
addEventListener("pagehide", () => save(view));

function isMac() {
  return /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
}

// ---------------------------------------------------------------- the API

const editor = {
  view,
  /** The editor's text. */
  get text() {
    return view.state.doc.toString();
  },
  /** Replace the text (an undoable edit), the cursor at its end. */
  setText(text) {
    view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text }, selection: { anchor: text.length }, userEvent: "input.set" });
  },
  /**
   * Insert `text` for another panel (the catalog's table, the sources' file):
   * at the cursor, in place of the selection; or with `statement: true` as a
   * statement of its own on a new line at the end, selected, and with
   * `run: true` run at once. The editor takes the keyboard unless `focus` is
   * false. Resolves once it has run (with `run`), to whether it succeeded.
   */
  async insert(text, { statement = false, run = false, focus = true } = {}) {
    const doc = view.state.doc;
    let from;
    let to;
    let insert = text;
    if (statement) {
      const end = doc.toString().replace(/\s+$/, "").length;
      const before = end ? "\n\n" : "";
      from = end;
      to = doc.length;
      // SQL ends at its `;`; a dot-command is a line of its own and takes none
      // (`.graph karate;` would name a graph `karate;`).
      const body = text.trim();
      const last = statements(body).at(-1);
      const semicolon = !last || last.command || /;$/.test(body) ? "" : ";";
      insert = `${before}${body}${semicolon}\n`;
      const start = from + before.length;
      view.dispatch({ changes: { from, to, insert }, selection: { anchor: start, head: start + insert.trimEnd().length - before.length }, scrollIntoView: true, userEvent: "input.paste" });
    } else {
      const sel = view.state.selection.main;
      from = sel.from;
      to = sel.to;
      view.dispatch({ changes: { from, to, insert }, selection: { anchor: from + insert.length }, scrollIntoView: true, userEvent: "input.paste" });
    }
    if (focus) view.focus();
    if (run) {
      const sel = view.state.selection.main;
      const text = view.state.doc.toString();
      const s = statementAt(statements(text), statement ? sel.from : sel.head, text);
      return runStatements(view, s ? [s] : []);
    }
    return true;
  },
  /** Run the statement at the cursor, or the selection (Ctrl+Enter). */
  runCurrent: () => runStatements(view, currentStatements(view.state)),
  /** Run every statement, stopping at the first error (Shift+Ctrl+Enter). */
  runAll: () => runStatements(view, statements(view.state.doc.toString())),
  /** Whether a run is going on. */
  get running() {
    return Boolean(busy);
  },
  /** The errors shown: [{from, to, at, until, message, hint, kind}]. */
  get errors() {
    return view.state.field(errors);
  },
  /** The colour spans painted now, as the session answered them: [{from, to, kind}]. */
  get spans() {
    const out = [];
    const iter = view.state.field(colours).iter();
    for (; iter.value; iter.next()) out.push({ from: iter.from, to: iter.to, kind: iter.value.spec.attributes["data-kind"] });
    return out;
  },
  /** Counts: `highlights` (answers applied to the text as it was), `runs`. */
  stats: ursa,
  focus: () => view.focus(),
};

window.ursaConsole ??= {};
window.ursaConsole.editor = editor;
mount.dispatchEvent(new CustomEvent("ursa:editor", { detail: editor, bubbles: true }));
