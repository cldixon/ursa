// Which build of the layout a worker loads (#503): the threaded one only
// where the page is cross-origin isolated (so `SharedArrayBuffer` exists and
// a shared `WebAssembly.Memory` can be posted to a worker), else the
// unthreaded build, which is the shell's as it ships.
//
// `threads` is how many workers the pool gets: every core but one (the
// page's thread draws), at most `MAX_THREADS`.
export const MAX_THREADS = 8;

export function pickBuild(scope = globalThis) {
  const cores = scope.navigator?.hardwareConcurrency ?? 1;
  const isolated = scope.crossOriginIsolated === true && typeof scope.SharedArrayBuffer === "function";
  if (!isolated || cores < 2) return { build: "serial", threads: 1, isolated };
  return { build: "threads", threads: Math.max(2, Math.min(MAX_THREADS, cores - 1)), isolated };
}

/**
 * Load the layout: `{ module, wasm, build, threads, initMs, poolMs }`.
 * `want` overrides the choice (`{ build, threads }`), for the measurement.
 * If the threaded build fails to start, the unthreaded one is loaded.
 */
export async function loadLayout(want = {}) {
  const picked = pickBuild();
  let build = want.build ?? picked.build;
  let threads = want.threads ?? picked.threads;
  if (build === "threads" && !picked.isolated) build = "serial";
  let fellBack = null;
  for (;;) {
    try {
      const t0 = performance.now();
      const module = await import(`./pkg-${build}/ursa_viz_threads.js`);
      const wasm = await module.default();
      const t1 = performance.now();
      if (build === "threads") await module.initThreadPool(threads);
      else threads = 1;
      const t2 = performance.now();
      return { module, wasm, build, threads, initMs: t1 - t0, poolMs: t2 - t1, isolated: picked.isolated, fellBack };
    } catch (e) {
      if (build !== "threads") throw e;
      fellBack = String(e);
      build = "serial";
    }
  }
}
