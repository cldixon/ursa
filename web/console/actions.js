// What the Catalog and Sources panels do with what is chosen in them: a
// query (`SELECT * FROM … LIMIT 100`) or a command (`.graph karate`), handed
// to the editor. One function, so that the panels need not know which
// panels exist yet:
//
//   import { useQuery, quoteIdent, quoteString } from "/console/actions.js";
//   useQuery("SELECT * FROM t LIMIT 100", { from: "catalog" });
//
// In order, the first that is there:
// 1. the Editor (URSA-13): `ursaConsole.editor.insert(text)`, which puts the
//    text in the editor (and focuses it);
// 2. the Results panel (URSA-14): `ursaConsole.run(text)`, which runs it into
//    a tab of its own;
// 3. neither (this console without them): a query runs on the session as
//    `query` and the panel says what came back (how many rows, how long); a
//    command, which `query` cannot run, is copied to the clipboard.
//
// It resolves to what happened: {how: "inserted" | "run" | "ran" | "copied"
// | "failed", rows?, ms?, error?}, which the panels show.
import { session } from "./session.js";

/** Hand `text` (SQL, or a dot-command) to the editor, or run it. */
export async function useQuery(text, { label } = {}) {
  const ursa = window.ursaConsole ?? {};
  const editor = ursa.editor;
  if (editor && typeof editor.insert === "function") {
    await editor.insert(text);
    return { how: "inserted" };
  }
  if (typeof ursa.run === "function") {
    await ursa.run(text);
    return { how: "run" };
  }
  if (/^\s*\./.test(text)) {
    try {
      await navigator.clipboard.writeText(text);
      return { how: "copied" };
    } catch (e) {
      return { how: "failed", error: `cannot copy ${text}: ${e.message}` };
    }
  }
  try {
    const reply = await session.run("query", { sql: text, rows: 0 }, { label: label ?? "query" });
    // Nothing shows the result yet: let it go.
    if (reply.result) session.client?.call("close", { result: reply.result }).catch(() => {});
    return { how: "ran", rows: reply.rows, ms: reply.ms };
  } catch (e) {
    return { how: "failed", error: e.message };
  }
}

/** What happened, in a few words, for a panel's note. */
export function describe(outcome, text) {
  switch (outcome.how) {
    case "inserted":
      return "In the editor.";
    case "run":
      return "Running in Results.";
    case "copied":
      return `Copied ${text}`;
    case "ran":
      return `Ran in ${Math.round(outcome.ms ?? 0)} ms: ${count(outcome.rows)}.`;
    default:
      return outcome.error ?? "It failed.";
  }
}

function count(rows) {
  if (rows == null) return "done";
  return `${rows.toLocaleString("en-US")} row${rows === 1 ? "" : "s"}`;
}

/** An SQL identifier, quoted when it must be. */
export function quoteIdent(name) {
  return /^[a-z_][a-z0-9_]*$/.test(name) && !RESERVED.has(name) ? name : `"${name.replaceAll('"', '""')}"`;
}

/** An SQL string literal. */
export function quoteString(text) {
  return `'${text.replaceAll("'", "''")}'`;
}

// Words that cannot be bare table names.
const RESERVED = new Set([
  "all", "and", "as", "asc", "by", "case", "cast", "create", "cross", "desc", "distinct", "drop", "else", "end",
  "except", "from", "full", "group", "having", "in", "inner", "intersect", "into", "is", "join", "left", "like",
  "limit", "natural", "not", "null", "offset", "on", "or", "order", "outer", "right", "select", "table", "then",
  "union", "using", "when", "where", "with",
]);

/** A short-lived note at the foot of a panel (role=status, so it is read). */
export function makeNote(container) {
  const note = document.createElement("div");
  note.className = "panel-note";
  note.setAttribute("role", "status");
  container.append(note);
  let timer = null;
  return (text, kind = "info") => {
    clearTimeout(timer);
    note.textContent = text;
    note.dataset.kind = kind;
    note.classList.toggle("shown", Boolean(text));
    if (text && kind !== "error") timer = setTimeout(() => note.classList.remove("shown"), 6000);
  };
}

/** Bytes, for people: 1.2 MB. */
export function formatBytes(bytes) {
  if (bytes == null) return "";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}
