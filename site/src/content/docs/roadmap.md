---
title: Roadmap
description: What executes today, what is modelled but not wired, and what is next — in rough priority order.
subtitle: Difference imaging. Subtract the spec from the build; what remains is this page.
---

## Where things stand

**The current release is on PyPI** (the footer names it). It closed most of the v0.1 spec: the full relational surface,
strong components, the shortest-path cost column, a full predicate algebra, native ingest from
Python data structures and the scientific-Python ecosystem, and bundled datasets — on top of the
engine that was already there.

| Layer | State |
|---|---|
| **`ursa-core`** — CSR topology + kernels | Dense `u32` indexing, lazy-transpose CSR with the `edge_ids` permutation. Kernels: `degree` / `pagerank` / `connected_components` (weak union-find **and** strong Tarjan SCC) / `triangle_count` / `clustering_coefficient` / `bfs` / `closeness` / `betweenness` / `label_propagation` / `louvain`, each with a weighted variant. Determinism is enforced by tests across thread pools of 1–8 workers, bit-for-bit. Parallelism sits behind an on-by-default `rayon` feature: turning it off gives an ordered serial fallback with **identical** output, and the crate then compiles for `wasm32-unknown-unknown` (single-threaded) — the guard that keeps an in-browser build possible. |
| **`ursa-plan`** — the engine | Each `collect()` is one DataFusion `LogicalPlan` with graph operators as real logical nodes. Parquet/CSV scans — local, object storage, or `http(s)://` — with the column projection pushed into the file. |
| **`ursa-py`** — bindings | Arrow in and out zero-copy via PyCapsule; the GIL released during compute. `py.typed` ships, so type-checkers see the stubs. |
| **Python surface** | Data-first `EdgeFrame`/`NodeFrame` constructors; interop from polars, pandas, pyarrow, edge lists, networkx, numpy and scipy; the relational verbs `filter` / `select` / `with_columns` / `sort` / `head` / `distinct` / `sample` / `rename` / `group_by().agg()` / `join`; traversals, neighbour aggregation, whole-graph stats; `ur.datasets`; `on_null="drop"` opt-in on edge ingest. |

## Modelled but not wired

The honest list is much shorter than it used to be. These raise a clear error naming themselves:

| Surface | Note |
|---|---|
| `schema()` | the one remaining plan-only relational verb |
| `scan_*(store=...)` | needs a real cross-extension interop mechanism for `object_store` |
| `scan_*(**format_opts)` | e.g. a CSV `delimiter=` |
| `neighbors(from_=...)` | resolving the aggregation against a different node frame |
| A list of paths per scan | one path or glob per scan for now |
| `.str` / `.dt` expression namespaces | the algebra covers comparisons, boolean logic and arithmetic |

There are also stated **composition limits** — one `join` / `group_by` / `with_columns` /
`select` per pipeline, no `filter` after `group_by` (HAVING), no `join` or `group_by` on
traversal results, no tail after `describe()` — each of which raises rather than mis-executing.
See the [reference](/docs/reference#frame-methods) for the full list.

## Next, in rough priority order

**1. Scale-oriented kernel refinements.** The direction-optimizing (top-down/bottom-up) BFS
switch, and delta-stepping for the already-shipping Dijkstra-based weighted SSSP.

**2. Optimizer rules.** Push node-set filters before traversal; fuse `neighbors().agg` into a
segmented CSR reduction so neighbour lists are never materialized; prune columns before
materializing frontiers. The topology index is already built once and shared across ops over a
frame — the index-preservation contract — which is the seam these rules register on.

**3. Benchmarks on reproducible hardware.** The cross-library flywheel runs; what is missing is
the authority of absolute numbers — a pinned container image and a dedicated-hardware runner, so
a published number comes with an image, a machine class and an exact command. See
[Benchmarks](/docs/benchmarks).

**4. Loosening the composition limits.** Multiple `join`s and `group_by`s per pipeline,
HAVING-style filters on grouped results, relational verbs over traversal results — the current
single-shot limits are implementation stages, not design positions.

## Deferred, deliberately

**Motif finding** — `ur.find("(a)-[e]->(b); ...")`, GraphFrames-style — is the first post-v0.1
feature, not an omission.

Also deferred: `ur.sql()` over registered tables, heterogeneous graphs, a `u64` node
space, streaming and out-of-core execution, and temporal semantics.

Subgraph views over filtered frames have **shipped**: a `filter` on an EdgeFrame is now a view
over the parent CSR — the dropped rows ride along as an edge-row bitmask, so every node-valued
kernel (pagerank, degree, closeness, betweenness, components, louvain, label propagation,
triangle count, clustering) runs restricted to the kept edges with no rebuild. Repeated filters
intersect, and a node left with no unmasked incident edge stays present at degree 0.

Node-valued graph ops over a **traversal result** have shipped on the same machinery: a kernel
over `ur.hop(...)` / `ur.shortest_path(...)` runs over the *induced subgraph of the reached nodes*
— the reached region masks the parent CSR, no rebuild — so the explorer's "expand from here, then
rank what I reached" loop composes directly.

Still deferred here: traversals (`hop`/`shortest_path`) *over* a subgraph view or another
traversal; a relational verb *between* a traversal and a graph op; and graph ops on
`distinct`/`sample`/`join`/`group_by`-derived frames, which reshape the edge set beyond what a
mask can express.

## Permanent non-goals

No query language. No mutation API. Not a database. No GNN training — Ursa feeds embedding
pipelines through random walks and `to_arrow`, it does not run them. See
[Core concepts](/docs/concepts#non-goals).
