// The Inspector panel (URSA-23, design/console.md §6): what is selected, in
// full. A node or an edge in a result's Graph view, or a row of its table.
//
//   import { mountInspector } from "/console/inspector.js";
//   const inspector = mountInspector(panels.inspector, { results });
//
// - The graph's selection: the graph pane's own inspector
//   (graph-inspector.js), docked here (results.setInspector, graph-pane.js's
//   `inspector` option) rather than floating over the view. It is the same
//   panel the shell floats: the node's label and degree, the row or rows
//   behind it (an edge's too), and its buttons (expand, hide, pin, path from
//   here), whose commands go through the pane's commands sink: History.
// - The table's selection: the row of the active cell (table.js's "row"),
//   one field per line, its name and type beside its whole value (long text
//   in full, wrapped; NULL dimmed).
// - It shows whichever was selected last. When that goes (the graph's
//   selection is cleared, the drawing closes, the row's tab closes), it shows
//   the other if it is still selected, and else says what to select.
// - Keyboard: the shown fields are one Tab stop (they scroll with the
//   arrows); the graph's buttons are buttons, `i` in the graph view moves
//   here and Esc goes back. What it shows is announced politely.
const svg = (inner) => `<svg viewBox="0 0 16 16" aria-hidden="true">${inner}</svg>`;
const ICON = {
  row: svg('<g fill="none" stroke="currentColor" stroke-width="1.3"><rect x="2" y="3" width="12" height="10" rx="1.5"/><path d="M2 6.5h12M2 9.8h12"/></g><path d="M2.7 7.1h10.6v2.1H2.7z" fill="currentColor" opacity=".55"/>'),
  node: svg('<g fill="none" stroke="currentColor" stroke-width="1.3"><path d="M4.5 11 8 4.5l3.5 6.5"/></g><g fill="currentColor"><circle cx="8" cy="4.5" r="2.2"/><circle cx="4.5" cy="11" r="1.6" opacity=".55"/><circle cx="11.5" cy="11" r="1.6" opacity=".55"/></g>'),
  edge: svg('<g fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M4.5 11.5 11.5 4.5"/></g><g fill="currentColor" opacity=".55"><circle cx="4" cy="12" r="2"/><circle cx="12" cy="4" r="2"/></g>'),
};

const el = (tag, className, attrs = {}) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  return node;
};
const number = (n) => n.toLocaleString("en-US");

/** The Inspector panel in `mount`: see the top of this file. */
export function mountInspector(mount, { results }) {
  mount.replaceChildren();
  mount.classList.add("inspector-panel");

  const root = el("div", "ix");
  // Nothing selected: what to select.
  const empty = el("div", "ix-empty");
  empty.innerHTML = `
    <div class="ix-empty-art" aria-hidden="true">${ICON.row}${ICON.node}${ICON.edge}</div>
    <p class="ix-empty-what">Nothing selected</p>
    <p>Select a row in a result's table, or a node or an edge in its Graph view, to see it here in full.</p>`;
  // The graph pane's inspector, docked.
  const dock = el("div", "ix-graph", { hidden: "" });
  // A table's row.
  const row = el("div", "ix-row", { hidden: "" });
  const head = el("div", "ix-head");
  const title = el("h3", "ix-title");
  const source = el("p", "ix-source");
  head.append(title, source);
  const fields = el("dl", "ix-fields", { tabindex: "0", role: "region" });
  row.append(head, fields);
  const live = el("span", "visually-hidden", { "aria-live": "polite" });
  root.append(empty, dock, row, live);
  mount.append(root);

  // What there is to show, and which was selected last: "graph" or "row".
  let graph = null; // {kind, title} while the docked inspector shows something
  let selection = null; // {tab, row, view, ready, fields} while a row is selected
  let showing = null;

  function render() {
    if (showing === "graph" && !graph) showing = selection ? "row" : null;
    if (showing === "row" && !selection) showing = graph ? "graph" : null;
    empty.hidden = showing !== null;
    dock.hidden = showing !== "graph";
    row.hidden = showing !== "row";
    root.dataset.showing = showing ?? "nothing";
  }

  let sayTimer = 0;
  /** Say `text` politely, once the selection rests (arrow keys move it fast). */
  function say(text) {
    clearTimeout(sayTimer);
    sayTimer = setTimeout(() => (live.textContent = text), 350);
  }

  // ---------------------------------------------------------------- the graph

  results.setInspector(dock);
  dock.addEventListener("inspectorchange", (e) => {
    graph = e.detail;
    if (graph) {
      showing = "graph";
      say(`Inspecting ${graph.title}`);
    } else if (showing === "graph") say(selection ? `Inspecting row ${number(selection.row + 1)}` : "Nothing selected");
    render();
  });

  // ---------------------------------------------------------------- a table's row

  function showRow(sel) {
    const changed = !selection || selection.tab.id !== sel.tab.id || selection.row !== sel.row;
    selection = sel;
    showing = "row";
    const name = `row ${number(sel.row + 1)}`;
    title.textContent = name;
    source.replaceChildren(document.createTextNode("of "), Object.assign(el("code"), { textContent: sel.tab.label }), document.createTextNode(` · ${sel.tab.id}`));
    source.title = sel.tab.sql.trim();
    fields.setAttribute("aria-label", `${name} of ${sel.tab.label}`);
    fields.replaceChildren(
      ...sel.fields.map((f) => {
        const line = el("div", `ix-field${f.numeric ? " num" : ""}`);
        const dt = el("dt");
        const fieldName = el("span", "ix-name");
        fieldName.textContent = f.name;
        const type = el("span", "ix-type");
        type.textContent = f.type;
        type.title = f.fullType;
        dt.append(fieldName, type);
        const dd = el("dd");
        if (f.text === undefined) dd.append(Object.assign(el("span", "ix-wait"), { textContent: "loading…" }));
        else if (f.text === null) dd.append(Object.assign(el("span", "ix-null"), { textContent: "NULL" }));
        else dd.append(Object.assign(el("span", "ix-value"), { textContent: f.text }));
        line.append(dt, dd);
        return line;
      }),
    );
    if (changed) fields.scrollTop = 0;
    render();
    if (changed) say(`Inspecting ${name} of ${sel.tab.label}`);
  }
  results.on("row", showRow);
  results.on("close", (tab) => {
    if (selection?.tab.id !== tab.id) return;
    selection = null;
    render();
  });

  render();

  return {
    element: root,
    /** What the panel shows: "graph", "row" or null. */
    showing: () => showing,
    /**
     * What it shows, for tests: {showing, title, fields: [[name, value]]
     * (a row's), text (the panel's text)}.
     */
    state() {
      const shown = showing === "graph" ? dock : showing === "row" ? row : empty;
      return {
        showing,
        title: showing === "graph" ? (graph?.title ?? null) : showing === "row" ? title.textContent : null,
        fields: showing === "row" ? selection.fields.map((f) => [f.name, f.text ?? null]) : null,
        text: shown.innerText,
      };
    },
  };
}
