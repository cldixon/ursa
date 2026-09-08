# Ursa — Polars-shaped dataframes for graph data

> **Status:** Design specification / v0.1 README draft. This document serves two purposes: it is written *as* the entry-level user documentation for the eventual package, and it doubles as the authoritative design handoff for implementation. Sections marked **[IMPL]** are implementation guidance, not user docs.

Ursa is an in-memory, single-machine graph analytics library with a dataframe-first API. It is to graph data what Polars is to tabular data: a Rust core, a lazy query engine, Apache Arrow throughout, and a fluent Python expression API — with graph traversals and graph algorithms as first-class citizens of the query plan.

```python
import ursa as ur

edges = ur.scan_edges("s3://lake/graph/links/*.parquet", src="tower_a", dst="tower_b")
nodes = ur.scan_nodes("s3://lake/graph/towers/*.parquet", id="tower_id")

result = (
    nodes
    .with_columns(
        pagerank  = ur.pagerank(edges, damping=0.85),
        component = ur.connected_components(edges),
        nbr_avg_capacity = ur.neighbors(edges).agg(ur.col("capacity_gbps").mean()),
    )
    .filter(ur.col("pagerank") > 0.001)
    .sort("pagerank", descending=True)
    .collect()
)

result.to_polars()   # zero-copy, it's all Arrow
```

## Why Ursa

The modern data stack settled on a set of primitives: Apache Arrow as the in-memory format, columnar single-machine engines (Polars, DuckDB, DataFusion) as the compute layer, and object storage as the persistence layer. Graph analytics never joined this world. The de facto tool, NetworkX, is object-per-node pure Python — orders of magnitude too slow past a few million edges. The fast alternatives are either object-graph libraries with dated ergonomics (igraph, graph-tool, rustworkx) or embedded graph *databases* with query languages (Kùzu, DuckPGQ). Spark's GraphFrames proved that "a graph is just two dataframes" is the right mental model, but it is trapped in the JVM.

Ursa's position: **graph analytics as a library, not a database; frames, not objects; expressions, not query languages.** If you know Polars, you already know 80% of Ursa.

The target user is a data engineer or analyst with a graph of roughly 1M–500M edges — fraud rings, entity resolution, data lineage, infrastructure topology, web/social graphs — that fits in workstation RAM but makes NetworkX unusable. The target workflow is: scan from Parquet/CSV (local or object storage), compute graph metrics and traversals inside a lazy dataframe pipeline, and flow results back out through Arrow to Polars, Parquet, Iceberg, or an API response.

### The name

Ursa Minor is the constellation that contains **Polaris**, the North Star. A constellation is literally a graph — stars for nodes, asterism lines for edges. The name continues the data-ecosystem lineage of bears (pandas, Polars) and arctic references, and nods to Ursa Labs, the original home of Apache Arrow development.

- Package: `ursa` on PyPI (fallback: `ursa-graph` with `import ursa`)
- Import convention: `import ursa as ur`
- Crates: `ursa-core`, `ursa-plan`, `ursa-py`

## Installation

```bash
pip install ursa-graph  # or: uv add ursa-graph   (the import name is `ursa`)
```

Wheels bundle the Rust core; no Rust toolchain is required. Python ≥ 3.10. `polars` is an optional (but recommended) dependency, used only for `.to_polars()` / `ur.from_polars()` interop.

## Get started

### One file: a bare edgelist

The simplest graph dataset is a two-column edgelist. Here, a web hyperlink graph (e.g., SNAP `web-Google`):

```python
import ursa as ur

edges = ur.scan_edges(
    "web-google.csv",
    src="FromNodeId",
    dst="ToNodeId",
)
```

`scan_edges` is lazy — nothing is read yet. The `src=` / `dst=` arguments are **role mappings**, not renames: the frame remembers which columns play the source and destination roles, and the original column names are preserved for round-tripping. In reusable code you can refer to roles abstractly with `ur.src()`, `ur.dst()`, and `ur.id()`.

With no node file, the node set is derived from the edges:

```python
nodes = edges.nodes()   # lazy NodeFrame: distinct union of src and dst values
```

Graph algorithms are **expressions**. They slot into `with_columns` alongside ordinary column expressions, and the whole pipeline is one lazy plan:

```python
top_pages = (
    nodes
    .with_columns(
        pagerank   = ur.pagerank(edges, damping=0.85, max_iter=30),
        in_degree  = ur.degree(edges, direction="in"),
        component  = ur.connected_components(edges),
        triangles  = ur.triangle_count(edges),
    )
    .filter(ur.col("in_degree") > 0)
    .sort("pagerank", descending=True)
    .head(20)
    .collect()
)
```

All four graph kernels share a single cached topology index (built on first use), and the filter/sort/top-k run on the embedded relational engine. Note `direction="in"`: **directedness is a parameter of each operation, never a property of the frame.** There is no `Graph` vs `DiGraph` split; an EdgeFrame is just rows, and duplicate rows are just parallel edges until you `.distinct()` them.

Whole-graph descriptive statistics:

```python
ur.describe(edges).collect()
# ┌─────────┬─────────┬──────────┬──────────────┬────────────┬─────┐
# │ n_nodes ┆ n_edges ┆ density  ┆ n_components ┆ avg_degree ┆ ... │
# └─────────┴─────────┴──────────┴──────────────┴────────────┴─────┘

ur.density(edges)          # → 3.1e-06   (plain float; see note below)
```

`describe` returns a one-row summary frame that flows anywhere a frame flows. A small documented set of scalar metrics (`density`, `diameter`, `avg_path_length`) additionally exist as *eager* convenience functions returning plain Python numbers — the one deliberate exception to laziness, chosen for ergonomics.

### Traversals, paths, and walks

Traversal results are frames too. A path is an EdgeFrame with extra columns; a k-hop reachability result is an EdgeFrame whose `src` is the seed and whose `dst` is the reached node:

```python
route = ur.shortest_path(
    edges,
    source=17, target=42,
    weight=ur.col("latency_ms"),      # omit for unweighted BFS
).collect()
# EdgeFrame: one row per edge on the path, in order, with a `hop` column

reachable = (
    ur.hop(edges, n=2)
    .from_(nodes.filter(ur.col("in_degree") > 100))    # seed set is just a NodeFrame
    .distinct()
)

walks = ur.random_walk(
    edges,
    start=nodes.sample(1_000),
    steps=10, walks_per_node=4, seed=7,
)
# frame: walk_id, step, node — feeds node2vec-style embedding pipelines directly
```

Because a hop returns an EdgeFrame, traversal **composes**: you can filter it, join attributes onto it, aggregate it, or hop again.

### Two files, object storage, and attributes

A telecom infrastructure graph stored as Parquet in S3:

```python
edges = ur.scan_edges(
    "s3://telco-lake/graph/links/*.parquet",
    src="tower_a", dst="tower_b",
    storage_options={"region": "us-east-1"},
)
nodes = ur.scan_nodes(
    "s3://telco-lake/graph/towers/*.parquet",
    id="tower_id",
)
```

Object storage is first-class: edge scans push their column projection down into Parquet in S3/GCS/Azure (predicate pushdown and node-column projection are planned). `scan_*` also reserves a `store=` parameter for a pre-configured [`obstore`](https://github.com/developmentseed/obstore) store object in place of `storage_options` — obstore and Ursa's engine bind the *same* underlying Rust `object_store` crate — though `store=` is **not yet supported** and currently raises; use `storage_options=` for now.

Neighbor aggregation pulls attributes across the topology:

```python
enriched = (
    nodes
    .with_columns(
        deg              = ur.degree(edges, direction="both"),
        nbr_avg_capacity = ur.neighbors(edges).agg(ur.col("capacity_gbps").mean()),
        nbr_regions      = ur.neighbors(edges).agg(ur.col("region").n_unique()),
        betweenness      = ur.betweenness(edges, sample=0.1, weight=ur.col("latency_ms")),
    )
    .collect()
)
```

**Attribute resolution rule:** inside `ur.neighbors(edges).agg(...)`, topology comes from the threaded EdgeFrame; attribute columns resolve against the ambient frame the expression runs in (here `nodes`, since neighbors are rows of the same frame). An explicit source override exists for exotic cases: `ur.neighbors(edges, from_=other_nodes)`.

Weights, likewise, are never a blessed column — any expression over edge columns works: `weight=ur.col("amount") * ur.col("fx_rate")`.

### Getting results out

Everything is Arrow, so egress is zero-copy:

```python
result = enriched.filter(ur.col("component") == 0)

df  = result.to_polars()            # polars.DataFrame, zero-copy
tbl = result.to_arrow()             # pyarrow.Table → pyiceberg: table.append(tbl)

result.sink_parquet("s3://telco-lake/analytics/graph_metrics/")

@app.get("/towers/critical")        # FastAPI
def critical():
    return result.sort("betweenness", descending=True).head(100).to_dicts()
```

And ingress from data already in the Python runtime is symmetric. The primary
constructors take native data directly — row dicts, a column dict, a polars/pandas
DataFrame, or pyarrow — so the user never has to touch pyarrow:

```python
edges = ur.EdgeFrame([{"a": 0, "b": 1}, {"a": 1, "b": 2}], src="a", dst="b")
edges = ur.EdgeFrame({"a": [0, 1], "b": [1, 2]}, src="a", dst="b")
nodes = ur.NodeFrame(df, id="node_id")                # attribute table

# ...with from_polars / from_pandas / from_arrow as thin, typed aliases:
edges = ur.from_polars(df, src="a", dst="b")     # zero-copy
edges = ur.from_arrow(tbl, src="a", dst="b")
```

However a frame is built — these constructors, the aliases, or `scan_edges` /
`scan_nodes` from a file — the data normalizes to one canonical Arrow source, so
nothing downstream can tell how it was constructed; execution is identical from
that point on.

## Core concepts and object model

### There is no Graph object

Ursa has exactly two public types, both lazy:

| Type | Definition | Special roles |
|---|---|---|
| `EdgeFrame` | a frame with designated `src` and `dst` columns, plus an internal cached topology index | `src`, `dst` |
| `NodeFrame` | a frame with a designated `id` column | `id` |

There is no binding object that "contains" the graph. **The EdgeFrame *is* the graph** — edges are sufficient to define structure, and a NodeFrame is an attribute table that relates to it the way two database tables with a foreign key relate: by join semantics, by key convention. Nothing registers nodes with edges. When an operation needs both topology and node attributes, the EdgeFrame appears as an explicit argument — the same way `df.join(other)` needs `other`. Explicit threading is the honest form of "there is no graph, only frames," and it composes: you can use *different* edge frames within one query (degree in the full graph vs. degree in a filtered subgraph, side by side).

### The algebra is closed over frames

**No operation in the public API ever returns anything that is not a frame** (or, for a documented handful of eager scalar metrics, a plain Python number):

- one hop: EdgeFrame → EdgeFrame — therefore traversal composes by chaining
- a path: an EdgeFrame with `hop` (and optionally cost) columns
- k-hop reachability: an EdgeFrame (`src` = seed, `dst` = reached)
- motif matches (future): a frame with one column per bound variable
- node-valued algorithms (PageRank, components, centralities): either NodeFrames of `(id, value)` or expressions inside `with_columns` — two spellings of the same kernel
- random walks: a frame of `(walk_id, step, node)`

This is the headline design principle, and the property that NetworkX, rustworkx, and embedded graph databases cannot offer.

### Lazy-only

Both frame types are lazy — a frame *is* a logical plan until `.collect()` — mirroring where the ecosystem has converged and matching the engine's native model. `.collect()` returns a materialized frame; `.to_polars()` / `.to_arrow()` exit to the wider ecosystem. There is no eager/lazy class split to learn.

### Immutability and the index-preservation contract

Frames are immutable; transformations return new frames. This makes the topology index shareable and its lifecycle *predictable*. The contract, which is the user-visible performance model:

| Operation | Effect on cached topology index |
|---|---|
| `with_columns(...)` on property columns | **preserved** (shared via cheap reference) |
| `filter(...)` on an EdgeFrame | **preserved** as a *subgraph view* — the dropped rows ride along as an edge mask over the parent CSR, so a graph op runs restricted with **no rebuild** |
| `distinct()`, `sample()`, `join()`, `group_by().agg()` on an EdgeFrame | **dropped** (these reshape the edge set in a way a mask can't express; rebuilt lazily on next graph op) |
| `select(...)` that drops `src` or `dst` | frame **demotes** to a plain tabular frame |
| any operation on a NodeFrame | no effect (NodeFrames carry no index) |

The index is built lazily on the first operation that needs it, cached, and shared by every subsequent graph operation over that frame — including concurrent ones.

### Directions, weights, multiplicity

- **Direction is a per-operation parameter**: `direction="out" | "in" | "both"` (default `"out"`). No directed/undirected class split.
- **Weight is an expression**: `weight=ur.col(...)` on any algorithm that supports it. No blessed weight column.
- **Multiplicity is just rows**: duplicate `(src, dst)` rows are parallel edges; `.distinct()` if you don't want them. Self-loops are just rows where `src == dst`; algorithms document their treatment of them.

### The expression dialect

Ursa's API is deliberately, documentedly **Polars-shaped**: `ur.col`, `ur.lit`, `.filter`, `.with_columns`, `.group_by().agg`, `.sort`, `.head`, `.join`, lazy-until-collect. What transfers from Polars is the muscle memory and the mental model — not the import. Ursa expressions are Ursa objects compiling to the underlying engine; `pl.Expr` objects are **not** accepted (see Architecture for why), and no translator between the two dialects will be built or maintained. Graph verbs (`ur.degree`, `ur.neighbors`, `ur.hop`, `ur.pagerank`, ...) are native members of the same expression family. Where graph semantics eventually diverge from tabular semantics, Ursa evolves its own dialect deliberately.

## API surface (v0.1)

### IO

```python
ur.scan_edges(path, *, src, dst, storage_options=None, store=None, **format_opts) -> EdgeFrame
ur.scan_nodes(path, *, id,       storage_options=None, store=None, **format_opts) -> NodeFrame
ur.from_polars(df, *, src=..., dst=...) / ur.from_polars(df, *, id=...)   # zero-copy
ur.from_arrow(tbl, ...)                                                    # zero-copy
ur.read_edges(...) / ur.read_nodes(...)   # eager conveniences: scan + collect
```

Formats: Parquet and CSV in v0.1 (Parquet with edge-column projection pushdown; predicate pushdown and JSON/NDJSON later). Paths: local, `s3://`, `gs://`, `az://`, plus glob patterns. `store=` (a pre-configured obstore store) and `**format_opts` (e.g. a CSV `delimiter=`) are accepted by the signature but not yet wired — passing them raises rather than being silently ignored.

### Frame methods (both types, relational)

`filter, select, with_columns, rename, sort, head/limit, sample, distinct, group_by(...).agg(...), join, collect, explain, schema, to_polars, to_arrow, to_dicts, sink_parquet, sink_csv`

### EdgeFrame-specific

```python
edges.nodes()                       -> NodeFrame     # distinct union of src/dst
edges.reverse()                     -> EdgeFrame     # swap src/dst roles (metadata-only)
edges.src_col / edges.dst_col       -> str           # role introspection
```

### Expressions

```python
ur.col(name), ur.lit(value), ur.src(), ur.dst(), ur.id()
# plus the standard Polars-shaped expression namespace: arithmetic, comparisons,
# .str, .dt, .list namespaces, aggregations — scoped pragmatically for v0.1
```

### Graph verbs (expression-positioned unless noted)

```python
ur.degree(edges, direction="out")                         # -> UInt32 per node
ur.neighbors(edges, direction="out", from_=None).agg(expr) # neighbor aggregation
ur.hop(edges, n=1, direction="out")                        # -> lazy EdgeFrame (frame-positioned)
    .from_(node_frame_or_ids)                              # restrict seeds
ur.shortest_path(edges, source, target, weight=None, direction="out")  # -> EdgeFrame
ur.random_walk(edges, start, steps, walks_per_node=1, seed=None)       # -> frame
```

### Algorithms (dual-positioned: expression in `with_columns`, or standalone returning a NodeFrame)

| Function | Notes |
|---|---|
| `ur.pagerank(edges, damping=0.85, max_iter=30, tol=1e-6, weight=None)` | pull-based fixpoint |
| `ur.connected_components(edges, mode="weak")` | `"strong"` in a later release |
| `ur.triangle_count(edges)` | per-node; treats edges as undirected |
| `ur.clustering_coefficient(edges)` | local; derived from triangles |
| `ur.betweenness(edges, sample=None, weight=None)` | Brandes; `sample=` for approximation — exact is O(nm) and the docs say so |
| `ur.closeness(edges, weight=None)` | |
| `ur.label_propagation(edges, max_iter=20, seed=None)` | community detection |
| `ur.louvain(edges, weight=None, resolution=1.0, seed=None)` | community detection |

### Graph-level statistics

```python
ur.describe(edges)          # -> lazy one-row summary frame
ur.density(edges)           # -> float   (eager)
ur.avg_path_length(edges, sample=None)   # -> float (eager; sampled estimator by default)
ur.diameter(edges, approximate=True)     # -> int   (eager; exact only on request)
```

## Non-goals (v0.1, some permanent)

- **No query language.** No Cypher, no GQL, no SQL-over-graph surface of our own. Expressions are the interface. (`ur.sql()` over registered node/edge *tables* may arrive later as a free byproduct of the engine — it would be relational SQL, not a graph query language.)
- **No mutation API.** No `add_edge` / `remove_node`. Construct new frames from data.
- **Homogeneous graphs only.** One node type, one edge type per frame pair. A `type` column is just a column you can filter on; first-class heterogeneous/multi-relation APIs are future work.
- **Single machine, in memory.** No distribution, no out-of-core (streaming/spilling may come later via the engine).
- **Not a database.** No persistence format of our own, no transactions, no indexes beyond the in-memory topology index. Parquet on object storage is the storage layer.
- **No GNN training.** We feed embedding/GNN pipelines (random walks, `to_arrow`), we don't run them.

---

## Architecture **[IMPL]**

Everything from here down is implementation guidance for the Rust core. The governing rule for the whole system:

> **Arrow at the boundaries, index in the middle.** Every graph kernel takes Arrow columns plus a shared topology index in, and hands Arrow arrays out. "Everything is a frame" is not a marketing promise; it is a mechanical consequence of the operator contract.

### Engine choice: Apache DataFusion

The relational engine is **DataFusion**, not the Polars crates. Rationale (decided, not open):

1. **Extensibility is the designed use case.** Custom logical plan nodes, user-registered optimizer rules, and pluggable `ExecutionPlan` operators let graph operations be first-class citizens of one unified query plan. Polars' planner is closed; building on it condemns the project to a permanent "traffic cop" architecture coordinating two engines from outside.
2. **Stability posture.** DataFusion is Apache-governed infrastructure with an ecosystem of dependent engines (Comet, LanceDB, InfluxDB 3, GlareDB). The Polars crates are explicitly unstable and subordinate to the Polars Python product.
3. **Free capabilities.** `object_store` integration (S3/GCS/Azure with pushdown) delivers the object-storage story nearly for free; SQL over registered tables is available later at negligible cost.
4. **Room to diverge.** Fixpoint iteration, frontier execution, and (someday) worst-case-optimal joins for motifs will need plan shapes no tabular engine anticipates. DataFusion hands us extension points; anything else hands us a fork.

The cost, accepted deliberately: the public API is a Polars-*shaped* dialect (`ur.col`), not literal `pl.Expr`. **Do not build a Polars-expression translator.** Interop is Arrow, and Arrow interop is zero-copy regardless of engine.

### Crate layout

```
ursa/
├── ursa-core/    # Topology struct + algorithm kernels. Pure Rust.
│                 # Depends on arrow + rayon. NO DataFusion dependency.
│                 # Independently unit-testable and benchmarkable (criterion).
├── ursa-plan/    # DataFusion extensions: custom logical nodes (Hop, NeighborAgg,
│                 # GraphAlgorithm, ...), optimizer rules, ExecutionPlan impls
│                 # that call into ursa-core. Scan/session plumbing, object_store.
└── ursa-py/      # PyO3 bindings. Thin: expression/plan builders, collect()
                  # orchestration, Arrow FFI to/from Python (pycapsule interface).
```

### The topology index

The index is a CSR (compressed sparse row) adjacency structure with three load-bearing design details:

**1. Dense internal indexing.** User node IDs are arbitrary (gappy i64, strings, UUIDs). Kernels never see them. Index construction builds a bidirectional mapping (hash map in; the id column gathered by position out) from user IDs to dense `u32` internal indices `0..n`. All kernels operate in `u32` space over flat arrays — a PageRank vector is literally a `Vec<f64>` indexed by node. Results translate back to user IDs only when materializing output batches. `u32` caps ~4.29B nodes: correct trade for cache behavior; note a `u64` feature flag as future work.

**2. The edge permutation array.** CSR reorders edges (grouped by source), but edge *attributes* stay in the original Arrow columns, unduplicated. Alongside `offsets: Vec<u32|u64>` (len n+1) and `targets: Vec<u32>` (len m), keep `edge_ids: Vec<u32>` mapping each CSR slot to its original row. Weighted kernels evaluate the user's weight expression via DataFusion into an Arrow array first, then gather `weight[edge_ids[k]]` on the fly. Topology in the index; properties in Arrow; nothing copied twice. This array is also the hook for future subgraph views (bitmask over parent CSR instead of rebuild after `filter`), so it is included from day one even for unweighted use.

**3. Directional laziness.** CSR gives out-neighbors; in-neighbors need the transpose (CSC). Build each direction independently on first demand — a PageRank-only pipeline (pull-based, wants incoming) should not pay for both. Budget: ~12 bytes/edge + 8 bytes/node per direction; a 500M-edge graph is ~6 GB/direction — inside the target envelope.

Construction is a parallel counting sort over the `src`/`dst` Arrow columns (two passes). For triangle counting / intersection kernels, sort each adjacency list at build time (or on first intersection use).

**Lifecycle.** The index lives on the EdgeFrame as `Arc<Topology>` with `OnceLock`-style lazy slots per direction. Immutable once built. Property-only transformations clone the `Arc` (the preservation contract in §Core concepts); structural transformations drop it; concurrent queries over one frame share one build.

### Graph operators in the DataFusion plan

Graph kernels are **physical operators that happen to consult a side data structure**. DataFusion executes streams of `RecordBatch`es and already contains pipeline breakers (sort, hash aggregate) that materialize before emitting. An iterative graph algorithm is architecturally the same: `PageRankExec` blocks, runs its fixpoint over the CSR, and emits results as Arrow batches into the downstream plan.

- **Logical nodes** (via `UserDefinedLogicalNode`): `HopNode`, `NeighborAggNode`, `GraphAlgorithmNode { algo, params }`, `ShortestPathNode`, `RandomWalkNode`.
- **Optimizer rules** (registered alongside DataFusion's built-ins), the cross-boundary moves no tabular optimizer knows:
  - push node-set filters *before* traversal (filter seeds, then hop);
  - prune columns before materializing frontiers;
  - fuse `neighbors().agg(...)` into a segmented reduction over CSR — never materialize neighbor lists;
  - share one topology build across multiple graph ops in a plan.
- **v0.1 planner ambition (decided):** ship the custom nodes with *naive* placement first — execute in written order, correctness over cleverness — and land the optimizer rules incrementally in v0.1.x/v0.2. The public API is identical either way; semantics get proven before optimization.

### Algorithm kernels **[IMPL]**

Kernels cluster into four computational shapes; knowing them bounds the effort:

| Shape | Algorithms | Technique |
|---|---|---|
| Fixpoint iteration | PageRank, label propagation, Louvain phases | dense per-node vectors; Rayon sweep over vertex ranges per iteration; converge or max_iter |
| Frontier expansion | BFS, k-hop, unweighted shortest path | visited bitmaps + frontier queues; direction-optimizing BFS (top-down/bottom-up switch) |
| Adjacency intersection | triangle count, clustering coefficient | sorted adjacency lists; parallel merge-intersections |
| Priority / disjoint-set | delta-stepping SSSP, connected components (union-find with path compression / Afforest), Brandes betweenness (with source sampling) | per-algorithm |

**Do not invent these.** The GAP Benchmark Suite reference implementations are the canonical versions of exactly these kernels; port from them. Evaluate the `graph` crate (Martin Junghanns, Neo4j GDS lineage — CSR + Rayon implementations of the GAP kernels) as prior art before writing from scratch; Ursa will likely still own its topology struct because the Arrow coupling and `edge_ids` permutation are load-bearing, but the kernel logic is well-trodden.

Determinism: every stochastic algorithm (`random_walk`, `label_propagation`, `louvain`, sampled `betweenness`) takes `seed=`; same seed + same thread count ⇒ same result (document if a kernel can only promise per-seed determinism at fixed parallelism).

### Runtime integration **[IMPL]** — two known traps, decided handling

1. **Thread pools.** DataFusion executes on tokio (async, IO-oriented); kernels want Rayon (data-parallel compute). Running Rayon loops on tokio workers starves the runtime. Graph `ExecutionPlan`s must dispatch compute via `spawn_blocking` (or a dedicated compute pool) and stream results back. State this in code comments; it otherwise becomes a mysterious deadlock in month two.
2. **The GIL.** `collect()` releases the GIL for the duration of execution (`py.detach`, formerly `py.allow_threads`), so Ursa behaves inside threaded Python servers. Arrow FFI via the Arrow PyCapsule interface for zero-copy exchange with polars/pyarrow.

### Version and interop policy

- Arrow is the compatibility contract; `to_polars`/`from_polars` and `to_arrow`/`from_arrow` are zero-copy via the C data interface.
- No dependency on the Polars Rust crates. `polars` (Python) is an optional dependency touched only in interop shims.
- MSRV and DataFusion version pinned per release; DataFusion upgrades are routine minor-version work.

## v0.1 scope checklist **[IMPL]**

**In:**
- [ ] `EdgeFrame` / `NodeFrame`, lazy, immutable, role mapping, index contract
- [ ] `scan_edges`/`scan_nodes` (Parquet + CSV; local + object storage + glob), `from_polars`/`from_arrow`
- [ ] Relational surface: filter, select, with_columns, sort, head, distinct, group_by/agg, join, sample, rename
- [ ] Expression dialect: `ur.col`/`ur.lit`/`ur.src`/`ur.dst`/`ur.id`, arithmetic/comparison/boolean, core aggregations, minimal `.str`/`.dt`
- [ ] Graph verbs: `degree`, `neighbors().agg()`, `hop().from_()`, `shortest_path`, `random_walk`
- [ ] Algorithms: pagerank, connected_components (weak), triangle_count, clustering_coefficient, betweenness (sampled), closeness, label_propagation, louvain
- [ ] Stats: `describe`, `density`, `avg_path_length` (sampled), `diameter` (approximate)
- [ ] Egress: `collect`, `to_polars`, `to_arrow`, `to_dicts`, `sink_parquet`, `sink_csv`, `explain`
- [ ] Benchmarks vs NetworkX / rustworkx / igraph on GAP-style datasets; criterion micro-benches in `ursa-core`

**Out (deferred):** motif finding (`ur.find("(a)-[e]->(b); ...")` — GraphFrames-style, the first post-v0.1 feature), `ur.sql()`, heterogeneous graphs, u64 node space, streaming/out-of-core, temporal semantics.

*Shipped since the original plan:* strong components (`connected_components(mode="strong")`); subgraph views over filtered frames — a `filter` on an EdgeFrame is now an edge-mask view over the parent CSR (no rebuild), and every node-valued kernel runs over the masked edge set; and node-valued graph ops over a **traversal result** — `ur.pagerank(ur.hop(edges, n).from_(seeds))` runs over the induced subgraph of the reached nodes via the same mask, no rebuild; and an **`f32` output dtype** (`dtype="f32"`) on the float-valued kernels, narrowing the emitted column to halve its wire/on-disk size while the kernel still accumulates in `f64`.

## Open questions (deliberately unresolved)

1. Exact `.str`/`.dt`/`.list` expression coverage for v0.1 — scope pragmatically during implementation.
2. `hop()` result multiplicity: distinct-by-default vs. one row per path (current lean: keep duplicates, user calls `.distinct()` — consistent with multiplicity philosophy) — confirm with real usage.
3. Null handling in `src`/`dst` (error vs. drop-with-warning at index build). Lean: error by default, `on_null="drop"` opt-in. (Implemented: a null endpoint errors by default; `on_null="drop"` is not yet wired.)
4. ~~Whether `describe` computes expensive members (`n_components`) by default or behind `full=True`.~~ **Resolved:** the expensive `n_components` is gated behind `describe(edges, full=True)`; the default one-row summary omits it.

---

*This document is the design authority for v0.1. Implementation choices that contradict a **decided** item above require revisiting this document first.*
