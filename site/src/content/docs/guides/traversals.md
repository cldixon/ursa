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

Returns an EdgeFrame of `(src, dst, hop, cost)` — one row per edge on the path, in order, with
`hop` the zero-based position. Unweighted, this is BFS, and `cost` is the hop count so the schema
stays uniform.

Pass `weight=` and it becomes Dijkstra over the edge-cost expression, with `cost` the cumulative
cost from the source — the final row carries the total path cost:

```python
route = ur.shortest_path(
    edges,
    source=17, target=42,
    weight=ur.col("latency_ms"),
).collect()

total = route.to_dicts()[-1]["cost"]   # what the cheapest path actually costs
```

An unreachable target returns an empty frame rather than raising.

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
feature, not a v0.1 omission.

Subgraph *views* over a filtered frame (a bitmask over the parent CSR instead of a rebuild) have
**shipped** for node-valued kernels: `ur.degree`, `ur.pagerank`, `ur.closeness`,
`ur.betweenness`, `ur.connected_components`, `ur.louvain`, `ur.label_propagation`,
`ur.triangle_count` and `ur.clustering_coefficient` all run over a `edges.filter(...)` view
directly.

A node-valued kernel over a **traversal result** has shipped too: `ur.pagerank(ur.hop(edges,
n=2).from_(seeds))` (and the `.nodes().with_columns(...)` form) runs over the *induced subgraph of
the reached nodes* — the seeds' k-hop region, or a `shortest_path`'s nodes — with no rebuild. This
is the explorer's "expand from here, then rank what I reached" loop. Repeated seeds and both
directions are supported; a node reached but left with no incident edge in the region stays
present at degree 0.

Still deferred: a **traversal** (`hop`/`shortest_path`) *over* a subgraph view or over another
traversal, and a **relational verb** (`filter`/`sort`/`join`/`group_by`/`distinct`) *between* a
traversal and a graph op. For those, run the traversal over the unfiltered frame, or materialize
the result into a new frame first (`ur.EdgeFrame(frontier.collect().to_arrow(), ...)`).
