# Ursa — Polars-shaped dataframes for graph data

> **Status: v0.1 in progress.** An in-memory, single-machine graph analytics
> library with a dataframe-first API — what Polars is to tabular data, for graphs.
> The engine foundation is in place: real algorithm kernels, and `collect()`
> executing as one DataFusion plan with graph ops as first-class logical nodes.
> The full design lives in [`docs/SPEC.md`](docs/SPEC.md); this README describes
> what is *actually built right now* and how the pieces fit. Coming from
> NetworkX? See [how Ursa's algorithm semantics differ](docs/networkx-semantics.md).
>
> **Documentation: <https://ursa-docs.cl-dixon.workers.dev>** — landing page,
> guides and API reference. Source in [`site/`](site/).

Ursa is a Rust core (Apache Arrow throughout), a DataFusion query engine with
graph operators as first-class plan nodes, and a fluent, Polars-shaped Python
expression API. There is no `Graph` object — an `EdgeFrame` *is* the graph, and
every operation returns a frame.

```python
import ursa as ur

edges = ur.scan_edges("web-google.csv", src="FromNodeId", dst="ToNodeId")
top = (
    edges.nodes()
    .with_columns(
        pagerank  = ur.pagerank(edges, damping=0.85),
        in_degree = ur.degree(edges, direction="in"),
    )
    .sort("pagerank", descending=True)
    .head(20)
    .collect()          # runs as one DataFusion plan; results are Arrow
)
```

## What works today

The architecture, crate boundaries, and load-bearing seams are all in place, and
the parts that prove the design is sound are real and tested end-to-end.

| Layer | State |
|---|---|
| **`ursa-core`** — CSR topology index + kernels | ✅ **Real & unit-tested.** Dense `u32` indexing, lazy-transpose CSR with the `edge_ids` permutation, and working `degree` / `pagerank` (pull-based) / `connected_components` (union-find) / `triangle_count` / `clustering_coefficient` (sorted-adjacency intersection) / `bfs` (frontier) / `closeness` / `betweenness` (Brandes, with source sampling) / `label_propagation` / `louvain` (modularity) kernels, each with a weighted variant. |
| **`ursa-plan`** — DataFusion engine | ✅ **Unified plan.** Each `collect()` is **one** DataFusion `LogicalPlan` — `Limit → Sort → Filter → GraphAlgorithmNode` — where `GraphAlgorithmNode` is a real `UserDefinedLogicalNode` lowered to `GraphAlgorithmExec` by our own `ExtensionPlanner`. Graph ops are first-class citizens of the plan (not orchestrated from outside), which is where future optimizer rules register. A DataFusion scan reads Parquet/CSV edge/node files, local or from object storage (`s3://` / `gs://` / `az://`), with the column projection pushed into the file. |
| **`ursa-py`** — PyO3 bindings | ✅ **Wired.** Arrow in/out zero-copy (PyCapsule), GIL released during compute. |
| **Python dialect + `collect()`** | ✅ **Live & executing.** The Polars-shaped expression/plan builder, plus `collect()` for a standalone algorithm, a composed `with_columns(...).filter(...).sort(...).head(n).select(...)` pipeline, **node-attribute enrichment** (in-memory *or* `scan_nodes` file-backed tables joined by id, `ur.col("attr")` usable in filter/sort; `with_columns` stays additive and **`select(...)`** narrows the output Polars-faithfully — which drives a **projection pushdown** so a `scan_nodes` file reads only the columns the plan proves it needs), **`neighbors().agg()`** over numeric *and* string attributes, the **traversals** `hop()` and `shortest_path()` (first-class `HopNode`/`ShortestPathNode` returning EdgeFrames) plus `random_walk()` (a `RandomWalkNode` returning a `(walk_id, step, node)` frame), and the whole-graph stats **`describe()`** / **`density()`** / **`avg_path_length()`** / **`diameter()`**. Over in-memory or `scan_edges`/`scan_nodes` sources — local files **or object storage** (`s3://` / `gs://` / `az://`, with `storage_options={...}`). |

```python
import ursa as ur

# The EdgeFrame *is* the graph. Build one from native Python data — a list of
# row dicts, a dict of columns, a polars/pandas DataFrame, or pyarrow. No
# `import pyarrow` required; `src`/`dst` name the endpoint columns:
edges = ur.EdgeFrame(
    [{"s": 1, "d": 0}, {"s": 2, "d": 0}, {"s": 3, "d": 0}, {"s": 0, "d": 1}],
    src="s",
    dst="d",
)
# equivalently: ur.EdgeFrame({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}, src="s", dst="d")

# Composed pipeline — runs through DataFusion end to end:
(
    edges.nodes()
    .with_columns(pr=ur.pagerank(edges), indeg=ur.degree(edges, direction="in"))
    .filter(ur.col("indeg") > 0)
    .sort("pr", descending=True)
    .head(10)
    .collect()
    .to_polars()
)

# ...or straight from a file (Parquet/CSV, projection pushed into the scan):
ur.pagerank(ur.scan_edges("edges.parquet", src="s", dst="d")).collect().to_polars()

# However the frame is built — constructor, from_polars/from_pandas/from_arrow,
# or scan_edges — it behaves identically from here on.

# Node ids may be int64 (the fast path) or strings (e.g. UUIDs) — auto-detected
# from the column type; results come back keyed by the original ids:
str_edges = ur.EdgeFrame({"s": ["u1", "u1", "u2"], "d": ["u2", "u3", "u3"]}, src="s", dst="d")
ur.pagerank(str_edges).collect().to_polars()   # id column is Utf8
```

`NodeFrame(data, id=...)` is the matching constructor for an attribute table
(joined to the graph by `id`); a `NodeFrame` is optional — nodes are implicit in
the edges (`edges.nodes()`), and attributes are a *separate* table you attach when
you have them.

## Architecture

```
ursa/
├── ursa-core/    # Topology (CSR) + algorithm kernels. Pure Rust: arrow + rayon.
│                 # NO DataFusion dependency. Independently testable.
├── ursa-plan/    # DataFusion extensions: custom logical node + ExecutionPlan
│                 # (-> ursa-core), the query builder, scan/session plumbing.
│                 # The ONE seam where our dialect lowers to DataFusion.
│                 # (optimizer rules + object_store: future work)
├── ursa-py/      # PyO3 bindings. Thin: plan builders, collect(), Arrow FFI.
└── python/ursa/  # Python package: dialect, frames, IO, graph verbs, stats.
```

Governing rule: **Arrow at the boundaries, index in the middle.** Every kernel
takes Arrow columns plus a shared topology index in, and hands Arrow arrays out.

Why DataFusion (not the Polars crates): extensibility is the designed use case —
graph ops must be first-class citizens of *one* query plan, not coordinated by a
"traffic cop" around a closed planner. The accepted cost is that we own a
Polars-*shaped* expression frontend; it is deliberately quarantined at one seam —
`python/ursa/_expr.py` builds the dialect, and `ursa-plan/src/query.rs` lowers it
(today a small JSON column IR + comparison filters) to a DataFusion plan.

## Develop

The Python side is managed with [uv](https://docs.astral.sh/uv/); linting and
formatting use [ruff](https://docs.astral.sh/ruff/) and type-checking uses
[ty](https://github.com/astral-sh/ty).

```bash
# Rust core: real kernels, fast to build/test (arrow + rayon only)
cargo test -p ursa-core

# Whole workspace (compiles DataFusion; slower)
cargo check

# Python: uv creates the venv, builds the maturin extension, and installs the
# dev dependency group in one step.
uv sync

uv run pytest                     # pure-Python tests + native-kernel tests
uv run ruff check .               # lint
uv run ruff format .              # format
uv run ty check                   # type-check
```

`uv run` rebuilds the native extension as needed, so editing Rust and re-running
`uv run pytest` picks up the change. Requirements: Python ≥ 3.10 and
[uv](https://docs.astral.sh/uv/#installation); the Rust toolchain is pinned in
[`rust-toolchain.toml`](rust-toolchain.toml) (rustup installs it automatically),
so local `cargo clippy` uses the exact same lint set as CI.

## Roadmap

The engine foundation is in place — every `collect()` is one DataFusion plan with
custom graph logical nodes — and many features have fanned out on top of it:
node-valued algorithms (pagerank, degree, connected_components, triangle_count,
clustering_coefficient, closeness, betweenness, label_propagation, louvain),
composed pipelines, `scan_edges`/`scan_nodes` sources,
**bring-your-own-format ingress** — beyond pyarrow / polars / pandas / row-dicts,
the interop constructors `from_edgelist` (edge tuples), `from_networkx`
(+ `nodes_from_networkx` for node attributes), `from_numpy` (adjacency matrix or
edge array), and `from_scipy_sparse` (any sparse format); networkx / numpy / scipy
stay optional and are never imported unless you call the matching constructor —
**bundled example datasets** via `ur.datasets` (`load_karate` with faction labels,
`load_lesmis` weighted, `load_florentine`, `load_kite` — offline, in the wheel;
plus `load_facebook`, downloaded and cached on first use) —
**node-attribute enrichment** (in-memory or file-backed tables joined by id),
**`neighbors().agg()`** over numeric and string attributes, the **traversals**
`hop()` and `shortest_path()` (each its own first-class logical node returning an
EdgeFrame, on a shared single-source BFS kernel family) plus `random_walk()`, the eager whole-graph
stats **`density`** / **`avg_path_length`** / **`diameter`** and the one-row
**`describe`**, **object-storage scans** (`s3://` / `gs://` / `az://` via
`object_store`, with `storage_options`) plus **`http(s)://` URL reads** for a
single hosted Parquet/CSV file, and `sink_parquet`/`sink_csv` egress. A null
`src`/`dst` endpoint errors by default; pass **`on_null="drop"`** to the edge
constructors (`from_arrow` / `from_edgelist` / `scan_edges` / `read_edges`) to
filter those rows out instead (with a logged count).

**Weighted algorithms** are live across the board: `weight=` is a per-operation
*expression* over edge columns (`weight=ur.col("amount") * ur.col("fx")`),
evaluated to an f64 per edge and gathered per CSR slot via the `edge_ids`
permutation. Weighted **PageRank**, **`shortest_path`** (Dijkstra), **closeness**,
**betweenness** (Dijkstra-Brandes), and **louvain** all ship.

The **relational tail** — `filter`, `sort`, `head`, `rename`, `distinct`,
`sample`, and now **`group_by(keys...).agg(...)`** — composes on top of any
graph/traversal/plain output through one shared canonical pipeline
(`filter → group_by → distinct → sample → sort → limit → rename`). A grouped
aggregation replaces the schema with `[keys..., outputs...]`; aggregations are
written `ur.col(c).mean()` (also `sum`/`min`/`max`/`count`/`n_unique`),
optionally `.alias(name)` or named via `.agg(name=...)`. It runs on the engine
(DataFusion) for graph-derived frames and in pyarrow for source-backed ones,
with both paths resolving to identical output columns.

**`join(other, on=, how=)`** executes too — an equi-join of two frames on shared
key column(s) (`inner`/`left`), distinct from the automatic attribute attach;
the relational tail applies to the joined result. Only `schema` remains modelled
in the plan (shows in `.explain()`) but **not yet executable**.

Next, in rough priority order:

1. Scale-oriented kernel refinements: the direction-optimizing (top-down/bottom-up)
   BFS switch, and delta-stepping for the (already shipping, Dijkstra-based)
   weighted SSSP.
2. **Optimizer rules** — push node-set filters before traversal, fuse
   `neighbors().agg` into a segmented CSR reduction. (The topology index is now
   built once and shared across ops over a frame — the index-preservation
   contract — which is the seam these rules register on.)
3. **Breadth** — broadening the benchmark coverage. (The docs site now lives in
   [`site/`](site/) and deploys to Cloudflare Workers via Workers Builds.)
   The cross-library **benchmark flywheel** is live in
   [`benchmarks/`](benchmarks/) — a `typer`/`rich` CLI that races Ursa against
   NetworkX, rustworkx, and igraph across nine algorithms, separates cold
   end-to-end from warm kernel time, cross-checks every result against the
   NetworkX oracle, and writes raw rows to Parquet so leaderboards and gap-hunting
   are re-renderable downstream steps. (String/UUID node ids alongside int64 are
   already supported, auto-detected from the column type.)

## License

MIT OR Apache-2.0.
