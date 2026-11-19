# The Graph Explorer: query → graph → explore

> **Status:** design, November 2026, for `ursa` 0.12 and after. Nothing here is built yet.
> **Tracks:** #280 (graph view), #281 (exploration), #343 (Python widget and HTML export), #395 (layouts in SQL).
> **Issue drafts:** [`graph-explorer-issues.md`](graph-explorer-issues.md). **Measurement scripts:** [`graph-explorer/`](graph-explorer/).

Ursa's web shell runs SQL on a real engine in the browser. This design makes it "a Neo4j Browser on Ursa": type a query, see its result as a graph, and explore from there. It also makes the visualization a capability of the `ursa-viz` crate, so the Python notebook widget, an exported HTML page and a native window reuse it.

## Summary

1. **A result becomes a graph by rule, not by position.** Today `.graph` draws a result's first two columns as edges, whatever they are. The new rules read what each column *is* (a node key, an edge's endpoints, a path, a hop distance, a metric). The engine attaches that to the result as Arrow field metadata, and column names and types are the fallback. The pane says which rule it used, and one `AS …` clause overrides it.
2. **Exploration is made of shell commands.** A double-click on node 33 runs `.expand 33`, which is echoed in the terminal, is kept in history and replays from a share link like any other command. Each command is defined by the SQL it runs (`hop` plus a new `induced_edges`). `.graph sql` folds the whole exploration into one SQL statement.
3. **The open graph is a table.** `explorer.nodes`, `explorer.edges` and `explorer.selection` are temporary views of what is drawn, so the terminal can query the picture, and a query can change it (`.graph + QUERY`).
4. **Scale comes from the layout's own hierarchy.** The coarse-to-fine ForceAtlas2 already builds a deterministic pyramid of the graph. The same pyramid gives the first paint (a coarse level, drawn within half a second), the aggregated view when zoomed out, and the order in which edges are drawn. A frame budget decides how many edges a frame draws while the view moves, and the rest are drawn when it rests.
5. **Memory is shared by not rebuilding, not by a shared heap.** Copying a million edges through JavaScript costs 5 ms. What costs is that three wasm instances each rebuild the graph: 350 ms on the page's thread and about 150 MB of heap at 1M edges. Phase 0 and Phase 3 remove the rebuilds. A threaded build with one shared memory is a later, time-boxed spike, because it needs a nightly toolchain and cannot run in notebooks or exported pages.
6. **`ursa-viz` becomes a core and its front ends.** The core (the graph-result model, the scene, level of detail, picking, history) has no GPU and no browser in it. Rendering is a feature. The web shell, the widget, the HTML export and a native window are thin.

The phases, each of which ships something a user can use:

| phase | ships | parent |
|---|---|---|
| 0 | Foundations and fixes: the model and its wire format, `.graph` resolution out of `ursa-wasm`, pinch zoom, no nonsense drawings | #280 |
| 1 | Results → graph: the rules, paths and hop rings, labels, hover and selection | #280 |
| 2 | Explore: expand, hide, pin, undo, inspector, search, the phone layout | #281 |
| 3 | Scale: frame budget, first paint from coarse levels, aggregation, 5M edges | #280 |
| 4 | History → SQL: `.graph sql`, shared explorations, export | #281 |
| 5 | Beyond the web: the notebook widget and HTML export on the same core | #343 |

Most of Phases 3 and 5 does not depend on Phase 2 and can run beside it.

## 1. Current state and gaps

Read from the code on `main` at `7650a1c` and measured locally. The numbers are from headless Chromium 153 on 4 container cores with **no GPU**: WebGL2 on SwiftShader, and no WebGPU adapter. They say where time goes. They do not say what a laptop's GPU does: the other agent's benchmark harness (`claude/viz-bench`) is the source for that.

### What exists

- **`ursa-viz`** (1,850 lines): `scene.rs` (the graph, the live layout, node styles, themes, the camera), `render.rs` (wgpu: edges as one line list indexed into `x` and `y` vertex buffers, nodes as instanced discs), `view.rs` (scene plus renderer, pan, zoom, fit, pick, snapshot) and `web.rs` (wasm-bindgen: `createView`, `Layout`, `sizeBy`, `colorBy`, `setTheme`, `pick`, `setPositions`).
- **Layout:** `Fa2Multilevel` in `ursa-core`, in a Web Worker, deterministic, posting positions every 100 ms.
- **The shell's Graph view:** `.graph [SOURCE] [NODES (q)] [SIZE BY x] [COLOR BY y]`. `ursa-wasm/src/graph.rs` (1,365 lines) resolves the source, maps ids to dense indexes, and resolves `SIZE BY` and `COLOR BY` against a `NODES` query, the vertex tables or an algorithm. `web/public/graph.js` owns the canvas, the pointer, the keys, the status line and the legend.
- **Quality of what is there:** high. The layout is deterministic and tested for quality, the view is themed with a palette checked for colour-vision deficiencies, the legend is text, errors say what to do, frames are read back in tests, and e2e runs in CI.

### What it does not do

| area | today |
|---|---|
| Result → graph | Columns named `src` and `dst`, else **the first two columns, of any type**. No notion of nodes, paths or distances. |
| Node identity | The id as a string, by dense index. No vertex table, label or property reaches the page except through `SIZE BY` / `COLOR BY`. |
| Interaction | Pan, zoom, fit, click to name a node in the header. No hover, selection, labels, edge picking, expand, hide, pin, search or undo. |
| Drawing | 1-pixel edges of one colour. No direction, weight, highlight, label or dimming. |
| Phone | One-pointer drag pans. **There is no way to zoom**: no pinch, and the `+` and `-` keys need a keyboard. |
| Scale | Every frame draws every edge. A hard cap of 2M edges and 4M nodes. |
| Reuse | The resolution logic is in `ursa-wasm`, so the CLI and Python cannot use it. The Python view (#343) does not exist. |

What `.graph` draws today after each kind of result on the karate graph (`graph-explorer/rules-today.cjs`):

| last result | drawn today | should be |
|---|---|---|
| `pagerank(karate) LIMIT 5` | 5 edges from each id **to its score**, 9 "nodes" | 5 nodes, sized by PageRank, with the edges among them |
| `algorithms(karate, pagerank(…), community => louvain(…))` | 34 edges id → score, 61 nodes | the graph, sized by PageRank, coloured by community |
| `hop(karate, seeds => [0], n => 2)` | a star: every reached node tied to the seed | rings: distance 1 and 2, with the real edges |
| `ANY SHORTEST … COLUMNS (path_length(p), vertices(p), edges(p))` | one edge, `5 → [16, 5, 0, 8, 33, 26]` | the route, six nodes in order |
| `COLUMNS (vertices(p) AS members)` | error: one column | the paths' union |
| `MATCH (a)-[]-(b)-[]-(c) COLUMNS (a.id, b.id, c.id)` | only `a`–`b` | both edges of each match |
| `COLUMNS (a.club, b.club, a.id, b.id)` | 2 nodes, `Officer` and `Mr. Hi` | the edges between ids |
| `shortest_path(karate, …)` | correct (`src`, `dst`) | the same, drawn as a route |

### Where the time and memory go

`.graph rmat` (46,318 nodes, 1,000,000 edges; the README's 131K-node R-MAT is the demo's, not this dataset), measured with `graph-explorer/view-cost.cjs` and `handoff.cjs`:

| step | measured | note |
|---|---|---|
| Engine returns the graph | 0.31 s | query, id interning, copy out of wasm |
| `createView` on the page's thread | 0.19–0.35 s, **blocking** | it builds a `Topology` and an undirected CSR only to read degrees and seed positions (`View::new`) |
| Layout worker: topology and hierarchy | 0.71 s | `new Layout(…)` |
| First drawing | 1.08 s after Enter | |
| Layout done | 6–11 s | 6 s in `web/README.md`; 10.7 s here, with software rendering taking the cores |
| One redraw, 1M edges | **3.9 s** | software rendering |
| One redraw, 300K / 100K / 10K / 1K edges | 1.45 s / 0.73 s / 0.22 s / 0.05 s | not linear in edges: it is fill, not vertices |
| Pick | 0.25 ms at 46K nodes, 0.5 ms at 131K, 4.9 ms at 1M | a linear scan |
| Copying `src` and `dst` (8 MB) | 4.5 ms; a transfer is 0.1 ms | |
| wasm heap, page's view | 50 MB | `Graph` keeps `src`, `dst`, a topology and the line indices |
| wasm heap, layout worker | 102 MB | topology and hierarchy |

Three things follow.

1. **Copies through JS are not the problem.** A million edges copy in 5 ms. The page blocks for a third of a second because it *rebuilds* what the worker also builds, and the two instances hold 150 MB for an 8 MB edge list.
2. **A frame's cost is pixels.** 10K edges cost a quarter of the 100K frame, not a tenth. Long translucent lines across a hairball are overdraw. So a level of detail that draws fewer and shorter edges helps more than one that only draws fewer vertices.
3. **Every early frame draws every edge of a layout that is still a few dozen coarse points**, each edge between two points that coincide with thousands of others. That is the most expensive way to draw the least information.

The engine side is already fast enough to explore on (`graph-explorer/engine-point.cjs`, wasm engine, 1M-edge graph, times include parsing, planning and encoding the result):

| query | median |
|---|---|
| `hop(rmat, seeds => [x], n => 1, direction => 'both')` | 8.6 ms |
| `hop(…, n => 2, …)` (thousands of rows) | 7.7 ms |
| `hop` from 500 seeds | 19 ms |
| `GRAPH_TABLE (rmat MATCH (a WHERE a.id = x)-[e]-(b) …)` | 19 ms |
| `ANY SHORTEST` between two nodes, with `vertices`, `edges`, `element_id` | 25 ms |
| A vertex's properties, `SELECT * FROM members WHERE id = x` | 8.7 ms |
| Edges among 5,000 nodes, edge-table scan with `IN` | 180 ms |
| Edges among 5,000 nodes, `GRAPH_TABLE` with `IN` lists | 1.5–5.3 s |
| `pagerank(rmat)` / `louvain(rmat)` | 60 ms / 620 ms |

So a click can be answered in about 10 ms of engine time. The one gap is **the edges among a set of nodes**: `hop` returns nodes (`seed`, `id`, `distance`), not edges, and the only general way to get the edges is to scan the edge table, which grows with the graph, not with the answer. Section 3 proposes `induced_edges` for that.

Checked in the engine: `vertices(p)`, `edges(p)` (the edges' rows in the edge table, counted from 0), `element_id(p)` (the two alternating; integer keys only) and `path_length(p)` exist, and so do `hop` and `shortest_path`. A path variable is only allowed on a single quantified edge. An algorithm, and `hop`, run on one vertex table and one edge table; `GRAPH_TABLE` spans all of a graph's tables.

**The browser has no stored index.** The memory-mapped index of 0.11 is native only (`ursa-engine/src/database/mapped.rs` is compiled out on wasm32). In the browser the index is built once per session and cached (`IndexCache`), which is what the 8 ms above measures. The stored index serves the same explorer where the engine is native: the notebook widget's kernel, and later `ursa ui club.ursa`.

## 2. The competitive landscape

What each does well, where it stops, and what Ursa should take from it. Vendor pages for Neo4j, Linkurious, sigma.js and Graphistry could not be fetched from this session (the network allows GitHub and search only), so those rows rest on search results and are marked where a number is a community report rather than documentation.

| tool | does well | limits |
|---|---|---|
| **Neo4j Browser** | Query → graph with no configuration: any node or relationship in a Cypher result is drawn. Double-click expands. A result frame per query. | Built for hundreds of nodes: `initialNodeDisplay` defaults to 300 and `maxNeighbours` to 100 [1][2]. Users report it unresponsive at about 1,000 [3]. Needs a server. |
| **Neo4j Bloom / Aura Explore** | The reference for exploration: expand, dismiss, undo and redo, shortest path between two selected nodes, an inspector on double-click, a search bar of phrases, saved scenes [4][5]. | Expansion is capped by a "node query limit"; users report a 10,000-node ceiling [6]. No SQL, no way back from a scene to a query. Licensed with Neo4j. |
| **Graphistry** | Scale with analysis around the picture: histograms, filters, a time bar. Layout on server GPUs, streamed to a WebGL client: "up to 8MM nodes + edges", with most client GPUs smooth at 100K–2M [7]. | Needs a server with NVIDIA GPUs (the AWS listing is $10 an hour on a g4dn.2xlarge [8]). The server is proprietary. Data leaves the machine. |
| **cosmos.gl / Cosmograph** | The fastest layout in a browser: the force simulation runs in fragment shaders, with "hundreds of thousands of points and links" in real time [9] and over a million claimed [10]. cosmos.gl is MIT and an OpenJS project. Cosmograph 2 adds DuckDB-WASM, Mosaic and SQLRooms for filtering [11]. | Needs float-texture extensions (no Android device without `OES_texture_float`; iOS broke it once) [9]. The GPU simulation is not documented as deterministic, and it draws the whole graph: there is no graph engine behind it to expand from, no traversal, no algorithms. Cosmograph itself is closed source, CC-BY-NC [11]. |
| **Kepler.gl** | Columnar data straight to the GPU: a million polygons in about 2 s with GeoArrow [12]. Filters and time playback over millions of rows. MIT. | Geospatial: nodes need coordinates. No graph layout. Its arcs are a layer, not a graph. |
| **sigma.js / Gephi Lite** | A clean WebGL renderer over a real graph model (graphology), with labels, hover, and node and edge programs. MIT. Gephi Lite puts Gephi's workflow (ForceAtlas2, filters, appearance) in a browser. | "Tens of thousands of nodes and edges" smoothly [13]. Layout is JavaScript on the CPU. Edge picking is off by default because of its cost [13]. |
| **G6 (AntV)** | The richest interaction set in open source: combos (grouping with collapse and expand), behaviours, many layouts, some in Rust → wasm or on WebGPU [14]. MIT. | Aimed at thousands of elements: its viewport optimisation is recommended from about 500 [15]. |
| **Linkurious / Ogma** | Investigation workflows on a graph database; grouping, level-of-detail aggregation and transformations in the library [16]. | Commercial only. Claims 100K nodes and 100K edges at launch; its documentation says a 25K-node layout in under a second [16]. |

Sources: [1] [Neo4j Browser settings](https://neo4j.com/docs/browser/legacy/operations/browser-settings/) · [2] [Browser settings, current](https://neo4j.com/docs/browser-manual/current/operations/browser-settings/) · [3] [community: browser node limit](https://community.neo4j.com/t/browser-node-relationship-limit/69507) · [4] [Bloom scene interactions](https://neo4j.com/docs/bloom-user-guide/current/bloom-visual-tour/bloom-scene-interactions/) · [5] [Aura Explore: default actions](https://neo4j.com/docs/aura/explore/explore-default-actions/) · [6] [community: 10,000-node limit in Bloom](https://community.neo4j.com/t/bypass-10000-node-limit-in-bloom-neo4j-desktop/45670) · [7] [pygraphistry on PyPI](https://pypi.org/project/graphistry) · [8] [Graphistry on AWS Marketplace](https://aws.amazon.com/marketplace/pp/B07Q88KH8C) · [9] [cosmosgl/graph](https://github.com/cosmosgl/graph) · [10] [OpenJS: introducing cosmos.gl](https://openjsf.org/blog/introducing-cosmos-gl) · [11] [Cosmograph README](https://cdn.jsdelivr.net/npm/@cosmograph/cosmograph@2.3.2/README.md), [what's new](https://cosmograph.app/docs-general/whats-new/) · [12] [kepler.gl 3.0](https://foursquare.com/?p=31976) · [13] [sigma.js: performance](https://v4.sigmajs.org/how-to/technical/performance/) · [14] [G6 features](https://g6.antv.antgroup.com/en/manual/whats-new/feature) · [15] [G6: optimize viewport transform](https://g6.antv.antgroup.com/en/manual/behavior/optimize-viewport-transform) · [16] [Ogma](https://linkurio.us/ogma-js-library-large-scale-graph-visualization/), [Ogma docs](https://doc.linkurious.com/ogma/latest/) · [17] [web.dev: WebGPU in major browsers](https://web.dev/blog/webgpu-supported-major-browsers).

### Where Ursa's advantages are real

- **The engine is in the page.** No other tool here has a graph engine with traversal, pattern matching and algorithms next to the renderer, with no server. Neo4j and Graphistry need one. Cosmograph has DuckDB-WASM, which filters tables but cannot expand a node. So Ursa alone can draw *part* of a graph and fetch the rest on a click, in about 10 ms, offline.
- **SQL both ways.** Bloom's scenes cannot be turned into a query. Here every action is a command with SQL behind it, so an exploration is reproducible and shareable as text.
- **A deterministic multilevel layout.** The same graph gives the same picture on every machine, so positions can be cached, shared and tested. cosmos.gl is faster per iteration, but a GPU simulation is not reproducible. And the hierarchy is an asset no competitor's layout hands over: it is the level-of-detail structure (section 5).
- **The stored index.** Natively, a click on a 100M-edge database costs a memory-mapped lookup, not a server round trip and not a load of the whole graph.
- **Algorithms as columns.** `SIZE BY pagerank COLOR BY louvain` is one line. In Bloom or Browser it is GDS calls, written-back properties and styling rules.

### Where Ursa is behind

- **Looks and feel.** No labels, hover, selection, direction or edge widths. sigma.js, G6 and Bloom all have these. Phase 1 closes most of it.
- **Layout speed on a GPU machine.** One thread of wasm against a GPU simulation: 7.5 s for 1M edges here, real time in cosmos.gl. Ursa's answer is to show the coarse levels at once and to refine; a threaded build is the later lever (section 6).
- **Analysis around the picture.** Graphistry's histograms, filters and time bar. Ursa's answer is SQL in the terminal over `explorer.nodes`, then a metrics panel (#281).
- **Grouping.** G6's combos and Ogma's grouping. Phase 3's aggregation is the answer.
- **Unmeasured on real GPUs.** Every frame time in this document is software rendering.

## 3. Query → graph

### The model: a graph result

Everything drawn is a **graph result**: nodes, edges, highlights and where they came from. It is defined in `ursa-viz` (section 6) and produced by the engine.

```text
GraphResult
  graph        the property graph it is part of ("karate"), or none
  nodes        key (typed, and as text), label (which vertex table), columns (properties, metrics)
  edges        src, dst (node indexes), label (which edge table), row (its row in the edge table), columns
  highlights   paths (ordered nodes and edges), seeds, distance per node
  encodings    size by, colour by, group by, layout (force | rings | layers)
  provenance   the SQL, the rule that fired, each column's role
```

A node's identity is `(label, key)`, so two vertex tables may reuse a key. An edge's identity is `(label, row)` where the engine knows the row, else `(src, dst, ordinal)`. A **delta** adds or removes nodes, edges and highlights, or sets columns; a scene is a base result and the deltas applied to it.

### How the engine knows what a column is

By **provenance first, names second**.

- **Provenance:** graph functions and `GRAPH_TABLE` mark their output fields with Arrow field metadata. `ursa.role` is one of `node`, `src`, `dst`, `seed`, `distance`, `path_nodes`, `path_edges`, `path_elements`, `metric`, `category`, `property`. `ursa.graph` names the property graph, `ursa.label` the vertex or edge table, and for `GRAPH_TABLE`, `ursa.var` the pattern variable and `ursa.pattern` (on the schema) the pattern's edges as pairs of variables. Metadata follows a column through `SELECT`, `WHERE`, `ORDER BY`, `LIMIT` and joins, and is lost when an expression computes a new value (`round(pagerank, 4)`), which is right: the new column is no longer known to be a node key. The first task of that issue is to confirm which DataFusion 55 operators keep field metadata, and to carry it through aliases if they do not. Roles also keep the view independent of column names, which #415 proposes to change (`shortest_path`'s `hop` to `step`, `random_walk`'s `node` to `id`).
- **Names and types:** when there is no metadata (a plain `SELECT` over a Parquet file), the fallback reads names: `src`/`dst`, `source`/`target`, `from`/`to` are endpoints; `id` or `node` is a node key; `seed`, `id`, `distance` together are a hop result; a list column whose elements have the node keys' type is a path.

A node key or an endpoint must be an integer, a string, or another discrete type, and both endpoints must be the same type. **A float or a list is never a node id.** That alone removes today's `id → pagerank` drawings.

### The rules

The first rule that matches draws the result. The pane's status line says which: `drawn as a path on karate (members, ties) · .graph AS … to change`.

| # | the result has | drawn as |
|---|---|---|
| 0 | an `AS …` clause (below) | what it says |
| 1 | a path column (`vertices(p)`, optionally `edges(p)` or `element_id(p)`) | **Paths.** Each row is a path; its consecutive nodes are edges. The union of all rows is drawn, highlighted. One row (`ANY SHORTEST`): a **route**, its ends marked, laid out left to right. Several rows with the same ends (`ALL SHORTEST`, `SHORTEST k`): **layers**, by position in the path. Other columns (`steps`, a cost) label the path. |
| 2 | `seed`, `id` and `distance` (a `hop` result) | **Rings.** Nodes are the seeds and the reached nodes. Edges are the graph's edges among them (`induced_edges`). One seed: concentric rings by distance. Several: layers by distance from the nearest seed. Distance colours the nodes unless `COLOR BY` says otherwise. |
| 3 | endpoint columns (`src` and `dst` by role or name; or exactly two node variables' keys from `GRAPH_TABLE`) | **Edges.** One edge a row. Other columns are edge attributes; a numeric `weight` sets edge width. |
| 4 | three or more node variables' keys from `GRAPH_TABLE` | **Pattern matches.** For each row, one edge for each edge of the pattern whose two ends are both in `COLUMNS`. `a.club` and other properties attach to their variable's node. Variables of different vertex tables are nodes of different labels, coloured by label unless `COLOR BY` says otherwise. |
| 5 | one node key column (`pagerank`, `louvain`, `algorithms`, `degree`, a vertex table) | **Nodes.** The nodes, and the graph's edges among them. The first numeric metric sizes them and the first category colours them, by what the column is: `pagerank`, `betweenness`, `closeness`, `degree`, `triangle_count` and `clustering_coefficient` size; `louvain`, `label_propagation` and `connected_components` colour. When the rows are every node of the graph, this is `.graph g SIZE BY … COLOR BY …`. |
| 6 | none of these | Not a graph. The message names the columns and their types, and shows the `AS` clause that would draw it. |

With no metadata and no matching names, **two columns of the same discrete type** are still drawn as edges, as today, so `.graph SELECT a, b FROM pairs` keeps working. What goes is "the first two columns of any type".

Rules 2 and 5 need the graph the ids belong to, to find edges. It is `ursa.graph`, else the `ON graph` clause, else the session's only property graph. With none, rule 2 draws seed-to-node edges for distance 1 only and says so, and rule 5 draws the nodes with no edges and says so.

### When it is ambiguous

| case | what is drawn | why |
|---|---|---|
| Endpoint columns *and* a path column | the path (rule 1), over the edges if they are the same graph's | the path is the more specific answer |
| `src`, `dst` *and* `hop` (a `shortest_path()` result) | a route: the rows in `hop` order | the function's role metadata says so; by names alone, edges |
| Two node keys and metrics (`a.id`, `b.id`, `pagerank`) | edges; the metric is an edge attribute | a metric joined to an edge list cannot be assigned to one end |
| A node key twice (a self-join) | edges | two keys are two ends |
| Several graphs in one query | the graph of the node columns; refuse if they differ | ids from two graphs do not meet |
| A hop result with `distance` renamed | edges `seed`–`id`, with a note | without the distance there are no rings |
| An integer `id` and an integer `community`, no metadata | nodes, coloured by `community` | `id` is the key by name |
| More rows than the cap | refuse, with the `LIMIT` to add, as today | until Phase 3 raises the cap |

The pane never guesses silently: the status line names the rule, and a result that matched a second rule adds `· also a path (members): .graph AS path(members)`.

### What the user can override

One clause, after the source, beside the existing ones:

```text
.graph [SOURCE] [ON graph]
       [AS edges(src, dst [, weight]) | nodes(id) | path(nodes [, edges]) | hops(seed, id, distance)]
       [NODES (query)] [SIZE BY x] [COLOR BY y] [GROUP BY z] [LAYOUT force|rings|layers]
```

- `AS …` names the shape and the columns. It is rule 0.
- `ON graph` names the graph the ids belong to.
- `NODES (query)`, `SIZE BY` and `COLOR BY` are today's. `GROUP BY` is Phase 3's aggregation.
- `LAYOUT` overrides the layout the rule chose.

`.graph` with no arguments draws the last result by the rules. `.graph NAME` draws a property graph, as today.

### Drawing paths, rings and metrics

- **Highlights** are a second layer: wide edges (instanced quads, 3 px), a halo on their nodes, and everything else dimmed to 35%. A route's first and last nodes carry a ring and are always labelled.
- **Rings** and **layers** are layouts in their own right, deterministic and cheap: rings place distance `d` on a circle of radius `d`, ordered to shorten edges by a fixed number of barycentre sweeps; layers do the same on lines. Neither needs ForceAtlas2, so a path or a neighbourhood appears in one frame.
- **Metrics** use the existing `sizeBy` and `colorBy`, with the legend. Rule 5 only chooses their defaults.
- **Direction** is drawn (arrowheads) when at most 5,000 edges are in view; **labels** when a node's disc is large enough, largest first, without overlap (section 5).

### The new SQL function

```sql
induced_edges(graph, nodes => …)   -- the graph's edges whose two ends are both in nodes
```

It returns `src`, `dst` and `edge` (the edge's row in the edge table, as `edges(p)` numbers them), read from the index in time proportional to the nodes' degrees. `nodes` is the existing "node ids" parameter: a list, or a query of one column. Without it, edges among a node set cost a scan of the edge table (180 ms at 1M edges in the browser, and growing with the graph), or seconds through `GRAPH_TABLE` with long `IN` lists. With it, rules 2 and 5, expansion and "connect the nodes I see" are one index read. It is a traversal like `hop`, documented in the function reference, and useful on its own (`induced_edges(g, nodes => (SELECT id FROM pagerank(g) ORDER BY pagerank DESC LIMIT 100))`).

## 4. Exploration

### Actions are commands

Every action that changes what is drawn is a shell command. The pointer and the keys only type it.

| command | does | the SQL behind it |
|---|---|---|
| `.expand ID [in\|out\|both] [LIMIT n]` | adds a node's neighbours and the edges among everything shown | `hop(g, seeds => [ID], n => 1, direction => …)`, then `induced_edges(g, nodes => (SELECT id FROM explorer.nodes))` |
| `.hide ID…`, `.hide selection`, `.hide others` | removes nodes from the view | none: a change to `explorer.nodes` |
| `.path A B` | adds and highlights a shortest path | `GRAPH_TABLE (g MATCH p = ANY SHORTEST (a WHERE a.id = A)-[e]-*(b WHERE b.id = B) COLUMNS (vertices(p), edges(p)))` |
| `.find TEXT` or `.find WHERE condition` | selects matching nodes and fits them in view | a query on `explorer.nodes` joined to the vertex table |
| `.pin ID…`, `.unpin ID…` | fixes a node's position | none |
| `.select ID…` | sets the selection | none |
| `.undo`, `.redo` | steps the history | none |
| `.graph + QUERY` | merges a query's result into the open graph, by the rules | the query |
| `.graph sql` | prints the exploration as one SQL statement | Phase 4 |

Because they are commands:

- **The terminal shows them.** A double-click on node 33 echoes `.expand 33`, and the shell answers `+16 nodes, +41 edges` as it would if typed. This is the answer to "does a click write the equivalent SQL into the terminal": it writes the command, and `.graph sql` or `.expand 33 --sql` prints the SQL. Echoing raw SQL for every click would bury the terminal.
- **History and share links work unchanged.** The commands are in `.history`, and a `#run=` link replays them after its usual question. Nothing new is needed for a shared exploration to open on another machine.
- **They are testable** in the shell's own tests, with no browser.

Not everything echoes. Selection, hover, pan and zoom change nothing that is drawn, so they stay silent; `.select` exists for scripts and share links. A drag that pins a node echoes `.pin ID AT x y` when it ends.

### The explorer session

The engine side keeps an **explore session** for the open graph: the base graph, the map from `(label, key)` to node index, the shown set, and the history. It turns a command into SQL, runs it, applies the result → graph rules to what comes back, and emits a delta. The page applies the delta to the scene. Undo emits the inverse delta, with no query.

It also registers the view as temporary views in the session:

- `explorer.nodes`: `id`, `label`, `selected`, `pinned`, `x`, `y`, and the columns `SIZE BY` and `COLOR BY` use;
- `explorer.edges`: `src`, `dst`, `edge`;
- `explorer.selection`: the selected ids.

So the open graph is a table. `SELECT club, count(*) FROM members JOIN explorer.selection USING (id) GROUP BY club` describes a selection. `.graph + SELECT * FROM hop(karate, seeds => (SELECT id FROM explorer.selection), n => 1)` expands it. No other graph tool can do this, because no other has the engine and the picture in one session.

### Expanding

- **Double-click**, or Enter on the selected node, runs `.expand`. Shift expands in both directions with no limit up to 5,000.
- **The cap:** 100 neighbours by default, those of highest degree first, then by key, so the same click always adds the same nodes. A node with more shows a `+N` badge; expanding it again adds the next 100. Neo4j Browser's limit is the same number, but it does not say which 100.
- **A node's badge** shows how many of its neighbours are not drawn. It comes free: the session knows each node's degree in the graph and in the view.
- **Where new nodes go:** on a golden-angle spiral around the node they came from, which is how the multilevel layout already places a group's members. Then ForceAtlas2 runs warm: nodes already drawn are held (pinned nodes exactly, the others with ten times the inertia), so the picture settles in a few dozen iterations without moving what the user has learned. This needs a per-node `fixed` mask in `Fa2State`, a small change in `ursa-core`.
- **Latency:** `hop` 9 ms, `induced_edges` about as much, a delta and a frame. The target is 100 ms from click to the drawn frame (section 9).

### The inspector

A click selects a node or an edge and opens the inspector: a panel in the pane, on the right, or a sheet from the bottom on a phone.

- **A node:** its key and label; its row in the vertex table (`SELECT * FROM members WHERE id = 33`, 9 ms measured); the metrics drawn; its degree in the graph and in the view; buttons for expand, hide, pin and "path from here".
- **An edge:** its ends; its row in the edge table, fetched by the edge's row number where the index has it, else by its ends.
- **Several selected:** counts, and a line of SQL to copy: `… WHERE id IN (SELECT id FROM explorer.selection)`.

Values are text, selectable, and each property has a "colour by this" and a "size by this" button, which run `.graph` with the clause added. Edge picking is needed for this; it uses the same spatial grid as node picking (section 5), nodes first.

### Search and focus

- `/` in the pane opens a search box. Plain text matches node keys, then the label column (the first string property), in the view first and then in the graph. Enter selects the matches and fits them. A match that is not drawn is offered as "add to the graph".
- `.find WHERE club = 'Officer'` filters by any condition on the vertex table.
- `f` fits the selection, `0` the whole graph, as today.

### Pin, hide, undo

- **Pin:** drag a node to move and pin it; `p` toggles. Pinned nodes keep their place through layouts and expansions. The pin is drawn as a small ring.
- **Hide:** `h` or Delete hides the selection; `.hide others` keeps only it (Bloom's "dismiss others"). Hidden nodes stay in the session, so undo and `.show` bring them back with their positions.
- **Undo:** Ctrl+Z and `u`, redo Ctrl+Shift+Z and `U`. Every content command is one step. Undo applies the inverse delta and restores the positions of that step, so it is one frame and no query.

### The history

The history is the list of steps: the command, its SQL, what it added, and when. Phase 2 keeps it and shows it as text (`.graph history`). Phase 4 adds a panel that jumps to a step, and `.graph sql`, which folds the steps into one statement:

```sql
-- Explored from: SELECT * FROM hop(karate, seeds => [0], n => 1)
-- 2 expansions, 1 path, 3 nodes hidden
WITH shown(id) AS (
  SELECT id FROM hop(karate, seeds => [0], n => 1, direction => 'both')
  UNION SELECT 0
  UNION SELECT id FROM hop(karate, seeds => [33, 2], n => 1, direction => 'both')
  UNION SELECT unnest([16, 5, 0, 8, 33, 26])
  EXCEPT SELECT unnest([11, 12, 17])
)
SELECT * FROM induced_edges(karate, nodes => (SELECT id FROM shown));
```

Expansions with a cap fold to the ids they added, not to the `hop`, so the statement returns exactly what is on screen. Run on the same data it gives the same nodes and edges; that is the acceptance test.

## 5. Scale and novelty

The aim: several million edges in a browser tab, a first frame in well under a second, and pan and zoom that never wait for the graph's size. The plan uses what Ursa already has that the others do not.

### The layout's hierarchy is the level of detail

`Fa2Multilevel` coarsens the graph into levels before it lays anything out. For the 131K-node, 1M-edge R-MAT (measured natively with `ursa-core/examples/fa2_multilevel.rs`):

| level | 10 | 9 | 8 | 7 | 6 | 5 | 4 | 3 | 2 | 1 | 0 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| nodes | 43 | 87 | 174 | 348 | 696 | 1,393 | 2,789 | 5,586 | 11,200 | 22,881 | 131,072 |
| merged edges | 0.9K | 3.7K | 15K | 60K | 221K | 504K | 709K | 804K | 847K | 869K | 929K |
| laid out by | 0.30 s | 0.33 s | 0.40 s | 0.61 s | 1.3 s | 2.5 s | 3.4 s | 4.1 s | 4.5 s | 4.9 s | 6.3 s |

Each coarse node has a mass (its members) and each merged edge a weight (the edges it stands for). The hierarchy is a pure function of the graph, built in 0.26 s natively and about 0.7 s in the browser. Three things come from it.

1. **First paint.** Draw the level being laid out, as an aggregated graph: discs sized by mass, edges by weight. Level 8 (174 nodes, 15K edges) is laid out by 0.4 s and costs a few milliseconds to draw. Today the same moment draws a million edges between points that coincide. The drawing then refines level by level, each a short animation from parents to children, as the layout reaches it.
2. **The aggregated view.** Zoomed out, a million-edge graph is a blot at any frame rate. The view draws a *cut* through the hierarchy instead: the coarsest nodes whose discs are at least a few pixels apart on screen. Zooming in splits the groups in view into their children. The cut is recomputed when the camera rests, in the layout worker, which holds the hierarchy.
3. **Edge order.** Edges go to the GPU sorted by importance: the level at which their two ends merge, highest first, then by weight. A prefix of that buffer is always a sensible sample: the backbone between far groups first, the local fill last.

The R-MAT table also shows the limit: merged edges do not thin out until the nodes are few (60K edges at 348 nodes). So the aggregated view also ranks merged edges by weight and draws the heaviest within the frame's budget. A clustered graph thins faster.

**Aggregation by community** is the same machinery with the user's grouping in place of the hierarchy's: `.graph g GROUP BY louvain` draws one disc per community, sized by members, labelled, with the edges between communities weighted by count. A double-click on a community opens it in place (its members laid out inside its disc); `.expand` on a disc is that command. The grouping column is any category: `louvain`, `connected_components`, a vertex property. This is what Bloom and Browser cannot do at all, and what G6's combos do only for graphs small enough to draw whole.

### A frame budget

A frame has 16 ms. The renderer keeps to it by drawing less, not by dropping frames.

- **While the view moves** (a pan, a zoom, a layout step), it draws the nodes, the highlights, and the prefix of the edge buffer that fits the budget. The prefix length follows the measured cost of the last frames, so it finds each device's level by itself: everything on a discrete GPU, perhaps 200K edges on an integrated one, a few thousand in software. Edge opacity scales with the fraction drawn, so the density reads the same.
- **When the view rests**, it draws the remaining edges in slices across frames into an offscreen texture and shows that. A million edges take one frame on a GPU and a few seconds in software, without blocking input, because each slice is one budget.
- **When a pan starts**, the finished texture is redrawn shifted and scaled under the live nodes, so the full picture stays on screen while moving, as a map's tiles do, and is refined again at rest.

This is the "fast first paint, then refinement" of the brief, applied to every frame. It is also what makes the software-rendered CI numbers meaningful: the same code holds the budget everywhere, and only the amount of detail while moving differs.

### Picking, hover and culling

A uniform grid over the node positions, rebuilt when positions change (one pass, a few milliseconds at a million nodes), gives:

- picking and hover in constant time (today 4.9 ms at 1M nodes, fine for a click and too slow for every pointer move);
- edge picking near the pointer, by testing the edges of the nodes in neighbouring cells;
- culling: when zoomed in, only the cells in view are drawn.

### Labels

The core decides which labels to show: nodes in view, largest first, each claiming a rectangle in a screen grid, until a few hundred are placed. Highlighted, selected and pinned nodes always get theirs. The front end draws the text: the web shell in a 2D canvas over the WebGPU one (crisp text, the page's font, no glyph atlas in the wasm), a native window with a text renderer of its own. The label set is recomputed when the camera rests.

### What a browser tab can hold

| | today | Phase 3 target |
|---|---|---|
| Edge cap | 2M | 5M by default on a desktop, more with `SET ursa.graph_max_edges` |
| Renderer's heap | 50 B/edge | 16 B/edge (the index buffer's source, freed after upload where the backend allows) |
| Layout worker's heap | 100 B/edge | 60 B/edge |
| Position updates | six copies of 8 B/node each 100 ms | two |

At 5M edges and 2M nodes that is about 80 MB for the renderer and 300 MB for the layout, in a 4 GB wasm address space. The layout is the limit, not the drawing: its finest level at 5M edges is about a second an iteration on one thread, by the 1M-edge rate. So at that size the view is usable from the coarse levels within a second, and the fine level arrives over a minute, or from saved positions (`POSITIONS (SELECT id, x, y …)`, with #395's `layout_fa2` as a table function), or from threads (section 6).

### How this compares

| | Bloom / Browser | Graphistry | Cosmograph | sigma.js / G6 | Ursa, after Phase 3 |
|---|---|---|---|---|---|
| Edges drawn | hundreds to 10K | millions (server GPU) | ~1M (client GPU) | tens of thousands | 5M (client, any GPU) |
| Needs a server | yes | yes, with GPUs | no | no | no |
| Expand from a larger graph | yes | no | no | with your own back end | yes, 10 ms, in the tab |
| Aggregation | no | clustering on the server | no | combos, small graphs | hierarchy and community, any size |
| Deterministic layout | no | no | no | seeded, CPU | yes, bit for bit |
| Query language | Cypher, one way | none | SQL filters | none | SQL, both ways |

## 6. Architecture

### `ursa-viz`: a core and its front ends

```text
ursa-core      Fa2State, Fa2Multilevel (+ a fixed-node mask), the hierarchy exposed for LOD
   │
ursa-viz       default features: no GPU, no browser, no Arrow
   ├─ model      GraphResult, GraphDelta, highlights, encodings, the UGR wire format
   ├─ scene      nodes and edges shown, styles, flags (selected, pinned, hidden, dimmed), camera
   ├─ interact   the selection, pins, hover, the history and undo
   ├─ lod        the edge order, the frame budget, the hierarchy cut, groups, the label set
   ├─ spatial    the grid: pick, hover, cull
   ├─ layout     LiveLayout (flat, multilevel, warm), rings, layers
   ├─ render     feature "render": wgpu pipelines, the highlight layer, the cached edge texture
   ├─ web        feature "web": wasm-bindgen for a canvas and for a worker
   └─ ui/        a small framework-free ES module: inspector, legend, search, toolbar, labels
   │
ursa-engine    graph::view: the result → graph rules, .graph's clauses, the explore session
   │             (moved from ursa-wasm/src/graph.rs; depends on ursa-viz's model only)
   │
front ends     ursa-wasm + web/      the shell: commands, the pane, the terminal
               ursa-py               the notebook widget, HTML export (#343)
               ursa-cli              .graph … TO 'graph.html'; later ursa ui, a native window
```

The rules:

- **The core never calls an engine.** It takes results and deltas, and gives back what to draw and what the user did. A front end with an engine (the shell's worker, a notebook's kernel) runs the SQL; one without (an exported page) still has selection, hide, pin, search within the view and undo.
- **SQL lives in `ursa-engine`.** `ursa-viz` knows nothing of SQL or Arrow, as today. `ursa-engine::graph::view` reads Arrow results, applies the rules, and writes a `GraphResult`. Moving `.graph`'s resolution there from `ursa-wasm` is what lets the CLI and Python use it.
- **A front end is thin.** It owns the window or canvas, input events, text and the panels. The shell and the widget share the panels as `ursa-viz/ui/`, plain DOM with no framework, styled by CSS custom properties, which each host sets.

### The wire format

A `GraphResult` and a `GraphDelta` cross four boundaries: engine worker → page, kernel → widget, file → exported page, and later a native server → browser. One format serves all four: **UGR**, a small JSON header followed by aligned little-endian sections.

```text
"UGR1" · header length · header (JSON: counts, columns and their types, highlights, encodings, provenance)
       · sections: node keys (offsets + UTF-8), node labels (u16), edge src (u32), edge dst (u32),
                   edge rows (u64), columns (f32 | u32 | dictionary + codes), positions (f32 x, f32 y)
```

- Rust reads sections with `bytemuck`; JavaScript reads them as typed arrays over the buffer, with no parsing and no copy.
- It is a transferable `ArrayBuffer`, a notebook comm's binary buffer, or a base64 block in an HTML file.
- Arrow IPC was the alternative. It would put an Arrow reader in the viz wasm (several hundred KB) to read five flat arrays, and Arrow's schema metadata would still need the JSON. Producers that have Arrow (the engine, `ursa-py`) convert once, where Arrow already is.

### Threads, instances and memory

Today there are three wasm instances, each with its own memory: the engine in a worker (`ursa-wasm`), the renderer on the page (`ursa-viz`), and the layout in a second worker (`ursa-viz` again). Three questions decide the architecture.

**1. Must they be three threads?** Yes. One layout iteration at 1M edges is 0.2 to 0.5 s of uninterruptible wasm (525 ms flat, 2.6 times less at the multilevel's `theta`); `louvain` on the same graph is 620 ms. A renderer on the same thread as either would freeze for that long. So query, layout and drawing each need a thread, and without shared memory each thread is an instance with its own heap.

**2. Does copying between them matter?** No. Measured: 4.5 ms to copy 1M edges, 0.1 ms to transfer them, 1 ms for the positions of 46K nodes. The costs are elsewhere:

- the page rebuilds a topology it does not need: 190–350 ms, blocking, and 50 MB;
- the layout worker rebuilds a topology the engine already has: part of 0.7 s, and 100 MB;
- positions are copied six times per update (the layout's state → a `Vec` → a `Float32Array` → `new Float32Array(…)` → the page's wasm → the view's `x` and `y` → the GPU's staging buffer).

**3. Would one shared memory fix that?** It would, and it would also let the layout run on every core, which is the larger prize: native `Fa2State` already runs on all cores with the same bits. But a shared wasm memory is a `SharedArrayBuffer`, which needs three things:

- **Cross-origin isolation** (`Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp`). For the shell this is free: every resource is same-origin already, as the CSP requires. Tested with the two headers added in front of the preview server (`graph-explorer/coi-proxy.ts`): `crossOriginIsolated` is true, a shared `WebAssembly.Memory` can be made, the datasets load with range requests, `.graph` draws, `.share` makes its link, and the console reports no error. The headers are compatible with the CSP as it stands; `frame-ancestors 'none'` already forbids embedding.
- **A threaded wasm build:** `-C target-feature=+atomics,+bulk-memory` with a rebuilt standard library, which is nightly-only (`-Z build-std`). The workspace pins stable 1.97. DataFusion, tokio and object_store would run threaded in the browser for the first time.
- **A host that sends the headers.** A notebook does not (Jupyter, Colab and VS Code's webviews are not cross-origin isolated), and neither does an HTML file opened from disk. So the widget and the export can never rely on it, and the unshared build has to stay.

**Decision.**

- **Now:** keep separate instances, and stop rebuilding. The renderer instance holds only what it draws (Phase 0). Where the engine has an index it hands the layout worker a CSR, not an edge list to sort again, and positions travel in two copies, not six (Phase 3). This reaches the memory targets in section 9 without a new toolchain, and works in every host.
- **Now, too:** send the two isolation headers from the shell. They cost nothing, they are verified, they make `performance.now()` precise (the timers that measure this work are otherwise rounded to 100 µs) and `performance.measureUserAgentSpecificMemory()` available for the memory metric, and they are the precondition for the next step.
- **Later, time-boxed** (the last issue of Phase 3): a threaded build of the layout only, in which the layout worker becomes a pool over one shared memory. The engine stays as it is. This isolates the toolchain risk to `ursa-core` and `ursa-viz`, which are small and have no async runtime, and it targets the one number that copies cannot improve: layout time.
- **Rejected:** one module with the engine and the renderer drawing to an `OffscreenCanvas` in the engine's worker. It is the literal reading of #280's "sharing memory with the engine", and it gives true zero-copy from the engine's index to the GPU. But every frame would wait for whatever SQL is running, text and the accessible DOM would still live on the page, and what it saves is the 5 ms copy.

The README's plan of one view for every front end stands. What changes from #280's title is the reading of "sharing memory": one owner for each structure, and transfers between owners.

### The renderer

- **Backends:** as today, WebGPU else WebGL2 through wgpu, Vulkan, Metal and DX12 natively. Everything stays expressible in WebGL2 (vertex and index buffers, instancing, one render target): no storage buffers, no compute. WebGPU has shipped in Chrome, Edge, Safari 26 and Firefox on Windows and macOS, but Firefox on Linux and Android was still in progress in late 2025 [17], and older phones and CI need the fallback.
- **Layers,** back to front: the cached edge texture or the live edge prefix; highlighted edges (instanced quads with width); nodes (instanced discs, as today, plus a per-node flags attribute for selected, hovered, pinned, dimmed and group); arrowheads when few edges are in view; labels, by the front end.
- **Per-node flags** are one byte per node in their own buffer, so a selection or a hover uploads a few bytes, not the style buffers.
- **Appending:** node and edge buffers grow by doubling, and a delta writes only its range. Hidden nodes keep their slot with a flag, so indexes never change and undo is a truncation.

### The web shell

- `ursa-shell` gains the explorer's host commands (`.expand`, `.hide`, …) as it has `.graph`: it lists them in `.help` and hands them to the page.
- `engine-worker.js` gains one op, `explore`, which returns a UGR delta as a transferable buffer.
- `graph.js` shrinks to wiring: events to commands, deltas to the view, and `ursa-viz/ui/` for the panels. The layout worker stays, and gains warm starts and the hierarchy cut.
- The CSP is unchanged. The label overlay is a canvas; the inspector is DOM built with `textContent`, never HTML from data.

## 7. The shell, the keyboard and phones

### Graph and terminal

- **Graph → terminal:** content actions echo as commands (section 4). The echo is one line, and the shell's answer one more. A setting, `.graph echo off`, silences them for a long session; they are still recorded in history.
- **Terminal → graph:** `.graph …` replaces the drawing; `.graph + QUERY` merges into it; `.path`, `.expand` and the rest act on it. With `.graph follow on`, every result the rules can draw replaces the drawing, as Neo4j Browser draws every query. It is off by default: a session that prints a hundred tables should not redraw a hundred graphs, and a result that *can* be drawn is not always one the user wants drawn.
- **A hint, once:** the first time a result could be drawn and the pane is closed, the shell adds a dim line: `.graph draws this as a path on karate`.
- **Focus:** as today, the terminal keeps the focus when a command opens the pane. F6 moves between the terminal, the pane and the inspector. Esc in the pane clears the selection, then closes the inspector, then closes the pane.

### Keyboard

The canvas is one tab stop (`role="application"`), as today. Inside it:

| key | does |
|---|---|
| arrows, `+` `-`, `0` | pan, zoom, fit (as today) |
| Alt+arrows | move the selection to the nearest node in that direction |
| `[` `]` | previous and next neighbour of the selected node |
| Enter | expand the selection; Shift+Enter with no limit |
| `i` | focus the inspector |
| `/` | search |
| `f` | fit the selection |
| `h` or Delete, `p` | hide, pin |
| `u` or Ctrl+Z, `U` or Ctrl+Shift+Z | undo, redo |
| `?` | this list (`.help shortcuts` has it too) |

A screen reader hears each change through the status line, which is already a live region: `Selected node 33: club Officer, degree 17, 5 neighbours not shown.` For a view of at most 200 nodes, the pane also has a list of the nodes (a listbox, hidden visually until focused) with the same commands, so a graph can be explored without the canvas. Above that the terminal is the accessible route: every action is a command, and `explorer.nodes` is a table.

### Phones

- **Gestures:** one finger pans, two pinch to zoom and pan (today there is no zoom at all), a tap selects, a double tap expands, a long press opens a menu (expand, hide, pin, path from here, inspect). Targets are 44 px: a tap picks the nearest node within 22 px, not only the one under the finger.
- **Layout:** under 640 px the pane is under the terminal, as today, with a button that gives it the whole screen and one that returns. The inspector is a sheet from the bottom, half the height, dragged up or away.
- **Budget:** the frame budget already adapts. The edge cap defaults to 500K on a phone, and the device pixel ratio is capped at 2 for the canvas.
- **Typing is hard on a phone**, so the pane's toolbar has the commands the keyboard has: search, undo, redo, fit, the layout, and the menu's actions on the selection.

## 8. Beyond the web

What each front end takes from `ursa-viz`:

| | web shell | notebook widget (#343) | HTML export (#343) | native |
|---|---|---|---|---|
| model, UGR | ✓ | ✓ | ✓ | ✓ |
| scene, interact, lod, spatial | ✓ | ✓ | ✓ | ✓ |
| render | wasm, WebGPU/WebGL2 | the same wasm bundle | the same, embedded | wgpu native |
| layout | wasm worker | the kernel, on every core, streamed | precomputed | native threads |
| `ui/` panels | ✓ | ✓ | ✓ | its own |
| engine | wasm worker | the kernel (`ursa-py`), with the stored index | none | native |
| expand | 10 ms in the tab | a round trip to the kernel | within what was exported | native |

- **The notebook widget** is the shell's pane without the terminal. `ur.view(result)` takes any Arrow table and applies the same rules, because they are in `ursa-engine`. The widget's front end is the same wasm bundle and `ui/` module, in an anywidget. An expand goes to the kernel as a message, the kernel runs `hop` on the native engine (on a `.ursa` database, the memory-mapped index: this is where 0.11's point queries pay off), and the delta comes back as a UGR buffer. The layout runs in the kernel on every core and streams positions, as #343 plans.
- **The HTML export** is one file: the wasm bundle (gzip, base64, about 1.4 MB), `ui/`, and a UGR payload with positions already computed, so it opens drawn. It has no engine. It can carry a **halo**: `halo => 1` also exports the neighbours of what is shown, hidden, so a reader can expand one step with no engine. It works from `file://`, so it never uses shared memory.
- **The CLI** gets the export for nothing: `.graph karate SIZE BY pagerank TO 'karate.html'` in the native shell writes the same file. That is the first way to use the view locally, before any window.
- **A native window** (`ursa ui`, or `show()` from Python) is `View` on a winit surface with the same core. It is not planned here; nothing in the core may assume a browser, and the `web` feature stays the only place `web_sys` appears.

## 9. Performance targets

Three reference machines:

- **CI:** headless Chromium, software WebGL2, 4 cores. What every number in section 1 is. Asserted in e2e with a margin.
- **Laptop:** an integrated GPU of the last five years (Apple M1, Intel Iris Xe) in Chrome. Measured with the benchmark harness on `claude/viz-bench` and recorded in its report. Not asserted in CI.
- **Phone:** a mid-range phone of the last three years. Measured by hand at each phase's end.

Each row says how it is measured. "Timings" is `window.ursaShell.graphTimings()`, extended in Phase 0 with interaction marks; "harness" is the viz benchmark.

| metric | today (CI) | Phase 1 | Phase 2 | Phase 3 | measured by |
|---|---|---|---|---|---|
| First frame, ≤10K edges, from Enter | 0.15–0.36 s | ≤0.3 s | ≤0.3 s | ≤0.3 s | timings, asserted in e2e |
| First frame, 1M edges, from Enter | 1.08 s | no worse | no worse | **≤0.6 s** (CI), ≤0.4 s (laptop) | timings; `tests/graph-timing.ts` |
| Layout stable, 1M edges | 6–11 s | no worse | no worse | ≤8 s (laptop); ≤3 s with threads | timings; harness |
| Frame while moving, 1M edges | 3.9 s | no worse | no worse | **≤50 ms p95** (CI), ≤16 ms (laptop) | harness: wheel and drag traces |
| Full detail after rest, 1M edges | 3.9 s, blocking | — | — | ≤5 s (CI), ≤0.3 s (laptop), never blocking input for more than 50 ms | harness; long-task observer |
| Edges drawn, cap | 2M | 2M | 2M | **5M** (laptop), 500K (phone) | harness |
| Page thread blocked making the view, 1M edges | 190–350 ms | **≤60 ms** (Phase 0) | | | long-task observer |
| Pick or hover | 0.25 ms at 46K, 4.9 ms at 1M | | ≤1 ms at 100K | ≤0.2 ms at any size | timings |
| Click → expanded frame (degree ≤100, 1M-edge graph) | — | — | **≤100 ms p95** (laptop), ≤250 ms (CI) | the same | timings: `action` to `frame` |
| Engine time of an expand | 8.6 ms (`hop`) | | ≤25 ms with `induced_edges` | | `engine-point.cjs`, in e2e |
| Inspector shows a node's row | — | — | ≤50 ms | | timings |
| Undo | — | — | one frame, no query | | e2e |
| wasm heap of the viz instances, 1M edges | 152 MB | 120 MB (Phase 0) | | **≤76 MB** (16 + 60 B/edge) | `WebAssembly.Memory` size; `measureUserAgentSpecificMemory` |
| Replay of a 20-step exploration | — | — | — | ≤2 s (Phase 4) | e2e |

Two rules for the numbers:

- **A phase is not done until its row is measured on the laptop** as well as in CI, and the measurement is in the benchmark report. CI's software renderer is a floor: it proves the budget holds, not that the view is fast.
- **No target is met by dropping correctness.** The frame budget draws fewer edges while moving and all of them at rest; the picture at rest is the same pixels as today's, and the browser harness reads it back to check.

## 10. The phased plan

### Phase 0: foundations and fixes

- **Ships:** no nonsense drawings (a float is never a node), pinch zoom on phones, a view that does not block the page; inside, the model, the wire format and the crate boundary that everything else builds on.
- **Scope:** `ursa-viz` split into core and `render`; `GraphResult`, `GraphDelta` and UGR; `.graph`'s resolution moved to `ursa-engine::graph::view` with no change in behaviour; the renderer without a topology; the isolation headers; interaction timing marks.
- **Depends on:** nothing. Coordinate the timing marks with the benchmark harness.
- **Risks:** a refactor with no visible feature. Kept small by moving code before changing it, and by e2e, which must pass unchanged.
- **Done when:** e2e passes as it is; `createView` blocks the page ≤60 ms at 1M edges; `.graph` after `pagerank(…)` no longer draws scores as nodes; a phone can zoom.

### Phase 1: results → graph

- **Ships:** type a query, type `.graph`, and get the right picture for paths, neighbourhoods, metrics and edge lists, with labels, hover and a selection.
- **Scope:** provenance metadata; the rules and the `AS` and `ON` clauses; `induced_edges`; the highlight layer; rings and layers; labels; hover, selection and tooltips; direction and weight; the docs and the tour.
- **Depends on:** Phase 0. `induced_edges` and the metadata are engine work in `ursa-sql` and `ursa-engine`: coordinate with the SQL conformance work, since the metadata touches every graph function's schema.
- **Risks:** DataFusion dropping field metadata in some operator (the fallback by names covers it, less precisely); rule changes surprising someone who relied on "first two columns" (two same-typed discrete columns still draw).
- **Done when:** each row of the "should be" table in section 1 is drawn as described, in e2e; the status line names the rule; the docs list the rules.

### Phase 2: explore

- **Ships:** the Neo4j-style loop: click to expand, inspect, search, hide, pin, undo, on a desktop and a phone, with every step in the terminal.
- **Scope:** the explore session and its commands; scene deltas and undo; warm layout with fixed nodes; the inspector; search; edge picking; the keyboard map; the phone layout; `explorer.*` views; `.graph +` and `follow`; the metrics panel from #281.
- **Depends on:** Phase 1 (the rules produce the deltas; `induced_edges`; selection).
- **Risks:** the layout jumping on expand (the fixed mask and warm start are the mitigation, and the acceptance test measures how far old nodes move); terminal noise (one line an action, and `echo off`).
- **Done when:** from `.graph` of a `hop` result on rmat, ten expansions, a path, hides and undos run by pointer and by keyboard; click → frame ≤100 ms on the laptop; a share link replays the session to the same picture.

### Phase 3: scale

- **Ships:** millions of edges that stay responsive: an instant coarse picture, smooth pan and zoom at any size, communities as collapsible groups.
- **Scope:** the edge order and frame budget; the cached edge texture; first paint from coarse levels; the hierarchy cut; `GROUP BY`; the spatial grid; the memory diet; saved positions (with #395); raised caps; the threaded-layout spike.
- **Depends on:** Phase 0. Independent of Phase 2 except for `GROUP BY`'s expand. Needs the benchmark harness for its acceptance, and #395 for saved positions.
- **Risks:** WebGL2's limits on render targets and buffer updates (kept to one target and vertex buffers); aggregated views that mislead (a disc always shows its count, and the status line says "aggregated: 348 groups of 131,072 nodes"); the spike not paying off (it is time-boxed and nothing depends on it).
- **Done when:** the Phase 3 column of section 9 is met on the laptop and in CI.

### Phase 4: history → SQL

- **Ships:** an exploration as one SQL statement, a link, a picture or a file.
- **Scope:** `.graph sql`; view state (camera, pins) in share links; the history panel; export of the view as PNG, and its nodes and edges as CSV or Parquet through `.export`.
- **Depends on:** Phase 2.
- **Risks:** a folded statement that does not reproduce the view (the test runs it and compares the sets); long links (state is compacted, and a link over the browser's limit says so and offers the SQL instead).
- **Done when:** for the Phase 2 acceptance session, `.graph sql`'s statement returns exactly the edges on screen, and a shared link reproduces the picture.

### Phase 5: beyond the web

- **Ships:** `ur.view(…)` in a notebook, `save("graph.html")`, and `.graph … TO 'file.html'` in the CLI.
- **Scope:** `ursa-viz/ui/` as a shared module; the widget on UGR with kernel-side expansion; the HTML exporter with precomputed positions and an optional halo; the CLI's export.
- **Depends on:** Phase 0 for the export and a static widget; Phase 2 for expansion in the widget. It is #343's second half, re-planned on this design.
- **Risks:** wheel size (the bundle is about 1 MB gzipped, behind an extra); notebooks' sandboxes (no workers from blob URLs in some hosts: the widget falls back to laying out in the kernel only).
- **Done when:** the same graph result gives the same picture, read back, in the shell, a notebook and an exported file.

## 11. Decisions

**Decided (2026-11-19): the maintainer accepted every recommendation below.** They were put as open questions, each with a recommendation; they stand as decisions now, and the issues are filed on them.

1. **A new SQL function for the edges among a set of nodes?** Decided: yes, `induced_edges(g, nodes => …)`. Without it expansion needs an edge-table scan (180 ms at 1M edges in the browser, growing with the graph). The alternative name is `subgraph_edges`; a `hop(…, edges => true)` variant would not serve rule 5.
2. **Provenance as Arrow field metadata, visible in exported Arrow and Parquet files?** Decided: yes, under an `ursa.` prefix. It is the standard place for it, and a file that remembers its graph is a feature. Strip it only if a consumer breaks.
3. **Do the rules replace "the first two columns"?** Decided: yes, as a behaviour change in 0.12, keeping two same-typed discrete columns as edges. The cases it breaks are the nonsense ones.
4. **Actions as dot-commands echoed in the terminal, or silent with a history panel only?** Decided: commands, echoed, with `.graph echo off`. It is what makes history, sharing and tests free.
5. **Where does the model live: `ursa-viz` or `ursa-core`?** Decided: `ursa-viz`'s core, with `ursa-engine` depending on it without default features. `ursa-core` stays algorithms only.
6. **Send COOP and COEP from the shell now?** Decided: yes. Verified compatible, and it keeps the threaded option open. The cost is that the shell can never embed a cross-origin resource without CORP, which the CSP already forbids.
7. **A threaded wasm build at all?** Decided: a one-week spike at the end of Phase 3, layout only, on a nightly toolchain kept out of the release build. Decide on its numbers. Do not start with it.
8. **`follow` mode off by default?** Decided: off. Neo4j Browser draws every result because every result is a graph; a SQL shell's are mostly tables.
9. **The default expansion cap, and its order?** Decided: 100, by degree then key. The number is Neo4j's; the order is what makes it reproducible.
10. **No new epic?** Decided: none. #280 takes Phases 0, 1 and 3, #281 takes 2 and 4, #343 takes 5, and #277 stays the epic. Each of the three gets a checklist of its issues.
11. **Is 5M edges the right ceiling for Phase 3?** Decided: yes for the browser. Past it the layout, not the drawing, is the limit, and the answer is saved positions or the native engine, not a larger tab.

## Appendix: reproducing the measurements

With the shell built and served (`cd web && bun run build && bun scripts/preview.ts 8800`), and Playwright available to node:

```sh
node design/graph-explorer/engine-point.cjs   # hop, GRAPH_TABLE, properties, induced edges: engine latency
node design/graph-explorer/view-cost.cjs      # .graph timings, redraw and pick cost by edge count
node design/graph-explorer/handoff.cjs        # copies, createView, wasm heap
node design/graph-explorer/rules-today.cjs    # what .graph draws for each kind of result
bun  design/graph-explorer/coi-proxy.ts 8801  # the preview behind COOP/COEP; open http://localhost:8801
```

The hierarchy table is `cargo run -p ursa-core --release --example fa2_multilevel rmat17 1`.
