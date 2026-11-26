// The Catalog panel (URSA-15, design/console.md §6): what the session has
// defined, as a tree. Databases, then their schemas, then tables and views
// (each marked by its kind), then columns with their types; property graphs
// in a section of their own, with their vertex and edge tables.
//
// - It asks the session (`catalog`) when the page connects and again on
//   every `catalog` event, which the session sends every connection after
//   anything that changed it (a CREATE in this tab, another tab, an agent):
//   what was open stays open, and the scroll stays where it was.
// - Choosing a table (click, or Enter) hands `SELECT * FROM <name> LIMIT
//   100` to the editor, and a graph `.graph <name>` (actions.js).
// - The filter box keeps the tables, columns and graphs whose names have
//   what is typed, with the folders above them opened.
// - The keyboard follows the ARIA tree pattern: one stop for Tab, Up and
//   Down, Right to open or go in, Left to close or go out, Home and End, *
//   to open a level, and a letter to jump to the next name with it.
//
// The tree draws only the rows that are open, as one flat list of
// treeitems with aria-level, aria-setsize and aria-posinset, so a large
// catalog costs only what shows.
import { session } from "./session.js";
import { useQuery, describe, quoteIdent, makeNote } from "./actions.js";

/** The engine's default database and schema: tables there need no prefix. */
const DEFAULT_DB = "ursa";
const DEFAULT_SCHEMA = "main";

const ICONS = {
  database: '<svg viewBox="0 0 16 16"><ellipse cx="8" cy="4" rx="5.25" ry="2"/><path d="M2.75 4v8c0 1.1 2.35 2 5.25 2s5.25-.9 5.25-2V4M2.75 8c0 1.1 2.35 2 5.25 2s5.25-.9 5.25-2"/></svg>',
  schema: '<svg viewBox="0 0 16 16"><path d="M2.5 4.5a1 1 0 0 1 1-1h3l1.5 1.5h4.5a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1h-9a1 1 0 0 1-1-1Z"/></svg>',
  table: '<svg viewBox="0 0 16 16"><rect x="2.5" y="3" width="11" height="10" rx="1.5"/><path d="M2.5 6.5h11M2.5 9.75h11M6.5 6.5V13"/></svg>',
  view: '<svg viewBox="0 0 16 16"><path d="M1.75 8S4 3.75 8 3.75 14.25 8 14.25 8 12 12.25 8 12.25 1.75 8 1.75 8Z"/><circle cx="8" cy="8" r="2"/></svg>',
  external: '<svg viewBox="0 0 16 16"><path d="M4 2.5h5.5L12.5 5.5v8H4Z"/><path d="M9.5 2.5v3h3M6 8.5h4.5M6 11h4.5"/></svg>',
  column: '<svg viewBox="0 0 16 16"><path d="M5.5 3v10M10.5 3v10" opacity=".55"/><rect x="5.5" y="3" width="5" height="10" rx=".5"/></svg>',
  graph: '<svg viewBox="0 0 16 16"><path d="M4 4.5 11.5 4M4 4.5l2 7.5M11.5 4 6 12M11.5 4l1 6.5"/><circle cx="4" cy="4.5" r="1.75"/><circle cx="11.5" cy="4" r="1.75"/><circle cx="6" cy="12" r="1.75"/><circle cx="12.5" cy="10.5" r="1.5"/></svg>',
  vertex: '<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="4.25"/></svg>',
  edge: '<svg viewBox="0 0 16 16"><path d="M2.5 8h10M9.5 5l3 3-3 3"/></svg>',
};
/** What a row is, said after its name to a screen reader. */
const SPOKEN = { database: "database", schema: "schema", table: "table", view: "view", external: "table of files", graph: "property graph", vertex: "vertex table", edge: "edge table" };

export function mountCatalog(mount) {
  mount.replaceChildren();
  mount.classList.add("catalog-panel");
  mount.innerHTML = `
    <div class="panel-tools">
      <label class="filter">
        <svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.25"/><path d="m10.25 10.25 3.25 3.25"/></svg>
        <span class="visually-hidden">Filter the catalog</span>
        <input type="search" class="filter-input" placeholder="Filter tables, columns, graphs" autocomplete="off" spellcheck="false">
      </label>
    </div>
    <div class="catalog-state" role="status"></div>
    <h3 class="tree-head" id="catalog-tables-head" hidden>Tables</h3>
    <div class="tree" role="tree" aria-labelledby="catalog-tables-head" data-tree="tables"></div>
    <h3 class="tree-head" id="catalog-graphs-head" hidden>Property graphs</h3>
    <div class="tree" role="tree" aria-labelledby="catalog-graphs-head" data-tree="graphs"></div>`;
  const filterInput = mount.querySelector(".filter-input");
  const stateLine = mount.querySelector(".catalog-state");
  const trees = { tables: mount.querySelector('[data-tree="tables"]'), graphs: mount.querySelector('[data-tree="graphs"]') };
  const heads = { tables: mount.querySelector("#catalog-tables-head"), graphs: mount.querySelector("#catalog-graphs-head") };
  const note = makeNote(mount);

  /** The catalog as last fetched, as trees of nodes: {tables: [], graphs: []}. */
  let model = { tables: [], graphs: [] };
  /** Every node by key. */
  let byKey = new Map();
  /** What is open, by key; and what has been seen, so new folders open once. */
  const open = new Set();
  const seen = new Set();
  /** The row with the keyboard's stop (roving tabindex). */
  let active = null;
  let filter = "";
  let loaded = false;
  let fetching = null;
  let again = false;

  // ---------------------------------------------------------------- model

  function build(catalog) {
    const keys = new Map();
    const node = (key, props, parent) => {
      const n = { key, parent, children: null, ...props };
      keys.set(key, n);
      return n;
    };
    const tables = (catalog.databases ?? []).map((db) => {
      const d = node(`d/${db.name}`, { type: "database", name: db.name }, null);
      d.children = (db.schemas ?? []).map((sc) => {
        const s = node(`s/${db.name}/${sc.name}`, { type: "schema", name: sc.name }, d);
        s.children = (sc.tables ?? []).map((t) => {
          const kind = SPOKEN[t.kind] ? t.kind : "table";
          const tn = node(`t/${db.name}/${sc.name}/${t.name}`, { type: kind, name: t.name, sql: selectFrom(qualify(db.name, sc.name, t.name)), detail: columnsLabel(t.columns) }, s);
          tn.children = (t.columns ?? []).map((c) => node(`c/${db.name}/${sc.name}/${t.name}/${c.name}`, { type: "column", name: c.name, detail: shortType(c.type), title: `${c.type}${c.nullable ? "" : ", not null"}`, notNull: !c.nullable }, tn));
          return tn;
        });
        s.detail = String(s.children.length);
        return s;
      });
      return d;
    });
    const graphs = (catalog.graphs ?? []).map((g) => {
      const gn = node(`g/${g.name}`, { type: "graph", name: g.name, command: `.graph ${g.name}`, error: g.error, detail: g.error ? "unavailable" : `${(g.vertices ?? []).length} V · ${(g.edges ?? []).length} E` }, null);
      gn.children = [
        ...(g.vertices ?? []).map((v) => node(`g/${g.name}/v/${v.name}`, { type: "vertex", name: v.name, sql: selectFrom(v.table), detail: v.table === v.name ? `key ${v.key}` : `${v.table} · key ${v.key}`, title: `vertex table ${v.table}, key ${v.key}` }, gn)),
        ...(g.edges ?? []).map((e) => node(`g/${g.name}/e/${e.name}`, { type: "edge", name: e.name, sql: selectFrom(e.table), detail: `${e.source} → ${e.destination}`, title: `edge table ${e.table}, from ${e.source} to ${e.destination}` }, gn)),
      ];
      return gn;
    });
    // Databases, schemas and graphs open the first time they are seen.
    for (const [key, n] of keys) {
      if (seen.has(key)) continue;
      seen.add(key);
      if (n.type === "database" || n.type === "schema" || n.type === "graph") open.add(key);
    }
    model = { tables, graphs };
    byKey = keys;
  }

  // ---------------------------------------------------------------- fetch

  async function refresh() {
    if (fetching) {
      again = true;
      return fetching;
    }
    const client = session.client;
    if (!client) return;
    fetching = (async () => {
      try {
        const catalog = await client.call("catalog", {});
        build(catalog);
        loaded = true;
        render();
      } catch (e) {
        if (!loaded) showState(`Cannot read the catalog: ${e.message}`, "error");
        else note(`Cannot read the catalog: ${e.message}`, "error");
      }
    })();
    await fetching;
    fetching = null;
    if (again) {
      again = false;
      return refresh();
    }
  }

  function showState(text, kind = "") {
    stateLine.textContent = text;
    stateLine.dataset.kind = kind;
    stateLine.hidden = !text;
  }

  // ---------------------------------------------------------------- render

  /** Whether `n` or one under it has the filter. */
  function matches(n) {
    if (!filter) return true;
    if (n.name.toLowerCase().includes(filter)) return true;
    return n.children?.some(matches) ?? false;
  }
  const selfMatch = (n) => filter && n.name.toLowerCase().includes(filter);
  /** Whether `n` shows its children: open, or (filtering) one of them matches. */
  function expanded(n) {
    if (!n.children?.length) return false;
    if (filter) {
      // A matching table shows its columns only when opened by hand.
      if (n.children.some((c) => c.name.toLowerCase().includes(filter) || c.children?.some(matches))) return true;
      return open.has(n.key);
    }
    return open.has(n.key);
  }
  /** The rows that show, in order: [node, level, setsize, posinset]. */
  function visible(roots) {
    const rows = [];
    const walk = (list, level) => {
      const shown = list.filter(matches);
      shown.forEach((n, i) => {
        rows.push([n, level, shown.length, i + 1]);
        if (expanded(n)) walk(n.children, level + 1);
      });
    };
    walk(roots, 1);
    return rows;
  }

  function render() {
    const scroll = mount.scrollTop;
    const hadFocus = mount.contains(document.activeElement) && document.activeElement.getAttribute("role") === "treeitem";
    let total = 0;
    for (const which of ["tables", "graphs"]) {
      const rows = visible(model[which]);
      total += rows.length;
      const frag = document.createDocumentFragment();
      for (const [n, level, size, pos] of rows) frag.append(row(n, level, size, pos));
      trees[which].replaceChildren(frag);
      trees[which].hidden = rows.length === 0;
      heads[which].hidden = rows.length === 0;
    }
    if (!loaded) showState("Reading the catalog…");
    else if (!model.tables.length && !model.graphs.length) showState("Nothing here yet. Create a table, or attach a database or open a file from Sources.", "empty");
    else if (!total) showState(`Nothing matches “${filterInput.value.trim()}”.`, "empty");
    else showState("");
    // The keyboard's stop: the row it was on, or the one above it that is
    // still there, or the first.
    let target = active && byKey.has(active) ? rowFor(active) : null;
    for (let n = byKey.get(active)?.parent; !target && n; n = n.parent) target = rowFor(n.key);
    target ??= mount.querySelector('[role="treeitem"]');
    for (const el of mount.querySelectorAll('[role="treeitem"]')) el.tabIndex = -1;
    if (target) {
      target.tabIndex = 0;
      active = target.dataset.key;
      if (hadFocus) target.focus({ preventScroll: true });
    }
    mount.scrollTop = scroll;
  }

  const rowFor = (key) => mount.querySelector(`[role="treeitem"][data-key="${CSS.escape(key)}"]`);

  function row(n, level, size, pos) {
    const el = document.createElement("div");
    el.className = `tree-row type-${n.type}`;
    el.setAttribute("role", "treeitem");
    el.dataset.key = n.key;
    el.setAttribute("aria-level", String(level));
    el.setAttribute("aria-setsize", String(size));
    el.setAttribute("aria-posinset", String(pos));
    el.style.setProperty("--level", String(level - 1));
    el.tabIndex = -1;
    if (n.children?.length) el.setAttribute("aria-expanded", String(expanded(n)));
    if (n.title || n.error) el.title = n.error ?? n.title;
    const twisty = document.createElement("span");
    twisty.className = "twisty";
    twisty.setAttribute("aria-hidden", "true");
    if (n.children?.length) twisty.innerHTML = '<svg viewBox="0 0 12 12"><path d="M4.5 3 7.5 6 4.5 9"/></svg>';
    const icon = document.createElement("span");
    icon.className = "icon";
    icon.setAttribute("aria-hidden", "true");
    icon.innerHTML = ICONS[n.type] ?? "";
    const name = document.createElement("span");
    name.className = "name";
    name.append(...highlight(n.name));
    el.append(twisty, icon, name);
    if (n.type === "view" || n.type === "external") {
      const chip = document.createElement("span");
      chip.className = "kind-chip";
      chip.textContent = n.type === "view" ? "view" : "files";
      if (n.type === "external") chip.setAttribute("aria-hidden", "true");
      el.append(chip);
    }
    // What it is, for a screen reader (a view's and a table of files' chip says it).
    if (SPOKEN[n.type] && n.type !== "view") el.append(hidden(n.type === "external" ? " (a table of files)" : `, ${SPOKEN[n.type]}`));
    if (n.detail) {
      const detail = document.createElement("span");
      detail.className = "detail";
      detail.textContent = n.detail;
      if (n.type === "column") detail.prepend(hidden(", "));
      el.append(detail);
    }
    if (n.notNull) {
      const nn = document.createElement("span");
      nn.className = "not-null";
      nn.textContent = "!";
      nn.title = "not null";
      nn.setAttribute("aria-label", "not null");
      el.append(nn);
    }
    if (n.error) {
      el.classList.add("has-error");
      el.append(hidden(`, unavailable: ${n.error}`));
    }
    if (n.sql || n.command) el.classList.add("actionable");
    return el;
  }

  function hidden(text) {
    const s = document.createElement("span");
    s.className = "visually-hidden";
    s.textContent = text;
    return s;
  }

  /** `name` with the filter's match marked. */
  function highlight(name) {
    if (!filter) return [name];
    const at = name.toLowerCase().indexOf(filter);
    if (at < 0) return [name];
    const mark = document.createElement("mark");
    mark.textContent = name.slice(at, at + filter.length);
    return [name.slice(0, at), mark, name.slice(at + filter.length)];
  }

  // ---------------------------------------------------------------- actions

  function toggle(key, to = !open.has(key)) {
    if (to) open.add(key);
    else open.delete(key);
    active = key;
    render();
  }

  async function activate(n) {
    const text = n.sql ?? n.command;
    if (!text) {
      if (n.children?.length) toggle(n.key);
      return;
    }
    note(`${text}`, "pending");
    const outcome = await useQuery(text, { label: n.sql ? `SELECT from ${n.name}` : n.command });
    note(n.sql && outcome.how === "ran" ? `${text} — ${describe(outcome, text)}` : describe(outcome, text), outcome.how === "failed" ? "error" : "info");
    mount.dispatchEvent(new CustomEvent("ursa-catalog-use", { bubbles: true, detail: { text, outcome } }));
  }

  mount.addEventListener("click", (e) => {
    const el = e.target.closest('[role="treeitem"]');
    if (!el) return;
    const n = byKey.get(el.dataset.key);
    if (!n) return;
    active = n.key;
    // The arrow opens and closes; the row does what it is for.
    if (e.target.closest(".twisty") || !(n.sql || n.command)) {
      if (n.children?.length) toggle(n.key, !expanded(n));
      else focusRow(el);
      return;
    }
    focusRow(el);
    activate(n);
  });

  function focusRow(el) {
    for (const r of mount.querySelectorAll('[role="treeitem"][tabindex="0"]')) r.tabIndex = -1;
    el.tabIndex = 0;
    active = el.dataset.key;
    el.focus();
  }

  mount.addEventListener("keydown", (e) => {
    const el = e.target.closest?.('[role="treeitem"]');
    if (!el || e.altKey || e.ctrlKey || e.metaKey) return;
    const n = byKey.get(el.dataset.key);
    const rows = [...mount.querySelectorAll('[role="treeitem"]')];
    const i = rows.indexOf(el);
    const go = (r) => r && focusRow(r);
    let handled = true;
    switch (e.key) {
      case "ArrowDown":
        go(rows[i + 1]);
        break;
      case "ArrowUp":
        go(rows[i - 1]);
        break;
      case "Home":
        go(rows[0]);
        break;
      case "End":
        go(rows[rows.length - 1]);
        break;
      case "ArrowRight":
        if (n.children?.length && !expanded(n)) toggle(n.key, true), rowFor(n.key)?.focus();
        else if (n.children?.length) go(rows[i + 1]);
        break;
      case "ArrowLeft":
        if (n.children?.length && expanded(n)) toggle(n.key, false), rowFor(n.key)?.focus();
        else if (n.parent) go(rowFor(n.parent.key));
        break;
      case "Enter":
        activate(n);
        break;
      case " ":
        if (n.children?.length) toggle(n.key, !expanded(n)), rowFor(n.key)?.focus();
        else activate(n);
        break;
      case "*": {
        const siblings = n.parent ? n.parent.children : el.closest('[role="tree"]') === trees.graphs ? model.graphs : model.tables;
        for (const s of siblings) if (s.children?.length) open.add(s.key);
        render();
        rowFor(n.key)?.focus();
        break;
      }
      default:
        if (e.key.length === 1 && /\S/.test(e.key)) {
          const ch = e.key.toLowerCase();
          const next = [...rows.slice(i + 1), ...rows.slice(0, i)].find((r) => byKey.get(r.dataset.key)?.name.toLowerCase().startsWith(ch));
          go(next);
        } else handled = false;
    }
    if (handled) e.preventDefault();
  });

  // Down from the filter goes into the tree; Esc clears it.
  filterInput.addEventListener("input", () => {
    filter = filterInput.value.trim().toLowerCase();
    render();
  });
  filterInput.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      mount.querySelector('[role="treeitem"][tabindex="0"]')?.focus();
    } else if (e.key === "Escape" && filterInput.value) {
      e.preventDefault();
      filterInput.value = "";
      filter = "";
      render();
    }
  });

  // ---------------------------------------------------------------- wiring

  session.on("connect", () => refresh());
  session.on("state", (s) => mount.classList.toggle("stale", s.state !== "connected"));
  session.onEvent("catalog", () => refresh());
  render();
  if (session.state === "connected") refresh();

  return {
    refresh,
    /** The catalog as last fetched (for tests). */
    get model() {
      return model;
    },
    /** Open the rows with these keys, and draw. */
    expand(keys) {
      for (const k of keys) open.add(k);
      render();
    },
  };
}

// ---------------------------------------------------------------- names

function qualify(db, schema, table) {
  if (db === DEFAULT_DB && schema === DEFAULT_SCHEMA) return quoteIdent(table);
  if (db === DEFAULT_DB) return `${quoteIdent(schema)}.${quoteIdent(table)}`;
  return `${quoteIdent(db)}.${quoteIdent(schema)}.${quoteIdent(table)}`;
}

const selectFrom = (name) => `SELECT * FROM ${name} LIMIT 100`;

function columnsLabel(columns) {
  const n = columns?.length ?? 0;
  return n ? `${n} col${n === 1 ? "" : "s"}` : "";
}

/** Arrow's type names, shorter: Utf8 → varchar, Int64 → bigint, … */
function shortType(type) {
  const map = {
    Utf8: "varchar", Utf8View: "varchar", LargeUtf8: "varchar", Int8: "tinyint", Int16: "smallint", Int32: "int", Int64: "bigint",
    UInt8: "utinyint", UInt16: "usmallint", UInt32: "uint", UInt64: "ubigint", Float16: "half", Float32: "float", Float64: "double",
    Boolean: "boolean", Date32: "date", Date64: "date", Binary: "blob", BinaryView: "blob", LargeBinary: "blob", Null: "null",
  };
  if (map[type]) return map[type];
  if (/^Timestamp/.test(type)) return "timestamp";
  if (/^Decimal/.test(type)) return type.replace(/^Decimal\d*/, "decimal");
  if (/^(Large)?List/.test(type)) return "list";
  if (/^Struct/.test(type)) return "struct";
  return type.length > 18 ? `${type.slice(0, 16)}…` : type;
}
