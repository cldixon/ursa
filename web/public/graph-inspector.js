// The inspector: a panel of the Graph view that shows everything known about
// what is selected (design/graph-explorer.md §4, #444).
//
//   a node         its key and label, its degree as drawn and its
//                  neighbours in the whole graph, the values drawn (SIZE BY, COLOR BY, a hop's
//                  distance), buttons (expand, hide, pin, path from here) and
//                  its row of the vertex table
//   an edge        its two ends, its edge columns and its row of the edge
//                  table
//   several nodes  how many, the edges among them, and a WHERE clause to copy
//
// It is self-contained, so it can move to ursa-viz/ui/ as it is: it has no
// import, and knows nothing of the view, the engine or the terminal. Its host
// (graph.js) describes what is selected as plain data (`show`) and gives it
// callbacks: `fetchRow` and `fetchNeighbours` ask the engine, `run` types a
// command in the terminal, `pin` and `select` change the view. Its styles are
// graph-inspector.css beside it, which it links on first use.
//
// Every value is set with textContent: nothing from the data is ever HTML.
//
// Keys: the host focuses it (`focus()`, on `i`); Esc inside it hands the
// focus back (`onLeave`).

let styled = false;
/** Link graph-inspector.css, once. */
function style() {
  if (styled) return;
  styled = true;
  const link = document.createElement("link");
  link.rel = "stylesheet";
  link.href = new URL("./graph-inspector.css", import.meta.url).href;
  document.head.append(link);
}

const number = (n) => n.toLocaleString("en-US");
const plural = (n, one) => `${number(n)} ${one}${n === 1 ? "" : "s"}`;

/** `name` as SQL reads an identifier: bare if it is a plain lower-case name, else quoted. */
export function identifier(name) {
  return /^[a-z_][a-z0-9_]*$/.test(name) ? name : `"${name.replaceAll('"', '""')}"`;
}

/** `key` as a SQL literal: a number as it is for an "int" key, else a string. */
export function literal(key, type) {
  return type === "int" && /^-?\d+$/.test(key) ? key : `'${String(key).replaceAll("'", "''")}'`;
}

/**
 * The `.graph` command `command` with its `clause` ("SIZE BY" or "COLOR BY")
 * naming the column `name`: the clause's name replaced where the command has
 * the clause, and else the clause added at the end.
 */
export function withClause(command, clause, name) {
  const words = clause.trim().split(/\s+/).join("\\s+");
  const has = new RegExp(`(\\b${words}\\s+)("(?:[^"]|"")*"|[^\\s;]+)`, "i");
  const text = command.trim().replace(/;\s*$/, "");
  if (has.test(text)) return text.replace(has, (_, lead) => `${lead}${identifier(name)}`);
  return `${text} ${clause} ${identifier(name)}`;
}

/**
 * A WHERE clause that keeps the nodes with the keys `keys` (each {key, type},
 * type "int" or "text"): `WHERE id IN (33, 0)`.
 */
export function whereOf(keys) {
  return `WHERE id IN (${keys.map((k) => literal(k.key, k.type)).join(", ")})`;
}

/** The shortest path's `.graph` command, between two keys of the graph `graph`. */
export function pathCommand(graph, from, to) {
  return `.graph SELECT * FROM shortest_path(${identifier(graph)}, source => ${literal(from.key, from.type)}, target => ${literal(to.key, to.type)}, direction => 'both')`;
}

/**
 * An inspector. `host` gives:
 *
 *   fetchRow(subject)     -> Promise of {table, sql, columns: [{name, numeric}],
 *                            rows: [[text | null]], row, note}: the subject's
 *                            row of the graph's tables
 *   fetchNeighbours(subject) -> Promise of how many neighbours a node has in
 *                            the whole graph, or null
 *   run(command)          type a command in the terminal and run it
 *   pin(index)            pin a node, or unpin it; returns whether it is pinned now
 *   select(index)         select a node alone (an edge's end)
 *   onLeave()             Esc was pressed in the panel: take the focus back
 *   onClose()             the panel has closed
 *
 * It returns {element, show(subject), close(), focus(), state()}. `element`
 * is the panel, hidden until `show`. A subject may say which `side` of the
 * view the panel takes: "right" (the right, or the bottom where it is a sheet
 * across a narrow view), or "left" (the left, or the top) when what is
 * selected is drawn under where the panel would be. A subject is one of:
 *
 *   {kind: "node", index, key, keyType, label, graph, command, degree, values,
 *    pinned, styled}: `graph` is the property graph drawn (or null), `command`
 *    the `.graph` command that drew it, `degree` the edges drawn at the node,
 *    `values` its [name, text] pairs, and `styled` whether SIZE BY and COLOR BY
 *    can name a column of its row
 *   {kind: "edge", index, src, dst, label, row, values}: `src` and `dst` are
 *    {index, key}
 *   {kind: "nodes", nodes: [{index, key, keyType}], edges}: several nodes, and
 *    how many drawn edges join two of them
 */
export function createInspector(host) {
  style();
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  };
  const button = (text, title, onClick) => {
    const b = el("button", null, text);
    b.type = "button";
    if (title) b.title = title;
    b.addEventListener("click", onClick);
    return b;
  };

  const root = el("aside", "graph-inspector");
  root.hidden = true;
  root.tabIndex = -1;
  root.setAttribute("aria-label", "Inspector");
  const head = el("div", "inspector-head");
  const title = el("h2", "inspector-title");
  const closeButton = button("", "Close the inspector (the selection stays)", () => close());
  closeButton.className = "inspector-close";
  closeButton.setAttribute("aria-label", "Close the inspector");
  closeButton.textContent = "×";
  head.append(title, closeButton);
  const body = el("div", "inspector-body");
  root.append(head, body);

  // Esc hands the focus back, and goes no further: the pane's own Esc would
  // clear the selection.
  root.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    e.preventDefault();
    e.stopPropagation();
    host.onLeave?.();
  });
  // A click in the panel is not a click on the graph under it.
  for (const type of ["pointerdown", "pointerup", "dblclick", "wheel"]) root.addEventListener(type, (e) => e.stopPropagation());

  // What is shown, for state(): tests and the host read it.
  let shown = null;
  // Bumped by every show and close: an answer for an older subject is dropped.
  let seq = 0;
  // The node a path starts from, once "path from here" was pressed: {key, type, graph}.
  let pathFrom = null;

  /** A list of name and value lines; `tools(name, value)` may give buttons for one. */
  const properties = (pairs, tools) => {
    const list = el("dl", "inspector-props");
    for (const [name, value] of pairs) {
      const dt = el("dt", null, name);
      const dd = el("dd");
      if (value === null) dd.append(el("span", "null", "NULL"));
      else dd.append(el("span", "value", value));
      const extra = tools?.(name, value) ?? [];
      if (extra.length) {
        const bar = el("span", "inspector-tools");
        bar.append(...extra);
        dd.append(bar);
      }
      list.append(dt, dd);
    }
    return list;
  };
  const section = (name) => {
    const s = el("section", "inspector-section");
    if (name) s.append(el("h3", null, name));
    return s;
  };

  /** The subject's table row, asked of the engine and filled in when it comes. */
  const rowSection = (subject, mine, started) => {
    const s = section("row");
    const wait = el("p", "inspector-note", "Reading its row…");
    s.append(wait);
    s.read = Promise.resolve()
      .then(() => host.fetchRow(subject))
      .then(
        (found) => {
          if (mine !== seq) return;
          s.replaceChildren();
          const heading = el("h3", null, found.table ? `row of ${found.table}` : "row");
          if (found.sql) heading.title = found.sql;
          s.append(heading);
          const tools =
            subject.kind === "node" && subject.styled
              ? (name) => {
                  const column = found.columns.find((c) => c.name === name);
                  const out = [button("colour by", `Colour the nodes by ${name}`, () => host.run(withClause(subject.command, "COLOR BY", name)))];
                  if (column?.numeric) out.push(button("size by", `Size the nodes by ${name}`, () => host.run(withClause(subject.command, "SIZE BY", name))));
                  return out;
                }
              : null;
          found.rows.forEach((row, i) => {
            if (found.rows.length > 1) s.append(el("h4", null, `row ${i + 1} of ${found.rows.length}`));
            s.append(properties(found.columns.map((c, j) => [c.name, row[j]]), tools));
          });
          if (found.note) s.append(el("p", "inspector-note", found.note));
          if (found.sql) {
            const sql = el("code", "inspector-sql", found.sql);
            sql.title = "The query that reads this row";
            s.append(sql);
          }
          shown.table = found.table ?? null;
          shown.note = found.note ?? null;
          shown.sql = found.sql ?? null;
          shown.rows = found.rows.map((row) => found.columns.map((c, j) => [c.name, row[j]]));
          shown.rowMs = performance.now() - started;
          shown.engineMs = found.ms ?? null;
          shown.indexed = found.indexed ?? null;
        },
        (e) => {
          if (mine !== seq) return;
          wait.textContent = `Its row could not be read: ${String(e?.message ?? e)}`;
          shown.note = wait.textContent;
          shown.rows = [];
        },
      );
    return s;
  };

  const showNode = (subject, mine, started) => {
    // In a grouped drawing (#499) a node is a group ("open" or "closed")
    // or a member of an open one ("member").
    const isGroup = subject.group === "open" || subject.group === "closed";
    title.textContent = `${isGroup ? "group" : "node"} ${subject.key}`;
    const facts = [];
    if (subject.label) facts.push(["label", subject.label]);
    const degree = el("span", "value", number(subject.degree));
    const list = properties(facts);
    const dd = el("dd");
    dd.append(degree, el("span", "inspector-unit", " drawn"));
    list.append(el("dt", null, "degree"), dd);
    const top = section();
    top.append(list);
    // The row first: the engine answers one question at a time.
    const row = rowSection(subject, mine, started);
    if (subject.graph !== null) {
      row.read
        .then(() => (mine === seq ? host.fetchNeighbours(subject) : null))
        .then(
          (whole) => {
            if (mine !== seq || whole === null || whole === undefined) return;
            dd.append(el("span", "inspector-unit", " · "), el("span", "value", number(whole)), el("span", "inspector-unit", ` ${whole === 1 ? "neighbour" : "neighbours"} in ${subject.graph}`));
            shown.neighbours = whole;
          },
          () => {},
        );
    }
    if (subject.values.length) top.append(properties(subject.values));

    const actions = el("div", "inspector-actions");
    // What a command names it by: its key, or a group's "group:3".
    const key = subject.id ?? subject.key;
    if (subject.group === "open") actions.append(button("collapse", `Close this group (.collapse ${key})`, () => host.run(`.collapse ${key}`)));
    else if (isGroup) actions.append(button("expand", `Open this group: its members, in its disc (.expand ${key})`, () => host.run(`.expand ${key}`)));
    else if (!subject.group) actions.append(button("expand", `Add this node's neighbours (.expand ${key})`, () => host.run(`.expand ${key}`)));
    actions.append(button("hide", `Take this ${isGroup ? "group" : "node"} out of the view (.hide ${key})`, () => host.run(`.hide ${key}`)));
    const pin = button(subject.pinned ? "unpin" : "pin", "Mark this node with a pin", () => {
      const on = host.pin(subject.index);
      pin.textContent = on ? "unpin" : "pin";
      pin.setAttribute("aria-pressed", String(on));
      shown.pinned = on;
    });
    pin.setAttribute("aria-pressed", String(!!subject.pinned));
    actions.append(pin);
    if (subject.graph !== null && !isGroup) {
      const here = { key, type: subject.keyType, graph: subject.graph };
      if (pathFrom && pathFrom.graph === subject.graph && pathFrom.key !== key) {
        const from = pathFrom;
        actions.append(
          button(`path from ${from.key} to here`, `Draw the shortest path from ${from.key} to ${key}`, () => {
            pathFrom = null;
            host.run(pathCommand(subject.graph, from, here));
          }),
        );
      }
      const start = button("path from here", "Then select another node, and draw the shortest path to it", () => {
        pathFrom = here;
        start.textContent = "path from here: select the other end";
        start.disabled = true;
        shown.pathFrom = key;
      });
      if (pathFrom?.key === key && pathFrom.graph === subject.graph) {
        start.textContent = "path from here: select the other end";
        start.disabled = true;
      }
      actions.append(start);
    }
    top.append(actions);
    body.replaceChildren(top, row);
  };

  const showEdge = (subject, mine, started) => {
    title.textContent = `edge ${subject.src.key} → ${subject.dst.key}`;
    const top = section();
    const ends = el("dl", "inspector-props");
    for (const [name, end] of [
      ["from", subject.src],
      ["to", subject.dst],
    ]) {
      const dd = el("dd");
      const bar = el("span", "inspector-tools");
      bar.append(button("select", `Select node ${end.key}`, () => host.select(end.index)));
      dd.append(el("span", "value", end.key), bar);
      ends.append(el("dt", null, name), dd);
    }
    top.append(ends);
    const facts = [];
    if (subject.label) facts.push(["label", subject.label]);
    if (subject.row !== null) facts.push(["row", subject.row]);
    if (facts.length || subject.values.length) top.append(properties([...facts, ...subject.values]));
    body.replaceChildren(top, rowSection(subject, mine, started));
  };

  const showNodes = (subject) => {
    const { nodes } = subject;
    title.textContent = `${number(nodes.length)} nodes`;
    const top = section();
    top.append(
      properties([
        ["nodes", number(nodes.length)],
        ["edges among them", number(subject.edges)],
        ["keys", nodes.map((n) => n.key).join(", ")],
      ]),
    );
    const actions = el("div", "inspector-actions");
    const keys = nodes.map((n) => n.id ?? n.key).join(" ");
    actions.append(button("hide", "Take these nodes out of the view", () => host.run(`.hide ${keys}`)));
    if (nodes.length === 2 && subject.graph !== null) {
      const [a, b] = nodes.map((n) => ({ key: n.key, type: n.keyType }));
      actions.append(button("path between", `Draw the shortest path from ${a.key} to ${b.key}`, () => host.run(pathCommand(subject.graph, a, b))));
    }
    top.append(actions);

    const where = whereOf(nodes.map((n) => ({ key: n.key, type: n.keyType })));
    const s = section("as SQL");
    const line = el("div", "inspector-copy");
    const code = el("code", "inspector-sql", where);
    const copy = button("copy", "Copy this WHERE clause", async () => {
      try {
        await navigator.clipboard.writeText(where);
        copy.textContent = "copied";
      } catch {
        // No clipboard (an insecure page, a refusal): select it, to copy by hand.
        const range = document.createRange();
        range.selectNodeContents(code);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
        copy.textContent = "selected";
      }
      setTimeout(() => (copy.textContent = "copy"), 1500);
    });
    line.append(code, copy);
    s.append(line, el("p", "inspector-note", "Keeps these nodes in a query on a table keyed by id."));
    body.replaceChildren(top, s);
    shown.where = where;
    shown.edges = subject.edges;
    shown.count = nodes.length;
  };

  /** Show `subject`, replacing what was shown. */
  function show(subject) {
    const mine = ++seq;
    const started = performance.now();
    shown = { kind: subject.kind, index: subject.index ?? null, rows: null, table: null, note: null, sql: null, rowMs: null, engineMs: null };
    if (subject.kind === "node") showNode(subject, mine, started);
    else if (subject.kind === "edge") showEdge(subject, mine, started);
    else showNodes(subject);
    shown.title = title.textContent;
    shown.side = subject.side === "left" ? "left" : "right";
    root.classList.toggle("left", shown.side === "left");
    root.hidden = false;
  }

  /** Hide the panel. The host's selection is its own affair. */
  function close() {
    seq++;
    const had = root.contains(document.activeElement);
    root.hidden = true;
    shown = null;
    body.replaceChildren();
    if (had) host.onLeave?.();
    host.onClose?.();
  }

  return {
    element: root,
    show,
    close,
    /** Move the focus into the panel, if it is open. Returns whether it did. */
    focus() {
      if (root.hidden) return false;
      root.focus();
      return true;
    },
    /**
     * What is shown, or null: {kind, title, table, rows (each a list of
     * [name, value] pairs; null until they are read), note, sql, rowMs (from
     * show to the row drawn), engineMs (the engine's part of it), indexed
     * (an edge's rows came through the graph's index, not a scan), text
     * (the panel's text), focused, …}.
     */
    state() {
      if (!shown) return null;
      return { ...shown, text: root.innerText, focused: root.contains(document.activeElement), buttons: Array.from(root.querySelectorAll("button"), (b) => b.textContent) };
    },
  };
}
