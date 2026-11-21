// What the Graph view costs to use (#430), kept by graph.js as it draws and
// read through window.ursaShell.graphTimings() (app.js): frame times, pick
// times, each action's time from its input to the frame that showed it, and
// the page's long tasks.
//
// Keeping them costs a frame three clock reads, a timer and at most two
// writes into arrays made once: nothing is allocated per frame or per pick,
// and nothing is sorted or summed until something reads them. An action allocates one small object,
// and only the last ACTIONS are kept.

/** How many frames and picks the percentiles are over. */
export const WINDOW = 120;
/** How many actions and long tasks are kept. */
const ACTIONS = 32;
const LONG_TASKS = 64;
/**
 * A frame that input asked for (a drag, a wheel, a key) and that began no
 * later than this after the page's thread was free of the last frame is the
 * view moving, and the time from that frame's start to this one's is a frame
 * interval. A frame after a longer rest is not: the wait before it was the
 * user's, not the view's. A held key repeats about every 33 ms.
 */
const MOVING_MS = 100;

/** The last `size` numbers pushed, with no allocation after the first. */
class Ring {
  constructor(size) {
    this.values = new Float64Array(size);
    this.count = 0;
  }
  push(v) {
    this.values[this.count++ % this.values.length] = v;
  }
  get length() {
    return Math.min(this.count, this.values.length);
  }
  /** The last value pushed, or null. */
  last() {
    return this.count ? this.values[(this.count - 1) % this.values.length] : null;
  }
  /** The values held, oldest first. */
  toArray() {
    const n = this.length;
    const start = this.count - n;
    return Array.from({ length: n }, (_, i) => this.values[(start + i) % this.values.length]);
  }
  clear() {
    this.count = 0;
  }
}

/** {count, p50, p95, max} of `values` (ms, to a hundredth), or count 0 and nulls. */
export function summary(values, total = values.length) {
  if (!values.length) return { count: total, p50: null, p95: null, max: null };
  const sorted = Float64Array.from(values).sort();
  // Nearest rank: the smallest value with at least p of them at or under it.
  const at = (p) => round(sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1))]);
  return { count: total, p50: at(0.5), p95: at(0.95), max: round(sorted[sorted.length - 1]) };
}
const round = (ms) => Math.round(ms * 100) / 100;

const renders = new Ring(WINDOW);
const intervals = new Ring(WINDOW);
const picks = new Ring(WINDOW);
// When the last frame began, when the page's thread was free of it, and
// whether input asked for the next.
//
// A frame is not over when render() returns. With no GPU the browser
// rasterises it on the page's thread before that thread takes another task
// (4 s for a million edges), and with one it is over at once. So a frame's
// end is when the next task runs: a timer set as it is drawn, which is also
// when the actions it shows are shown.
let frameStart = 0;
let frameFree = -Infinity;
let asked = false;
// A frame is being painted and its timer has yet to say the thread is free
// (DREW if graph.js drew it); how many of the waiting actions it shows; and
// whether a timer is set.
const PAINTED = 1;
const DREW = 2;
let busy = 0;
let showing = 0;
let timer = false;
const onTimer = () => {
  timer = false;
  if (busy) freed(performance.now());
};

// Actions shown, oldest first, and those waiting for their frame.
const shown = [];
let waiting = [];

/**
 * Input asked for a frame (a pan, a zoom, a fit): not the layout's new
 * positions, whose pace is the layout's.
 */
export function frameAsked() {
  asked = true;
}

/** A frame was drawn: `render()` ran from `start` to `end` (performance.now()). */
export function frameDrawn(start, end) {
  // No task ran since the last frame: the thread was busy with it until now.
  if (busy) freed(start);
  renders.push(end - start);
  if (asked && start - frameFree <= MOVING_MS) intervals.push(start - frameStart);
  asked = false;
  frameStart = start;
  painted(DREW);
}

/**
 * The browser is painting a frame now, with everything that waits to be
 * shown: one graph.js drew, or (for an action that draws nothing, as a click
 * that names a node in the header) the next animation frame.
 */
export function painted(how = PAINTED) {
  busy = Math.max(busy, how);
  showing = waiting.length;
  if (!timer) {
    timer = true;
    setTimeout(onTimer, 0);
  }
}

/**
 * The page's thread was free of the last frame at `now`: the actions it
 * showed were shown. Set off by the frame's timer.
 */
export function freed(now) {
  // A frame nothing was drawn in says nothing of the view's pace.
  if (busy === DREW) frameFree = now;
  busy = 0;
  if (!showing) return;
  for (let i = 0; i < showing; i++) {
    const a = waiting[i];
    a.frame = now - a.at;
    shown.push(a);
  }
  waiting = waiting.slice(showing);
  showing = 0;
  if (shown.length > ACTIONS) shown.splice(0, shown.length - ACTIONS);
}

/** A pick took `ms`. */
export function picked(ms) {
  picks.push(ms);
}

/**
 * An action began: `command` names it (`.graph karate`, `click`, `key +`),
 * and `at` is when its input arrived (an event's timeStamp, or
 * performance.now()). The returned record takes `engine` and `delta` (ms in
 * the engine, and ms applying its answer to the view) as they become known;
 * the next frame fills `frame`, the ms from `at` to when the page's thread
 * was free of that frame.
 */
export function action(command, at = performance.now()) {
  const a = { command, at, engine: null, delta: null, frame: null };
  waiting.push(a);
  return a;
}

/** An action that will have no frame (its drawing failed, or was replaced). */
export function drop(a) {
  const i = waiting.indexOf(a);
  if (i < 0) return;
  waiting.splice(i, 1);
  if (i < showing) showing--;
}

// Long tasks on the page's thread: [start, duration] in two rings. The
// observer is the browser's; an entry costs two writes.
const longStart = new Ring(LONG_TASKS);
const longDuration = new Ring(LONG_TASKS);
try {
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) {
      longStart.push(e.startTime);
      longDuration.push(e.duration);
    }
  }).observe({ type: "longtask", buffered: true });
} catch {
  // No long-task timing in this browser: longTasks stays empty.
}

/**
 * The timings, for graphTimings(): times in ms, those on the page's clock
 * given from `origin` (the last `.graph`'s Enter).
 *
 * - `frames`: `count` frames drawn since the page loaded; `render`, the
 *   time in `view.render()` of the last 120; `interval`, the time from one
 *   frame's start to the next's of the last 120 drawn while the view was
 *   moving. Each is {count, p50, p95, max}.
 * - `pick`: {count, last, p95} of the last 120 picks.
 * - `actions`: the last 32 shown, oldest first: {command, at, engine, delta,
 *   frame}; `engine` and `delta` are null for an action that needs neither,
 *   and `frame` is to when the page's thread was free of the frame.
 * - `longTasks`: the page's last 64 tasks over 50 ms, {start, duration}.
 */
export function read(origin = 0) {
  const pick = summary(picks.toArray(), picks.count);
  const starts = longStart.toArray();
  const durations = longDuration.toArray();
  return {
    frames: { count: renders.count, render: summary(renders.toArray()), interval: summary(intervals.toArray()) },
    pick: { count: pick.count, last: picks.count ? round(picks.last()) : null, p95: pick.p95 },
    actions: shown.map((a) => ({
      command: a.command,
      at: Math.round(a.at - origin),
      engine: a.engine === null ? null : round(a.engine),
      delta: a.delta === null ? null : round(a.delta),
      frame: round(a.frame),
    })),
    longTasks: starts.map((s, i) => ({ start: Math.round(s - origin), duration: Math.round(durations[i]) })),
  };
}

/** Forget the frames and picks (not the actions or long tasks): a test's clean slate. */
export function reset() {
  renders.clear();
  intervals.clear();
  picks.clear();
  asked = false;
  busy = 0;
  frameFree = -Infinity;
}
