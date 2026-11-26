// A result as a table (URSA-14): typed columns, paged from the session with
// `fetch`, drawn a screenful at a time, so 100,000 rows or a million scroll
// as smoothly as ten.
//
//   const table = createTable({ container, columns, rows, first, fetch, label });
//
//   columns   query's columns: [{name, type, nullable, metadata}], `type`
//             as the engine names it (Int64, Utf8View, List(Int64), …) and
//             `metadata` the column's `ursa.*` keys (its role in a graph)
//   rows      how many rows the whole result has
//   first     query's `arrow` part: the first rows, an Arrow IPC stream
//   fetch     fetch(offset, rows) → the Arrow IPC stream of those rows (the
//             session's `fetch`); it rejects as the session does, `not_found`
//             for a result that has gone, which the table leaves to its host
//   label     the grid's accessible name
//
// The grid (role="grid", with aria-rowcount and aria-colcount) is one focus
// stop; the arrow keys move the active cell (aria-activedescendant), Shift
// widens a range, PageUp and PageDown move a screenful, Home and End go to
// the row's ends and Ctrl+Home and Ctrl+End to the first and last rows, Ctrl+A
// selects everything, and Ctrl+C copies the cell or the range as
// tab-separated text. A pointer selects too: click, Shift+click, drag.
//
// Numbers are right-aligned; NULL is shown as the shell shows it, dimmed;
// long text is cut short with its whole value in the cell's tooltip, and in
// a strip under the table while its cell is active. A column's header sorts
// by it (ascending, descending, as it came): only the rows loaded, which the
// table says, since the whole result stays in the session.
import { tableFromIPC } from "./vendor/arrow.mjs";

/** Rows a fetch asks for. */
export const PAGE = 1000;
/** A row's height, in CSS pixels (console.css's --rt-row). */
const ROW = 26;
/** Rows drawn beyond those on screen, above and below. */
const OVERSCAN = 10;
/** The tallest the scrolled area is made; a taller result scrolls in proportion. */
const MAX_HEIGHT = 6_000_000;
/** The most rows Ctrl+C copies at once: Export writes more. */
const COPY_MAX = 50_000;
/** The most pages kept in the page; the farthest from the view go first. */
const KEEP_PAGES = 400;

const ROLE = "ursa.role";

let tables = 0;

/** Whether a column of engine type `type` holds numbers. */
function numericType(type) {
  return /^(U?Int\d+|Float\d+|Decimal\d*)/.test(type);
}

/** A column's type, short: what its header shows (the whole type in its tooltip). */
function shortType(type) {
  return type
    .replace(/^Timestamp\((\w+?)(?:second)?,\s*None\)$/i, "Timestamp($1)")
    .replace(/Field \{[^}]*data_type: (\w+)[^}]*\}/g, "$1")
    .replace(/\bnullable /g, "");
}

const pad = (n, w = 2) => String(n).padStart(w, "0");

function formatDate(ms, withTime) {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return String(ms);
  const day = `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}`;
  if (!withTime) return day;
  const frac = d.getUTCMilliseconds() ? `.${pad(d.getUTCMilliseconds(), 3)}` : "";
  return `${day} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}${frac}`;
}

/** A value as text: what a cell shows and Ctrl+C copies. Null is null. */
function text(value, type) {
  if (value === null || value === undefined) return null;
  const id = type?.typeId;
  if (id === 7 /* Decimal */) return decimal(value, type.scale);
  if (id === 8 /* Date */) return formatDate(value instanceof Date ? value.getTime() : Number(value), false);
  if (id === 10 /* Timestamp */) return formatDate(value instanceof Date ? value.getTime() : Number(value), true);
  return plain(value);
}

/** Any value, nested ones included, as text. */
function plain(value) {
  if (value === null || value === undefined) return "NULL";
  switch (typeof value) {
    case "string":
      return value;
    case "number":
    case "bigint":
    case "boolean":
      return String(value);
  }
  if (value instanceof Date) return formatDate(value.getTime(), true);
  if (value instanceof Uint8Array) return `\\x${Array.from(value.subarray(0, 64), (b) => b.toString(16).padStart(2, "0")).join("")}${value.length > 64 ? "…" : ""}`;
  // A list (an Arrow Vector), a struct (a StructRow) or a map.
  if (typeof value.toArray === "function" && typeof value.length === "number" && !(value instanceof Map)) {
    const items = [];
    for (let i = 0; i < value.length; i++) items.push(plain(value.get(i)));
    return `[${items.join(", ")}]`;
  }
  if (typeof value.toJSON === "function") {
    const json = value.toJSON();
    if (json && typeof json === "object") {
      const entries = json instanceof Map ? [...json.entries()] : Object.entries(json);
      return `{${entries.map(([k, v]) => `${k}: ${plain(v)}`).join(", ")}}`;
    }
  }
  return String(value);
}

/** A decimal's unscaled integer (Arrow JS gives its words) at `scale`. */
function decimal(value, scale) {
  let digits;
  try {
    digits = BigInt(String(value)).toString();
  } catch {
    return String(value);
  }
  if (!scale) return digits;
  const negative = digits.startsWith("-");
  const abs = (negative ? digits.slice(1) : digits).padStart(scale + 1, "0");
  return `${negative ? "-" : ""}${abs.slice(0, -scale)}.${abs.slice(-scale)}`;
}

/** Text for a TSV cell: a tab, a newline or a quote quotes it. */
function tsv(value) {
  if (value === null) return "";
  return /[\t\n\r"]/.test(value) ? `"${value.replace(/"/g, '""')}"` : value;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

/** Compare two raw values for a sort: nulls last, numbers as numbers. */
function compare(a, b) {
  if (a === b) return 0;
  if (a === null || a === undefined) return 1;
  if (b === null || b === undefined) return -1;
  if (typeof a === "number" || typeof a === "bigint") return a < b ? -1 : a > b ? 1 : 0;
  if (typeof a === "boolean") return a === b ? 0 : a ? 1 : -1;
  if (a instanceof Date) return a.getTime() - b.getTime();
  return collator.compare(typeof a === "string" ? a : plain(a), typeof b === "string" ? b : plain(b));
}

const count = (n, one, many = `${one}s`) => `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;

/** The table, in `container`: see the top of this file. */
export function createTable({ container, columns, rows: total, first, fetch, label = "Result" }) {
  const n = ++tables;
  const prefix = `rt${n}`;
  const numeric = columns.map((c) => numericType(c.type));

  // ---------------------------------------------------------------- data

  /** Loaded pages: page number → {rows, vectors, types}; pending ones → a promise. */
  const pages = new Map();
  const pending = new Map();
  let failure = null;
  const listeners = { error: new Set(), change: new Set() };
  const emit = (name, value) => listeners[name].forEach((fn) => fn(value));

  function decode(bytes) {
    const t = tableFromIPC(bytes);
    return {
      rows: t.numRows,
      vectors: columns.map((_, i) => t.getChildAt(i)),
      types: t.schema.fields.map((f) => f.type),
    };
  }
  /** The arrow part of `query`'s reply as page 0 (and on, for a first part over a page). */
  function addRows(offset, bytes) {
    const page = decode(bytes);
    // The first part holds PAGE rows (query's `rows`); a page holds one page.
    pages.set(Math.floor(offset / PAGE), page);
  }
  if (first) addRows(0, first);

  /** The raw value at row `r` (of the result), column `c`, or undefined if not loaded. */
  function raw(r, c) {
    const page = pages.get(Math.floor(r / PAGE));
    if (!page) return undefined;
    const i = r % PAGE;
    if (i >= page.rows) return undefined;
    return page.vectors[c]?.get(i) ?? null;
  }
  const loaded = (r) => {
    const page = pages.get(Math.floor(r / PAGE));
    return Boolean(page) && r % PAGE < page.rows;
  };
  /** The text at row `r`, column `c`: null for NULL, undefined if not loaded. */
  function cellText(r, c) {
    const page = pages.get(Math.floor(r / PAGE));
    if (!page) return undefined;
    const i = r % PAGE;
    if (i >= page.rows) return undefined;
    return text(page.vectors[c]?.get(i) ?? null, page.types[c]);
  }

  /** Load page `p` (once); resolves when it has come. */
  function load(p) {
    if (pages.has(p)) return Promise.resolve();
    if (pending.has(p)) return pending.get(p);
    const offset = p * PAGE;
    const promise = fetch(offset, Math.min(PAGE, total - offset)).then(
      (bytes) => {
        pending.delete(p);
        pages.set(p, decode(bytes));
        trim();
        schedule();
      },
      (e) => {
        pending.delete(p);
        failure = e;
        emit("error", e);
        throw e;
      },
    );
    pending.set(p, promise);
    promise.catch(() => {});
    return promise;
  }

  /** Keep at most KEEP_PAGES pages: drop the farthest from the view (not while sorted). */
  function trim() {
    if (pages.size <= KEEP_PAGES || order) return;
    const at = Math.floor(view.first / PAGE);
    const far = [...pages.keys()].filter((p) => p !== 0).sort((a, b) => Math.abs(b - at) - Math.abs(a - at));
    for (const p of far.slice(0, pages.size - KEEP_PAGES)) pages.delete(p);
  }

  /** The pages the view needs, nearest first, one request at a time. */
  let loading = false;
  async function want() {
    if (loading || failure) return;
    const pagesNeeded = [];
    for (let v = view.first; v <= view.last; v++) {
      const r = rowOf(v);
      if (r === undefined || loaded(r)) continue;
      const p = Math.floor(r / PAGE);
      if (!pagesNeeded.includes(p)) pagesNeeded.push(p);
    }
    if (!pagesNeeded.length) return;
    loading = true;
    try {
      await load(pagesNeeded[0]);
    } catch {
      return;
    } finally {
      loading = false;
    }
    want();
  }

  // ---------------------------------------------------------------- sort

  /** While sorted: {column, descending} and the loaded rows in their order. */
  let sort = null;
  let order = null;
  /** The table's rows as shown: the whole result, or the loaded rows sorted. */
  const viewRows = () => (order ? order.length : total);
  /** The result's row shown at view row `v`. */
  const rowOf = (v) => (order ? order[v] : v < total ? v : undefined);

  function sortBy(column) {
    const was = sort?.column === column ? sort : null;
    const next = !was ? { column, descending: false } : !was.descending ? { column, descending: true } : null;
    sort = next;
    if (!next) order = null;
    else {
      const loadedRows = [];
      for (const [p, page] of [...pages.entries()].sort((a, b) => a[0] - b[0])) for (let i = 0; i < page.rows; i++) loadedRows.push(p * PAGE + i);
      const values = new Map(loadedRows.map((r) => [r, raw(r, column)]));
      loadedRows.sort((a, b) => {
        const c = compare(values.get(a), values.get(b));
        return (next.descending && values.get(a) !== null && values.get(b) !== null ? -c : c) || a - b;
      });
      order = loadedRows;
    }
    anchor = { r: 0, c: active.c };
    active = { r: 0, c: active.c };
    scroller.scrollTop = 0;
    drawHeader();
    sayNote();
    schedule(true);
    emit("change", state());
  }

  // ---------------------------------------------------------------- markup

  const el = (tag, className, attrs = {}) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
    return node;
  };

  const root = el("div", "rt");
  const scroller = el("div", "rt-scroll", {
    role: "grid",
    tabindex: "0",
    "aria-label": label,
    "aria-rowcount": String(total + 1),
    "aria-colcount": String(columns.length + 1),
    "aria-multiselectable": "true",
    "aria-readonly": "true",
    "aria-describedby": `${prefix}-note ${prefix}-help`,
  });
  const sizer = el("div", "rt-sizer");
  const head = el("div", "rt-row rt-head", { role: "row", "aria-rowindex": "1" });
  const body = el("div", "rt-body", { role: "rowgroup" });
  sizer.append(head, body);
  scroller.append(sizer);
  const note = el("div", "rt-note", { id: `${prefix}-note`, "aria-live": "polite" });
  const peek = el("div", "rt-peek", { "aria-hidden": "true" });
  const help = el("span", "visually-hidden", { id: `${prefix}-help` });
  help.textContent = "Arrow keys move, Shift widens the selection, Ctrl+C copies it; a column's header sorts the loaded rows.";
  root.append(note, scroller, peek, help);
  container.append(root);

  // Column widths: the header, and the first rows' text, in the table's
  // monospace font (7.3 px a character at 12.5 px).
  const CHAR = 7.3;
  const widths = columns.map((col, c) => {
    let chars = Math.max(col.name.length + 2, shortType(col.type).length + (col.metadata?.[ROLE] ? col.metadata[ROLE].length + 3 : 0));
    const page = pages.get(0);
    for (let i = 0; i < Math.min(page?.rows ?? 0, 200); i++) {
      const t = cellText(i, c);
      chars = Math.max(chars, t === null ? 4 : t.length);
    }
    const min = numeric[c] ? 64 : 80;
    return Math.round(Math.min(numeric[c] ? 220 : 340, Math.max(min, chars * CHAR + 26)));
  });
  const gutter = Math.max(44, String(total).length * CHAR + 24);
  const template = `${gutter}px ${widths.map((w) => `${w}px`).join(" ")}`;
  root.style.setProperty("--rt-columns", template);
  const width = gutter + widths.reduce((a, b) => a + b, 0);
  sizer.style.width = `${width}px`;

  function drawHeader() {
    head.replaceChildren();
    const corner = el("div", "rt-cell rt-gutter rt-corner", { role: "columnheader", "aria-colindex": "1" });
    corner.innerHTML = '<span class="visually-hidden">Row</span>#';
    head.append(corner);
    columns.forEach((col, c) => {
      const role = col.metadata?.[ROLE];
      const sorted = sort?.column === c ? (sort.descending ? "descending" : "ascending") : "none";
      const cell = el("div", `rt-cell rt-th${numeric[c] ? " num" : ""}`, { role: "columnheader", "aria-colindex": String(c + 2), "aria-sort": sorted, id: `${prefix}-h${c}` });
      const button = el("button", "rt-sort", { type: "button", tabindex: "-1" });
      const name = el("span", "rt-name");
      name.textContent = col.name;
      const type = el("span", "rt-type");
      type.textContent = shortType(col.type);
      const line = el("span", "rt-typeline");
      line.append(type);
      if (role) {
        const chip = el("span", `rt-role role-${role.replace(/[^a-z_]/gi, "")}`);
        chip.textContent = role;
        line.append(chip);
      }
      const arrow = el("span", "rt-arrow", { "aria-hidden": "true" });
      arrow.textContent = sorted === "ascending" ? "↑" : sorted === "descending" ? "↓" : "";
      button.append(name, line, arrow);
      const meta = Object.entries(col.metadata ?? {})
        .filter(([k]) => k.startsWith("ursa."))
        .map(([k, v]) => `${k}: ${v}`);
      button.title = [`${col.name}: ${col.type}${col.nullable ? "" : ", not null"}`, ...meta, "Click to sort the loaded rows"].join("\n");
      button.addEventListener("click", () => {
        sortBy(c);
        scroller.focus({ preventScroll: true });
      });
      cell.append(button);
      head.append(cell);
    });
  }
  drawHeader();

  // ---------------------------------------------------------------- view

  /** The rows drawn: view rows first..last. */
  const view = { first: 0, last: -1 };
  /** The active cell and the range's other corner, in view rows and columns. */
  let active = { r: 0, c: 0 };
  let anchor = { r: 0, c: 0 };

  const scaled = () => viewRows() * ROW > MAX_HEIGHT;
  const headHeight = () => head.offsetHeight || ROW + 12;
  /** The scrolled area's height for the rows shown. */
  function sizeRows() {
    const h = scaled() ? MAX_HEIGHT : viewRows() * ROW;
    sizer.style.height = `${h + headHeight()}px`;
    scroller.setAttribute("aria-rowcount", String(viewRows() + 1));
  }
  /** The first view row on screen. */
  function topRow() {
    const visible = Math.max(1, Math.floor((scroller.clientHeight - headHeight()) / ROW));
    if (!scaled()) return Math.floor(scroller.scrollTop / ROW);
    const max = scroller.scrollHeight - scroller.clientHeight;
    const share = max > 0 ? scroller.scrollTop / max : 0;
    return Math.round(share * Math.max(0, viewRows() - visible));
  }
  /** Scroll so that view row `v` is at the top. */
  function scrollToTop(v) {
    if (!scaled()) scroller.scrollTop = v * ROW;
    else {
      const visible = Math.max(1, Math.floor((scroller.clientHeight - headHeight()) / ROW));
      const max = scroller.scrollHeight - scroller.clientHeight;
      scroller.scrollTop = (v / Math.max(1, viewRows() - visible)) * max;
    }
  }
  const visibleRows = () => Math.max(1, Math.floor((scroller.clientHeight - headHeight()) / ROW));

  /** Bring view row `v` and column `c` on screen. */
  function reveal(v, c) {
    const top = topRow();
    const shown = visibleRows();
    if (v < top) scrollToTop(v);
    else if (v >= top + shown) scrollToTop(v - shown + 1);
    if (c !== undefined) {
      const left = gutter + widths.slice(0, c).reduce((a, b) => a + b, 0);
      const right = left + widths[c];
      if (left - gutter < scroller.scrollLeft) scroller.scrollLeft = left - gutter;
      else if (right > scroller.scrollLeft + scroller.clientWidth) scroller.scrollLeft = right - scroller.clientWidth;
    }
  }

  /** The row elements drawn, reused as the view scrolls. */
  const pool = [];
  function rowElement(i) {
    if (pool[i]) return pool[i];
    const row = el("div", "rt-row", { role: "row" });
    const g = el("div", "rt-cell rt-gutter", { role: "rowheader", "aria-colindex": "1" });
    row.append(g);
    for (let c = 0; c < columns.length; c++) row.append(el("div", `rt-cell${numeric[c] ? " num" : ""}`, { role: "gridcell", "aria-colindex": String(c + 2) }));
    body.append(row);
    pool[i] = row;
    return row;
  }

  let frame = 0;
  function schedule(now = false) {
    if (now) {
      cancelAnimationFrame(frame);
      frame = 0;
      draw();
      return;
    }
    if (!frame) frame = requestAnimationFrame(() => ((frame = 0), draw()));
  }

  const inRange = (v, c) => {
    const [r0, r1] = [Math.min(anchor.r, active.r), Math.max(anchor.r, active.r)];
    const [c0, c1] = [Math.min(anchor.c, active.c), Math.max(anchor.c, active.c)];
    return v >= r0 && v <= r1 && c >= c0 && c <= c1;
  };

  function draw() {
    sizeRows();
    const rowsShown = viewRows();
    const top = topRow();
    const first = Math.max(0, top - OVERSCAN);
    const last = Math.min(rowsShown - 1, top + visibleRows() + OVERSCAN);
    view.first = first;
    view.last = last;
    // Where the drawn rows start: under the header, at their place, or in a
    // scaled view, at the top row's place on screen.
    const offset = scaled() ? scroller.scrollTop - (top - first) * ROW : first * ROW;
    body.style.transform = `translateY(${offset}px)`;
    const multi = anchor.r !== active.r || anchor.c !== active.c;
    let activeId = null;
    let i = 0;
    for (let v = first; v <= last; v++, i++) {
      const row = rowElement(i);
      row.hidden = false;
      const r = rowOf(v);
      row.setAttribute("aria-rowindex", String(v + 2));
      row.dataset.v = String(v);
      const cells = row.children;
      cells[0].textContent = String((r ?? v) + 1);
      const ready = r !== undefined && loaded(r);
      row.classList.toggle("loading", !ready);
      for (let c = 0; c < columns.length; c++) {
        const cell = cells[c + 1];
        const t = ready ? cellText(r, c) : undefined;
        const isNull = t === null;
        cell.textContent = t === undefined ? "" : isNull ? "NULL" : t.length > 2000 ? `${t.slice(0, 2000)}…` : t;
        cell.classList.toggle("null", isNull);
        cell.removeAttribute("title");
        const selected = inRange(v, c);
        cell.classList.toggle("selected", selected && multi);
        cell.setAttribute("aria-selected", String(selected));
        const isActive = v === active.r && c === active.c;
        cell.classList.toggle("active", isActive);
        if (isActive) cell.id = activeId = `${prefix}-active`;
        else cell.removeAttribute("id");
      }
    }
    for (; i < pool.length; i++) pool[i].hidden = true;
    if (activeId) scroller.setAttribute("aria-activedescendant", activeId);
    else scroller.removeAttribute("aria-activedescendant");
    showPeek();
    want();
  }

  /** Under the table: the active cell's whole value, when it is cut short. */
  function showPeek() {
    const r = rowOf(active.r);
    const t = r === undefined ? undefined : cellText(r, active.c);
    const cell = scroller.querySelector(`#${prefix}-active`);
    const cut = cell && (cell.scrollWidth > cell.clientWidth + 1 || (t && t.length > 2000));
    const focused = document.activeElement === scroller;
    peek.hidden = !(focused && cut && typeof t === "string");
    if (!peek.hidden) peek.textContent = t;
  }

  // ---------------------------------------------------------------- keyboard and pointer

  function move(r, c, extend) {
    const rowsShown = viewRows();
    active = { r: Math.max(0, Math.min(rowsShown - 1, r)), c: Math.max(0, Math.min(columns.length - 1, c)) };
    if (!extend) anchor = { ...active };
    reveal(active.r, active.c);
    schedule(true);
  }

  scroller.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    const page = visibleRows() - 1;
    const { r, c } = active;
    let handled = true;
    switch (e.key) {
      case "ArrowDown":
        move(mod ? viewRows() - 1 : r + 1, c, e.shiftKey);
        break;
      case "ArrowUp":
        move(mod ? 0 : r - 1, c, e.shiftKey);
        break;
      case "ArrowRight":
        move(r, mod ? columns.length - 1 : c + 1, e.shiftKey);
        break;
      case "ArrowLeft":
        move(r, mod ? 0 : c - 1, e.shiftKey);
        break;
      case "PageDown":
        move(r + page, c, e.shiftKey);
        break;
      case "PageUp":
        move(r - page, c, e.shiftKey);
        break;
      case "Home":
        move(mod ? 0 : r, mod ? c : 0, e.shiftKey);
        break;
      case "End":
        move(mod ? viewRows() - 1 : r, mod ? c : columns.length - 1, e.shiftKey);
        break;
      case "Escape":
        if (anchor.r === active.r && anchor.c === active.c) handled = false;
        else move(r, c, false);
        break;
      default:
        handled = false;
    }
    if (mod && !e.altKey && (e.key === "a" || e.key === "A")) {
      anchor = { r: 0, c: 0 };
      active = { r: viewRows() - 1, c: columns.length - 1 };
      all = true;
      schedule(true);
      handled = true;
    } else if (handled) all = false;
    if (mod && !e.altKey && (e.key === "c" || e.key === "C")) {
      copy();
      handled = true;
    }
    if (handled) e.preventDefault();
  });

  /** Whether the whole table is selected (Ctrl+A): a copy has the header. */
  let all = false;

  const cellAt = (target) => {
    const cell = target.closest?.(".rt-cell");
    const row = cell?.parentElement;
    if (!cell || !row || row === head || row.dataset.v === undefined) return null;
    const index = [...row.children].indexOf(cell);
    return { r: Number(row.dataset.v), c: Math.max(0, index - 1) };
  };
  body.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    const at = cellAt(e.target);
    if (!at) return;
    e.preventDefault();
    scroller.focus({ preventScroll: true });
    all = false;
    move(at.r, at.c, e.shiftKey);
    const drag = (m) => {
      const over = cellAt(document.elementFromPoint(m.clientX, m.clientY) ?? scroller);
      if (over && (over.r !== active.r || over.c !== active.c)) move(over.r, over.c, true);
    };
    const up = () => {
      removeEventListener("pointermove", drag);
      removeEventListener("pointerup", up);
    };
    addEventListener("pointermove", drag);
    addEventListener("pointerup", up);
  });
  // The whole value of a cut cell, on hover.
  body.addEventListener("pointerover", (e) => {
    const cell = e.target.closest?.(".rt-cell");
    if (!cell || cell.classList.contains("rt-gutter")) return;
    if (cell.scrollWidth > cell.clientWidth + 1 && !cell.title) cell.title = cell.textContent;
  });
  scroller.addEventListener("scroll", () => schedule(), { passive: true });
  scroller.addEventListener("focus", () => schedule(true));
  scroller.addEventListener("blur", () => (peek.hidden = true));
  new ResizeObserver(() => schedule()).observe(scroller);

  // ---------------------------------------------------------------- copy

  /** The selected range as tab-separated text, loading the rows it needs first. */
  async function selection() {
    const [r0, r1] = [Math.min(anchor.r, active.r), Math.max(anchor.r, active.r)];
    const [c0, c1] = [Math.min(anchor.c, active.c), Math.max(anchor.c, active.c)];
    if (r1 - r0 + 1 > COPY_MAX) throw new Error(`Copy takes at most ${count(COPY_MAX, "row")}: Export writes the whole result.`);
    const need = new Set();
    for (let v = r0; v <= r1; v++) {
      const r = rowOf(v);
      if (r !== undefined && !loaded(r)) need.add(Math.floor(r / PAGE));
    }
    for (const p of need) await load(p);
    const lines = [];
    if (all) lines.push(columns.slice(c0, c1 + 1).map((col) => tsv(col.name)).join("\t"));
    for (let v = r0; v <= r1; v++) {
      const r = rowOf(v);
      const cells = [];
      for (let c = c0; c <= c1; c++) cells.push(tsv(cellText(r, c) ?? null));
      lines.push(cells.join("\t"));
    }
    return { text: lines.join("\n"), rows: r1 - r0 + 1, columns: c1 - c0 + 1 };
  }

  /** What the last copy put on the clipboard, for tests. */
  let copied = null;
  async function copy() {
    let got;
    try {
      got = await selection();
    } catch (e) {
      say(e.message);
      return null;
    }
    copied = got.text;
    let ok = false;
    try {
      await navigator.clipboard.writeText(got.text);
      ok = true;
    } catch {
      // Without the async clipboard (permissions): a hidden textarea's copy.
      const area = document.createElement("textarea");
      area.value = got.text;
      area.setAttribute("readonly", "");
      area.className = "visually-hidden";
      document.body.append(area);
      area.select();
      try {
        ok = document.execCommand("copy");
      } catch {}
      area.remove();
      scroller.focus({ preventScroll: true });
    }
    const what = got.rows === 1 && got.columns === 1 ? "the cell" : `${count(got.rows, "row")} × ${count(got.columns, "column")}`;
    say(ok ? `Copied ${what}.` : `Could not copy ${what}: the browser refused the clipboard.`, true);
    return got.text;
  }

  // ---------------------------------------------------------------- the note

  let flash = null;
  /** Say `message` in the note over the table (for a moment, if `brief`). */
  function say(message, brief = false) {
    clearTimeout(flash);
    note.textContent = message;
    note.hidden = !message;
    note.classList.toggle("brief", brief);
    if (brief) flash = setTimeout(sayNote, 2500);
  }
  /** The note: what is sorted, and that it is the loaded rows only. */
  function sayNote() {
    note.replaceChildren();
    note.classList.remove("brief");
    if (!sort) {
      note.hidden = true;
      return;
    }
    note.hidden = false;
    const name = columns[sort.column].name;
    const span = el("span");
    const whole = order.length === total;
    span.textContent = whole
      ? `Sorted by ${name}, ${sort.descending ? "descending" : "ascending"}: all ${count(total, "row")} are loaded, so this is the whole result.`
      : `Sorted by ${name}, ${sort.descending ? "descending" : "ascending"}: only the ${count(order.length, "row")} loaded of ${total.toLocaleString("en-US")}, not the whole result. ORDER BY sorts them all.`;
    const clear = el("button", "rt-clear", { type: "button" });
    clear.textContent = "Clear sort";
    clear.addEventListener("click", () => {
      sort = { column: sort.column, descending: true };
      sortBy(sort.column);
      scroller.focus({ preventScroll: true });
    });
    note.append(span, clear);
  }
  sayNote();

  schedule();

  function state() {
    return {
      rows: total,
      viewRows: viewRows(),
      first: view.first,
      last: view.last,
      pages: pages.size,
      sort: sort && { column: columns[sort.column].name, descending: sort.descending },
      active: { ...active, row: rowOf(active.r) },
      // The rows drawn on screen: [row number, …cells].
      shown: [...body.children]
        .filter((row) => !row.hidden)
        .map((row) => [...row.children].map((c) => c.textContent)),
    };
  }

  return {
    element: root,
    grid: scroller,
    /** The keyboard into the grid. */
    focus: () => scroller.focus(),
    /** Make view row `v` (and column `c`) the active cell, on screen. */
    goTo: (v, c = active.c) => move(v, c, false),
    /** Select view rows r0..r1 and columns c0..c1. */
    select(r0, c0, r1, c1) {
      anchor = { r: r0, c: c0 };
      all = false;
      move(r1, c1, true);
    },
    sortBy,
    copy,
    copied: () => copied,
    /** The text of a loaded cell (null for NULL). */
    cell: (r, c) => cellText(r, c),
    /** After a failed fetch: try again (the next draw asks again). */
    retry() {
      failure = null;
      schedule();
    },
    on: (name, fn) => (listeners[name].add(fn), () => listeners[name].delete(fn)),
    state,
    /** Whether every row has been loaded. */
    complete: () => [...pages.values()].reduce((a, p) => a + p.rows, 0) >= total,
  };
}
