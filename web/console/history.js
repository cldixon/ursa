// The History panel (URSA-17, design/console.md §6 and §9): what ran in the
// session, who ran it, and how it went, to open again, run again or replay.
//
//   import { mountHistory } from "/console/history.js";
//   const history = mountHistory(panels.history, { session, results });
//   results.setCommandsSink(history.sink);   // the graph pane's commands
//
// - The list is the session's, not the page's: the session sends every
//   connection an `activity` event after each request that ran something (a
//   query, a statement, a graph step), naming the client that ran it
//   (`hello`'s `{name, kind}`), and keeps the last 500 for `history`, which
//   this panel asks for on every (re)connect. So a second tab on the shared
//   session, or an agent's steps (paired mode, C4), show here as they
//   happen, and a page opened later sees what came before. Entries merge by
//   their `id`.
// - Each entry: the statement or command, its client, when it started and
//   how long it took, its rows or its error. Graph steps (a double-click's
//   `.expand 33`) are grouped under the drawing they belong to (`drawing`,
//   which `graph.open` starts), and a drawing under the query whose result
//   it drew.
// - Choosing an entry (click, Enter) shows its result's tab if the page
//   still has it (a drawing's or a step's: its Graph view); else it offers
//   to run it again, or to put it in the editor.
// - Copy as SQL copies the history as a script the shell replays: the
//   statements and graph steps that succeeded and changed something, in
//   order, as the shell's share link keeps them (`.tables` and other
//   commands that only look are left out; a drawing is `.graph`, or `.graph
//   <query>` when it is not of the result just before it).
// - The graph pane's commands come here (`sink`, given to the Results
//   panel's setCommandsSink): each runs in the pane, and its entry says what
//   asked for it (the graph, its inspector, the search).
// - Keyboard: one Tab stop for the list; Up and Down move between entries
//   (steps included), Home and End, Enter or Space chooses, Escape closes an
//   offer. New entries are announced politely.
import { shorten } from "./results.js";

/** Commands that only look: Copy as SQL leaves them out, as the shell's Share does (app.js's LOOKS). */
const LOOKS = /^\s*\.(help|tables|graphs|describe|functions|version|print|history|files|share|quit|exit|dataset|schema|indexes|show)\b/i;
/** Graph steps that change the drawing, and so replay. */
const REPLAYS = /^\s*\.(expand|collapse|hide|show|undo|redo)\b/i;
/** What asked for a pane command, in words. */
const VIA = { view: "from the graph", inspector: "from the inspector", search: "from the search" };

const svg = (inner, box = 16) => `<svg viewBox="0 0 ${box} ${box}" aria-hidden="true">${inner}</svg>`;
const ICON = {
  query: svg('<g fill="none" stroke="currentColor" stroke-width="1.3"><rect x="2" y="3" width="12" height="10" rx="1.5"/><path d="M2 6.5h12M6 6.5V13"/></g>'),
  feed: svg('<g fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="m3 4.5 3.5 3.5L3 11.5M8.5 12h4.5"/></g>'),
  attach: svg('<g fill="none" stroke="currentColor" stroke-width="1.3"><ellipse cx="8" cy="4" rx="5" ry="1.9"/><path d="M3 4v8c0 1 2.2 1.9 5 1.9s5-.9 5-1.9V4M3 8c0 1 2.2 1.9 5 1.9s5-.9 5-1.9"/></g>'),
  graph: svg('<g fill="none" stroke="currentColor" stroke-width="1.3"><path d="M4.5 11 8 4.5l3.5 6.5M4.5 11h7"/></g><g fill="currentColor"><circle cx="8" cy="4.5" r="1.9"/><circle cx="4.5" cy="11" r="1.9"/><circle cx="11.5" cy="11" r="1.9"/></g>'),
  step: svg('<g fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"><circle cx="5" cy="8" r="2"/><path d="M7 8h6M10.5 5.5 13 8l-2.5 2.5"/></g>'),
  find: svg('<g fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"><circle cx="7" cy="7" r="4"/><path d="m10 10 3.5 3.5"/></g>'),
  copy: svg('<g fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5V4a1.5 1.5 0 0 0-1.5-1.5H4A1.5 1.5 0 0 0 2.5 4v5A1.5 1.5 0 0 0 4 10.5h1.5"/></g>'),
  again: svg('<g fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"><path d="M12.5 8a4.5 4.5 0 1 1-1.3-3.2M12.5 2.8v2.4h-2.4"/></g>'),
  edit: svg('<g fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"><path d="M3 13l.6-2.6L10.4 3.6a1.3 1.3 0 0 1 1.9 0l.1.1a1.3 1.3 0 0 1 0 1.9L5.6 12.4Z"/></g>'),
  agent: svg('<g fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"><rect x="3" y="5" width="10" height="8" rx="2"/><path d="M8 5V2.8M6 9h0M10 9h0" stroke-linecap="round" stroke-width="1.8"/></g>'),
  error: svg('<g fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="8" cy="8" r="5.5"/><path d="M8 5v3.5M8 10.9v.1"/></g>'),
};

const el = (tag, className, attrs = {}) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  for (const [k, v] of Object.entries(attrs)) if (v !== undefined && v !== null && v !== false) node.setAttribute(k, v === true ? "" : v);
  return node;
};
const text = (tag, className, value, attrs) => {
  const node = el(tag, className, attrs);
  node.textContent = value;
  return node;
};

const count = (n, one, many = `${one}s`) => `${Number(n).toLocaleString("en-US")} ${n === 1 ? one : many}`;

/** How long it took: 4.2 ms, 312 ms, 1.4 s, 2 min 5 s. */
export function duration(ms) {
  if (!(ms >= 0)) return "";
  if (ms < 10) return `${ms.toFixed(1)} ms`;
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`;
  return `${Math.floor(ms / 60_000)} min ${Math.round((ms % 60_000) / 1000)} s`;
}

/** When it started, as a clock: 14:02:31. */
function clock(at) {
  return new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
}

/** Who ran it, in words. */
function who(client) {
  return client?.name ?? "an unnamed client";
}

/** `sql`'s last statement, without the `;`: what `.graph <query>` takes. */
function lastStatement(sql) {
  const statements = sql
    .split(/;\s*(?:\n|$)/)
    .map((s) => s.trim())
    .filter(Boolean);
  return (statements.at(-1) ?? "").replace(/;\s*$/, "");
}

/** A statement as a script line: ending in `;`, unless it is a dot-command. */
const terminated = (s) => (s.trim().startsWith(".") || /;\s*$/.test(s) ? s.trim() : `${s.trim()};`);

/**
 * The history `entries` (oldest first) as a script the shell replays: see
 * the top of this file. Returns {script, commands}.
 */
export function scriptOf(entries) {
  const commands = [];
  /** Query results, by id, and the last one the script made. */
  const queries = new Map();
  let lastResult = null;
  /** The drawings the script opened: only their steps replay. */
  const drawn = new Set();
  for (const e of entries) {
    if (e.error) continue;
    const summary = (e.summary ?? "").trim();
    if (!summary) continue;
    switch (e.op) {
      case "query":
        commands.push(terminated(summary));
        if (e.result) {
          queries.set(e.result, summary);
          lastResult = e.result;
        }
        break;
      case "feed":
        if (LOOKS.test(summary)) break;
        commands.push(terminated(summary));
        // The shell's last result is now that statement's; History knows no id for it.
        lastResult = null;
        break;
      case "sources.attach":
        commands.push(terminated(summary));
        break;
      case "graph.open": {
        let line = summary;
        if (summary === ".graph" && e.result && e.result !== lastResult) {
          const sql = queries.get(e.result);
          if (!sql) break;
          line = `.graph ${lastStatement(sql)}`;
        }
        commands.push(line);
        if (e.drawing) drawn.add(e.drawing);
        break;
      }
      case "graph.explore":
        if (REPLAYS.test(summary) && drawn.has(e.drawing)) commands.push(summary);
        break;
    }
  }
  return { script: commands.length ? `${commands.join("\n")}\n` : "", commands };
}

/** The History panel in `mount`: see the top of this file. */
export function mountHistory(mount, { session, results = null } = {}) {
  mount.replaceChildren();
  mount.classList.add("history-panel");

  const tools = el("div", "panel-tools hx-tools");
  const summaryLine = text("span", "hx-count", "", { "aria-hidden": "true" });
  const copyButton = el("button", "tool-button hx-copy", { type: "button", title: "Copy the history as a script the shell replays: its statements and graph steps, in order" });
  copyButton.innerHTML = `${ICON.copy}<span>Copy as SQL</span>`;
  tools.append(summaryLine, copyButton);

  const list = el("ol", "hx-list", { "aria-label": "History: queries and graph steps, oldest first" });
  const empty = el("div", "hx-empty");
  empty.innerHTML = `
    <div class="hx-empty-art" aria-hidden="true">${ICON.query}${ICON.graph}${ICON.step}</div>
    <p class="hx-empty-what">Nothing has run yet</p>
    <p>Queries and graph steps run in this session show here as they happen, with who ran them: this page, another tab, or an agent.</p>`;
  const live = el("div", "visually-hidden", { role: "status", "aria-live": "polite" });
  const scroller = el("div", "hx-scroll");
  scroller.append(list, empty);
  mount.append(tools, scroller, live);

  /** Every entry, by id; and `via`, what in this page asked for a pane command. */
  const entries = new Map();
  /** The session the entries are of (hello's session id), and the highest id seen. */
  let sessionId;
  let highest = 0;
  /** The entry whose offer (run again, edit) is open, by its key. */
  let offering = null;
  /** The pane commands this page is running, waiting for their entries: [{command, via}]. */
  const pendingVia = [];

  // ---------------------------------------------------------------- data

  function add(entry, { announce = false } = {}) {
    if (!entry || typeof entry.id !== "number") return;
    if (entries.has(entry.id)) return;
    const mine = pendingVia.findIndex((p) => p.command === entry.summary && entry.op.startsWith("graph."));
    if (mine >= 0) {
      entry.via = pendingVia[mine].via;
      pendingVia.splice(mine, 1);
    }
    entries.set(entry.id, entry);
    highest = Math.max(highest, entry.id);
    if (announce) say(spoken(entry));
    schedule();
  }

  /** The entries, oldest first. */
  const ordered = () => [...entries.values()].sort((a, b) => a.id - b.id);

  session.onEvent("activity", (event) => {
    const { event: _, ...entry } = event;
    add(entry, { announce: true });
  });

  async function load(hello) {
    const id = hello?.session?.id ?? null;
    if (id !== sessionId) {
      // Another session (the server restarted): its own history.
      sessionId = id;
      entries.clear();
      highest = 0;
      schedule();
    }
    let reply;
    try {
      reply = await session.client.call("history", {});
    } catch (e) {
      // An engine without history (an older one): only what happens from now.
      return;
    }
    const got = reply.entries ?? [];
    // A worker started afresh counts from 1 again.
    if (got.length && got.at(-1).id < highest) {
      entries.clear();
      highest = 0;
    }
    for (const entry of got) add(entry);
    schedule();
  }
  session.on("connect", load);
  if (session.client && session.hello) load(session.hello);

  // ---------------------------------------------------------------- the tree

  /**
   * The entries as a tree: queries (and other statements) at the top, each
   * query's drawings under it, each drawing's steps under that. A drawing or
   * step whose parent is not in the list stands at the top.
   */
  function tree() {
    const all = ordered();
    const byResult = new Map();
    const drawings = new Map();
    const top = [];
    const node = (entry) => ({ entry, children: [] });
    for (const e of all) {
      if (e.op === "graph.open") {
        const n = node(e);
        if (e.drawing) drawings.set(e.drawing, n);
        const parent = e.result ? byResult.get(e.result) : null;
        (parent ? parent.children : top).push(n);
      } else if (e.op === "graph.explore" || e.op === "graph.find") {
        const parent = e.drawing ? drawings.get(e.drawing) : null;
        (parent ? parent.children : top).push(node(e));
      } else {
        const n = node(e);
        if (e.op === "query" && e.result) byResult.set(e.result, n);
        top.push(n);
      }
    }
    return top;
  }

  // ---------------------------------------------------------------- rendering

  let frame = 0;
  function schedule() {
    if (frame) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      render();
    });
  }

  /** The kept tab of `result` in this page, if it has one. */
  function tabOf(result) {
    if (!result || !results) return null;
    return results.tabs().find((t) => t.id === result && !t.dropped && !t.closed) ?? null;
  }

  /** What an entry is, in a word, for its row and for a screen reader. */
  function kind(e) {
    if (e.op === "graph.open") return "drawing";
    if (e.op === "graph.explore") return "graph step";
    if (e.op === "graph.find") return "search";
    if (e.op === "sources.attach") return "attach";
    if (e.op === "feed") return e.summary?.trim().startsWith(".") ? "command" : "statement";
    return "query";
  }
  function icon(e) {
    if (e.op === "graph.open") return ICON.graph;
    if (e.op === "graph.explore") return ICON.step;
    if (e.op === "graph.find") return ICON.find;
    if (e.op === "sources.attach") return ICON.attach;
    if (e.op === "feed") return ICON.feed;
    return ICON.query;
  }
  /** Its outcome, short: 34 rows, 37 nodes · 84 edges, OK. */
  function outcome(e) {
    if (e.error) return e.error === "cancelled" ? "cancelled" : "failed";
    if (e.op === "query") return typeof e.rows === "number" ? count(e.rows, "row") : "OK";
    if (e.op.startsWith("graph.") && typeof e.nodes === "number") return `${count(e.nodes, "node")} · ${count(e.edges, "edge")}`;
    return "OK";
  }
  function spoken(e) {
    const what = e.op === "graph.open" && e.summary === ".graph" && e.result ? `a drawing of ${e.result}` : `${kind(e)} ${shorten(e.summary ?? "", 60)}`;
    const how = e.error ? (e.error === "cancelled" ? "cancelled" : `failed: ${e.error}`) : outcome(e);
    return `${who(e.client)} ran ${what}: ${how}, in ${duration(e.ms)}`;
  }

  function row(e, depth) {
    const key = String(e.id);
    const li = el("li", `hx-item depth-${depth}`, { "data-id": key });
    if (e.error) li.classList.add(e.error === "cancelled" ? "cancelled" : "failed");
    if (e.id > shownUpTo && shownUpTo > 0) li.classList.add("new");
    li.dataset.op = e.op;
    const button = el("button", "hx-row", { type: "button", tabindex: "-1", "data-key": key, "aria-expanded": offering === key ? "true" : null, "aria-controls": offering === key ? `hx-offer-${key}` : null });
    button.setAttribute("aria-label", `${spoken(e)}, at ${clock(e.at)}`);

    const mark = el("span", `hx-icon k-${kind(e).replace(" ", "-")}`, { "aria-hidden": "true" });
    mark.innerHTML = e.error && e.error !== "cancelled" ? ICON.error : icon(e);
    const body = el("span", "hx-body");
    const statement = el("span", "hx-statement");
    if (e.op === "graph.open" && e.summary === ".graph" && e.result) {
      statement.append(text("code", "", ".graph"), text("span", "hx-of", ` of ${e.result}`));
    } else {
      statement.append(text("code", "", e.summary ?? ""));
    }
    if (e.truncated) statement.append(text("span", "hx-of", " (cut)"));
    statement.title = e.summary ?? "";

    const meta = el("span", "hx-meta");
    const client = el("span", `hx-client ${e.client?.kind === "agent" ? "agent" : "human"}`);
    if (e.client?.kind === "agent") client.innerHTML = ICON.agent;
    else client.append(el("span", "hx-dot", { "aria-hidden": "true" }));
    client.append(text("span", "", who(e.client)));
    client.title = e.client ? `Run by ${e.client.name} (${e.client.kind})` : "Its client gave no name";
    const time = text("time", "hx-time", clock(e.at), { datetime: new Date(e.at).toISOString() });
    time.title = new Date(e.at).toLocaleString();
    const took = text("span", "hx-ms", duration(e.ms));
    const result = text("span", "hx-outcome", outcome(e));
    meta.append(client, sep(), time, sep(), took, result);
    if (e.result && e.op === "query") result.title = `Kept by the session as ${e.result}`;
    if (e.via) statement.append(text("span", "hx-via", ` · ${VIA[e.via] ?? e.via}`));
    body.append(statement, meta);
    if (e.error && e.error !== "cancelled") body.append(text("span", "hx-error", e.error));
    button.append(mark, body);
    li.append(button);
    if (offering === key) li.append(offer(e));
    return li;
  }
  const sep = () => text("span", "hx-sep", "·", { "aria-hidden": "true" });

  /** What choosing `e` offers when its result is not shown here: run again, edit. */
  function offer(e) {
    const key = String(e.id);
    const box = el("div", "hx-offer", { id: `hx-offer-${key}`, role: "group", "aria-label": "What to do with it" });
    const target = runnable(e);
    const why = el("p", "hx-offer-text");
    why.textContent = reason(e);
    box.append(why);
    const buttons = el("div", "hx-offer-buttons");
    if (target) {
      const again = el("button", "tool-button primary hx-again", { type: "button" });
      again.innerHTML = `${ICON.again}<span>Run again</span>`;
      again.addEventListener("click", () => runAgain(e));
      buttons.append(again);
      if (window.ursaConsole?.editor?.insert) {
        const edit = el("button", "tool-button hx-edit", { type: "button" });
        edit.innerHTML = `${ICON.edit}<span>Put in the editor</span>`;
        edit.addEventListener("click", () => {
          close();
          window.ursaConsole.editor.insert(target.text, { statement: true });
        });
        buttons.append(edit);
      }
    }
    const dismiss = text("button", "tool-button hx-dismiss", "Close", { type: "button" });
    dismiss.addEventListener("click", () => close(true));
    buttons.append(dismiss);
    box.append(buttons);
    box.addEventListener("keydown", (ev) => {
      if (ev.key === "Escape") {
        ev.preventDefault();
        ev.stopPropagation();
        close(true);
      }
    });
    return box;
  }

  /** Why an entry offers to run again. */
  function reason(e) {
    const q = queryOf(e);
    if (e.error === "cancelled") return "It was cancelled.";
    if (e.error) return `It failed: ${e.error}`;
    if (e.op === "query" && e.result) return `${e.result} is not open in this page: it was closed, dropped from the session, or run elsewhere.`;
    if (e.op.startsWith("graph.")) return q ? `Its drawing has gone, and ${q.result} is not open in this page.` : "Its drawing has gone.";
    return "It ran in the session: run it again here to see what it gives.";
  }

  /** The query entry a drawing, or a step, drew from. */
  function queryOf(e) {
    let open = e;
    if (e.op !== "graph.open" && e.op.startsWith("graph.")) open = ordered().find((x) => x.op === "graph.open" && x.drawing && x.drawing === e.drawing) ?? null;
    if (!open?.result) return null;
    return ordered().find((x) => x.op === "query" && x.result === open.result) ?? null;
  }

  /** What running `e` again runs: {text}, or null. */
  function runnable(e) {
    if (e.op === "query" || e.op === "feed" || e.op === "sources.attach") return { text: e.summary };
    const q = queryOf(e);
    return q ? { text: q.summary } : null;
  }

  async function runAgain(e) {
    const target = runnable(e);
    close();
    if (!target) return;
    const t = target.text.trim();
    if (t.startsWith(".")) {
      await session.run("feed", { line: t }, { label: shorten(t, 40) }).catch(() => {});
      return;
    }
    if (typeof window.ursaConsole?.run === "function") {
      const r = await window.ursaConsole.run(t);
      // A drawing's or a step's: show its graph again.
      if (e.op.startsWith("graph.") && r?.tab?.graph) r.tab.show("graph");
    } else {
      await session.run("query", { sql: t, rows: 0 }, { label: shorten(t, 40) }).catch(() => {});
    }
  }

  function close(refocus = false) {
    const was = offering;
    offering = null;
    render();
    if (refocus && was) list.querySelector(`.hx-row[data-key="${was}"]`)?.focus();
  }

  function render() {
    const focused = list.contains(document.activeElement) ? document.activeElement : null;
    const focusKey = focused?.closest(".hx-item")?.dataset.id ?? null;
    const focusWasRow = focused?.classList.contains("hx-row");
    const atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 24;
    const top = tree();
    const build = (nodes, depth, into) => {
      for (const n of nodes) {
        const li = row(n.entry, depth);
        if (n.children.length) {
          const what = n.entry.op === "graph.open" ? `Steps of drawing ${n.entry.drawing ?? ""}` : `Drawings of ${n.entry.result ?? "it"}`;
          const sub = el("ol", "hx-children", { "aria-label": what.trim() });
          build(n.children, depth + 1, sub);
          li.append(sub);
        }
        into.append(li);
      }
    };
    const fresh = document.createDocumentFragment();
    build(top, 0, fresh);
    list.replaceChildren(fresh);
    const n = entries.size;
    empty.hidden = n > 0;
    list.hidden = n === 0;
    copyButton.disabled = n === 0;
    summaryLine.textContent = n ? count(n, "entry", "entries") : "";
    // One Tab stop: the focused entry's, else the newest.
    const rows = [...list.querySelectorAll(".hx-row")];
    const stopKey = focusKey ?? current?.dataset.key;
    const keep = rows.find((r) => r.dataset.key === stopKey) ?? rows.at(-1);
    if (keep) keep.tabIndex = 0;
    current = keep ?? null;
    if (focusKey) {
      const item = list.querySelector(`.hx-item[data-id="${focusKey}"]`);
      const target = focusWasRow ? item?.querySelector(".hx-row") : item?.querySelector(".hx-offer button") ?? item?.querySelector(".hx-row");
      target?.focus({ preventScroll: true });
    }
    if (atBottom) scroller.scrollTop = scroller.scrollHeight;
    shownUpTo = Math.max(shownUpTo, highest);
  }
  let current = null;
  /** The highest id drawn so far: newer entries flash once. */
  let shownUpTo = 0;

  // ---------------------------------------------------------------- choosing

  function choose(e) {
    const key = String(e.id);
    if (offering === key) return close(true);
    // A query's tab, or a drawing's and a step's: its query's tab, as a graph.
    const q = e.op === "query" ? e : e.op.startsWith("graph.") ? queryOf(e) : null;
    const tab = q && !e.error ? tabOf(q.result) : null;
    if (tab) {
      offering = null;
      if (e.op.startsWith("graph.") && tab.graph) {
        const drawn = results?.drawn();
        tab.show("graph");
        say(drawn?.id === tab.id ? `Showing the graph of ${tab.id}` : `Drawing ${tab.id} again`);
      } else {
        tab.select();
        say(`Showing ${tab.id}`);
      }
      render();
      return;
    }
    offering = key;
    render();
    list.querySelector(`#hx-offer-${key} .hx-again, #hx-offer-${key} button`)?.focus();
  }

  list.addEventListener("click", (ev) => {
    const button = ev.target.closest(".hx-row");
    if (!button) return;
    const e = entries.get(Number(button.dataset.key));
    if (!e) return;
    setCurrent(button);
    choose(e);
  });

  function setCurrent(button) {
    for (const r of list.querySelectorAll(".hx-row")) r.tabIndex = r === button ? 0 : -1;
    current = button;
  }

  list.addEventListener("keydown", (ev) => {
    const button = ev.target.closest?.(".hx-row");
    if (!button) return;
    const rows = [...list.querySelectorAll(".hx-row")];
    const at = rows.indexOf(button);
    let next = null;
    if (ev.key === "ArrowDown") next = rows[Math.min(at + 1, rows.length - 1)];
    else if (ev.key === "ArrowUp") next = rows[Math.max(at - 1, 0)];
    else if (ev.key === "Home") next = rows[0];
    else if (ev.key === "End") next = rows.at(-1);
    else if (ev.key === "Escape" && offering) {
      ev.preventDefault();
      close(true);
      return;
    } else return;
    ev.preventDefault();
    setCurrent(next);
    next.focus();
  });

  // ---------------------------------------------------------------- copy as SQL

  async function copy() {
    const { script, commands } = scriptOf(ordered());
    if (!script) {
      say("Nothing to copy: no statement or graph step has succeeded yet");
      return { script, commands };
    }
    let copied = false;
    try {
      await navigator.clipboard.writeText(script);
      copied = true;
    } catch {
      // Without the clipboard API (a page not focused, an old browser).
      const area = el("textarea", "", { "aria-hidden": "true" });
      area.value = script;
      area.style.cssText = "position:fixed;opacity:0;pointer-events:none";
      document.body.append(area);
      area.select();
      try {
        copied = document.execCommand("copy");
      } catch {}
      area.remove();
    }
    const label = copyButton.querySelector("span");
    label.textContent = copied ? "Copied" : "Could not copy";
    copyButton.classList.toggle("done", copied);
    clearTimeout(copy.timer);
    copy.timer = setTimeout(() => {
      label.textContent = "Copy as SQL";
      copyButton.classList.remove("done");
    }, 1600);
    say(copied ? `Copied a script of ${count(commands.length, "command")}` : "Could not copy the script");
    return { script, commands, copied };
  }
  copyButton.addEventListener("click", copy);

  // ---------------------------------------------------------------- the graph pane's commands

  /**
   * The Results panel's commands sink (setCommandsSink): run the pane's
   * command in the pane; the session's `activity` lists it, marked with what
   * asked for it. A view's command for a drawing that has gone is not run.
   */
  async function sink(command, { via, current: stillShown, exec }) {
    if (!stillShown()) {
      say(`Not run: ${command}: its drawing has gone`);
      return false;
    }
    const pending = { command: command.trim().replace(/;\s*$/, ""), via };
    pendingVia.push(pending);
    let r;
    try {
      r = await exec(command);
    } finally {
      // Its entry came before the reply, or none will (the pane ran nothing).
      const at = pendingVia.indexOf(pending);
      if (at >= 0) pendingVia.splice(at, 1);
    }
    return Boolean(r?.ok);
  }

  /** Say `words` politely, once. */
  function say(words) {
    live.textContent = "";
    requestAnimationFrame(() => (live.textContent = words));
  }

  render();

  return {
    sink,
    /** The entries, oldest first. */
    entries: ordered,
    /** The script Copy as SQL copies: {script, commands}. */
    script: () => scriptOf(ordered()),
    copy,
    /** Choose the entry `id`, as a click does. */
    choose: (id) => {
      const e = entries.get(id);
      if (e) choose(e);
    },
    element: mount,
  };
}
