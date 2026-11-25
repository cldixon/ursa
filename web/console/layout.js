// The console's layout (design/console.md §6): three columns, sources and
// catalog | the editor over the results | the inspector over History, each
// panel a mount point (`panels.editor` and the rest: the panel's body) for
// the issue that fills it.
//
// - The splitters (role="separator") resize the side columns and share each
//   column between its two panels: drag one, or focus it and use the arrow
//   keys (Shift for bigger steps), Home and End; Enter folds a side column
//   away and back; a double-click puts it back as it was at first.
// - Each panel's title folds it to its header, and the panel beside it takes
//   the room.
// - The header's two buttons show and hide the side columns. Under 1024px
//   they are drawers over the editor, one at a time, that Esc closes.
// - F6 and Shift+F6 move the keyboard from panel to panel, and the skip
//   links to the editor, the results, the sources, the inspector and the
//   status bar, opening a closed column first.
//
// The sizes and what is folded are kept in this browser (localStorage).

const KEY = "ursa-console-layout";
const NARROW = matchMedia("(max-width: 1023px)");
const DEFAULTS = {
  left: 272, // px
  right: 300,
  rows: { left: 0.5, main: 0.42, right: 0.5 }, // the first panel's share
  open: { left: true, right: true },
  folded: {},
};
const WIDTH = { min: 200, max: 560, step: 16, bigStep: 64 };
const SHARE = { min: 0.12, max: 0.88, step: 0.04, bigStep: 0.16 };
/** Each column's panels, top first. */
const COLUMNS = { left: ["sources", "catalog"], main: ["editor", "results"], right: ["inspector", "history"] };

const $ = (id) => document.getElementById(id);

function load() {
  try {
    const saved = JSON.parse(localStorage.getItem(KEY) ?? "null");
    if (saved && typeof saved === "object")
      return { ...DEFAULTS, ...saved, rows: { ...DEFAULTS.rows, ...saved.rows }, open: { ...DEFAULTS.open, ...saved.open }, folded: { ...saved.folded } };
  } catch {}
  return structuredClone(DEFAULTS);
}

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

export function setupLayout() {
  const root = $("console");
  const state = load();
  // On a narrow window, the drawer that is open ("left", "right" or null).
  let drawer = null;

  const save = () => {
    try {
      localStorage.setItem(KEY, JSON.stringify(state));
    } catch {}
  };
  /** A side column's widest: under half the window, with room for the editor. */
  const maxWidth = () => clamp(Math.min(WIDTH.max, root.clientWidth * 0.4), WIDTH.min, WIDTH.max);
  /** Whether a side column shows: open on a wide window, its drawer on a narrow one. */
  const shown = (side) => (NARROW.matches ? drawer === side : state.open[side]);

  function apply() {
    const narrow = NARROW.matches;
    root.dataset.narrow = narrow ? "true" : "false";
    for (const side of ["left", "right"]) {
      const column = $(side);
      const visible = shown(side);
      column.hidden = !visible;
      root.style.setProperty(`--${side}-width`, `${clamp(state[side], WIDTH.min, maxWidth())}px`);
      $(`toggle-${side}`).setAttribute("aria-expanded", String(visible));
      const splitter = root.querySelector(`[data-split="${side}-width"]`);
      splitter.hidden = narrow || !visible;
      const width = Math.round(clamp(state[side], WIDTH.min, maxWidth()));
      splitter.setAttribute("aria-valuemin", String(WIDTH.min));
      splitter.setAttribute("aria-valuemax", String(Math.round(maxWidth())));
      splitter.setAttribute("aria-valuenow", String(width));
      splitter.setAttribute("aria-valuetext", `${width} pixels wide`);
    }
    root.querySelector(".scrim").hidden = !(narrow && drawer);
    for (const [column, [first, second]] of Object.entries(COLUMNS)) {
      const share = clamp(state.rows[column], SHARE.min, SHARE.max);
      const a = state.folded[first];
      const b = state.folded[second];
      for (const [name, folded, grow] of [
        [first, a, share],
        [second, b, 1 - share],
      ]) {
        const panel = $(name);
        panel.classList.toggle("folded", Boolean(folded));
        // Both folded: the second keeps the room, its body still folded.
        panel.style.flexGrow = folded ? "0" : a || b ? "1" : String(grow);
        $(`${name}-title`).setAttribute("aria-expanded", String(!folded));
        $(`${name}-body`).hidden = Boolean(folded);
      }
      const splitter = root.querySelector(`[data-split="${column}"]`);
      splitter.hidden = Boolean(a || b);
      const percent = Math.round(share * 100);
      splitter.setAttribute("aria-valuemin", String(Math.round(SHARE.min * 100)));
      splitter.setAttribute("aria-valuemax", String(Math.round(SHARE.max * 100)));
      splitter.setAttribute("aria-valuenow", String(percent));
      splitter.setAttribute("aria-valuetext", `${label(first)} ${percent}%, ${label(second)} ${100 - percent}%`);
    }
  }
  const label = (name) => $(`${name}-title`).textContent.trim();

  /** Show or hide a side column (a drawer on a narrow window). */
  function toggle(side, open = !shown(side)) {
    if (NARROW.matches) drawer = open ? side : drawer === side ? null : drawer;
    else state.open[side] = open;
    save();
    apply();
  }

  for (const side of ["left", "right"]) {
    $(`toggle-${side}`).addEventListener("click", () => {
      const open = !shown(side);
      toggle(side, open);
      // Into the drawer it opened, where the keyboard can reach it.
      if (open && NARROW.matches) $(COLUMNS[side][0]).focus();
    });
  }
  root.querySelector(".scrim").addEventListener("click", () => {
    drawer = null;
    apply();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && NARROW.matches && drawer && !e.defaultPrevented) {
      const side = drawer;
      drawer = null;
      apply();
      $(`toggle-${side}`).focus();
    }
    if (e.key === "F6") {
      e.preventDefault();
      cycle(e.shiftKey ? -1 : 1);
    }
  });
  NARROW.addEventListener("change", () => {
    drawer = null;
    apply();
  });
  addEventListener("resize", () => apply());

  // Fold a panel to its header.
  for (const names of Object.values(COLUMNS))
    for (const name of names)
      $(`${name}-title`).addEventListener("click", () => {
        state.folded[name] = !state.folded[name];
        save();
        apply();
      });

  for (const splitter of root.querySelectorAll(".splitter")) resizer(splitter);

  function resizer(splitter) {
    const split = splitter.dataset.split;
    const side = split.endsWith("-width") ? split.slice(0, -"-width".length) : null;
    const set = (value) => {
      if (side) state[side] = clamp(value, WIDTH.min, maxWidth());
      else state.rows[split] = clamp(value, SHARE.min, SHARE.max);
      apply();
    };
    const get = () => (side ? clamp(state[side], WIDTH.min, maxWidth()) : clamp(state.rows[split], SHARE.min, SHARE.max));
    splitter.addEventListener("keydown", (e) => {
      const limits = side ? WIDTH : SHARE;
      const step = e.shiftKey ? limits.bigStep : limits.step;
      // The left column grows to the right; the right one to the left; a
      // column's first panel downwards.
      const grow = side === "right" ? ["ArrowLeft"] : side ? ["ArrowRight"] : ["ArrowDown"];
      const shrink = side === "right" ? ["ArrowRight"] : side ? ["ArrowLeft"] : ["ArrowUp"];
      if (grow.includes(e.key)) set(get() + step);
      else if (shrink.includes(e.key)) set(get() - step);
      else if (e.key === "Home") set(side ? WIDTH.min : SHARE.min);
      else if (e.key === "End") set(side ? maxWidth() : SHARE.max);
      else if (e.key === "Enter" && side) {
        toggle(side, false);
        $(`toggle-${side}`).focus();
      } else return;
      e.preventDefault();
      save();
    });
    splitter.addEventListener("dblclick", () => {
      set(side ? DEFAULTS[side] : DEFAULTS.rows[split]);
      save();
    });
    splitter.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      splitter.setPointerCapture(e.pointerId);
      splitter.classList.add("dragging");
      document.body.classList.add(side ? "resizing-columns" : "resizing-rows");
      const column = side ? null : splitter.parentElement;
      const move = (m) => {
        const box = (column ?? root).getBoundingClientRect();
        if (side === "left") set(m.clientX - box.left);
        else if (side === "right") set(box.right - m.clientX);
        else set((m.clientY - box.top) / box.height);
      };
      const up = () => {
        splitter.releasePointerCapture(e.pointerId);
        splitter.classList.remove("dragging");
        document.body.classList.remove("resizing-columns", "resizing-rows");
        splitter.removeEventListener("pointermove", move);
        save();
      };
      splitter.addEventListener("pointermove", move);
      splitter.addEventListener("pointerup", up, { once: true });
      splitter.addEventListener("pointercancel", up, { once: true });
    });
  }

  /** Bring `name`'s panel into view: its column shown, the panel unfolded. */
  function reveal(name) {
    const side = Object.entries(COLUMNS).find(([, names]) => names.includes(name))?.[0];
    if (side && side !== "main" && !shown(side)) toggle(side, true);
    if (state.folded[name]) {
      state.folded[name] = false;
      save();
      apply();
    }
  }

  /** F6: the next panel shown (or the status bar), from the one with the keyboard. */
  function cycle(direction) {
    const order = [...COLUMNS.left, ...COLUMNS.main, ...COLUMNS.right, "status"].filter((name) => $(name).offsetParent !== null);
    const at = order.findIndex((name) => $(name).contains(document.activeElement));
    const next = order[(at + direction + order.length) % order.length] ?? order[0];
    $(next).focus();
  }

  for (const link of document.querySelectorAll(".skip-links a")) {
    link.addEventListener("click", (e) => {
      const name = link.getAttribute("href").slice(1);
      e.preventDefault();
      reveal(name);
      $(name).focus();
    });
  }

  apply();

  return {
    /** Each panel's mount point: its body, where its issue puts its UI. */
    panels: Object.fromEntries(Object.values(COLUMNS).flat().map((name) => [name, $(`${name}-body`)])),
    reveal,
    toggle,
    state: () => structuredClone({ ...state, drawer, narrow: NARROW.matches }),
  };
}
