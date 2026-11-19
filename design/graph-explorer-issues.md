# Graph Explorer: issue drafts

Drafts for the design in [`graph-explorer.md`](graph-explorer.md), ready to file. Section numbers (§) refer to it.

- **No new epic.** #277 stays the epic. #280 takes Phases 0, 1 and 3; #281 takes Phases 2 and 4; #343 takes Phase 5. Each of the three gets the checklist under "Parent updates" below.
- **Order:** by phase, and within a phase in the order listed, unless "Depends on" says otherwise. Ids such as `P1-2` are for cross-references in this file; replace them with issue numbers when filing.
- **Size:** each is one pull request, about one to three days of an agent's work.
- **Every issue** keeps `web/tests/e2e.ts` passing and, where it changes behaviour a user sees, updates `web/README.md`, `.help graph` and the docs page in the same pull request.

| phase | issues | parent |
|---|---|---|
| 0. Foundations and fixes | 7 | #280 |
| 1. Results → graph | 9 | #280 |
| 2. Explore | 10 | #281 |
| 3. Scale | 9 | #280 |
| 4. History → SQL | 4 | #281 |
| 5. Beyond the web | 4 | #343 |
| **Total** | **43** | |

"Bugs and quick wins" at the end lists what the review found in today's view. The larger ones are issues above; the small ones can be fixed in passing.

## Parent updates

**For #280** (replace its body's bullets with):

> The design is `design/graph-explorer.md`. This issue tracks its Phases 0, 1 and 3.
> - **Phase 0, foundations:** P0-1 … P0-7.
> - **Phase 1, results → graph:** P1-1 … P1-9.
> - **Phase 3, scale:** P3-1 … P3-9.
> - **Targets** are the design's §9, measured by the viz benchmark and `graphTimings()`. The original target (60 fps at 1M edges on a laptop) is Phase 3's "frame while moving ≤16 ms".
> - "Sharing memory with the engine" is decided in §6: one owner per structure and transfers between instances now; a threaded shared-memory layout as a time-boxed spike (P3-9).

**For #281:**

> The design is `design/graph-explorer.md`. This issue tracks its Phases 2 and 4.
> - **Phase 2, explore:** P2-1 … P2-10.
> - **Phase 4, history → SQL:** P4-1 … P4-4.
> - Click to expand uses `hop` for the neighbours and `induced_edges` (P1-2) for the edges, because `hop` returns nodes, not edges.
> - `ursa ui club.ursa` stays later; the wire format (P0-1) is what it will speak.

**For #343** (add to part 2):

> Part 2 is re-planned on `design/graph-explorer.md` §8 as P5-1 … P5-4: the widget and the export use the same result → graph rules (in `ursa-engine`), the same wire format and the same panels as the shell.

---

## Phase 0: foundations and fixes

### P0-1 `[viz] A GPU-free core: the graph-result model, deltas and the UGR wire format`

**Parent:** #280. **Depends on:** nothing.

**Goal.** Give everything drawn one data model that the engine produces and every front end reads, in a crate that builds without a GPU or a browser.

**Scope.**
- Make `wgpu` an optional `render` feature of `ursa-viz` (on for `web`). `scene`'s non-GPU parts build with default features off.
- Add `model`: `GraphResult` (nodes with typed key, text and label; edges with `src`, `dst`, label, row; typed columns for both; highlights: paths, seeds, distances; encodings; provenance) and `GraphDelta` (add and remove nodes, edges and highlights; set columns), with `apply` and `invert`.
- Add the wire format `UGR1`: a JSON header and aligned little-endian sections (§6 "The wire format"). `to_bytes` / `from_bytes` in Rust with `bytemuck`; a 100-line reader in JavaScript (`ursa-viz/ui/ugr.js`) that returns typed-array views.
- No change to what the shell draws yet.

**Design pointers.** §3 "The model", §6. Node identity is `(label, key)`; indexes are append-only; a hidden node keeps its slot.

**Acceptance.**
- `cargo build -p ursa-viz --no-default-features` builds with no `wgpu` in `cargo tree`.
- A result round-trips through bytes, in Rust and Rust → JS, bit for bit.
- `apply(delta)` then `apply(delta.invert())` restores the result.

**Tests.** Unit tests for the model, deltas and the format (including an empty graph, string and integer keys, two labels sharing a key, a truncated buffer refused with a message). A bun test reads Rust-written fixtures with `ugr.js`.

### P0-2 `[web] Move .graph's resolution from ursa-wasm into ursa-engine (graph::view)`

**Parent:** #280. **Depends on:** P0-1.

**Goal.** The logic that turns a source and its `NODES`, `SIZE BY` and `COLOR BY` clauses into a graph is usable by the CLI and Python, not only the browser.

**Scope.**
- Move `ursa-wasm/src/graph.rs` (parsing the clauses, `Source`, `build_indexed`, `resolve`, `size_by`, `color_by`) to `ursa-engine/src/graph/view/`. `ursa-engine` depends on `ursa-viz` with default features off.
- Its output becomes a `GraphResult`. `WebShell.graph` returns it as one UGR buffer (transferred) instead of separate getters, which also removes the per-getter clones (`src()`, `dst()`, `ids()` each clone today).
- `engine-worker.js` and `graph.js` read the buffer. Behaviour, messages and the legend are unchanged.

**Acceptance.** e2e passes with no change to its expectations. `ursa-wasm` has no graph-building code left. The native `ursa-engine` tests cover what `ursa-wasm`'s unit tests covered.

**Tests.** The moved unit tests, now native. e2e unchanged.

### P0-3 `[viz] The page's view builds no topology (createView blocks the page 190–350 ms at 1M edges)`

**Parent:** #280. **Depends on:** nothing (easier after P0-1).

**Goal.** Making the view costs an upload, not a graph build.

**Scope.**
- `View::new` builds a `Topology`, an undirected CSR and a throwaway `Fa2State` to read degrees and seed positions (`ursa-viz/src/view.rs`). Measured: 190–350 ms blocking the page's thread and 50 MB of wasm heap at 1M edges (`design/graph-explorer/handoff.cjs`).
- Split `Graph` into what the renderer needs (counts, the line indices) and what a layout needs (the topology). The web view takes the first only. Degrees are computed from `src`/`dst` in one pass (the page already does this in JS; do it once, in Rust). Initial positions come from the layout's first message; until then nothing is drawn, as today.
- `View::step` (flat layout on the view's own thread, used by the demo's `&multilevel=0` path) builds the topology lazily, on first use.
- Free the line-index `Vec` after the GPU upload where the view does not need it again.

**Acceptance.**
- `createView` at 46K nodes and 1M edges blocks the page ≤60 ms in headless Chromium (long-task observer in `tests/graph-timing.ts`).
- The page's wasm heap after `createView` at 1M edges is ≤20 MB (was 50).
- Frames read back in `ursa-viz/tests/browser/view.ts` are unchanged.

**Tests.** `graph-timing.ts` prints and checks the long task. The browser harness.

### P0-4 `[web] .graph draws scores as nodes: endpoints must be ids, not floats or lists`

**Parent:** #280. **Depends on:** nothing.

**Goal.** Stop drawing nonsense. After `SELECT * FROM pagerank(karate) LIMIT 5`, `.graph` draws five edges from each id to its PageRank (9 "nodes"). After a path query it draws one edge from `5` to `[16, 5, 0, 8, 33, 26]`.

**Scope.** In `endpoint_columns` (`ursa-wasm/src/graph.rs`, or `graph::view` after P0-2): when the endpoints are found by position (no `src`/`dst` names), both columns must be of one discrete type (an integer, a string or a date; not a float, a boolean, a list or a struct). Otherwise refuse, naming the columns and types, and say what would draw: `.graph g SIZE BY pagerank` for an algorithm's result, `SELECT … AS src, … AS dst` otherwise. Columns named `src` and `dst` of a float or list type are refused the same way.

This is the stopgap for the full rules (P1-3), small enough to ship first.

**Acceptance.** The cases in the design's §1 table that draw nonsense today give the message instead. `.graph SELECT a, b FROM pairs` with two integer or two string columns draws as before.

**Tests.** Unit tests for each refusal. e2e: `.graph` after `pagerank` shows the message.

### P0-5 `[web] Graph view on a phone: pinch to zoom (there is no way to zoom today)`

**Parent:** #280. **Depends on:** nothing.

**Goal.** A phone can zoom the graph. Today one finger pans, the wheel and `+`/`-` zoom, and a phone has neither.

**Scope.** In `graph.js`, track active pointers. Two pointers: zoom by the change in their distance around their midpoint, and pan by the midpoint's movement. A second finger cancels a pending tap. Double tap fits (as double-click does). `touch-action: none` is already set. Cap the canvas's device pixel ratio at 2.

**Acceptance.** In e2e's phone viewport, a synthesized two-pointer pinch changes the camera's scale, and the point between the fingers stays under them within 2 px.

**Tests.** e2e with Playwright touch pointers (CDP `Input.dispatchTouchEvent`). A unit test of the pinch arithmetic if it moves into `Camera`.

### P0-6 `[web] Send COOP and COEP: the shell runs cross-origin isolated`

**Parent:** #280. **Depends on:** nothing.

**Goal.** Precise timers and memory measurement now, and the precondition for a shared-memory build later (§6).

**Scope.** Add `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Embedder-Policy: require-corp` and `Cross-Origin-Resource-Policy: same-origin` to `SECURITY_HEADERS` in `web/src/headers.ts`, so the Worker and the preview both send them. Document them beside the CSP. Verified compatible with a proxy in front of the preview (`design/graph-explorer/coi-proxy.ts`): datasets load by range, `.graph` draws, share links work, no console violation.

**Acceptance.** `self.crossOriginIsolated === true` on the page and in both workers. e2e passes. The headers are on every response, including `/data/*` (206 responses) and `/engine/*.wasm`.

**Tests.** e2e asserts the three headers and `crossOriginIsolated`. The existing "no CSP violation" check.

### P0-7 `[viz] Interaction timings: frame time, pick time and action → frame, in graphTimings()`

**Parent:** #280. **Depends on:** nothing. **Coordinate with:** the viz benchmark harness (`claude/viz-bench`), which should read these rather than add its own.

**Goal.** The numbers §9 sets targets for can be read from the page.

**Scope.**
- `graphTimings()` gains `frames` (count, p50, p95 and max of the last 120 frame intervals while moving, and of `render()` itself), `pick` (last and p95), and `actions` (for each action: command, engine ms, delta ms, and ms from input to the frame that showed it).
- A long-task observer on the page, exposed as `longTasks`.
- With cross-origin isolation (P0-6), `memory()` returns `measureUserAgentSpecificMemory()` and each wasm instance's `memory.buffer.byteLength`.
- `tests/graph-timing.ts` prints them.

**Acceptance.** `graph-timing.ts` reports frame p95 while a scripted wheel zoom runs on `.graph rmat`, and the wasm heaps. No measurable cost when nothing reads them (ring buffers, no allocation per frame).

**Tests.** e2e checks the fields exist and are sane on karate.

---

## Phase 1: results → graph

### P1-1 `[sql] Results say what their columns are: ursa.role and ursa.graph field metadata`

**Parent:** #280. **Depends on:** nothing. **Coordinate with:** #415 (one vocabulary for output columns): roles make the view independent of the names #415 changes.

**Goal.** A result carries what each column is, so a consumer does not guess from names.

**Scope.**
- First, a short spike: which DataFusion 55 operators keep Arrow field metadata (projection of a column, alias, filter, sort, limit, joins, `UNION`, subqueries, `SELECT *`)? Record the answer in the pull request. Where an alias drops it, carry it.
- Graph table functions set field metadata on their outputs: `ursa.graph` (the graph's name), `ursa.label` (the vertex or edge table), and `ursa.role`: `node` (an algorithm's `id`), `metric` or `category` (its output column, from the function catalog), `seed`, `node` and `distance` (`hop`), `src`, `dst` and `step` (`shortest_path`), `walk`, `step` and `node` (`random_walk`).
- `GRAPH_TABLE`: a `COLUMNS` item that is a node variable's key gets `role = node`, `ursa.var` and `ursa.label`; a property gets `role = property` and its `ursa.var`; `vertices(p)`, `edges(p)` and `element_id(p)` get `path_nodes`, `path_edges` and `path_elements`. The schema's metadata gets `ursa.pattern`: the pattern's edges as pairs of variables with their direction.
- An expression over such a column (`round(pagerank, 4)`, `a.id + 1`) carries no role.
- Document the keys in `docs/sql/reference` (a short "Result metadata" section).

**Acceptance.** `SELECT * FROM hop(…)`, `… JOIN pagerank(…) USING (id) ORDER BY … LIMIT …` and each `GRAPH_TABLE` example in `docs/sql/pattern-matching.md` return schemas with the roles above (a test reads them). Exported Arrow and Parquet keep them. No result's values or column names change.

**Tests.** `ursa-sql` tests per function and per `GRAPH_TABLE` form; a test that a computed column has no role; the docs test for the new section.

### P1-2 `[sql] induced_edges(g, nodes => …): the graph's edges among a set of nodes, from the index`

**Parent:** #280. **Depends on:** nothing.

**Goal.** One index read answers "which edges connect these nodes", which drawing a node set, a `hop` neighbourhood and click-to-expand all need. Today it takes an edge-table scan (180 ms for 5,000 nodes of a 1M-edge graph in the browser, growing with the graph) or seconds through `GRAPH_TABLE` with `IN` lists (`design/graph-explorer/induced.cjs`).

**Scope.**
- A traversal table function in the catalog: `induced_edges(graph, nodes => …)`. `nodes` is the existing "node ids" parameter (a list, a one-column query, or one id). Ids not in the graph match nothing, as for `hop`.
- Returns `src`, `dst` (the keys, as the graph stores the edge's direction) and `edge` (the edge's row in the edge table, as `edges(p)` numbers it). Parallel edges are separate rows. Rows are ordered by `edge`.
- Implementation: mark the nodes in a bitmap, walk the out-adjacency of each, keep edges whose other end is marked. Cost is the sum of the nodes' out-degrees.
- Field metadata as P1-1 defines (`src`, `dst`, `ursa.graph`), if P1-1 has landed.
- `ursa_functions()` and the generated function reference list it.

**Acceptance.**
- Results equal `SELECT src, dst FROM g.edges WHERE src IN (…) AND dst IN (…)` on karate, lesmis and rmat, for a list, a subquery and an empty set.
- 5,000 nodes of rmat in ≤20 ms in the wasm engine, and time independent of the graph's edge count for a fixed answer (checked natively on a stored index).

**Tests.** Engine tests against the join; parity fixtures; the docs' function-reference test.

### P1-3 `[web] The result → graph rules: edges, nodes, hop rings, pattern matches and paths`

**Parent:** #280. **Depends on:** P0-2, P1-1, P1-2.

**Goal.** `.graph` after any graph query draws what the result means (§3 "The rules").

**Scope.**
- In `ursa-engine::graph::view`, `interpret(schema, batches, session) -> GraphResult`: assign roles from metadata, else from names and types (§3 "How the engine knows"), then apply rules 1 to 6 in order.
- Rule 1 (paths): rows of `path_nodes` become highlighted paths; with `path_edges` or `path_elements`, edges get their rows.
- Rule 2 (hop): seeds, reached nodes and distances; edges from `induced_edges`.
- Rule 3 (edges), rule 4 (pattern matches, from `ursa.pattern`), rule 5 (nodes, with edges from `induced_edges` and default `SIZE BY` / `COLOR BY` from the metric and category roles), rule 6 (the refusal, with the `AS` clause that would draw it).
- The base graph: `ursa.graph`, else the session's only property graph.
- The ambiguity table of §3 as tests.
- The result records the rule and each column's role; the status line shows `drawn as …`, and the terminal's "Drawing …" line names it.
- The existing caps apply to what is drawn.

**Acceptance.** Every row of the "should be" column in the design's §1 table, on karate. On rmat, `.graph` after `hop(rmat, seeds => [1], n => 2, direction => 'both')` draws the neighbourhood with its real edges.

**Tests.** Native tests of `interpret` for each rule and each ambiguous case, from schemas with and without metadata. e2e for a path, a hop, a node table and a three-variable match.

### P1-4 `[web] .graph … AS edges(…) | nodes(…) | path(…) | hops(…), ON graph and LAYOUT`

**Parent:** #280. **Depends on:** P1-3.

**Goal.** The user can say how to read a result when the rules cannot, or choose otherwise.

**Scope.** Extend the clause parser (`parse` in `graph::view`): `AS edges(src, dst [, weight])`, `AS nodes(id)`, `AS path(nodes [, edges])`, `AS hops(seed, id, distance)`; `ON graph`; `LAYOUT force|rings|layers`. Clauses in any order after the source, as today. `AS` is rule 0 and skips inference. Errors name the column that is missing or of the wrong type and list the result's columns. `.help graph` documents the clauses.

**Acceptance.** Each form draws on a result whose columns have arbitrary names (`SELECT a.id AS x, b.id AS y …` with `AS edges(x, y)`), and each misuse has a message that says what to write.

**Tests.** Parser unit tests (the existing ones extended), `interpret` tests with overrides, e2e for one of each.

### P1-5 `[viz] A highlight layer: wide edges, halos, dimming, and per-node flags`

**Parent:** #280. **Depends on:** P0-1.

**Goal.** A path, a selection or a hover can be seen.

**Scope.**
- A per-node flags buffer (one byte: selected, hovered, pinned, dimmed, highlighted, hidden) as an instance attribute; the node shader draws a halo or ring and applies dimming from it. Updating flags uploads only that buffer.
- Highlighted edges as instanced quads with a width in pixels and a colour, in their own pipeline, drawn over the line list. Expressible in WebGL2 (no storage buffers).
- When any highlight is set, everything else draws at 35% (a uniform).
- Theme tokens for highlight, selection and pin colours (`--graph-highlight`, …), in both themes, at 3:1 against the background.
- `web.rs`: `setFlags(Uint8Array)`, `setHighlightEdges(Uint32Array pairs)`.

**Acceptance.** A path of five edges on karate reads back as a 3 px line in the highlight colour with the rest dimmed, in both themes, on WebGL2. Setting a selection of one node uploads ≤ n bytes.

**Tests.** `ursa-viz/tests/browser/view.ts` reads frames back. Native shader validation (naga), as today.

### P1-6 `[viz] Rings and layers: deterministic layouts for neighbourhoods and paths`

**Parent:** #280. **Depends on:** P0-1.

**Goal.** A hop result and a set of paths appear in one frame, in a layout that shows their structure.

**Scope.**
- `rings(distance per node, edges)`: seeds at the centre, distance `d` on the circle of radius `d`, the order on each ring chosen by a fixed number of barycentre sweeps (deterministic; ties by node index). Several seeds: one ring system per seed's nearest set, packed.
- `layers(layer per node, edges)`: the same on parallel lines, for paths with shared ends (layer = position in the path) and for hop results with several seeds.
- A route (one path): a straight line, left to right.
- In `ursa-core` beside `layout.rs` if #395 will expose them as SQL functions, else in `ursa-viz::layout`. Recommended: `ursa-core`, so `layout_rings` can follow #395's pattern later.
- `LiveLayout` gains a `Fixed` engine that is done at once; the worker posts one message.

**Acceptance.** Same input, same bits on every run. On karate, `hop(…, seeds => [0], n => 2)` gives two rings with no node overlapping another; `ALL SHORTEST` from 16 to 26 gives six layers.

**Tests.** Unit tests for determinism, ring membership and non-overlap. A frame read back for each.

### P1-7 `[viz] Labels: the core chooses which, the page draws them`

**Parent:** #280. **Depends on:** P0-1, P1-5.

**Goal.** Nodes have names on screen without covering the graph.

**Scope.**
- Core: `labels(camera, budget) -> [(node, x, y, priority)]`. Nodes in view, by priority (highlighted, selected and pinned first, then by drawn radius), each claiming a rectangle in a screen grid; stop at the budget (300). Recomputed when the camera rests and when positions settle.
- A node's label text is its key, or the column named by a new `LABEL BY col` clause, or by default the first string property of its vertex table when there is one.
- Web: a 2D canvas over the view, in the page's font, with a halo in the background colour. Not drawn while the view moves if the frame budget is short.
- The canvas's accessible description is unchanged; labels are decoration (`aria-hidden`).

**Acceptance.** Karate shows all 34 labels with no overlap at the fitted zoom. rmat shows at most 300, the largest nodes', and none overlapping. Label selection for 131K nodes takes ≤5 ms.

**Tests.** Core unit tests (no overlap, priority order, budget). e2e: label pixels present on karate in both themes.

### P1-8 `[web] Hover and selection: a tooltip, a selected state, and the pane's status`

**Parent:** #280. **Depends on:** P1-5.

**Goal.** Pointing at a node says what it is; clicking selects it. This is the base the inspector (P2-5) builds on.

**Scope.**
- Hover (pointer devices): the node under the pointer gets the hover flag and a tooltip (key, label, the `SIZE BY` and `COLOR BY` values, degree). Throttled to one pick per frame.
- Click selects (replacing the "names it in the header" behaviour, which becomes the selection's status text); Shift+click adds; a click on empty space clears; a drag on empty space still pans. Selected nodes and their incident edges are highlighted.
- The status line (a live region) reads `Selected node 33: …`.
- `explorer`-independent: state lives in `ursa-viz`'s `interact` module (selection set), exposed through `web.rs`.

**Acceptance.** e2e: hovering a known node shows its tooltip; clicking selects it and the status names it; Shift+click selects two; Esc clears before it closes the pane.

**Tests.** e2e; unit tests of the selection set.

### P1-9 `[viz] Direction and weight: arrowheads when few edges are in view, edge width and opacity by weight`

**Parent:** #280. **Depends on:** P1-5.

**Goal.** A directed graph looks directed, and a weighted one weighted, at the sizes where it can be read.

**Scope.**
- Arrowheads as instanced triangles at the destination end, stopped at the node's disc, drawn only when at most 5,000 edges are in view and the graph's edges are directed (the result says so: an edge list from a property graph is; `direction => 'both'` results are not).
- An optional per-edge weight (rule 3's `weight`, or `AS edges(src, dst, w)`): opacity for the line list (a per-vertex attribute is not possible with shared position buffers, so weighted edges above a threshold go to the quad pipeline of P1-5, the rest stay lines at base opacity). The legend names the weight column and its range.

**Acceptance.** Karate's stored direction shows arrowheads at the fitted zoom and none on rmat until zoomed in. A weighted lesmis draws its heaviest ties wider, and the legend says by what.

**Tests.** Frames read back; unit tests of the in-view count and the threshold.

---

## Phase 2: explore

### P2-1 `[web] The explore session: commands → SQL → deltas (.expand, .hide, .show, .undo, .redo)`

**Parent:** #281. **Depends on:** P1-2, P1-3.

**Goal.** The engine side of exploration: a session that knows what is shown and turns each command into SQL and a delta.

**Scope.**
- `ursa-engine::graph::view::Session`: the base graph, the `(label, key)` → index map, the shown set, and the history (a list of steps: command, SQL, delta, inverse).
- Commands, as `ursa-shell` host commands the page hands back, like `.graph`: `.expand ID [in|out|both] [LIMIT n]` (`hop` for the neighbours, capped at 100 by degree then key, then `induced_edges` over the shown set plus the new nodes), `.hide ID…`, `.show ID…|all`, `.undo`, `.redo`, `.graph history`.
- Each prints one line (`+16 nodes, +41 edges`, or why nothing changed) and returns a UGR delta. `--sql` prints the SQL instead of running it.
- `WebShell` gains `explore(command) -> delta`; `engine-worker.js` an `explore` op.
- A command with no open graph, or an id not in the graph, says so.

**Acceptance.** In the native shell tests (no browser), a script of `.graph` on a hop result, three expansions, a hide and two undos yields the expected node and edge sets at each step, and the same sets when replayed.

**Tests.** `ursa-shell` and `ursa-engine` tests; an e2e smoke test that `.expand` typed in the terminal changes the node count.

### P2-2 `[viz] Scenes change in place: applying deltas, append-only buffers, undo`

**Parent:** #281. **Depends on:** P0-1, P1-5.

**Goal.** The view applies a delta in time proportional to the delta, not to the graph.

**Scope.**
- `View::apply(&GraphDelta)`: append nodes and edges (GPU buffers grow by doubling; a delta writes only its range), set the hidden flag for removed nodes, drop edges at hidden nodes from the drawn index range, update styles for changed columns.
- Undo is `apply(inverse)`; indexes never change, and a step's saved positions are restored.
- `web.rs`: `applyDelta(bytes)`.
- The legend, the counts in the status line and `info()` follow.

**Acceptance.** Applying a 100-node, 300-edge delta to a 1M-edge view uploads ≤64 KB and takes ≤2 ms outside the frame. A delta and its inverse leave the frame bit-identical (read back).

**Tests.** Native tests of the scene's bookkeeping; browser harness for the read-back.

### P2-3 `[core] ForceAtlas2 with fixed nodes and a warm start, for layouts that grow`

**Parent:** #281. **Depends on:** nothing.

**Goal.** New nodes find their place without moving what is already drawn.

**Scope.**
- `Fa2State`: a per-node `fixed` mask (no displacement, still exerts forces) and a per-node inertia factor. `from_positions` exists; add `extend(new positions, masses)`.
- `LiveLayout::grow(delta)`: place each new node on a golden-angle spiral around its parent (the expanded node), as the multilevel prolongation does; hold pinned nodes fixed and old nodes at ten times the inertia; run a bounded schedule (60 iterations for a small delta).
- Deterministic: the same sequence of deltas gives the same bits, on any thread count.
- The layout worker takes `{type: "grow", delta}`.

**Acceptance.** After expanding a node of degree 16 in a 500-node view, old unpinned nodes move on average less than 5% of the view's width, pinned nodes not at all, and the new nodes do not overlap. Bit-identical across runs and thread counts.

**Tests.** `ursa-core` tests for the mask and determinism; `ursa-viz` test for the displacement bound.

### P2-4 `[web] Click to expand: double-click or Enter, a neighbour cap with a +N badge, echoed in the terminal`

**Parent:** #281. **Depends on:** P2-1, P2-2, P2-3, P1-8.

**Goal.** The Neo4j loop: double-click a node and its neighbours appear.

**Scope.**
- Double-click a node (double tap on a phone), or Enter with a node selected, runs `.expand ID` through the terminal's queue, echoed as a typed command would be (dimmed "from the graph" prefix), with the shell's one-line answer. Shift expands with `LIMIT 5000`. Double-click on empty space still fits.
- A badge on a node with neighbours not shown: `+N`, from the session's degree in the graph against the view. Drawn by the label overlay.
- `.graph echo off|on`.
- While a command runs, further double-clicks queue (the engine is serial); the node shows a busy ring.
- Timings recorded as an action (P0-7).

**Acceptance.** On rmat after `.graph SELECT * FROM hop(rmat, seeds => [1], n => 1, direction => 'both')`: ten double-clicks each add nodes, each echoes one command, and `.history` lists them. Click → frame ≤250 ms p95 in CI. A shared link of the session replays to the same node and edge counts.

**Tests.** e2e for the loop, the echo, the badge and the replay.

### P2-5 `[web] The inspector: a node's and an edge's properties, and edge picking`

**Parent:** #281. **Depends on:** P1-8, P2-1.

**Goal.** Selecting something shows everything known about it.

**Scope.**
- A panel in the pane (right side; P2-9 makes it a sheet on phones), part of `ursa-viz/ui/` if P5-1 has landed, else in `web/public/` in a form P5-1 can move.
- A node: key, label, its vertex-table row (one query by key), the drawn metrics, degree in the graph and in the view, and buttons: expand, hide, pin, path from here.
- An edge: its ends, and its edge-table row (by the edge's row number where known, else by its ends).
- Several selected: counts, and a copyable `WHERE id IN (SELECT id FROM explorer.selection)`.
- Each property has "colour by" and "size by" buttons, which run `.graph` with the clause changed.
- Edge picking: the nearest edge within 4 px of the pointer when no node is under it, among the edges of nodes near the pointer (a linear pass until P3-6's grid).
- Values are set with `textContent`; no HTML from data. Long values wrap and can be selected.

**Acceptance.** Clicking member 33 on karate shows `club: Officer`; clicking a tie shows its row. The row appears ≤50 ms after the click on a 1M-edge graph. Keyboard: `i` focuses the panel, Esc returns.

**Tests.** e2e for a node, an edge, a multi-selection, and a value containing markup shown as text.

### P2-6 `[web] Search and focus: / in the pane, .find, and fit to the selection`

**Parent:** #281. **Depends on:** P1-8, P2-1.

**Goal.** Find a node by what the user knows about it.

**Scope.**
- `/` opens a search box in the pane's header. Text matches keys by prefix, then the label column by substring, in the view first; Enter selects the matches and fits them. Matches in the graph that are not drawn are listed under "not shown", each with "add" (`.graph + …`).
- `.find TEXT` and `.find WHERE condition` (a condition on the vertex table's columns) from the terminal.
- `f` fits the selection (an animated camera move, 200 ms, skipped under `prefers-reduced-motion`).
- At most 1,000 matches are selected; the status says how many there were.

**Acceptance.** On karate, `/33` selects node 33 and centres it, and `.find WHERE club = 'Officer'` selects the officers' members. On rmat a key search answers in ≤50 ms.

**Tests.** e2e; engine tests for `.find`'s SQL.

### P2-7 `[web] Pin, hide, a context menu, and the pane's keyboard map`

**Parent:** #281. **Depends on:** P2-1, P2-3, P1-8.

**Goal.** The remaining Bloom-style actions, by pointer and by key.

**Scope.**
- Drag a node to move it; on release it is pinned (`.pin ID AT x y`, echoed). `p` toggles the pin on the selection. A pinned node draws a ring.
- `h` or Delete hides the selection; `.hide others` keeps only it.
- A context menu on right-click (and the menu key): expand, expand all, hide, hide others, pin, path from here / to here, inspect, copy id. Built as a menu with roles and arrow-key navigation.
- The keyboard map of the design's §7: Alt+arrows to the nearest node in a direction, `[` `]` through a node's neighbours, Enter, `i`, `/`, `f`, `u`/Ctrl+Z, `U`/Ctrl+Shift+Z, `?`.
- `.help shortcuts` lists them (the `SHORTCUTS` text, which e2e checks against the keys).
- For a view of at most 200 nodes, a visually hidden listbox of the nodes with the same commands, and status-line announcements for every action.

**Acceptance.** Every action in the menu is reachable by keyboard alone, and e2e drives an expand, a hide, a pin and an undo without the pointer. A dragged node stays where it was dropped through the next expansion.

**Tests.** e2e keyboard-only run; the shortcuts check; an axe-style check that the menu and listbox have roles and names.

### P2-8 `[web] The open graph is a table: explorer.nodes, explorer.edges, explorer.selection; .graph + QUERY; .path; follow`

**Parent:** #281. **Depends on:** P2-1, P1-3.

**Goal.** The terminal can query the picture, and a query can change it.

**Scope.**
- The session registers temporary views: `explorer.nodes` (`id`, `label`, `selected`, `pinned`, `x`, `y`, and the size and colour columns), `explorer.edges` (`src`, `dst`, `edge`), `explorer.selection` (`id`). They are in-memory tables refreshed on each step, and gone when the pane closes. In a database session they are temporary (not stored).
- `.graph + QUERY` merges a result into the open graph by the rules: new nodes and edges are added, and a path result becomes a highlight over what is drawn.
- `.path A B` runs `ANY SHORTEST` between two nodes and highlights it, adding its nodes. With two nodes selected, the menu's "path between".
- `.graph follow on|off` (off by default): when on, each drawable result replaces the drawing.
- Once per session, when a drawable result is printed with the pane closed, a dim hint line says what `.graph` would draw.

**Acceptance.** `SELECT count(*) FROM explorer.nodes` equals the node count in the status line after each of a series of expands and hides. Selecting three nodes then `SELECT * FROM members JOIN explorer.selection USING (id)` returns their rows. `.path 16 26` on an open karate highlights six nodes.

**Tests.** Shell tests for the views and commands; e2e for the selection round trip and the path overlay.

### P2-9 `[web] The explorer on a phone: full-screen graph, a bottom sheet, long-press menu, a toolbar`

**Parent:** #281. **Depends on:** P0-5, P2-5, P2-7.

**Goal.** Exploration works with thumbs.

**Scope.**
- Under 640 px: a button gives the pane the whole screen, and one returns to the terminal.
- The inspector is a sheet from the bottom at half height, dragged up to full or away to close.
- Tap selects the nearest node within 22 px; double tap expands; long press (500 ms, cancelled by movement) opens the context menu as a sheet.
- A toolbar in the pane: search, undo, redo, fit, and the menu for the selection. Targets are at least 44 px.
- The default edge cap is 500K when the device reports a coarse pointer and at most 4 GB of memory.

**Acceptance.** e2e at 390×844 with touch: open a graph, expand by double tap, inspect in the sheet, hide from the long-press menu, undo from the toolbar. No horizontal scroll at 320 px.

**Tests.** e2e phone run; target sizes measured.

### P2-10 `[web] A metrics panel for the view: counts, a degree histogram, community sizes`

**Parent:** #281 (its "metrics panel" bullet). **Depends on:** P2-8.

**Goal.** A summary of what is drawn, without typing.

**Scope.** A collapsible section of the inspector when nothing is selected: nodes and edges shown and in the graph, density, components of the view, a degree histogram (20 bins, as DOM bars with text values), and, when coloured by a category, each category's size. Computed by queries over `explorer.nodes` and `explorer.edges` (shown with a "copy SQL" button), run after each step when the panel is open, skipped above 200K edges in view unless asked.

**Acceptance.** The numbers match the same SQL typed in the terminal. Opening the panel on a 100K-edge view takes ≤200 ms.

**Tests.** e2e comparing the panel to a typed query.

---

## Phase 3: scale

### P3-1 `[viz] A frame budget: edges in importance order, and a prefix drawn while the view moves`

**Parent:** #280. **Depends on:** P0-3, P0-7. **Needs:** the benchmark harness for its acceptance.

**Goal.** Pan and zoom hold the frame time at any graph size, on any GPU.

**Scope.**
- Order the edge index buffer by importance: the hierarchy level at which the edge's ends merge (coarsest first), then weight, then a hash, so a prefix is a spatially even backbone. The layout worker computes the order once (it has the hierarchy) and sends the permutation; with no hierarchy (flat, rings), by the ends' larger radius.
- While the camera or the positions change, draw the prefix `0..k`. `k` follows the measured cost of the last frames (GPU timestamp queries on WebGPU where available, else frame intervals) toward a 12 ms target. Edge opacity scales with `k / m` so the density reads the same.
- At rest, draw everything (P3-2 makes that incremental).
- `info()` reports `k` and the frame p95.

**Acceptance.** On `.graph rmat`, a scripted wheel zoom holds p95 ≤50 ms in CI's software renderer (3.9 s today) and ≤16 ms on the laptop. The frame at rest is pixel-identical to today's.

**Tests.** The benchmark's interaction trace; a read-back of the rest frame; unit tests of the controller (converges, never 0 edges, never oscillates by more than 10% at steady load).

### P3-2 `[viz] Refinement at rest: full detail drawn in slices into a cached texture, reused while panning`

**Parent:** #280. **Depends on:** P3-1.

**Goal.** The full picture arrives without blocking input, and stays on screen while the view moves.

**Scope.**
- When the view rests, draw the edges beyond the prefix in budget-sized slices across frames into an offscreen target, then present it under the nodes. Any input cancels the remaining slices.
- When a pan or zoom starts, draw the finished texture transformed by the camera's change (a textured quad), under the live prefix and nodes, until the next rest.
- One extra render target at canvas size (WebGL2-safe). Released when the pane closes or the context is lost.
- The status line shows `refining…` while slices remain.

**Acceptance.** At 1M edges in CI, no task on the page's thread exceeds 50 ms during refinement, and full detail completes ≤5 s after rest. On the laptop it completes in ≤0.3 s. The refined frame equals a single full draw (read back, within blending tolerance).

**Tests.** Long-task observer in the benchmark; read-back comparison.

### P3-3 `[viz] First paint from the coarse levels: draw the level being laid out`

**Parent:** #280. **Depends on:** P0-3.

**Goal.** A large graph shows a meaningful picture within about half a second, and refines.

**Scope.**
- `Fa2Multilevel` exposes each level's nodes (position, mass, parent) and merged edges (ends, weight). The worker posts the current level's aggregated graph, not all positions prolonged to the input graph.
- The view draws that level: discs with area by mass, the heaviest merged edges within the frame budget, width by weight.
- When the layout moves to a finer level, children animate from their parent's position (150 ms; none under `prefers-reduced-motion`).
- The input graph's own edges are first drawn when the layout reaches level 0, or the cut (P3-4) asks for them.
- The status line reads `… · level 3 of 9 · 348 groups`.

**Acceptance.** `.graph rmat`: first frame ≤0.6 s after Enter in CI (1.08 s today) and each frame before level 0 costs ≤50 ms there. The final layout is bit-identical to today's.

**Tests.** `graph-timing.ts`; determinism test of the final positions; frames read back at a coarse level.

### P3-4 `[viz] The aggregated view: a cut through the hierarchy by zoom`

**Parent:** #280. **Depends on:** P3-3, P3-1.

**Goal.** Zoomed out, a million-edge graph is a readable picture of its groups, and zooming in opens them.

**Scope.**
- In the layout worker (which holds the hierarchy), `cut(camera)`: the coarsest set of hierarchy nodes whose discs are at least `s` pixels apart on screen, refined only for groups that intersect the view. It returns the cut's nodes (position, mass, colour by majority category) and merged edges (heaviest first).
- Recomputed when the camera rests; the previous cut is drawn, transformed, meanwhile.
- A group shows its member count as a label at sufficient size. Picking a group selects its members; the status line always says `aggregated: G groups of N nodes`.
- A toggle (`a`, and `.graph detail on|off`) forces full detail.
- Fully zoomed in, the cut is the input graph and the drawing is the same as without it.

**Acceptance.** At the fitted zoom, rmat draws ≤20K discs and ≤100K merged edges; zooming into a region reaches individual nodes; `cut` takes ≤30 ms at 1M edges. Group counts sum to the node count.

**Tests.** Unit tests of the cut (an antichain covering every node; determinism); benchmark traces; read-back at two zooms.

### P3-5 `[web] GROUP BY: aggregate by a column, and open a group in place`

**Parent:** #280. **Depends on:** P3-4, P2-1 for opening groups.

**Goal.** `.graph g GROUP BY louvain` draws communities as discs, and a double-click opens one.

**Scope.**
- `GROUP BY name`, resolved as `COLOR BY` is (a `NODES` column, a vertex column, or an algorithm: `louvain`, `connected_components`, `label_propagation`). The engine returns the groups (id, size, label) and the edges between groups with counts; edges inside a group are not sent until it is opened.
- One disc per group, area by size, labelled with the group and its size; between-group edges with width by count. The legend says `grouped by louvain: 12 groups`.
- Double-click (or `.expand group:3`) opens a group: its members are fetched (`induced_edges` over them and the open groups' members) and laid out inside the disc's area; `.collapse group:3` closes it.
- Groups beyond 2,000 are folded into "other" by size.

**Acceptance.** `.graph rmat GROUP BY louvain` draws within 2 s of the algorithm's result with one disc per community over one member; opening the largest shows its members within its disc. `.graph karate GROUP BY club` draws two discs and the ties between them, counted.

**Tests.** Engine tests of the grouped result against a `GROUP BY` query; e2e for karate; a read-back.

### P3-6 `[viz] A spatial grid: constant-time pick and hover, edge picking, viewport culling`

**Parent:** #280. **Depends on:** P0-1.

**Goal.** Pointing stays instant at millions of nodes, and a zoomed-in view draws only what is in it.

**Scope.**
- A uniform grid over node positions (cell size about the median drawn radius times 4), built in one pass when positions change at rest; during a layout, the previous grid with a linear fallback for moved nodes.
- `pick` and hover read the 3×3 cells at the pointer. Edge picking (P2-5) tests the edges of nodes in nearby cells.
- Culling: when under half the nodes are in view, draw only the instances in visible cells (nodes sorted by cell at rest, drawn as ranges).

**Acceptance.** Pick ≤0.2 ms at 1M nodes (4.9 ms today). Building the grid ≤15 ms at 1M nodes. Zoomed into 1% of rmat, the node draw covers ≤5% of the instances.

**Tests.** Unit tests against the linear scan on random and clustered positions; the benchmark's pick timing.

### P3-7 `[viz] Memory: a CSR to the layout worker, positions in two copies, and bytes per edge on the page`

**Parent:** #280. **Depends on:** P0-2, P0-3.

**Goal.** The viz instances hold ≤76 MB at 1M edges (152 MB today), and updates stop copying positions six times.

**Scope.**
- Where the engine holds an index (a property graph), send the layout worker its undirected CSR (offsets and neighbours) as UGR sections, and build `UndirectedCsr` from them without sorting. Otherwise send `src`/`dst` as today.
- Positions: the worker writes `x` then `y` into a `Float32Array` it allocated once per message (a view of wasm memory copied once), transfers it, and the page writes it straight to the GPU buffers (`queue.write_buffer` from the JS-owned array through one wasm copy at most). Remove `new Float32Array(layout.positions())`'s second copy and the view's `x`/`y` mirror where picking has the grid's own copy.
- The page drops `graph.src`/`graph.dst` after `createView`.
- Report bytes per edge for each instance in `graphTimings().memory`.

**Acceptance.** At 1M edges: page instance ≤16 MB, layout worker ≤60 MB, measured as in P0-7. A position update copies the positions at most twice. Layout results unchanged, bit for bit.

**Tests.** The memory fields asserted in `graph-timing.ts` with a margin; determinism tests.

### P3-8 `[web] Saved positions and raised caps: POSITIONS (query), 5M edges on a desktop`

**Parent:** #280. **Depends on:** P3-1, P3-2, P3-7; #395 for `layout_fa2` as a table function.

**Goal.** Draw a graph too large to lay out interactively from positions computed once, and raise the default caps to what the frame budget now allows.

**Scope.**
- `.graph … POSITIONS (SELECT id, x, y …)`: draw at saved positions with no layout (the clause #395 anticipates). Missing nodes are placed by one warm pass around their neighbours.
- `MAX_EDGES` becomes a setting, `ursa.graph_max_edges`: 5,000,000 by default, 500,000 on a phone (P2-9's test), with the refusal message naming the setting.
- Above 2M edges without `POSITIONS`, the status line says the fine level will take about a minute and offers `COPY (SELECT * FROM layout_fa2(g)) TO 'pos.parquet'`.

**Acceptance.** A 5M-edge R-MAT with saved positions reaches its first frame ≤2 s after the engine returns and holds the moving-frame target on the laptop. Positions written by `layout_fa2(g)` draw the same picture as the live layout's end state (read back).

**Tests.** e2e with saved positions on rmat; the benchmark at 5M edges (generated, not shipped).

### P3-9 `[viz] Spike: a threaded layout over one shared memory (time-boxed to a week)`

**Parent:** #280. **Depends on:** P0-6, P3-7.

**Goal.** Decide, on measurements, whether a threaded build is worth carrying.

**Scope.**
- Build `ursa-viz`'s layout (not the engine) for `wasm32-unknown-unknown` with `+atomics,+bulk-memory` and a rebuilt `std` on a pinned nightly, in a separate build script and CI job that cannot affect the release build.
- Run `Fa2Multilevel` on a pool of workers over one shared `WebAssembly.Memory` (`wasm-bindgen-rayon` or a hand-rolled pool), used only when `crossOriginIsolated` is true, with the unthreaded build as the fallback.
- Measure layout time at 100K, 1M and 5M edges on 4 and 8 cores, the bundle's size, and start-up cost. Check the positions are the same bits as the unthreaded build's.
- Write up: numbers, what broke, the maintenance cost of the nightly toolchain, and a recommendation.

**Acceptance.** A report in `design/` with the measurements and a yes or no. If yes, a follow-up issue for productionising. Nothing in the release build changes.

**Tests.** The determinism check in the spike's CI job.

---

## Phase 4: history → SQL

### P4-1 `[web] .graph sql: the explored view as one SQL statement`

**Parent:** #281. **Depends on:** P2-1, P2-8.

**Goal.** An exploration is reproducible as text.

**Scope.**
- `.graph sql` prints one statement that returns the edges on screen (and `.graph sql nodes` the nodes with their drawn columns), folded from the history (§4 "The history"): the original query's node set, `UNION` the expansions (uncapped ones as `hop` calls, capped ones as the ids they added), paths as their ids, `EXCEPT` the hidden, then `induced_edges`.
- A header comment summarises the steps. Long id lists wrap at the terminal's width.
- `.graph sql` with no exploration prints the `.graph` source's own query.
- The statement is also what `.share` uses when the command list would be longer.

**Acceptance.** For the Phase 2 acceptance session, running the printed statement returns exactly `SELECT src, dst FROM explorer.edges` (as sets), and `.graph` of it draws the same nodes and edges. Holds after undo, redo and hide.

**Tests.** Shell tests comparing the folded statement's result to the session's edge set over generated sequences of actions (a small property test with a fixed seed).

### P4-2 `[web] Shared links carry the view: camera, pins, selection and styling`

**Parent:** #281. **Depends on:** P2-7, P2-8.

**Goal.** A shared exploration opens as the same picture, not only the same graph.

**Scope.**
- `.graph state` prints, and `.graph state <blob>` restores, the view state that commands do not capture: the camera, pinned positions, the selection, `echo` and `follow`. A compact base64url blob.
- `.share` appends it to the session's commands when a graph is open. The replay question lists it as `.graph state (camera, 3 pins)`.
- The layout is deterministic, so positions are not shared: replaying the commands reproduces them, and pins restore the rest.
- A link over 8,000 characters is refused with the size and the suggestion to share `.graph sql`'s statement instead.

**Acceptance.** e2e: explore, pin two nodes, zoom, share; a fresh page answering `y` shows a frame equal to the original (read back, within antialiasing tolerance).

**Tests.** e2e round trip; unit tests of the blob (versioned, refuses garbage with a message, never executes anything).

### P4-3 `[web] A history panel: the steps, jump to one, and what each added`

**Parent:** #281. **Depends on:** P2-1, P2-5.

**Goal.** See and navigate the exploration.

**Scope.** A section of the inspector: the steps as a list (command, `+n nodes`, time), the current one marked. Clicking a step jumps to it (a series of undos or redos, applied as one delta). A step's menu: copy its SQL, "start over from here" (drops later steps). A new action after jumping back drops the redo tail, as in an editor; the status line says so.

**Acceptance.** After ten steps, jumping to step 3 shows step 3's node and edge counts in one frame, and step 7's on jumping forward. Keyboard: the list is a listbox with arrow keys and Enter.

**Tests.** e2e for jump back, forward and branch.

### P4-4 `[web] Export the view: a PNG, and the nodes and edges as files`

**Parent:** #281. **Depends on:** P2-8.

**Goal.** Take the picture and its data elsewhere.

**Scope.**
- `.export png` downloads the view at 2× with labels and the legend composed in (the existing `snapshot()` plus the label canvas), on the theme's background.
- `.export nodes FORMAT` and `.export edges FORMAT` write `explorer.nodes` and `explorer.edges` as CSV, Parquet, JSON or Arrow through the existing export path.
- A shared link's `.export` stays skipped, as today.

**Acceptance.** The PNG's pixels match the canvas's frame (read back) with labels present. The exported edges, read back with `SELECT * FROM 'edges.parquet'`, equal `explorer.edges`.

**Tests.** e2e downloads, as for the existing formats.

---

## Phase 5: beyond the web

### P5-1 `[viz] ursa-viz/ui: the explorer's panels as a framework-free module the shell and the widget share`

**Parent:** #343. **Depends on:** P1-8; P2-5 for the inspector.

**Goal.** One implementation of the legend, tooltip, inspector, search, toolbar and label overlay for every web front end.

**Scope.**
- `ursa-viz/ui/` as ES modules with no dependencies and no build step: `mountExplorer(element, {view, onCommand, theme})`, plus the UGR reader (P0-1). Styled by CSS custom properties with defaults; no global styles.
- Move the shell's legend, tooltip, selection, inspector and label overlay into it. `web/public/graph.js` keeps only the shell's wiring (the terminal's queue, the pane, timings).
- `onCommand(text)` is the only way out: the shell feeds it to the terminal, a widget sends it to its kernel, an exported page handles the local ones.
- Works under the shell's CSP (no inline style attributes beyond what the CSP allows, no `eval`), and in a shadow root.

**Acceptance.** e2e passes with the shell using the module. A static test page mounts the explorer on a UGR fixture with no shell and passes the same hover, select and legend checks.

**Tests.** e2e; a bun + Playwright test of the standalone page.

### P5-2 `[python] ur.view(…): a notebook widget on the shared core, with expansion in the kernel`

**Parent:** #343 (part 2). **Depends on:** P0-1, P0-2, P5-1; P2-1 for expansion.

**Goal.** The same view in Jupyter, VS Code and Colab, from any Arrow table or SQL result.

**Scope.**
- `ur.view(result, on=None, size=None, color=None, …)` accepts an Arrow table, a SQL result or a graph, applies the result → graph rules in `ursa-engine` through `ursa-py`, and returns an anywidget.
- The wheel carries the `ursa-viz` web bundle and `ui/` behind the `ursa-graph[viz]` extra. The graph travels as one UGR buffer.
- The layout runs in the kernel on every core (`Fa2Multilevel` natively) and streams positions; saved positions skip it.
- With a session behind the result, the widget's commands go to the kernel: `.expand` runs `hop` and `induced_edges` on the native engine (on a `.ursa` database, the memory-mapped index) and returns a delta. Without one, the local commands only.
- `view.selection` is readable from Python.

**Acceptance.** In a headless notebook run, `ur.view(sql("SELECT * FROM hop(…)"))` draws rings; a simulated double-click returns a delta from the kernel in ≤50 ms on a 1M-edge stored database; `view.selection` reflects a click.

**Tests.** The widget's model round-trips; a headless browser test of the front end on fixtures (reusing `ursa-viz/tests/browser/view.ts`); a kernel-side test of the command handler.

### P5-3 `[python] HTML export: one self-contained file with saved positions and an optional halo`

**Parent:** #343 (part 2). **Depends on:** P0-1, P5-1.

**Goal.** `ur.view(…).save("graph.html")` gives a page that opens anywhere, drawn.

**Scope.**
- One file: the wasm bundle (gzip, base64, decompressed with `DecompressionStream`), `ui/`, and the UGR payload with positions computed before export.
- No engine and no network. Local commands work: select, hide, pin, search within the view, undo, fit.
- `halo=1` also embeds the neighbours of the shown nodes and the edges among them, hidden, so `.expand` works one step offline.
- Works from `file://` (no workers from files: the layout is precomputed, and nothing needs a worker). No shared memory.
- The exporter is Rust in `ursa-engine::graph::view` (so P5-4 can call it), exposed through `ursa-py`.

**Acceptance.** The file opens from disk in headless Chromium with no network and draws the same frame as the widget (read back). karate's file is under 2 MB. With `halo=1`, expanding a boundary node adds its neighbours.

**Tests.** A headless test opening the file from `file://`; size check.

### P5-4 `[cli] .graph … TO 'graph.html' in the native shell`

**Parent:** #343. **Depends on:** P0-2, P5-3.

**Goal.** The first way to use the view locally: the CLI writes the page.

**Scope.** In the native `ursa` shell, `.graph [SOURCE] […clauses] TO 'file.html'` resolves the graph as the web shell does (the shared `graph::view`), lays it out natively, and writes P5-3's file. Without `TO`, `.graph` says the view needs a file here and shows the form. `ursa club.ursa -c ".graph g SIZE BY pagerank TO 'g.html'"` works in scripts. The bundle is embedded in the `ursa` binary behind a cargo feature that release builds enable (about 1 MB).

**Acceptance.** The command writes a file that passes P5-3's headless test. A 1M-edge graph exports in the layout's time plus ≤2 s.

**Tests.** A CLI test that writes the file and checks its payload; the headless test in the shell-e2e job.

---

## Bugs and quick wins found in review

Found while reading the code and running the shell at `7650a1c`. The first four are issues above; the rest are small enough to fix in passing or to file as one "loose ends" issue under #280.

| # | what | where | fix |
|---|---|---|---|
| 1 | `.graph` after an algorithm's result draws ids → scores; after a path query, one edge to a list | `endpoint_columns`, `ursa-wasm/src/graph.rs` | P0-4 |
| 2 | No zoom on a phone: only the wheel and `+`/`-` zoom | `web/public/graph.js` | P0-5 |
| 3 | `createView` blocks the page 190–350 ms at 1M edges, and holds 50 MB, to build a topology it uses only for degrees and seed positions | `View::new`, `ursa-viz/src/view.rs` | P0-3 |
| 4 | `hop(…, n => 2)` draws as a star (every reached node tied to the seed) | first-two-columns rule | P1-3 |
| 5 | Positions take an extra copy on every update: `new Float32Array(layout.positions())` copies the array wasm-bindgen already copied out of a `Vec` built for the call | `web/public/graph-worker.js` `post()`; `ursa-viz/demo/layout-worker.js` | Return a `Float32Array` from `Layout::positions` (`js_sys::Float32Array::from(&[f32])` once) and transfer it. Part of P3-7, but a two-line change now. |
| 6 | `WebShell.graph`'s getters clone `src`, `dst`, `ids`, `size` and `color` on every read | `ursa-wasm/src/shell.rs` | P0-2 replaces them with one buffer |
| 7 | The page computes each node's degree in JS, and `View::new` computes it again in wasm | `graph.js` `show()`; `view.rs` | P0-3 |
| 8 | The README and `web/README.md` call the rmat dataset "131K nodes"; the shipped file has 46,318 nodes (`describe(rmat)`). The 131K-node R-MAT is the demo's `scale=17` | `ursa-viz/README.md` "Measured", `web/README.md` | Say which graph each number is for |
| 9 | While the layout is on coarse levels, every frame draws all the input graph's edges between coincident points: the most expensive frames carry the least information (3.9 s each in software at 1M edges) | `graph.js` draws on every `positions` message | P3-3; until then, skip edge drawing while `level > 0` above 200K edges and draw nodes only |
| 10 | A click on a node is announced only in the header text; there is no visual mark on the node picked | `graph.js` `pick()` | P1-8 |
| 11 | `pick` scans every node: 4.9 ms at 1M nodes. Fine for a click, too slow for hover | `View::pick` | P3-6 |
| 12 | A layout of 10K edges takes 3.5 s to "Done" and 100K edges 12 s, longer than 1M edges (10.7 s), because small graphs get 200 fine iterations. The picture is stable long before | `MultilevelParams`' schedule | Report "Done" when movement falls under a threshold, or shorten the schedule when the last level's movement is small. Needs the determinism rule kept (a fixed schedule); so prefer showing `Settling…` after the picture is stable. Worth an issue of its own if the benchmark confirms it on a GPU machine. |
| 13 | Chromium logs `GPU stall due to ReadPixels` on WebGL2 during ordinary drawing in the shell (not only in tests' snapshots) | seen in `view-cost.cjs`'s console under SwiftShader | Find out whether it is wgpu's GL backend presenting through a read-back, or headless Chromium's software compositor. If the former, it is a copy per frame on WebGL2 |
| 14 | The demo's `View::step` path (flat layout on the page's thread) is the only user of the view's own topology | `view.rs` | P0-3 makes it lazy; consider dropping it once the demo uses the worker for both modes |

## What is deliberately not drafted

- **A native window** (`ursa ui`, `show()`): after Phase 5, when the core has proved itself in three front ends. Nothing above may add a browser assumption to the core.
- **`ursa ui club.ursa`** (the shell served locally over the native engine): #281 lists it as later. The wire format and the explore session are what it needs; the transport is its own design.
- **Productionising threads:** only if P3-9 says yes.
- **Curved and bundled edges, 3D, time-based playback:** not needed for the vision's first version, and each is a renderer project of its own.
