# Ursa — Polars-shaped dataframes for graph data

> **Status: v0.1 skeleton.** This repository is the initial scaffold for Ursa, an
> in-memory, single-machine graph analytics library with a dataframe-first API —
> what Polars is to tabular data, for graphs. The full design lives in
> [`docs/SPEC.md`](docs/SPEC.md); this README describes what is *actually built
> right now* and how the pieces fit.

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
    .collect()          # <- the one piece not yet wired (see Roadmap)
)
```

## What works today

This is a **skeleton**: the architecture, crate boundaries, type surfaces, and the
load-bearing seams are all in place, and the parts that prove the design is sound
are real and tested end-to-end.

| Layer | State |
|---|---|
| **`ursa-core`** — CSR topology index + kernels | ✅ **Real & unit-tested.** Dense `u32` indexing, lazy-transpose CSR with the `edge_ids` permutation, and working `degree` / `pagerank` (pull-based) / `connected_components` (union-find) / `triangle_count` (sorted-adjacency intersection) kernels. Remaining frontier/BFS kernels are documented stubs. |
| **`ursa-plan`** — DataFusion engine | ✅ **Executing.** `GraphAlgorithmExec` runs node-valued kernels as a real pipeline-breaking `ExecutionPlan`; a DataFusion scan reads Parquet/CSV edge files; a DataFusion `DataFrame` runs the `filter`/`sort`/`limit` tail of composed pipelines. |
| **`ursa-py`** — PyO3 bindings | ✅ **Wired.** Arrow in/out zero-copy (PyCapsule), GIL released during compute. |
| **Python dialect + `collect()`** | ✅ **Live & executing.** The Polars-shaped expression/plan builder, plus `collect()` for a standalone algorithm *and* a composed `with_columns(...).filter(...).sort(...).head(n)` pipeline, over in-memory or `scan_edges` sources. |

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
```

## Architecture

```
ursa/
├── ursa-core/    # Topology (CSR) + algorithm kernels. Pure Rust: arrow + rayon.
│                 # NO DataFusion dependency. Independently testable.
├── ursa-plan/    # DataFusion extensions: custom logical nodes, ExecutionPlans
│                 # (-> ursa-core), optimizer rules, scan/session/object_store.
│                 # The ONE seam where our dialect lowers to DataFusion.
├── ursa-py/      # PyO3 bindings. Thin: plan builders, collect(), Arrow FFI.
└── python/ursa/  # Python package: dialect, frames, IO, graph verbs, stats.
```

Governing rule: **Arrow at the boundaries, index in the middle.** Every kernel
takes Arrow columns plus a shared topology index in, and hands Arrow arrays out.

Why DataFusion (not the Polars crates): extensibility is the designed use case —
graph ops must be first-class citizens of *one* query plan, not coordinated by a
"traffic cop" around a closed planner. The accepted cost is that we own a
Polars-*shaped* expression frontend; it is deliberately quarantined in
`ursa-plan/src/expr.rs` + `python/ursa/_expr.py` so that cost is one bounded seam.

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
`uv run pytest` picks up the change. Requirements: Rust ≥ 1.80, Python ≥ 3.10,
and [uv](https://docs.astral.sh/uv/#installation).

## Roadmap to a walking skeleton

The next milestone is the smallest end-to-end vertical slice through the *real*
engine, proving the operator contract before fanning out:

1. `scan_edges` (local CSV) → build `Arc<Topology>` in a DataFusion scan.
2. `DegreeExec` / `PageRankExec` as real pipeline-breaking `ExecutionPlan`s
   (kernel dispatched via `spawn_blocking`, results emitted as Arrow batches).
3. `EdgeFrame.collect()` → `to_polars()` round-trip, zero-copy.

Everything else in the spec (the full algorithm set, traversals, object storage,
the optimizer rules) builds outward from that slice.

## License

MIT OR Apache-2.0.
