# Ursa Viz — Visualization and interactive exploration for Ursa graphs

> **Status:** Vision and design document. This is the companion to `ursa_spec.md`, describing the visualization product direction. **Nothing in this document affects v0.1 core implementation** — the sole intersection is a note that layout algorithms are ordinary `ursa-core` kernels (§Layout is an algorithm), which requires no changes to the core spec. Sections marked **[IMPL]** are implementation guidance for when each tier is built.

## The thesis: render the answer to a query

Every existing graph visualization tool treats the problem as *"draw the graph."* That framing is why they all fail the same way: a force-directed layout of anything past ~10,000 nodes is a hairball, and no amount of rendering horsepower fixes a picture that shouldn't have been drawn.

Ursa Viz treats visualization as **"render the answer to a query."** What makes this possible — and what no competitor has — is a full lazy analytical engine sitting directly behind the view. In Ursa, every interaction *is* an expression:

| Interaction in the explorer | What it actually is |
|---|---|
| click a node to expand its neighborhood | `ur.hop(edges).from_(clicked_id)` |
| drag a filter slider | `.filter(ur.col("amount") > x)` |
| "color by community" | `ur.louvain(edges)` |
| zoom out on a 100M-edge graph | Louvain coarsening → render the metagraph |
| lasso-select a cluster | a `NodeFrame` handed back to your Python session |

Gephi has no engine. sigma.js and D3 have no engine. Cosmograph renders beautifully but computes nothing. Graphistry has an engine, but it is a GPU server product, not a library in your process. Ursa Viz is the visualization layer that a query engine grows — not a chart library bolted onto one.

Strategically: Polars ships plotting as a courtesy feature (a thin delegation to hvplot). For Ursa, the explorer is the **acquisition funnel** — the shareable, front-page-of-HN artifact that makes someone install the library. Graphs are the one data structure normal humans actively want to look at.

## The three tiers

1. **Tier 1 — Layout as expressions + static rendering.** Layout kernels in Rust, positions as columns, quick static plots. Cheap, early, spectacular benchmark material.
2. **Tier 2 — The live explorer.** `ur.explore(...)`: a local server + WebGL frontend over the live in-memory frames, Arrow all the way to the GPU. The flagship.
3. **Tier 3 — Publish & WASM.** Self-contained shareable HTML exports; ultimately `ursa-core` compiled to WebAssembly reading Parquet straight from object storage — a graph exploration as a static file plus a URL, in the mold of DuckDB-WASM.

---

## Tier 1 — Layout is an algorithm

The founding observation: **a force-directed layout is an iterative fixpoint kernel over CSR** — the same computational shape as PageRank. ForceAtlas2 or Fruchterman–Reingold with Barnes–Hut approximation is: per iteration, a Rayon-parallel sweep over vertex ranges computing attraction along edges and repulsion via a quadtree. It therefore belongs in `ursa-core` beside the other kernels, and it surfaces through the closed frame algebra like everything else — positions are just columns:

```python
import ursa as ur

frame = (
    nodes
    .with_columns(
        pos       = ur.layout_fa2(edges, iterations=300, weight=ur.col("amount"), seed=42),
        pagerank  = ur.pagerank(edges),
        community = ur.louvain(edges),
    )
    .unnest("pos")           # struct{x: f32, y: f32} -> x, y columns
)
```

Because positions are columns, they compose with everything: filter them, join them, `sink_parquet` them (precomputing layout once and caching it in the lake is a *workflow*, not a hack), or feed them to any scatter renderer — matplotlib, datashader, deck.gl, plotly.

A single-command static render exists for the quick look:

```python
ur.plot(
    edges, nodes,
    layout="fa2",                      # or pos=("x", "y") to reuse precomputed
    color=ur.col("community"),
    size=ur.col("pagerank"),
    edge_alpha=0.05,
)                                       # -> matplotlib Figure / PNG / SVG
```

**Why this wins on day one:** D3's force simulation is single-threaded JavaScript; igraph and NetworkX layouts are similarly serial. A Rayon Barnes–Hut kernel over CSR is parallel and O(n log n), which is a different asymptotic class from what the incumbents offer.

**What that is actually worth, measured** (`ursa-core/examples/layout_scaling.rs`, #142). This paragraph used to promise "a million-node graph in seconds". It was written before the kernel existed and it was too generous; these are the numbers instead:

| | |
|---|---|
| Growth per doubling | **~2.1×** — n log n, out to 128K nodes |
| 1M nodes, ForceAtlas2 | **~1.3 s per iteration** on 4 cores |
| 1M nodes, 300 iterations | **~400 s** on 4 cores |

Iteration count is a quality dial rather than a constant, so the per-iteration figure is the honest headline. ~97% of that time is the tree traversal, which is fully parallel — the serial tree build is 2.6% — so the wall clock falls close to linearly with cores: the same work on 32 cores is minutes, not the better part of an hour. "Seconds" at 300 iterations and a million nodes is not a claim this kernel supports on any machine we have measured, and the comparison worth publishing is against NetworkX and igraph in the benchmark harness, not against a round number.

**Algorithms (Tier 1 set):** `layout_fa2` (ForceAtlas2 w/ Barnes–Hut, LinLog mode, gravity) and `layout_fr` (Fruchterman–Reingold) have **landed** (#141, #142), along with `layout_random` / `layout_circle` (seeding and trivial cases). Deterministic under `seed=` and bit-identical across thread counts, same policy as other stochastic kernels — except LinLog mode, whose `ln` is not guaranteed identical across platforms. Still open: **edge-weight influence** on attraction, which needs a weighted undirected adjacency the core does not build yet. Later: UMAP-style embedding projection, hierarchical/DAG layouts (Sugiyama) for lineage graphs.

---

## Tier 2 — The explorer

```python
session = ur.explore(
    edges, nodes,
    color=ur.col("community"),
    size=ur.col("pagerank"),
    label=ur.col("name"),
    weight=ur.col("amount"),
)
# → serving http://localhost:7431  (opens browser; renders inline in Jupyter)
```

One call. The Rust core spins up a local web server, serves a prebuilt WebGL frontend, and streams the graph over a websocket. The user is flying through their in-memory frames within a second or two.

### What makes it different in use

**Every knob is an expression.** The sidebar's filters, color scales, and size mappings compile to Ursa expressions and execute on the engine — which means they work at engine speed and at engine scale, on data that was never shipped to the browser wholesale.

**Exploration is traversal.** Click-to-expand runs a hop on the live frames. Double-click collapses. Right-click → "shortest path to selection" runs the kernel and highlights the result. The frontier the user sees is a query result, incrementally grown.

**Selection round-trips to Python.** This is the feature analysts will love most, and no competitor has it:

```python
sus = session.selection()        # lasso-selected nodes, as a live NodeFrame
ring = ur.hop(edges, n=2).from_(sus).collect()
session.highlight(ring)          # push a result back into the view
```

The explorer is a *bidirectional* surface of the Python session, not a terminal export. Notebook workflow: explore → select → compute → highlight → repeat.

**Semantic zoom, powered by the algorithm library.** No renderer saves you from 500M edges on screen; coarsening does. Zoomed out, the explorer shows the *metagraph*: communities collapsed into supernodes (sized by member count, edges weighted by inter-community volume). Zooming into a supernode expands its interior — which is just a filter-plus-layout query over the member set. Level-of-detail is driven by Louvain, k-core filtering, and degree-weighted sampling — kernels the core ships anyway. **The visualization is a consumer of the analytics engine, not a parallel subsystem.**

### Performance target

- Smooth pan/zoom at **1M+ nodes / 5M+ edges** rendered (the Cosmograph-demonstrated envelope for WebGL point/line rendering — the existence proof that the browser side is achievable).
- Beyond that, LOD kicks in: the *rendered* set stays bounded (~1M primitives) while the *explorable* graph is bounded only by engine memory.
- Interaction latency budget: local round-trip (interaction → expression → kernel → Arrow diff → GPU) under 100ms for hop/filter operations on the target 1M–500M edge band.

### Architecture **[IMPL]**

```
Python session
   │  ur.explore(...)
   ▼
ursa-viz-server (Rust, axum on the tokio runtime we already run)
   │   • holds Arc refs to the session's EdgeFrame/NodeFrame
   │   • compiles interaction messages -> Ursa expressions -> engine
   │   • runs layout kernels (ursa-core) off-runtime via spawn_blocking
   ▼   websocket: Arrow IPC frames (binary), JSON only for control messages
Browser frontend (prebuilt static assets, TypeScript + WebGL2)
   │   • Arrow JS reads IPC buffers -> typed arrays
   ▼
GPU vertex buffers (positions, colors, sizes — near-direct upload)
```

**The wire protocol is Arrow IPC over websocket.** This is the load-bearing performance decision. Columnar Arrow buffers map almost directly onto GPU vertex attribute buffers, so node positions/colors/sizes travel engine → socket → typed array → GPU with no per-node JavaScript object materialization — the exact pathology that kills D3 (and every JSON-based graph viz) at ~50k nodes. Message types, sketched:

| Direction | Message | Payload |
|---|---|---|
| S→C | `snapshot` | Arrow IPC: node table (id, x, y, color, size, label?), edge table (src_idx, dst_idx, weight?) — *dense u32 render indices, not user IDs* |
| S→C | `delta` | Arrow IPC: added/removed nodes & edges, updated columns (e.g., positions during live layout ticks) |
| C→S | `interact` | JSON: `{op: "expand"|"filter"|"recolor"|"select"|..., params}` — compiled server-side to expressions |
| S→C | `selection_result` / C→S `selection` | Arrow IPC node id sets, for the Python round-trip |

Reuse of core design: the dense `u32` internal indexing from the topology index (`ursa_spec.md`) is exactly the right identifier space for render buffers; user IDs stay server-side except for labels/tooltips, fetched lazily on hover.

**Layout placement — server-side by default.** The Rust Barnes–Hut kernel runs layout and streams position deltas per tick (live "unfolding" animation for free, throttled to frame budget). A browser-side GPU force simulation (Cosmograph-style, transform feedback / WebGPU compute) is an optional mode for small graphs (<100k nodes) where zero-latency parameter fiddling matters. Do not build the GPU simulation first; the server kernel serves both explorer and Tier 1 and is the differentiator.

**Frontend scope discipline.** The frontend is a renderer + input surface, deliberately dumb: it holds render buffers and a viewport, and forwards intent. All graph logic — LOD decisions, sampling, traversal, filtering — lives server-side in the engine. This keeps the frontend small (a prebuilt asset bundle shipped in the wheel — users never touch a Node toolchain) and keeps the "expressions are the interface" invariant intact.

**Jupyter/marimo integration.** An iframe widget pointing at the local server; `session` object exposes `.selection()`, `.highlight()`, `.set_color()`, `.close()`. Terminal usage opens the default browser. Remote/SSH usage documents port-forwarding (same UX as Jupyter itself).

### Level-of-detail pipeline **[IMPL]**

The LOD ladder, all reusing shipped kernels:

1. **Full graph** (≤ ~1M rendered primitives): render everything.
2. **Sampled view**: degree-weighted node sampling + edge sampling with guaranteed inclusion of the current selection/frontier; visually honest for hairball-scale overviews.
3. **Metagraph**: Louvain (or user-supplied grouping column — any categorical column can be the coarsening key: region, type, tenant) → supernodes + aggregated superedges. Computed once, cached as ordinary frames.
4. **Drill-down**: supernode expansion = `filter(member == community_i)` + local layout of the interior, composited into the parent view.

Each rung is a query over frames; the explorer stores no bespoke data structures beyond render buffers.

---

## Tier 3 — Publish and WASM

### Static export (cheap, early)

```python
session.export_html("fraud_ring.html")     # or ur.export_html(edges, nodes, ...)
```

A single self-contained HTML file: the frontend bundle + the current view's node/edge Arrow buffers embedded (positions baked, colors/sizes materialized). Pan/zoom/hover/search work; engine-backed interactions (expand, re-filter, re-layout) are absent or degraded gracefully. This is the shareable artifact for reports, docs, and issue threads — trivial to build once Tier 2 exists, and disproportionately valuable.

### The WASM moonshot

The DuckDB-WASM precedent, transplanted: compile the engine to the browser, fetch data via **HTTP range requests against Parquet in object storage**, and a graph exploration becomes *a static HTML file plus a Parquet URL* — serverless, embeddable in a HuggingFace dataset page, a blog post, an internal wiki. The reader's browser scans the edge list, builds the CSR, runs layout and algorithms locally.

**[IMPL] Sober notes, recorded now so the dream stays buildable:**

- **Compile `ursa-core`, not the full engine.** DataFusion does compile to WASM but is a heavy artifact; the explorer's needs are covered by `ursa-core` (topology + kernels) plus a minimal Parquet range-reader and filter/project shim. The core spec's decision that `ursa-core` has **no DataFusion dependency** — made for testability — is precisely what makes this feasible. Guard it: keep `ursa-core` `wasm32-unknown-unknown`-clean (no tokio, no filesystem assumptions in kernel code; `rayon` behind a feature flag).
- **Threading:** Rayon-in-WASM needs SharedArrayBuffer + COOP/COEP headers — deployment friction on third-party hosts. Ship single-threaded WASM as the baseline; browser-scale graphs (≤ ~10M edges) are fine single-threaded with Barnes–Hut. Threaded WASM is an optimization for self-hosted contexts.
- **Memory:** wasm32 is a 4GB space; combined with the u32 node space this bounds browser-mode graphs — document the envelope honestly (~10M edges comfortable, ~50M ceiling).

---

## Packaging and product shape

- **Core stays lean.** Only the `ur.layout_*` kernels live in core — they are just algorithms. Everything else ships as `ursa-viz`: `pip install ursa[viz]`.
- `ursa-viz` contains: the axum server crate, the prebuilt frontend assets (no Node toolchain for users; frontend built in CI), `ur.plot`, `ur.explore`, `export_html`.
- Version-locked to core minor versions; the websocket protocol is versioned from day one.
- The frontend is a candidate for eventual standalone value (an "Ursa Explorer" that opens any Parquet edgelist), but is built library-first.

## Competitive framing

| | Engine behind the view | 1M+ node rendering | In-process w/ your data | Bidirectional w/ Python | Serverless sharing |
|---|---|---|---|---|---|
| Gephi | ✗ (desktop app) | partial | ✗ | ✗ | ✗ |
| D3 / sigma.js / pyvis | ✗ | ✗ (~50k) | ✗ | ✗ | partial |
| Cosmograph | ✗ (render-only) | ✓ | ✗ | ✗ | partial |
| Graphistry | ✓ (GPU server) | ✓ | ✗ (SaaS/server) | partial | ✗ |
| **Ursa Viz** | **✓ (Ursa itself)** | **✓** | **✓** | **✓** | **✓ (Tier 3)** |

## Roadmap and sequencing

| Phase | Deliverable | Depends on |
|---|---|---|
| v0.2-adjacent | `layout_fa2`/`layout_fr` kernels, positions-as-columns, `ur.plot` | core v0.1 |
| v0.3 | `export_html` static snapshots (frontend v0: render-only) | layout kernels |
| v0.4 | `ur.explore` flagship: server, Arrow-over-websocket, expand/filter/select, Python round-trip | v0.3 frontend |
| v0.5 | Semantic zoom / LOD ladder, live layout streaming, Jupyter widget polish | v0.4 + louvain |
| later | WASM build of `ursa-core`, publish-to-URL, GPU browser sim, hierarchical layouts | all above |

## Non-goals

- Not a BI charting library — no bar charts; `to_polars()` and use the tabular ecosystem.
- Not a diagramming/editing tool — no manual node placement persistence, no drawing.
- Not a hosted SaaS (the library emits local servers and static artifacts; hosting is the user's).
- No 3D layout in early tiers (cool, rarely legible; revisit on demand).

## Open questions (deliberately unresolved)

1. Frontend rendering stack: hand-rolled WebGL2 vs. regl vs. wgpu-compiled-to-WASM sharing Rust code with core. Decide at Tier 2 start; protocol design is stack-agnostic.
2. Position dtype: f32 vs f64 columns (f32 likely sufficient and halves GPU traffic).
3. Whether `ur.plot` renders via matplotlib delegation or a native rasterizer (datashader-style) for edge-heavy stills. Lean: matplotlib first, native rasterizer when benchmarks demand it.
4. Live-layout tick streaming rate control (server-paced vs client-requested frames).

---

*Companion to `ursa_spec.md`. The core spec remains the sole authority for v0.1 implementation; this document governs nothing until Tier 1 work begins, except the standing note that layout kernels are ordinary `ursa-core` algorithms.*
