// The Graph view's search (#445): the box `/` opens in the pane's header,
// and the list under it of the matches that are not shown.
//
// The box runs `.find` as the terminal would: Enter types `.find TEXT` (or
// `.find WHERE condition`, when the text starts with WHERE) into the shell,
// so the search is in the terminal and its history, and the engine answers
// it (ursa-engine's graph::view::find). app.js gives the answer to the view
// (graph.js's selectFound: select the matches, fit them) and to show() here.
//
// Plain DOM and no state but what is on screen, so the page loads it with
// app.js and it costs nothing until `/`.

/** `key` as an explore command takes an id: quoted when it has a space or a quote. */
export function idWord(key) {
  return /[\s'"]/.test(key) || key === "" ? `'${key.replace(/'/g, "''")}'` : key;
}

/**
 * What the search box runs for `typed`: `.find TEXT`, or null for nothing
 * typed. A line break would be a second command, so it is a space.
 */
export function findCommand(typed) {
  const text = typed.replace(/\s+/g, " ").trim().replace(/;+$/, "").trim();
  return text ? `.find ${text}` : null;
}

/**
 * Wire the search: `form` and its `input` (the box), `button` (which opens
 * it, for a pointer), `list` (the matches not shown) and `status` (the
 * header's line, which the box covers while it is open). `run(command)` types
 * a command into the shell; `focusView()` puts the focus back on the graph.
 * Gives {open, close, show, clear}.
 */
export function setup({ form, input, button, list, status, run, focusView }) {
  const close = (refocus = true) => {
    if (form.hidden) return;
    const had = form.contains(document.activeElement);
    form.hidden = true;
    status.hidden = false;
    button.setAttribute("aria-expanded", "false");
    if (refocus && had) focusView();
  };
  const open = () => {
    form.hidden = false;
    status.hidden = true;
    button.setAttribute("aria-expanded", "true");
    input.focus();
    input.select();
  };
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const command = findCommand(input.value);
    close();
    if (command) run(command);
  });
  input.addEventListener("keydown", (e) => {
    // The box's own keys: none of them is the pane's (Esc would close it,
    // `f` fit, `/` search again).
    e.stopPropagation();
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  });
  // A click elsewhere closes it, as Esc does, without moving the focus.
  input.addEventListener("blur", () => setTimeout(() => form.contains(document.activeElement) || close(false)));
  button.addEventListener("click", () => (form.hidden ? open() : close()));

  const clear = () => {
    list.replaceChildren();
    list.hidden = true;
  };
  /**
   * The matches the view does not show: `notShown`, [{key, hidden}], the
   * first of `total`. A hidden node has a button that shows it again
   * (`.show KEY`); a node of the graph that was never drawn is named only.
   */
  const show = (notShown, total) => {
    if (!total) return clear();
    const el = (tag, cls, text) => {
      const e = document.createElement(tag);
      if (cls) e.className = cls;
      if (text !== undefined) e.textContent = text;
      return e;
    };
    const items = el("ul");
    items.setAttribute("aria-label", "Matches not shown");
    for (const { key, hidden } of notShown) {
      const li = el("li");
      li.append(el("span", "key", key));
      if (hidden) {
        const add = el("button", "add", "show");
        add.type = "button";
        add.title = `Show node ${key} again (.show ${idWord(key)})`;
        add.setAttribute("aria-label", `Show node ${key} again`);
        add.addEventListener("click", () => {
          li.remove();
          if (!items.children.length) clear();
          // The button is gone: the focus is the graph's, not nowhere.
          focusView();
          run(`.show ${idWord(key)}`);
        });
        li.append(" ", add);
      }
      items.append(li);
    }
    const more = total - notShown.length;
    const dismiss = el("button", "dismiss", "×");
    dismiss.type = "button";
    dismiss.title = "Dismiss";
    dismiss.setAttribute("aria-label", "Dismiss the matches not shown");
    dismiss.addEventListener("click", clear);
    list.replaceChildren(el("span", "head", `Not shown (${total.toLocaleString("en-US")}):`), items, ...(more > 0 ? [el("span", "more", `and ${more.toLocaleString("en-US")} more`)] : []), dismiss);
    list.hidden = false;
  };
  return { open, close, show, clear };
}
