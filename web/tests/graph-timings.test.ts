// graph-timings.js (#430), the numbers behind window.ursaShell.graphTimings():
//
//   bun test tests/graph-timings.test.ts
import { expect, test } from "bun:test";
// @ts-ignore: a plain module of the page's.
import * as timings from "../public/graph-timings.js";

/** A frame drawn from `start` to `end`, the page's thread free of it `after` ms later. */
function frame(start: number, end: number, after = 0) {
  timings.frameDrawn(start, end);
  timings.freed(end + after);
}

test("a summary is nearest-rank percentiles, to a hundredth of a ms", () => {
  expect(timings.summary([])).toEqual({ count: 0, p50: null, p95: null, max: null });
  expect(timings.summary([7])).toEqual({ count: 1, p50: 7, p95: 7, max: 7 });
  const hundred = Array.from({ length: 100 }, (_, i) => 100 - i); // 100 … 1
  expect(timings.summary(hundred)).toEqual({ count: 100, p50: 50, p95: 95, max: 100 });
  expect(timings.summary([1.234, 10.006, 2.5], 9)).toEqual({ count: 9, p50: 2.5, p95: 10.01, max: 10.01 });
});

test("frames: render() of every frame, intervals only while input keeps the view moving", () => {
  timings.reset();
  // The layout's frames: no input asked, so no interval however close.
  frame(1000, 1004);
  frame(1016, 1021);
  let f = timings.read().frames;
  expect(f.render).toEqual({ count: 2, p50: 4, p95: 5, max: 5 });
  expect(f.interval.count).toBe(0);
  // A key after a rest: a frame, but not movement.
  timings.frameAsked();
  frame(5000, 5003);
  expect(timings.read().frames.interval.count).toBe(0);
  // A wheel: each frame asked for by input and begun just after the last ended.
  for (const start of [5016, 5032, 5082]) {
    timings.frameAsked();
    timings.frameAsked(); // several events a frame
    frame(start, start + 6);
  }
  f = timings.read().frames;
  expect(f.interval).toEqual({ count: 3, p50: 16, p95: 50, max: 50 });
  expect(f.render.count).toBe(6);
  // A frame the browser rasterises on the page's thread for 4 s after
  // render() returned is still one interval: the next began as soon as the
  // thread was free.
  timings.frameAsked();
  frame(5090, 5092, 3900);
  timings.frameAsked();
  frame(9010, 9020);
  expect(timings.read().frames.interval.max).toBe(3920);
  expect(timings.read().frames.render.max).toBe(10);
  // And with no task between two frames, the thread was busy until the second.
  timings.frameAsked();
  timings.frameDrawn(9500, 9501);
  timings.frameAsked();
  frame(12000, 12001);
  expect(timings.read().frames.interval).toEqual({ count: 6, p50: 16, p95: 3920, max: 3920 });
  // A click's frame, in which nothing is drawn, is not the view moving: a
  // key in the same frame, after a rest, starts no interval.
  timings.action("click", 20000);
  timings.painted();
  timings.frameAsked();
  frame(20010, 20011);
  expect(timings.read().frames.interval.count).toBe(6);
  // A frame the layout asked for between two of the user's does not count.
  frame(9030, 9031);
  expect(timings.read().frames.interval.count).toBe(6);
});

test("frames and picks keep the last 120, and count them all", () => {
  timings.reset();
  const before = timings.read().frames.count;
  for (let i = 0; i < 300; i++) {
    frame(i * 10, i * 10 + (i < 180 ? 9 : 1));
    timings.picked(i < 299 ? 0.5 : 3);
  }
  const t = timings.read();
  expect(t.frames.count).toBe(before + 300);
  expect(t.frames.render).toEqual({ count: 120, p50: 1, p95: 1, max: 1 });
  expect(t.pick).toEqual({ count: 300, last: 3, p95: 0.5 });
  timings.reset();
  expect(timings.read().pick).toEqual({ count: 0, last: null, p95: null });
});

test("an action is shown by the next frame, with its engine and delta times", () => {
  const shown = () => timings.read(1000).actions;
  const start = shown().length;
  const graph = timings.action(".graph karate", 1000);
  graph.engine = 12.345;
  graph.delta = 30;
  const click = timings.action("click", 1100);
  const dropped = timings.action(".graph rmat", 1150);
  timings.drop(dropped);
  expect(shown().length).toBe(start); // none shown yet
  // The frame is drawn by 1200.5, and the thread is free of it at 1230.5.
  // An action that arrives in between is the next frame's.
  timings.frameDrawn(1190, 1200.5);
  const late = timings.action("key 0", 1210);
  expect(shown().length).toBe(start);
  timings.freed(1230.5);
  expect(shown().slice(start)).toEqual([
    { command: ".graph karate", at: 0, engine: 12.35, delta: 30, frame: 230.5 },
    { command: "click", at: 100, engine: null, delta: null, frame: 130.5 },
  ]);
  frame(1300, 1301);
  expect(shown().at(-1)).toEqual({ command: "key 0", at: 210, engine: null, delta: null, frame: 91 });
  expect(late.frame).toBe(91);
  // An action that draws nothing is shown by the frame the browser paints next.
  timings.action("click", 2000);
  timings.painted();
  timings.freed(2016);
  expect(shown().at(-1)).toEqual({ command: "click", at: 1000, engine: null, delta: null, frame: 16 });
  // One dropped while its frame is being painted is not shown.
  const gone = timings.action(".graph gone", 2100);
  timings.action("key -", 2101);
  timings.painted();
  timings.drop(gone);
  timings.freed(2110);
  expect(shown().at(-1).command).toBe("key -");
  expect(shown().some((a: any) => a.command === ".graph gone")).toBe(false);
  // Only the last 32 are kept.
  for (let i = 0; i < 40; i++) timings.action(`key ${i}`, 3000 + i);
  timings.painted();
  timings.freed(4000);
  const kept = shown();
  expect(kept.length).toBe(32);
  expect(kept[0].command).toBe("key 8");
  expect(Array.isArray(timings.read().longTasks)).toBe(true);
});
