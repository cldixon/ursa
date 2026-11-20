# Measurements for the Graph Explorer design

Throwaway scripts behind the numbers in [`../graph-explorer.md`](../graph-explorer.md). They are not tests and not a benchmark harness (that is the viz benchmark's job); each answers one design question.

Build and serve the shell (`cd web && bun run build && bun scripts/preview.ts 8800`), then, with Playwright available to node (`NODE_PATH` set to where it is installed):

| script | question |
|---|---|
| `engine-point.cjs` | How fast are `hop`, bound `GRAPH_TABLE` matches, shortest paths and property lookups in the wasm engine? |
| `induced.cjs` | What do the edges among a set of nodes cost through SQL today? |
| `view-cost.cjs` | For 1K to 1M edges: `.graph`'s timings, one redraw, one pick. |
| `handoff.cjs` | Copies in and out of wasm, `createView` on the page's thread, the wasm heap. |
| `rules-today.cjs` | What `.graph` draws after each kind of result. |
| `coi-proxy.ts` | Does the shell run cross-origin isolated? (`bun coi-proxy.ts 8801`, then open `:8801`.) |

All were run in headless Chromium 153 on 4 cores with software WebGL2 (SwiftShader) and no WebGPU adapter, in November 2026.
