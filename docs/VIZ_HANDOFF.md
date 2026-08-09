# Ursa Viz — Implementation handoff

> **Status:** Implementation handoff for milestone 2 (the visualization layer).
> This document sits between [`VIZ_VISION.md`](VIZ_VISION.md) (the product vision:
> the three tiers, the "render the answer to a query" thesis, competitive framing)
> and the implementation work. It records the decisions, revisions, and technical
> guidance that came out of design review against the current state of the repo.
> Where this document and `VIZ_VISION.md` disagree, **this document wins** — each
> deliberate revision to the vision doc is called out explicitly below.
> [`SPEC.md`](SPEC.md) remains the authority for the core library.

## Context: where the repo stands

Milestone 1 (the core library + docs site) is done and faithful to `SPEC.md`:

- `ursa-core` has the CSR topology with all three load-bearing details (dense
  `u32` indexing via `IdMap`, the `edge_ids` permutation, lazy per-direction
  transpose) and the full v0.1 kernel set, each with weighted variants.
- `ursa-plan` executes every `collect()` as one DataFusion `LogicalPlan` with
  graph ops as real `UserDefinedLogicalNode`s.
- The Python dialect covers composed pipelines, traversals, joins, stats,
  projection pushdown, and object-storage scans; there is an extensive test
  suite including NetworkX cross-validation and determinism-across-thread-count
  tests, plus a benchmark harness and a datasets module.
- The docs site (`site/`, Astro + bun, deployed to ursa.cldixon.dev via
  Cloudflare Workers) already contains an articulated visual design system —
  see §Design tokens below.

There is **no viz code anywhere yet**. Clean slate.

## Product decisions (settled with the project owner)

These came out of design discussion and are not up for re-derivation:

1. **Three surfaces, one renderer.** The inline notebook plot, the docs-site
   "plot this snippet" tab, and the full-screen explorer are three *hosts* of a
   single embeddable TypeScript renderer (working name: **the instrument**) —
   not three implementations. The hosts differ only in chrome and data feed:

   | Surface | Host | Data feed |
   |---|---|---|
   | Notebook inline plot | anywidget in the cell output | embedded Arrow buffers (small graphs); websocket to local server (large) |
   | Docs snippet tab | component in the Astro site | Arrow buffers baked at site build time by running Ursa itself |
   | Explorer | full-page app | live engine over websocket (later: WASM + Parquet range reads) |

   `export_html` is the widget serialized to a self-contained file — same
   artifact, no third implementation.

2. **The notebook cell is interactive by default.** `ur.plot(...)` in a
   notebook returns the live widget (pan/zoom/hover/drag with a springy
   settle), not a static image. A static escape hatch (`static=True` and/or
   `.save_png()`) exists for papers, CI, and headless contexts. The notebook
   mechanism is **anywidget** — one small Python wrapper around the prebuilt JS
   bundle; works identically in Jupyter, JupyterLab, VS Code, Colab, and
   marimo with no per-frontend packaging.

3. **Docs snippets get live visualization tabs.** Where a code example carries
   graph data, the docs page offers a tab that renders it — interactive, in the
   page. The figures' numbers should be *real*: Ursa itself computes them. This
   is both a demo of the library and dogfooding of the renderer.

   **The site build does *not* gain a Python step.** `docs.yml` is deliberately
   decoupled from `ci.yml` ("the docs never gate the code, and the code build
   never gates the docs. Nothing here needs the native extension"), and the
   production deploy is not that workflow at all — it is Cloudflare Workers
   Builds, whose build image would have to grow Python and an installed Ursa.
   Instead: a job in the **code** CI runs Ursa over the snippet data and emits
   Arrow fixtures committed under `site/`, with a staleness check that fails if
   the committed fixtures disagree with freshly generated ones. The site build
   stays pure bun; the figures are still real Ursa output.

   Note what this deletes: `site/src/lib/graph.ts` currently generates a
   Barabási–Albert graph *and computes PageRank in TypeScript* to caption the
   existing figures — a second implementation of the library's own math living
   in the docs. Removing it is worth more than the demo value.

4. **Design tokens: adopt what exists, don't over-index on it.** The owner is
   *not* sold on the current design system's specifics, but wants every
   visualization surface and the site to share **one token set** so a future
   token iteration restyles everything in unison. Practically:
   - The renderer consumes the existing tokens (`site/src/styles/tokens.css`)
     and the stretch/ramp machinery (`site/src/lib/stretch.ts`) **as a
     dependency, not a copy** — factor them into a small shared package (or
     equivalent single source of truth) that both the site and the renderer
     import. Getting this seam right matters more than any current token value.
   - Do not elaborate the astronomical metaphor into new hard-coded visual
     specifics; keep everything themeable through the token layer.

5. **Packaging follows the vision doc.** Core stays lean (only `ur.layout_*`
   kernels belong in core — they're just algorithms). Everything else is
   `ursa-viz` / `pip install ursa-graph[viz]`: the server crate, the prebuilt
   frontend assets (built in CI; users never touch a Node toolchain),
   `ur.plot`, `ur.explore`, `export_html`, the anywidget wrapper. A standalone
   CLI (`ursa explore edges.parquet`) is a later, cheap byproduct — design the
   server so nothing assumes a Python session exists, but don't build the CLI
   first.

   (Note the install line: the distribution is **`ursa-graph`**, not `ursa` —
   the bare `ursa` is an abandoned reservation on PyPI. The vision doc's
   `pip install ursa[viz]` would install the wrong project.)

   **Where the Rust lives, and when.** All the heavy compute is Rust; that is
   not in question. But the *artifacts* arrive on different schedules, and the
   wheel matrix should follow rather than lead:

   | Component | Language | Ships in |
   |---|---|---|
   | `layout_fa2` / `layout_fr` kernels (step 3) | Rust | the existing `ursa-graph` wheel — they are ordinary `ursa-core` algorithms |
   | The instrument: renderer, spring sim, LOD front end (steps 1–2) | TypeScript + WebGL | a prebuilt asset bundle — no compilation |
   | anywidget wrapper, `ur.plot`, `export_html` (steps 1–3) | Python | pure-Python |
   | The explorer server (step 4) | Rust (axum) | see below |

   So steps 1–2 add **no new compiled artifact**: `ursa-viz` is pure Python plus
   a JS bundle, one universal wheel, no build matrix. Step 3's Rust goes into
   the wheel that already exists. Only step 4 introduces new Rust needing
   distribution — and it should go into the **existing `ursa-py` extension
   module** (behind a Cargo feature) rather than spawning a second abi3 wheel
   matrix across five platform legs. There is likely never a second matrix.

## Deliberate revisions to `VIZ_VISION.md`

1. **The browser-side simulation moves to the front of the line.** The vision
   doc says "do not build the GPU/browser simulation first; the server kernel
   is the differentiator." The interactive-by-default decision inverts this for
   the *small-graph* case: drag-a-node-with-springy-settle needs a live force
   simulation in the page (you cannot round-trip drag events to a server at
   60fps, and the docs tabs and exported HTML have no server at all). So the
   first deliverable includes a modest TypeScript spring integrator
   (D3-force-like: no GPU, no WASM; fine to ~10–50k nodes). The Rust
   FA2/Barnes–Hut kernel remains the authoritative layout for real graphs and
   the benchmark headline — the browser sim is for liveness at snapshot scale,
   **seeded from kernel-computed positions when they exist**. Discipline
   required: the two implementations share force-model parameters so a graph
   does not visibly re-organize when it crosses between them.

2. **Drag/pin semantics.** A dragged node becomes temporarily pinned; the
   simulation reheats locally; pins release on double-click (or stay, per
   user). Pins are just two override columns (`x`, `y`) — "positions are
   columns" stays honest under manual arrangement without becoming a
   diagramming tool (still a non-goal).

3. **`ur.plot` is the widget, not matplotlib-first.** The vision doc's open
   question 3 (matplotlib vs native rasterizer) is superseded for the
   interactive path. A static rasterized path is still needed (papers, CI,
   truly edge-heavy stills); note that matplotlib `LineCollection` at millions
   of edges is minutes-slow — expect to want a small Rust edge-accumulating
   rasterizer (datashader-style, over the CSR already in memory) sooner rather
   than later. Matplotlib delegation is acceptable as the v0 static path.

## Sequencing

| Step | Deliverable | Notes |
|---|---|---|
| 1 | **The instrument v0 + anywidget wrapper.** TS renderer: pan/zoom/hover/drag, small-graph spring sim, plate+sky theming, stretch/ramp via the shared token package. `ur.plot` returns it in notebooks. | No server, no Rust changes beyond plumbing data out. Daily-touch surface lands first. |
| 2 | **Docs snippet tabs.** Instantiate the renderer in the Astro site with build-time-computed (real Ursa) Arrow data. | Small, hugely visible, dogfoods library + renderer. |
| 3 | **Rust layout kernels.** `layout_fa2` (Barnes–Hut, LinLog, gravity, weight influence), `layout_fr`, `layout_random`/`layout_circle`; positions as columns; static export path. | The benchmark story ("million-node layout in seconds"). Feeds precomputed positions to every surface. |
| 4 | **The explorer.** axum server, Arrow IPC over websocket, expand/filter/select compiled to expressions, Python round-trip (`session.selection()` / `.highlight()`), then the LOD ladder (sampling → metagraph → drill-down). | The flagship; wire protocol per `VIZ_VISION.md` §Tier 2 architecture. **Depends on #114 and #116** — see §Engine dependencies. |
| 5 | **WASM / publish.** Per `VIZ_VISION.md` §Tier 3, sober notes included there. | |

Steps 1–2 are modest engineering with outsized visibility, and everything they
produce is load-bearing for 3–5.

## Engine dependencies — tracked separately, built in parallel

Design review against the repo surfaced five places where the viz work needs
something the core engine does not do yet. Each is filed as a standalone core
issue and worked in a separate session; **none of them blocks steps 1–2**, so
viz work proceeds concurrently. This is the intended feedback loop — the
visualization layer is the forcing function for engine capability, not a
consumer bolted on the side.

| Issue | What | Blocks | Notes |
|---|---|---|---|
| [#114](https://github.com/cldixon/ursa/issues/114) | **Subgraph views** — graph ops over a filtered edge frame, as a bitmask over the parent CSR | step 4 | The big one. "Drag a filter slider then recolor" *is* a graph op on a filtered edge frame; `_reject_derived_edge_graph_op` rejects it today, and collect-and-re-ingest rebuilds the CSR per tick. Promoted out of "deferred, deliberately" on the roadmap. |
| [#115](https://github.com/cldixon/ursa/issues/115) | **Multi-output kernels + per-query kernel memo** | step 3 | `OutputColumn::Algo` emits exactly one array with no memoization, so `layout_fa2` producing `(x, y)` would run ForceAtlas2 twice. Also fixes the live triangle/clustering double-compute. |
| [#116](https://github.com/cldixon/ursa/issues/116) | **Graph ops on traversal results** (child-plan seeding) | step 4 | The explorer's expand-then-compute loop. Sequenced after #114; may reduce to it. |
| [#117](https://github.com/cldixon/ursa/issues/117) | **`f32` output columns** | step 3 | No kernel can emit `f32` today. Small and self-contained. |
| [#118](https://github.com/cldixon/ursa/issues/118) | **rayon behind a feature flag + wasm32 / `--no-default-features` CI legs** | Tier 3 | Pulled to step 0 — costs nothing now, nothing depends on it, and the cost grows with every kernel added. The CI legs are the real deliverable; the flag rots without them. |

Practical consequence for step 4's scope: if #114 and #116 land first, the
explorer's filter and expand interactions are genuine subgraph queries. If they
have not, step 4's v0 falls back to "algorithms always run on the full graph;
filtering only changes what is rendered" — workable, but the UI then has to say
so, because otherwise the numbers on screen silently describe a different graph
than the one being displayed. Prefer the former; check status before scoping.

## Technical guidance and pre-work guards

- **Take the rayon feature-flag guard before step 3.** The Tier 3 WASM path
  requires `ursa-core` to stay `wasm32-unknown-unknown`-clean with `rayon`
  behind an (on-by-default) feature flag. Today rayon is an unconditional dep
  used across ~9 files. Serial fallbacks largely exist already (the
  byte-identical serial/parallel CSR builds). Cheap now, painful after more
  kernels land. Core is otherwise clean (no tokio, no filesystem deps).

- **Positions are two `f32` columns, not a struct.** The vision doc's
  `struct{x,y}` + `.unnest("pos")` example implies struct-valued expressions
  and `unnest` — neither exists in the dialect. Don't build them for this:
  `layout_*` follows the dual-positioning convention the algorithms already
  use — standalone form returns an `(id, x, y)` NodeFrame; expression form
  yields two columns. Add struct/unnest support only when something else also
  needs it. Dtype: `f32` end-to-end (halves GPU traffic; ample precision for
  screen space); kernels may accumulate in `f64` internally.

- **Layout determinism.** `seed=` at fixed thread count is the standing policy,
  but parallel force accumulation is FP-order-sensitive in a way counting sorts
  aren't. Positions get cached to Parquet as a workflow, so reproducibility is
  more than cosmetic. The repo's existing determinism tests
  (`ursa-core/tests/determinism_threads.rs`, exact `==` across pools of 1–8
  workers) are the pattern to extend. Three separable guarantees — decide each:

  1. **Bit-identical across thread counts, same binary.** *Achievable at little
     or no cost, and the recommendation* — provided two implementation
     constraints are taken from day one rather than retrofitted:
     - **Gather, never scatter.** Parallelize attraction over *nodes* (each node
       sums forces from its own incident edges via CSR) rather than over
       *edges* (each edge pushing force to both endpoints). The scatter form is
       order-dependent; the gather form is not, and it is what every existing
       kernel already does.
     - **Deterministic quadtree.** Parallel point insertion produces an
       order-dependent tree. Build it from a Morton-code sort instead
       (deterministic, and more cache-friendly — plausibly a net speedup), with
       centre-of-mass reductions in fixed child order.

     Given both, repulsion is a per-node sequential traversal of a fixed tree —
     deterministic by construction. FA2 runs a fixed iteration count, so there
     is no convergence-based early exit to introduce ambiguity either.

  2. **Bit-identical across platforms (x86 / aarch64).** *Do not promise this
     for LinLog mode.* Plain FA2 is `+ - * /` and `sqrt`, all IEEE-exact, so it
     can likely hold. LinLog introduces `ln`, which is not exactly specified by
     IEEE-754 and differs across libm implementations — it would be the first
     transcendental inside a determinism-tested kernel. Note this was never
     guaranteed for any kernel (the existing test varies thread count on one
     machine), but shared Parquet position caches make it newly visible.

  3. **Perceptual stability.** Worth adding regardless of 1 and 2, because it
     catches what bit-equality on a small test graph cannot: that a refactor
     didn't quietly change what the layout *looks like*. Test as Procrustes-
     aligned RMS position error below a threshold — layouts are invariant under
     rotation, reflection and translation, so raw coordinate comparison between
     two runs is meaningless without alignment.

- **The query compiler lives in Python, not Rust.** `python/ursa/_execute.py` is
  ~1500 lines of IR construction; the PyO3 entry points (`run_node_query` and
  friends) take *already-lowered* JSON IR plus parameters. So "the server
  compiles interaction messages → Ursa expressions → engine" and "design the
  server so nothing assumes a Python session exists" are in tension with where
  the compiler currently sits. Three options: reimplement lowering in Rust
  (duplication, guaranteed drift); have axum call back into Python (GIL
  contention on the tokio runtime — directly undercuts the GIL-released-during-
  compute property); or have the Rust server serve a small **closed vocabulary**
  of interaction shapes built directly against `ursa-plan`. Take the third, and
  state the consequence plainly: at step 4 the sidebar's knobs are a fixed
  vocabulary, not arbitrary user expressions. "Every knob is an expression" is
  true of the architecture well before it is true of the product.

- **The two force models will not agree.** Revision 1 above asks the TS spring
  integrator and the Rust FA2 kernel to "share force-model parameters so a graph
  does not visibly re-organize." FA2 with LinLog and Barnes–Hut approximation
  and a D3-style Verlet integrator will not converge to the same configuration
  no matter how the constants are matched. The achievable form of the same
  invariant: when kernel positions exist, the browser sim **never re-lays-out**
  — it runs constrained, global positions pinned, with a local reheat confined
  to a neighbourhood around the dragged node. Same user-visible property, and
  it is actually buildable.

- **Barnes–Hut is the one genuinely new computational shape.** Every existing
  kernel is a CSR sweep in one of the spec's four shapes; FA2 repulsion adds a
  spatial quadtree rebuilt per iteration. Well-trodden (the FA2 paper, Gephi's
  implementation, existing Rust crates as prior art) but there is no GAP
  reference implementation to port — budget accordingly.

- **Interaction latency (step 4).** The server holds `Arc` refs to live
  frames, so the topology index survives across interactions per the
  index-preservation contract — hops and recolors pay no rebuild. If
  per-interaction DataFusion plan build/optimize overhead becomes the tail on
  tiny incremental queries, cache plans for the fixed interaction shapes
  (expand/filter/recolor).

- **Wire protocol.** As specified in `VIZ_VISION.md` §Tier 2: Arrow IPC over
  websocket, dense `u32` render indices (the topology index's internal id
  space — user IDs stay server-side except labels/tooltips fetched on hover),
  JSON only for control messages. Version the protocol from day one.

- **Renderer stack.** Undecided in the vision doc (open question 1); the
  protocol is stack-agnostic. Lean: hand-rolled WebGL2 or regl for v0 — the
  render needs (instanced points + lines + text sprites) are small; wgpu-in-
  WASM buys little until Tier 3 wants shared Rust rendering code. The
  renderer must render acceptably from a plain 2D-canvas fallback for tiny
  graphs if that meaningfully simplifies v0 — but don't build two render
  paths without need.

## Design-system pointers (for step 1)

The existing system to adopt (and factor into the shared package):

- `site/src/styles/tokens.css` — two grounds ("plate" = paper/ink for docs and
  static figures; "sky" = inverted, for the live instrument; `[data-ground]`
  re-points semantic tokens), the temperature ramp (the only sequential color
  scale), flat greys for categorical, serif + mono type.
- `site/src/lib/stretch.ts` — asinh/log/linear stretch functions with
  per-algorithm defaults (asinh for pagerank/betweenness, log for
  degree/counts, linear for positions). Port semantics verbatim; this is the
  size/brightness mapping.
- `site/src/components/SkyField.astro` — the "detection" affordance (ellipse +
  catalog id + leader line) that hover/selection should echo.

Also worth knowing before step 1: **the site ships zero client-side JavaScript
today.** No framework integration in `astro.config.mjs`, no `<script>` blocks,
no `client:` directives, `syntaxHighlight: false`. Step 2 adds the first
hydrated island to a site whose entire character is static ink on paper. Not a
blocker, but every docs page must still read completely if the tab never loads.

**Known open design problem — categorical color.** The token system mandates
flat greys for categorical channels (correct: the ramp implies order) but has
only four greys, and "color by community" appears to need dozens of
distinguishable categories.

Reframe before solving: a legible view never shows dozens of categories. It
shows the top-k communities by size plus an "other" bucket — k of roughly 8–12.
So the tractable version of the ask is *"a categorical palette of k ≤ 12
designed for the sky ground, with everything past k collapsing to a neutral,"*
which is an ordinary design problem rather than an open-ended one. Surface it
when it bites (explorer work, step 4), propose through the token layer
(categorical palette for the sky ground; communities as spatial
regions/contours; hue-only-with-legend in the live instrument), and get an owner
decision rather than shipping an ad-hoc palette.

## Open questions (carry forward, decide in-flight)

1. Renderer stack final call (WebGL2 vs regl vs wgpu) — decide at step 1
   start; keep the protocol stack-agnostic.
2. Live-layout tick streaming rate control (server-paced vs client-requested)
   — decide at step 4.
3. Categorical color at instrument scale — see above; owner decision needed.
4. Exact shape of the shared token package (npm workspace package in `site/`?
   a `viz/` top-level dir owning it with the site importing from it?) — decide
   at step 1; the requirement is only *one source of truth consumed by both*.
5. Where the frontend source lives (`site/` toolchain family vs. a new
   top-level `viz-frontend/`) — the constraint is that the wheel ships
   prebuilt assets and the site can import the renderer as a component.
