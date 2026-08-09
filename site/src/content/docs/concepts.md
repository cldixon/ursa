---
title: Core concepts
description: There is no Graph object; the algebra is closed over frames; everything is lazy; the topology index has a written contract.
subtitle: Four decisions that explain the rest of the API.
---

## There is no Graph object

Ursa has exactly two public types, both lazy.

| Type | Definition | Special roles |
|---|---|---|
| `EdgeFrame` | a frame with designated `src` and `dst` columns, plus an internal cached topology index | `src`, `dst` |
| `NodeFrame` | a frame with a designated `id` column | `id` |

There is no binding object that "contains" the graph. **The EdgeFrame *is* the graph** — edges are
sufficient to define structure — and a NodeFrame is an attribute table that relates to it the way
two database tables with a foreign key relate: by join semantics, by key convention. Nothing
registers nodes with edges.

When an operation needs both topology and node attributes, the EdgeFrame appears as an explicit
argument, the same way `df.join(other)` needs `other`:

```python
nodes.with_columns(pr=ur.pagerank(edges))
```

Explicit threading is the honest form of "there is no graph, only frames," and it composes: you
can use *different* edge frames within one query — degree in the full graph beside degree in a
filtered subgraph.

## The algebra is closed over frames

No operation in the public API returns anything that is not a frame, apart from a documented
handful of eager scalar metrics.

- one hop: EdgeFrame → EdgeFrame — therefore traversal composes by chaining
- a path: an EdgeFrame with a `hop` column
- k-hop reachability: an EdgeFrame, `src` = seed, `dst` = reached
- node-valued algorithms: either NodeFrames of `(id, value)`, or expressions inside
  `with_columns` — two spellings of the same kernel
- random walks: a frame of `(walk_id, step, node)`

This is the headline design principle, and the property that NetworkX, rustworkx and embedded
graph databases cannot offer.

## Lazy only

Both frame types are lazy — a frame *is* a logical plan until `.collect()`. That mirrors where the
ecosystem converged and matches the engine's native model, and it means there is no eager/lazy
class split to learn.

```python
frame.explain()   # the plan, without running it
frame.collect()   # a MaterializedFrame, backed by chunked Arrow
```

`.collect()` returns a materialized frame; `.to_polars()` and `.to_arrow()` exit to the wider
ecosystem.

## Immutability and the index-preservation contract

Frames are immutable; transformations return new frames. That makes the topology index shareable
and its lifecycle *predictable* — and the lifecycle is the user-visible performance model, so it
is written down rather than left to be discovered.

| Operation | Effect on the cached topology index |
|---|---|
| `with_columns(...)` on property columns | **preserved** (shared by cheap reference) |
| `filter(...)`, `distinct()`, row-changing ops on an EdgeFrame | **dropped** |
| `select(...)` that drops `src` or `dst` | frame **demotes** to a plain tabular frame |
| any operation on a NodeFrame | no effect (NodeFrames carry no index) |

The index is built lazily on the first operation that needs it, cached, and shared by every
subsequent graph operation over that frame — including concurrent ones.

One honest caveat on the dropped case: today a *graph op* over a filtered or otherwise
row-changed EdgeFrame **raises** rather than transparently rebuilding — subgraph views over a
parent index are deferred work. The relational tail on the filtered frame executes fine
(`edges.filter(...).collect()`); to run algorithms on the subgraph, materialize it and construct
a new frame:

```python
sub = ur.EdgeFrame(edges.filter(ur.col("kind") == "road").collect().to_arrow(),
                   src=edges.src_col, dst=edges.dst_col)
ur.pagerank(sub).collect()
```

`edges.reverse()` is a metadata-only swap of the `src` and `dst` roles: no data moves, and a graph
op over the reversed frame builds the transpose, so `degree(edges.reverse(), "out")` equals
`degree(edges, "in")`.

## Directions, weights, multiplicity

**Direction is a per-operation parameter** — `direction="out" | "in" | "both"`, default `"out"`.
There is no directed/undirected class split, so the same frame answers both questions in one
pipeline.

**Weight is an expression**, never a blessed column:

```python
ur.pagerank(edges, weight=ur.col("amount") * ur.col("fx_rate"))
```

**Multiplicity is just rows.** Duplicate `(src, dst)` rows are parallel edges; call `.distinct()`
if you do not want them. Self-loops are just rows where `src == dst`. Each algorithm documents its
treatment of both — see [Semantics vs NetworkX](/docs/semantics).

## The expression dialect

Ursa's API is deliberately, documentedly **Polars-shaped**: `ur.col`, `ur.lit`, `.filter`,
`.with_columns`, `.sort`, `.head`, lazy-until-collect. What transfers from Polars is the muscle
memory and the mental model — not the import. Ursa expressions are Ursa objects compiling to the
underlying engine; `pl.Expr` objects are **not** accepted, and no translator between the two
dialects will be built or maintained.

Graph verbs (`ur.degree`, `ur.neighbors`, `ur.hop`, `ur.pagerank`, …) are native members of the
same expression family. Where graph semantics eventually diverge from tabular semantics, Ursa
evolves its own dialect deliberately.

## Node identifiers

User node ids are arbitrary — gappy int64, or strings such as UUIDs. The type is auto-detected
from the column, and results come back keyed by the original ids.

Internally the kernels never see them: index construction builds a bidirectional mapping to dense
`u32` indices `0..n`, so a PageRank vector is literally a `Vec<f64>` indexed by node. `u32` caps
the node space at about 4.29 billion, which is the correct trade for cache behaviour at the target
scale.

## Non-goals

Some are v0.1 scoping; some are permanent.

- **No query language.** No Cypher, no GQL, no graph-SQL surface of our own. Expressions are the
  interface.
- **No mutation API.** No `add_edge` / `remove_node`. Construct new frames from data.
- **Homogeneous graphs only.** One node type, one edge type per frame pair. A `type` column is
  just a column you can filter on.
- **Single machine, in memory.** No distribution, no out-of-core.
- **Not a database.** No persistence format of our own, no transactions, no indexes beyond the
  in-memory topology index. Parquet on object storage is the storage layer.
- **No GNN training.** Ursa feeds embedding and GNN pipelines — random walks, `to_arrow` — it does
  not run them.
