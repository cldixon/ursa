# Ursa — Polars-shaped dataframes for graph data

> **Status: v0.1 in progress.** An in-memory, single-machine graph analytics
> library with a dataframe-first API — what Polars is to tabular data, for graphs.
> The engine foundation is in place: real algorithm kernels, and `collect()`
> executing as one DataFusion plan with graph ops as first-class logical nodes.
> The full design lives in [`docs/SPEC.md`](docs/SPEC.md); this README describes
> what is *actually built right now* and how the pieces fit.

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
| **`ursa-core`** — CSR topology index + kernels | ✅ **Real & unit-tested.** Dense `u32` indexing, lazy-transpose CSR with the `edge_ids` permutation, and working `degree` / `pagerank` (pull-based) / `connected_components` (union-find) / `triangle_count` / `clustering_coefficient` (sorted-adjacency intersection) kernels. Remaining frontier/BFS kernels are documented stubs. |
| **`ursa-plan`** — DataFusion engine | ✅ **Unified plan.** Each `collect()` is **one** DataFusion `LogicalPlan` — `Limit → Sort → Filter → GraphAlgorithmNode` — where `GraphAlgorithmNode` is a real `UserDefinedLogicalNode` lowered to `GraphAlgorithmExec` by our own `ExtensionPlanner`. Graph ops are first-class citizens of the plan (not orchestrated from outside), which is where future optimizer rules register. A DataFusion scan reads Parquet/CSV edge files. |
| **`ursa-py`** — PyO3 bindings | ✅ **Wired.** Arrow in/out zero-copy (PyCapsule), GIL released during compute. |
| **Python dialect + `collect()`** | ✅ **Live & executing.** The Polars-shaped expression/plan builder, plus `collect()` for a standalone algorithm, a composed `with_columns(...).filter(...).sort(...).head(n)` pipeline, **node-attribute enrichment** (in-memory *or* `scan_nodes` file-backed tables joined by id, `ur.col("attr")` usable in filter/sort), **`neighbors().agg()`** over numeric *and* string attributes, a **`hop()` traversal** (first-class `HopNode`, returns an `(src, dst)` EdgeFrame), and the **`describe()`** summary frame. Over in-memory or `scan_edges`/`scan_nodes` sources. |

```python
import ursa as ur, pyarrow as pa

edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")

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
```

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
clustering_coefficient), composed pipelines, `scan_edges`/`scan_nodes` sources,
**node-attribute enrichment** (in-memory or file-backed tables joined by id),
**`neighbors().agg()`** over numeric and string attributes, a first-class
**`hop()` traversal** (its own `HopNode`, returning an `(src, dst)` EdgeFrame that
composes with filter/sort/distinct), the eager **`density`** and one-row
**`describe`** stats, and `sink_parquet`/`sink_csv` egress.

Next, in rough priority order:

1. **Weighted algorithms** (`weight=`, using the `edge_ids` permutation already in
   place) and object storage (`s3://`) to complete the enrichment/IO story.
2. **More traversals** — `shortest_path` / `random_walk` on the frontier/BFS
   kernel family `hop` established (a distance-returning `bfs` unblocks the
   `diameter` / `avg_path_length` stats).
3. **Optimizer rules** — push node-set filters before traversal, fuse
   `neighbors().agg` into a segmented CSR reduction, share one topology build.
4. **Breadth** — remaining algorithms (closeness, betweenness, louvain,
   label_propagation), string/UUID node ids, caching the topology index on the
   frame, benchmarks.

## License

MIT OR Apache-2.0.
