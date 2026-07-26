---
title: Traversals
description: hop, shortest_path and random_walk — each a first-class logical node, each returning a frame.
subtitle: One hop is EdgeFrame → EdgeFrame, which is why traversal composes.
---

Traversal results are frames. That is the whole reason they compose: you can filter a hop, sort
it, join attributes onto it, or hop again.

## k-hop reachability

`ur.hop` returns an **EdgeFrame** whose `src` is the seed and whose `dst` is the reached node.

```python
reachable = ur.hop(edges, n=2).from_([0]).collect()
```

`.from_(...)` restricts the seed set. It takes a list of ids, or a `NodeFrame` — so the seeds can
themselves be the result of a computation:

```python
hubs = edges.nodes().with_columns(indeg=ur.degree(edges, direction="in")).filter(
    ur.col("indeg") > 100
)

ur.hop(edges, n=2).from_(hubs).distinct().collect()
```

`n=` is the hop count and `direction=` is `"out"` (default), `"in"` or `"both"`.

Multiplicity follows the general rule: a hop keeps duplicates, one row per path found, and you
call `.distinct()` if you want the reachable *set*. Being explicit here is consistent with edges
being rows rather than a set.

## Shortest path

```python
route = ur.shortest_path(edges, source=17, target=42).collect()
```

Returns an EdgeFrame of `(src, dst, hop)` — one row per edge on the path, in order, with `hop` the
zero-based position. Unweighted, this is BFS.

Pass `weight=` and it becomes Dijkstra over the edge-cost expression:

```python
route = ur.shortest_path(
    edges,
    source=17, target=42,
    weight=ur.col("latency_ms"),
).collect()
```

The accumulated path cost is not yet returned as a column — the weighted call gives you the
minimum-cost path, but you would have to re-join the weights to total it. That is a known gap, not
a design position.

## Random walks

```python
walks = ur.random_walk(
    edges,
    start=[0, 1, 2],
    steps=10,
    walks_per_node=4,
    seed=7,
).collect()
```

Returns a frame of `(walk_id, step, node)` — the shape node2vec-style embedding pipelines want,
which you can hand straight to `to_arrow()` and out.

`seed=` makes a run reproducible. Ursa's determinism guarantee is stronger than the usual one:
same seed means the same result **regardless of thread count**, not merely at fixed parallelism.
Walks currently draw from one serial RNG stream precisely to hold that guarantee.

## Composing on a traversal

Because a hop is an EdgeFrame, the relational tail works on it:

```python
(ur.hop(edges, n=2)
   .from_([0])
   .sort("dst")
   .head(50)
   .collect()
   .to_polars())
```

And because the result is a frame, a traversal can feed the next stage of an ordinary dataframe
pipeline rather than needing to be unpacked into Python objects first.

## What is not here yet

Motif finding — `ur.find("(a)-[e]->(b); ...")`, GraphFrames-style — is the first post-v0.1
feature, not a v0.1 omission. Subgraph *views* over a filtered frame (a bitmask over the parent
CSR instead of a rebuild) are planned; today a filtered EdgeFrame rebuilds its index.
