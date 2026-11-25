// The graph pane: the Graph view (graph.js) with everything around it that a
// page needs to show one, for any host. The Web Shell (app.js) embeds it
// beside its terminal; the console (design/console.md §3) beside its query
// editor; web/tests/pane-host.html, a page the e2e test drives, with nothing
// else at all.
//
// A host gives the pane what only a host has, and nothing more:
//
//   const pane = createPane({ container, engine, commands, status });
//
//   container  the element the pane's markup goes in (appended to it)
//   engine     the engine client (engine-client.js's connect(), or anything
//              with its call(op, args)): graph.open, graph.explore,
//              graph.find, graph.notShown, graph.inspect, graph.close
//   commands   the commands sink: commands(command, {via, current}) runs a
//              command the pane asks for, and resolves once it has run.
//              `via` says what asked: "view" (a double-click's `.expand 33`,
//              Enter with nodes selected), "inspector" (its buttons) or
//              "search" (the `/` box's `.find TEXT`, a not-shown match's
//              `.show KEY`). `current()` says whether the drawing it was
//              asked of is still the one shown: a host that queues commands
//              drops a view's command whose drawing has gone. The shell's sink
//              is its terminal (typed, echoed, kept in the history); the
//              console's is its History panel. Either way the command comes
//              back to the pane's exec() to run.
//   status     status(message): what went wrong outside of any command (a
//              lost GPU device, a layout that stopped), for the host to say
//              where it says things. The pane has closed by then.
//
// and, optionally, `focus()`: where the keyboard goes when it leaves the pane
// (Esc, the close button); and `prefix` ("graph"), the ids' prefix.
//
// The pane owns its markup (a <section> with its header, search box, legend,
// list of matches not shown and stage; ids `${prefix}-pane`,
// `${prefix}-status`, …, which style.css styles by class), its keys (`/`
// searches, Esc clears the selection and then closes), the not-shown counts
// (asked again after every change), the theme (it follows the page's
// data-theme and the system's), and graph.js, which it imports on the first
// drawing: a host that draws no graph downloads none of it.
//
// The pane runs the graph commands itself, through exec(line): `.graph ...`
// (a drawing, `.graph detail`, `.graph history`), `.find`, and the explore
// commands (`.expand`, `.collapse`, `.hide`, `.show`, `.undo`, `.redo`).
// Each resolves to {ok, out, err, error, replay}: `out` and `err` what the
// engine printed, `error` the pane's own error message (or null), and
// `replay` the command, as a log a session could be replayed from keeps it,
// if it changed what is drawn (or null). It rejects only if the engine call
// itself failed (a crash: e.crash).
import { setup as setupSearch } from "./graph-search.js";

/** The commands exec() runs, by their first word. */
export const PANE_COMMANDS = [".graph", ".find", ".expand", ".collapse", ".hide", ".show", ".undo", ".redo"];

const EXPLORE = new Set([".expand", ".collapse", ".hide", ".show", ".undo", ".redo"]);

const svg = (inner) => `<svg viewBox="0 0 24 24" aria-hidden="true">${inner}</svg>`;
const FIND_ICON = svg('<circle cx="10.5" cy="10.5" r="5.5" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M14.7 14.7 19 19" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>');
const CLOSE_ICON = svg('<path d="M6 6l12 12M18 6 6 18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>');

/** The pane's markup, hidden, in `container`: its parts by name. */
function markup(container, prefix) {
  const el = (tag, props = {}, attrs = {}, ...children) => {
    const node = Object.assign(document.createElement(tag), props);
    for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
    node.append(...children);
    return node;
  };
  const id = (name) => `${prefix}-${name}`;
  const status = el("span", { id: id("status"), className: "graph-status" }, { "aria-live": "polite" });
  const input = el(
    "input",
    { id: id("search-input"), type: "search", placeholder: "Find a key or a label, or WHERE …", autocomplete: "off", spellcheck: false },
    { "aria-label": "Find nodes: a key, a label, or WHERE and a condition", autocapitalize: "off", enterkeyhint: "search" },
  );
  // Search (/): graph-search.js. Enter runs .find through the commands sink.
  const form = el("form", { id: id("search"), className: "graph-search", hidden: true }, { role: "search" }, input);
  const find = el("button", { id: id("find"), type: "button", className: "graph-close", title: "Find nodes (/)", innerHTML: FIND_ICON }, { "aria-label": "Find nodes", "aria-expanded": "false", "aria-controls": id("search") });
  const close = el("button", { id: id("close"), type: "button", className: "graph-close", title: "Close the graph view (Esc)", innerHTML: CLOSE_ICON }, { "aria-label": "Close the graph view" });
  const legend = el("div", { id: id("legend"), className: "graph-legend", hidden: true }, { "aria-label": "Legend" });
  const found = el("div", { id: id("found"), className: "graph-found", hidden: true });
  const stage = el("div", { id: id("stage"), className: "graph-stage" });
  const pane = el("section", { id: id("pane"), className: "graph-pane", hidden: true }, { "aria-label": "Graph view" }, el("div", { className: "graph-head" }, {}, status, form, find, close), legend, found, stage);
  container.append(pane);
  return { pane, status, form, input, find, close, legend, found, stage };
}

/** The graph pane in `container`, for a host: see the top of this file. */
export function createPane({ container, engine, commands, status: report, focus = () => {}, prefix = "graph" }) {
  const ui = markup(container, prefix);
  const call = (op, args = {}) => engine.call(op, args);

  // graph.js, imported on the first drawing (it loads ursa-viz, a few MB, in
  // turn); `view` once loaded, for the host's test hooks.
  let loading = null;
  let view = null;
  const load = () =>
    (loading ??= import("./graph.js").then(
      (m) => (view = m),
      (e) => {
        loading = null;
        throw e;
      },
    ));

  const isOpen = () => !ui.pane.hidden;

  // Bumped by each drawing: a command its view asked for is not run on the next one.
  let drawings = 0;

  // When each step of the last drawing ended (performance.now()), for tests
  // and tests/graph-timing.ts: enter, engine, viz, posted, view, the worker's
  // workerStart, workerLoaded and hierarchy, firstPositions, firstDraw, done;
  // and `command`, what was typed. timings() gives them.
  let times = null;
  // The last explore command's answer, for tests: its `command`, how many
  // `nodes` and `edges` the engine shows after it, `deltaBytes`, the size of
  // the change it sent (0 for none), and what the view `drawn` of it.
  let explored = null;
  // The last .find's answer, for tests: its `command`, the `ms` the engine
  // took to answer (the round trip), how many nodes `matched` in the view and
  // how many of them were `selected`, and the matches `notShown`
  // ([{key, hidden}], the first of `notShownTotal`).
  let found = null;

  const search = setupSearch({
    form: ui.form,
    input: ui.input,
    button: ui.find,
    list: ui.found,
    status: ui.status,
    run: (command) => commands(command, { via: "search", current: () => true }),
    focusView: () => {
      const canvas = ui.pane.querySelector("canvas");
      if (canvas) canvas.focus();
      else focus();
    },
  });

  /** Close the pane: the engine's graph closes with it, and the focus goes back to the host. */
  function close() {
    if (ui.pane.hidden) return;
    search.close(false);
    search.clear();
    // .expand then says no graph is open.
    call("graph.close").catch(() => {});
    const hadFocus = ui.pane.contains(document.activeElement);
    ui.pane.hidden = true;
    loading?.then((m) => m.close(), () => {});
    ui.stage.replaceChildren();
    ui.status.textContent = "";
    ui.legend.replaceChildren();
    ui.legend.hidden = true;
    if (hadFocus) focus();
  }

  /** Esc: clear the selection, or else close. Whether the pane was open. */
  function escape() {
    if (!isOpen()) return false;
    if (!view?.clearSelection()) close();
    return true;
  }

  /**
   * Ask the engine how many of each node's edges lead to something not
   * shown, and tell the view (its +N, in the tooltip and the header). Not
   * waited for: the answer is for whatever is drawn when it comes, or it is
   * dropped.
   */
  function countNotShown() {
    call("graph.notShown").then(
      ({ counts }) => isOpen() && view?.setNotShown(counts),
      () => {},
    );
  }

  const result = (fields = {}) => ({ ok: true, out: "", err: "", error: null, replay: null, ...fields });
  const failed = (error) => result({ ok: false, error });

  /**
   * Draw `text` (`.graph`'s argument: a query, a property graph's name, or
   * nothing for the last result), replacing what the pane drew.
   *
   * The pane opens with "Loading graph view…" as soon as the engine takes
   * more than a moment (at once, for a big graph; not at all, for a quick
   * typo). graph.js, and ursa-viz with it, is first downloaded once the
   * engine has the graph: a `.graph` that fails downloads none of it,
   * however long the engine took to say so (URSA-11). Once downloaded, its
   * layout worker starts with the pane, while the engine still works.
   */
  async function draw(text, line = `.graph ${text}`) {
    const t = (times = { enter: performance.now(), command: `.graph ${text}`.trim() });
    const wasOpen = isOpen();
    const before = ui.status.textContent;
    let opened = false;
    const open = () => {
      if (opened) return;
      opened = true;
      ui.pane.hidden = false;
      ui.status.textContent = "Loading graph view…";
      view?.preload();
    };
    const soon = setTimeout(open, 150);
    let graph;
    try {
      graph = await call("graph.open", { text });
      t.engine = performance.now();
    } catch (e) {
      clearTimeout(soon);
      // The pane as it was: closed, or with its drawing.
      if (opened && !wasOpen) close();
      else if (opened) ui.status.textContent = before;
      if (e.crash) throw e;
      return failed(e.message);
    }
    clearTimeout(soon);
    open();
    const drawing = ++drawings;
    const current = () => drawing === drawings && isOpen();
    let drawn;
    try {
      const module = await load();
      // A new drawing: the last search's matches were of the old one.
      search.clear();
      drawn = await module.show(ui.stage, graph, {
        status: ui.status,
        legend: ui.legend,
        onError: viewFailed,
        onCommand: (command) => commands(command, { via: "view", current }),
        times: t,
        run: (command) => commands(command, { via: "inspector", current: () => true }),
        inspect: (args) => call("graph.inspect", args),
      });
    } catch (e) {
      // Closed (Esc) before it was drawn: nothing went wrong.
      if (!isOpen()) return result();
      close();
      return failed(`the graph view could not start: ${String(e?.message ?? e)}`);
    }
    const count = (n, one) => `${n.toLocaleString("en-US")} ${one}${n === 1 ? "" : "s"}`;
    const skipped = graph.skipped ? `, skipping ${count(graph.skipped, "row")} with a NULL endpoint` : "";
    const as = drawn.drawnAs ? ` as ${drawn.drawnAs}` : "";
    const at = drawn.saved ? " at its saved positions" : "";
    const replacing = wasOpen ? "replacing the last drawing; " : "";
    // A drawing is part of a session a log replays: what is explored starts from it.
    const replay = isOpen() ? line.trim() : null;
    if (drawn.grouped) {
      // A grouped drawing (#499) says its groups, and how one opens.
      const g = drawn.grouped;
      return result({
        replay,
        out:
          `Drawing ${drawn.drawnAs}: ${count(g.groups, "group")} of ${count(g.total, "node")} by ${g.by}, with ${count(drawn.edges, "edge")} between groups${skipped} ` +
          `(double-click a group, or .expand group:N, to open it; ${replacing}Esc closes)\r\n`,
      });
    }
    let out = `Drawing ${count(drawn.edges, "edge")}, ${count(drawn.nodes, "node")}${as}${at}${skipped} (${replacing}Esc closes)\r\n`;
    // A layout that will take a minute: how to run it once and draw from it (#502).
    const hint = drawn.saved ? null : view?.positionsHint(drawn.edges, drawn.graph);
    if (hint)
      out +=
        `Its layout's fine level takes about a minute. To draw it at once, lay it out once and draw from that:\r\n` +
        `  ${hint.copy}\r\n` +
        `  ${hint.draw}\r\n` +
        `The file stays in this session (.files lists it). As a table instead:\r\n` +
        `  ${hint.here.join("\r\n  ")}\r\n` +
        `The ursa CLI runs the same COPY on every core, and .files add brings its file here.\r\n`;
    countNotShown();
    return result({ out, replay });
  }

  /** The view failed while drawing (a lost GPU, a layout that stopped): close, and tell the host. */
  function viewFailed(message) {
    close();
    report(message);
  }

  /**
   * `.graph detail ?on|off?`: draw a large graph in full detail, or
   * aggregated again (#498); with no word, say which. The view's `a` does
   * the same.
   */
  async function detail(words) {
    const word = (words[0] ?? "").toLowerCase();
    if (words.length > 1 || !["", "on", "off"].includes(word)) return failed("usage: .graph detail on|off");
    const graph = await load();
    const { detail, large } = graph.detail(word ? word === "on" : undefined);
    const small = large === false ? " The graph drawn now is small enough to be drawn whole either way." : "";
    return result({
      out:
        (detail
          ? "A large graph is drawn in full detail, every node and edge (.graph detail off aggregates it again)."
          : "A large graph is drawn aggregated once it is laid out: a disc for each group of nodes, opened as the view zooms in (.graph detail on draws every node and edge).") +
        small +
        "\r\n",
    });
  }

  /**
   * An explore command (.expand, .collapse, .hide, .show, .undo, .redo,
   * .graph history): the engine changes its graph and prints what changed,
   * and gives the change as a UGR delta, which the view applies to its
   * drawing (graph.js's applyDelta): new nodes appear beside their
   * neighbours, hidden ones go, and an undo puts every node back where it
   * was. A command that changed the graph is to be replayed.
   */
  async function explore(line) {
    const at = performance.now();
    const r = await call("graph.explore", { command: line });
    const command = line.trim().replace(/;\s*$/, "");
    explored = { command: line, nodes: r.nodes, edges: r.edges, deltaBytes: r.delta?.byteLength ?? 0, drawn: null };
    let lost = null;
    if (r.delta && isOpen()) {
      const kind = /^\.(undo|redo)\b/i.exec(command)?.[1].toLowerCase() ?? "step";
      try {
        explored.drawn = (await load()).applyDelta(r.delta, { kind, command, at, engine: performance.now() - at });
      } catch (e) {
        lost = String(e?.message ?? e);
      }
    }
    const answer = { ok: !r.err, out: r.out, err: r.err };
    if (lost !== null) {
      // The pane no longer draws what the engine holds: close both.
      close();
      return result({ ...answer, ok: false, error: `the graph view could not draw the change (${lost}); run .graph to draw it again` });
    }
    if (r.delta) countNotShown();
    return result({ ...answer, replay: r.delta ? command : null });
  }

  /**
   * `.find TEXT` or `.find WHERE condition`: the engine says which nodes of
   * the graph drawn match (ursa-engine's graph::view::find), the view
   * selects them and fits them (graph.js's selectFound), and the pane lists
   * the matches that are not shown. A search that matches nothing in the
   * view leaves the selection as it was.
   */
  async function find(line) {
    const start = performance.now();
    const r = await call("graph.find", { command: line });
    found = { command: line, ms: performance.now() - start, matched: r.matches, selected: r.selected.length, notShown: r.notShown, notShownTotal: r.notShownTotal };
    if (r.err) return result({ ok: false, out: r.out, err: r.err });
    if (r.selected.length) view?.selectFound(r.selected, r.matches);
    search.show(r.notShown, r.notShownTotal);
    return result({ out: r.out, err: r.err });
  }

  /** Run a graph command (PANE_COMMANDS): see the top of this file. Null for any other. */
  async function exec(line) {
    const [name, ...rest] = line.trim().replace(/;\s*$/, "").split(/\s+/);
    const arg = rest.join(" ");
    if (name === ".graph") {
      if (/^history\b/i.test(arg)) return explore(line);
      if (/^detail\b/i.test(arg)) return detail(rest.slice(1));
      // The query as typed: its spaces may be in a string.
      return draw(line.trim().slice(name.length).trim(), line);
    }
    if (name === ".find") return find(line);
    if (EXPLORE.has(name)) return explore(line);
    return null;
  }

  ui.close.onclick = () => {
    close();
    focus();
  };
  ui.pane.addEventListener("keydown", (e) => {
    // `/` searches, from anywhere in the pane.
    if (e.key === "/" && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      search.open();
      return;
    }
    if (e.key !== "Escape") return;
    e.preventDefault();
    // A selection is cleared first; Esc again closes the pane.
    if (view?.clearSelection()) return;
    close();
    focus();
  });

  // The theme: the view draws in the page's colours (style.css's --graph-*),
  // so it draws again when the page's data-theme or the system's changes.
  const retheme = () => view?.retheme();
  new MutationObserver(retheme).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", retheme);

  return {
    element: ui.pane,
    isOpen,
    exec,
    close,
    escape,
    /** graph.js once loaded (its test hooks: info(), nodeAt(), …), else null. */
    view: () => view,
    /** The last drawing's step times ({enter, engine, viz, …, command}), or null. */
    times: () => times,
    explored: () => explored,
    found: () => found,
  };
}
