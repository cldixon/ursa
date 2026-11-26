// The statements in the editor's text, for running them one at a time: SQL
// statements end at a `;` outside strings, quoted names, dollar quotes and
// comments; a dot-command (`.tables`, `.graph karate`) is a line of its own
// where a statement starts, as the shell reads them (ursa-shell's
// highlight.rs and its line reader). Offsets are UTF-16, as the editor's.
//
//   statements("SELECT 1; .tables\nSELECT 'a;b'")
//   // [{from: 0, to: 9, text: "SELECT 1", command: false},
//   //  {from: 10, to: 17, text: ".tables", command: true},
//   //  {from: 18, to: 30, text: "SELECT 'a;b'", command: false}]
//
// `from` is where the statement's first word is (comments before it are
// left out), `to` is past its `;` when it has one; `text` is what runs,
// without the `;`. A statement of comments alone is none.

/** The statements of `text`: [{from, to, text, command}]. */
export function statements(text) {
  const out = [];
  const n = text.length;
  let i = 0;
  /** Where the statement being read has its first word, or -1 before it. */
  let start = -1;
  const end = (stop, after) => {
    if (start >= 0) {
      const body = text.slice(start, stop).replace(/\s+$/, "");
      if (body) out.push({ from: start, to: after, text: body, command: false });
    }
    start = -1;
  };
  while (i < n) {
    const c = text[i];
    const next = text[i + 1];
    if (start < 0 && /\s/.test(c)) {
      i++;
      continue;
    }
    // Comments: before a statement's first word they are not part of it.
    if (c === "-" && next === "-") {
      const eol = text.indexOf("\n", i);
      i = eol < 0 ? n : eol;
      continue;
    }
    if (c === "/" && next === "*") {
      i = skipBlockComment(text, i);
      continue;
    }
    if (start < 0) {
      // A dot-command: where a statement starts, a line of its own.
      if (c === "." && /[A-Za-z]/.test(next ?? "")) {
        const eol = text.indexOf("\n", i);
        const stop = eol < 0 ? n : eol;
        const line = text.slice(i, stop).replace(/\s+$/, "");
        out.push({ from: i, to: i + line.length, text: line, command: true });
        i = stop;
        continue;
      }
      start = i;
    }
    if (c === ";") {
      end(i, i + 1);
      i++;
      continue;
    }
    if (c === "'") {
      // E'…' takes backslash escapes; '' is a quote in any string.
      const escapes = /[eE]/.test(text[i - 1] ?? "") && !/[\w$]/.test(text[i - 2] ?? "");
      i = skipQuoted(text, i, "'", escapes);
      continue;
    }
    if (c === '"') {
      i = skipQuoted(text, i, '"', false);
      continue;
    }
    if (c === "$" && !/[\w$]/.test(text[i - 1] ?? "")) {
      const tag = text.slice(i).match(/^\$([A-Za-z_][\w]*)?\$/);
      if (tag) {
        const close = text.indexOf(tag[0], i + tag[0].length);
        i = close < 0 ? n : close + tag[0].length;
        continue;
      }
    }
    i++;
  }
  end(n, n);
  // A statement's `to` without its `;` is its last character's end.
  for (const s of out) if (!s.command && text[s.to - 1] !== ";") s.to = s.from + s.text.length;
  return out;
}

function skipQuoted(text, i, quote, escapes) {
  const n = text.length;
  let j = i + 1;
  while (j < n) {
    const c = text[j];
    if (escapes && c === "\\") j += 2;
    else if (c === quote && text[j + 1] === quote) j += 2;
    else if (c === quote) return j + 1;
    else j++;
  }
  return n;
}

function skipBlockComment(text, i) {
  const n = text.length;
  let depth = 0;
  let j = i;
  while (j < n) {
    if (text[j] === "/" && text[j + 1] === "*") {
      depth++;
      j += 2;
    } else if (text[j] === "*" && text[j + 1] === "/") {
      depth--;
      j += 2;
      if (depth === 0) return j;
    } else j++;
  }
  return n;
}

/**
 * The statement at `pos` in `list` (statements()'s): the one it is in; else,
 * after a statement on the same line (past its `;`), that one; else the
 * next; else the last.
 */
export function statementAt(list, pos, text) {
  if (!list.length) return null;
  for (const s of list) if (pos >= s.from && pos <= s.to) return s;
  for (let k = 0; k < list.length; k++) {
    const s = list[k];
    const next = list[k + 1];
    if (pos > s.to && (!next || pos < next.from)) {
      if (!next || !text.slice(s.to, pos).includes("\n")) return s;
      return next;
    }
  }
  return list[0];
}

