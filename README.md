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
| **`ursa-core`** — CSR topology index + kernels | ✅ **Real & unit-tested.** Dense `u32` indexing, lazy-transpose CSR with the `edge_ids` permutation, and working `degree` / `pagerank` (pull-based) / `connected_components` (union-find) kernels. `triangle_count` + frontier/BFS are documented stubs. |
| **`ursa-py`** — PyO3 bindings | ✅ **Builds & wired.** A `ursa.demo` namespace runs the real kernels over in-memory edge lists — the full Python → PyO3 → Rust path, GIL released during compute. |
| **Python dialect** (`ur.col`, `ur.pagerank`, …) | ✅ **Live.** The Polars-shaped expression tree and lazy `EdgeFrame`/`NodeFrame` plan-builder (role mapping, index-preservation contract, `.explain()`) are pure-Python and tested. |
| **`ursa-plan`** — DataFusion extensions | 🚧 **Scaffolded.** Module structure, the custom logical-node params, and the *single* expression-lowering seam are laid out with documented `TODO`s. Implementing `collect()` (one kernel as a real `ExecutionPlan`) is the next step. |

```python
# The native path is real today via the demo namespace:
from ursa import demo
demo.pagerank([0, 0, 1, 2], [1, 2, 2, 0])      # -> [(0, 0.29…), (1, 0.13…), (2, 0.57…)]
demo.degree([0, 0, 1, 2], [1, 2, 2, 0], direction="in")
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

```bash
# Rust core: real kernels, fast to build/test (arrow + rayon only)
cargo test -p ursa-core

# Whole workspace (compiles DataFusion; slower)
cargo check

# Python: build the native extension into a virtualenv and run all tests
python3 -m venv .venv && . .venv/bin/activate
pip install -e '.[dev]'          # or: maturin develop
pytest                            # pure-Python tests run without the native build;
                                  # native-kernel tests auto-skip until it's built
```

Requirements: Rust ≥ 1.80, Python ≥ 3.10.

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
