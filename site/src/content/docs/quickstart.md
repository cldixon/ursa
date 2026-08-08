---
title: Quickstart
description: A tour of what Ursa v0.1 does today — algorithms, composed pipelines, attribute enrichment, traversals, statistics and egress.
subtitle: Every listing here is runnable. The same tour lives in examples/quickstart.py.
---

The graph for this tour: two triangles (`0-1-2` and `3-4-5`) joined by the bridge `2 → 3`, plus
two extra nodes `6` and `7` that both point at hub `0`. Small enough to check by hand.

```python
import pyarrow as pa
import ursa as ur

src = [0, 1, 2, 3, 4, 5, 2, 6, 7]
dst = [1, 2, 0, 4, 5, 3, 3, 0, 0]
edges = ur.from_arrow(pa.table({"s": src, "d": dst}), src="s", dst="d")
```

`src=` and `dst=` are **role mappings**, not renames. The frame remembers which columns play the
source and destination roles; the original column names survive for round-tripping.

## Standalone algorithms

Every node-valued algorithm can be called bare. It returns a lazy `NodeFrame` of `(id, value)`,
where the value column takes the algorithm's name.

```python
ur.pagerank(edges, damping=0.85).collect().to_polars()
ur.degree(edges, direction="in").collect().to_dicts()
ur.connected_components(edges).collect().to_dicts()
ur.triangle_count(edges).collect().to_dicts()
ur.clustering_coefficient(edges).collect().to_dicts()

ur.closeness(edges).collect().to_dicts()
ur.betweenness(edges).collect().to_dicts()
ur.louvain(edges, seed=1).collect().to_dicts()
ur.label_propagation(edges, seed=1).collect().to_dicts()
```

Nothing runs until `.collect()`. Note `direction="in"`: **directedness is a parameter of each
operation, never a property of the frame.** There is no `Graph` versus `DiGraph` split.

## A composed pipeline

The same algorithms are also expressions. Inside `with_columns` they read as column definitions,
and the whole pipeline is one lazy plan.

```python
top = (
    edges.nodes()
    .with_columns(
        pr    = ur.pagerank(edges),
        indeg = ur.degree(edges, direction="in"),
        tri   = ur.triangle_count(edges),
    )
    .filter(ur.col("indeg") > 0)
    .sort("pr", descending=True)
    .head(5)
)

print(top.explain())          # inspect the plan without running it
top.collect().to_polars()
```

`edges.nodes()` is the distinct union of the `src` and `dst` values, as a lazy `NodeFrame`. All
three kernels share a single topology index, built once on first use; the filter, sort and top-*k*
run in the engine.

## Attribute enrichment

A node attribute table joins to the computed metrics by id. Filter on an attribute column, sort on
a computed one — they are all just columns by the time the tail runs.

```python
nodes = ur.from_arrow(
    pa.table({
        "id":        [0, 1, 2, 3, 4, 5, 6, 7],
        "team":      ["red", "red", "red", "blue", "blue", "blue", "red", "blue"],
        "seniority": [5, 2, 4, 1, 3, 2, 1, 5],
    }),
    id="id",
)

enriched = (
    nodes.with_columns(
        pr    = ur.pagerank(edges),
        indeg = ur.degree(edges, direction="in"),
        # average seniority of each node's predecessors
        nbr_seniority = ur.neighbors(edges, direction="in").agg(ur.col("seniority").mean()),
    )
    .filter(ur.col("seniority") > 1)
    .sort("pr", descending=True)
)

enriched.collect().to_polars()
```

The join is a LEFT join, so every attribute row survives. See
[Attributes and neighbours](/docs/guides/attributes) for the resolution rules.

## Traversals

Traversal results are frames too, which is why they compose.

```python
# k-hop reachability: an EdgeFrame whose src is the seed and dst the reached node
ur.hop(edges, n=2).from_([0]).sort("dst").collect().to_polars()

# a path: one row per edge, in order, with a `hop` column
ur.shortest_path(edges, 0, 5).collect().to_polars()

# random walks: a (walk_id, step, node) frame, ready for node2vec-style pipelines
ur.random_walk(edges, start=[0], steps=4, walks_per_node=2, seed=7).collect().to_polars()
```

## Weights

Weight is never a blessed column. It is a per-operation **expression** over edge columns,
evaluated to one f64 per edge.

```python
weighted = ur.from_arrow(
    pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 0, 0], "amount": [1.0, 9.0, 1.0, 1.0]}),
    src="s", dst="d",
)

ur.pagerank(weighted, weight=ur.col("amount")).collect().to_polars()
```

Weighted PageRank splits a node's rank by edge weight; weighted `shortest_path` is Dijkstra over
the cost. See [Weights](/docs/guides/weights).

## Whole-graph statistics

```python
ur.describe(edges, full=True).collect().to_polars()   # lazy one-row summary frame

ur.density(edges)                        # eager float
ur.avg_path_length(edges)                # eager float
ur.diameter(edges, approximate=False)    # eager int
```

`describe` is lazy and returns a frame. The three scalars are eager and return plain Python
numbers — the one deliberate exception to laziness, chosen for ergonomics.

## Files in, files out

```python
edges  = ur.scan_edges("links.csv", src="from", dst="to")
towers = ur.scan_nodes("towers.csv", id="id")

result = towers.with_columns(pr=ur.pagerank(edges)).sort("pr", descending=True)
result.collect().sink_parquet("metrics.parquet")
```

`scan_*` is lazy; the Parquet column projection is pushed into the file, so a scan reads only the
columns the plan proves it needs. The same call reads from object storage by changing the path to
`s3://`, `gs://` or `az://` and passing `storage_options={...}`.

## The expression dialect

`ur.col` and friends are pure Python and always available, with or without the compiled extension.

```python
predicate = (ur.col("pr") * ur.lit(100) > ur.lit(5)) & (ur.col("indeg") >= ur.lit(2))
```

The dialect is deliberately Polars-*shaped*: what transfers is the muscle memory, not the import.
`pl.Expr` objects are not accepted, and no translator between the two dialects will be built —
interop is Arrow, and Arrow interop is zero-copy anyway.

Today the executable filter surface is narrower than the dialect you can build: predicates lower
as `col <op> literal`. Richer predicates raise a clear error rather than silently mis-executing.
