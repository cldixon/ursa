---
title: Roadmap
description: What executes today, what is modelled but not wired, and what is next — in rough priority order.
subtitle: Difference imaging. Subtract the spec from the build; what remains is this page.
---

## What executes today

The architecture, crate boundaries and load-bearing seams are all in place, and the parts that
prove the design is sound are real and tested end to end.

| Layer | State |
|---|---|
| **`ursa-core`** — CSR topology + kernels | Real and unit-tested. Dense `u32` indexing, lazy-transpose CSR with the `edge_ids` permutation, and working `degree` / `pagerank` / `connected_components` / `triangle_count` / `clustering_coefficient` / `bfs` / `closeness` / `betweenness` / `label_propagation` / `louvain`, each with a weighted variant. |
| **`ursa-plan`** — the engine | Unified plan. Each `collect()` is **one** DataFusion `LogicalPlan` with `GraphAlgorithmNode` as a real `UserDefinedLogicalNode`, lowered by our own `ExtensionPlanner`. Parquet/CSV scans, local or object storage, with the column projection pushed into the file. |
| **`ursa-py`** — bindings | Arrow in and out zero-copy via PyCapsule; the GIL released during compute. |
| **Python dialect + `collect()`** | Live. The expression/plan builder, standalone algorithms, composed `with_columns(...).filter(...).sort(...).head(n).select(...)` pipelines, node-attribute enrichment (in-memory or file-backed, joined by id, with the projection pushdown), `neighbors().agg()` over numeric and string attributes, the traversals `hop()` / `shortest_path()` / `random_walk()`, and the whole-graph stats. |

Weighted algorithms are live across the board, string and int64 node ids are both supported, and
object-storage scans work on `s3://`, `gs://` and `az://`.

## Modelled but not wired

These are in the plan — they compose, and they show up in `.explain()` — but raise a clear error
when collected. They are listed here so the gap is visible rather than discovered.

| Surface | Note |
|---|---|
| `group_by().agg()` | needs aggregation expressions in the dialect alongside it |
| `join(other, on=, how=)` | the public frame-to-frame join, distinct from the internal attribute join |
| `sample(n, seed=)` | |
| `rename(mapping)` | |
| `schema()` | |
| `connected_components(mode="strong")` | SCC has no kernel yet |
| `scan_*(store=...)` | needs a real cross-extension interop mechanism for `object_store` |
| `scan_*(**format_opts)` | e.g. a CSV `delimiter=` |
| `neighbors(from_=...)` | resolving the aggregation against a different node frame |
| Richer filter predicates | today predicates lower as `col <op> literal` |

## Next, in rough priority order

**1. Scale-oriented kernel refinements.** The direction-optimizing (top-down/bottom-up) BFS
switch, and delta-stepping for the already-shipping Dijkstra-based weighted SSSP.

**2. Optimizer rules.** Push node-set filters before traversal; fuse `neighbors().agg` into a
segmented CSR reduction so neighbour lists are never materialized; prune columns before
materializing frontiers. The topology index is already built once and shared across ops over a
frame — the index-preservation contract — which is the seam these rules register on.

**3. Breadth.** Broadening the benchmark coverage, and running it on reproducible dedicated
hardware so absolute numbers can be published with an image, a machine class and an exact command.
See [Benchmarks](/docs/benchmarks).

**4. The relational surface.** Making `group_by().agg`, `join`, `sample` and `rename` execute, and
deepening the expression dialect (`col <op> col`, boolean combinations, `ur.src()`/`ur.dst()`/
`ur.id()`, core aggregations, minimal `.str`/`.dt`) — which is what turns Ursa from "graph metrics
plus a filter/sort tail" into a frame library you can pipeline in.

## Deferred, deliberately

**Motif finding** — `ur.find("(a)-[e]->(b); ...")`, GraphFrames-style — is the first post-v0.1
feature, not an omission.

Also out of v0.1 scope: subgraph views over filtered frames (a bitmask over the parent CSR instead
of a rebuild), `ur.sql()` over registered tables, heterogeneous graphs, a `u64` node space,
streaming and out-of-core execution, and temporal semantics.

## Permanent non-goals

No query language. No mutation API. Not a database. No GNN training — Ursa feeds embedding
pipelines through random walks and `to_arrow`, it does not run them. See
[Core concepts](/docs/concepts#non-goals).
